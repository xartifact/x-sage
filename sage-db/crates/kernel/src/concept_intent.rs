//! The P2 concept-text operation registry (v1) and its two payload codecs.
//!
//! P1 owns the request *frame* — canonical versioning, the read set, ceilings and the
//! `request`-domain digest. P2 alone assigns meaning to `op_kind`, `op_version` and the
//! payload bytes (`docs/design/storage-digests.md` §4.7), so this module is the one place
//! those three numbers and their layouts are defined for concept text.
//!
//! # Layouts (little-endian, no padding)
//!
//! ```text
//! SubmitConceptText v1 payload
//!   subject         RecordKey  kind:u16 | id:u128      (kind must be Term)
//!   field_mask      u8                                  (1 = atom, 2 = body, 3 = both)
//!   atom            str  len:u32 | UTF-8 bytes          iff bit0 set
//!   body            str  len:u32 | UTF-8 bytes          iff bit1 set
//!   declared_author str  len:u32 | UTF-8 bytes          (copied verbatim, may be empty)
//!
//! ReviewConceptText v1 payload
//!   proposal        RecordKey  kind:u16 | id:u128      (kind must be Proposal)
//!   decision        u16                                 (0 approve, 1 reject)
//!   note            str  len:u32 | UTF-8 bytes          (non-empty)
//! ```
//!
//! A mask of `0` or any value above `3` names no field this schema edits, so it is refused
//! rather than treated as "nothing changed". A field that is named must carry its `str`
//! even when empty — absence and emptiness are different statements, and the mask is what
//! distinguishes them.
//!
//! # Why atom and body are separate fields with a mask
//!
//! The old `refine-atom` and `refine-body` commands each rewrote exactly one field. One
//! operation covering both keeps a single codec and a single review path while still
//! saying precisely which fields the proposal claims to replace: the mask is the claim, and
//! review re-derives the same mask from the stored bytes rather than trusting the reviewer.
//!
//! # What this does not edit
//!
//! Nothing structural. `slug`, `layer`, `domains`, `confidence`, `source`, `triggers`,
//! `retirement` and every assertion are out of the payload's reach; a proposal built
//! from these bytes can only move the two text fields it names.
//!
//! # Encode and decode apply the same rules
//!
//! An encoder that can emit bytes its own decoder rejects is a defect, so both directions
//! validate kind, id range, mask, emptiness, NUL and length. Decoding is metered: it takes
//! the caller's ledger, checkpoints before each pass and charges the bytes it actually
//! walks, because a payload may legitimately be up to 64 KiB.
//!
//! # Two atom bounds, two owners
//!
//! The atom carries a **canonical** ceiling ([`MAX_ATOM_BYTES`], the format's record-field
//! limit) and a **business** ceiling ([`MAX_ATOM_UTF16_UNITS`], the product's 200 UTF-16 code
//! units as enforced by the shipped TypeScript validators). Both apply, in that order, and each
//! reports its own error. The canonical bound is never relaxed here, and no normalisation,
//! grapheme or scalar reinterpretation is performed: the business count is the same
//! `encode_utf16()` length the JavaScript validators see.
//!
//! # The declared author is copied, not policed
//!
//! `declared_author` is attribution, not identity: the contract bounds it by byte length
//! and a NUL scan, and it is then stored exactly as written. An empty declaration stays
//! empty rather than being invented into a requirement this slice never had.

use sage_db_store::typed::{MAX_ATOM_BYTES, MAX_DECLARED_AUTHOR, MAX_PAYLOAD, MAX_TEXT_BYTES};
use sage_db_types::framing::Refusal;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{RecordKey, RecordKind};

/// Operation kind of `SubmitConceptText`.
pub const SUBMIT_CONCEPT_TEXT: u16 = 7;
/// Operation kind of `ReviewConceptText`.
pub const REVIEW_CONCEPT_TEXT: u16 = 8;
/// The original submit payload and the review payload version.
pub const OPERATION_VERSION: u16 = 1;
/// Submit payload version with an explicit reason after the v1 fields.
pub const SUBMIT_REASON_VERSION: u16 = 2;
/// Decision wire value: approve.
pub const DECISION_APPROVE: u16 = 0;
/// Decision wire value: reject.
pub const DECISION_REJECT: u16 = 1;

/// Mask bit selecting the atom field.
pub const MASK_ATOM: u8 = 0b01;
/// Mask bit selecting the body field.
pub const MASK_BODY: u8 = 0b10;
/// The only legal mask values: one field or both, never neither.
pub const MASK_ALLOWED: &[u8] = &[MASK_ATOM, MASK_BODY, MASK_ATOM | MASK_BODY];

/// The **business** atom bound: UTF-16 code units, matching the shipped product validators
/// (`packages/core/src/types/concept.ts` and `knowledge/records.ts` both enforce
/// `z.string().min(1).max(200)`, and JavaScript string length is UTF-16 code units).
///
/// This is a different contract from [`MAX_ATOM_BYTES`]: that is the canonical record-field
/// ceiling, and it stays exactly as the format defines it. A Rust `str` is UTF-8, so this
/// bound is counted with `encode_utf16()` — allocation-free — and *after* the byte guard, so
/// an over-byte atom is refused for the byte reason first.
///
/// No grapheme-cluster or Unicode-scalar reinterpretation is performed anywhere: the counter
/// is the same one the shipped validators use, so the boundary is identical.
pub const MAX_ATOM_UTF16_UNITS: usize = 200;

/// Bytes of one encoded `RecordKey`: `kind:u16 | id:u128`.
const KEY_BYTES: usize = 18;
/// Bytes of a `str` or count prefix.
const LEN_BYTES: usize = 4;
/// Bytes of the mask.
const MASK_BYTES: usize = 1;

/// One concept-text edit as the submitter states it.
///
/// The two texts are the authored bytes verbatim: no trimming, normalising or re-encoding
/// happens anywhere between here and the blob each becomes. Only the fields named by
/// [`Self::field_mask`] are carried, and the others are `None` rather than empty — that is
/// what makes "I am not changing the body" distinguishable from "the body becomes empty".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConceptTextEdit<'a> {
    /// The existing active term whose text this edit replaces.
    pub subject: RecordKey,
    /// Which fields the edit claims to replace: 1 atom, 2 body, 3 both.
    pub field_mask: u8,
    /// The new atom, present exactly when `field_mask & 1 != 0`.
    pub atom: Option<&'a str>,
    /// The new body, present exactly when `field_mask & 2 != 0`. May be empty.
    pub body: Option<&'a str>,
    /// Declared attribution, copied verbatim; may be empty.
    pub declared_author: &'a str,
}

impl ConceptTextEdit<'_> {
    /// Whether this edit replaces the atom.
    #[must_use]
    pub const fn replaces_atom(&self) -> bool {
        self.field_mask & MASK_ATOM != 0
    }

    /// Whether this edit replaces the body.
    #[must_use]
    pub const fn replaces_body(&self) -> bool {
        self.field_mask & MASK_BODY != 0
    }
}

/// Why a concept-text payload is not acceptable.
///
/// Every variant is a caller-input verdict. A generated image that disagrees with these
/// rules is the writer's own defect and is reported separately by the caller, never
/// dressed up as input damage.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConceptTextPayloadError {
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
    /// The mask names no field, or names a bit this schema does not define.
    UnknownMask(u8),
    /// A field the mask names is absent from the edit.
    MissingField {
        /// The mask bit whose field is absent.
        bit: u8,
    },
    /// A field the mask does not name was supplied anyway.
    UnexpectedField {
        /// The mask bit whose field was supplied.
        bit: u8,
    },
    /// The atom was empty, or contained a NUL. An empty atom would leave a term with no
    /// summary while claiming to have edited one.
    InvalidAtom,
    /// The atom exceeds [`MAX_ATOM_BYTES`] — the canonical record-field ceiling.
    AtomTooLong {
        /// Supplied byte length.
        len: u64,
        /// The canonical bound.
        max: u32,
    },
    /// The atom exceeds [`MAX_ATOM_UTF16_UNITS`] — the **business** ceiling.
    ///
    /// Distinct from [`Self::AtomTooLong`] on purpose: an atom can be within the canonical
    /// byte bound and still be refused here (many small code units, or astral pairs), and the
    /// two have different owners. Reporting one as the other would tell an operator to look
    /// at the wrong limit.
    AtomUtf16TooLong {
        /// Supplied length in UTF-16 code units.
        units: usize,
        /// The business bound.
        max: usize,
    },
    /// The body is legal opaque content, so only a length failure can occur — and it can
    /// only come from its `u32` prefix.
    BodyTooLong {
        /// Supplied byte length.
        len: u64,
    },
    /// The declared author exceeds its canonical bound or contains a NUL.
    InvalidDeclaredAuthor,
    /// The submission reason contains a NUL or exceeds the canonical text bound.
    InvalidReason,
    /// A length-prefixed value is not valid UTF-8.
    NotUtf8,
    /// A decision value is outside {0, 1}.
    UnknownDecision(u16),
    /// The review note is empty, over-long or contains a NUL.
    InvalidNote,
    /// A generated length does not fit its `u32` prefix.
    LengthOverflow,
    /// The encoded or supplied payload exceeds the registered operation ceiling.
    TooLarge {
        /// Measured byte length.
        len: u64,
        /// The registered ceiling.
        max: u32,
    },
    /// A decoded proposal key is not a proposal.
    NotAProposal(RecordKind),
    /// This module's own layout arithmetic disagreed with the bytes it emitted.
    ///
    /// Unreachable through the public encoders, which build a payload of a length they
    /// already admitted; it exists so an impossible arm reports itself as an internal
    /// defect rather than as a resource refusal.
    InternalInvariant,
    /// The caller's ledger refused or stopped the work.
    Refused(Refusal),
}

impl From<Refusal> for ConceptTextPayloadError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// A decoded `SubmitConceptText` payload, borrowing the caller's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedSubmit<'a> {
    /// The term the edit applies to.
    pub subject: RecordKey,
    /// Which fields the edit names.
    pub field_mask: u8,
    /// The replacement atom, when named.
    pub atom: Option<&'a str>,
    /// The replacement body, when named.
    pub body: Option<&'a str>,
    /// Declared attribution, verbatim.
    pub declared_author: &'a str,
    /// Original submission reason, present only in v2.
    pub reason: Option<&'a str>,
}

/// A decoded `ReviewConceptText` payload, borrowing the caller's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodedReview<'a> {
    /// The proposal being decided.
    pub proposal: RecordKey,
    /// The decision wire value.
    pub decision: u16,
    /// The reviewer's note.
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

    fn take(&mut self, len: usize) -> Result<&'a [u8], ConceptTextPayloadError> {
        let end = self
            .at
            .checked_add(len)
            .ok_or(ConceptTextPayloadError::Truncated)?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(ConceptTextPayloadError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ConceptTextPayloadError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ConceptTextPayloadError> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, ConceptTextPayloadError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u128(&mut self) -> Result<u128, ConceptTextPayloadError> {
        let bytes = self.take(16)?;
        let mut raw = [0u8; 16];
        raw.copy_from_slice(bytes);
        Ok(u128::from_le_bytes(raw))
    }

    fn key(&mut self) -> Result<RecordKey, ConceptTextPayloadError> {
        let raw_kind = self.u16()?;
        let kind =
            RecordKind::from_u16(raw_kind).ok_or(ConceptTextPayloadError::UnknownKind(raw_kind))?;
        let key = RecordKey::new(kind, self.u128()?);
        if !key.id_is_in_range() {
            return Err(ConceptTextPayloadError::IdOutOfRange { kind });
        }
        Ok(key)
    }

    fn str(&mut self) -> Result<&'a str, ConceptTextPayloadError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).map_err(|_| ConceptTextPayloadError::NotUtf8)
    }

    fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// Charge and check one decode pass over `len` bytes.
///
/// The charge is the work actually performed, not a guess: a pass walks every byte once.
fn admit_pass(ledger: &mut dyn SharedLedger, len: usize) -> Result<(), ConceptTextPayloadError> {
    ledger
        .checkpoint()
        .map_err(|stop| ConceptTextPayloadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge(len as u64)
        .map_err(|error| ConceptTextPayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .try_charge_entries(1)
        .map_err(|error| ConceptTextPayloadError::Refused(Refusal::Budget(error)))?;
    Ok(())
}

/// The declared ceiling check shared by both directions.
fn check_ceiling(len: u64) -> Result<(), ConceptTextPayloadError> {
    if len > u64::from(MAX_PAYLOAD) {
        return Err(ConceptTextPayloadError::TooLarge {
            len,
            max: MAX_PAYLOAD,
        });
    }
    Ok(())
}

/// Validate a key's domain before any bytes are produced.
fn check_key(key: RecordKey, required: RecordKind) -> Result<(), ConceptTextPayloadError> {
    if !key.id_is_in_range() {
        return Err(ConceptTextPayloadError::IdOutOfRange { kind: key.kind() });
    }
    if key.kind() != required {
        return Err(ConceptTextPayloadError::WrongKind {
            expected: required,
            found: key.kind(),
        });
    }
    Ok(())
}

/// Validate the atom against its canonical bounds.
///
/// A NUL is refused because a NUL in a persisted string is a format violation, and an
/// empty atom is refused because the term's atom is its one-line summary: replacing it
/// with nothing claims an edit that removes information rather than restating it.
pub(crate) fn check_atom(atom: &str) -> Result<(), ConceptTextPayloadError> {
    if atom.bytes().any(|byte| byte == 0) {
        return Err(ConceptTextPayloadError::InvalidAtom);
    }
    if atom.is_empty() {
        return Err(ConceptTextPayloadError::InvalidAtom);
    }
    // The canonical byte ceiling first: it is the storage contract, and an atom that cannot be
    // persisted should be refused for the reason that owns that limit.
    let len = atom.len() as u64;
    if len > u64::from(MAX_ATOM_BYTES) {
        return Err(ConceptTextPayloadError::AtomTooLong {
            len,
            max: MAX_ATOM_BYTES,
        });
    }
    // Then the business ceiling, counted exactly as the shipped JS validators count it. This
    // is allocation-free: `encode_utf16()` is a lazy iterator and nothing is collected.
    let units = atom.encode_utf16().count();
    if units > MAX_ATOM_UTF16_UNITS {
        return Err(ConceptTextPayloadError::AtomUtf16TooLong {
            units,
            max: MAX_ATOM_UTF16_UNITS,
        });
    }
    Ok(())
}

/// Validate the body as opaque content.
///
/// Empty, Unicode and NUL are all legal: the body is a blob, and a caller may legitimately
/// want it empty. Only the `u32` length prefix can fail.
fn check_body(body: &str) -> Result<(), ConceptTextPayloadError> {
    u32::try_from(body.len()).map_err(|_| ConceptTextPayloadError::BodyTooLong {
        len: body.len() as u64,
    })?;
    Ok(())
}

/// Validate the declared author against its canonical bound and the NUL rule.
pub(crate) fn check_declared_author(author: &str) -> Result<(), ConceptTextPayloadError> {
    if author.bytes().any(|byte| byte == 0) {
        return Err(ConceptTextPayloadError::InvalidDeclaredAuthor);
    }
    if author.len() as u64 > u64::from(MAX_DECLARED_AUTHOR) {
        return Err(ConceptTextPayloadError::InvalidDeclaredAuthor);
    }
    Ok(())
}

fn check_reason(reason: &str) -> Result<(), ConceptTextPayloadError> {
    if !u32::try_from(reason.len()).is_ok_and(|len| len <= MAX_TEXT_BYTES)
        || reason.as_bytes().contains(&0)
    {
        return Err(ConceptTextPayloadError::InvalidReason);
    }
    Ok(())
}

/// Validate a review note.
fn check_note(note: &str) -> Result<(), ConceptTextPayloadError> {
    if note.is_empty() {
        return Err(ConceptTextPayloadError::InvalidNote);
    }
    if note.bytes().any(|byte| byte == 0) {
        return Err(ConceptTextPayloadError::InvalidNote);
    }
    if note.len() as u64 > u64::from(MAX_TEXT_BYTES) {
        return Err(ConceptTextPayloadError::InvalidNote);
    }
    Ok(())
}

/// Validate the mask and the presence of exactly the fields it names, without touching bytes.
fn check_edit(edit: &ConceptTextEdit<'_>) -> Result<(), ConceptTextPayloadError> {
    if !MASK_ALLOWED.contains(&edit.field_mask) {
        return Err(ConceptTextPayloadError::UnknownMask(edit.field_mask));
    }
    match (edit.replaces_atom(), edit.atom) {
        (true, Some(atom)) => check_atom(atom)?,
        (true, None) => return Err(ConceptTextPayloadError::MissingField { bit: MASK_ATOM }),
        (false, Some(_)) => {
            return Err(ConceptTextPayloadError::UnexpectedField { bit: MASK_ATOM });
        }
        (false, None) => {}
    }
    match (edit.replaces_body(), edit.body) {
        (true, Some(body)) => check_body(body)?,
        (true, None) => return Err(ConceptTextPayloadError::MissingField { bit: MASK_BODY }),
        (false, Some(_)) => {
            return Err(ConceptTextPayloadError::UnexpectedField { bit: MASK_BODY });
        }
        (false, None) => {}
    }
    check_declared_author(edit.declared_author)?;
    Ok(())
}

/// Admit exactly `total` bytes, then return the reserved buffer.
///
/// The ceiling and the arithmetic both run first, so a payload that could never be legal is
/// refused before the allocator is asked for it.
fn admit(ledger: &mut dyn SharedLedger, total: usize) -> Result<Vec<u8>, ConceptTextPayloadError> {
    check_ceiling(total as u64)?;
    ledger
        .checkpoint()
        .map_err(|stop| ConceptTextPayloadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge(total as u64)
        .map_err(|error| ConceptTextPayloadError::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| ConceptTextPayloadError::Refused(Refusal::Stopped(stop)))?;
    let mut out = Vec::new();
    out.try_reserve_exact(total)
        .map_err(|_| ConceptTextPayloadError::Refused(Refusal::Allocation { requested: total }))?;
    Ok(out)
}

fn put_key(out: &mut Vec<u8>, key: RecordKey) {
    out.extend_from_slice(&key.kind().as_u16().to_le_bytes());
    out.extend_from_slice(&key.id().to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    // The caller validated that each length fits its prefix before this ran.
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

/// Bytes of one edit's body, in payload order.
fn edit_len(edit: &ConceptTextEdit<'_>) -> Result<usize, ConceptTextPayloadError> {
    let mut total = KEY_BYTES
        .checked_add(MASK_BYTES)
        .ok_or(ConceptTextPayloadError::LengthOverflow)?;
    if let Some(atom) = edit.atom {
        total = total
            .checked_add(LEN_BYTES)
            .and_then(|n| n.checked_add(atom.len()))
            .ok_or(ConceptTextPayloadError::LengthOverflow)?;
    }
    if let Some(body) = edit.body {
        total = total
            .checked_add(LEN_BYTES)
            .and_then(|n| n.checked_add(body.len()))
            .ok_or(ConceptTextPayloadError::LengthOverflow)?;
    }
    total
        .checked_add(LEN_BYTES)
        .and_then(|n| n.checked_add(edit.declared_author.len()))
        .ok_or(ConceptTextPayloadError::LengthOverflow)
}

fn put_edit(out: &mut Vec<u8>, edit: &ConceptTextEdit<'_>) {
    put_key(out, edit.subject);
    out.push(edit.field_mask);
    if let Some(atom) = edit.atom {
        put_str(out, atom);
    }
    if let Some(body) = edit.body {
        put_str(out, body);
    }
    put_str(out, edit.declared_author);
}

/// Encode a `SubmitConceptText` v1 payload.
///
/// # Errors
///
/// [`ConceptTextPayloadError::WrongKind`] / [`ConceptTextPayloadError::IdOutOfRange`] for a
/// key outside its position's domain, [`ConceptTextPayloadError::UnknownMask`] for a mask
/// naming no field or an undefined bit, [`ConceptTextPayloadError::MissingField`] /
/// [`ConceptTextPayloadError::UnexpectedField`] when the carried fields disagree with the
/// mask, [`ConceptTextPayloadError::InvalidAtom`] /
/// [`ConceptTextPayloadError::AtomTooLong`] for an empty, NUL-bearing or over-long atom,
/// [`ConceptTextPayloadError::InvalidDeclaredAuthor`], [`ConceptTextPayloadError::TooLarge`]
/// above the operation ceiling, and [`ConceptTextPayloadError::Refused`] for the caller's
/// own budget, stop or allocation refusal.
pub fn encode_submit(
    edit: &ConceptTextEdit<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, ConceptTextPayloadError> {
    encode_submit_fields(edit, None, ledger)
}

/// Encode a v2 text submission including its original reason.
///
/// # Errors
///
/// As [`encode_submit`], plus [`ConceptTextPayloadError::InvalidReason`].
pub fn encode_submit_with_reason(
    edit: &ConceptTextEdit<'_>,
    reason: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, ConceptTextPayloadError> {
    encode_submit_fields(edit, Some(reason), ledger)
}

fn encode_submit_fields(
    edit: &ConceptTextEdit<'_>,
    reason: Option<&str>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, ConceptTextPayloadError> {
    check_key(edit.subject, RecordKind::Term)?;
    check_edit(edit)?;
    let mut total = edit_len(edit)?;
    if let Some(reason) = reason {
        ledger.checkpoint().map_err(Refusal::Stopped)?;
        check_reason(reason)?;
        total = total
            .checked_add(LEN_BYTES)
            .and_then(|len| len.checked_add(reason.len()))
            .ok_or(ConceptTextPayloadError::LengthOverflow)?;
    }
    let mut out = admit(ledger, total)?;
    put_edit(&mut out, edit);
    if let Some(reason) = reason {
        put_str(&mut out, reason);
    }
    if out.len() != total {
        return Err(ConceptTextPayloadError::InternalInvariant);
    }
    Ok(out)
}

/// Encode a `ReviewConceptText` v1 payload.
///
/// # Errors
///
/// As [`encode_submit`] for the key, plus
/// [`ConceptTextPayloadError::UnknownDecision`] outside {0, 1},
/// [`ConceptTextPayloadError::InvalidNote`] for an empty, over-long or NUL-bearing note.
pub fn encode_review(
    proposal: RecordKey,
    decision: u16,
    note: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, ConceptTextPayloadError> {
    check_key(proposal, RecordKind::Proposal)?;
    if decision != DECISION_APPROVE && decision != DECISION_REJECT {
        return Err(ConceptTextPayloadError::UnknownDecision(decision));
    }
    check_note(note)?;
    let total = KEY_BYTES
        .checked_add(2)
        .and_then(|n| n.checked_add(LEN_BYTES))
        .and_then(|n| n.checked_add(note.len()))
        .ok_or(ConceptTextPayloadError::LengthOverflow)?;
    let mut out = admit(ledger, total)?;
    put_key(&mut out, proposal);
    out.extend_from_slice(&decision.to_le_bytes());
    put_str(&mut out, note);
    if out.len() != total {
        return Err(ConceptTextPayloadError::InternalInvariant);
    }
    Ok(out)
}

/// The mask and fields read from one submit payload.
struct ReadEdit<'a> {
    /// Which fields the edit names.
    field_mask: u8,
    /// The replacement atom, when named.
    atom: Option<&'a str>,
    /// The replacement body, when named.
    body: Option<&'a str>,
    /// Declared attribution, verbatim.
    declared_author: &'a str,
}

/// Read the mask and its fields, borrowing the payload.
fn read_edit<'a>(cursor: &mut Cursor<'a>) -> Result<ReadEdit<'a>, ConceptTextPayloadError> {
    let field_mask = cursor.u8()?;
    if !MASK_ALLOWED.contains(&field_mask) {
        return Err(ConceptTextPayloadError::UnknownMask(field_mask));
    }
    let atom = if field_mask & MASK_ATOM != 0 {
        let value = cursor.str()?;
        check_atom(value)?;
        Some(value)
    } else {
        None
    };
    let body = if field_mask & MASK_BODY != 0 {
        let value = cursor.str()?;
        check_body(value)?;
        Some(value)
    } else {
        None
    };
    let declared_author = cursor.str()?;
    check_declared_author(declared_author)?;
    Ok(ReadEdit {
        field_mask,
        atom,
        body,
        declared_author,
    })
}

/// Decode a `SubmitConceptText` v1 payload.
///
/// # Errors
///
/// [`ConceptTextPayloadError::TooLarge`] before any traversal,
/// [`ConceptTextPayloadError::Truncated`] / [`ConceptTextPayloadError::Trailing`] for a
/// length disagreement, an unknown or out-of-range kind, an illegal mask, a malformed atom,
/// an invalid declared author, or invalid UTF-8, and
/// [`ConceptTextPayloadError::Refused`] for the caller's own budget or stop.
pub fn decode_submit<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedSubmit<'a>, ConceptTextPayloadError> {
    decode_submit_fields(payload, false, ledger)
}

/// Decode a v2 text submission including its original reason.
///
/// # Errors
///
/// As [`decode_submit`], plus [`ConceptTextPayloadError::InvalidReason`].
pub fn decode_submit_with_reason<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedSubmit<'a>, ConceptTextPayloadError> {
    decode_submit_fields(payload, true, ledger)
}

fn decode_submit_fields<'a>(
    payload: &'a [u8],
    has_reason: bool,
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedSubmit<'a>, ConceptTextPayloadError> {
    check_ceiling(payload.len() as u64)?;
    admit_pass(ledger, payload.len())?;
    let mut cursor = Cursor::new(payload);
    let subject = cursor.key()?;
    if subject.kind() != RecordKind::Term {
        return Err(ConceptTextPayloadError::WrongKind {
            expected: RecordKind::Term,
            found: subject.kind(),
        });
    }
    let edit = read_edit(&mut cursor)?;
    let reason = if has_reason {
        let value = cursor.str()?;
        check_reason(value)?;
        Some(value)
    } else {
        None
    };
    if !cursor.at_end() {
        return Err(ConceptTextPayloadError::Trailing);
    }
    Ok(DecodedSubmit {
        subject,
        field_mask: edit.field_mask,
        atom: edit.atom,
        body: edit.body,
        declared_author: edit.declared_author,
        reason,
    })
}

/// Decode a `ReviewConceptText` v1 payload.
///
/// # Errors
///
/// As [`decode_submit`], plus [`ConceptTextPayloadError::NotAProposal`] for a key that is
/// not a proposal, [`ConceptTextPayloadError::UnknownDecision`] and
/// [`ConceptTextPayloadError::InvalidNote`].
pub fn decode_review<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedReview<'a>, ConceptTextPayloadError> {
    check_ceiling(payload.len() as u64)?;
    admit_pass(ledger, payload.len())?;
    let mut cursor = Cursor::new(payload);
    let proposal = cursor.key()?;
    if proposal.kind() != RecordKind::Proposal {
        return Err(ConceptTextPayloadError::NotAProposal(proposal.kind()));
    }
    let decision = cursor.u16()?;
    if decision != DECISION_APPROVE && decision != DECISION_REJECT {
        return Err(ConceptTextPayloadError::UnknownDecision(decision));
    }
    let note = cursor.str()?;
    check_note(note)?;
    if !cursor.at_end() {
        return Err(ConceptTextPayloadError::Trailing);
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
    use sage_db_types::query::RunToCompletion;

    fn ledger() -> sage_db_store::lease::AttemptLedger<'static> {
        static CONTROL: RunToCompletion = RunToCompletion;
        sage_db_store::lease::AttemptLedger::open(1 << 22, 1 << 16, &CONTROL).unwrap()
    }

    fn subject() -> RecordKey {
        RecordKey::term(1)
    }

    #[test]
    fn operation_kinds_and_version_are_the_assigned_ones() {
        assert_eq!(SUBMIT_CONCEPT_TEXT, 7);
        assert_eq!(REVIEW_CONCEPT_TEXT, 8);
        assert_eq!(OPERATION_VERSION, 1);
    }

    #[test]
    fn every_mask_round_trips_and_preserves_the_distinction_between_absent_and_empty() {
        let mut l = ledger();
        for (mask, atom, body) in [
            (MASK_ATOM, Some("a"), None),
            (MASK_BODY, None, Some("")),
            (MASK_ATOM | MASK_BODY, Some("a"), Some("b")),
        ] {
            let edit = ConceptTextEdit {
                subject: subject(),
                field_mask: mask,
                atom,
                body,
                declared_author: "",
            };
            let bytes = encode_submit(&edit, &mut l).expect("encodes");
            let decoded = decode_submit(&bytes, &mut l).expect("decodes");
            assert_eq!(decoded.field_mask, mask);
            assert_eq!(decoded.atom, atom, "absent atom stays absent");
            assert_eq!(decoded.body, body, "an empty body stays empty, not absent");
        }
    }

    #[test]
    fn a_mask_naming_no_field_is_refused() {
        let mut l = ledger();
        let edit = ConceptTextEdit {
            subject: subject(),
            field_mask: 0,
            atom: None,
            body: None,
            declared_author: "",
        };
        assert_eq!(
            encode_submit(&edit, &mut l),
            Err(ConceptTextPayloadError::UnknownMask(0))
        );
    }

    #[test]
    fn fields_must_agree_with_the_mask() {
        let mut l = ledger();
        let missing = ConceptTextEdit {
            subject: subject(),
            field_mask: MASK_ATOM,
            atom: None,
            body: None,
            declared_author: "",
        };
        assert_eq!(
            encode_submit(&missing, &mut l),
            Err(ConceptTextPayloadError::MissingField { bit: MASK_ATOM })
        );
        let unexpected = ConceptTextEdit {
            subject: subject(),
            field_mask: MASK_ATOM,
            atom: Some("a"),
            body: Some("b"),
            declared_author: "",
        };
        assert_eq!(
            encode_submit(&unexpected, &mut l),
            Err(ConceptTextPayloadError::UnexpectedField { bit: MASK_BODY })
        );
    }

    #[test]
    fn the_atom_bound_is_exact_and_an_empty_atom_is_refused() {
        let mut l = ledger();
        // BMP characters: one UTF-16 unit each, so the business bound is the binding one.
        let at_max = "x".repeat(MAX_ATOM_UTF16_UNITS);
        let edit = ConceptTextEdit {
            subject: subject(),
            field_mask: MASK_ATOM,
            atom: Some(&at_max),
            body: None,
            declared_author: "",
        };
        assert!(encode_submit(&edit, &mut l).is_ok());

        let over = "x".repeat(MAX_ATOM_UTF16_UNITS + 1);
        let edit_over = ConceptTextEdit {
            atom: Some(&over),
            ..edit
        };
        assert_eq!(
            encode_submit(&edit_over, &mut l),
            Err(ConceptTextPayloadError::AtomUtf16TooLong {
                units: MAX_ATOM_UTF16_UNITS + 1,
                max: MAX_ATOM_UTF16_UNITS,
            })
        );

        // Both bounds are exceeded here: 270 BMP units and 810 UTF-8 bytes.
        // The canonical byte guard owns the first refusal; this proves error priority.
        let wide = "世".repeat(270);
        assert_eq!(
            encode_submit(
                &ConceptTextEdit {
                    atom: Some(&wide),
                    ..edit
                },
                &mut l
            ),
            Err(ConceptTextPayloadError::AtomTooLong {
                len: 810,
                max: MAX_ATOM_BYTES,
            })
        );

        let empty = ConceptTextEdit {
            atom: Some(""),
            ..edit
        };
        assert_eq!(
            encode_submit(&empty, &mut l),
            Err(ConceptTextPayloadError::InvalidAtom)
        );
    }

    #[test]
    fn a_nul_bearing_atom_is_refused_but_a_nul_body_is_legal() {
        let mut l = ledger();
        let atom = ConceptTextEdit {
            subject: subject(),
            field_mask: MASK_ATOM,
            atom: Some("a\0b"),
            body: None,
            declared_author: "",
        };
        assert_eq!(
            encode_submit(&atom, &mut l),
            Err(ConceptTextPayloadError::InvalidAtom)
        );
        let body = ConceptTextEdit {
            subject: subject(),
            field_mask: MASK_BODY,
            atom: None,
            body: Some("a\0b"),
            declared_author: "",
        };
        let bytes = encode_submit(&body, &mut l).expect("an opaque body may contain NUL");
        assert_eq!(decode_submit(&bytes, &mut l).unwrap().body, Some("a\0b"));
    }

    #[test]
    fn the_review_shape_round_trips_and_rejects_bad_decisions_and_empty_notes() {
        let mut l = ledger();
        let proposal = RecordKey::new(RecordKind::Proposal, 3);
        let bytes = encode_review(proposal, DECISION_APPROVE, "ok", &mut l).expect("encodes");
        let decoded = decode_review(&bytes, &mut l).expect("decodes");
        assert_eq!(decoded.proposal, proposal);
        assert_eq!(decoded.decision, DECISION_APPROVE);
        assert_eq!(decoded.note, "ok");

        assert_eq!(
            encode_review(proposal, 2, "ok", &mut l),
            Err(ConceptTextPayloadError::UnknownDecision(2))
        );
        assert_eq!(
            encode_review(proposal, DECISION_APPROVE, "", &mut l),
            Err(ConceptTextPayloadError::InvalidNote)
        );
    }

    #[test]
    fn a_wrong_kind_and_a_trailing_byte_are_refusals() {
        let mut l = ledger();
        let not_term = ConceptTextEdit {
            subject: RecordKey::new(RecordKind::Claim, 1),
            field_mask: MASK_BODY,
            atom: None,
            body: Some("b"),
            declared_author: "",
        };
        assert_eq!(
            encode_submit(&not_term, &mut l),
            Err(ConceptTextPayloadError::WrongKind {
                expected: RecordKind::Term,
                found: RecordKind::Claim,
            })
        );

        let edit = ConceptTextEdit {
            subject: subject(),
            field_mask: MASK_BODY,
            atom: None,
            body: Some("b"),
            declared_author: "",
        };
        let mut bytes = encode_submit(&edit, &mut l).expect("encodes");
        bytes.push(0xFF);
        assert_eq!(
            decode_submit(&bytes, &mut l),
            Err(ConceptTextPayloadError::Trailing)
        );
    }
}
