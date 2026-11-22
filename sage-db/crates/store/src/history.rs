//! Bounded newest-first reads of actual committed transaction events.
//!
//! Event-boundary metadata is derived while committing/replaying, owns its reservations, and is checked against
//! the actual transaction FILE hash on every query. It is not a record authority or an on-disk index. The fallback
//! discovers boundaries with the same ledger and reports that traversal too. Returned records share the fold's
//! immutable owner only after the matching on-disk after-image and its references have been verified.

use crate::fs::FileSystem;
use crate::lease::{LeaseLedger, OwnedReservation};
use crate::open::OpenRefusal;
use crate::view::{ViewBasis, ViewDurability};
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};

/// An immutable retained after-image, used by historical and owned current reads.
/// The historical lookup authenticates disk bytes before sharing it; a current
/// lookup shares its already-verified publication owner without another copy.
pub struct HistoryImage {
    inner: crate::fold::SharedRecord,
}
impl HistoryImage {
    pub(crate) fn new(inner: crate::fold::SharedRecord) -> Self {
        Self { inner }
    }
    /// The complete typed record, including audit and extensions.
    #[must_use]
    pub fn record(&self) -> &crate::fold::FoldedRecord {
        self.inner.get()
    }
    /// Bytes of actual reservations held inside this shared record owner.
    /// Sharing an image creates no second claim; this is not a physical/RSS measure.
    #[must_use]
    pub fn reserved_bytes(&self) -> u64 {
        self.inner.reserved_bytes()
    }
    /// Compare values rather than owner addresses.
    #[must_use]
    pub fn value_eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}
impl core::fmt::Debug for HistoryImage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RecordImage")
            .field("record", self.record())
            .finish()
    }
}
impl Clone for HistoryImage {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// One proven event hit on the indicated observed root.
pub struct HistoryHit {
    /// The shared already-admitted after-image owner.
    pub image: HistoryImage,
    /// Transaction version, independent of the record's revision number.
    pub source_version: u64,
    /// Event candidates visited plus uncached boundary-discovery visits.
    /// Each parser/descriptor pass also charges its real work to the same ledger.
    pub scanned_events: u64,
    /// CURRENT-named manifest FILE identity.
    pub basis: ViewBasis,
    /// Integrity-complete but unsynced roots remain explicitly Unverified.
    pub durability: ViewDurability,
}

/// Why a search could not finish; a scan cap is not a fabricated ledger budget.
#[derive(Debug)]
pub enum HistoryIncomplete {
    /// No materialized current basis is available.
    NoCurrentView,
    /// Explicit per-call event limit was reached.
    EventLimit {
        /// The exact requested limit.
        limit: u64,
    },
    /// The SAME real ledger/control refused this traversal.
    Refused(Refusal),
}

/// A bounded historical read result.
pub enum HistoryOutcome {
    /// One exact on-disk event was found and verified.
    Found(HistoryHit),
    /// All committed wire events were covered without a match.
    Absent {
        /// Actual visits used to prove absence.
        scanned_events: u64,
        /// Observed root on which absence was established.
        basis: ViewBasis,
        /// Durability is never stripped from an Unverified result.
        durability: ViewDurability,
    },
    /// Search stopped without proving absence.
    Incomplete {
        /// Honest per-call or canonical-ledger reason.
        reason: HistoryIncomplete,
        /// Visits completed before refusal.
        scanned_events: u64,
    },
    /// Exact I/O/format/corruption/local failure, in the same taxonomy as open.
    Error(OpenRefusal),
}

/// Hard event-visit bound in addition to the caller's actual ledger.
#[derive(Clone, Copy, Debug)]
pub struct HistoryBudget {
    /// Maximum wire-event visits, not a second allowance counter.
    pub max_events: u64,
    /// Reuse authenticated event boundaries; false exercises the bounded framing fallback.
    pub use_cached_boundaries: bool,
}
impl HistoryBudget {
    /// Construct a per-call bound.
    #[must_use]
    pub const fn new(max_events: u64) -> Self {
        Self {
            max_events,
            use_cached_boundaries: true,
        }
    }
    /// Exercise the genuine bounded scanner without cached event boundaries.
    #[must_use]
    pub const fn without_cached_boundaries(mut self) -> Self {
        self.use_cached_boundaries = false;
        self
    }
}

#[derive(Clone, Copy)]
struct EventSpan {
    start: usize,
    end: usize,
}

struct FrameOwner {
    version: u64,
    tx_id: [u8; 16],
    file_sha256: [u8; 32],
    spans: Vec<EventSpan>,
    previous: Option<HistoryFrame>,
    _owner: OwnedReservation,
    _spans: OwnedReservation,
}

impl Drop for FrameOwner {
    fn drop(&mut self) {
        let mut previous = self.previous.take();
        while let Some(frame) = previous {
            match triomphe::Arc::try_unwrap(frame.0) {
                Ok(mut owner) => previous = owner.previous.take(),
                Err(shared) => {
                    drop(shared);
                    break;
                }
            }
        }
    }
}

/// Derived framing metadata, retained with real block and buffer ownership.
///
/// No record-key index is required: queries still visit transactions/events newest first. This table only allows
/// reverse framing without re-reading every earlier record. A query always authenticates the actual file bytes.
pub(crate) struct HistoryFrame(triomphe::Arc<FrameOwner>);
impl Clone for HistoryFrame {
    fn clone(&self) -> Self {
        Self(triomphe::Arc::clone(&self.0))
    }
}

impl HistoryFrame {
    /// Build from the actual encoded image, before persistence or replay publication.
    pub(crate) fn from_image(
        version: u64,
        tx_id: [u8; 16],
        image: &[u8],
        previous: Option<Self>,
        limits: &Limits,
        ledger: &mut dyn LeaseLedger,
    ) -> Result<Self, OpenRefusal> {
        if previous.as_ref().map_or(version != 1, |p| {
            p.0.version.checked_add(1) != Some(version)
        }) {
            return Err(OpenRefusal::HistoryFrameMismatch);
        }
        let file_sha256 = sage_db_format::digest::raw_sha256_bounded(image, ledger)
            .map_err(crate::open::refusal_from_hash)?;
        let envelope = format_result(sage_db_format::transaction::read_envelope(
            image, limits, ledger,
        ))?;
        let mut spans = Vec::new();
        let mut reader = sage_db_format::wire::Reader::new(&envelope.body);
        sage_db_format::event::read_header(&mut reader, ledger)
            .map_err(crate::open::from_read_error)?;
        while !reader.is_empty() {
            ledger
                .try_charge_entries(1)
                .map_err(|e| OpenRefusal::Refused(Refusal::Budget(e)))?;
            ledger
                .checkpoint()
                .map_err(|s| OpenRefusal::Refused(Refusal::Stopped(s)))?;
            let start = reader.offset();
            sage_db_format::event::read_event(&mut reader, ledger)
                .map_err(crate::open::from_read_error)?;
            let needed = spans
                .len()
                .checked_add(1)
                .ok_or(OpenRefusal::ExtentOverflow)?;
            crate::open::reserve_slots(&mut spans, needed, ledger)?;
            spans.push(EventSpan {
                start,
                end: reader.offset(),
            });
        }
        Self::allocate_owner(version, tx_id, file_sha256, spans, previous, ledger)
    }

    /// The provider block is a prediction bound to this concrete type and provider version.
    fn owner_layout() -> Result<core::alloc::Layout, OpenRefusal> {
        core::alloc::Layout::new::<core::sync::atomic::AtomicUsize>()
            .extend(core::alloc::Layout::new::<FrameOwner>())
            .map_err(|_| OpenRefusal::HistoryLayout)
            .map(|(layout, _)| layout.pad_to_align())
    }

    /// The real single owner-allocation site, shared with the narrow private measurement consumer.
    fn allocate_owner(
        version: u64,
        tx_id: [u8; 16],
        file_sha256: [u8; 32],
        spans: Vec<EventSpan>,
        previous: Option<Self>,
        ledger: &mut dyn LeaseLedger,
    ) -> Result<Self, OpenRefusal> {
        let layout = Self::owner_layout()?;
        let extent = u64::try_from(layout.size()).map_err(|_| OpenRefusal::ExtentOverflow)?;
        let owner = ledger
            .reserve_owned(extent)
            .map_err(|e| OpenRefusal::Refused(Refusal::Budget(e)))?;
        let slots = spans
            .capacity()
            .checked_mul(core::mem::size_of::<EventSpan>())
            .ok_or(OpenRefusal::ExtentOverflow)?;
        let slots = u64::try_from(slots).map_err(|_| OpenRefusal::ExtentOverflow)?;
        let retained = ledger
            .adopt_retained(slots)
            .map_err(OpenRefusal::HistoryTransfer)?;
        triomphe::Arc::try_new(FrameOwner {
            version,
            tx_id,
            file_sha256,
            spans,
            previous,
            _owner: owner,
            _spans: retained,
        })
        .map(Self)
        .map_err(|_| {
            OpenRefusal::Refused(Refusal::Allocation {
                requested: layout.size(),
            })
        })
    }
}

fn format_result<T>(result: FormatOutcome<T>) -> Result<T, OpenRefusal> {
    match result {
        FormatOutcome::Ok(v) => Ok(v),
        FormatOutcome::Format(e) => Err(OpenRefusal::Format(e)),
        FormatOutcome::Refused(r) => Err(OpenRefusal::Refused(r)),
    }
}

fn visit(
    scanned: &mut u64,
    budget: HistoryBudget,
    ledger: &mut dyn LeaseLedger,
) -> Result<(), HistoryIncomplete> {
    if *scanned >= budget.max_events {
        return Err(HistoryIncomplete::EventLimit {
            limit: budget.max_events,
        });
    }
    ledger
        .try_charge_entries(1)
        .map_err(|e| HistoryIncomplete::Refused(Refusal::Budget(e)))?;
    ledger
        .checkpoint()
        .map_err(|s| HistoryIncomplete::Refused(Refusal::Stopped(s)))?;
    *scanned = scanned
        .checked_add(1)
        .ok_or(HistoryIncomplete::EventLimit {
            limit: budget.max_events,
        })?;
    Ok(())
}

fn failure(error: OpenRefusal, scanned_events: u64) -> HistoryOutcome {
    match error {
        OpenRefusal::Refused(refusal) => HistoryOutcome::Incomplete {
            reason: HistoryIncomplete::Refused(refusal),
            scanned_events,
        },
        other => HistoryOutcome::Error(other),
    }
}

/// Search actual named committed transaction files, newest event first.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lookup<F: FileSystem>(
    fs: &F,
    families: &crate::commit::Families<F>,
    fold: &crate::fold::StateFold,
    frames: Option<&HistoryFrame>,
    basis: ViewBasis,
    durability: ViewDurability,
    identity: &crate::fold::RecordIdentity,
    revision: u64,
    budget: HistoryBudget,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> HistoryOutcome {
    let mut scanned = 0;
    if budget.max_events == 0 {
        return HistoryOutcome::Incomplete {
            reason: HistoryIncomplete::EventLimit { limit: 0 },
            scanned_events: 0,
        };
    }
    let Some(first) = frames else {
        return failure(OpenRefusal::HistoryFrameMismatch, 0);
    };
    if first.0.version != basis.version.get() {
        return failure(OpenRefusal::HistoryFrameMismatch, 0);
    }
    let mut bytes = Vec::new();
    let mut cursor = Some(first);
    while let Some(frame) = cursor {
        if let Err(e) = ledger.try_charge_entries(1) {
            return failure(OpenRefusal::Refused(Refusal::Budget(e)), scanned);
        }
        if let Err(s) = ledger.checkpoint() {
            return failure(OpenRefusal::Refused(Refusal::Stopped(s)), scanned);
        }
        let name =
            crate::names::EntryName::Transaction(crate::names::Id128::from_bytes(frame.0.tx_id));
        if let Err(e) = crate::open::read_whole_strict(
            fs,
            &families.transactions,
            name,
            &mut bytes,
            limits.max_tx_file_bytes,
            ledger,
        ) {
            return failure(e, scanned);
        }
        let actual = match sage_db_format::digest::raw_sha256_bounded(&bytes, ledger) {
            Ok(v) => v,
            Err(e) => return failure(crate::open::refusal_from_hash(e), scanned),
        };
        if actual != frame.0.file_sha256 {
            return failure(
                OpenRefusal::Corrupt(crate::open::CorruptReason::TxFileHashMismatch),
                scanned,
            );
        }
        let envelope = match format_result(sage_db_format::transaction::read_envelope(
            &bytes, limits, ledger,
        )) {
            Ok(v) => v,
            Err(e) => return failure(e, scanned),
        };
        let mut fallback = Vec::new();
        let spans = if budget.use_cached_boundaries {
            frame.0.spans.as_slice()
        } else {
            // Discovery is actual wire traversal and is included in the hard cap and reported count.
            let mut reader = sage_db_format::wire::Reader::new(&envelope.body);
            if let Err(e) = sage_db_format::event::read_header(&mut reader, ledger) {
                return failure(crate::open::from_read_error(e), scanned);
            }
            while !reader.is_empty() {
                if let Err(reason) = visit(&mut scanned, budget, ledger) {
                    return HistoryOutcome::Incomplete {
                        reason,
                        scanned_events: scanned,
                    };
                }
                let start = reader.offset();
                if let Err(e) = sage_db_format::event::read_event(&mut reader, ledger) {
                    return failure(crate::open::from_read_error(e), scanned);
                }
                let Some(needed) = fallback.len().checked_add(1) else {
                    return failure(OpenRefusal::ExtentOverflow, scanned);
                };
                if let Err(e) = crate::open::reserve_slots(&mut fallback, needed, ledger) {
                    return failure(e, scanned);
                }
                fallback.push(EventSpan {
                    start,
                    end: reader.offset(),
                });
            }
            fallback.as_slice()
        };
        for span in spans.iter().rev() {
            if let Err(reason) = visit(&mut scanned, budget, ledger) {
                return HistoryOutcome::Incomplete {
                    reason,
                    scanned_events: scanned,
                };
            }
            let Some(event_bytes) = envelope.body.get(span.start..span.end) else {
                return failure(OpenRefusal::HistoryFrameMismatch, scanned);
            };
            let mut raw_reader = sage_db_format::wire::Reader::new(event_bytes);
            let raw = match sage_db_format::event::read_event(&mut raw_reader, ledger) {
                Ok(raw) => raw,
                Err(e) => return failure(crate::open::from_read_error(e), scanned),
            };
            let mut owned_reader = sage_db_format::wire::Reader::new(event_bytes);
            let payload = match format_result(sage_db_format::dispatch::read_dispatched(
                &mut owned_reader,
                ledger,
            )) {
                Ok(sage_db_format::dispatch::DispatchedEvent::Record { payload, .. }) => payload,
                Ok(sage_db_format::dispatch::DispatchedEvent::Validated { family, .. }) => {
                    return failure(
                        OpenRefusal::Unsupported(
                            crate::open::UnsupportedReason::UnmaterializedEventFamily(family),
                        ),
                        scanned,
                    );
                }
                Err(e) => return failure(e, scanned),
            };
            let hit = match crate::handle::history_image(payload, identity, revision, fold, ledger)
            {
                Ok(v) => v,
                Err(e) => return failure(e, scanned),
            };
            if let Some(image) = hit {
                if let Err(e) = crate::handle::verify_event_refs(fs, families, raw.fields(), ledger)
                {
                    return failure(e, scanned);
                }
                return HistoryOutcome::Found(HistoryHit {
                    image,
                    source_version: frame.0.version,
                    scanned_events: scanned,
                    basis,
                    durability,
                });
            }
        }
        cursor = frame.0.previous.as_ref();
    }
    HistoryOutcome::Absent {
        scanned_events: scanned,
        basis,
        durability,
    }
}
