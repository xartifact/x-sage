//! Prepared atomic commit: S2 §4.2 steps 4–14 in the order that makes retries safe.
//!
//! ## The two phases
//!
//! * **Orphan phase** (steps 4–10): staging, blobs, transaction, manifest, `CURRENT.tmp`. A failure leaves files
//!   this attempt created, but none is reachable from any root, so the attempted authority is **unchanged**. That
//!   is `NotCommitted`, never `Unknown` — nothing moved.
//! * **Attempted-authority phase** (steps 11–12): `rename(CURRENT.tmp → CURRENT)` then the root directory sync.
//!   A failure here is genuinely undecided: the root may name the new version without its durability being
//!   proven. That is `Unknown`, and the handle must fence.
//!
//! ## What this module does not do
//!
//! No recovery, no resolution: `Unknown` is returned, not repaired (P1-5 owns that). No snapshot swap — the caller
//! does that after the durable point. No authorization, lifecycle validation or inference.
//!
//! ## The one input that must be explicit
//!
//! Each declared blob needs either the owned bytes to write or a statement that the file is already **present**.
//! `BlobSource` says which; there is no default and no empty-vector placeholder, because a missing source is a
//! missing input rather than a zero-length blob.

use sage_db_format::encode::{DecodedManifest, TxHeaderToEncode, encode_manifest};
use sage_db_format::prepared::{encode_view, view_for_encode};
use sage_db_types::framing::FormatError;
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::RequestOutcomeEvent;
use sage_db_types::prepared::{BlobSource, OwnedEvent, PreparedTransaction};

use crate::fs::{CreateDir, Family, FileSystem, FsError, FsErrorKind, HashReport, PublishMode};
use crate::names::{EntryName, RootName, copy_value_bounded, render_name_bounded};
use crate::names::{Id128, StagingKind};
use crate::prepared::{CarrierParts, split_carrier};

/// One commit attempt's result.
///
/// `Unknown` is not a resolution: P1-4 fences the handle and P1-5 resolves.
// The success variant carries a whole outcome record plus the manifest identity, so the variants differ in size.
// Boxing would add an infallible allocation to a refusal path — the same reasoning as `RecordedOutcome`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum CommitOutcome {
    /// Reached the durable point; `CURRENT` names this version and the root is synced.
    Committed {
        /// Caller stop noticed after persistence began; diagnostic only, never an abort reason.
        deferred_stop: Option<sage_db_types::query::ControlStop>,
        /// Version committed.
        version: u64,
        /// Version this transaction followed; `None` for genesis.
        prev_version: Option<u64>,
        /// What the transaction recorded. Genesis records no outcome, and that is a distinct case rather than a
        /// fabricated event.
        recorded: RecordedOutcome,
        /// The manifest file name this version published. Returned so the next version can name its predecessor
        /// without re-deriving it, and so a caller never has to reconstruct a name the store already rendered.
        manifest_name: String,
        /// Transferred claim over the manifest-name buffer, retained by the handle.
        manifest_retained: crate::lease::OwnedReservation,
        /// That manifest's file hash, which `CURRENT` records and the next version chains from.
        manifest_sha256: [u8; 32],
    },
    /// Attempted authority is definitely unchanged. Orphans may exist.
    NotCommitted {
        /// Caller stop noticed during persistence; the real protocol refusal remains separate.
        deferred_stop: Option<sage_db_types::query::ControlStop>,
        /// Why it stopped.
        reason: CommitRefusal,
    },
    /// The attempted-authority phase was ambiguous. Fence and resolve.
    Unknown {
        /// Caller stop noticed during persistence; authority is undecided for the real I/O reason.
        deferred_stop: Option<sage_db_types::query::ControlStop>,
        /// Which half was ambiguous.
        at: AuthorityPhase,
    },
}

impl CommitOutcome {
    /// The first caller stop noticed after begin_persist, recorded without changing the protocol outcome.
    #[must_use]
    pub const fn deferred_stop(&self) -> Option<sage_db_types::query::ControlStop> {
        match self {
            Self::Committed { deferred_stop, .. }
            | Self::NotCommitted { deferred_stop, .. }
            | Self::Unknown { deferred_stop, .. } => *deferred_stop,
        }
    }
}

/// Why a commit stopped before the durable point.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommitRefusal {
    /// A filesystem operation failed in the orphan phase.
    ///
    /// Carries the seam's own error so `Effect` survives: a `NoEffect` failure definitely did nothing, while an
    /// `Unknown` one may have created a file. Collapsing both into a step name would discard exactly the
    /// distinction the orphan/attempted-authority split exists to preserve.
    Filesystem {
        /// Step that failed.
        step: CommitStep,
        /// The seam's error, intact.
        error: crate::fs::FsError,
    },
    /// The ledger refused.
    Budget,
    /// The caller stopped before `begin_persist`.
    Stopped,
    /// The encoder rejected the bytes: a four-class format verdict, preserved as-is.
    Format(FormatError),
    /// The encoder refused for a non-verdict reason. `Allocation` stays `Allocation`.
    Refused(Refusal),
    /// The transaction carries no `RequestOutcomeRecorded`, so there is no outcome to ACK.
    ///
    /// A legal non-genesis transaction always has one, so this means the candidate is malformed. Inventing a value
    /// would put an outcome in the ACK that the durable bytes do not contain.
    MissingOutcome,
    /// A helper's construction failed for a reason the caller should treat as an encoding refusal.
    Encode,
    /// A physical fold or transfer precondition failed; its exact local cause is preserved.
    Fold(crate::fold::FoldDefect),
    /// Constructing the shared recorded reply failed before persistence.
    Outcome(SharedOutcomeError),
    /// A retained metadata buffer was not admitted on the same counter.
    Transfer(crate::lease::TransferError),
    /// A rebuilt canonical container was rejected by its own canonicaliser.
    ///
    /// The source was canonical, so this is a defect in the copy — never budget, allocation or an input verdict.
    CopyNotCanonical,
    /// A derived extent did not fit `u64`. An arithmetic limit: nothing was allocated and no allowance was consulted, so
    /// this is neither `Allocation` nor `Budget`.
    ExtentOverflow,
    /// The scratch-fold copy path is not resource-resolved, so committing through it is refused rather than assumed.
    ///
    /// `StateFold::try_fork` reserves its tables fallibly but deep-copies every owned `String`/`Vec` in all thirteen
    /// record families with `clone_from`, which allocates infallibly and unadmitted. A budget precharge would not make
    /// those copies fallible, so a commit that proceeded here would be claiming resource closure it does not have.
    /// This refusal exists so the gap cannot be papered over by a passing scalar-only fixture; it is removed only when
    /// the copy path is genuinely shared-immutable or genuinely admitted-and-fallible.
    UnresolvedFoldCopy,
    /// A declared blob's bytes did not match its declaration.
    BlobMismatch {
        /// Which declared blob.
        sha256: [u8; 32],
    },
    /// A declared length does not fit this platform's address space.
    ///
    /// No reservation was attempted, so this is deliberately not reported as `Allocation`.
    DeclaredLengthTooLarge {
        /// The declared length.
        declared_len: u64,
    },
    /// The transaction names a blob the declaration list omits.
    ///
    /// S2 SEAM §2 requires the declarations to be a superset of the canonical references, so publishing without one
    /// would make a durable claim about a blob this attempt never promised to make durable.
    UndeclaredRef {
        /// The omitted reference.
        sha256: [u8; 32],
    },
}

impl From<crate::fold::FoldDefect> for CommitRefusal {
    fn from(defect: crate::fold::FoldDefect) -> Self {
        match defect {
            crate::fold::FoldDefect::Stopped(stop) => Self::Refused(Refusal::Stopped(stop)),
            crate::fold::FoldDefect::Outcome(SharedOutcomeError::Refused(refusal)) => {
                Self::Refused(refusal)
            }
            defect => Self::Fold(defect),
        }
    }
}

/// The persistence steps, named so a refusal identifies itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommitStep {
    /// Step 4: stage a blob and sync it.
    StageBlob,
    /// Step 5: publish a blob and sync its directory.
    PublishBlob,
    /// Step 6: encode and sync the transaction.
    StageTransaction,
    /// Step 7: publish the transaction and sync its directory.
    PublishTransaction,
    /// Step 8: encode and sync the manifest.
    StageManifest,
    /// Step 9: publish the manifest and sync its directory.
    PublishManifest,
    /// Step 10: write and sync `CURRENT.tmp`.
    StageCurrent,
}

/// Which half of the attempted-authority phase was ambiguous.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthorityPhase {
    /// Step 11: the `CURRENT` rename.
    Rename,
    /// Step 12: the root directory sync.
    RootSync,
}

/// The filesystem handles one attempt needs, opened by the caller so an attempt does no discovery.
pub struct CommitTarget<'a, F: FileSystem> {
    /// The seam.
    pub fs: &'a F,
    /// KB root.
    pub root: &'a F::Dir,
    /// `staging/`, opened with `CreateDir::Never`.
    pub staging: &'a F::Dir,
    /// `blobs/`.
    pub blobs: &'a F::Dir,
    /// `transactions/`.
    pub transactions: &'a F::Dir,
    /// `manifests/`.
    pub manifests: &'a F::Dir,
}

/// A sink that collects the complete encoded image of one artifact.
///
/// Deliberately a collector rather than a filesystem writer: the format hands over one complete image per call,
/// and this attempt decides where those bytes go.
pub struct Collect {
    /// The collected bytes.
    pub bytes: Vec<u8>,
}

impl sage_db_types::framing::BytesSink for Collect {
    fn write(&mut self, bytes: &[u8], ledger: &mut dyn SharedLedger) -> Result<(), Refusal> {
        ledger
            .try_charge(bytes.len() as u64)
            .map_err(Refusal::Budget)?;
        if self.bytes.try_reserve_exact(bytes.len()).is_err() {
            return Err(Refusal::Allocation {
                requested: bytes.len(),
            });
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

/// The manifest facts an attempt derives from its prepared transaction.
///
/// Takes the digests as inputs rather than computing them here: `state_digest`/`graph_digest` need the canonical
/// projection, and `chain_digest` needs the previous chain digest — all of which the caller holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ManifestFacts {
    /// Knowledge-state digest after this version.
    pub state_digest: [u8; 32],
    /// Active-graph digest after this version.
    pub graph_digest: [u8; 32],
    /// Digest covering the previous chain digest and this transaction.
    pub chain_digest: [u8; 32],
    /// RequestOutcome / ImportSession / high-water digest.
    pub bookkeeping_digest: [u8; 32],
    /// Previous manifest name; `None` for genesis.
    pub prev_manifest_name: Option<String>,
    /// Previous manifest digest; present iff the name is.
    pub prev_manifest_sha256: Option<[u8; 32]>,
    /// The rules binding this version commits: profile, digest and ref together.
    ///
    /// Carried so a caller can build the published pin identity without re-deriving the trio from separate fields.
    pub rules: sage_db_types::prepared::RulesBinding,
}

/// The artifacts one attempt builds before persisting anything.
pub struct Built {
    /// The encoder header.
    pub header: TxHeaderToEncode,
    /// The carrier parts the header does not hold.
    pub parts: crate::prepared::CarrierParts,
    /// The transaction image.
    pub tx_bytes: Vec<u8>,
    /// Its file hash, which the manifest names.
    pub tx_hash: HashReport,
    /// The manifest image.
    pub manifest_bytes: Vec<u8>,
    /// Its file hash, which `CURRENT` names.
    pub manifest_hash: HashReport,
    /// The `CURRENT` image.
    pub current_bytes: Vec<u8>,
    /// What this transaction records as its outcome.
    ///
    /// Genesis is the documented exception: version 1 carries `Genesis` + nine `IdAllocation` events and no
    /// `RequestOutcomeRecorded`, so requiring one would make every legal genesis fail. The two cases are distinct
    /// in the type rather than represented by a fabricated event.
    pub recorded: RecordedOutcome,
    /// The already-rendered manifest name used by CURRENT and returned to the handle.
    pub manifest_name: String,
    /// The name's transferred byte claim; moves with that name on success.
    pub manifest_retained: crate::lease::OwnedReservation,
    /// Pre-reserved scratch used to read the actual rules inode during persistence.
    pub rules_buffer: Vec<u8>,
}

/// What a transaction records as its outcome.
///
/// Ordinary replies share their retained immutable payload. Genesis has no outcome event, represented explicitly.
/// No new payload allocation or admission is needed to return an exact retry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RecordedOutcome {
    /// An ordinary transaction's own `RequestOutcomeRecorded`, held behind a shared Store-owned owner.
    ///
    /// Shared rather than owned so an exact retry can return the ALREADY-admitted stored outcome without a new allocation
    /// or a new admission. `SharedOutcome` is a public opaque wrapper: its provider and lease internals are private.
    Outcome(SharedOutcome),
    /// Genesis: no outcome event exists, and `CURRENT`/the ACK carry no outcome for it.
    GenesisNone,
}

/// A shared reply could not be constructed before persistence.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SharedOutcomeError {
    /// The real ledger or allocator refused; the class and attempted extent are intact.
    Refused(Refusal),
    /// The moved event's payload had not been admitted on this same counter.
    Transfer(crate::lease::TransferError),
    /// The provider's concrete owner layout could not be represented.
    Layout,
    /// The sum of actual retained capacities could not be represented.
    ExtentOverflow,
}

#[derive(Debug)]
struct OutcomeOwner {
    event: RequestOutcomeEvent,
    _owner: crate::lease::OwnedReservation,
    _payload: crate::lease::OwnedReservation,
}

/// An immutable recorded reply whose block and payload stay charged until the last owner drops.
///
/// Its provider and tokens are private. Cloning shares the existing claim and never allocates or admits another copy.
#[derive(Debug)]
pub struct SharedOutcome {
    inner: triomphe::Arc<OutcomeOwner>,
}

impl SharedOutcome {
    /// Move an already-admitted decoded event into a fallibly allocated shared owner.
    ///
    /// # Errors
    ///
    /// Preserves live-byte budget, control, transfer and real allocation failures separately. The layout extent is bound to
    /// the pinned provider's AtomicUsize header and the concrete OutcomeOwner type, not a stable provider ABI.
    pub fn new(
        event: RequestOutcomeEvent,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> Result<Self, SharedOutcomeError> {
        ledger
            .checkpoint()
            .map_err(|stop| SharedOutcomeError::Refused(Refusal::Stopped(stop)))?;
        let payload_bytes = outcome_payload_extent(&event, ledger)?;
        let owner_bytes = core::alloc::Layout::new::<core::sync::atomic::AtomicUsize>()
            .extend(core::alloc::Layout::new::<OutcomeOwner>())
            .ok()
            .and_then(|(layout, _)| u64::try_from(layout.pad_to_align().size()).ok())
            .ok_or(SharedOutcomeError::Layout)?;
        let owner = ledger
            .reserve_owned(owner_bytes)
            .map_err(|error| SharedOutcomeError::Refused(Refusal::Budget(error)))?;
        let payload = ledger
            .adopt_retained(payload_bytes)
            .map_err(SharedOutcomeError::Transfer)?;
        triomphe::Arc::try_new(OutcomeOwner {
            event,
            _owner: owner,
            _payload: payload,
        })
        .map(|inner| Self { inner })
        .map_err(|_| {
            SharedOutcomeError::Refused(Refusal::Allocation {
                requested: owner_bytes as usize,
            })
        })
    }

    /// Borrow the immutable recorded payload.
    #[must_use]
    pub fn event(&self) -> &RequestOutcomeEvent {
        &self.inner.event
    }
}

fn outcome_payload_extent(
    event: &RequestOutcomeEvent,
    ledger: &mut dyn SharedLedger,
) -> Result<u64, SharedOutcomeError> {
    let mut bytes = (event.extensions.capacity() as u64)
        .checked_mul(core::mem::size_of::<(String, Vec<u8>)>() as u64)
        .ok_or(SharedOutcomeError::ExtentOverflow)?;
    for text in [
        Some(&event.principal_id),
        Some(&event.request_id),
        event.audit.recorded_by.as_ref(),
        event.audit.reason.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        bytes = bytes
            .checked_add(text.capacity() as u64)
            .ok_or(SharedOutcomeError::ExtentOverflow)?;
    }
    for (key, value) in event.extensions.entries() {
        ledger
            .checkpoint()
            .map_err(|stop| SharedOutcomeError::Refused(Refusal::Stopped(stop)))?;
        bytes = bytes
            .checked_add(key.capacity() as u64)
            .and_then(|bytes| bytes.checked_add(value.capacity() as u64))
            .ok_or(SharedOutcomeError::ExtentOverflow)?;
    }
    Ok(bytes)
}

impl Clone for SharedOutcome {
    fn clone(&self) -> Self {
        Self {
            inner: triomphe::Arc::clone(&self.inner),
        }
    }
}

impl PartialEq for SharedOutcome {
    fn eq(&self, other: &Self) -> bool {
        self.event() == other.event()
    }
}
impl Eq for SharedOutcome {}

/// Build and validate every artifact **without touching a filesystem**.
///
/// This is where the ordering obligation lives: everything fallible that does not depend on a published digest
/// happens here, so `commit` has nothing left to fail on except I/O. It is a separate function because a
/// filesystem-free function can be tested for that property directly.
///
/// # Errors
///
/// `NotCommitted` carrying the class the failing stage produced. `Allocation` stays `Allocation`; a format verdict
/// stays a format verdict.
// `CommitOutcome` is large because it carries a whole outcome record on success. Boxing would add an infallible
// allocation to the path that reports a refusal, which is precisely where an allocation must not be able to fail
// invisibly. The value is returned once per attempt.
#[allow(clippy::result_large_err)]
#[allow(clippy::too_many_lines)]
pub fn prebuild(
    prepared: PreparedTransaction,
    facts: &ManifestFacts,
    limits: &Limits,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<Built, CommitOutcome> {
    let (header, parts) = split_carrier(prepared);
    prebuild_parts(header, parts, facts, limits, ledger)
}

// The handle already consumed its carrier before derivation. Preserve that same owned header and event storage
// through prebuild rather than reconstructing an owning header from borrowed fields.
#[allow(clippy::result_large_err, clippy::too_many_lines)]
fn prebuild_parts(
    header: sage_db_format::encode::TxHeaderToEncode,
    parts: CarrierParts,
    facts: &ManifestFacts,
    limits: &Limits,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<Built, CommitOutcome> {
    let attempt = Id128::from_bytes(header.tx_id);
    let tx_name = EntryName::Transaction(attempt);

    // The declarations must cover every canonical reference, checked BEFORE any artifact is built or written.
    if let Err(refusal) = check_declared_superset(&parts.events, &parts.blobs, ledger) {
        return Err(not_committed(refusal));
    }

    // The transaction image, from the carrier's REAL events.
    let tx_bytes = {
        let view = match view_for_encode(&header, &parts.events, limits, ledger) {
            FormatOutcome::Ok(view) => view,
            FormatOutcome::Format(error) => {
                return Err(not_committed(CommitRefusal::Format(error)));
            }
            FormatOutcome::Refused(refusal) => {
                return Err(not_committed(CommitRefusal::Refused(refusal)));
            }
        };
        let mut sink = Collect { bytes: Vec::new() };
        match encode_view(&view, &mut sink, limits, ledger) {
            FormatOutcome::Ok(()) => sink.bytes,
            FormatOutcome::Format(error) => {
                return Err(not_committed(CommitRefusal::Format(error)));
            }
            FormatOutcome::Refused(refusal) => {
                return Err(not_committed(CommitRefusal::Refused(refusal)));
            }
        }
    };
    // Bounded hashing: the bytes this reads are admitted and checkpointed, which the unbounded helper does not do.
    let tx_hash = HashReport {
        len: tx_bytes.len() as u64,
        sha256: match sage_db_format::digest::raw_sha256_bounded(&tx_bytes, ledger) {
            Ok(digest) => digest,
            Err(sage_db_format::digest::BoundedHashError::Budget(error)) => {
                return Err(not_committed(CommitRefusal::Refused(Refusal::Budget(
                    error,
                ))));
            }
            Err(sage_db_format::digest::BoundedHashError::Stopped(stop)) => {
                return Err(not_committed(CommitRefusal::Refused(Refusal::Stopped(
                    stop,
                ))));
            }
        },
    };

    // The manifest image. `tx_file_sha256` is the hash computed above, which does not depend on publication.
    let manifest_name = EntryName::Manifest {
        version: header.version,
        id: attempt,
    };
    let manifest_bytes = {
        let manifest = DecodedManifest {
            kb_uuid: header.kb_uuid,
            version: header.version,
            created_at: header.created_at,
            rules_profile: header.rules_profile,
            rules_digest: header.rules_digest,
            state_digest: facts.state_digest,
            graph_digest: facts.graph_digest,
            chain_digest: facts.chain_digest,
            bookkeeping_digest: facts.bookkeeping_digest,
            prev_manifest_name: match facts.prev_manifest_name.as_deref() {
                Some(name) => match copy_value_bounded(name, ledger) {
                    Ok(copied) => Some(copied),
                    Err(refusal) => return Err(not_committed(CommitRefusal::Refused(refusal))),
                },
                None => None,
            },
            prev_manifest_sha256: facts.prev_manifest_sha256,
            tx_file_name: match render_name_bounded(tx_name, ledger) {
                Ok(name) => name,
                Err(refusal) => return Err(not_committed(CommitRefusal::Refused(refusal))),
            },
            tx_file_sha256: tx_hash.sha256,
        };
        let mut sink = Collect { bytes: Vec::new() };
        match encode_manifest(&manifest, &mut sink, limits, ledger) {
            FormatOutcome::Ok(()) => sink.bytes,
            FormatOutcome::Format(error) => {
                return Err(not_committed(CommitRefusal::Format(error)));
            }
            FormatOutcome::Refused(refusal) => {
                return Err(not_committed(CommitRefusal::Refused(refusal)));
            }
        }
    };
    let manifest_hash = HashReport {
        len: manifest_bytes.len() as u64,
        sha256: match sage_db_format::digest::raw_sha256_bounded(&manifest_bytes, ledger) {
            Ok(digest) => digest,
            Err(sage_db_format::digest::BoundedHashError::Budget(error)) => {
                return Err(not_committed(CommitRefusal::Refused(Refusal::Budget(
                    error,
                ))));
            }
            Err(sage_db_format::digest::BoundedHashError::Stopped(stop)) => {
                return Err(not_committed(CommitRefusal::Refused(Refusal::Stopped(
                    stop,
                ))));
            }
        },
    };

    let manifest_name_rendered = render_name_bounded(manifest_name, ledger)
        .map_err(|error| not_committed(CommitRefusal::Refused(error)))?;
    let manifest_retained = ledger
        .adopt_retained(manifest_name_rendered.capacity() as u64)
        .map_err(|error| not_committed(CommitRefusal::Transfer(error)))?;

    // The CURRENT image, naming the manifest built above.
    let current_bytes = {
        let current = match sage_db_format::current::CurrentToEncode::new(
            header.kb_uuid,
            header.version,
            &manifest_name_rendered,
            manifest_hash.sha256,
        ) {
            FormatOutcome::Ok(current) => current,
            FormatOutcome::Format(error) => {
                return Err(not_committed(CommitRefusal::Format(error)));
            }
            FormatOutcome::Refused(refusal) => {
                return Err(not_committed(CommitRefusal::Refused(refusal)));
            }
        };
        let mut out = Vec::new();
        match sage_db_format::current::encode_current(&current, &mut out, limits, ledger) {
            FormatOutcome::Ok(()) => out,
            FormatOutcome::Format(error) => {
                return Err(not_committed(CommitRefusal::Format(error)));
            }
            FormatOutcome::Refused(refusal) => {
                return Err(not_committed(CommitRefusal::Refused(refusal)));
            }
        }
    };

    // The recorded outcome comes from the transaction's own `RequestOutcomeRecorded`, if it has one.
    //
    // Genesis is the exception: version 1 is `Genesis` + nine `IdAllocation` and records no outcome, so requiring
    // one here made every legal genesis fail with MissingOutcome.
    //
    // `outcome_from_events` below DECODES the event through `prepared::decode_owned_event`, so the recorded outcome
    // carries every audit field and extension the durable bytes hold — it is not a reconstruction of them.
    // The outcome is DECODED through the real dispatch, so it carries every audit field and extension the
    // durable bytes hold rather than a reconstruction of them.
    let recorded = match outcome_from_events(&parts.events, limits, ledger) {
        Ok(Some(outcome)) => match SharedOutcome::new(outcome, ledger) {
            Ok(shared) => RecordedOutcome::Outcome(shared),
            Err(SharedOutcomeError::Refused(refusal)) => {
                return Err(not_committed(CommitRefusal::Refused(refusal)));
            }
            Err(error) => return Err(not_committed(CommitRefusal::Outcome(error))),
        },
        // Genesis records no outcome; that is a distinct case, not a missing field.
        Ok(None) if header.version == 1 => RecordedOutcome::GenesisNone,
        Ok(None) => return Err(not_committed(CommitRefusal::MissingOutcome)),
        Err(outcome) => return Err(outcome),
    };

    let rules_len = parts
        .blobs
        .iter()
        .find(|blob| blob.sha256 == parts.rules.rules_ref)
        .ok_or_else(|| {
            not_committed(CommitRefusal::UndeclaredRef {
                sha256: parts.rules.rules_ref,
            })
        })?
        .declared_len;
    let rules_capacity = usize::try_from(rules_len).map_err(|_| {
        not_committed(CommitRefusal::DeclaredLengthTooLarge {
            declared_len: rules_len,
        })
    })?;
    ledger
        .try_charge(rules_len)
        .map_err(|error| not_committed(CommitRefusal::Refused(Refusal::Budget(error))))?;
    ledger
        .checkpoint()
        .map_err(|stop| not_committed(CommitRefusal::Refused(Refusal::Stopped(stop))))?;
    let mut rules_buffer = Vec::new();
    rules_buffer
        .try_reserve_exact(rules_capacity)
        .map_err(|_| {
            not_committed(CommitRefusal::Refused(Refusal::Allocation {
                requested: rules_capacity,
            }))
        })?;
    Ok(Built {
        manifest_name: manifest_name_rendered,
        manifest_retained,
        rules_buffer,
        header,
        parts,
        tx_bytes,
        tx_hash,
        manifest_bytes,
        manifest_hash,
        current_bytes,
        recorded,
    })
}

/// Commit one prepared transaction.
///
/// Builds everything through [`prebuild`], then persists in S2 §4.2 order. The caller swaps its snapshot and ACKs
/// only on `Committed`; on `Unknown` it fences.
///
/// # Errors
///
/// Never `Err`: "did not commit" and "cannot say" are answers, not errors.
#[allow(clippy::too_many_lines)]
/// The attempt's persistence phase.
///
/// A thin scoped view over the SAME ledger. `try_charge`/`try_charge_entries` forward unchanged, so budget is still real and
/// never double-counted; `checkpoint` is the ONE behavioural difference and it stops honouring caller cancellation, because
/// after the first persistence operation the attempt must run to a protocol outcome (S2 §4.1).
///
/// Deliberately not a second allowance, not a no-op budget and not a second control input: there is one counter and one
/// caller control, exactly as before.
struct PersistingPhase<'a> {
    inner: &'a mut dyn SharedLedger,
    observed_stop: Option<sage_db_types::query::ControlStop>,
}

impl<'a> PersistingPhase<'a> {
    fn new(inner: &'a mut dyn SharedLedger) -> Self {
        Self {
            inner,
            observed_stop: None,
        }
    }
}

impl SharedLedger for PersistingPhase<'_> {
    fn try_charge(&mut self, bytes: u64) -> Result<(), sage_db_types::ledger::BudgetError> {
        // Real budget, same counter: a genuine refusal still surfaces.
        self.inner.try_charge(bytes)
    }

    fn try_charge_entries(&mut self, count: u64) -> Result<(), sage_db_types::ledger::BudgetError> {
        self.inner.try_charge_entries(count)
    }

    fn checkpoint(&mut self) -> Result<(), sage_db_types::query::ControlStop> {
        // Consult the SAME control: notice its stop, but an already-persisting attempt must finish the protocol.
        if let Err(stop) = self.inner.checkpoint() {
            self.observed_stop.get_or_insert(stop);
        }
        Ok(())
    }
}

/// Run one prepared transaction through the durable protocol.
///
/// # Errors
///
/// Never `Err`: a refusal and an ambiguous attempt are outcomes, not errors.
pub fn commit<F: FileSystem>(
    target: &CommitTarget<'_, F>,
    prepared: PreparedTransaction,
    facts: &ManifestFacts,
    limits: &Limits,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> CommitOutcome {
    let (header, parts) = split_carrier(prepared);
    commit_parts(target, header, parts, facts, limits, ledger)
}

// The Store moves its original carrier into header/parts exactly once before derivation. Publication consumes those
// existing values; both this path and the public carrier entry point use the same prebuild and persistence body.
#[allow(clippy::too_many_lines)]
pub(crate) fn commit_parts<F: FileSystem>(
    target: &CommitTarget<'_, F>,
    header: sage_db_format::encode::TxHeaderToEncode,
    parts: CarrierParts,
    facts: &ManifestFacts,
    limits: &Limits,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> CommitOutcome {
    let Built {
        header,
        parts,
        tx_bytes,
        tx_hash,
        manifest_bytes,
        manifest_hash,
        current_bytes,
        recorded,
        manifest_name: manifest_name_rendered,
        manifest_retained,
        mut rules_buffer,
    } = match prebuild_parts(header, parts, facts, limits, ledger) {
        Ok(built) => built,
        Err(outcome) => return outcome,
    };

    let attempt = Id128::from_bytes(header.tx_id);
    let tx_name = EntryName::Transaction(attempt);
    let manifest_name = EntryName::Manifest {
        version: header.version,
        id: attempt,
    };

    // ---- The one phase transition: prebuild done, nothing written yet. ----
    //
    // From here the attempt is PERSISTING. A caller Cancelled/Deadline must not abort it (S2 §4.1): once any byte may
    // exist, reporting "nothing durable happened" would be a claim the caller cannot verify. The adapter below forwards
    // charge/lease to the SAME counter and refuses `checkpoint` once persisting, so no second allowance, no no-op budget and
    // no second control input is introduced.
    // The LAST caller-control gate, immediately before the phase switch: after this point the attempt is persisting and a
    // caller stop is a diagnostic. Prebuild is complete and nothing has been written yet, so a stop here is still honest.
    if let Err(stop) = ledger.checkpoint() {
        return not_committed(CommitRefusal::Refused(Refusal::Stopped(stop)));
    }
    let mut persisting = PersistingPhase::new(ledger);

    let mut outcome = (|| {
        // ---- Steps 4–5: declared blobs. ----
        for blob in &parts.blobs {
            match &blob.source {
                BlobSource::Provide(bytes) => {
                    let staged = EntryName::Staging {
                        kind: StagingKind::Blob,
                        id: attempt,
                    };
                    if let Err(refusal) =
                        stage_bytes(target, target.staging, staged, bytes, CommitStep::StageBlob)
                    {
                        return not_committed(refusal);
                    }
                    let expected = HashReport {
                        len: blob.declared_len,
                        sha256: blob.sha256,
                    };
                    match target.fs.publish_no_overwrite(
                        target.staging,
                        staged,
                        target.blobs,
                        EntryName::Blob(blob.sha256),
                        &expected,
                        &mut persisting,
                    ) {
                        Ok(published) => {
                            if let Err(refusal) =
                                sync_after_publish(target, published, CommitStep::PublishBlob)
                            {
                                return not_committed(refusal);
                            }
                        }
                        // A hash disagreement is a wrong-bytes verdict, not an I/O effect.
                        Err(error) if error.kind() == FsErrorKind::HashMismatch => {
                            return not_committed(CommitRefusal::BlobMismatch {
                                sha256: blob.sha256,
                            });
                        }
                        Err(error) => {
                            return not_committed(CommitRefusal::Filesystem {
                                step: CommitStep::PublishBlob,
                                error,
                            });
                        }
                    }
                }
                BlobSource::Existing => {
                    let expected_len = blob.declared_len;
                    match target.fs.hash_bounded(
                        target.blobs,
                        EntryName::Blob(blob.sha256),
                        expected_len,
                        &mut persisting,
                    ) {
                        Ok(report)
                            if report.sha256 == blob.sha256 && report.len == expected_len => {}
                        Ok(_) => {
                            return not_committed(CommitRefusal::BlobMismatch {
                                sha256: blob.sha256,
                            });
                        }
                        Err(error) => {
                            return not_committed(CommitRefusal::Filesystem {
                                step: CommitStep::PublishBlob,
                                error,
                            });
                        }
                    }
                    // Presence is not durability.
                    if let Err(error) = target
                        .fs
                        .sync_entry(target.blobs, EntryName::Blob(blob.sha256))
                    {
                        return not_committed(CommitRefusal::Filesystem {
                            step: CommitStep::PublishBlob,
                            error,
                        });
                    }
                    if let Err(error) = target.fs.sync_dir(target.blobs) {
                        return not_committed(CommitRefusal::Filesystem {
                            step: CommitStep::PublishBlob,
                            error,
                        });
                    }
                }
            }
        }

        // The rules blob is required, not optional: verify it against the declaration, then sync it. `parts.rules`
        // carries ref, digest and profile together, which is what the accepted verifier binds.
        if let Err(refusal) = verify_and_sync_rules(
            target,
            &parts.rules,
            &parts.blobs,
            &mut rules_buffer,
            limits,
            &mut persisting,
        ) {
            return not_committed(refusal);
        }

        // ---- Steps 6–7: transaction. ----
        let tx_staged = EntryName::Staging {
            kind: StagingKind::Tx,
            id: attempt,
        };
        if let Err(refusal) = stage_bytes(
            target,
            target.staging,
            tx_staged,
            &tx_bytes,
            CommitStep::StageTransaction,
        ) {
            return not_committed(refusal);
        }
        match target.fs.publish_no_overwrite(
            target.staging,
            tx_staged,
            target.transactions,
            tx_name,
            &tx_hash,
            &mut persisting,
        ) {
            Ok(published) => {
                if let Err(refusal) =
                    sync_after_publish(target, published, CommitStep::PublishTransaction)
                {
                    return not_committed(refusal);
                }
            }
            Err(error) => {
                return not_committed(CommitRefusal::Filesystem {
                    step: CommitStep::PublishTransaction,
                    error,
                });
            }
        }

        // ---- Steps 8–9: manifest. ----
        let manifest_staged = EntryName::Staging {
            kind: StagingKind::Manifest,
            id: attempt,
        };
        if let Err(refusal) = stage_bytes(
            target,
            target.staging,
            manifest_staged,
            &manifest_bytes,
            CommitStep::StageManifest,
        ) {
            return not_committed(refusal);
        }
        match target.fs.publish_no_overwrite(
            target.staging,
            manifest_staged,
            target.manifests,
            manifest_name,
            &manifest_hash,
            &mut persisting,
        ) {
            Ok(published) => {
                if let Err(refusal) =
                    sync_after_publish(target, published, CommitStep::PublishManifest)
                {
                    return not_committed(refusal);
                }
            }
            Err(error) => {
                return not_committed(CommitRefusal::Filesystem {
                    step: CommitStep::PublishManifest,
                    error,
                });
            }
        }

        // ---- Step 10: CURRENT.tmp. ----
        // Step 10: `CURRENT.tmp` is a FIXED root name in the KB root, not a staging-family entry. Its only purpose is
        // to be the rename source for `replace_current`, which targets the root.
        let tmp = EntryName::Root(RootName::CurrentTmp);
        match target.fs.remove(target.root, tmp) {
            // A stale tmp from an earlier attempt is an orphan; its absence is the common case. Any other error is
            // real and must not be swallowed.
            Err(error) if error.kind() != FsErrorKind::NotFound => {
                return not_committed(CommitRefusal::Filesystem {
                    step: CommitStep::StageCurrent,
                    error,
                });
            }
            _ => {}
        }
        if let Err(refusal) = stage_bytes(
            target,
            target.root,
            tmp,
            &current_bytes,
            CommitStep::StageCurrent,
        ) {
            return not_committed(refusal);
        }

        // ---- Steps 11–12: attempted-authority phase. Any failure here is Unknown. ----
        if target
            .fs
            .replace_current(target.root, tmp, target.root)
            .is_err()
        {
            return CommitOutcome::Unknown {
                deferred_stop: None,
                at: AuthorityPhase::Rename,
            };
        }
        if target.fs.sync_dir(target.root).is_err() {
            return CommitOutcome::Unknown {
                deferred_stop: None,
                at: AuthorityPhase::RootSync,
            };
        }

        // Steps 13–14 are the caller's: snapshot swap and ACK, neither allocating here.
        CommitOutcome::Committed {
            deferred_stop: None,
            version: header.version,
            prev_version: header.prev_version,
            recorded,
            manifest_name: manifest_name_rendered,
            manifest_retained,
            manifest_sha256: manifest_hash.sha256,
        }
    })();
    let stop = persisting.observed_stop;
    match &mut outcome {
        CommitOutcome::Committed { deferred_stop, .. }
        | CommitOutcome::NotCommitted { deferred_stop, .. }
        | CommitOutcome::Unknown { deferred_stop, .. } => *deferred_stop = stop,
    }
    outcome
}

fn not_committed(reason: CommitRefusal) -> CommitOutcome {
    CommitOutcome::NotCommitted {
        reason,
        deferred_stop: None,
    }
}

/// The transaction's own recorded outcome, taken from the events it contains.
///
/// Uses `format::prepared::decode_owned_event`, which writes the event with the real writer and reads it back
/// through the real dispatch. An earlier revision indexed the fields by hand, which duplicated the decoder and
/// silently dropped `audit.recorded_by`, `audit.reason` and `extensions` — it could construct an outcome the
/// decoder would reject, and forget a tag without failing.
///
/// Returns `None` when the transaction carries no `RequestOutcomeRecorded`. Genesis is the documented exception;
/// every ordinary transaction has one.
// The error type is `CommitOutcome`, which carries an outcome on success; boxing it would add an allocation to a
// refusal path.
#[allow(clippy::result_large_err)]
fn outcome_from_events(
    events: &[OwnedEvent],
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<Option<RequestOutcomeEvent>, CommitOutcome> {
    let Some(event) = events.iter().find(|e| e.tag == 192) else {
        return Ok(None);
    };
    match sage_db_format::prepared::decode_owned_event(event, limits, ledger) {
        FormatOutcome::Ok(sage_db_format::dispatch::DispatchedPayload::RequestOutcome(record)) => {
            Ok(Some(record))
        }
        // A different family decoded from a 192 event: the registry and the decoder disagree, which is a build
        // defect rather than a file problem.
        FormatOutcome::Ok(_) => Err(not_committed(CommitRefusal::Encode)),
        FormatOutcome::Format(error) => Err(not_committed(CommitRefusal::Format(error))),
        FormatOutcome::Refused(refusal) => Err(not_committed(CommitRefusal::Refused(refusal))),
    }
}

/// Sync after a publish when the publication says the sync is still owed.
fn sync_after_publish<F: FileSystem>(
    target: &CommitTarget<'_, F>,
    published: crate::fs::Published,
    step: CommitStep,
) -> Result<(), CommitRefusal> {
    if published.needs_sync {
        let dir = match step {
            CommitStep::PublishBlob => target.blobs,
            CommitStep::PublishTransaction => target.transactions,
            CommitStep::PublishManifest => target.manifests,
            _ => target.root,
        };
        if let Err(error) = target.fs.sync_dir(dir) {
            return Err(CommitRefusal::Filesystem { step, error });
        }
    }
    Ok(())
}

/// Write `bytes` to a staging name, handling short writes, then sync the file.
fn stage_bytes<F: FileSystem>(
    target: &CommitTarget<'_, F>,
    dir: &F::Dir,
    name: EntryName,
    bytes: &[u8],
    step: CommitStep,
) -> Result<(), CommitRefusal> {
    let mut file = match target.fs.create_new(dir, name) {
        Ok(file) => file,
        Err(error) => return Err(CommitRefusal::Filesystem { step, error }),
    };
    let mut written = 0usize;
    while written < bytes.len() {
        match target.fs.write(&mut file, &bytes[written..]) {
            Ok(outcome) => {
                if outcome.written == 0 {
                    return Err(CommitRefusal::Filesystem {
                        step,
                        error: FsError::new(FsErrorKind::Io(0), crate::fs::Effect::NoEffect),
                    });
                }
                written += outcome.written;
            }
            Err(error) => return Err(CommitRefusal::Filesystem { step, error }),
        }
        // NO cancellation check here. `begin_persist` has already passed, so a caller stop must NOT be honoured:
        // reporting Stopped after staging bytes exist would tell the caller "nothing durable happened" while an
        // orphan may already be on disk, and the caller cannot distinguish that from "CURRENT moved". The attempt
        // finishes through protocol outcomes (S2 §4.1). Budget is still enforced by the write itself, which is an
        // I/O effect rather than a caller choice.
    }
    if let Err(error) = target.fs.sync_file(&file) {
        return Err(CommitRefusal::Filesystem { step, error });
    }
    Ok(())
}

/// Verify the rules bytes against what the transaction declares, then make them durable.
///
/// Three obligations, in this order, because each is worthless without the previous one:
///
/// 1. the blob's bytes must be readable and their **actual** digest and length must match the declaration — a
///    successful `hash_bounded` call is not a match, and its returned `HashReport` was previously discarded;
/// 2. the bytes must satisfy `rules_read::verify_rules` for the declared `(rules_ref, rules_digest, profile)`, which
///    is the accepted verifier and the only thing that binds ref, digest and domain together;
/// 3. the named file must be synced, and its directory, before any durable claim. The seam has no flush on a read
///    handle (`sync_file` takes a write handle), so `sync_entry` is the primitive for bytes this attempt did not
///    write; the read handle is held across verification so the bytes verified are the file's bytes.
///
/// A rules reference is required by definition, so "the caller did not declare it" is not a shortcut: it must be
/// present, verified, and durable either way.
fn verify_and_sync_rules<F: FileSystem>(
    target: &CommitTarget<'_, F>,
    rules: &sage_db_types::prepared::RulesBinding,
    declarations: &[sage_db_types::prepared::BlobDeclaration],
    bytes: &mut Vec<u8>,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<(), CommitRefusal> {
    let digest = rules.rules_ref;
    let entry = EntryName::Blob(digest);

    // The DECLARED length is known — it is the declaration that names this blob. An earlier version invented
    // `max_block_raw_bytes` as a ceiling for a rules blob, which is a read bound for something else entirely.
    let declared_len = match declarations.iter().find(|d| d.sha256 == digest) {
        Some(declared) => declared.declared_len,
        // Not declared: the superset check would have refused, so reaching here is a caller bug rather than a
        // reason to guess a bound.
        None => return Err(CommitRefusal::UndeclaredRef { sha256: digest }),
    };
    let declared_len_usize = match usize::try_from(declared_len) {
        Ok(len) => len,
        // The declared length does not fit this platform's address space. No reservation was attempted, so
        // reporting `Allocation` would describe a failure that did not happen.
        Err(_) => return Err(CommitRefusal::DeclaredLengthTooLarge { declared_len }),
    };

    // Scratch was admitted/reserved in prebuild. Persistence never grows it.
    debug_assert!(bytes.is_empty());
    debug_assert!(bytes.capacity() >= declared_len_usize);

    // Keep the read handle: verifying the bytes this handle returns and then syncing THAT handle is the
    // same-inode property, rather than re-opening the name and hoping it names the same file.
    let mut file = match target.fs.open_read(target.blobs, entry) {
        Ok(file) => file,
        Err(error) => {
            return Err(CommitRefusal::Filesystem {
                step: CommitStep::PublishBlob,
                error,
            });
        }
    };

    let mut buf = [0u8; 8192];
    let mut total: u64 = 0;
    loop {
        // Overrun is rejected BEFORE the append, so a grown file can never force an unadmitted `extend_from_slice`.
        if total >= declared_len {
            break;
        }
        let remaining = declared_len_usize.saturating_sub(bytes.len());
        let want = remaining.min(buf.len());
        if want == 0 {
            break;
        }
        let n = match target.fs.read(&mut file, &mut buf[..want]) {
            Ok(n) => n,
            Err(error) => {
                return Err(CommitRefusal::Filesystem {
                    step: CommitStep::PublishBlob,
                    error,
                });
            }
        };
        if n == 0 {
            break;
        }
        // Capacity was reserved for exactly the declared extent, so this cannot grow.
        bytes.extend_from_slice(&buf[..n]);
        total = total.saturating_add(n as u64);
    }
    // A file longer than declared is a mismatch, not a truncation to accept.
    if total != declared_len {
        return Err(CommitRefusal::BlobMismatch { sha256: digest });
    }
    // The extent matched, but the file may be LONGER. One stack-byte read settles it before any append, which is
    // what makes the "longer than declared is a mismatch" claim true rather than assumed.
    let mut probe = [0u8; 1];
    match target.fs.read(&mut file, &mut probe) {
        // Any byte here means the file exceeds its declaration.
        Ok(0) => {}
        Ok(_) => return Err(CommitRefusal::BlobMismatch { sha256: digest }),
        Err(error) => {
            return Err(CommitRefusal::Filesystem {
                step: CommitStep::PublishBlob,
                error,
            });
        }
    }

    // The actual bytes must hash to the reference the declaration names.
    let actual = match sage_db_format::digest::raw_sha256_bounded(bytes, ledger) {
        Ok(digest) => digest,
        // `BoundedHashError` has exactly two variants. Collapsing them into a fabricated `Allocation` would convert
        // "retry with more allowance" and "the caller cancelled" into an allocator failure that never happened.
        Err(sage_db_format::digest::BoundedHashError::Budget(error)) => {
            return Err(CommitRefusal::Refused(Refusal::Budget(error)));
        }
        Err(sage_db_format::digest::BoundedHashError::Stopped(stop)) => {
            return Err(CommitRefusal::Refused(Refusal::Stopped(stop)));
        }
    };
    if actual != digest {
        return Err(CommitRefusal::BlobMismatch { sha256: digest });
    }

    // And they must satisfy the accepted verifier, which binds ref, digest and profile together.
    let declared = sage_db_format::rules_read::DeclaredRules {
        rules_ref: rules.rules_ref,
        rules_digest: rules.rules_digest,
        profile: rules.profile,
    };
    match sage_db_format::rules_read::verify_rules(bytes, &declared, limits, ledger) {
        FormatOutcome::Ok(_verified) => {}
        FormatOutcome::Format(error) => return Err(CommitRefusal::Format(error)),
        FormatOutcome::Refused(refusal) => return Err(CommitRefusal::Refused(refusal)),
    }

    // The seam has no flush-on-read-handle: `sync_file` takes a WRITE handle, and this file is one this attempt did
    // not write. `sync_entry` exists for exactly that case ("reuse … must make bytes durable that this attempt did
    // not write"), so it is the correct primitive here. The read handle is still held across verification above,
    // which is what ties the bytes verified to the file read; `sync_entry` then makes that named file durable.
    if let Err(error) = target.fs.sync_entry(target.blobs, entry) {
        return Err(CommitRefusal::Filesystem {
            step: CommitStep::PublishBlob,
            error,
        });
    }
    if let Err(error) = target.fs.sync_dir(target.blobs) {
        return Err(CommitRefusal::Filesystem {
            step: CommitStep::PublishBlob,
            error,
        });
    }
    Ok(())
}

/// Check that the declared blob list is a superset of every canonical reference in the transaction's events.
///
/// S2 SEAM §2: the declarations must cover all references the format enumerates, so a commit cannot publish a
/// transaction naming a blob it never declared. Enumeration is format's (`for_each_blob_ref_fields`), never a
/// hand-rolled tag list — and `192 result_ref` is included because it is in the canonical set.
///
/// # Errors
///
/// `UndeclaredRef` naming the first reference the declaration list omits.
fn check_declared_superset(
    events: &[sage_db_types::prepared::OwnedEvent],
    declarations: &[sage_db_types::prepared::BlobDeclaration],
    ledger: &mut dyn SharedLedger,
) -> Result<(), CommitRefusal> {
    /// Checks declaration containment DURING enumeration, so no reference table is built.
    ///
    /// A `Vec<[u8; 32]>` collector would be an infallible `push` on a bounded path, and root's point is that the
    /// containment question can be answered as each reference arrives.
    struct Containment<'a> {
        declarations: &'a [sage_db_types::prepared::BlobDeclaration],
        first_missing: Option<[u8; 32]>,
    }

    impl sage_db_types::framing::BlobRefSink for Containment<'_> {
        fn push(
            &mut self,
            _tag: u16,
            sha256: [u8; 32],
            _ledger: &mut dyn SharedLedger,
        ) -> Result<(), Refusal> {
            if self.first_missing.is_some() {
                // Already refused; keep the first one, which is the diagnostic a caller acts on.
                return Ok(());
            }
            if !self.declarations.iter().any(|d| d.sha256 == sha256) {
                self.first_missing = Some(sha256);
            }
            Ok(())
        }
    }

    for event in events {
        // One field descriptor table per event, admitted before it is built.
        let table_bytes = (event.fields.len() as u64)
            .saturating_mul(core::mem::size_of::<sage_db_format::event::Field<'_>>() as u64);
        if let Err(error) = ledger.try_charge(table_bytes) {
            return Err(CommitRefusal::Refused(Refusal::Budget(error)));
        }
        if let Err(stop) = ledger.checkpoint() {
            return Err(CommitRefusal::Refused(Refusal::Stopped(stop)));
        }
        let mut fields: Vec<sage_db_format::event::Field<'_>> = Vec::new();
        if fields.try_reserve_exact(event.fields.len()).is_err() {
            return Err(CommitRefusal::Refused(Refusal::Allocation {
                requested: table_bytes as usize,
            }));
        }
        for (tag, value) in &event.fields {
            fields.push(sage_db_format::event::Field::new(*tag, value.as_slice()));
        }

        let mut containment = Containment {
            declarations,
            first_missing: None,
        };
        match sage_db_format::dispatch::for_each_blob_ref_fields(&fields, &mut containment, ledger)
        {
            Ok(()) => {}
            Err(refusal) => return Err(CommitRefusal::Refused(refusal)),
        }
        if let Some(sha256) = containment.first_missing {
            return Err(CommitRefusal::UndeclaredRef { sha256 });
        }
    }
    Ok(())
}

/// Whether a publish reused an existing name.
#[must_use]
pub fn was_already_present(published: &crate::fs::Published) -> bool {
    published.how == PublishMode::AlreadyPresent
}

/// The opened families, named rather than ordered.
pub struct Families<F: FileSystem> {
    /// `staging/`.
    pub staging: F::Dir,
    /// `blobs/`.
    pub blobs: F::Dir,
    /// `transactions/`.
    pub transactions: F::Dir,
    /// `manifests/`.
    pub manifests: F::Dir,
}

/// The families a commit expects; `init` creates them.
#[must_use]
pub const fn required_families() -> [Family; 4] {
    [
        Family::Staging,
        Family::Blobs,
        Family::Transactions,
        Family::Manifests,
    ]
}

/// Open the families a commit needs, without creating any.
///
/// # Errors
///
/// `NotFound` when the KB was never initialized.
pub fn open_families<F: FileSystem>(fs: &F, root: &F::Dir) -> Result<Families<F>, FsError> {
    Ok(Families {
        staging: fs.open_family(root, Family::Staging, CreateDir::Never)?,
        blobs: fs.open_family(root, Family::Blobs, CreateDir::Never)?,
        transactions: fs.open_family(root, Family::Transactions, CreateDir::Never)?,
        manifests: fs.open_family(root, Family::Manifests, CreateDir::Never)?,
    })
}

#[cfg(test)]
mod persisting_phase_tests {
    //! The phase adapter's ONE behavioural difference, and that it does not become a second allowance.
    use super::PersistingPhase;
    use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
    use sage_db_types::query::ControlStop;

    /// A ledger that refuses every cancel and records what it was asked.
    struct Cancelling {
        charges: u64,
        checkpoints: u64,
    }
    impl SharedLedger for Cancelling {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            self.charges = self.charges.saturating_add(bytes);
            Ok(())
        }
        fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            self.checkpoints += 1;
            Err(ControlStop::Cancelled)
        }
    }

    #[test]
    fn persisting_ignores_the_caller_stop_but_keeps_the_real_budget() {
        let mut inner = Cancelling {
            charges: 0,
            checkpoints: 0,
        };
        {
            let mut persisting = PersistingPhase::new(&mut inner);
            // A caller stop is a diagnostic once persisting: it must not abort.
            assert!(
                persisting.checkpoint().is_ok(),
                "a persisting attempt must not honour caller cancellation"
            );
            // Budget is still real and reaches the SAME counter.
            persisting.try_charge(64).expect("budget forwards");
            assert_eq!(
                persisting.observed_stop,
                Some(ControlStop::Cancelled),
                "the stop is recorded, not discarded"
            );
        }
        assert_eq!(inner.charges, 64, "the charge reached the same counter");
        assert_eq!(
            inner.checkpoints, 1,
            "the same control was consulted even though its cancellation cannot abort persistence"
        );
    }

    #[test]
    fn a_real_budget_refusal_still_surfaces_while_persisting() {
        struct NoBytes;
        impl SharedLedger for NoBytes {
            fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
                Err(BudgetError::new(BudgetUnit::Bytes, bytes, 0))
            }
            fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), ControlStop> {
                Ok(())
            }
        }
        let mut inner = NoBytes;
        let mut persisting = PersistingPhase::new(&mut inner);
        assert!(
            persisting.try_charge(1).is_err(),
            "the adapter must not swallow a genuine budget refusal"
        );
    }
}

#[cfg(test)]
mod shared_outcome_tests {
    use super::*;
    use crate::lease::{AttemptLedger, LeaseLedger};
    use sage_db_types::persist::{ActorOrigin, CreationAudit, Extensions};
    use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};

    fn event() -> (RequestOutcomeEvent, u64) {
        let mut principal = String::with_capacity(31);
        principal.push_str("system");
        let mut request = String::with_capacity(47);
        request.push_str("original-request");
        let mut actor = String::with_capacity(53);
        actor.push_str("recorded-actor");
        let mut reason = String::with_capacity(71);
        reason.push_str("complete audit");
        let mut key = String::with_capacity(29);
        key.push_str("x-note");
        let mut value = Vec::with_capacity(137);
        value.extend_from_slice(b"original bytes");
        let mut slots = Vec::with_capacity(5);
        let payload = (principal.capacity()
            + request.capacity()
            + actor.capacity()
            + reason.capacity()
            + key.capacity()
            + value.capacity()
            + slots.capacity() * core::mem::size_of::<(String, Vec<u8>)>())
            as u64;
        slots.push((key, value));
        let event = RequestOutcomeEvent {
            principal_id: principal,
            request_id: request,
            payload_digest: [2; 32],
            result_ref: [3; 32],
            commit_version: 2,
            audit: CreationAudit {
                recorded_by: Some(actor),
                actor_origin: ActorOrigin::Authenticated,
                reason: Some(reason),
                created_at: 7,
            },
            extensions: Extensions::from_canonical(slots).unwrap(),
        };
        (event, payload)
    }

    #[test]
    fn reply_payload_and_block_survive_the_attempt_and_release_on_last_clone() {
        let control = RunToCompletion;
        let (reply, observer, held) = {
            let mut ledger = AttemptLedger::open(1 << 20, 1000, &control).unwrap();
            let observer = ledger.reserve_owned(0).unwrap();
            let (event, payload) = event();
            ledger.try_charge(payload).unwrap();
            let before = ledger.stats();
            let reply = SharedOutcome::new(event, &mut ledger).unwrap();
            assert_eq!(
                reply.inner._payload.bytes(),
                payload,
                "all actual nested capacities transfer, including extension slots"
            );
            assert_eq!(
                ledger.stats().admitted_bytes.unwrap() - before.admitted_bytes.unwrap(),
                u128::from(reply.inner._owner.bytes()),
                "only the NEW owner block is admitted"
            );
            let held = reply.inner._owner.bytes() + payload;
            (reply, observer, held)
        };
        assert_eq!(observer.stats().live_bytes, held);
        assert_eq!(observer.stats().untransferred_bytes, 0);
        let other = reply.clone();
        assert!(triomphe::Arc::ptr_eq(&reply.inner, &other.inner));
        assert_eq!(observer.stats().held_bytes, held);
        drop(reply);
        assert_eq!(
            other.event().audit.reason.as_deref(),
            Some("complete audit")
        );
        assert_eq!(other.event().extensions.entries()[0].1, b"original bytes");
        assert_eq!(observer.stats().live_bytes, held);
        drop(other);
        assert_eq!(observer.stats().live_bytes, 0);
        assert_eq!(observer.stats().held_bytes, 0);
    }

    #[test]
    fn reply_transfer_shortfall_and_fresh_budget_refusal_are_different() {
        let control = RunToCompletion;
        let mut ledger = AttemptLedger::open(1 << 20, 1000, &control).unwrap();
        let (event, payload) = event();
        let error = SharedOutcome::new(event, &mut ledger).unwrap_err();
        assert_eq!(
            error,
            SharedOutcomeError::Transfer(crate::lease::TransferError::Untransferred {
                requested_bytes: payload,
                available_bytes: 0,
            })
        );
        assert_eq!(
            ledger.stats().live_bytes,
            0,
            "failed transfer releases the newly reserved owner"
        );
        let (event, _) = self::event();
        let mut no_bytes = AttemptLedger::open(0, 1000, &control).unwrap();
        match SharedOutcome::new(event, &mut no_bytes).unwrap_err() {
            SharedOutcomeError::Refused(Refusal::Budget(error)) => assert!(error.requested() > 0),
            other => panic!("actual owner admission must stay Budget: {other:?}"),
        }
        assert_eq!(no_bytes.stats().live_bytes, 0);
    }

    #[test]
    fn reply_construction_preserves_the_actual_control_stop() {
        struct Expired;
        impl QueryControl for Expired {
            fn checkpoint(&self) -> Result<(), ControlStop> {
                Err(ControlStop::DeadlineExceeded)
            }
        }
        let mut ledger = AttemptLedger::open(1 << 20, 1000, &Expired).unwrap();
        let (event, _) = event();
        assert_eq!(
            SharedOutcome::new(event, &mut ledger).unwrap_err(),
            SharedOutcomeError::Refused(Refusal::Stopped(ControlStop::DeadlineExceeded))
        );
        assert_eq!(ledger.stats().live_bytes, 0);
    }
}
