//! The P2 claim-edit operation registry (v1) and its two payload codecs.
//!
//! P1 owns the request *frame*: canonical versioning, the read set, ceilings and the
//! `request`-domain digest. P2 alone assigns meaning to `op_kind`, `op_version` and the
//! payload bytes (`docs/design/storage-digests.md` §4.7), so this module is the one
//! place those three numbers and their layouts are defined.
//!
//! # Layouts (little-endian, no padding)
//!
//! ```text
//! SubmitClaimEdit v1 payload
//!   subject   RecordKey  kind:u16 | id:u128        (kind must be Term)
//!   claim     RecordKey  kind:u16 | id:u128        (kind must be Claim)
//!   text      str        len:u32 | UTF-8 bytes     (non-empty)
//!   author    str        len:u32 | UTF-8 bytes     (copied verbatim, may be empty)
//!
//! ReviewClaimEdit v1 payload
//!   proposal  RecordKey  kind:u16 | id:u128        (kind must be Proposal)
//!   decision  u16        0 = approve, 1 = reject
//!   note      str        len:u32 | UTF-8 bytes     (non-empty)
//! ```
//!
//! Both payloads are carried as the single operation of the P1 frame, so the frame's
//! `1..=65536` operation ceiling is the ceiling here too — imported from
//! [`sage_db_store::typed::MAX_PAYLOAD`] rather than restated, and enforced **before**
//! anything is admitted, so an impossible payload never becomes an allocation.
//!
//! # Why the bytes are composed here rather than in the format crate
//!
//! `sage-db-format` encodes persisted records; a request payload is caller input with no
//! record identity, revision or audit tail. `kernel` depends on `types` and `store` only,
//! so a format dependency would invert the declared edge. The layout is therefore this
//! module's own contract, and the key domains it carries are the ones the canonical types
//! already define.
//!
//! # Encode and decode apply the same rules
//!
//! An encoder that can emit bytes its own decoder rejects is a defect, so both directions
//! validate key kind, id range, emptiness and length. Decoding is metered: it takes the
//! caller's ledger, checkpoints before each pass and charges the bytes it actually walks,
//! because a payload may legitimately be up to 64 KiB and an unmetered UTF-8 scan would be
//! work the caller never allowed.
//!
//! # The declared author is copied, not policed
//!
//! `declared_author` is attribution, not identity: the contract bounds it by byte length
//! and a NUL scan (both applied by the writer as caller-input checks), and it is then stored
//! exactly as written. An empty declaration is preserved as empty rather than invented into
//! a requirement this slice never had.

use sage_db_store::typed::MAX_PAYLOAD;
use sage_db_types::framing::Refusal;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{RecordKey, RecordKind};

/// Operation kind of `SubmitClaimEdit`.
pub const SUBMIT_CLAIM_EDIT: u16 = 1;
/// Operation kind of `ReviewClaimEdit`.
pub const REVIEW_CLAIM_EDIT: u16 = 2;
/// The only payload version this slice registers, for both operations.
pub const OPERATION_VERSION: u16 = 1;
/// Decision wire value: approve.
pub const DECISION_APPROVE: u16 = 0;
/// Decision wire value: reject.
pub const DECISION_REJECT: u16 = 1;

/// Bytes of one encoded `RecordKey`: `kind:u16 | id:u128`.
const KEY_BYTES: usize = 18;
/// Bytes of a `str` length prefix.
const LEN_BYTES: usize = 4;

/// One claim-text edit as the submitter states it.
///
/// The text is the authored prose, copied verbatim: no trimming, normalising or
/// re-encoding happens anywhere between here and the blob it becomes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClaimEdit<'a> {
    /// The term the claim is about; must be the claim's current `concept`.
    pub subject: RecordKey,
    /// The existing claim whose text this edit replaces.
    pub claim: RecordKey,
    /// Replacement text; non-empty.
    pub text: &'a str,
    /// Author as declared by the submitted document; never the executor.
    pub declared_author: &'a str,
}

/// A reviewer's disposition of a pending proposal.
///
/// Deliberately only two values: `Conflicted` is a *writer-derived* record state, not
/// something a reviewer chooses, so it cannot appear as a request decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReviewDecision {
    /// Apply the candidate and mark the proposal approved.
    Approve,
    /// Refuse the candidate without touching its subject.
    Reject,
}

impl ReviewDecision {
    /// The wire value of this decision.
    #[must_use]
    pub const fn wire(self) -> u16 {
        match self {
            Self::Approve => DECISION_APPROVE,
            Self::Reject => DECISION_REJECT,
        }
    }

    /// Decode a wire value; `None` outside the registered domain.
    #[must_use]
    pub const fn from_wire(value: u16) -> Option<Self> {
        match value {
            DECISION_APPROVE => Some(Self::Approve),
            DECISION_REJECT => Some(Self::Reject),
            _ => None,
        }
    }
}

/// Why a claim-edit payload is not acceptable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PayloadError {
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
    /// A key carried a kind this operation does not accept in that position.
    WrongKind {
        /// The kind the position requires.
        expected: RecordKind,
        /// The kind that appeared.
        found: RecordKind,
    },
    /// The submitted text is empty.
    EmptyText,
    /// The review note is empty.
    EmptyNote,
    /// A `u16` decision value outside `0..=1`.
    UnknownDecision(u16),
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
    /// already admitted. It exists so an impossible arm reports itself as an internal
    /// defect rather than as a resource refusal.
    InternalInvariant,
    /// The caller's ledger refused or stopped the work.
    Refused(Refusal),
}

/// A decoded `SubmitClaimEdit` payload, borrowing the caller's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedSubmit<'a> {
    /// The claimed subject term.
    pub subject: RecordKey,
    /// The claim to edit.
    pub claim: RecordKey,
    /// Replacement text.
    pub text: &'a str,
    /// Declared author, verbatim (possibly empty).
    pub declared_author: &'a str,
}

/// A decoded `ReviewClaimEdit` payload, borrowing the caller's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedReview<'a> {
    /// The proposal being decided.
    pub proposal: RecordKey,
    /// The reviewer's decision.
    pub decision: ReviewDecision,
    /// The review note.
    pub note: &'a str,
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

    fn take(&mut self, len: usize) -> Result<&'a [u8], PayloadError> {
        let end = self.at.checked_add(len).ok_or(PayloadError::Truncated)?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(PayloadError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn u16(&mut self) -> Result<u16, PayloadError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, PayloadError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u128(&mut self) -> Result<u128, PayloadError> {
        let bytes = self.take(16)?;
        let mut raw = [0u8; 16];
        raw.copy_from_slice(bytes);
        Ok(u128::from_le_bytes(raw))
    }

    fn key(&mut self) -> Result<RecordKey, PayloadError> {
        let raw_kind = self.u16()?;
        let kind = RecordKind::from_u16(raw_kind).ok_or(PayloadError::UnknownKind(raw_kind))?;
        let key = RecordKey::new(kind, self.u128()?);
        if !key.id_is_in_range() {
            return Err(PayloadError::IdOutOfRange { kind });
        }
        Ok(key)
    }

    fn str(&mut self) -> Result<&'a str, PayloadError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).map_err(|_| PayloadError::NotUtf8)
    }

    fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// Charge and check one decode pass over `len` bytes.
///
/// The charge is the work actually performed, not a guess: a pass walks every byte once.
fn admit_pass(ledger: &mut dyn SharedLedger, len: usize) -> Result<(), PayloadError> {
    ledger
        .checkpoint()
        .map_err(|stop| PayloadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge(len as u64)
        .map_err(|error| PayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .try_charge_entries(1)
        .map_err(|error| PayloadError::Refused(Refusal::Budget(error)))?;
    Ok(())
}

/// The declared ceiling check shared by both directions.
fn check_ceiling(len: u64) -> Result<(), PayloadError> {
    if len > u64::from(MAX_PAYLOAD) {
        return Err(PayloadError::TooLarge {
            len,
            max: MAX_PAYLOAD,
        });
    }
    Ok(())
}

/// Validate a key's domain before any bytes are produced.
fn check_key(key: RecordKey, required: RecordKind) -> Result<(), PayloadError> {
    if !key.id_is_in_range() {
        return Err(PayloadError::IdOutOfRange { kind: key.kind() });
    }
    if key.kind() != required {
        return Err(PayloadError::WrongKind {
            expected: required,
            found: key.kind(),
        });
    }
    Ok(())
}

/// Admit exactly `total` bytes, then return the reserved buffer.
///
/// The ceiling and the arithmetic both run first, so a payload that could never be legal
/// is refused before the allocator is asked for it. `total` is then the capacity the
/// buffer has, so the writes below cannot grow it.
fn admit(ledger: &mut dyn SharedLedger, total: usize) -> Result<Vec<u8>, PayloadError> {
    check_ceiling(total as u64)?;
    ledger
        .checkpoint()
        .map_err(|stop| PayloadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge(total as u64)
        .map_err(|error| PayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| PayloadError::Refused(Refusal::Stopped(stop)))?;
    let mut out = Vec::new();
    out.try_reserve_exact(total)
        .map_err(|_| PayloadError::Refused(Refusal::Allocation { requested: total }))?;
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

/// Encode a `SubmitClaimEdit` v1 payload.
///
/// # Errors
///
/// [`PayloadError::WrongKind`] / [`PayloadError::IdOutOfRange`] for a key outside its
/// position's domain, [`PayloadError::EmptyText`] for empty prose,
/// [`PayloadError::LengthOverflow`] for a value that cannot carry its `u32` prefix,
/// [`PayloadError::TooLarge`] when the assembled payload exceeds the operation ceiling,
/// and [`PayloadError::Refused`] for the caller's own budget, stop or allocation refusal.
pub fn encode_submit_edit(
    edit: &ClaimEdit<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, PayloadError> {
    check_key(edit.subject, RecordKind::Term)?;
    check_key(edit.claim, RecordKind::Claim)?;
    if edit.text.is_empty() {
        return Err(PayloadError::EmptyText);
    }
    let text_len = u32::try_from(edit.text.len()).map_err(|_| PayloadError::LengthOverflow)?;
    let author_len =
        u32::try_from(edit.declared_author.len()).map_err(|_| PayloadError::LengthOverflow)?;
    let total = (2 * KEY_BYTES)
        .checked_add(LEN_BYTES)
        .and_then(|n| n.checked_add(edit.text.len()))
        .and_then(|n| n.checked_add(LEN_BYTES))
        .and_then(|n| n.checked_add(edit.declared_author.len()))
        .ok_or(PayloadError::LengthOverflow)?;
    let mut out = admit(ledger, total)?;
    put_key(&mut out, edit.subject);
    put_key(&mut out, edit.claim);
    put_str(&mut out, edit.text, text_len);
    put_str(&mut out, edit.declared_author, author_len);
    if out.len() != total {
        return Err(PayloadError::InternalInvariant);
    }
    Ok(out)
}

/// Encode a `ReviewClaimEdit` v1 payload.
///
/// # Errors
///
/// [`PayloadError::WrongKind`] / [`PayloadError::IdOutOfRange`] for a non-proposal key,
/// [`PayloadError::EmptyNote`] for an empty note, [`PayloadError::LengthOverflow`] for a
/// value that cannot carry its `u32` prefix, [`PayloadError::TooLarge`] when the assembled
/// payload exceeds the operation ceiling, and [`PayloadError::Refused`] for the caller's
/// own budget, stop or allocation refusal.
pub fn encode_review_edit(
    proposal: RecordKey,
    decision: ReviewDecision,
    note: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, PayloadError> {
    check_key(proposal, RecordKind::Proposal)?;
    if note.is_empty() {
        return Err(PayloadError::EmptyNote);
    }
    let note_len = u32::try_from(note.len()).map_err(|_| PayloadError::LengthOverflow)?;
    let total = KEY_BYTES
        .checked_add(2)
        .and_then(|n| n.checked_add(LEN_BYTES))
        .and_then(|n| n.checked_add(note.len()))
        .ok_or(PayloadError::LengthOverflow)?;
    let mut out = admit(ledger, total)?;
    put_key(&mut out, proposal);
    out.extend_from_slice(&decision.wire().to_le_bytes());
    put_str(&mut out, note, note_len);
    if out.len() != total {
        return Err(PayloadError::InternalInvariant);
    }
    Ok(out)
}

/// Decode a `SubmitClaimEdit` v1 payload.
///
/// The same function decodes the payload the writer persists and the payload a reviewer
/// reads back, because both must be judged by one rule set; the caller decides whether a
/// failure is an input verdict or a stored-payload verdict.
///
/// # Errors
///
/// [`PayloadError::TooLarge`] before any traversal, [`PayloadError::Truncated`] /
/// [`PayloadError::Trailing`] for a length disagreement, an unknown or out-of-range kind,
/// a wrong position kind, empty text, invalid UTF-8, or [`PayloadError::Refused`] for the
/// caller's own budget, stop or allocation refusal.
pub fn decode_submit<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedSubmit<'a>, PayloadError> {
    check_ceiling(payload.len() as u64)?;
    admit_pass(ledger, payload.len())?;
    let mut cursor = Cursor::new(payload);
    let subject = cursor.key()?;
    if subject.kind() != RecordKind::Term {
        return Err(PayloadError::WrongKind {
            expected: RecordKind::Term,
            found: subject.kind(),
        });
    }
    let claim = cursor.key()?;
    if claim.kind() != RecordKind::Claim {
        return Err(PayloadError::WrongKind {
            expected: RecordKind::Claim,
            found: claim.kind(),
        });
    }
    let text = cursor.str()?;
    if text.is_empty() {
        return Err(PayloadError::EmptyText);
    }
    let declared_author = cursor.str()?;
    if !cursor.at_end() {
        return Err(PayloadError::Trailing);
    }
    Ok(DecodedSubmit {
        subject,
        claim,
        text,
        declared_author,
    })
}

/// Decode a `ReviewClaimEdit` v1 payload.
///
/// # Errors
///
/// [`PayloadError::TooLarge`] before any traversal, [`PayloadError::Truncated`] /
/// [`PayloadError::Trailing`] for a length disagreement, an unknown or out-of-range kind,
/// a non-proposal key, an unregistered decision, an empty note, invalid UTF-8, or
/// [`PayloadError::Refused`] for the caller's own budget, stop or allocation refusal.
pub fn decode_review<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedReview<'a>, PayloadError> {
    check_ceiling(payload.len() as u64)?;
    admit_pass(ledger, payload.len())?;
    let mut cursor = Cursor::new(payload);
    let proposal = cursor.key()?;
    if proposal.kind() != RecordKind::Proposal {
        return Err(PayloadError::WrongKind {
            expected: RecordKind::Proposal,
            found: proposal.kind(),
        });
    }
    let raw = cursor.u16()?;
    let decision = ReviewDecision::from_wire(raw).ok_or(PayloadError::UnknownDecision(raw))?;
    let note = cursor.str()?;
    if note.is_empty() {
        return Err(PayloadError::EmptyNote);
    }
    if !cursor.at_end() {
        return Err(PayloadError::Trailing);
    }
    Ok(DecodedReview {
        proposal,
        decision,
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sage_db_store::lease::AttemptLedger;
    use sage_db_types::ledger::BudgetUnit;
    use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};

    fn ledger() -> AttemptLedger<'static> {
        AttemptLedger::open(1 << 20, 1 << 16, &RunToCompletion).unwrap()
    }

    fn edit<'a>(text: &'a str, author: &'a str) -> ClaimEdit<'a> {
        ClaimEdit {
            subject: RecordKey::term(7),
            claim: RecordKey::new(RecordKind::Claim, 9),
            text,
            declared_author: author,
        }
    }

    /// The literal submit layout, composed independently from normative widths.
    fn literal_submit(text: &[u8], author: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&1u16.to_le_bytes()); // kind = Term
        out.extend_from_slice(&7u128.to_le_bytes()); // id
        out.extend_from_slice(&3u16.to_le_bytes()); // kind = Claim
        out.extend_from_slice(&9u128.to_le_bytes()); // id
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text);
        out.extend_from_slice(&(author.len() as u32).to_le_bytes());
        out.extend_from_slice(author);
        out
    }

    /// The literal review layout, composed independently from normative widths.
    fn literal_review(key_kind: u16, id: u128, decision: u16, note: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&key_kind.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&decision.to_le_bytes());
        out.extend_from_slice(&(note.len() as u32).to_le_bytes());
        out.extend_from_slice(note);
        out
    }

    #[test]
    fn submit_payload_is_byte_exactly_the_literal_layout() {
        let mut l = ledger();
        let bytes = encode_submit_edit(&edit("  spaced\ntext ", "Ada"), &mut l).unwrap();
        assert_eq!(bytes, literal_submit(b"  spaced\ntext ", b"Ada"));
        let decoded = decode_submit(&bytes, &mut l).unwrap();
        assert_eq!(decoded.text, "  spaced\ntext ");
        assert_eq!(decoded.declared_author, "Ada");
        assert_eq!(decoded.subject, RecordKey::term(7));
        assert_eq!(decoded.claim, RecordKey::new(RecordKind::Claim, 9));
    }

    #[test]
    fn review_payload_is_byte_exactly_the_literal_layout_for_both_decisions() {
        let key = RecordKey::new(RecordKind::Proposal, 4);
        for (decision, wire) in [
            (ReviewDecision::Approve, DECISION_APPROVE),
            (ReviewDecision::Reject, DECISION_REJECT),
        ] {
            let mut l = ledger();
            let bytes = encode_review_edit(key, decision, "noted", &mut l).unwrap();
            assert_eq!(bytes, literal_review(7, 4, wire, b"noted"));
            let decoded = decode_review(&bytes, &mut l).unwrap();
            assert_eq!(decoded.proposal, key);
            assert_eq!(decoded.decision, decision);
            assert_eq!(decoded.note, "noted");
        }
    }

    #[test]
    fn empty_declared_author_is_preserved_verbatim() {
        let mut l = ledger();
        let bytes = encode_submit_edit(&edit("t", ""), &mut l).unwrap();
        assert_eq!(bytes, literal_submit(b"t", b""));
        assert_eq!(decode_submit(&bytes, &mut l).unwrap().declared_author, "");
    }

    #[test]
    fn empty_text_and_empty_note_are_refused_in_both_directions() {
        let mut l = ledger();
        assert_eq!(
            encode_submit_edit(&edit("", "a"), &mut l),
            Err(PayloadError::EmptyText)
        );
        assert_eq!(
            encode_review_edit(
                RecordKey::new(RecordKind::Proposal, 1),
                ReviewDecision::Approve,
                "",
                &mut l
            ),
            Err(PayloadError::EmptyNote)
        );
        // A hand-built payload claiming zero-length text/note is refused on decode too.
        let mut zero_text = literal_submit(b"", b"a");
        assert_eq!(
            decode_submit(&zero_text, &mut l),
            Err(PayloadError::EmptyText)
        );
        zero_text.clear();
        zero_text.extend_from_slice(&literal_review(7, 1, DECISION_APPROVE, b""));
        assert_eq!(
            decode_review(&zero_text, &mut l),
            Err(PayloadError::EmptyNote)
        );
    }

    #[test]
    fn wrong_kinds_and_ids_are_refused_by_the_encoder_as_well_as_the_decoder() {
        let mut l = ledger();
        let swapped = ClaimEdit {
            subject: RecordKey::new(RecordKind::Claim, 1),
            claim: RecordKey::term(2),
            text: "t",
            declared_author: "a",
        };
        assert_eq!(
            encode_submit_edit(&swapped, &mut l),
            Err(PayloadError::WrongKind {
                expected: RecordKind::Term,
                found: RecordKind::Claim
            })
        );
        assert_eq!(
            encode_review_edit(RecordKey::term(1), ReviewDecision::Approve, "n", &mut l),
            Err(PayloadError::WrongKind {
                expected: RecordKind::Proposal,
                found: RecordKind::Term
            })
        );
        // A term id above `u32::MAX` cannot be decoded, so it must not be encoded either.
        let bad = ClaimEdit {
            subject: RecordKey::new(RecordKind::Term, u128::from(u32::MAX) + 1),
            claim: RecordKey::new(RecordKind::Claim, 2),
            text: "t",
            declared_author: "a",
        };
        assert_eq!(
            encode_submit_edit(&bad, &mut l),
            Err(PayloadError::IdOutOfRange {
                kind: RecordKind::Term
            })
        );
        let mut over = literal_submit(b"t", b"a");
        over[0..2].copy_from_slice(&3u16.to_le_bytes()); // claim in the subject slot
        assert_eq!(
            decode_submit(&over, &mut l),
            Err(PayloadError::WrongKind {
                expected: RecordKind::Term,
                found: RecordKind::Claim
            })
        );
    }

    #[test]
    fn unknown_decision_trailing_and_truncated_are_distinguished() {
        let mut l = ledger();
        let unknown = literal_review(7, 1, 9, b"n");
        assert_eq!(
            decode_review(&unknown, &mut l),
            Err(PayloadError::UnknownDecision(9))
        );
        let mut trailing = encode_submit_edit(&edit("t", "a"), &mut l).unwrap();
        trailing.push(0);
        assert_eq!(
            decode_submit(&trailing, &mut l),
            Err(PayloadError::Trailing)
        );
        assert_eq!(decode_submit(&[], &mut l), Err(PayloadError::Truncated));
        let mut short = literal_review(7, 1, DECISION_REJECT, b"n");
        short.pop();
        assert_eq!(decode_review(&short, &mut l), Err(PayloadError::Truncated));
    }

    #[test]
    fn over_ceiling_payload_is_refused_before_any_allocation() {
        let mut l = ledger();
        let big = "x".repeat(usize::try_from(MAX_PAYLOAD).unwrap());
        assert_eq!(
            encode_submit_edit(&edit(&big, "a"), &mut l),
            Err(PayloadError::TooLarge {
                // 18+18 key bytes, two 4-byte prefixes, the text and one author byte.
                len: 36 + 4 + u64::from(MAX_PAYLOAD) + 4 + 1,
                max: MAX_PAYLOAD
            })
        );
        assert_eq!(l.stats().live_bytes, 0);
        let bytes = vec![0u8; usize::try_from(MAX_PAYLOAD).unwrap() + 1];
        assert_eq!(
            decode_submit(&bytes, &mut l),
            Err(PayloadError::TooLarge {
                len: u64::from(MAX_PAYLOAD) + 1,
                max: MAX_PAYLOAD
            })
        );
        assert_eq!(l.stats().live_bytes, 0);
    }

    #[test]
    fn byte_budget_refuses_before_the_buffer_is_reserved() {
        let mut empty = AttemptLedger::open(0, 256, &RunToCompletion).unwrap();
        assert!(matches!(
            encode_submit_edit(&edit("text", "author"), &mut empty),
            Err(PayloadError::Refused(Refusal::Budget(error)))
                if error.unit() == BudgetUnit::Bytes && error.remaining() == 0
        ));
        assert_eq!(empty.stats().live_bytes, 0);
    }

    #[test]
    fn decoding_charges_the_bytes_it_walks() {
        let mut l = ledger();
        let bytes = encode_submit_edit(&edit("abc", "d"), &mut l).unwrap();
        let before = l.stats().admitted_bytes.unwrap();
        decode_submit(&bytes, &mut l).unwrap();
        let charged = l.stats().admitted_bytes.unwrap() - before;
        assert!(
            charged >= bytes.len() as u128,
            "a decode pass charges at least the bytes it traverses: {charged} for {}",
            bytes.len()
        );
        // An empty ledger cannot pay for the pass even though the payload is small.
        let mut zero = AttemptLedger::open(0, 256, &RunToCompletion).unwrap();
        assert!(matches!(
            decode_submit(&bytes, &mut zero),
            Err(PayloadError::Refused(Refusal::Budget(_)))
        ));
        assert_eq!(zero.stats().live_bytes, 0);
    }

    #[test]
    fn a_stopped_caller_is_not_a_payload_verdict() {
        struct Stop;
        impl QueryControl for Stop {
            fn checkpoint(&self) -> Result<(), ControlStop> {
                Err(ControlStop::Cancelled)
            }
        }
        let mut stopped = AttemptLedger::open(1024, 256, &Stop).unwrap();
        assert_eq!(
            encode_submit_edit(&edit("text", "author"), &mut stopped),
            Err(PayloadError::Refused(Refusal::Stopped(
                ControlStop::Cancelled
            )))
        );
        assert_eq!(
            decode_review(&[0u8; 4], &mut stopped),
            Err(PayloadError::Refused(Refusal::Stopped(
                ControlStop::Cancelled
            )))
        );
    }
}
