//! The P2 frontier-entry operation registry (v1) and its two payload codecs.
//!
//! P1 owns the request *frame*; P2 assigns meaning to `op_kind`, `op_version` and the
//! payload bytes, so this module is the one place those three numbers and their layouts are
//! defined for frontier auxiliary state.
//!
//! # What this is not
//!
//! A [`FrontierEntryRecord`](sage_db_types::persist::FrontierEntryRecord) is an **auxiliary
//! note about unexplored territory**, not graph content and not a proposal. Nothing here
//! touches Terms, Assertions, the graph or proposal semantics.
//!
//! # Layouts (little-endian, no padding)
//!
//! ```text
//! CreateFrontierEntry v1 payload
//!   refs_count   u32                                  (<= 64)
//!   concept_refs [RecordKey; refs_count]              (kind must be Term, ascending unique)
//!   text         str        len:u32 | UTF-8 bytes     (empty allowed)
//!   priority     u16                                  (0 high, 1 medium, 2 low)
//!   status       u16                                  (0 open, 1 resolved)
//!
//! ReviseFrontierEntry v1 payload
//!   frontier     RecordKey  kind:u16 | id:u128        (kind must be FrontierEntry)
//!   <the same complete state: refs_count, concept_refs, text, priority, status>
//! ```
//!
//! A revision carries the **complete** state rather than a patch: the record's after-image is
//! what gets persisted, and a patch would need a merge rule this format does not define.
//!
//! # Tag authority
//!
//! The canonical writer for this family is `sage_db_format::collection::write_frontier`,
//! whose event tag is `FRONTIER_CHANGED = 112` and whose own codec is the sole authority for
//! the record bytes. This module defines only the *operation payload* — the caller's
//! statement of intent, not a second record encoding — so it must not restate or reinterpret
//! that tag.
//!
//! # Resources
//!
//! The payload is the single operation of the P1 frame, so the frame's `MAX_PAYLOAD` ceiling
//! is this ceiling too — imported from [`sage_db_store::typed::MAX_PAYLOAD`] rather than
//! restated, and enforced **before** anything is admitted, so an impossible payload never
//! becomes an allocation. Decoding is metered through the caller's own ledger.

use sage_db_store::typed::MAX_PAYLOAD;
use sage_db_types::framing::Refusal;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{RecordKey, RecordKind};

/// Operation kind of `CreateFrontierEntry`.
pub const CREATE_FRONTIER: u16 = 5;
/// Operation kind of `ReviseFrontierEntry`.
pub const REVISE_FRONTIER: u16 = 6;
/// The only payload version this slice registers, for both operations.
pub const OPERATION_VERSION: u16 = 1;

/// Bytes of one encoded `RecordKey`: `kind:u16 | id:u128`.
const KEY_BYTES: usize = 18;
/// Bytes of a `str` or count prefix.
const LEN_BYTES: usize = 4;
/// Maximum listed concept references, matching the canonical codec's vector bound.
pub const MAX_REFS: u32 = 64;

/// One frontier entry as the caller states it.
///
/// `text` is the entry's prose, copied verbatim: no trimming, normalising or re-encoding
/// happens anywhere between here and the blob it becomes. An empty text is a legal,
/// genuinely empty blob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrontierState<'a> {
    /// Concepts this entry is about; may be empty (a subjectless frontier entry is valid).
    pub concept_refs: &'a [RecordKey],
    /// The entry's prose; may be empty.
    pub text: &'a str,
    /// Priority as authored: 0 high, 1 medium, 2 low.
    pub priority: u16,
    /// Lifecycle status: 0 open, 1 resolved.
    pub status: u16,
}

/// Why a frontier-entry payload is not acceptable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrontierPayloadError {
    /// The bytes ended before a declared value did.
    Truncated,
    /// Bytes remained after the last declared value.
    Trailing,
    /// A record kind value is not in the closed registry.
    UnknownKind(u16),
    /// A key's id exceeds the bound its kind declares.
    IdOutOfRange {
        /// The kind whose bound was exceeded.
        kind: RecordKind,
    },
    /// A key carried a kind this position does not accept.
    WrongKind {
        /// The kind the position requires.
        expected: RecordKind,
        /// The kind that appeared.
        found: RecordKind,
    },
    /// More than [`MAX_REFS`] concept references were listed.
    TooManyRefs(u32),
    /// Listed concept references were not strictly ascending and unique.
    RefsNotAscending {
        /// Index of the offending reference.
        index: usize,
    },
    /// A length-prefixed value is not valid UTF-8.
    NotUtf8,
    /// A generated length does not fit its `u32` prefix.
    LengthOverflow,
    /// `priority` is outside {0, 1, 2}.
    UnknownPriority(u16),
    /// `status` is outside {0, 1}.
    UnknownStatus(u16),
    /// The encoded or supplied payload exceeds the registered operation ceiling.
    TooLarge {
        /// Measured byte length.
        len: u64,
        /// The registered ceiling.
        max: u32,
    },
    /// This module's own layout arithmetic disagreed with the bytes it emitted.
    ///
    /// Unreachable through the public encoders, which build a payload of a length they
    /// already admitted; it exists so an impossible arm reports itself as an internal
    /// defect rather than as a resource refusal.
    InternalInvariant,
    /// The caller's ledger refused or stopped the work.
    Refused(Refusal),
}

impl From<Refusal> for FrontierPayloadError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// A decoded `CreateFrontierEntry` payload, borrowing the caller's bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DecodedCreate<'a> {
    /// Concept references, ascending and unique. Owned: they are derived by decoding,
    /// not borrowed from the payload, so they cannot be a slice view of it.
    pub concept_refs: Vec<RecordKey>,
    /// The entry's prose.
    pub text: &'a str,
    /// Priority as authored.
    pub priority: u16,
    /// Lifecycle status.
    pub status: u16,
}

/// A decoded `ReviseFrontierEntry` payload, borrowing the caller's bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DecodedRevise<'a> {
    /// The entry being revised.
    pub frontier: RecordKey,
    /// Complete replacement state.
    pub state: DecodedCreate<'a>,
}

/// A bounds-checked cursor over a payload's bytes.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], FrontierPayloadError> {
        let end = self
            .at
            .checked_add(len)
            .ok_or(FrontierPayloadError::Truncated)?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(FrontierPayloadError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn u16(&mut self) -> Result<u16, FrontierPayloadError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, FrontierPayloadError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u128(&mut self) -> Result<u128, FrontierPayloadError> {
        let bytes = self.take(16)?;
        let mut raw = [0u8; 16];
        raw.copy_from_slice(bytes);
        Ok(u128::from_le_bytes(raw))
    }

    fn key(&mut self) -> Result<RecordKey, FrontierPayloadError> {
        let raw_kind = self.u16()?;
        let kind =
            RecordKind::from_u16(raw_kind).ok_or(FrontierPayloadError::UnknownKind(raw_kind))?;
        let key = RecordKey::new(kind, self.u128()?);
        if !key.id_is_in_range() {
            return Err(FrontierPayloadError::IdOutOfRange { kind });
        }
        Ok(key)
    }

    fn str(&mut self) -> Result<&'a str, FrontierPayloadError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).map_err(|_| FrontierPayloadError::NotUtf8)
    }

    fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// Charge and check one decode pass over `len` bytes.
///
/// The charge is the work actually performed, not a guess: a pass walks every byte once.
fn admit_pass(ledger: &mut dyn SharedLedger, len: usize) -> Result<(), FrontierPayloadError> {
    ledger
        .checkpoint()
        .map_err(|stop| FrontierPayloadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge(len as u64)
        .map_err(|error| FrontierPayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .try_charge_entries(1)
        .map_err(|error| FrontierPayloadError::Refused(Refusal::Budget(error)))?;
    Ok(())
}

/// The declared ceiling check shared by both directions.
pub(crate) fn check_ceiling(len: u64) -> Result<(), FrontierPayloadError> {
    if len > u64::from(MAX_PAYLOAD) {
        return Err(FrontierPayloadError::TooLarge {
            len,
            max: MAX_PAYLOAD,
        });
    }
    Ok(())
}

/// Validate a key's domain before any bytes are produced.
fn check_key(key: RecordKey, required: RecordKind) -> Result<(), FrontierPayloadError> {
    if !key.id_is_in_range() {
        return Err(FrontierPayloadError::IdOutOfRange { kind: key.kind() });
    }
    if key.kind() != required {
        return Err(FrontierPayloadError::WrongKind {
            expected: required,
            found: key.kind(),
        });
    }
    Ok(())
}

/// Validate the complete state both directions share, without touching bytes.
///
/// Order matters: count, then per-ref kind and ordering, then the scalar domains. A
/// reference list that is not strictly ascending is refused rather than sorted, because
/// sorting would silently change which set the caller asserted.
/// Count and scalar-domain checks only, with no per-key kind or ordering work.
///
/// Split out so a command can bound the list and its domains first, then report a caller's
/// wrong key *kind* as a key error rather than as a payload-shape error, then run the full
/// state check. None of these steps allocate.
pub(crate) fn check_shape(state: &FrontierState<'_>) -> Result<(), FrontierPayloadError> {
    let count = u32::try_from(state.concept_refs.len())
        .map_err(|_| FrontierPayloadError::TooManyRefs(u32::MAX))?;
    if count > MAX_REFS {
        return Err(FrontierPayloadError::TooManyRefs(count));
    }
    if state.priority > 2 {
        return Err(FrontierPayloadError::UnknownPriority(state.priority));
    }
    if state.status > 1 {
        return Err(FrontierPayloadError::UnknownStatus(state.status));
    }
    Ok(())
}

pub(crate) fn check_state(state: &FrontierState<'_>) -> Result<(), FrontierPayloadError> {
    let count = u32::try_from(state.concept_refs.len())
        .map_err(|_| FrontierPayloadError::TooManyRefs(u32::MAX))?;
    if count > MAX_REFS {
        return Err(FrontierPayloadError::TooManyRefs(count));
    }
    for (index, key) in state.concept_refs.iter().enumerate() {
        check_key(*key, RecordKind::Term)?;
        if index > 0 && state.concept_refs[index - 1] >= *key {
            return Err(FrontierPayloadError::RefsNotAscending { index });
        }
    }
    if state.priority > 2 {
        return Err(FrontierPayloadError::UnknownPriority(state.priority));
    }
    if state.status > 1 {
        return Err(FrontierPayloadError::UnknownStatus(state.status));
    }
    Ok(())
}

/// Admit exactly `total` bytes, then return the reserved buffer.
fn admit(ledger: &mut dyn SharedLedger, total: usize) -> Result<Vec<u8>, FrontierPayloadError> {
    check_ceiling(total as u64)?;
    ledger
        .checkpoint()
        .map_err(|stop| FrontierPayloadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge(total as u64)
        .map_err(|error| FrontierPayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| FrontierPayloadError::Refused(Refusal::Stopped(stop)))?;
    let mut out = Vec::new();
    out.try_reserve_exact(total)
        .map_err(|_| FrontierPayloadError::Refused(Refusal::Allocation { requested: total }))?;
    Ok(out)
}

fn put_key(out: &mut Vec<u8>, key: RecordKey) {
    out.extend_from_slice(&key.kind().as_u16().to_le_bytes());
    out.extend_from_slice(&key.id().to_le_bytes());
}

/// Bytes of the shared state suffix: count, refs, text prefix+bytes, priority, status.
pub(crate) fn state_len(state: &FrontierState<'_>) -> Result<usize, FrontierPayloadError> {
    let refs = state
        .concept_refs
        .len()
        .checked_mul(KEY_BYTES)
        .ok_or(FrontierPayloadError::LengthOverflow)?;
    LEN_BYTES
        .checked_add(refs)
        .and_then(|n| n.checked_add(LEN_BYTES))
        .and_then(|n| n.checked_add(state.text.len()))
        .and_then(|n| n.checked_add(2))
        .and_then(|n| n.checked_add(2))
        .ok_or(FrontierPayloadError::LengthOverflow)
}

fn put_state(out: &mut Vec<u8>, state: &FrontierState<'_>, count: u32, text_len: u32) {
    out.extend_from_slice(&count.to_le_bytes());
    for key in state.concept_refs {
        put_key(out, *key);
    }
    out.extend_from_slice(&text_len.to_le_bytes());
    out.extend_from_slice(state.text.as_bytes());
    out.extend_from_slice(&state.priority.to_le_bytes());
    out.extend_from_slice(&state.status.to_le_bytes());
}

/// Encode a `CreateFrontierEntry` v1 payload.
///
/// # Errors
///
/// [`FrontierPayloadError::TooManyRefs`] past the vector bound,
/// [`FrontierPayloadError::WrongKind`] / [`FrontierPayloadError::IdOutOfRange`] for a
/// non-term reference, [`FrontierPayloadError::RefsNotAscending`] for an unsorted or
/// duplicated list, [`FrontierPayloadError::UnknownPriority`] /
/// [`FrontierPayloadError::UnknownStatus`] outside their closed domains,
/// [`FrontierPayloadError::LengthOverflow`] for a value that cannot carry its `u32` prefix,
/// [`FrontierPayloadError::TooLarge`] when the assembled payload exceeds the operation
/// ceiling, and [`FrontierPayloadError::Refused`] for the caller's own budget, stop or
/// allocation refusal.
pub fn encode_create(
    state: &FrontierState<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, FrontierPayloadError> {
    check_state(state)?;
    let count = u32::try_from(state.concept_refs.len())
        .map_err(|_| FrontierPayloadError::LengthOverflow)?;
    let text_len =
        u32::try_from(state.text.len()).map_err(|_| FrontierPayloadError::LengthOverflow)?;
    let total = state_len(state)?;
    let mut out = admit(ledger, total)?;
    put_state(&mut out, state, count, text_len);
    if out.len() != total {
        return Err(FrontierPayloadError::InternalInvariant);
    }
    Ok(out)
}

/// Encode a `ReviseFrontierEntry` v1 payload.
///
/// # Errors
///
/// As [`encode_create`], plus [`FrontierPayloadError::WrongKind`] for a frontier key that is
/// not a `FrontierEntry`.
pub fn encode_revise(
    frontier: RecordKey,
    state: &FrontierState<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, FrontierPayloadError> {
    check_key(frontier, RecordKind::FrontierEntry)?;
    check_state(state)?;
    let count = u32::try_from(state.concept_refs.len())
        .map_err(|_| FrontierPayloadError::LengthOverflow)?;
    let text_len =
        u32::try_from(state.text.len()).map_err(|_| FrontierPayloadError::LengthOverflow)?;
    let total = KEY_BYTES
        .checked_add(state_len(state)?)
        .ok_or(FrontierPayloadError::LengthOverflow)?;
    let mut out = admit(ledger, total)?;
    put_key(&mut out, frontier);
    put_state(&mut out, state, count, text_len);
    if out.len() != total {
        return Err(FrontierPayloadError::InternalInvariant);
    }
    Ok(out)
}

/// Read the shared state suffix, returning it borrowing the payload.
fn read_state<'a>(
    cursor: &mut Cursor<'a>,
    ledger: &mut dyn SharedLedger,
) -> Result<(Vec<RecordKey>, &'a str, u16, u16), FrontierPayloadError> {
    let count = cursor.u32()?;
    if count > MAX_REFS {
        return Err(FrontierPayloadError::TooManyRefs(count));
    }
    // Charge the retained slots before reserving them, exactly as the canonical reader does.
    let slot_bytes = u64::from(count).saturating_mul(core::mem::size_of::<RecordKey>() as u64);
    ledger
        .try_charge_entries(u64::from(count))
        .map_err(|error| FrontierPayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .try_charge(slot_bytes)
        .map_err(|error| FrontierPayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| FrontierPayloadError::Refused(Refusal::Stopped(stop)))?;
    let mut concept_refs = Vec::new();
    concept_refs
        .try_reserve_exact(count as usize)
        .map_err(|_| {
            FrontierPayloadError::Refused(Refusal::Allocation {
                requested: slot_bytes as usize,
            })
        })?;
    let mut previous: Option<RecordKey> = None;
    for index in 0..count as usize {
        let next = cursor.key()?;
        if next.kind() != RecordKind::Term {
            return Err(FrontierPayloadError::WrongKind {
                expected: RecordKind::Term,
                found: next.kind(),
            });
        }
        if previous.is_some_and(|prev| prev >= next) {
            return Err(FrontierPayloadError::RefsNotAscending { index });
        }
        previous = Some(next);
        concept_refs.push(next);
    }
    let text = cursor.str()?;
    let priority = cursor.u16()?;
    if priority > 2 {
        return Err(FrontierPayloadError::UnknownPriority(priority));
    }
    let status = cursor.u16()?;
    if status > 1 {
        return Err(FrontierPayloadError::UnknownStatus(status));
    }
    Ok((concept_refs, text, priority, status))
}

/// Decode a `CreateFrontierEntry` v1 payload.
///
/// # Errors
///
/// [`FrontierPayloadError::TooLarge`] before any traversal,
/// [`FrontierPayloadError::Truncated`] / [`FrontierPayloadError::Trailing`] for a length
/// disagreement, an unknown or out-of-range kind, an unsorted or duplicated reference list,
/// a non-term reference, invalid UTF-8, or a scalar outside its closed domain, and
/// [`FrontierPayloadError::Refused`] for the caller's own budget, stop or allocation refusal.
pub fn decode_create<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedCreate<'a>, FrontierPayloadError> {
    check_ceiling(payload.len() as u64)?;
    admit_pass(ledger, payload.len())?;
    let mut cursor = Cursor::new(payload);
    let (refs, text, priority, status) = read_state(&mut cursor, ledger)?;
    if !cursor.at_end() {
        return Err(FrontierPayloadError::Trailing);
    }
    Ok(DecodedCreate {
        concept_refs: refs,
        text,
        priority,
        status,
    })
}

/// Decode a `ReviseFrontierEntry` v1 payload.
///
/// # Errors
///
/// As [`decode_create`], with the additional frontier-key refusal.
pub fn decode_revise<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedRevise<'a>, FrontierPayloadError> {
    check_ceiling(payload.len() as u64)?;
    admit_pass(ledger, payload.len())?;
    let mut cursor = Cursor::new(payload);
    let frontier = cursor.key()?;
    if frontier.kind() != RecordKind::FrontierEntry {
        return Err(FrontierPayloadError::WrongKind {
            expected: RecordKind::FrontierEntry,
            found: frontier.kind(),
        });
    }
    let (refs, text, priority, status) = read_state(&mut cursor, ledger)?;
    if !cursor.at_end() {
        return Err(FrontierPayloadError::Trailing);
    }
    Ok(DecodedRevise {
        frontier,
        state: DecodedCreate {
            concept_refs: refs,
            text,
            priority,
            status,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::query::RunToCompletion;

    fn ledger() -> sage_db_store::lease::AttemptLedger<'static> {
        static CONTROL: RunToCompletion = RunToCompletion;
        sage_db_store::lease::AttemptLedger::open(1 << 20, 1 << 16, &CONTROL).unwrap()
    }

    #[test]
    fn operation_kinds_match_the_assigned_registry() {
        assert_eq!(CREATE_FRONTIER, 5);
        assert_eq!(REVISE_FRONTIER, 6);
        assert_eq!(OPERATION_VERSION, 1);
    }

    #[test]
    fn an_empty_subjectless_state_round_trips() {
        let mut l = ledger();
        let state = FrontierState {
            concept_refs: &[],
            text: "",
            priority: 0,
            status: 0,
        };
        let bytes = encode_create(&state, &mut l).unwrap();
        let decoded = decode_create(&bytes, &mut l).unwrap();
        assert!(decoded.concept_refs.is_empty());
        assert_eq!(decoded.text, "");
        assert_eq!((decoded.priority, decoded.status), (0, 0));
    }

    #[test]
    fn unsorted_refs_are_refused_not_sorted() {
        let mut l = ledger();
        let refs = [RecordKey::term(2), RecordKey::term(1)];
        let state = FrontierState {
            concept_refs: &refs,
            text: "t",
            priority: 1,
            status: 0,
        };
        assert_eq!(
            encode_create(&state, &mut l),
            Err(FrontierPayloadError::RefsNotAscending { index: 1 })
        );
    }
}
