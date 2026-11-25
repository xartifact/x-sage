//! Submit an authored assertion or a scoped replacement, then approve or reject it.
//!
//! # What this module is
//!
//! The unstructured-edge counterpart of [`crate::concept`]. A submitter proposes that
//! `from --relation--> target` should exist as a **new independent** assertion; a reviewer with
//! exactly [`Capability::Review`] decides. Approval allocates a fresh [`AssertionId`] and
//! publishes the assertion, the decided proposal, the id allocation and the outcome in one
//! transaction.
//!
//! Per command it composes, in order:
//!
//! 1. exactly one capability — `Propose` to submit, `Review` to decide. Neither implies the
//!    other, and no `Read` grant is inferred;
//! 2. caller-input checks and the canonical P1 payload, before anything is read;
//! 3. **idempotency first**: an exact retry returns the original [`HandleOutcome`] before any
//!    current-state, lifecycle or collision check. A different digest for a recorded request is
//!    a mismatch, never a silent replay;
//! 4. the write fence, then the real import gate;
//! 5. the authoritative reads the decision needs;
//! 6. one `PreparedTransaction` and the real `Store::commit`.
//!
//! # What approval means, and what it does not
//!
//! The candidate assertion is a **new support** with a fresh identity. A duplicate
//! `(from, relation, target)` triple is a separate assertion with its own identity, because
//! two people asserting the same edge are two pieces of evidence. A replacement pins one
//! active assertion revision and withdraws that support atomically with creating the new one.
//!
//! Approval validates the entire resulting active graph under the committed rule profile using
//! [`sage_db_infer::validate_candidate`]. For replacements the candidate omits the pinned support. A
//! candidate the rules reject returns a typed validation refusal and **no** write: the proposal
//! stays `Pending` so the reviewer can reject it with a different request.
//!
//! # Review derives the candidate from the stored bytes — on APPROVAL
//!
//! On approval the reviewer never supplies the edge: the stored request frame is decoded once
//! under its leased owner, the endpoints come from **that** payload, and the proposal's metadata
//! must agree with it. A caller cannot narrow the required baselines by passing fewer of them.
//!
//! A **rejection** deliberately does not decode the frame. `kernel-operations` says a rejection
//! only closes the proposal and is a real commit, so it needs no candidate — and a reviewer must
//! be able to close a proposal whose payload is inconsistent, which the full integrity gate would
//! forbid. The payload blob is still read and hash-verified; only the intent decode and its
//! cross-checks are approval-only.

use sage_db_infer::validate::Violation;
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::lease::LeaseLedger;
use sage_db_store::typed::{self, EventInput};
use sage_db_store::view::{CommittedBlob, ViewBasis};
use sage_db_types::budget::Budget;
use sage_db_types::confidence::Confidence;
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::identity::{AssertionId, RecordRevision};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ActorOrigin, AssertionRecordFull, CommonFields, Extensions, IdAllocationEvent, IdSpace,
    ProposalKind, ProposalRecord, ProposalStatus, RecordKey, RecordKind, RevPair, Watermark,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};
use sage_db_types::query::{QueryControl, QueryError};
use sage_db_types::relation::RelationId;

use crate::auth::{AuthError, Capability, Grants};
use crate::candidate::{self, Addition, CandidateError};
use crate::catalog::{self, CatalogError, SlugResolution, SlugScanError};
use crate::claims::{
    CommandError, InputError, InternalDefect, RequestMeta, StoredField, admitted_string,
    admitted_vec, check_meta, ensure_import_ready, extent, folded_current,
    frame_and_digest_with_version, map_format, outcome_event, retry_outcome, revision_next,
    version_next,
};

/// The registered operation kind for `SubmitRelationAssert` v1.
pub const SUBMIT_RELATION_ASSERT: u16 = 10;

/// The registered operation kind for `ReviewRelationAssert` v1.
pub const REVIEW_RELATION_ASSERT: u16 = 11;

/// The only operation version this build implements.
pub const OPERATION_VERSION: u16 = 1;

// Operation 10 v4 uses the v1 payload and a third read-set row pinning an Assertion.
// The legacy filename formats already use versions 2 and 3.
const REPLACEMENT_SUBMIT_VERSION: u16 = 4;

/// The decision value that approves.
pub const DECISION_APPROVE: u16 = 0;

/// The decision value that rejects.
pub const DECISION_REJECT: u16 = 1;

/// Bytes of a canonical [`RecordKey`] on the wire: `kind:u16 | id:u128`.
const KEY_BYTES: usize = 18;

/// Bytes of a canonical string's length prefix.
const LEN_BYTES: usize = 4;

use crate::legacy_proposal::{self, LegacyDirectory, LegacyIntent, LegacyProposal, LegacyReview};

/// One submitted relation-assert request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RelationAssert<'a, Target = RecordKey> {
    /// Authored subject; also the proposal's `subject`.
    pub from: RecordKey,
    /// Authored object.
    pub target: Target,
    /// Relation template, as its registered discriminant.
    pub relation: u16,
    /// Declared confidence, as its registered discriminant.
    pub confidence: u16,
    /// Authoring order, preserved for export parity.
    pub declaration_order: u32,
    /// Literal attribution, distinct from the authenticated actor.
    pub declared_author: &'a str,
    /// Why this edge is proposed; becomes the proposal's own reason.
    pub reason: &'a str,
}

impl<'a, Target> RelationAssert<'a, Target> {
    fn with_target<Next>(self, target: Next) -> RelationAssert<'a, Next> {
        RelationAssert {
            from: self.from,
            target,
            relation: self.relation,
            confidence: self.confidence,
            declaration_order: self.declaration_order,
            declared_author: self.declared_author,
            reason: self.reason,
        }
    }
}

#[derive(Clone, Copy)]
enum SubmitTarget<'a> {
    Key(RecordKey),
    Slug(&'a str),
}

/// A caller-supplied relation-assert value this operation cannot accept.
///
/// Every variant is detected **before** a byte is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelationAssertPayloadError {
    /// A key occupies a position of the wrong record kind.
    WrongKeyKind {
        /// The kind the position requires.
        expected: RecordKind,
        /// The kind supplied.
        found: RecordKind,
    },
    /// A key's id exceeds the bound its kind declares.
    KeyOutOfRange(RecordKey),
    /// `from` and `target` name the same term, which no authored assertion may.
    SelfRelation,
    /// The relation discriminant is not a registered relation.
    UnknownRelation(u16),
    /// The confidence discriminant is not a registered confidence.
    UnknownConfidence(u16),
    /// The declared author is NUL-bearing or exceeds its canonical attribution ceiling.
    InvalidAuthor,
    /// The reason is empty, NUL-bearing, or over its ceiling.
    InvalidReason,
    /// The target is not an authored lowercase legacy slug.
    InvalidTargetSlug,
    /// The review note is empty, NUL-bearing, or over its ceiling.
    InvalidNote,
    /// The decision discriminant is neither approve nor reject.
    UnknownDecision(u16),
    /// The payload's total length is not representable.
    LengthOverflow,
    /// This module's own length arithmetic disagreed with the bytes it emitted.
    InternalInvariant,
    /// The caller's ledger refused while the payload was being built.
    Refused(Refusal),
}

impl From<Refusal> for RelationAssertPayloadError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<sage_db_types::query::ControlStop> for RelationAssertPayloadError {
    fn from(stop: sage_db_types::query::ControlStop) -> Self {
        Self::Refused(Refusal::Stopped(stop))
    }
}

impl From<sage_db_types::ledger::BudgetError> for RelationAssertPayloadError {
    fn from(error: sage_db_types::ledger::BudgetError) -> Self {
        Self::Refused(Refusal::Budget(error))
    }
}

/// A submitted relation-assert request, borrowed from its own payload bytes.
#[derive(Clone, Copy, Debug)]
pub struct DecodedSubmit<'a, Target = RecordKey> {
    /// Authored subject.
    pub from: RecordKey,
    /// Authored object.
    pub target: Target,
    /// Relation discriminant, already checked to be registered.
    pub relation: RelationId,
    /// Confidence discriminant, already checked to be registered.
    pub confidence: Confidence,
    /// Authoring order.
    pub declaration_order: u32,
    /// Literal attribution.
    pub declared_author: &'a str,
    /// The submitter's reason.
    pub reason: &'a str,
}

/// A submitted edge whose object was authored as a slug rather than an established identity.
#[derive(Clone, Copy, Debug)]
pub struct DecodedPendingRelation<'a> {
    /// Existing subject key.
    pub from: RecordKey,
    /// Exact target spelling supplied at submission.
    pub target_slug: &'a str,
    /// Authored relation.
    pub relation: RelationId,
    /// Authored confidence.
    pub confidence: Confidence,
    /// Authored order as carried by the request.
    pub declaration_order: u32,
    /// Literal attribution.
    pub declared_author: &'a str,
    /// Literal submission reason.
    pub reason: &'a str,
}

/// A reviewed decision, borrowed from its own payload bytes.
#[derive(Clone, Copy, Debug)]
pub struct DecodedReview<'a> {
    /// The proposal being decided.
    pub proposal: RecordKey,
    /// `0` approve, `1` reject.
    pub decision: u16,
    /// The reviewer's note.
    pub note: &'a str,
}

// ---------------------------------------------------------------------------
// Canonical payload bytes
// ---------------------------------------------------------------------------

/// Reserve `total` bytes on the ledger, then build an empty buffer.
fn admit(
    ledger: &mut dyn SharedLedger,
    total: usize,
) -> Result<Vec<u8>, RelationAssertPayloadError> {
    ledger.checkpoint()?;
    ledger.try_charge(total as u64)?;
    ledger.checkpoint()?;
    let mut out = Vec::new();
    out.try_reserve_exact(total)
        .map_err(|_| Refusal::Allocation { requested: total })?;
    Ok(out)
}

/// Append a canonical key: `kind:u16 | id:u128`, little-endian.
fn put_key(out: &mut Vec<u8>, key: RecordKey) {
    out.extend_from_slice(&key.kind().as_u16().to_le_bytes());
    out.extend_from_slice(&key.id().to_le_bytes());
}

/// Read a canonical key from `bytes` at `at`.
fn get_key(bytes: &[u8], at: usize) -> Option<RecordKey> {
    let kind_bytes = bytes.get(at..at.checked_add(2)?)?;
    let id_bytes = bytes.get(at.checked_add(2)?..at.checked_add(KEY_BYTES)?)?;
    let kind = RecordKind::from_u16(u16::from_le_bytes([kind_bytes[0], kind_bytes[1]]))?;
    let mut raw = [0u8; 16];
    raw.copy_from_slice(id_bytes);
    Some(RecordKey::new(kind, u128::from_le_bytes(raw)))
}

/// Append a canonical string: `u32` byte length, then the bytes.
fn put_str(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

/// Read a canonical string from `bytes` at `at`, returning it and the next offset.
fn get_str(bytes: &[u8], at: usize) -> Option<(&str, usize)> {
    let len_bytes = bytes.get(at..at.checked_add(LEN_BYTES)?)?;
    let len = u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
    let start = at.checked_add(LEN_BYTES)?;
    let end = start.checked_add(len)?;
    let raw = bytes.get(start..end)?;
    let text = core::str::from_utf8(raw).ok()?;
    Some((text, end))
}

/// Reject a key whose kind is not the one this position requires.
fn check_key(key: RecordKey, required: RecordKind) -> Result<(), RelationAssertPayloadError> {
    if key.kind() != required {
        return Err(RelationAssertPayloadError::WrongKeyKind {
            expected: required,
            found: key.kind(),
        });
    }
    if !key.id_is_in_range() {
        return Err(RelationAssertPayloadError::KeyOutOfRange(key));
    }
    Ok(())
}

/// A canonical persisted string: non-empty, NUL-free, within `max` bytes.
fn check_text(value: &str, max: u32) -> bool {
    u32::try_from(value.len()).is_ok_and(|len| len <= max)
        && !value.is_empty()
        && !value.bytes().any(|byte| byte == 0)
}

/// Attribution may be absent; the canonical declared-author ceiling is distinct
/// from the smaller authenticated-identity ceiling. Check length before scanning.
fn check_author(value: &str) -> bool {
    u32::try_from(value.len()).is_ok_and(|len| len <= typed::MAX_DECLARED_AUTHOR)
        && !value.as_bytes().contains(&0)
}

/// Encode a `SubmitRelationAssert` v1 payload.
///
/// # Errors
///
/// [`RelationAssertPayloadError`] for a key of the wrong kind, an out-of-range id, identical
/// endpoints, an unregistered relation or confidence, malformed author/reason text, or a real
/// ledger refusal.
pub fn encode_submit(
    request: &RelationAssert<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, RelationAssertPayloadError> {
    check_key(request.from, RecordKind::Term)?;
    check_key(request.target, RecordKind::Term)?;
    if request.from == request.target {
        return Err(RelationAssertPayloadError::SelfRelation);
    }
    encode_submit_with_target(
        request,
        KEY_BYTES,
        |out| put_key(out, request.target),
        ledger,
    )
}

fn encode_pending_submit(
    request: &RelationAssert<'_, &str>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, RelationAssertPayloadError> {
    check_key(request.from, RecordKind::Term)?;
    check_target_slug(request.target)?;
    encode_submit_with_target(
        request,
        LEN_BYTES + request.target.len(),
        |out| put_str(out, request.target),
        ledger,
    )
}

fn check_target_slug(slug: &str) -> Result<(), RelationAssertPayloadError> {
    // The legacy product validates the lowercase slug spelling. Its basename
    // has a separate 255-byte cap. No Term field is being written here.
    if slug.len() <= legacy_proposal::MAX_FILENAME_BYTES {
        let mut bytes = slug.bytes();
        if bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Ok(());
        }
    }
    Err(RelationAssertPayloadError::InvalidTargetSlug)
}

fn encode_submit_with_target<Target>(
    request: &RelationAssert<'_, Target>,
    target_bytes: usize,
    append_target: impl FnOnce(&mut Vec<u8>),
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, RelationAssertPayloadError> {
    // The discriminants are checked through their canonical decoders, never by casting.
    let relation = u8::try_from(request.relation)
        .ok()
        .and_then(RelationId::from_u8)
        .ok_or(RelationAssertPayloadError::UnknownRelation(
            request.relation,
        ))?;
    let _ = relation;
    Confidence::from_u16(request.confidence).ok_or(
        RelationAssertPayloadError::UnknownConfidence(request.confidence),
    )?;
    if !check_author(request.declared_author) {
        return Err(RelationAssertPayloadError::InvalidAuthor);
    }
    if !check_text(request.reason, typed::MAX_TEXT_BYTES) {
        return Err(RelationAssertPayloadError::InvalidReason);
    }

    let total = KEY_BYTES
        .checked_add(target_bytes)
        .and_then(|n| n.checked_add(2))
        .and_then(|n| n.checked_add(2))
        .and_then(|n| n.checked_add(4))
        .and_then(|n| n.checked_add(LEN_BYTES))
        .and_then(|n| n.checked_add(request.declared_author.len()))
        .and_then(|n| n.checked_add(LEN_BYTES))
        .and_then(|n| n.checked_add(request.reason.len()))
        .ok_or(RelationAssertPayloadError::LengthOverflow)?;
    let mut out = admit(ledger, total)?;
    put_key(&mut out, request.from);
    append_target(&mut out);
    out.extend_from_slice(&request.relation.to_le_bytes());
    out.extend_from_slice(&request.confidence.to_le_bytes());
    out.extend_from_slice(&request.declaration_order.to_le_bytes());
    put_str(&mut out, request.declared_author);
    put_str(&mut out, request.reason);
    if out.len() != total {
        return Err(RelationAssertPayloadError::InternalInvariant);
    }
    Ok(out)
}

/// Encode a `ReviewRelationAssert` v1 payload.
///
/// # Errors
///
/// [`RelationAssertPayloadError`] for a non-proposal key, an unknown decision, a malformed note,
/// or a real ledger refusal.
pub fn encode_review(
    proposal: RecordKey,
    decision: u16,
    note: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, RelationAssertPayloadError> {
    check_key(proposal, RecordKind::Proposal)?;
    if decision != DECISION_APPROVE && decision != DECISION_REJECT {
        return Err(RelationAssertPayloadError::UnknownDecision(decision));
    }
    if !check_text(note, typed::MAX_TEXT_BYTES) {
        return Err(RelationAssertPayloadError::InvalidNote);
    }
    let total = KEY_BYTES
        .checked_add(2)
        .and_then(|n| n.checked_add(LEN_BYTES))
        .and_then(|n| n.checked_add(note.len()))
        .ok_or(RelationAssertPayloadError::LengthOverflow)?;
    let mut out = admit(ledger, total)?;
    put_key(&mut out, proposal);
    out.extend_from_slice(&decision.to_le_bytes());
    put_str(&mut out, note);
    if out.len() != total {
        return Err(RelationAssertPayloadError::InternalInvariant);
    }
    Ok(out)
}

/// Decode a `SubmitRelationAssert` v1 payload.
///
/// # Errors
///
/// [`RelationAssertPayloadError`] when the bytes are not exactly this payload: a wrong kind, an
/// unknown relation or confidence, a self relation, malformed text, or trailing bytes.
pub fn decode_submit<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedSubmit<'a>, RelationAssertPayloadError> {
    ledger.checkpoint()?;
    let from = get_key(payload, 0).ok_or(RelationAssertPayloadError::InternalInvariant)?;
    check_key(from, RecordKind::Term)?;
    let target =
        get_key(payload, KEY_BYTES).ok_or(RelationAssertPayloadError::InternalInvariant)?;
    check_key(target, RecordKind::Term)?;
    if from == target {
        return Err(RelationAssertPayloadError::SelfRelation);
    }
    decode_submit_tail(payload, KEY_BYTES * 2, from, target)
}

pub(crate) fn decode_pending_submit<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedPendingRelation<'a>, RelationAssertPayloadError> {
    ledger.checkpoint()?;
    let from = get_key(payload, 0).ok_or(RelationAssertPayloadError::InternalInvariant)?;
    check_key(from, RecordKind::Term)?;
    let width: [u8; LEN_BYTES] = payload
        .get(KEY_BYTES..KEY_BYTES + LEN_BYTES)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(RelationAssertPayloadError::InternalInvariant)?;
    if !usize::try_from(u32::from_le_bytes(width))
        .is_ok_and(|width| width != 0 && width <= legacy_proposal::MAX_FILENAME_BYTES)
    {
        return Err(RelationAssertPayloadError::InvalidTargetSlug);
    }
    let (target_slug, at) =
        get_str(payload, KEY_BYTES).ok_or(RelationAssertPayloadError::InternalInvariant)?;
    check_target_slug(target_slug)?;
    let parsed = decode_submit_tail(payload, at, from, target_slug)?;
    Ok(DecodedPendingRelation {
        from,
        target_slug,
        relation: parsed.relation,
        confidence: parsed.confidence,
        declaration_order: parsed.declaration_order,
        declared_author: parsed.declared_author,
        reason: parsed.reason,
    })
}

fn decode_submit_tail<Target>(
    payload: &[u8],
    mut at: usize,
    from: RecordKey,
    target: Target,
) -> Result<DecodedSubmit<'_, Target>, RelationAssertPayloadError> {
    let raw_relation = payload
        .get(at..at + 2)
        .ok_or(RelationAssertPayloadError::InternalInvariant)?;
    let relation_value = u16::from_le_bytes([raw_relation[0], raw_relation[1]]);
    let relation = u8::try_from(relation_value)
        .ok()
        .and_then(RelationId::from_u8)
        .ok_or(RelationAssertPayloadError::UnknownRelation(relation_value))?;
    at += 2;
    let raw_confidence = payload
        .get(at..at + 2)
        .ok_or(RelationAssertPayloadError::InternalInvariant)?;
    let confidence_value = u16::from_le_bytes([raw_confidence[0], raw_confidence[1]]);
    let confidence = Confidence::from_u16(confidence_value).ok_or(
        RelationAssertPayloadError::UnknownConfidence(confidence_value),
    )?;
    at += 2;
    let raw_order = payload
        .get(at..at + 4)
        .ok_or(RelationAssertPayloadError::InternalInvariant)?;
    let declaration_order =
        u32::from_le_bytes([raw_order[0], raw_order[1], raw_order[2], raw_order[3]]);
    at += 4;
    let (declared_author, next) =
        get_str(payload, at).ok_or(RelationAssertPayloadError::InternalInvariant)?;
    at = next;
    let (reason, end) =
        get_str(payload, at).ok_or(RelationAssertPayloadError::InternalInvariant)?;
    if end != payload.len() {
        return Err(RelationAssertPayloadError::InternalInvariant);
    }
    if !check_author(declared_author) {
        return Err(RelationAssertPayloadError::InvalidAuthor);
    }
    if !check_text(reason, typed::MAX_TEXT_BYTES) {
        return Err(RelationAssertPayloadError::InvalidReason);
    }
    Ok(DecodedSubmit {
        from,
        target,
        relation,
        confidence,
        declaration_order,
        declared_author,
        reason,
    })
}

/// Decode a `ReviewRelationAssert` v1 payload.
///
/// # Errors
///
/// [`RelationAssertPayloadError`] when the bytes are not exactly this payload.
pub fn decode_review<'a>(
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedReview<'a>, RelationAssertPayloadError> {
    ledger.checkpoint()?;
    let proposal = get_key(payload, 0).ok_or(RelationAssertPayloadError::InternalInvariant)?;
    check_key(proposal, RecordKind::Proposal)?;
    let raw = payload
        .get(KEY_BYTES..KEY_BYTES + 2)
        .ok_or(RelationAssertPayloadError::InternalInvariant)?;
    let decision = u16::from_le_bytes([raw[0], raw[1]]);
    if decision != DECISION_APPROVE && decision != DECISION_REJECT {
        return Err(RelationAssertPayloadError::UnknownDecision(decision));
    }
    let (note, end) =
        get_str(payload, KEY_BYTES + 2).ok_or(RelationAssertPayloadError::InternalInvariant)?;
    if end != payload.len() {
        return Err(RelationAssertPayloadError::InternalInvariant);
    }
    if !check_text(note, typed::MAX_TEXT_BYTES) {
        return Err(RelationAssertPayloadError::InvalidNote);
    }
    Ok(DecodedReview {
        proposal,
        decision,
        note,
    })
}

// ---------------------------------------------------------------------------
// Command surface
// ---------------------------------------------------------------------------

/// A relation-assert value this operation cannot accept, discovered after reading.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RelationAssertInputError {
    /// The proposal id space is exhausted.
    ProposalIdExhausted,
    /// The next proposal id is already occupied.
    ProposalIdCollision(RecordKey),
    /// The assertion id space is exhausted.
    AssertionIdExhausted,
    /// The next assertion id is already occupied.
    AssertionIdCollision(RecordKey),
    /// An endpoint term does not exist on the current basis.
    MissingEndpoint(RecordKey),
    /// The required old support is absent, retracted, or at a different revision.
    StaleAssertion(RevPair),
    /// A newly submitted slug already has an established Term identity.
    TargetSlugAlreadyExists,
    /// The authored target slug has no active Term during review.
    TargetSlugUnresolved,
    /// More than one Term claims the authored slug.
    TargetSlugAmbiguous,
    /// An endpoint term is retired, so it cannot carry a new assertion.
    EndpointRetired(RecordKey),
    /// An endpoint's current revision is not the one the payload pinned.
    StaleEndpoint {
        /// The endpoint.
        key: RecordKey,
        /// The revision the payload pinned.
        expected: RecordRevision,
    },
    /// A payload this module generated or was given is not a legal relation-assert payload.
    Payload(RelationAssertPayloadError),
    /// The candidate graph violates a rule the canonical validator enforces.
    ///
    /// The proposal stays `Pending`: baselines did not move, so this is not a conflict, and no
    /// write occurred. The reviewer may reject it with a new request.
    InvalidCandidate {
        /// The violations the validator reported, in its own deterministic order.
        violations: Vec<Violation>,
    },
}

/// Why a relation-assert command did not produce a commit.
#[derive(Debug)]
pub enum RelationAssertError {
    /// A shared admission, resource or store verdict.
    Command(CommandError),
    /// A relation-assert caller-input verdict.
    Input(RelationAssertInputError),
    /// The canonical validator refused to finish (budget, stop or a real read refusal).
    ///
    /// Distinct from [`Self::Input`]: the rules were never evaluated to a verdict.
    Validation(QueryError),
}

impl From<CommandError> for RelationAssertError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

impl From<InputError> for RelationAssertError {
    fn from(error: InputError) -> Self {
        Self::Command(CommandError::Input(error))
    }
}

impl From<AuthError> for RelationAssertError {
    fn from(error: AuthError) -> Self {
        Self::Command(error.into())
    }
}

impl From<Refusal> for RelationAssertError {
    fn from(refusal: Refusal) -> Self {
        Self::Command(refusal.into())
    }
}

impl From<RelationAssertInputError> for RelationAssertError {
    fn from(error: RelationAssertInputError) -> Self {
        Self::Input(error)
    }
}

impl From<RelationAssertPayloadError> for RelationAssertError {
    fn from(error: RelationAssertPayloadError) -> Self {
        match error {
            RelationAssertPayloadError::Refused(refusal) => Self::Command(refusal.into()),
            other => Self::Input(RelationAssertInputError::Payload(other)),
        }
    }
}

/// A real commit result is returned as-is; only a definite refusal becomes an error.
fn into_outcome(outcome: HandleOutcome) -> Result<HandleOutcome, RelationAssertError> {
    match outcome {
        HandleOutcome::NotCommitted { reason } => Err(CommandError::Store(reason).into()),
        other => Ok(other),
    }
}

/// Assemble the header fields shared by both commands.
#[allow(clippy::too_many_arguments)]
fn header(
    store: &Store<impl FileSystem>,
    principal: &str,
    meta: &RequestMeta<'_>,
    digest: [u8; 32],
    version: u64,
    events: Vec<OwnedEvent>,
    blobs: Vec<BlobDeclaration>,
    rules: sage_db_types::prepared::RulesBinding,
    ledger: &mut dyn SharedLedger,
) -> Result<PreparedTransaction, RelationAssertError> {
    Ok(PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: meta.tx_id,
        kb_uuid: store.kb_uuid(),
        created_at: meta.created_at,
        principal_id: admitted_string(principal, ledger)?,
        request_id: admitted_string(meta.request_id, ledger)?,
        payload_digest: digest,
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules,
    })
}

/// Submit a new authored assertion as a pending proposal.
///
/// `read_set` must be exactly the two endpoint baselines, ascending by key and unique — not a
/// whole-KB version pin. The caller's order is preserved into the proposal and into the frame;
/// nothing is normalised, sorted or deduplicated here.
///
/// # Errors
///
/// [`RelationAssertError`]: authorization (exactly `propose`), caller input, incomplete
/// authority, an idempotency mismatch, a fenced handle, a missing or retired endpoint, a stale
/// endpoint baseline, exhausted or occupied ids, or a real ledger/store refusal.
#[allow(clippy::too_many_arguments)]
pub fn submit_relation_assert<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    request: RelationAssert<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, RelationAssertError> {
    submit_relation_assert_inner(
        store,
        grants,
        meta,
        request.with_target(SubmitTarget::Key(request.target)),
        None,
        false,
        read_set,
        limits,
        ledger,
    )
}

/// Propose an atomic replacement of the Assertion pinned by the third read-set row.
///
/// The first two rows pin the new endpoints in key order. Approval uses
/// [`review_relation_assert`] and the exact replacement stored in the proposal.
///
/// # Errors
/// As [`submit_relation_assert`], plus a missing, retracted, or stale Assertion baseline.
pub fn submit_relation_replace<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    request: RelationAssert<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, RelationAssertError> {
    submit_relation_assert_inner(
        store,
        grants,
        meta,
        request.with_target(SubmitTarget::Key(request.target)),
        None,
        true,
        read_set,
        limits,
        ledger,
    )
}

/// Submit a legacy edge filename as part of the same canonical proposal commit.
///
/// # Errors
/// As [submit_relation_assert], plus an invalid filename or a moved comparison view.
#[allow(clippy::too_many_arguments)]
pub fn submit_legacy_relation_assert<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    request: RelationAssert<'_>,
    legacy: LegacyProposal<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, RelationAssertError> {
    submit_relation_assert_inner(
        store,
        grants,
        meta,
        request.with_target(SubmitTarget::Key(request.target)),
        Some(legacy),
        false,
        read_set,
        limits,
        ledger,
    )
}

/// Submit an authored legacy relation while the target slug has no Term identity.
///
/// # Errors
/// Refuses a stale publication or source baseline, a malformed slug or filename,
/// a slug already held by any Term, authorization failure or a resource refusal.
#[allow(clippy::too_many_arguments)]
pub fn submit_legacy_relation_by_slug<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    request: RelationAssert<'_, &str>,
    legacy: LegacyProposal<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, RelationAssertError> {
    submit_relation_assert_inner(
        store,
        grants,
        meta,
        request.with_target(SubmitTarget::Slug(request.target)),
        Some(legacy),
        false,
        read_set,
        limits,
        ledger,
    )
}

#[allow(clippy::too_many_arguments)]
fn submit_relation_assert_inner<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    request: RelationAssert<'_, SubmitTarget<'_>>,
    legacy: Option<LegacyProposal<'_>>,
    replace: bool,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, RelationAssertError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::Propose, ledger)?;
    check_meta(&meta, authorized.principal())?;

    let (endpoints, replaced) = split_read_set(read_set, replace)?;
    let (operation_version, payload) = match (request.target, legacy) {
        (SubmitTarget::Key(key), legacy) => {
            check_endpoint_read_set(endpoints, request.from, key)?;
            let payload = encode_submit(&request.with_target(key), ledger)?;
            match legacy {
                Some(options) => (
                    legacy_proposal::SUBMIT_RELATION_VERSION,
                    legacy_proposal::encode(options.file, &payload, ledger)?,
                ),
                None if replace => (REPLACEMENT_SUBMIT_VERSION, payload),
                None => (OPERATION_VERSION, payload),
            }
        }
        (SubmitTarget::Slug(slug), Some(options)) => {
            check_source_read_set(read_set, request.from)?;
            let payload = encode_pending_submit(&request.with_target(slug), ledger)?;
            (
                legacy_proposal::SUBMIT_RELATION_PENDING_VERSION,
                legacy_proposal::encode(options.file, &payload, ledger)?,
            )
        }
        (SubmitTarget::Slug(_), None) => return Err(InputError::LegacyProposalMetadata.into()),
    };
    // Hash the exact request before looking for a durable retry.
    let expect_version = legacy.map(|value| value.observed_version);
    let (frame_bytes, digest) = frame_and_digest_with_version(
        SUBMIT_RELATION_ASSERT,
        operation_version,
        &payload,
        read_set,
        expect_version,
        ledger,
    )?;
    if let Some(retry) = retry_outcome(store, authorized.principal(), meta.request_id, digest)? {
        return Ok(retry);
    }
    if store.is_fenced() {
        return Err(CommandError::Fenced.into());
    }
    ensure_import_ready(store, ledger)?;

    if let Some(expected) = expect_version {
        legacy_proposal::check_publication(store, expected)?;
    }

    let basis = store
        .commit_basis(IdSpace::ProposalId, ledger)
        .map_err(CommandError::from)?;

    // Every named identity must be active and pinned. A slug-only target has no
    // revision baseline and is independently checked on the same publication.
    let (mut from_slug, mut target_slug) = (None, None);
    for row in endpoints {
        let found = folded_current(store, row.key, ledger)?
            .ok_or(RelationAssertInputError::MissingEndpoint(row.key))?;
        let FoldedRecord::Term(term) = found else {
            return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
        };
        if term.retired {
            return Err(RelationAssertInputError::EndpointRetired(row.key).into());
        }
        if term.revision != row.revision {
            return Err(RelationAssertInputError::StaleEndpoint {
                key: row.key,
                expected: row.revision,
            }
            .into());
        }
        if row.key == request.from {
            from_slug = Some(term.slug.as_str());
        } else {
            target_slug = Some(term.slug.as_str());
        }
    }

    if let SubmitTarget::Slug(slug) = request.target {
        if lookup_legacy_target(store, slug, basis.basis, ledger)?.is_some() {
            return Err(RelationAssertInputError::TargetSlugAlreadyExists.into());
        }
        target_slug = Some(slug);
    }

    if let Some(options) = legacy {
        let relation = u8::try_from(request.relation)
            .ok()
            .and_then(RelationId::from_u8)
            .ok_or(RelationAssertPayloadError::UnknownRelation(
                request.relation,
            ))?;
        legacy_proposal::check_edge_name(
            options.file,
            from_slug.ok_or(CommandError::Internal(
                InternalDefect::EncodeDecodeDivergence,
            ))?,
            relation.as_str(),
            target_slug.ok_or(CommandError::Internal(
                InternalDefect::EncodeDecodeDivergence,
            ))?,
        )?;
    }

    if let Some(old) = replaced
        && active_assertion(store, old, ledger)?.is_none()
    {
        return Err(RelationAssertInputError::StaleAssertion(old).into());
    }

    let proposal_id = basis
        .allocated_high_water
        .checked_add(1)
        .ok_or(RelationAssertInputError::ProposalIdExhausted)?;
    let proposal_key = RecordKey::new(RecordKind::Proposal, proposal_id);
    if folded_current(store, proposal_key, ledger)?.is_some() {
        return Err(RelationAssertInputError::ProposalIdCollision(proposal_key).into());
    }

    let payload_ref = map_format(typed::raw_hash(&frame_bytes, ledger))?;
    let version = version_next(store.head())?;

    // Preserve the caller's read set verbatim: it is the proposal's baseline.
    let mut base_revisions: Vec<RevPair> = admitted_vec(read_set.len(), ledger)?;
    base_revisions.extend_from_slice(read_set);

    let proposal = ProposalRecord {
        key: proposal_key,
        revision: RecordRevision::FIRST,
        status: ProposalStatus::Pending,
        kind: ProposalKind::RelationAssert,
        // The subject is the authored `from`, never the target.
        subject: request.from,
        base_revisions,
        payload_ref,
        review_note: None,
        declared_author: admitted_string(request.declared_author, ledger)?,
        common: CommonFields {
            recorded_by: Some(admitted_string(authorized.principal(), ledger)?),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some(admitted_string(request.reason, ledger)?),
            created_at: meta.created_at,
            updated_at: None,
        },
        legacy_id: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    let allocation = IdAllocationEvent {
        allocation: Watermark {
            space: IdSpace::ProposalId,
            hwm: proposal_id,
        },
        reason: None,
        extensions: Extensions::new(),
    };

    let affected = [RevPair {
        key: proposal_key,
        revision: RecordRevision::FIRST,
    }];
    let counters = [allocation.allocation];
    let result_bytes =
        typed::result_payload(version, &affected, &counters, ledger).map_err(CommandError::from)?;
    let result_ref = map_format(typed::raw_hash(&result_bytes, ledger))?;
    let outcome = outcome_event(
        version,
        authorized.principal(),
        meta.request_id,
        digest,
        result_ref,
        meta.created_at,
        ledger,
    )?;

    let mut events: Vec<OwnedEvent> = admitted_vec(3, ledger)?;
    events.push(map_format(typed::prepare_event(
        EventInput::Proposal {
            decided: false,
            record: &proposal,
        },
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::IdAllocation(&allocation),
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::Outcome(&outcome),
        ledger,
    ))?);

    let payload_len = extent(frame_bytes.len())?;
    let result_len = extent(result_bytes.len())?;
    let mut blobs: Vec<BlobDeclaration> = admitted_vec(3, ledger)?;
    blobs.push(BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: BlobSource::Existing,
    });
    blobs.push(BlobDeclaration {
        sha256: payload_ref,
        declared_len: payload_len,
        source: BlobSource::Provide(frame_bytes),
    });
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: result_len,
        source: BlobSource::Provide(result_bytes),
    });

    let mut prepared = header(
        store,
        authorized.principal(),
        &meta,
        digest,
        version,
        events,
        blobs,
        basis.rules,
        ledger,
    )?;
    prepared.expect_version = expect_version;
    into_outcome(store.commit(prepared, limits, ledger))
}

fn check_source_read_set(
    read_set: &[RevPair],
    source: RecordKey,
) -> Result<(), RelationAssertError> {
    let [only] = read_set else {
        return Err(InputError::ReadSetLength {
            expected: 1,
            found: read_set.len(),
        }
        .into());
    };
    if only.key != source {
        return Err(InputError::ReadSetKey {
            index: 0,
            expected: source,
            found: only.key,
        }
        .into());
    }
    Ok(())
}

fn lookup_legacy_target<'a>(
    store: &'a Store<impl FileSystem>,
    slug: &str,
    expected: ViewBasis,
    ledger: &mut dyn SharedLedger,
) -> Result<Option<&'a sage_db_types::persist::TermRecord>, RelationAssertError> {
    let resolved = catalog::resolve_term_slug_in_store(store, slug, ledger).map_err(
        |error| -> RelationAssertError {
            match error {
                CatalogError::Command(command) => command.into(),
                CatalogError::Visitor(SlugScanError::Refused(refusal)) => refusal.into(),
                CatalogError::AmbiguousSlug { .. } => {
                    RelationAssertInputError::TargetSlugAmbiguous.into()
                }
                CatalogError::Visitor(SlugScanError::ClaimantOverflow)
                | CatalogError::Overflow { .. } => {
                    CommandError::Internal(InternalDefect::ExtentOverflow).into()
                }
                CatalogError::Internal { .. } => {
                    CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into()
                }
            }
        },
    )?;
    let (term, actual) = match resolved {
        SlugResolution::Found { term, basis } => (Some(term), basis),
        SlugResolution::Absent { basis } => (None, basis),
    };
    if actual != expected {
        return Err(CommandError::BasisDivergence.into());
    }
    Ok(term)
}

/// Reject a read set that is not exactly the two endpoints, ascending and unique.
fn check_endpoint_read_set(
    read_set: &[RevPair],
    from: RecordKey,
    target: RecordKey,
) -> Result<(), RelationAssertError> {
    if read_set.len() != 2 {
        return Err(CommandError::Input(InputError::ReadSetLength {
            expected: 2,
            found: read_set.len(),
        })
        .into());
    }
    // Ascending and unique, computed from the two keys rather than from the caller's order.
    let (first, second) = if from < target {
        (from, target)
    } else {
        (target, from)
    };
    for (index, expected) in [(0usize, first), (1, second)] {
        let row = &read_set[index];
        if row.key != expected {
            return Err(CommandError::Input(InputError::ReadSetKey {
                index,
                expected,
                found: row.key,
            })
            .into());
        }
        if row.revision.get() == 0 {
            return Err(CommandError::Input(InputError::ZeroRevision { index }).into());
        }
    }
    Ok(())
}

fn split_read_set(
    read_set: &[RevPair],
    replace: bool,
) -> Result<(&[RevPair], Option<RevPair>), RelationAssertError> {
    if !replace {
        return Ok((read_set, None));
    }
    let [_, _, old] = read_set else {
        return Err(InputError::ReadSetLength {
            expected: 3,
            found: read_set.len(),
        }
        .into());
    };
    check_key(old.key, RecordKind::Assertion)?;
    if old.revision.get() == 0 {
        return Err(InputError::ZeroRevision { index: 2 }.into());
    }
    Ok((&read_set[..2], Some(*old)))
}

fn active_assertion<'a, F: FileSystem>(
    store: &'a Store<F>,
    baseline: RevPair,
    ledger: &mut dyn LeaseLedger,
) -> Result<Option<&'a AssertionRecordFull>, RelationAssertError> {
    match folded_current(store, baseline.key, ledger)? {
        Some(FoldedRecord::Assertion(row))
            if !row.retracted && row.revision == baseline.revision =>
        {
            Ok(Some(row))
        }
        _ => Ok(None),
    }
}

/// The stored proposal as review needs it, with its ONE leased payload read.
struct StoredProposal {
    record: ProposalRecord,
    payload_ref: [u8; 32],
    payload: CommittedBlob,
}

impl StoredProposal {
    fn payload_len(&self) -> Result<u64, RelationAssertError> {
        extent(self.payload.len()).map_err(RelationAssertError::Command)
    }
}

/// Read a pending relation-assert proposal and its committed payload on `basis`.
fn read_pending_proposal<F: FileSystem>(
    store: &mut Store<F>,
    proposal: RecordKey,
    expected_revision: RecordRevision,
    basis: ViewBasis,
    ledger: &mut dyn LeaseLedger,
) -> Result<StoredProposal, RelationAssertError> {
    let (record, payload_ref) = {
        let found = folded_current(store, proposal, ledger)?
            .ok_or(CommandError::Input(InputError::MissingProposal(proposal)))?;
        let FoldedRecord::Proposal(stored) = found else {
            return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
        };
        if stored.revision != expected_revision {
            return Err(CommandError::Input(InputError::StaleBaseline {
                key: proposal,
                expected: expected_revision,
            })
            .into());
        }
        if stored.status != ProposalStatus::Pending {
            return Err(CommandError::Input(InputError::ProposalNotPending(stored.status)).into());
        }
        let copied = match map_format(typed::copy_payload(
            EventInput::Proposal {
                decided: stored.status.is_decided(),
                record: stored,
            },
            ledger,
        ))? {
            typed::DispatchedPayload::Proposal(owned) => owned,
            _ => {
                return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
            }
        };
        (copied, stored.payload_ref)
    };
    if record.kind != ProposalKind::RelationAssert {
        return Err(
            CommandError::Input(InputError::StoredMetadataMismatch(StoredField::Kind)).into(),
        );
    }
    let payload = store
        .read_committed_blob(payload_ref, typed::MAX_FRAME_BYTES, ledger)
        .map_err(CommandError::from)?;
    if payload.basis() != basis {
        return Err(CommandError::BasisDivergence.into());
    }
    Ok(StoredProposal {
        record,
        payload_ref,
        payload,
    })
}

/// Decode already hash-verified proposal bytes and check their recorded metadata.
///
/// The caller must bind the record and its content to the same publication.
///
/// # Errors
/// Refuses an unsupported operation, divergent metadata, or an exhausted caller ledger.
pub fn decode_stored_intent<'a>(
    record: &ProposalRecord,
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<DecodedSubmit<'a>, RelationAssertError> {
    if record.kind != ProposalKind::RelationAssert {
        return Err(
            CommandError::Input(InputError::StoredMetadataMismatch(StoredField::Kind)).into(),
        );
    }
    let decoded = match typed::decode_request(payload, ledger) {
        typed::RequestOutcome::Ok(decoded) => decoded,
        typed::RequestOutcome::Rejected(error) => {
            return Err(CommandError::Input(InputError::PersistedFrame(error)).into());
        }
        typed::RequestOutcome::Refused(refusal) => {
            return Err(CommandError::Refused(refusal).into());
        }
    };
    if let Some(pinned) = decoded.expected_version {
        return Err(CommandError::Input(InputError::StoredExpectedVersion { pinned }).into());
    }
    let operation = match decoded.ops.as_slice() {
        [operation] => operation,
        other => {
            return Err(CommandError::Input(InputError::StoredOperationCount {
                found: other.len(),
            })
            .into());
        }
    };
    if operation.kind != SUBMIT_RELATION_ASSERT
        || !matches!(
            operation.version,
            OPERATION_VERSION | REPLACEMENT_SUBMIT_VERSION
        )
    {
        return Err(CommandError::Input(InputError::UnsupportedStoredOperation {
            kind: operation.kind,
            version: operation.version,
        })
        .into());
    }
    let intent = decode_submit(operation.payload, ledger)?;

    // The stored payload must describe the proposal it is attached to.
    if intent.from != record.subject {
        return Err(
            CommandError::Input(InputError::StoredMetadataMismatch(StoredField::Subject)).into(),
        );
    }
    if intent.declared_author != record.declared_author {
        return Err(CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::DeclaredAuthor,
        ))
        .into());
    }
    // The frame and proposal must agree on exactly the two endpoints and, for v4,
    // the old assertion. An ordinary addition cannot acquire replacement semantics.
    let replace = operation.version == REPLACEMENT_SUBMIT_VERSION;
    let metadata_mismatch = |_| {
        CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::BaseRevisions,
        ))
    };
    let (endpoints, _) =
        split_read_set(&record.base_revisions, replace).map_err(metadata_mismatch)?;
    if endpoints.len() != 2 {
        return Err(InputError::StoredMetadataMismatch(StoredField::BaseRevisions).into());
    }
    if decoded.read_set != record.base_revisions {
        return Err(InputError::StoredMetadataMismatch(StoredField::ReadSet).into());
    }
    check_endpoint_read_set(endpoints, intent.from, intent.target).map_err(metadata_mismatch)?;
    Ok(intent)
}

/// Move one proposal to a decided revision, preserving every other field verbatim.
fn decide_proposal(
    record: &mut ProposalRecord,
    status: ProposalStatus,
    note: &str,
    reviewer: &str,
    created_at: i64,
    ledger: &mut dyn SharedLedger,
) -> Result<RecordRevision, RelationAssertError> {
    record.revision = revision_next(record.revision)?;
    record.status = status;
    record.review_note = Some(admitted_string(note, ledger)?);
    record.common.reason = Some(admitted_string(note, ledger)?);
    record.common.recorded_by = Some(admitted_string(reviewer, ledger)?);
    record.common.actor_origin = ActorOrigin::Authenticated;
    record.common.updated_at = Some(created_at);
    Ok(record.revision)
}

/// Commit one decided proposal revision with its result payload.
#[allow(clippy::too_many_arguments)]
fn finish_decision<F: FileSystem>(
    store: &mut Store<F>,
    principal: &str,
    meta: &RequestMeta<'_>,
    digest: [u8; 32],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
    decided: &ProposalRecord,
    decided_revision: RecordRevision,
    rules: sage_db_types::prepared::RulesBinding,
    rules_len: u64,
    payload: Option<([u8; 32], u64)>,
) -> Result<HandleOutcome, RelationAssertError> {
    let version = version_next(store.head())?;
    let affected = [RevPair {
        key: decided.key,
        revision: decided_revision,
    }];
    let result_bytes =
        typed::result_payload(version, &affected, &[], ledger).map_err(CommandError::from)?;
    let result_ref = map_format(typed::raw_hash(&result_bytes, ledger))?;
    let outcome = outcome_event(
        version,
        principal,
        meta.request_id,
        digest,
        result_ref,
        meta.created_at,
        ledger,
    )?;

    let mut events: Vec<OwnedEvent> = admitted_vec(2, ledger)?;
    events.push(map_format(typed::prepare_event(
        EventInput::Proposal {
            decided: true,
            record: decided,
        },
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::Outcome(&outcome),
        ledger,
    ))?);

    let mut blobs: Vec<BlobDeclaration> = admitted_vec(3, ledger)?;
    blobs.push(BlobDeclaration {
        sha256: rules.rules_ref,
        declared_len: rules_len,
        source: BlobSource::Existing,
    });
    if let Some((payload_ref, payload_len)) = payload {
        blobs.push(BlobDeclaration {
            sha256: payload_ref,
            declared_len: payload_len,
            source: BlobSource::Existing,
        });
    }
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: extent(result_bytes.len())?,
        source: BlobSource::Provide(result_bytes),
    });

    let prepared = header(
        store, principal, meta, digest, version, events, blobs, rules, ledger,
    )?;
    into_outcome(store.commit(prepared, limits, ledger))
}

/// Approve or reject a pending relation-assert proposal.
///
/// # Approval
///
/// The candidate assertion is derived only from the **stored** payload. The complete current
/// active graph plus that assertion is then validated by the canonical
/// [`sage_db_infer::validate_candidate`] under the committed rule profile; a rule violation returns
/// [`RelationAssertInputError::InvalidCandidate`] and writes nothing, leaving the proposal
/// `Pending`.
///
/// # Stale endpoints
///
/// If either endpoint's committed revision moved, the decision commits `Conflicted` with no
/// assertion: baselines genuinely changed. An unrelated commit does not do this.
///
/// # Errors
///
/// [`RelationAssertError`]: authorization (exactly `review`), caller input, incomplete authority,
/// an idempotency mismatch, a fenced handle, a terminal or absent proposal, an unregistered or
/// malformed stored payload, a stored-metadata contradiction, a retired endpoint, exhausted or
/// occupied assertion ids, a rule or resource refusal, or a real ledger/store refusal.
#[allow(clippy::too_many_arguments)]
pub fn review_relation_assert<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    proposal: RecordKey,
    expected_proposal_revision: RecordRevision,
    decision: u16,
    note: &str,
    validation_budget: Budget,
    validation_control: &dyn QueryControl,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, RelationAssertError> {
    review_relation_inner(
        store,
        grants,
        meta,
        proposal,
        expected_proposal_revision,
        decision,
        note,
        None,
        validation_budget,
        validation_control,
        limits,
        ledger,
    )
}

/// Atomically merge a submitted compatibility edge after checking the effective filename.
///
/// Existing identical subject/relation/target triples are approved without another Assertion.
/// Otherwise this validates and appends one assertion under the committed rule profile.
///
/// # Errors
/// Refuses a stale alias, missing endpoints, invalid graph, authorization or resource failures.
#[allow(clippy::too_many_arguments)]
pub fn merge_legacy_relation_assert<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    proposal: RevPair,
    review: LegacyReview<'_>,
    note: &str,
    budget: Budget,
    control: &dyn QueryControl,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, RelationAssertError> {
    review_relation_inner(
        store,
        grants,
        meta,
        proposal.key,
        proposal.revision,
        DECISION_APPROVE,
        note,
        Some(review),
        budget,
        control,
        limits,
        ledger,
    )
}

#[allow(clippy::too_many_arguments)]
fn review_relation_inner<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    proposal: RecordKey,
    expected_proposal_revision: RecordRevision,
    decision: u16,
    note: &str,
    legacy: Option<LegacyReview<'_>>,
    validation_budget: Budget,
    validation_control: &dyn QueryControl,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, RelationAssertError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::Review, ledger)?;
    check_meta(&meta, authorized.principal())?;
    if proposal.kind() != RecordKind::Proposal {
        return Err(CommandError::Input(InputError::WrongKeyKind {
            expected: RecordKind::Proposal,
            found: proposal.kind(),
        })
        .into());
    }
    let read_set = [RevPair {
        key: proposal,
        revision: expected_proposal_revision,
    }];

    let payload = encode_review(proposal, decision, note, ledger)?;
    let (op_version, payload) = if let Some(review) = legacy {
        if review.directory != LegacyDirectory::Edges {
            return Err(CommandError::from(InputError::LegacyProposalMetadata).into());
        }
        (
            legacy_proposal::REVIEW_VERSION,
            legacy_proposal::encode_review(review, None, &payload, ledger)?,
        )
    } else {
        (OPERATION_VERSION, payload)
    };
    let expect_version = legacy.map(|review| review.observed_version);
    let (_frame_bytes, digest) = frame_and_digest_with_version(
        REVIEW_RELATION_ASSERT,
        op_version,
        &payload,
        &read_set,
        expect_version,
        ledger,
    )?;
    if let Some(retry) = retry_outcome(store, authorized.principal(), meta.request_id, digest)? {
        return Ok(retry);
    }
    if store.is_fenced() {
        return Err(CommandError::Fenced.into());
    }
    ensure_import_ready(store, ledger)?;

    if let Some(review) = legacy {
        legacy_proposal::require_current_alias(store, review, read_set[0], ledger)?;
    }

    let basis = store
        .commit_basis(IdSpace::AssertionId, ledger)
        .map_err(CommandError::from)?;
    let stored = read_pending_proposal(
        store,
        proposal,
        expected_proposal_revision,
        basis.basis,
        ledger,
    )?;
    let payload_facts = (stored.payload_ref, stored.payload_len()?);

    // A rejection needs no candidate: it only closes the proposal.
    if decision != DECISION_APPROVE {
        let mut next = stored.record;
        let proposal_next = decide_proposal(
            &mut next,
            ProposalStatus::Rejected,
            note,
            authorized.principal(),
            meta.created_at,
            ledger,
        )?;
        return finish_decision(
            store,
            authorized.principal(),
            &meta,
            digest,
            limits,
            ledger,
            &next,
            proposal_next,
            basis.rules,
            basis.rules_len,
            Some(payload_facts),
        );
    }

    // The compatibility variant resolves the latest alias on the reviewer's view.
    // Native review still requires both of its submitted endpoint revisions.
    let (mut intent, conflicted, replaced) = if legacy.is_some() {
        let Some(legacy) =
            legacy_proposal::decode_stored(&stored.record, stored.payload.as_bytes(), ledger)?
        else {
            return Err(CommandError::from(InputError::LegacyProposalMetadata).into());
        };
        let intent = match legacy.intent {
            LegacyIntent::Edge(intent) => intent,
            LegacyIntent::PendingEdge(intent) => {
                let term = lookup_legacy_target(store, intent.target_slug, basis.basis, ledger)?
                    .ok_or(RelationAssertInputError::TargetSlugUnresolved)?;
                if term.retired {
                    return Err(RelationAssertInputError::EndpointRetired(term.key).into());
                }
                if term.key == intent.from {
                    return Err(RelationAssertPayloadError::SelfRelation.into());
                }
                DecodedSubmit {
                    from: intent.from,
                    target: term.key,
                    relation: intent.relation,
                    confidence: intent.confidence,
                    declaration_order: intent.declaration_order,
                    declared_author: intent.declared_author,
                    reason: intent.reason,
                }
            }
            LegacyIntent::Text(_) => return Err(InputError::LegacyProposalMetadata.into()),
        };
        (intent, false, None)
    } else {
        let intent = decode_stored_intent(&stored.record, stored.payload.as_bytes(), ledger)?;
        let mut conflicted = false;
        let mut replaced = None;
        for row in &stored.record.base_revisions {
            let matches = if row.key.kind() == RecordKind::Assertion {
                replaced = active_assertion(store, *row, ledger)?;
                replaced.is_some()
            } else {
                let current = folded_current(store, row.key, ledger)?;
                matches!(current, Some(FoldedRecord::Term(term))
                    if !term.retired && term.revision == row.revision)
            };
            conflicted |= !matches;
        }
        (intent, conflicted, replaced)
    };
    if conflicted {
        let mut next = stored.record;
        let proposal_next = decide_proposal(
            &mut next,
            ProposalStatus::Conflicted,
            note,
            authorized.principal(),
            meta.created_at,
            ledger,
        )?;
        return finish_decision(
            store,
            authorized.principal(),
            &meta,
            digest,
            limits,
            ledger,
            &next,
            proposal_next,
            basis.rules,
            basis.rules_len,
            Some(payload_facts),
        );
    }

    if legacy.is_some() {
        if let Some(order) = legacy_append_order(store, &intent, basis.basis, ledger)? {
            intent.declaration_order = order;
        } else {
            let mut next = stored.record;
            let revision = decide_proposal(
                &mut next,
                ProposalStatus::Approved,
                note,
                authorized.principal(),
                meta.created_at,
                ledger,
            )?;
            return finish_decision(
                store,
                authorized.principal(),
                &meta,
                digest,
                limits,
                ledger,
                &next,
                revision,
                basis.rules,
                basis.rules_len,
                Some(payload_facts),
            );
        }
    }

    // Baselines are current. Allocate a fresh assertion id from the committed mark and refuse
    // exhaustion or a collision rather than wrapping, reusing or searching.
    let assertion_id = basis
        .allocated_high_water
        .checked_add(1)
        .ok_or(RelationAssertInputError::AssertionIdExhausted)?;
    let assertion_key = RecordKey::new(RecordKind::Assertion, assertion_id);
    if folded_current(store, assertion_key, ledger)?.is_some() {
        return Err(RelationAssertInputError::AssertionIdCollision(assertion_key).into());
    }

    // The proposed after-image. Its audit names the REVIEWER (the trusted executor that
    // actually published it), while `declared_author` stays pure attribution.
    let proposed = AssertionRecordFull {
        key: assertion_key,
        revision: RecordRevision::FIRST,
        retracted: false,
        from: intent.from,
        relation: intent.relation,
        target: intent.target,
        confidence: intent.confidence,
        declaration_order: intent.declaration_order,
        common: CommonFields {
            recorded_by: Some(admitted_string(authorized.principal(), ledger)?),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some(admitted_string(note, ledger)?),
            created_at: meta.created_at,
            updated_at: None,
        },
        legacy_id: None,
        legacy_content_digest: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };

    let addition = sage_db_types::record::AssertionRecord {
        id: AssertionId::new(assertion_id),
        revision: RecordRevision::FIRST,
        from: candidate::term_id(intent.from)?,
        target: candidate::term_id(intent.target)?,
        relation: intent.relation,
        confidence: intent.confidence,
    };
    let candidate = match replaced {
        Some(old) => Addition::ReplaceAssertion {
            retract: AssertionId::new(old.key.id()),
            add: addition,
        },
        None => Addition::Assertion(addition),
    };
    candidate::validate(
        store,
        &basis,
        candidate,
        validation_budget,
        validation_control,
        ledger,
    )
    .map_err(|error| match error {
        CandidateError::Command(error) => RelationAssertError::Command(error),
        CandidateError::Validation(error) => RelationAssertError::Validation(error),
        CandidateError::Invalid(violations) => {
            RelationAssertInputError::InvalidCandidate { violations }.into()
        }
    })?;

    let allocation = IdAllocationEvent {
        allocation: Watermark {
            space: IdSpace::AssertionId,
            hwm: assertion_id,
        },
        reason: None,
        extensions: Extensions::new(),
    };

    let retracted = replaced
        .map(|old| retract_assertion(old, authorized.principal(), note, meta.created_at, ledger))
        .transpose()?;
    let mut next = stored.record;
    let proposal_next = decide_proposal(
        &mut next,
        ProposalStatus::Approved,
        note,
        authorized.principal(),
        meta.created_at,
        ledger,
    )?;

    let version = version_next(store.head())?;
    let added = RevPair {
        key: assertion_key,
        revision: RecordRevision::FIRST,
    };
    let decided = RevPair {
        key: next.key,
        revision: proposal_next,
    };
    let affected = [added, decided];
    let replaced_affected = retracted.as_ref().map(|old| {
        let removed = RevPair {
            key: old.key,
            revision: old.revision,
        };
        if removed.key < added.key {
            [removed, added, decided]
        } else {
            [added, removed, decided]
        }
    });
    let affected = replaced_affected
        .as_ref()
        .map_or(affected.as_slice(), |rows| rows.as_slice());
    let counters = [allocation.allocation];
    let result_bytes =
        typed::result_payload(version, affected, &counters, ledger).map_err(CommandError::from)?;
    let result_ref = map_format(typed::raw_hash(&result_bytes, ledger))?;
    let outcome = outcome_event(
        version,
        authorized.principal(),
        meta.request_id,
        digest,
        result_ref,
        meta.created_at,
        ledger,
    )?;

    let mut events: Vec<OwnedEvent> = admitted_vec(4 + usize::from(retracted.is_some()), ledger)?;
    if let Some(old) = &retracted {
        events.push(map_format(typed::prepare_event(
            EventInput::Assertion(typed::AssertionEvent::Retracted, old),
            ledger,
        ))?);
    }
    events.push(map_format(typed::prepare_event(
        EventInput::Assertion(sage_db_format_assertion_event(), &proposed),
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::Proposal {
            decided: true,
            record: &next,
        },
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::IdAllocation(&allocation),
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::Outcome(&outcome),
        ledger,
    ))?);

    let mut blobs: Vec<BlobDeclaration> = admitted_vec(3, ledger)?;
    blobs.push(BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: BlobSource::Existing,
    });
    blobs.push(BlobDeclaration {
        sha256: payload_facts.0,
        declared_len: payload_facts.1,
        source: BlobSource::Existing,
    });
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: extent(result_bytes.len())?,
        source: BlobSource::Provide(result_bytes),
    });

    let mut prepared = header(
        store,
        authorized.principal(),
        &meta,
        digest,
        version,
        events,
        blobs,
        basis.rules,
        ledger,
    )?;
    prepared.expect_version = expect_version;
    into_outcome(store.commit(prepared, limits, ledger))
}

fn retract_assertion(
    old: &AssertionRecordFull,
    reviewer: &str,
    note: &str,
    updated_at: i64,
    ledger: &mut dyn SharedLedger,
) -> Result<AssertionRecordFull, RelationAssertError> {
    let old_event = if old.revision == RecordRevision::FIRST {
        typed::AssertionEvent::Created
    } else {
        typed::AssertionEvent::Revised
    };
    let typed::DispatchedPayload::Assertion(mut row) = map_format(typed::copy_payload(
        EventInput::Assertion(old_event, old),
        ledger,
    ))?
    else {
        return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
    };
    row.revision = revision_next(row.revision)?;
    row.retracted = true;
    row.common.recorded_by = Some(admitted_string(reviewer, ledger)?);
    row.common.reason = Some(admitted_string(note, ledger)?);
    row.common.actor_origin = ActorOrigin::Authenticated;
    row.common.updated_at = Some(updated_at);
    row.changed_fields = None;
    Ok(row)
}

fn legacy_append_order(
    store: &Store<impl FileSystem>,
    intent: &DecodedSubmit<'_>,
    basis: ViewBasis,
    ledger: &mut dyn SharedLedger,
) -> Result<Option<u32>, RelationAssertError> {
    for key in [intent.from, intent.target] {
        match folded_current(store, key, ledger)? {
            Some(FoldedRecord::Term(term)) if !term.retired => {}
            Some(FoldedRecord::Term(_)) => {
                return Err(RelationAssertInputError::EndpointRetired(key).into());
            }
            _ => return Err(RelationAssertInputError::MissingEndpoint(key).into()),
        }
    }
    let mut highest = None;
    let mut exists = false;
    let observed = store
        .scan_identified_records(ledger, |row, ledger| {
            ledger.checkpoint().map_err(CommandError::from)?;
            if let FoldedRecord::Assertion(row) = row
                && !row.retracted
                && row.from == intent.from
            {
                exists |= row.relation == intent.relation && row.target == intent.target;
                highest = highest.max(Some(row.declaration_order));
            }
            Ok::<(), CommandError>(())
        })
        .map_err(|error| match error {
            sage_db_store::view::RecordScanError::Visitor(error) => error,
            sage_db_store::view::RecordScanError::Incomplete(error) => {
                CommandError::Incomplete(error)
            }
            sage_db_store::view::RecordScanError::Read(error) => error.into(),
        })?;
    if observed != basis {
        return Err(CommandError::BasisDivergence.into());
    }
    if exists {
        return Ok(None);
    }
    let position = match highest {
        Some(value) => value
            .checked_add(1)
            .ok_or(CommandError::Internal(InternalDefect::ExtentOverflow))?,
        None => 0,
    };
    Ok(Some(position))
}

/// The canonical `Created` assertion event.
fn sage_db_format_assertion_event() -> typed::AssertionEvent {
    typed::AssertionEvent::Created
}
