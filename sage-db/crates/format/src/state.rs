//! Canonical `state_digest` assembly: `storage-digests.md` §4.4.
//!
//! Input is the **fold's already-sorted record set**, not an unordered slice. Two rules
//! that are easy to get wrong and are therefore checked here rather than assumed:
//!
//! * `state kind` is [`StateKind`], **not** [`RecordKind`](sage_db_types::persist::RecordKind).
//!   Kinds 10..=13 (Asset, Model, KbMetadata, RuleConfig) have no numeric record identity
//!   at all, so an entry's raw `u16` is decoded through the closed `1..=13` domain and a
//!   14 or 0 is refused rather than folded into a digest nobody else computes.
//! * Ordering is `kind` numeric, then **key bytes** unsigned (§4.4.2). A little-endian
//!   `u32`/`u128` key therefore does *not* sort numerically, so "the LE representation
//!   happens to be ordered" is a defect that appears at the first id above `0xff`.
//!   [`RecordSet`] is what enforces the order; this module never sorts.
//!
//! This module owns framing and hashing only. It does not project event fields into a
//! body: §4.4.3's projection (omit exactly tags 1 and 13, keep everything else including
//! `status`, `legacy_*` and `extensions`) belongs to the fold that produced the body.
//! What it *does* enforce is that a supplied body is a well-formed `Field` sequence and
//! does not smuggle back the two tags the projection must omit.

use sage_db_types::ledger::{BudgetError, SharedLedger};
use sage_db_types::persist::{NaturalKey, RecordSet, StateEntry, StateKind};
use sage_db_types::query::ControlStop;

use crate::digest::{Domain, digest};
use crate::registry::is_registered_field;
use crate::wire::Reader;

/// `record_key` (tag 1) is already `Entry.kind` + `Entry.key`.
const TAG_RECORD_KEY: u16 = 1;
/// `changed_fields` (tag 13) is reconstructible from adjacent after-images.
const TAG_CHANGED_FIELDS: u16 = 13;

/// `model_key` bound from the field table (`storage-format.md` §6.2, tag 149).
const MAX_MODEL_KEY_BYTES: usize = 120;

/// Bytes an entry contributes besides its key and body: `kind:u16 | key_len:u32 |
/// revision:u64 | body_len:u32`.
const ENTRY_FIXED_BYTES: usize = 2 + 4 + 8 + 4;

/// Why a state payload was not assembled.
///
/// The refusal arms are **not** data verdicts and keep their own remedies
/// (retry bigger, resubmit, log a defect). They are never reported as corruption.
///
/// [`DigestAssemblyError::Structure`] is the fold-side class: the bytes never contained
/// this payload, so it is not a `CorruptReason` and is deliberately not mapped onto one.
/// A caller that needs the four byte classes can keep the refusal arms via
/// `impl From<DigestAssemblyError> for Refusal` and classify `Structure` as
/// [`sage_db_types::framing::OperatorAction::ReportDefect`]; giving it a `Corrupt`
/// mapping would make a caller bug look like a damaged disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DigestAssemblyError {
    /// This attempt's ledger refused the payload bytes.
    Budget(BudgetError),
    /// The caller's policy asked to stop.
    Stopped(ControlStop),
    /// The allocator refused a buffer. Nothing was published.
    Allocation {
        /// Bytes that were needed.
        requested: usize,
    },
    /// The supplied projection is not canonical. A fold defect, not a file verdict.
    Structure(StructureDefect),
}

impl From<BudgetError> for DigestAssemblyError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<ControlStop> for DigestAssemblyError {
    fn from(stop: ControlStop) -> Self {
        Self::Stopped(stop)
    }
}

impl DigestAssemblyError {
    /// This outcome's refusal, when it has one.
    ///
    /// `None` for [`Self::Structure`]: a non-canonical projection is the caller's own
    /// defect, so there is no `Refusal` variant that describes it and none is fabricated.
    /// Deliberately **not** a `From<DigestAssemblyError> for Refusal` impl: a conversion
    /// would have to invent an `Allocation` for the structural arm, which would report
    /// "the allocator refused" for a fold bug and send an operator after the wrong cause.
    ///
    /// A caller that needs the four byte classes should map the refusal arms through this
    /// accessor and classify [`Self::Structure`] as
    /// [`sage_db_types::framing::OperatorAction::ReportDefect`].
    #[must_use]
    pub const fn as_refusal(self) -> Option<sage_db_types::framing::Refusal> {
        use sage_db_types::framing::Refusal;
        match self {
            Self::Budget(budget) => Some(Refusal::Budget(budget)),
            Self::Stopped(stop) => Some(Refusal::Stopped(stop)),
            Self::Allocation { requested } => Some(Refusal::Allocation { requested }),
            Self::Structure(_) => None,
        }
    }
}

/// What exactly was non-canonical. A distinct variant per rule so a test can name the
/// rule it broke instead of matching one opaque "bad input".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StructureDefect {
    /// `state kind` outside the registered `1..=13`.
    UnregisteredKind {
        /// Entry position in the set.
        index: usize,
        /// The kind that appeared.
        kind: u16,
    },
    /// The key length does not match the kind's natural-key form.
    WrongKeyWidth {
        /// Entry position in the set.
        index: usize,
        /// The entry's kind.
        kind: u16,
        /// Bytes that appeared.
        found: usize,
    },
    /// A non-singleton record carried the empty key, which §4.4.2 reserves for singletons.
    EmptyNaturalKey {
        /// Entry position in the set.
        index: usize,
        /// The entry's kind.
        kind: u16,
    },
    /// A revision of zero: no record uses one, and `Entry.revision` has no `Option`.
    ZeroRevision {
        /// Entry position in the set.
        index: usize,
    },
    /// An immutable record carried a revision other than the fixed `1` (§4.4.4).
    ImmutableRevision {
        /// Entry position in the set.
        index: usize,
        /// The revision that appeared.
        revision: u64,
    },
    /// The body carried a tag the projection must omit (1 or 13).
    BodyForbiddenTag {
        /// Entry position in the set.
        index: usize,
        /// The tag that appeared.
        tag: u16,
    },
    /// The body is not a well-formed `Field` sequence.
    BodyMalformed {
        /// Entry position in the set.
        index: usize,
        /// Byte offset in the body where the walk stopped.
        offset: usize,
    },
    /// More entries or bytes than the `u32` prefix can express.
    TooLarge,
    /// The caller's buffer already held bytes.
    BufferNotEmpty,
    /// Graph projection: terms were not strictly ascending by id, or one id repeated.
    TermsNotAscending {
        /// Position in the caller's slice.
        index: usize,
    },
    /// Graph projection: assertions were not strictly ascending by id, or one repeated.
    AssertionsNotAscending {
        /// Position in the caller's slice.
        index: usize,
    },
    /// Graph projection: an assertion named a relation id outside `0..=6`.
    UnknownRelation {
        /// Position in the caller's slice.
        index: usize,
    },
    /// Bookkeeping: outcomes were not strictly ascending by `(principal_id, request_id)`.
    OutcomesNotAscending {
        /// Position in the caller's slice.
        index: usize,
    },
    /// Bookkeeping: imports were not strictly ascending by `import_session_id`.
    ImportsNotAscending {
        /// Position in the caller's slice.
        index: usize,
    },
    /// Bookkeeping: counters were not strictly ascending by space.
    CountersNotAscending {
        /// Position in the caller's slice.
        index: usize,
    },
    /// Bookkeeping: an outcome carried the reserved all-zero `payload_digest` sentinel
    /// that §4.7.1 reserves for "this transaction is not a caller request".
    SentinelPayloadDigest {
        /// Position in the caller's slice.
        index: usize,
    },
    /// Bookkeeping: a `high_water_mark` exceeded its space's maximum id.
    WatermarkBeyondSpace {
        /// Position in the caller's slice.
        index: usize,
    },
    /// Bookkeeping: `expected_counts` exceeded its registered maximum (32).
    ExpectedCountsTooLong {
        /// Position in the caller's import slice.
        index: usize,
    },
    /// Bookkeeping: `expected_counts` was not strictly ascending by kind, or a kind
    /// repeated.
    ExpectedCountsNotAscending {
        /// Position in the caller's import slice.
        index: usize,
    },
    /// Bookkeeping: an identity component a natural key is built from was empty, which
    /// would make two different keys compare equal.
    EmptyIdentity {
        /// Position in the caller's slice.
        index: usize,
        /// Which component was empty.
        field: &'static str,
    },
}

impl StructureDefect {
    /// Wrap this defect as the fold-side refusal class.
    ///
    /// `pub(crate)` rather than private because `graph` and `bookkeeping` report the same
    /// class of defect through the same error type; three copies of this wrapper would let
    /// them drift into three error surfaces.
    #[must_use]
    pub(crate) const fn error(self) -> DigestAssemblyError {
        DigestAssemblyError::Structure(self)
    }
}

/// Encode the §4.4 payload into `out`, which must be empty.
///
/// A caller that commits repeatedly can keep one buffer and clear it between uses: the
/// extent is admitted once and `try_reserve_exact` reuses the capacity, so a reused buffer
/// costs nothing extra. [§7.7(1)](../../../docs/design/storage-digests.md) requires the
/// admission to happen **before** the allocation, which is what the two passes do.
///
/// # Errors
///
/// [`DigestAssemblyError`]. On any error `out` is left empty: no partial payload is ever
/// published as a successful encoding.
pub fn encode_state_payload(
    set: &RecordSet,
    out: &mut Vec<u8>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), DigestAssemblyError> {
    // An empty operation is still an operation: a cancelled attempt must be observed
    // even when the set has no entries to walk.
    ledger.checkpoint()?;
    if !out.is_empty() {
        return Err(StructureDefect::BufferNotEmpty.error());
    }
    let count = u32::try_from(set.len()).map_err(|_| StructureDefect::TooLarge.error())?;

    // Pass 1 validates and measures. The digest prefix carries the payload length, so the
    // extent must be known before a byte is written; measuring first is what makes one
    // exact admission possible instead of a growing, repeatedly reserved buffer.
    let mut total = 4usize;
    for (index, entry) in set.entries().iter().enumerate() {
        ledger.checkpoint()?;
        total = total
            .checked_add(validate_entry(index, entry)?)
            .ok_or(StructureDefect::TooLarge.error())?;
    }

    ledger.try_charge(total as u64)?;
    out.try_reserve_exact(total)
        .map_err(|_| DigestAssemblyError::Allocation { requested: total })?;

    // Pass 2 writes. No element is copied into a temporary: the bytes come straight from
    // the caller's key and body.
    out.extend_from_slice(&count.to_le_bytes());
    for entry in set.entries() {
        let key_len =
            u32::try_from(entry.key.len()).map_err(|_| StructureDefect::TooLarge.error())?;
        let body_len =
            u32::try_from(entry.body.len()).map_err(|_| StructureDefect::TooLarge.error())?;
        out.extend_from_slice(&entry.kind.to_le_bytes());
        out.extend_from_slice(&key_len.to_le_bytes());
        out.extend_from_slice(&entry.key);
        out.extend_from_slice(&entry.revision.to_le_bytes());
        out.extend_from_slice(&body_len.to_le_bytes());
        out.extend_from_slice(&entry.body);
    }
    debug_assert_eq!(out.len(), total, "measured extent must match written bytes");
    Ok(())
}

/// `state_digest` over an assembled payload.
///
/// The payload is materialised because the digest prefix carries its length:
/// [`digest`] hashes `prefix ‖ u64be(len) ‖ payload` over one slice. That buffer is the
/// only temporary allocation, admitted for its exact extent before it is reserved.
/// [`encode_state_payload`] exists so a caller that already owns a buffer avoids even it.
///
/// # Errors
///
/// As [`encode_state_payload`].
pub fn state_digest(
    set: &RecordSet,
    ledger: &mut dyn SharedLedger,
) -> Result<[u8; 32], DigestAssemblyError> {
    let mut payload = Vec::new();
    encode_state_payload(set, &mut payload, ledger)?;
    // The payload bytes were charged while it was assembled; the scan itself is a control
    // checkpoint, not a second charge for the same bytes in one attempt.
    ledger.checkpoint()?;
    Ok(digest(Domain::State, &payload))
}

/// Validate one entry and return the bytes it will contribute.
fn validate_entry(index: usize, entry: &StateEntry) -> Result<usize, DigestAssemblyError> {
    let kind = StateKind::from_u16(entry.kind).ok_or(
        StructureDefect::UnregisteredKind {
            index,
            kind: entry.kind,
        }
        .error(),
    )?;
    let (min, max) = key_width(kind);
    if entry.key.len() < min || entry.key.len() > max {
        // The empty key is reported as its own case: legal for singletons, illegal
        // everywhere else, and "width 0" would hide which rule was broken.
        if entry.key.is_empty() && max != 0 {
            return Err(StructureDefect::EmptyNaturalKey {
                index,
                kind: entry.kind,
            }
            .error());
        }
        return Err(StructureDefect::WrongKeyWidth {
            index,
            kind: entry.kind,
            found: entry.key.len(),
        }
        .error());
    }
    if entry.revision == 0 {
        return Err(StructureDefect::ZeroRevision { index }.error());
    }
    // §4.4.4: Asset is content-addressed and immutable, so its revision is the explicit
    // constant 1 rather than an invented number.
    if !kind.is_revisable() && entry.revision != 1 {
        return Err(StructureDefect::ImmutableRevision {
            index,
            revision: entry.revision,
        }
        .error());
    }
    check_body(index, &entry.body)?;
    Ok(ENTRY_FIXED_BYTES + entry.key.len() + entry.body.len())
}

/// The natural-key width of a kind: `(min, max)` bytes; `(0, 0)` for a singleton.
///
/// Numeric-identity kinds carry their id little-endian, so Term is 4 bytes and the rest
/// are 16. A Term key of 16 bytes would otherwise accept an `Assertion` key under kind 1.
const fn key_width(kind: StateKind) -> (usize, usize) {
    match kind.natural_key() {
        Some(NaturalKey::Bytes32) => (32, 32),
        // `model_key` is a UTF-8 name: at least one byte, at most the field bound.
        Some(NaturalKey::Utf8) => (1, MAX_MODEL_KEY_BYTES),
        Some(NaturalKey::Singleton) => (0, 0),
        None => match kind {
            StateKind::Term => (4, 4),
            _ => (16, 16),
        },
    }
}

/// Walk the body as a `Field` sequence, enforcing what made it a body in the first place.
fn check_body(index: usize, body: &[u8]) -> Result<(), DigestAssemblyError> {
    let mut reader = Reader::new(body);
    let mut previous = 0u16;
    while !reader.is_empty() {
        let offset = reader.offset();
        let (Ok(tag), Ok(flags), Ok(len)) = (reader.u16(), reader.u16(), reader.u32()) else {
            return Err(StructureDefect::BodyMalformed { index, offset }.error());
        };
        if flags != 0 || len == 0 || tag <= previous {
            return Err(StructureDefect::BodyMalformed { index, offset }.error());
        }
        if tag == TAG_RECORD_KEY || tag == TAG_CHANGED_FIELDS {
            return Err(StructureDefect::BodyForbiddenTag { index, tag }.error());
        }
        if !is_registered_field(tag) {
            return Err(StructureDefect::BodyMalformed { index, offset }.error());
        }
        if reader.take_exact(len as usize).is_err() {
            return Err(StructureDefect::BodyMalformed { index, offset }.error());
        }
        previous = tag;
    }
    Ok(())
}
