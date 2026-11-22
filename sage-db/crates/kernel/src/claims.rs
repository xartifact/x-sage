//! P2 claim-text edits: submit a pending proposal, then approve or reject it.
//!
//! # What this module is
//!
//! The first *writing* slice of P2. It composes, in this order and for both operations:
//!
//! 1. the grant check ([`Grants::authorize`]) for exactly one capability;
//! 2. caller-input validation against the canonical ceilings re-exported by
//!    [`sage_db_store::typed`] (identity, text, principal, request id);
//! 3. the canonical P1 request frame (one operation, the caller's read set, no expected
//!    version) and its `request`-domain digest;
//! 4. **idempotency first**: an exact retry of an already-committed request returns the
//!    original outcome before any pending, baseline or lifecycle check runs;
//! 5. the write fence, then the real `Store::commit_basis`;
//! 6. the authoritative reads the decision needs;
//! 7. one `PreparedTransaction` and the real `Store::commit`.
//!
//! # Why idempotency precedes every state check
//!
//! A retry arrives after the world moved: the proposal is decided, the claim was edited,
//! the baseline is stale. Every one of those is *true*, and every one of them would make
//! a state-first implementation answer "conflict" to a request that already committed.
//! The stored outcome is the answer, so it is looked up before anything is validated.
//!
//! # Ordering inside approval
//!
//! The proposal's committed payload is read exactly once, by the store's leased reader, and
//! the returned owner is held for the rest of the command: the frame is decoded from those
//! bytes, the declared `Existing` length is that owner's verified length, and it must have
//! been admitted on this attempt's own commit basis. Nothing re-reads or re-hashes it.
//!
//! Once the stored payload is decoded and matched against the proposal's own metadata, the
//! **complete two-object baseline** is compared before the current lifecycle is examined.
//! A claim that moved (or a term that was retired *and* revised) therefore finalises the
//! proposal as `Conflicted` — a committed record decision with no content change — instead
//! of escaping as an unpersisted "not applicable" error that would leave it pending
//! forever.
//!
//! Only when the baseline is current does the lifecycle matter: an inactive subject or a
//! non-`Accepted` claim is then a refusal for *this* candidate (nothing is written and the
//! proposal stays pending), because content that never moved cannot legitimately have
//! changed state. The replacement text is hashed and copied *after* both of those gates, so
//! a conflict or a refused subject never pays for a candidate that cannot be committed.
//!
//! # Resources
//!
//! The caller's ledger spans the whole command; this module never opens a second one and
//! never guesses a remaining allowance. Every owned value it must build (record copies,
//! strings, event and blob tables) is admitted through that ledger **before** it is
//! reserved, and nothing on the production path clones, collects or formats infallibly.
//! Record copies go through [`sage_db_store::typed::copy_payload`], which is the canonical
//! encode/decode roundtrip rather than an owning `.clone()` with an after-the-fact charge.
//! The committed payload blob is read through the store's leased owner and its bytes are
//! borrowed, never stripped into a bare `Vec`.
//!
//! # What is deliberately not claimed
//!
//! This is a text-only slice. No graph field changes, no full dependency derivation, no
//! candidate graph validation, no authentication of credentials (the [`Grants`] value is
//! produced by trusted auth code) and no durability claim of its own: durability is
//! whatever `Store::commit` reports, and `HandleOutcome::Unknown` is returned as the
//! ambiguous attempt it is.

use sage_db_store::commit::RecordedOutcome;
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleOutcome, HandleRefusal, Store};
use sage_db_store::lease::LeaseLedger;
use sage_db_store::open::{OutcomeIncomplete, OutcomeLookup};
use sage_db_store::typed::{
    self, DecodedRequest, EventInput, MAX_DECLARED_AUTHOR, MAX_FRAME_BYTES, MAX_PRINCIPAL_ID_BYTES,
    MAX_REQUEST_ID_BYTES, MAX_TEXT_BYTES, RequestError, RequestFrame, RequestOp, RequestOutcome,
    ResultPreparationError,
};
use sage_db_store::view::{
    BlobReadError, CommitReadError, CommittedBlob, RecordLookup, RecordReadError, ViewBasis,
};
use sage_db_types::framing::{FormatError, FormatOutcome, Limits, Refusal};
use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::{BudgetError, SharedLedger};
use sage_db_types::persist::{
    ActorOrigin, ClaimRecord, ClaimStatus, CommonFields, CreationAudit, Extensions,
    IdAllocationEvent, IdSpace, ProposalKind, ProposalRecord, ProposalStatus, RecordKey,
    RecordKind, RequestOutcomeEvent, RevPair, Watermark,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};

use crate::auth::{AuthError, Authorized, Capability, Grants};
use crate::intent::{
    self, ClaimEdit, PayloadError, REVIEW_CLAIM_EDIT, ReviewDecision, SUBMIT_CLAIM_EDIT,
};

/// Trusted per-request metadata supplied by the executor, never by the caller's payload.
///
/// `tx_id` and `created_at` are transaction-header facts and are excluded from the
/// canonical request digest, so an exact retry may carry different values without
/// becoming a different request. `request_id` **is** part of the idempotency key and of
/// the digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RequestMeta<'a> {
    /// Caller-chosen idempotency key component (`177`).
    pub request_id: &'a str,
    /// This transaction's 16-byte id, which must equal the `tid` in its file name.
    pub tx_id: [u8; 16],
    /// Commit time, UTC microseconds, supplied by the executor's clock.
    pub created_at: i64,
}

/// A caller-supplied value this operation cannot accept, before anything is written.
///
/// Distinct from [`CommandError::Refused`]: nothing here is a resource question, and
/// nothing here is a verdict about stored bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputError {
    /// The request id is the empty string.
    EmptyRequestId,
    /// The request id exceeds its canonical ceiling.
    RequestIdTooLong {
        /// Supplied byte length.
        len: usize,
        /// The canonical bound.
        max: u32,
    },
    /// The request id contains a NUL byte, which no canonical string may.
    NulInRequestId,
    /// The authenticated principal exceeds its canonical ceiling.
    PrincipalTooLong {
        /// Supplied byte length.
        len: usize,
        /// The canonical bound.
        max: u32,
    },
    /// The principal contains a NUL byte.
    NulInPrincipal,
    /// The declared author exceeds the identity ceiling.
    DeclaredAuthorTooLong {
        /// Supplied byte length.
        len: usize,
        /// The canonical bound.
        max: u32,
    },
    /// The declared author contains a NUL byte.
    NulInDeclaredAuthor,
    /// The submitted text is empty.
    EmptyText,
    /// The review note is empty.
    EmptyNote,
    /// The review note exceeds the text ceiling (it becomes a persisted `reason`).
    NoteTooLong {
        /// Supplied byte length.
        len: usize,
        /// The canonical bound.
        max: u32,
    },
    /// The review note contains a NUL byte.
    NulInNote,
    /// The read set does not have the exact size this operation requires.
    ReadSetLength {
        /// Keys the operation requires.
        expected: usize,
        /// Rows supplied.
        found: usize,
    },
    /// A read-set row names a different record than the operation is about.
    ReadSetKey {
        /// The position.
        index: usize,
        /// The key required there.
        expected: RecordKey,
        /// The key supplied.
        found: RecordKey,
    },
    /// A read-set revision is zero, which no baseline may name.
    ZeroRevision {
        /// The position.
        index: usize,
    },
    /// A key's id exceeds the bound its kind declares.
    KeyOutOfRange {
        /// The key.
        key: RecordKey,
    },
    /// A record occupies a key position of the wrong kind.
    WrongKeyKind {
        /// The kind the position requires.
        expected: RecordKind,
        /// The kind supplied.
        found: RecordKind,
    },
    /// The subject record does not exist on the current basis.
    MissingSubject(RecordKey),
    /// The claim to edit does not exist on the current basis.
    MissingClaim(RecordKey),
    /// The proposal does not exist on the current basis.
    MissingProposal(RecordKey),
    /// The subject term is retired and is never resurrected by a text edit.
    SubjectRetired(RecordKey),
    /// The claim is not `Accepted`; superseded and rejected claims stay closed.
    ClaimNotAccepted(ClaimStatus),
    /// The claim is about a different term than the payload names.
    SubjectMismatch {
        /// The term from the submitted payload.
        declared: RecordKey,
        /// The claim's current concept.
        actual: RecordKey,
    },
    /// A caller-supplied baseline revision is not the current one; refused before writing.
    StaleBaseline {
        /// The record.
        key: RecordKey,
        /// The caller's expected revision.
        expected: RecordRevision,
    },
    /// Malformed legacy proposal filename, family or comparison basis.
    LegacyProposalMetadata,
    /// The quoted legacy atom differs from the active subject's atom.
    LegacyProposalAtomMismatch,
    /// The current filename no longer resolves to the requested proposal revision.
    LegacyProposalAliasMoved,
    /// The current legacy body projection moved. Neither proposal nor Term was changed.
    LegacyProposalBodyMismatch {
        /// The submitted public revision hash.
        expected: [u8; 32],
        /// Current public revision hash on the checked publication.
        found: [u8; 32],
    },
    /// The proposal id space is exhausted.
    ProposalIdExhausted,
    /// The next proposal id is already occupied.
    ProposalIdCollision(RecordKey),
    /// The proposal has already been decided.
    ProposalNotPending(ProposalStatus),
    /// A payload this module generated was refused by its own codec.
    Payload(PayloadError),
    /// The stored request frame is not a legal P1 request.
    PersistedFrame(RequestError),
    /// The stored frame pins a whole-KB `expected_version`, which no claim-edit
    /// submission may carry: this schema supports only an absent expectation.
    StoredExpectedVersion {
        /// The pin the stored frame declared.
        pinned: u64,
    },
    /// The stored frame does not carry exactly one registered operation.
    StoredOperationCount {
        /// Operations present.
        found: usize,
    },
    /// The stored frame carries an operation kind/version this build does not implement.
    UnsupportedStoredOperation {
        /// The stored operation kind.
        kind: u16,
        /// The stored operation version.
        version: u16,
    },
    /// The stored payload is malformed, or not this operation's payload.
    PersistedPayload(PayloadError),
    /// The proposal's stored metadata contradicts its own payload.
    StoredMetadataMismatch(StoredField),
    /// The subject is current but no claim edit can apply to it.
    SubjectNotEditable,
}

/// Which stored field contradicted the payload it points at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoredField {
    /// The proposal's `kind`.
    Kind,
    /// The proposal's `subject`.
    Subject,
    /// The proposal's `declared_author`.
    DeclaredAuthor,
    /// The proposal's pending submission reason.
    Reason,
    /// The proposal's `base_revisions`.
    BaseRevisions,
    /// The stored frame's read set.
    ReadSet,
}

/// A generated value this attempt could not represent.
///
/// Not a caller verdict and not a resource refusal: these are arithmetic or
/// encode/decode limits of the writer itself, so they are named as such.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InternalDefect {
    /// The commit version cannot advance.
    VersionOverflow,
    /// A record revision cannot advance.
    RevisionExhausted,
    /// A retained extent is not representable.
    ExtentOverflow,
    /// A canonical encoder produced a payload its own dispatcher decoded as another family.
    EncodeDecodeDivergence,
    /// This module's own payload layout arithmetic disagreed with the bytes it emitted.
    ///
    /// Unreachable through the public encoders, which build a payload of a length they
    /// already admitted; it reports itself here rather than as caller input or as a
    /// resource refusal.
    PayloadLengthInvariant,
}

/// Why a claim-edit command did not produce a commit.
///
/// This is **not** the commit result: a successful path returns the real
/// [`HandleOutcome`], including `Unknown`, which is an ambiguous attempt and never a
/// fabricated error.
#[derive(Debug)]
pub enum CommandError {
    /// Authorization refused for the exact capability this operation requires.
    Auth(AuthError),
    /// The caller's input was unacceptable; nothing was read from or written to disk.
    Input(InputError),
    /// The outcome index or current view cannot answer definitively.
    Incomplete(OutcomeIncomplete),
    /// The request id is already recorded with different content.
    IdempotencyMismatch,
    /// Writes are fenced by an ambiguous earlier attempt; resolve before retrying.
    Fenced,
    /// Persisted administrative import is unfinished; resume/verify it before ordinary access.
    ImportInProgress,
    /// The real commit-basis read refused.
    Basis(CommitReadError),
    /// The committed payload read refused, or found confirmed damage.
    Blob(BlobReadError),
    /// The payload blob was admitted on a different publication than this attempt's basis,
    /// so it describes a world this transaction is not writing to.
    BasisDivergence,
    /// A current record read refused.
    Record(RecordReadError),
    /// This module's own request frame was refused by the P1 encoder.
    Request(RequestError),
    /// A generated result payload could not be prepared.
    ResultPreparation(ResultPreparationError),
    /// A canonical encoder classified a generated image.
    Format(FormatError),
    /// The caller's ledger refused or stopped the work.
    Refused(Refusal),
    /// The store refused the prepared transaction before any durable point.
    Store(HandleRefusal),
    /// A generated value is not representable by this writer.
    Internal(InternalDefect),
}

impl From<InputError> for CommandError {
    fn from(error: InputError) -> Self {
        Self::Input(error)
    }
}

impl From<sage_db_types::query::ControlStop> for CommandError {
    fn from(stop: sage_db_types::query::ControlStop) -> Self {
        Self::Refused(Refusal::Stopped(stop))
    }
}

impl From<AuthError> for CommandError {
    fn from(error: AuthError) -> Self {
        Self::Auth(error)
    }
}

impl From<Refusal> for CommandError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<BudgetError> for CommandError {
    fn from(error: BudgetError) -> Self {
        Self::Refused(Refusal::Budget(error))
    }
}

impl From<ResultPreparationError> for CommandError {
    fn from(error: ResultPreparationError) -> Self {
        Self::ResultPreparation(error)
    }
}

impl From<CommitReadError> for CommandError {
    fn from(error: CommitReadError) -> Self {
        Self::Basis(error)
    }
}

impl From<BlobReadError> for CommandError {
    fn from(error: BlobReadError) -> Self {
        Self::Blob(error)
    }
}

impl From<RecordReadError> for CommandError {
    fn from(error: RecordReadError) -> Self {
        Self::Record(error)
    }
}

impl From<PayloadError> for CommandError {
    fn from(error: PayloadError) -> Self {
        map_caller_payload(error)
    }
}

/// Map a *generated* payload's codec verdict.
///
/// A resource refusal stays canonical, this module's own layout invariant stays internal,
/// and everything else is what the caller wrote. Kept as one function so the submit path
/// and the stored-decode path cannot classify the same defect differently.
fn map_caller_payload(error: PayloadError) -> CommandError {
    match error {
        PayloadError::Refused(refusal) => CommandError::Refused(refusal),
        PayloadError::InternalInvariant => {
            CommandError::Internal(InternalDefect::PayloadLengthInvariant)
        }
        other => CommandError::Input(InputError::Payload(other)),
    }
}

/// Map a *stored* payload's codec verdict.
///
/// Same rule set, different attribution: a malformed stored payload is a fact about the
/// proposal, not about this reviewer's request, so it carries `PersistedPayload` instead.
fn map_stored_payload(error: PayloadError) -> CommandError {
    match error {
        PayloadError::Refused(refusal) => CommandError::Refused(refusal),
        PayloadError::InternalInvariant => {
            CommandError::Internal(InternalDefect::PayloadLengthInvariant)
        }
        other => CommandError::Input(InputError::PersistedPayload(other)),
    }
}

pub(crate) fn map_format<T>(outcome: FormatOutcome<T>) -> Result<T, CommandError> {
    match outcome {
        FormatOutcome::Ok(value) => Ok(value),
        FormatOutcome::Format(error) => Err(CommandError::Format(error)),
        FormatOutcome::Refused(refusal) => Err(CommandError::Refused(refusal)),
    }
}

pub(crate) fn map_request<T>(outcome: RequestOutcome<T>) -> Result<T, CommandError> {
    match outcome {
        RequestOutcome::Ok(value) => Ok(value),
        RequestOutcome::Rejected(error) => Err(CommandError::Request(error)),
        RequestOutcome::Refused(refusal) => Err(CommandError::Refused(refusal)),
    }
}

fn check_request_id(request_id: &str) -> Result<(), InputError> {
    if request_id.is_empty() {
        return Err(InputError::EmptyRequestId);
    }
    if request_id.as_bytes().contains(&0) {
        return Err(InputError::NulInRequestId);
    }
    if u32::try_from(request_id.len()).map_or(true, |len| len > MAX_REQUEST_ID_BYTES) {
        return Err(InputError::RequestIdTooLong {
            len: request_id.len(),
            max: MAX_REQUEST_ID_BYTES,
        });
    }
    Ok(())
}

fn check_principal(principal: &str) -> Result<(), InputError> {
    if principal.as_bytes().contains(&0) {
        return Err(InputError::NulInPrincipal);
    }
    if u32::try_from(principal.len()).map_or(true, |len| len > MAX_PRINCIPAL_ID_BYTES) {
        return Err(InputError::PrincipalTooLong {
            len: principal.len(),
            max: MAX_PRINCIPAL_ID_BYTES,
        });
    }
    Ok(())
}

pub(crate) fn check_meta(meta: &RequestMeta<'_>, principal: &str) -> Result<(), InputError> {
    check_request_id(meta.request_id)?;
    check_principal(principal)
}

/// Bound the declared author, which is copied verbatim afterwards.
///
/// Emptiness is deliberately not a refusal: attribution may legitimately be absent, and
/// this slice preserves it as written rather than inventing a requirement.
fn check_declared_author(author: &str) -> Result<(), InputError> {
    if author.as_bytes().contains(&0) {
        return Err(InputError::NulInDeclaredAuthor);
    }
    if u32::try_from(author.len()).map_or(true, |len| len > MAX_DECLARED_AUTHOR) {
        return Err(InputError::DeclaredAuthorTooLong {
            len: author.len(),
            max: MAX_DECLARED_AUTHOR,
        });
    }
    Ok(())
}

fn check_note(note: &str) -> Result<(), InputError> {
    if note.is_empty() {
        return Err(InputError::EmptyNote);
    }
    if note.as_bytes().contains(&0) {
        return Err(InputError::NulInNote);
    }
    if u32::try_from(note.len()).map_or(true, |len| len > MAX_TEXT_BYTES) {
        return Err(InputError::NoteTooLong {
            len: note.len(),
            max: MAX_TEXT_BYTES,
        });
    }
    Ok(())
}

fn check_key(key: RecordKey) -> Result<(), InputError> {
    if !key.id_is_in_range() {
        return Err(InputError::KeyOutOfRange { key });
    }
    Ok(())
}

/// Require the read set to be exactly `expected`, strictly ascending and unique.
///
/// `expected` is already in the order the canonical read set requires, so a caller who
/// happens to name the same two objects in the other order is refused rather than having
/// its request silently rewritten.
fn expect_read_set(read_set: &[RevPair], expected: &[RecordKey]) -> Result<(), InputError> {
    if read_set.len() != expected.len() {
        return Err(InputError::ReadSetLength {
            expected: expected.len(),
            found: read_set.len(),
        });
    }
    for (index, row) in read_set.iter().enumerate() {
        check_key(row.key)?;
        if row.revision.get() == 0 {
            return Err(InputError::ZeroRevision { index });
        }
        if row.key != expected[index] {
            return Err(InputError::ReadSetKey {
                index,
                expected: expected[index],
                found: row.key,
            });
        }
    }
    Ok(())
}

/// The two keys this slice's read set names, in canonical ascending order.
fn ordered_pair(first: RecordKey, second: RecordKey) -> [RecordKey; 2] {
    if first <= second {
        [first, second]
    } else {
        [second, first]
    }
}

/// Admit and build an owned copy of one string under the caller's ledger.
pub(crate) fn admitted_string(
    value: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<String, CommandError> {
    ledger.checkpoint()?;
    ledger.try_charge(value.len() as u64)?;
    ledger.checkpoint()?;
    let mut out = String::new();
    out.try_reserve_exact(value.len())
        .map_err(|_| Refusal::Allocation {
            requested: value.len(),
        })?;
    out.push_str(value);
    Ok(out)
}

/// Admit and reserve a table of `len` slots of `T`.
pub(crate) fn admitted_vec<T>(
    len: usize,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<T>, CommandError> {
    ledger.checkpoint()?;
    let bytes = len
        .checked_mul(core::mem::size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(CommandError::Internal(InternalDefect::ExtentOverflow))?;
    ledger.try_charge_entries(len as u64)?;
    ledger.try_charge(bytes)?;
    ledger.checkpoint()?;
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| Refusal::Allocation {
            requested: bytes as usize,
        })?;
    Ok(out)
}

/// Admit and copy bytes into a freshly reserved buffer.
pub(crate) fn admitted_bytes(
    value: &[u8],
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, CommandError> {
    // The slot allocation is the byte allocation for u8; admitted_vec charges
    // that extent once before reserving it. Do not charge a second copy.
    let mut out = admitted_vec::<u8>(value.len(), ledger)?;
    out.extend_from_slice(value);
    Ok(out)
}

pub(crate) fn extent(value: usize) -> Result<u64, CommandError> {
    u64::try_from(value).map_err(|_| CommandError::Internal(InternalDefect::ExtentOverflow))
}

/// Encode the one-operation frame and return its bytes with the request digest.
pub(crate) fn frame_and_digest(
    kind: u16,
    op_version: u16,
    payload: &[u8],
    read_set: &[RevPair],
    ledger: &mut dyn SharedLedger,
) -> Result<(Vec<u8>, [u8; 32]), CommandError> {
    frame_and_digest_with_version(kind, op_version, payload, read_set, None, ledger)
}

pub(crate) fn frame_and_digest_with_version(
    kind: u16,
    op_version: u16,
    payload: &[u8],
    read_set: &[RevPair],
    expect_version: Option<u64>,
    ledger: &mut dyn SharedLedger,
) -> Result<(Vec<u8>, [u8; 32]), CommandError> {
    let ops = [RequestOp::new(kind, op_version, payload)];
    let frame = RequestFrame::new(&ops, read_set, expect_version);
    let bytes = map_request(typed::encode_request(&frame, ledger))?;
    let digest = map_request(typed::request_digest_of(&bytes, ledger))?;
    Ok((bytes, digest))
}

/// Gate ordinary serving on the actual persisted import completion marker.
/// Administrative import and verification use the physical Store separately.
pub(crate) fn ensure_import_ready(
    store: &Store<impl FileSystem>,
    ledger: &mut dyn SharedLedger,
) -> Result<(), CommandError> {
    match store.import_readiness(ledger)? {
        sage_db_store::view::ImportReadiness::Ready { .. } => Ok(()),
        sage_db_store::view::ImportReadiness::Incomplete { .. } => {
            Err(CommandError::ImportInProgress)
        }
    }
}

/// The outcome an exact retry returns, or `None` when this request is novel.
pub(crate) fn retry_outcome(
    store: &Store<impl FileSystem>,
    principal: &str,
    request_id: &str,
    digest: [u8; 32],
) -> Result<Option<HandleOutcome>, CommandError> {
    match store.outcome(principal, request_id) {
        OutcomeLookup::Found {
            outcome,
            original_version,
            ..
        } => {
            if outcome.event().payload_digest != digest {
                return Err(CommandError::IdempotencyMismatch);
            }
            Ok(Some(HandleOutcome::AlreadyCommitted {
                version: original_version,
                recorded: RecordedOutcome::Outcome(outcome),
            }))
        }
        OutcomeLookup::Absent { .. } => Ok(None),
        OutcomeLookup::Incomplete(reason) => Err(CommandError::Incomplete(reason)),
    }
}

/// The current record at `key`, or the `None` of a confirmed absence.
pub(crate) fn folded_current<'a>(
    store: &'a Store<impl FileSystem>,
    key: RecordKey,
    ledger: &mut dyn SharedLedger,
) -> Result<Option<&'a FoldedRecord>, CommandError> {
    match store.record(key, ledger)? {
        RecordLookup::Found { record, .. } => Ok(Some(record)),
        RecordLookup::Absent { .. } => Ok(None),
        RecordLookup::Incomplete(reason) => Err(CommandError::Incomplete(reason)),
    }
}

/// The canonical event a stored claim after-image was written under.
fn claim_event(record: &ClaimRecord) -> typed::ClaimEvent {
    match (record.status, record.revision) {
        (ClaimStatus::Rejected, _) => typed::ClaimEvent::Rejected,
        (ClaimStatus::Superseded, _) => typed::ClaimEvent::Superseded,
        (ClaimStatus::Accepted, revision) if revision == RecordRevision::FIRST => {
            typed::ClaimEvent::Created
        }
        (ClaimStatus::Accepted, _) => typed::ClaimEvent::Revised,
    }
}

/// Copy a claim canonically rather than by an owning clone.
fn claim_copy(
    record: &ClaimRecord,
    ledger: &mut dyn SharedLedger,
) -> Result<ClaimRecord, CommandError> {
    match map_format(typed::copy_payload(
        EventInput::Claim(claim_event(record), record),
        ledger,
    ))? {
        typed::DispatchedPayload::Claim(owned) => Ok(owned),
        _ => Err(CommandError::Internal(
            InternalDefect::EncodeDecodeDivergence,
        )),
    }
}

/// Copy a proposal canonically rather than by an owning clone.
fn proposal_copy(
    record: &ProposalRecord,
    ledger: &mut dyn SharedLedger,
) -> Result<ProposalRecord, CommandError> {
    match map_format(typed::copy_payload(
        EventInput::Proposal {
            decided: record.status.is_decided(),
            record,
        },
        ledger,
    ))? {
        typed::DispatchedPayload::Proposal(owned) => Ok(owned),
        _ => Err(CommandError::Internal(
            InternalDefect::EncodeDecodeDivergence,
        )),
    }
}

pub(crate) fn version_next(head: Option<u64>) -> Result<u64, CommandError> {
    match head {
        None => Ok(1),
        Some(head) => head
            .checked_add(1)
            .ok_or(CommandError::Internal(InternalDefect::VersionOverflow)),
    }
}

pub(crate) fn revision_next(revision: RecordRevision) -> Result<RecordRevision, CommandError> {
    revision
        .checked_next()
        .map_err(|_| CommandError::Internal(InternalDefect::RevisionExhausted))
}

/// Build the outcome event that joins this transaction's header.
pub(crate) fn outcome_event(
    version: u64,
    principal: &str,
    request_id: &str,
    digest: [u8; 32],
    result_ref: [u8; 32],
    created_at: i64,
    ledger: &mut dyn SharedLedger,
) -> Result<RequestOutcomeEvent, CommandError> {
    Ok(RequestOutcomeEvent {
        principal_id: admitted_string(principal, ledger)?,
        request_id: admitted_string(request_id, ledger)?,
        payload_digest: digest,
        result_ref,
        commit_version: version,
        audit: CreationAudit {
            recorded_by: Some(admitted_string(principal, ledger)?),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at,
        },
        extensions: Extensions::new(),
    })
}

/// Move one proposal to a decided revision, preserving every other field verbatim.
fn decide_proposal(
    record: &mut ProposalRecord,
    status: ProposalStatus,
    note: &str,
    reviewer: &str,
    created_at: i64,
    ledger: &mut dyn SharedLedger,
) -> Result<RecordRevision, CommandError> {
    record.revision = revision_next(record.revision)?;
    record.status = status;
    record.review_note = Some(admitted_string(note, ledger)?);
    record.common.reason = Some(admitted_string(note, ledger)?);
    record.common.recorded_by = Some(admitted_string(reviewer, ledger)?);
    record.common.actor_origin = ActorOrigin::Authenticated;
    record.common.updated_at = Some(created_at);
    Ok(record.revision)
}

/// One attempt's identity and clock, as every command here needs it.
#[derive(Clone, Copy)]
struct Attempt<'a> {
    /// The authenticated principal performing the write.
    principal: &'a str,
    /// The request metadata supplied by the executor.
    meta: &'a RequestMeta<'a>,
    /// Canonical request digest, which the header and outcome must share.
    digest: [u8; 32],
}

/// Assemble the header fields shared by every command here.
fn header(
    store: &Store<impl FileSystem>,
    attempt: &Attempt<'_>,
    version: u64,
    events: Vec<OwnedEvent>,
    blobs: Vec<BlobDeclaration>,
    rules: sage_db_types::prepared::RulesBinding,
    ledger: &mut dyn SharedLedger,
) -> Result<PreparedTransaction, CommandError> {
    let principal = attempt.principal;
    let meta = attempt.meta;
    let digest = attempt.digest;
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

/// Submit a claim-text edit as a pending proposal.
///
/// The request frame itself becomes the proposal's payload blob: its bytes are persisted
/// verbatim, so the reviewer reads exactly the request the author made, and `payload_ref`
/// is the raw SHA-256 of those bytes.
///
/// # Errors
///
/// [`CommandError`]: authorization, caller input (including a stale submitted baseline,
/// which is refused before any authoritative write), incomplete authority, an
/// idempotency mismatch, a fenced handle, a real ledger/store refusal, or a canonical
/// encoder verdict on a generated image.
pub fn submit_claim_edit<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    edit: ClaimEdit<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, CommandError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::Propose, ledger)?;
    check_meta(&meta, authorized.principal())?;
    check_declared_author(edit.declared_author)?;
    check_key(edit.subject)?;
    check_key(edit.claim)?;
    if edit.subject.kind() != RecordKind::Term {
        return Err(CommandError::Input(InputError::WrongKeyKind {
            expected: RecordKind::Term,
            found: edit.subject.kind(),
        }));
    }
    if edit.claim.kind() != RecordKind::Claim {
        return Err(CommandError::Input(InputError::WrongKeyKind {
            expected: RecordKind::Claim,
            found: edit.claim.kind(),
        }));
    }
    expect_read_set(read_set, &ordered_pair(edit.subject, edit.claim))?;

    // The payload is prepared before the outcome lookup because its digest *is* the
    // idempotency key component: an exact retry must hash to the same bytes.
    let payload = intent::encode_submit_edit(&edit, ledger)?;
    let (frame_bytes, digest) = frame_and_digest(
        SUBMIT_CLAIM_EDIT,
        intent::OPERATION_VERSION,
        &payload,
        read_set,
        ledger,
    )?;
    if let Some(retry) = retry_outcome(store, authorized.principal(), meta.request_id, digest)? {
        return Ok(retry);
    }
    if store.is_fenced() {
        return Err(CommandError::Fenced);
    }
    ensure_import_ready(store, ledger)?;

    let basis = store.commit_basis(IdSpace::ProposalId, ledger)?;

    // The submitted baseline and the two lifecycle preconditions, all before any write.
    let (term_revision, claim_revision) = {
        let term = folded_current(store, edit.subject, ledger)?.ok_or(CommandError::Input(
            InputError::MissingSubject(edit.subject),
        ))?;
        let FoldedRecord::Term(term) = term else {
            return Err(CommandError::Internal(
                InternalDefect::EncodeDecodeDivergence,
            ));
        };
        let claim = folded_current(store, edit.claim, ledger)?
            .ok_or(CommandError::Input(InputError::MissingClaim(edit.claim)))?;
        let FoldedRecord::Claim(claim) = claim else {
            return Err(CommandError::Internal(
                InternalDefect::EncodeDecodeDivergence,
            ));
        };
        if term.retired {
            return Err(CommandError::Input(InputError::SubjectRetired(
                edit.subject,
            )));
        }
        if claim.status != ClaimStatus::Accepted {
            return Err(CommandError::Input(InputError::ClaimNotAccepted(
                claim.status,
            )));
        }
        if claim.concept != edit.subject {
            return Err(CommandError::Input(InputError::SubjectMismatch {
                declared: edit.subject,
                actual: claim.concept,
            }));
        }
        (term.revision, claim.revision)
    };
    let current_revision = |key: RecordKey| {
        if key == edit.subject {
            term_revision
        } else {
            claim_revision
        }
    };
    for row in read_set {
        if current_revision(row.key) != row.revision {
            return Err(CommandError::Input(InputError::StaleBaseline {
                key: row.key,
                expected: row.revision,
            }));
        }
    }

    // Allocate the next proposal id from the actual committed mark, then check the id is
    // genuinely free: the mark's absence means "nothing allocated", not "id 1 is free".
    let proposal_id = basis
        .allocated_high_water
        .checked_add(1)
        .ok_or(CommandError::Input(InputError::ProposalIdExhausted))?;
    let proposal_key = RecordKey::new(RecordKind::Proposal, proposal_id);
    if folded_current(store, proposal_key, ledger)?.is_some() {
        return Err(CommandError::Input(InputError::ProposalIdCollision(
            proposal_key,
        )));
    }

    let payload_ref = map_format(typed::raw_hash(&frame_bytes, ledger))?;
    let version = version_next(store.head())?;

    // The proposal after-image: revision 1, pending, the author's baseline verbatim.
    let mut base_revisions: Vec<RevPair> = admitted_vec(read_set.len(), ledger)?;
    for row in read_set {
        base_revisions.push(*row);
    }
    let proposal = ProposalRecord {
        key: proposal_key,
        revision: RecordRevision::FIRST,
        status: ProposalStatus::Pending,
        kind: ProposalKind::ClaimEdit,
        subject: edit.subject,
        base_revisions,
        payload_ref,
        review_note: None,
        declared_author: admitted_string(edit.declared_author, ledger)?,
        common: CommonFields {
            recorded_by: Some(admitted_string(authorized.principal(), ledger)?),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
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
    let result_bytes = typed::result_payload(version, &affected, &counters, ledger)?;
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

    let prepared = header(
        store,
        &Attempt {
            principal: authorized.principal(),
            meta: &meta,
            digest,
        },
        version,
        events,
        blobs,
        basis.rules,
        ledger,
    )?;
    into_outcome(store.commit(prepared, limits, ledger))
}

/// The stored proposal as this command needs it, already copied canonically.
///
/// It owns the ONE leased read of the proposal's committed payload. Nothing re-reads or
/// re-hashes that blob later: the frame is decoded from this owner's borrowed bytes, the
/// declared `Existing` length is this owner's verified length, and the owner is held
/// across the commit so those bytes stay charged until the transaction is done.
struct StoredProposal {
    record: ProposalRecord,
    payload_ref: [u8; 32],
    payload: CommittedBlob,
}

impl StoredProposal {
    /// The verified committed payload length.
    fn payload_len(&self) -> Result<u64, CommandError> {
        extent(self.payload.len())
    }
}

/// Read a pending proposal, its expected revision, and its committed payload.
///
/// The payload is opened through the store's leased reader, so its length and bytes are
/// the verified committed ones rather than a guess, and its basis must be the very
/// publication this command's commit basis came from: a payload admitted on another root
/// would describe a world this attempt is not writing to.
fn read_pending_proposal<F: FileSystem>(
    store: &mut Store<F>,
    proposal: RecordKey,
    expected_revision: RecordRevision,
    basis: ViewBasis,
    ledger: &mut dyn LeaseLedger,
) -> Result<StoredProposal, CommandError> {
    let (record, payload_ref) = {
        let found = folded_current(store, proposal, ledger)?
            .ok_or(CommandError::Input(InputError::MissingProposal(proposal)))?;
        let FoldedRecord::Proposal(record) = found else {
            return Err(CommandError::Internal(
                InternalDefect::EncodeDecodeDivergence,
            ));
        };
        if record.revision != expected_revision {
            return Err(CommandError::Input(InputError::StaleBaseline {
                key: proposal,
                expected: expected_revision,
            }));
        }
        if record.status != ProposalStatus::Pending {
            return Err(CommandError::Input(InputError::ProposalNotPending(
                record.status,
            )));
        }
        let copied = proposal_copy(record, ledger)?;
        (copied, record.payload_ref)
    };
    if record.kind != ProposalKind::ClaimEdit {
        return Err(CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::Kind,
        )));
    }
    let payload = store.read_committed_blob(payload_ref, MAX_FRAME_BYTES, ledger)?;
    if payload.basis() != basis {
        return Err(CommandError::BasisDivergence);
    }
    Ok(StoredProposal {
        record,
        payload_ref,
        payload,
    })
}

/// Approve or reject a pending claim-edit proposal.
///
/// Approval re-derives the candidate from the **stored** payload, never from the
/// reviewer's memory of it: the frame's operation kind, version, subject, claim,
/// declared author and read set must all agree with the proposal's own metadata, and the
/// stored text is the text that becomes the new claim body.
///
/// # Errors
///
/// [`CommandError`]: authorization, caller input, incomplete authority, an idempotency
/// mismatch, a fenced handle, a terminal or absent proposal, an unregistered or malformed
/// stored payload, a stored-metadata contradiction, a subject that is current but not
/// editable, or a real ledger/store refusal. A stale baseline is *not* an error: it is a
/// committed `Conflicted` decision, returned as a successful [`HandleOutcome`].
#[allow(clippy::too_many_arguments)]
pub fn review_claim_edit<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    proposal: RecordKey,
    expected_proposal_revision: RecordRevision,
    decision: ReviewDecision,
    note: &str,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, CommandError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::Review, ledger)?;
    check_meta(&meta, authorized.principal())?;
    check_note(note)?;
    check_key(proposal)?;
    if proposal.kind() != RecordKind::Proposal {
        return Err(CommandError::Input(InputError::WrongKeyKind {
            expected: RecordKind::Proposal,
            found: proposal.kind(),
        }));
    }
    let read_set = [RevPair {
        key: proposal,
        revision: expected_proposal_revision,
    }];
    expect_read_set(&read_set, &[proposal])?;

    let payload = intent::encode_review_edit(proposal, decision, note, ledger)?;
    // The review frame is not persisted as a blob: it names no candidate, so only its
    // digest (in the transaction header and the outcome) has to survive.
    let (_frame_bytes, digest) = frame_and_digest(
        REVIEW_CLAIM_EDIT,
        intent::OPERATION_VERSION,
        &payload,
        &read_set,
        ledger,
    )?;
    if let Some(retry) = retry_outcome(store, authorized.principal(), meta.request_id, digest)? {
        return Ok(retry);
    }
    if store.is_fenced() {
        return Err(CommandError::Fenced);
    }
    ensure_import_ready(store, ledger)?;

    let basis = store.commit_basis(IdSpace::ProposalId, ledger)?;
    let stored = read_pending_proposal(
        store,
        proposal,
        expected_proposal_revision,
        basis.basis,
        ledger,
    )?;

    // A rejection needs no candidate: it only closes the proposal, declaring the payload
    // blob it still references so the declaration set stays a superset of the refs.
    if decision == ReviewDecision::Reject {
        let mut next = proposal_copy(&stored.record, ledger)?;
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
            &authorized,
            &meta,
            digest,
            limits,
            ledger,
            &next,
            proposal_next,
            basis.rules,
            basis.rules_len,
            Some((stored.payload_ref, stored.payload_len()?)),
        );
    }

    // The candidate is derived from the stored frame, decoded by the ONE P1 decoder.
    let plan = read_stored_intent(&stored, ledger)?;

    // The complete stored two-object baseline, before any lifecycle question.
    let term_found = {
        let record = folded_current(store, plan.subject, ledger)?;
        match record {
            Some(FoldedRecord::Term(term)) => Some((term.retired, term.revision)),
            _ => None,
        }
    };
    let claim_found = {
        let record = folded_current(store, plan.claim, ledger)?;
        match record {
            Some(FoldedRecord::Claim(claim)) => Some(claim.revision),
            _ => None,
        }
    };
    let conflict = term_found.map(|(_, revision)| revision) != Some(plan.term_baseline)
        || claim_found != Some(plan.claim_baseline);
    if conflict {
        let mut next = proposal_copy(&stored.record, ledger)?;
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
            &authorized,
            &meta,
            digest,
            limits,
            ledger,
            &next,
            proposal_next,
            basis.rules,
            basis.rules_len,
            Some((stored.payload_ref, stored.payload_len()?)),
        );
    }

    // The baseline is current, so the lifecycle states whether THIS candidate can apply.
    if term_found.is_some_and(|(retired, _)| retired) {
        return Err(CommandError::Input(InputError::SubjectRetired(
            plan.subject,
        )));
    }
    let mut next_claim = {
        let record = folded_current(store, plan.claim, ledger)?
            .ok_or(CommandError::Input(InputError::MissingClaim(plan.claim)))?;
        let FoldedRecord::Claim(record) = record else {
            return Err(CommandError::Internal(
                InternalDefect::EncodeDecodeDivergence,
            ));
        };
        if record.status != ClaimStatus::Accepted || record.concept != plan.subject {
            return Err(CommandError::Input(InputError::SubjectNotEditable));
        }
        claim_copy(record, ledger)?
    };

    // Only now, with the baseline current and the subject editable, is the replacement
    // text made into a blob: one hash and one admitted copy of exactly the stored bytes.
    let text_ref = map_format(typed::raw_hash(plan.text.as_bytes(), ledger))?;
    let text_bytes = admitted_bytes(plan.text.as_bytes(), ledger)?;

    // The candidate: only the text reference and the revision/audit fields move. Every
    // other claim field — kind, concept, confidence, status, legacy, extensions and the
    // advisory changed_fields — is preserved exactly as copied.
    next_claim.revision = revision_next(next_claim.revision)?;
    next_claim.text_ref = text_ref;
    next_claim.common.reason = Some(admitted_string(note, ledger)?);
    next_claim.common.recorded_by = Some(admitted_string(authorized.principal(), ledger)?);
    next_claim.common.actor_origin = ActorOrigin::Authenticated;
    next_claim.common.updated_at = Some(meta.created_at);
    let claim_next = next_claim.revision;

    let mut next_proposal = proposal_copy(&stored.record, ledger)?;
    let proposal_next = decide_proposal(
        &mut next_proposal,
        ProposalStatus::Approved,
        note,
        authorized.principal(),
        meta.created_at,
        ledger,
    )?;

    let version = version_next(store.head())?;
    let affected = [
        RevPair {
            key: next_claim.key,
            revision: claim_next,
        },
        RevPair {
            key: next_proposal.key,
            revision: proposal_next,
        },
    ];
    let result_bytes = typed::result_payload(version, &affected, &[], ledger)?;
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
        EventInput::Claim(typed::ClaimEvent::Revised, &next_claim),
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::Proposal {
            decided: true,
            record: &next_proposal,
        },
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::Outcome(&outcome),
        ledger,
    ))?);

    let result_len = extent(result_bytes.len())?;
    let mut blobs: Vec<BlobDeclaration> = admitted_vec(4, ledger)?;
    blobs.push(BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: BlobSource::Existing,
    });
    blobs.push(BlobDeclaration {
        sha256: stored.payload_ref,
        declared_len: stored.payload_len()?,
        source: BlobSource::Existing,
    });
    // The edited prose becomes a real blob; an already-present identical text is legal.
    blobs.push(BlobDeclaration {
        sha256: text_ref,
        declared_len: extent(text_bytes.len())?,
        source: BlobSource::Provide(text_bytes),
    });
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: result_len,
        source: BlobSource::Provide(result_bytes),
    });

    let prepared = header(
        store,
        &Attempt {
            principal: authorized.principal(),
            meta: &meta,
            digest,
        },
        version,
        events,
        blobs,
        basis.rules,
        ledger,
    )?;
    into_outcome(store.commit(prepared, limits, ledger))
}

/// The reviewed candidate as recovered from the proposal's committed payload.
///
/// Everything here is either a scalar or a borrow **of the proposal's held payload
/// owner**, so no decoded table or frame buffer is copied out and no borrowed slice can
/// outlive the bytes it points into. The replacement text is deliberately a borrow: it is
/// hashed and copied only once the baseline is known to be current.
struct StoredIntent<'a> {
    subject: RecordKey,
    claim: RecordKey,
    term_baseline: RecordRevision,
    claim_baseline: RecordRevision,
    text: &'a str,
}

/// Decode the stored frame and verify it against the proposal metadata.
///
/// This performs validation only: it reads the held payload, decodes the ONE P1 frame,
/// applies the operation registry, decodes the operation payload and matches every stored
/// field against the proposal's own metadata. It produces **no** candidate and no hash, so
/// the caller can finish its baseline and lifecycle checks before any replacement text is
/// hashed or copied.
///
/// The decoded request and its operation table are locals whose borrows end here; the
/// returned plan borrows only the payload owner passed in, which the caller holds.
fn read_stored_intent<'a>(
    stored: &'a StoredProposal,
    ledger: &mut dyn LeaseLedger,
) -> Result<StoredIntent<'a>, CommandError> {
    let decoded: DecodedRequest<'_> = match typed::decode_request(stored.payload.as_bytes(), ledger)
    {
        RequestOutcome::Ok(decoded) => decoded,
        RequestOutcome::Rejected(error) => {
            return Err(CommandError::Input(InputError::PersistedFrame(error)));
        }
        RequestOutcome::Refused(refusal) => return Err(CommandError::Refused(refusal)),
    };
    // This schema names its two objects explicitly and pins no whole-KB version. A stored
    // frame that pins one is legal P1 but is not a submission this schema can interpret,
    // so it is refused as unsupported rather than silently ignored.
    if let Some(pinned) = decoded.expected_version {
        return Err(CommandError::Input(InputError::StoredExpectedVersion {
            pinned,
        }));
    }
    let operation = match decoded.ops.as_slice() {
        [operation] => operation,
        other => {
            return Err(CommandError::Input(InputError::StoredOperationCount {
                found: other.len(),
            }));
        }
    };
    if operation.kind != SUBMIT_CLAIM_EDIT || operation.version != intent::OPERATION_VERSION {
        return Err(CommandError::Input(
            InputError::UnsupportedStoredOperation {
                kind: operation.kind,
                version: operation.version,
            },
        ));
    }
    let decoded_intent =
        intent::decode_submit(operation.payload, ledger).map_err(map_stored_payload)?;

    // The stored payload must describe the proposal it is attached to.
    if decoded_intent.subject != stored.record.subject {
        return Err(CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::Subject,
        )));
    }
    if decoded_intent.declared_author != stored.record.declared_author {
        return Err(CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::DeclaredAuthor,
        )));
    }
    let [first, second] = stored.record.base_revisions.as_slice() else {
        return Err(CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::BaseRevisions,
        )));
    };
    let term_baseline = if first.key == decoded_intent.subject {
        first.revision
    } else if second.key == decoded_intent.subject {
        second.revision
    } else {
        return Err(CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::BaseRevisions,
        )));
    };
    let claim_baseline = if first.key == decoded_intent.claim {
        first.revision
    } else if second.key == decoded_intent.claim {
        second.revision
    } else {
        return Err(CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::BaseRevisions,
        )));
    };
    if decoded.read_set != stored.record.base_revisions {
        return Err(CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::ReadSet,
        )));
    }

    Ok(StoredIntent {
        subject: decoded_intent.subject,
        claim: decoded_intent.claim,
        term_baseline,
        claim_baseline,
        text: decoded_intent.text,
    })
}

/// Commit one decided proposal revision with its result payload.
///
/// Used by reject and conflict: the proposal record is the only affected record, no
/// counter advances, and the referenced payload blob is re-declared as `Existing` with
/// the length this attempt actually verified.
#[allow(clippy::too_many_arguments)]
fn finish_decision<F: FileSystem>(
    store: &mut Store<F>,
    authorized: &Authorized<'_>,
    meta: &RequestMeta<'_>,
    digest: [u8; 32],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
    decided: &ProposalRecord,
    decided_revision: RecordRevision,
    rules: sage_db_types::prepared::RulesBinding,
    rules_len: u64,
    payload: Option<([u8; 32], u64)>,
) -> Result<HandleOutcome, CommandError> {
    let version = version_next(store.head())?;
    let affected = [RevPair {
        key: decided.key,
        revision: decided_revision,
    }];
    let result_bytes = typed::result_payload(version, &affected, &[], ledger)?;
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

    let result_len = extent(result_bytes.len())?;
    let mut blobs: Vec<BlobDeclaration> = admitted_vec(3, ledger)?;
    blobs.push(BlobDeclaration {
        sha256: rules.rules_ref,
        declared_len: rules_len,
        source: BlobSource::Existing,
    });
    if let Some((sha256, declared_len)) = payload {
        blobs.push(BlobDeclaration {
            sha256,
            declared_len,
            source: BlobSource::Existing,
        });
    }
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: result_len,
        source: BlobSource::Provide(result_bytes),
    });

    let prepared = header(
        store,
        &Attempt {
            principal: authorized.principal(),
            meta,
            digest,
        },
        version,
        events,
        blobs,
        rules,
        ledger,
    )?;
    into_outcome(store.commit(prepared, limits, ledger))
}

/// A real commit result is returned as-is; only a definite refusal becomes an error.
///
/// `Unknown` stays `Ok`: it is an ambiguous attempt whose authority must be resolved,
/// not a fabricated error, and this module makes no durability claim of its own.
fn into_outcome(outcome: HandleOutcome) -> Result<HandleOutcome, CommandError> {
    match outcome {
        HandleOutcome::NotCommitted { reason } => Err(CommandError::Store(reason)),
        other => Ok(other),
    }
}

#[cfg(test)]
mod admitted_bytes_tests {
    #[test]
    fn byte_copy_is_admitted_once_at_its_exact_logical_extent() {
        use sage_db_store::lease::AttemptLedger;
        use sage_db_types::query::RunToCompletion;
        let control = RunToCompletion;
        let mut ledger = AttemptLedger::open(3, 3, &control).unwrap();
        let bytes = super::admitted_bytes(b"abc", &mut ledger).unwrap();
        assert_eq!(bytes, b"abc");
        assert_eq!(ledger.stats().live_bytes, 3);
        assert_eq!(ledger.stats().admitted_bytes, Some(3));
    }
}
