//! The P2 session-checkpoint operation registry (v1) and its two payload codecs.
//!
//! P1 owns the request *frame*; P2 assigns meaning to `op_kind`, `op_version` and the
//! payload bytes, so this module is the one place those three numbers and their layouts
//! are defined for auxiliary session state.
//!
//! # What this is not
//!
//! A [`SessionState`] checkpoint is **user-session state**, not a storage checkpoint and
//! not a whole-KB snapshot. Nothing here touches Terms, Claims, the graph or the storage
//! checkpoint machinery.
//!
//! # Layouts (little-endian, no padding)
//!
//! ```text
//! CreateSessionCheckpoint v1 payload
//!   concept     RecordKey  kind:u16 | id:u128        (kind must be Term)
//!   session_no  u32                                  (>= 1)
//!   content     str        len:u32 | UTF-8 bytes     (empty allowed)
//!
//! ReviseSessionCheckpoint v1 payload
//!   checkpoint  RecordKey  kind:u16 | id:u128        (kind must be SessionCheckpoint)
//!   concept     RecordKey  kind:u16 | id:u128        (kind must be Term)
//!   session_no  u32                                  (>= 1)
//!   content     str        len:u32 | UTF-8 bytes     (empty allowed)
//! ```
//!
//! The payload is the single operation of the P1 frame, so the frame's `MAX_PAYLOAD`
//! ceiling is the ceiling here too — imported from [`sage_db_store::typed::MAX_PAYLOAD`]
//! rather than restated, and enforced **before** anything is admitted, so an impossible
//! payload never becomes an allocation.
//!
//! # Encode and decode apply the same rules
//!
//! An encoder that can emit bytes its own decoder rejects is a defect, so both directions
//! validate key kind, id range, the session-number domain and length prefixes. Decoding is
//! metered: it charges the pass it actually walks through the caller's own ledger.

use sage_db_store::typed::MAX_PAYLOAD;
use sage_db_types::framing::Refusal;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{RecordKey, RecordKind};

/// Operation kind of `CreateSessionCheckpoint`.
pub const CREATE_SESSION_CHECKPOINT: u16 = 3;
/// Operation kind of `ReviseSessionCheckpoint`.
pub const REVISE_SESSION_CHECKPOINT: u16 = 4;
/// The only payload version this slice registers, for both operations.
pub const OPERATION_VERSION: u16 = 1;

/// Bytes of one encoded `RecordKey`: `kind:u16 | id:u128`.
const KEY_BYTES: usize = 18;
/// Bytes of a `str` length prefix.
const LEN_BYTES: usize = 4;

/// One session checkpoint as the caller states it.
///
/// `content` is the serialised session state, copied verbatim: no trimming, normalising
/// or re-encoding happens anywhere between here and the blob it becomes. An empty state is
/// a legal, genuinely empty blob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SessionState<'a> {
    /// The term this session worked on.
    pub concept: RecordKey,
    /// Session ordinal within the concept, from 1.
    pub session_no: u32,
    /// Serialised session state; may be empty.
    pub content: &'a str,
}

/// Why a session-checkpoint payload is not acceptable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionPayloadError {
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
    /// `session_no` was zero; session ordinals start at 1.
    ZeroSessionNo,
    /// A length-prefixed value is not valid UTF-8.
    NotUtf8,
    /// A generated length does not fit its `u32` prefix.
    LengthOverflow,
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

/// A decoded `CreateSessionCheckpoint` payload, borrowing the caller's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedCreate<'a> {
    /// The concept term.
    pub concept: RecordKey,
    /// Session ordinal, from 1.
    pub session_no: u32,
    /// Serialised session state, possibly empty.
    pub content: &'a str,
}

/// A decoded `ReviseSessionCheckpoint` payload, borrowing the caller's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedRevise<'a> {
    /// The checkpoint being revised.
    pub checkpoint: RecordKey,
    /// The concept term, which a revision cannot change.
    pub concept: RecordKey,
    /// Session ordinal, which a revision cannot change.
    pub session_no: u32,
    /// Replacement serialised session state, possibly empty.
    pub content: &'a str,
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

    fn take(&mut self, len: usize) -> Result<&'a [u8], SessionPayloadError> {
        let end = self
            .at
            .checked_add(len)
            .ok_or(SessionPayloadError::Truncated)?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(SessionPayloadError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn u16(&mut self) -> Result<u16, SessionPayloadError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, SessionPayloadError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u128(&mut self) -> Result<u128, SessionPayloadError> {
        let bytes = self.take(16)?;
        let mut raw = [0u8; 16];
        raw.copy_from_slice(bytes);
        Ok(u128::from_le_bytes(raw))
    }

    fn key(&mut self) -> Result<RecordKey, SessionPayloadError> {
        let raw_kind = self.u16()?;
        let kind =
            RecordKind::from_u16(raw_kind).ok_or(SessionPayloadError::UnknownKind(raw_kind))?;
        let key = RecordKey::new(kind, self.u128()?);
        if !key.id_is_in_range() {
            return Err(SessionPayloadError::IdOutOfRange { kind });
        }
        Ok(key)
    }

    fn str(&mut self) -> Result<&'a str, SessionPayloadError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).map_err(|_| SessionPayloadError::NotUtf8)
    }

    fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// Charge and check one decode pass over `len` bytes.
///
/// The charge is the work actually performed, not a guess: a pass walks every byte once.
fn admit_pass(ledger: &mut dyn SharedLedger, len: usize) -> Result<(), SessionPayloadError> {
    ledger
        .checkpoint()
        .map_err(|stop| SessionPayloadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge(len as u64)
        .map_err(|error| SessionPayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .try_charge_entries(1)
        .map_err(|error| SessionPayloadError::Refused(Refusal::Budget(error)))?;
    Ok(())
}

/// The declared ceiling check shared by both directions.
fn check_ceiling(len: u64) -> Result<(), SessionPayloadError> {
    if len > u64::from(MAX_PAYLOAD) {
        return Err(SessionPayloadError::TooLarge {
            len,
            max: MAX_PAYLOAD,
        });
    }
    Ok(())
}

/// Validate a key's domain before any bytes are produced.
fn check_key(key: RecordKey, required: RecordKind) -> Result<(), SessionPayloadError> {
    if !key.id_is_in_range() {
        return Err(SessionPayloadError::IdOutOfRange { kind: key.kind() });
    }
    if key.kind() != required {
        return Err(SessionPayloadError::WrongKind {
            expected: required,
            found: key.kind(),
        });
    }
    Ok(())
}

/// Admit exactly `total` bytes, then return the reserved buffer.
fn admit(ledger: &mut dyn SharedLedger, total: usize) -> Result<Vec<u8>, SessionPayloadError> {
    check_ceiling(total as u64)?;
    ledger
        .checkpoint()
        .map_err(|stop| SessionPayloadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge(total as u64)
        .map_err(|error| SessionPayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| SessionPayloadError::Refused(Refusal::Stopped(stop)))?;
    let mut out = Vec::new();
    out.try_reserve_exact(total)
        .map_err(|_| SessionPayloadError::Refused(Refusal::Allocation { requested: total }))?;
    Ok(out)
}

fn put_key(out: &mut Vec<u8>, key: RecordKey) {
    out.extend_from_slice(&key.kind().as_u16().to_le_bytes());
    out.extend_from_slice(&key.id().to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str, len: u32) {
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

/// Read `session_no` and refuse the zero the record domain excludes.
fn read_session_no(cursor: &mut Cursor<'_>) -> Result<u32, SessionPayloadError> {
    let session_no = cursor.u32()?;
    if session_no == 0 {
        return Err(SessionPayloadError::ZeroSessionNo);
    }
    Ok(session_no)
}

/// Encode a `CreateSessionCheckpoint` v1 payload.
///
/// # Errors
///
/// [`SessionPayloadError::WrongKind`] / [`SessionPayloadError::IdOutOfRange`] for a
/// non-term concept, [`SessionPayloadError::ZeroSessionNo`] for ordinal 0,
/// [`SessionPayloadError::LengthOverflow`] for a value that cannot carry its `u32`
/// prefix, [`SessionPayloadError::TooLarge`] when the assembled payload exceeds the
/// operation ceiling, and [`SessionPayloadError::Refused`] for the caller's own budget,
/// stop or allocation refusal.
pub fn encode_create(
    state: &SessionState<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, SessionPayloadError> {
    check_key(state.concept, RecordKind::Term)?;
    if state.session_no == 0 {
        return Err(SessionPayloadError::ZeroSessionNo);
    }
    let content_len =
        u32::try_from(state.content.len()).map_err(|_| SessionPayloadError::LengthOverflow)?;
    let total = KEY_BYTES
        .checked_add(4)
        .and_then(|n| n.checked_add(LEN_BYTES))
        .and_then(|n| n.checked_add(state.content.len()))
        .ok_or(SessionPayloadError::LengthOverflow)?;
    let mut out = admit(ledger, total)?;
    put_key(&mut out, state.concept);
    out.extend_from_slice(&state.session_no.to_le_bytes());
    put_str(&mut out, state.content, content_len);
    if out.len() != total {
        return Err(SessionPayloadError::InternalInvariant);
    }
    Ok(out)
}

/// Encode a `ReviseSessionCheckpoint` v1 payload.
///
/// # Errors
///
/// As [`encode_create`], plus [`SessionPayloadError::WrongKind`] for a checkpoint key
/// that is not a `SessionCheckpoint`.
pub fn encode_revise(
    checkpoint: RecordKey,
    state: &SessionState<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, SessionPayloadError> {
    check_key(checkpoint, RecordKind::SessionCheckpoint)?;
    check_key(state.concept, RecordKind::Term)?;
    if state.session_no == 0 {
        return Err(SessionPayloadError::ZeroSessionNo);
    }
    let content_len =
        u32::try_from(state.content.len()).map_err(|_| SessionPayloadError::LengthOverflow)?;
    let total = (2 * KEY_BYTES)
        .checked_add(4)
        .and_then(|n| n.checked_add(LEN_BYTES))
        .and_then(|n| n.checked_add(state.content.len()))
        .ok_or(SessionPayloadError::LengthOverflow)?;
    let mut out = admit(ledger, total)?;
    put_key(&mut out, checkpoint);
    put_key(&mut out, state.concept);
    out.extend_from_slice(&state.session_no.to_le_bytes());
    put_str(&mut out, state.content, content_len);
    if out.len() != total {
        return Err(SessionPayloadError::InternalInvariant);
    }
    Ok(out)
}

/// Decode a `CreateSessionCheckpoint` v1 payload.
///
/// # Errors
///
/// [`SessionPayloadError::TooLarge`] before any traversal, [`SessionPayloadError::Truncated`]
/// / [`SessionPayloadError::Trailing`] for a length disagreement, an unknown or
/// out-of-range kind, a non-term concept, ordinal 0, invalid UTF-8, or
/// [`SessionPayloadError::Refused`] for the caller's own budget, stop or allocation
/// refusal.
pub fn decode_create<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedCreate<'a>, SessionPayloadError> {
    check_ceiling(payload.len() as u64)?;
    admit_pass(ledger, payload.len())?;
    let mut cursor = Cursor::new(payload);
    let concept = cursor.key()?;
    if concept.kind() != RecordKind::Term {
        return Err(SessionPayloadError::WrongKind {
            expected: RecordKind::Term,
            found: concept.kind(),
        });
    }
    let session_no = read_session_no(&mut cursor)?;
    let content = cursor.str()?;
    if !cursor.at_end() {
        return Err(SessionPayloadError::Trailing);
    }
    Ok(DecodedCreate {
        concept,
        session_no,
        content,
    })
}

/// Decode a `ReviseSessionCheckpoint` v1 payload.
///
/// # Errors
///
/// As [`decode_create`], with the additional checkpoint-key and non-term concept
/// refusals.
pub fn decode_revise<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedRevise<'a>, SessionPayloadError> {
    check_ceiling(payload.len() as u64)?;
    admit_pass(ledger, payload.len())?;
    let mut cursor = Cursor::new(payload);
    let checkpoint = cursor.key()?;
    if checkpoint.kind() != RecordKind::SessionCheckpoint {
        return Err(SessionPayloadError::WrongKind {
            expected: RecordKind::SessionCheckpoint,
            found: checkpoint.kind(),
        });
    }
    let concept = cursor.key()?;
    if concept.kind() != RecordKind::Term {
        return Err(SessionPayloadError::WrongKind {
            expected: RecordKind::Term,
            found: concept.kind(),
        });
    }
    let session_no = read_session_no(&mut cursor)?;
    let content = cursor.str()?;
    if !cursor.at_end() {
        return Err(SessionPayloadError::Trailing);
    }
    Ok(DecodedRevise {
        checkpoint,
        concept,
        session_no,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_types::ledger::BudgetError;
    use sage_db_types::query::ControlStop;

    struct Open;

    impl SharedLedger for Open {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    fn state(content: &str) -> SessionState<'_> {
        SessionState {
            concept: RecordKey::term(7),
            session_no: 3,
            content,
        }
    }

    #[test]
    fn create_round_trips_and_accepts_empty_content() {
        for content in ["", "state bytes"] {
            let mut ledger = Open;
            let bytes = encode_create(&state(content), &mut ledger).expect("encodes");
            let decoded = decode_create(&bytes, &mut ledger).expect("decodes");
            assert_eq!(decoded.concept, RecordKey::term(7));
            assert_eq!(decoded.session_no, 3);
            assert_eq!(decoded.content, content);
        }
    }

    #[test]
    fn revise_round_trips_both_keys() {
        let mut ledger = Open;
        let checkpoint = RecordKey::new(RecordKind::SessionCheckpoint, 11);
        let bytes = encode_revise(checkpoint, &state("next"), &mut ledger).expect("encodes");
        let decoded = decode_revise(&bytes, &mut ledger).expect("decodes");
        assert_eq!(decoded.checkpoint, checkpoint);
        assert_eq!(decoded.concept, RecordKey::term(7));
        assert_eq!(decoded.session_no, 3);
        assert_eq!(decoded.content, "next");
    }

    #[test]
    fn zero_session_number_is_refused_in_both_directions() {
        let mut ledger = Open;
        let zero = SessionState {
            concept: RecordKey::term(7),
            session_no: 0,
            content: "x",
        };
        assert_eq!(
            encode_create(&zero, &mut ledger),
            Err(SessionPayloadError::ZeroSessionNo)
        );

        let mut payload = Vec::new();
        put_key(&mut payload, RecordKey::term(7));
        payload.extend_from_slice(&0u32.to_le_bytes());
        put_str(&mut payload, "x", 1);
        assert_eq!(
            decode_create(&payload, &mut ledger),
            Err(SessionPayloadError::ZeroSessionNo)
        );
    }

    #[test]
    fn wrong_key_kinds_are_refused_on_the_encode_side() {
        let mut ledger = Open;
        let wrong = SessionState {
            concept: RecordKey::new(RecordKind::Claim, 1),
            session_no: 1,
            content: "",
        };
        assert_eq!(
            encode_create(&wrong, &mut ledger),
            Err(SessionPayloadError::WrongKind {
                expected: RecordKind::Term,
                found: RecordKind::Claim,
            })
        );
        assert_eq!(
            encode_revise(RecordKey::term(1), &state("x"), &mut ledger),
            Err(SessionPayloadError::WrongKind {
                expected: RecordKind::SessionCheckpoint,
                found: RecordKind::Term,
            })
        );
    }

    #[test]
    fn trailing_and_truncated_bytes_are_refused() {
        let mut ledger = Open;
        let bytes = encode_create(&state("x"), &mut ledger).expect("encodes");
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(
            decode_create(&trailing, &mut ledger),
            Err(SessionPayloadError::Trailing)
        );
        // Dropping the last content byte makes the declared `u32` length exceed the bytes.
        let short = &bytes[..bytes.len() - 1];
        assert_eq!(
            decode_create(short, &mut ledger),
            Err(SessionPayloadError::Truncated)
        );
    }

    #[test]
    fn a_payload_over_the_operation_ceiling_never_reaches_allocation() {
        let mut ledger = Open;
        let huge = "x".repeat(MAX_PAYLOAD as usize);
        let error = encode_create(&state(&huge), &mut ledger).expect_err("ceiling");
        assert!(matches!(error, SessionPayloadError::TooLarge { .. }));
    }
}

#[cfg(test)]
mod root_payload_boundaries {
    use super::*;
    use sage_db_store::lease::AttemptLedger;
    use sage_db_types::query::RunToCompletion;
    #[test]
    fn accepted_maximum_and_first_rejected_value_for_both_session_payloads() {
        let control = RunToCompletion;
        let mut l = AttemptLedger::open(4 * 1024 * 1024, 1000, &control).unwrap();
        let concept = RecordKey::term(1);
        let checkpoint = RecordKey::new(RecordKind::SessionCheckpoint, 1);
        // Actual literal wire overhead: 18-byte key + u32 ordinal + u32 length.
        let max_create = MAX_PAYLOAD as usize - 26;
        let text = "x".repeat(max_create);
        let state = SessionState {
            concept,
            session_no: 1,
            content: &text,
        };
        let encoded = encode_create(&state, &mut l).unwrap();
        assert_eq!(encoded.len(), MAX_PAYLOAD as usize);
        assert_eq!(decode_create(&encoded, &mut l).unwrap().content, text);
        let too_long = "x".repeat(max_create + 1);
        let before = l.stats().admitted_bytes;
        assert!(
            matches!(encode_create(&SessionState{content:&too_long,..state},&mut l),Err(SessionPayloadError::TooLarge{len,max}) if len==u64::from(MAX_PAYLOAD)+1 && max==MAX_PAYLOAD)
        );
        assert_eq!(
            l.stats().admitted_bytes,
            before,
            "ceiling check precedes new admission"
        );
        // Revise adds exactly one more 18-byte RecordKey, not an arbitrary margin.
        let max_revise = MAX_PAYLOAD as usize - 44;
        let text = "y".repeat(max_revise);
        let state = SessionState {
            concept,
            session_no: 1,
            content: &text,
        };
        let encoded = encode_revise(checkpoint, &state, &mut l).unwrap();
        assert_eq!(encoded.len(), MAX_PAYLOAD as usize);
        assert_eq!(decode_revise(&encoded, &mut l).unwrap().content, text);
        let too_long = "y".repeat(max_revise + 1);
        let before = l.stats().admitted_bytes;
        assert!(
            matches!(encode_revise(checkpoint,&SessionState{content:&too_long,..state},&mut l),Err(SessionPayloadError::TooLarge{len,max}) if len==u64::from(MAX_PAYLOAD)+1 && max==MAX_PAYLOAD)
        );
        assert_eq!(l.stats().admitted_bytes, before);
    }
}
