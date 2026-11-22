//! Concept text review lifecycle: submit and review atom/body edits of an **existing**
//! active Term.
//!
//! # What this module is
//!
//! The structured successor of the old `refine-atom` / `refine-body` commands. It owns a
//! Proposal of kind [`ProposalKind::ConceptMarkdown`] whose payload is the registered
//! concept-text request frame, plus the review that applies or refuses it.
//!
//! Per command it composes, in order:
//!
//! 1. exactly one explicit capability — [`Capability::Propose`] to submit,
//!    [`Capability::Review`] to decide; neither implies the other;
//! 2. caller-input validation, before any read;
//! 3. the canonical P1 request frame and its `request`-domain digest, through the existing
//!    claims helpers — no second framing or hashing authority;
//! 4. **idempotency first**: an exact retry returns the original outcome before any
//!    current-state, lifecycle or collision check;
//! 5. the write fence, then the real import gate;
//! 6. the authoritative reads the decision needs (the subject's current revision, or the
//!    stored proposal and its committed payload);
//! 7. one `PreparedTransaction` and the real `Store::commit`.
//!
//! # What it deliberately does not touch
//!
//! `slug`, `layer`, `domains`, `confidence`, `source`, `triggers`, `retired`, and every
//! assertion and graph edge are out of reach. An approved edit replaces the two text fields
//! the payload names and nothing else, so this is not Term creation, retirement or a
//! full-concept schema edit.
//!
//! # The subject's atom lives in the record; the body lives in a blob
//!
//! `TermRecord.atom` is an inline string, while `TermRecord.body_ref` names a
//! content-addressed blob. An approved atom edit therefore changes the record, an approved
//! body edit changes the blob, and both change the record's `body_ref`. A field the payload
//! does *not* name keeps its committed value exactly, including its existing blob — which
//! is re-declared as `Existing` with the length the store verified, never a guessed zero.
//!
//! # Review derives the candidate from the stored bytes
//!
//! The reviewer never supplies what the edit should be: the stored request frame is decoded
//! once under its leased owner, and the candidate is built from *that*. Dependencies come
//! from the payload, so no caller-supplied "required" subset can narrow or widen them.

use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::lease::LeaseLedger;
use sage_db_store::typed::{self, EventInput};
use sage_db_store::view::{CommittedBlob, ViewBasis};
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ActorOrigin, CommonFields, Extensions, IdAllocationEvent, IdSpace, ProposalKind,
    ProposalRecord, ProposalStatus, RecordKey, RecordKind, RevPair, Watermark,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};

use crate::auth::{AuthError, Capability, Grants};
use crate::claims::{
    CommandError, InputError, InternalDefect, RequestMeta, StoredField, admitted_bytes,
    admitted_string, admitted_vec, check_meta, ensure_import_ready, extent, folded_current,
    frame_and_digest_with_version, map_format, outcome_event, retry_outcome, revision_next,
    version_next,
};
use crate::concept_intent::{
    self, ConceptTextEdit, ConceptTextPayloadError, DECISION_APPROVE, REVIEW_CONCEPT_TEXT,
    SUBMIT_CONCEPT_TEXT,
};

use crate::legacy_proposal::{
    self, BodyObservation, LegacyDirectory, LegacyIntent, LegacyProposal, LegacyProposalFile,
    LegacyReview,
};

/// A caller-supplied concept-text value this operation cannot accept, before anything is
/// written.
///
/// The payload codec's own refusals travel as [`ConceptTextPayloadError`]; this type adds
/// only the authoritative-state verdicts the writer discovers after reading.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConceptTextInputError {
    /// The subject term does not exist on the current basis.
    MissingSubject(RecordKey),
    /// The subject term is retired; a text edit never revives it.
    SubjectRetired(RecordKey),
    /// The proposal id space is exhausted.
    ProposalIdExhausted,
    /// The next proposal id is already occupied.
    ProposalIdCollision(RecordKey),
    /// A payload this module generated or was given is not a legal concept-text payload.
    Payload(ConceptTextPayloadError),
}

/// A concept-text defect that no caller input can cause.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConceptTextInternal {
    /// The canonical dispatcher read back a term image as another family.
    CopiedFamily,
    /// The record's atom and the decoded edit disagree in a way the writer did not create.
    EncoderDivergence,
}

/// Why a concept-text command did not produce a commit.
///
/// The same shape as the other P2 families: a shared admission, resource or store verdict
/// travels in [`CommandError`], while this family adds only its own caller-input and
/// internal arms. No reviewer capability is inferred from authorship.
#[derive(Debug)]
pub enum ConceptTextError {
    /// A shared admission, resource or store verdict.
    Command(CommandError),
    /// A concept-text caller-input verdict.
    Input(ConceptTextInputError),
    /// A concept-text internal invariant.
    Internal(ConceptTextInternal),
}

impl From<CommandError> for ConceptTextError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

impl From<InputError> for ConceptTextError {
    fn from(error: InputError) -> Self {
        Self::Command(CommandError::Input(error))
    }
}

impl From<ConceptTextInputError> for ConceptTextError {
    fn from(error: ConceptTextInputError) -> Self {
        Self::Input(error)
    }
}

impl From<ConceptTextInternal> for ConceptTextError {
    fn from(error: ConceptTextInternal) -> Self {
        Self::Internal(error)
    }
}

impl From<AuthError> for ConceptTextError {
    fn from(error: AuthError) -> Self {
        Self::Command(error.into())
    }
}

impl From<Refusal> for ConceptTextError {
    fn from(refusal: Refusal) -> Self {
        Self::Command(refusal.into())
    }
}

impl From<ConceptTextPayloadError> for ConceptTextError {
    fn from(error: ConceptTextPayloadError) -> Self {
        match error {
            ConceptTextPayloadError::Refused(refusal) => Self::Command(refusal.into()),
            other => Self::Input(ConceptTextInputError::Payload(other)),
        }
    }
}

/// Reject a key whose kind is not the one this position requires.
fn check_kind(key: RecordKey, required: RecordKind) -> Result<(), ConceptTextError> {
    if key.kind() != required {
        return Err(CommandError::Input(InputError::WrongKeyKind {
            expected: required,
            found: key.kind(),
        })
        .into());
    }
    Ok(())
}

/// Reject a read set that is not exactly `expected`, in that order.
///
/// Borrowed comparison only: the expected key is computed, not collected into a vector.
fn expect_read_set(read_set: &[RevPair], expected: RecordKey) -> Result<(), ConceptTextError> {
    if read_set.len() != 1 {
        return Err(CommandError::Input(InputError::ReadSetLength {
            expected: 1,
            found: read_set.len(),
        })
        .into());
    }
    let row = &read_set[0];
    if row.key != expected {
        return Err(CommandError::Input(InputError::ReadSetKey {
            index: 0,
            expected,
            found: row.key,
        })
        .into());
    }
    Ok(())
}

/// The canonical event a stored term after-image was written under.
///
/// `changed_fields` is advisory and legal only on a non-first revision, which is why it
/// travels with a `Revised` image; a creation image carrying it is refused by the writer.
///
/// Derived from the record itself, never from a caller flag: the canonical writer refuses a
/// creation event on any revision but the first, and refuses a later event on the first
/// revision, so the revision and the lifecycle flag together are the only correct source.
/// A retired record is always beyond the first revision (retirement is a revision), which is
/// what the writer enforces.
fn term_event(record: &sage_db_types::persist::TermRecord) -> typed::TermEvent {
    if record.retired {
        typed::TermEvent::Retired
    } else if record.revision == RecordRevision::FIRST {
        typed::TermEvent::Created
    } else {
        typed::TermEvent::Revised
    }
}

/// Copy a term canonically rather than by an owning clone.
///
/// The event is derived from the record's actual revision and lifecycle, so copying a term at
/// revision 2 emits a `Revised` after-image rather than a creation image the canonical writer
/// would (correctly) refuse.
fn term_copy(
    record: &sage_db_types::persist::TermRecord,
    ledger: &mut dyn SharedLedger,
) -> Result<sage_db_types::persist::TermRecord, ConceptTextError> {
    let event = term_event(record);
    match map_format(typed::copy_payload(EventInput::Term(event, record), ledger))? {
        typed::DispatchedPayload::Term(owned) => Ok(owned),
        _ => Err(ConceptTextInternal::CopiedFamily.into()),
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
) -> Result<PreparedTransaction, ConceptTextError> {
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

/// A real commit result is returned as-is; only a definite refusal becomes an error.
fn into_outcome(outcome: HandleOutcome) -> Result<HandleOutcome, ConceptTextError> {
    match outcome {
        HandleOutcome::NotCommitted { reason } => Err(CommandError::Store(reason).into()),
        other => Ok(other),
    }
}

/// Submit an atom/body edit of an existing active term as a pending proposal.
///
/// The request frame itself becomes the proposal's payload blob: its bytes are persisted
/// verbatim, so the reviewer reads exactly the request the author made, and `payload_ref` is
/// the raw SHA-256 of those bytes. `declared_author` is stored as a **separate** field from
/// the authenticated executor, never inferred from it.
///
/// `read_set` must be exactly `[subject @ current revision]`.
///
/// # Errors
///
/// [`ConceptTextError`]: authorization (exactly `propose`), caller input (wrong key kind,
/// illegal mask, malformed atom/body/author, payload ceiling, a stale submitted baseline),
/// incomplete authority, an idempotency mismatch, a fenced handle, a missing or retired
/// subject, an exhausted or occupied proposal id, or a real ledger/store refusal.
#[allow(clippy::too_many_arguments)]
pub fn submit_concept_text<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    edit: ConceptTextEdit<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, ConceptTextError> {
    submit_concept_text_with_reason(store, grants, meta, edit, None, read_set, limits, ledger)
}

/// Submit a concept text proposal, preserving an explicitly supplied submission reason.
///
/// An absent reason keeps the original operation bytes and receipt identity.
/// A present reason is bound to the v2 request before the outcome lookup.
///
/// # Errors
///
/// As [submit_concept_text], including a malformed or oversized reason.
#[allow(clippy::too_many_arguments)]
pub fn submit_concept_text_with_reason<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    edit: ConceptTextEdit<'_>,
    reason: Option<&str>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, ConceptTextError> {
    submit_concept_text_inner(
        store, grants, meta, edit, reason, None, read_set, limits, ledger,
    )
}

/// Submit legacy atom or body metadata as part of the same canonical proposal commit.
///
/// The trusted caller obtains the legacy comparison basis and subject revision
/// from its observed view. A body hash is the caller's projection of that view.
///
/// # Errors
/// As [submit_concept_text_with_reason], with malformed legacy metadata or a moved view.
#[allow(clippy::too_many_arguments)]
pub fn submit_legacy_concept_text<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    edit: ConceptTextEdit<'_>,
    reason: &str,
    legacy: LegacyProposal<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, ConceptTextError> {
    submit_concept_text_inner(
        store,
        grants,
        meta,
        edit,
        Some(reason),
        Some(legacy),
        read_set,
        limits,
        ledger,
    )
}

#[allow(clippy::too_many_arguments)]
fn submit_concept_text_inner<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    edit: ConceptTextEdit<'_>,
    reason: Option<&str>,
    legacy: Option<LegacyProposal<'_>>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, ConceptTextError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::Propose, ledger)?;
    check_meta(&meta, authorized.principal())?;
    check_kind(edit.subject, RecordKind::Term)?;
    expect_read_set(read_set, edit.subject)?;

    // The payload is prepared before the outcome lookup because its digest *is* the
    // idempotency key component: an exact retry must hash to the same bytes.
    let (operation_version, payload) = if let Some(options) = legacy {
        legacy_proposal::encode_concept_submit(
            options.file,
            &edit,
            reason.ok_or(CommandError::from(InputError::LegacyProposalMetadata))?,
            ledger,
        )?
    } else if let Some(reason) = reason {
        (
            concept_intent::SUBMIT_REASON_VERSION,
            concept_intent::encode_submit_with_reason(&edit, reason, ledger)?,
        )
    } else {
        (
            concept_intent::OPERATION_VERSION,
            concept_intent::encode_submit(&edit, ledger)?,
        )
    };
    let expect_version = legacy.map(|value| value.observed_version);
    let (frame_bytes, digest) = frame_and_digest_with_version(
        SUBMIT_CONCEPT_TEXT,
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

    // The subject must be current, active, and pinned at the revision the caller named.
    {
        let found = folded_current(store, edit.subject, ledger)?
            .ok_or(ConceptTextInputError::MissingSubject(edit.subject))?;
        let FoldedRecord::Term(term) = found else {
            return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
        };
        if term.retired {
            return Err(ConceptTextInputError::SubjectRetired(edit.subject).into());
        }
        if let Some(options) = legacy {
            legacy_proposal::check_text_name(options.file, &term.slug)?;
            if let LegacyProposalFile::Atom { current_atom, .. } = options.file
                && current_atom != term.atom
            {
                return Err(CommandError::from(InputError::LegacyProposalAtomMismatch).into());
            }
        }
        if term.revision != read_set[0].revision {
            return Err(CommandError::Input(InputError::StaleBaseline {
                key: edit.subject,
                expected: read_set[0].revision,
            })
            .into());
        }
    }

    // Allocate the next proposal id from the actual committed mark, then check the id is
    // genuinely free: an absent mark means "nothing allocated", not "id 1 is free".
    let proposal_id = basis
        .allocated_high_water
        .checked_add(1)
        .ok_or(ConceptTextInputError::ProposalIdExhausted)?;
    let proposal_key = RecordKey::new(RecordKind::Proposal, proposal_id);
    if folded_current(store, proposal_key, ledger)?.is_some() {
        return Err(ConceptTextInputError::ProposalIdCollision(proposal_key).into());
    }

    let frame_bytes = if operation_version == legacy_proposal::SUBMIT_REFERENCED_BODY_VERSION {
        let body = edit
            .body
            .ok_or(CommandError::from(InputError::LegacyProposalMetadata))?;
        legacy_proposal::pack_body_bundle(&frame_bytes, body, ledger)?
    } else {
        frame_bytes
    };
    let payload_ref = map_format(typed::raw_hash(&frame_bytes, ledger))?;
    let version = version_next(store.head())?;

    // The proposal after-image: revision 1, pending, the author's baseline verbatim.
    let mut base_revisions: Vec<RevPair> = admitted_vec(1, ledger)?;
    base_revisions.push(read_set[0]);
    let proposal = ProposalRecord {
        key: proposal_key,
        revision: RecordRevision::FIRST,
        status: ProposalStatus::Pending,
        // The registered kind. The payload's operation kind is what distinguishes this
        // request from a legacy markdown submission, and review checks both.
        kind: ProposalKind::ConceptMarkdown,
        subject: edit.subject,
        base_revisions,
        payload_ref,
        review_note: None,
        declared_author: admitted_string(edit.declared_author, ledger)?,
        common: CommonFields {
            recorded_by: Some(admitted_string(authorized.principal(), ledger)?),
            actor_origin: ActorOrigin::Authenticated,
            reason: reason
                .map(|value| admitted_string(value, ledger))
                .transpose()?,
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

/// The stored proposal as review needs it, already copied canonically.
///
/// It owns the ONE leased read of the proposal's committed payload. Nothing re-reads or
/// re-hashes that blob later: the frame is decoded from this owner's borrowed bytes, and the
/// owner is held across the commit so those bytes stay charged until the transaction ends.
struct StoredProposal {
    record: ProposalRecord,
    payload_ref: [u8; 32],
    payload: CommittedBlob,
}

impl StoredProposal {
    /// The verified committed payload length.
    fn payload_len(&self) -> Result<u64, ConceptTextError> {
        extent(self.payload.len()).map_err(ConceptTextError::Command)
    }
}

/// Read a pending proposal, its expected revision, and its committed payload on `basis`.
fn read_pending_proposal<F: FileSystem>(
    store: &mut Store<F>,
    proposal: RecordKey,
    expected_revision: RecordRevision,
    basis: ViewBasis,
    ledger: &mut dyn LeaseLedger,
) -> Result<StoredProposal, ConceptTextError> {
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
    if record.kind != ProposalKind::ConceptMarkdown {
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

/// The concept-text intent decoded from a stored proposal's own bytes.
pub struct ConceptTextIntent<'a> {
    /// Term whose text is being proposed.
    pub subject: RecordKey,
    /// Its last observed revision at submission.
    pub term_baseline: RecordRevision,
    /// Optional complete replacement atom.
    pub atom: Option<&'a str>,
    /// Optional complete replacement body.
    pub body: Option<&'a str>,
    /// Original submission reason from the hash-bound request, independent of the review note.
    pub reason: Option<&'a str>,
}

/// Decode already hash-verified proposal bytes and check their recorded metadata.
///
/// The caller must bind the record and its content to the same publication.
///
/// # Errors
/// Refuses an unsupported operation, divergent metadata, or an exhausted caller ledger.
///
/// A reviewer that accepted a legacy markdown submission here would apply bytes nobody
/// registered, so an unregistered operation is refused rather than guessed at.
pub fn decode_stored_intent<'a>(
    record: &ProposalRecord,
    payload: &'a [u8],
    ledger: &mut dyn SharedLedger,
) -> Result<ConceptTextIntent<'a>, ConceptTextError> {
    if record.kind != ProposalKind::ConceptMarkdown {
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
    // A stored frame that pins a whole-KB version is legal P1 but is not a submission this
    // schema can interpret.
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
    let intent = match (operation.kind, operation.version) {
        (SUBMIT_CONCEPT_TEXT, concept_intent::OPERATION_VERSION) => {
            concept_intent::decode_submit(operation.payload, ledger)
        }
        (SUBMIT_CONCEPT_TEXT, concept_intent::SUBMIT_REASON_VERSION) => {
            concept_intent::decode_submit_with_reason(operation.payload, ledger)
        }
        (kind, version) => {
            return Err(CommandError::Input(InputError::UnsupportedStoredOperation {
                kind,
                version,
            })
            .into());
        }
    }
    .map_err(|error| match error {
        ConceptTextPayloadError::Refused(refusal) => {
            ConceptTextError::Command(CommandError::Refused(refusal))
        }
        other => ConceptTextError::Input(ConceptTextInputError::Payload(other)),
    })?;

    // The stored payload must describe the proposal it is attached to.
    if intent.subject != record.subject {
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
    if operation.version == concept_intent::SUBMIT_REASON_VERSION
        && record.status == ProposalStatus::Pending
        && record.common.reason.as_deref() != intent.reason
    {
        return Err(
            CommandError::Input(InputError::StoredMetadataMismatch(StoredField::Reason)).into(),
        );
    }
    let [only] = record.base_revisions.as_slice() else {
        return Err(CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::BaseRevisions,
        ))
        .into());
    };
    if only.key != intent.subject {
        return Err(CommandError::Input(InputError::StoredMetadataMismatch(
            StoredField::BaseRevisions,
        ))
        .into());
    }
    if decoded.read_set != record.base_revisions {
        return Err(
            CommandError::Input(InputError::StoredMetadataMismatch(StoredField::ReadSet)).into(),
        );
    }

    // The mask is the edit's own claim about which fields it replaces, so it must agree with
    // the fields the codec returned. A divergence would mean the encoder and decoder disagree
    // about the layout — an internal defect, never caller input.
    let claims_atom = intent.field_mask & concept_intent::MASK_ATOM != 0;
    let claims_body = intent.field_mask & concept_intent::MASK_BODY != 0;
    if claims_atom != intent.atom.is_some() || claims_body != intent.body.is_some() {
        return Err(ConceptTextInternal::EncoderDivergence.into());
    }

    Ok(ConceptTextIntent {
        subject: intent.subject,
        term_baseline: only.revision,
        atom: intent.atom,
        body: intent.body,
        reason: intent.reason,
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
) -> Result<RecordRevision, ConceptTextError> {
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
///
/// Used by reject and conflict: the proposal record is the only affected record, no counter
/// advances, and the referenced payload blob is re-declared as `Existing` with the length
/// this attempt verified.
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
) -> Result<HandleOutcome, ConceptTextError> {
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

/// Approve or reject a pending concept-text proposal.
///
/// Approval re-derives the candidate from the **stored** payload, never from the reviewer's
/// memory of it: the frame's operation kind, version, subject, field mask, declared author
/// and read set must all agree with the proposal's own metadata, and the stored text is the
/// text that becomes the new atom or body.
///
/// A stale baseline is a committed `Conflicted` decision, not an error. It is checked
/// **before** any lifecycle question, and no unrelated pending proposal is ever scanned.
///
/// `max_existing_body_len` is a **resource policy**, supplied by the caller's executor and
/// excluded from the request digest: it bounds re-reading the *unchanged* body blob, whose
/// length this writer never sees in the payload (an atom-only edit names no body). It is not
/// the frame ceiling — a committed body may legitimately exceed the 64 KiB that a *new*
/// payload's text is bounded by — and it is checked only after the idempotency lookup, so an
/// exact retry still returns its original outcome even with a zero ceiling. The caller's
/// ledger remains the actual work cap.
///
/// # Errors
///
/// [`ConceptTextError`]: authorization (exactly `review`), caller input, incomplete
/// authority, an idempotency mismatch, a fenced handle, a terminal or absent proposal, an
/// unregistered or malformed stored payload, a stored-metadata contradiction, a subject that
/// is current but retired, or a real ledger/store refusal.
#[allow(clippy::too_many_arguments)]
pub fn review_concept_text<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    proposal: RecordKey,
    expected_proposal_revision: RecordRevision,
    decision: u16,
    note: &str,
    max_existing_body_len: u64,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, ConceptTextError> {
    review_concept_inner(
        store,
        grants,
        meta,
        proposal,
        expected_proposal_revision,
        decision,
        note,
        None,
        max_existing_body_len,
        limits,
        ledger,
    )
}

/// Atomically merge one compatibility atom or body proposal on the checked publication.
///
/// Atom writes compare the quoted atom. Body writes compare the public revision passed by
/// the product projection, except that an already-equal body approves without rewriting
/// the Term. A mismatch leaves the proposal pending.
///
/// # Errors
/// Refuses a stale alias or public revision, malformed data, authorization or resource failures.
#[allow(clippy::too_many_arguments)]
pub fn merge_legacy_concept_text<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    proposal: RevPair,
    review: LegacyReview<'_>,
    body: Option<BodyObservation>,
    note: &str,
    max_existing_body_len: u64,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, ConceptTextError> {
    review_concept_inner(
        store,
        grants,
        meta,
        proposal.key,
        proposal.revision,
        DECISION_APPROVE,
        note,
        Some((review, body)),
        max_existing_body_len,
        limits,
        ledger,
    )
}

#[allow(clippy::too_many_arguments)]
fn review_concept_inner<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    proposal: RecordKey,
    expected_proposal_revision: RecordRevision,
    decision: u16,
    note: &str,
    legacy: Option<(LegacyReview<'_>, Option<BodyObservation>)>,
    max_existing_body_len: u64,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, ConceptTextError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::Review, ledger)?;
    check_meta(&meta, authorized.principal())?;
    check_kind(proposal, RecordKind::Proposal)?;
    let read_set = [RevPair {
        key: proposal,
        revision: expected_proposal_revision,
    }];
    expect_read_set(&read_set, proposal)?;

    let payload = concept_intent::encode_review(proposal, decision, note, ledger)?;
    // The review frame is not persisted as a blob: it names no candidate, so only its digest
    // (in the transaction header and the outcome) has to survive.
    let (op_version, payload) = if let Some((review, body)) = legacy {
        if review.directory == LegacyDirectory::Edges {
            return Err(CommandError::from(InputError::LegacyProposalMetadata).into());
        }
        (
            legacy_proposal::REVIEW_VERSION,
            legacy_proposal::encode_review(review, body, &payload, ledger)?,
        )
    } else {
        (concept_intent::OPERATION_VERSION, payload)
    };
    let expect_version = legacy.map(|(review, _)| review.observed_version);
    let (_frame_bytes, digest) = frame_and_digest_with_version(
        REVIEW_CONCEPT_TEXT,
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

    if let Some((review, _)) = legacy {
        legacy_proposal::require_current_alias(store, review, read_set[0], ledger)?;
    }

    let basis = store
        .commit_basis(IdSpace::ProposalId, ledger)
        .map_err(CommandError::from)?;
    let stored = read_pending_proposal(
        store,
        proposal,
        expected_proposal_revision,
        basis.basis,
        ledger,
    )?;

    // The verified payload facts are captured before the record moves: the declaration set
    // must stay a superset of the refs, and the length is the store's own verified one.
    let payload_facts = (stored.payload_ref, stored.payload_len()?);

    // A rejection needs no candidate: it only closes the proposal, declaring the payload
    // blob it still references so the declaration set stays a superset of the refs.
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

    let (plan, body_sha256) = if let Some((_, observation)) = legacy {
        let Some(compatibility) =
            legacy_proposal::decode_stored(&stored.record, stored.payload.as_bytes(), ledger)?
        else {
            return Err(CommandError::from(InputError::LegacyProposalMetadata).into());
        };
        let LegacyIntent::Text(intent) = compatibility.intent else {
            return Err(CommandError::from(InputError::LegacyProposalMetadata).into());
        };
        let found = folded_current(store, intent.subject, ledger)?
            .ok_or(ConceptTextInputError::MissingSubject(intent.subject))?;
        let FoldedRecord::Term(term) = found else {
            return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
        };
        if term.retired {
            return Err(ConceptTextInputError::SubjectRetired(intent.subject).into());
        }
        let revision = term.revision;
        let body_ref = term.body_ref;
        match (compatibility.compatibility.file, observation) {
            (LegacyProposalFile::Atom { current_atom, .. }, None) => {
                let quoted = current_atom.trim_matches(legacy_proposal::js_whitespace);
                if !quoted.is_empty() && term.atom != quoted {
                    return Err(CommandError::from(InputError::LegacyProposalAtomMismatch).into());
                }
            }
            (
                LegacyProposalFile::Body {
                    expected_revision, ..
                },
                Some(observation),
            ) => {
                let payload = store
                    .read_committed_blob(body_ref, max_existing_body_len, ledger)
                    .map_err(CommandError::from)?;
                if payload.basis() != basis.basis {
                    return Err(CommandError::BasisDivergence.into());
                }
                let body = core::str::from_utf8(payload.as_bytes())
                    .map_err(|_| CommandError::Internal(InternalDefect::EncodeDecodeDivergence))?;
                let candidate = intent
                    .body
                    .ok_or(CommandError::from(InputError::LegacyProposalMetadata))?;
                ledger
                    .try_charge_entries(extent(body.len())?)
                    .map_err(CommandError::from)?;
                ledger
                    .try_charge_entries(extent(candidate.len())?)
                    .map_err(CommandError::from)?;
                ledger.checkpoint().map_err(CommandError::from)?;
                let equal = body.trim_end_matches(legacy_proposal::js_whitespace)
                    == candidate.trim_end_matches(legacy_proposal::js_whitespace);
                drop(payload);
                if equal {
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
                if expected_revision != observation.public_revision {
                    return Err(CommandError::from(InputError::LegacyProposalBodyMismatch {
                        expected: expected_revision,
                        found: observation.public_revision,
                    })
                    .into());
                }
            }
            _ => return Err(CommandError::from(InputError::LegacyProposalMetadata).into()),
        }
        (
            ConceptTextIntent {
                subject: intent.subject,
                term_baseline: revision,
                atom: intent.atom,
                body: intent.body,
                reason: intent.reason,
            },
            compatibility.body_sha256,
        )
    } else {
        let plan = decode_stored_intent(&stored.record, stored.payload.as_bytes(), ledger)?;
        let current = match folded_current(store, plan.subject, ledger)? {
            Some(FoldedRecord::Term(term)) => Some((term.revision, term.retired)),
            Some(_) => {
                return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
            }
            None => None,
        };
        if current.map(|(revision, _)| revision) != Some(plan.term_baseline) {
            let mut next = stored.record;
            let revision = decide_proposal(
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
                revision,
                basis.rules,
                basis.rules_len,
                Some(payload_facts),
            );
        }
        if current.is_some_and(|(_, retired)| retired) {
            return Err(ConceptTextInputError::SubjectRetired(plan.subject).into());
        }
        (plan, None)
    };

    let mut next_term = {
        let found = folded_current(store, plan.subject, ledger)?
            .ok_or(ConceptTextInputError::MissingSubject(plan.subject))?;
        let FoldedRecord::Term(term) = found else {
            return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
        };
        term_copy(term, ledger)?
    };

    // Only now, with the baseline current and the subject active, are the replacement bytes
    // made into blobs: one hash and one admitted copy of exactly the stored text.
    //
    // The atom is inline and the body is content-addressed, so each named field is handled
    // in its own unit. A field the payload does not name keeps its committed value: for the
    // body that means re-declaring the EXISTING blob with the length the store verified, not
    // a zero or a guess.
    // A named body becomes a NEW blob provided by this transaction; an unnamed body keeps
    // its committed ref and is re-declared as `Existing` below, with the store's verified
    // length. The two cases are separate values so neither can masquerade as the other.
    let mut provided_body: Option<Vec<u8>> = None;
    if let Some(atom) = plan.atom {
        // The atom is bounded by the codec before it can reach here; the record stores it
        // inline, so it is admitted as a string rather than as a blob.
        next_term.atom = admitted_string(atom, ledger)?;
    }
    if let Some(body) = plan.body {
        let body_bytes = admitted_bytes(body.as_bytes(), ledger)?;
        next_term.body_ref = match body_sha256 {
            Some(verified) => verified,
            None => map_format(typed::raw_hash(&body_bytes, ledger))?,
        };
        provided_body = Some(body_bytes);
    }

    // Only the revision, the two text fields and the audit move. Every other field — slug,
    // layer, domains, source, confidence, triggers, retirement, legacy provenance, the
    // advisory extensions and changed_fields — is preserved exactly as copied.
    next_term.revision = revision_next(next_term.revision)?;
    next_term.common.reason = Some(admitted_string(note, ledger)?);
    next_term.common.recorded_by = Some(admitted_string(authorized.principal(), ledger)?);
    next_term.common.actor_origin = ActorOrigin::Authenticated;
    next_term.common.updated_at = Some(meta.created_at);

    // The proposal's decision and the term's revision are ONE commitment.
    let mut next_proposal = stored.record;
    let proposal_next = decide_proposal(
        &mut next_proposal,
        ProposalStatus::Approved,
        note,
        authorized.principal(),
        meta.created_at,
        ledger,
    )?;
    let term_next = next_term.revision;

    let version = version_next(store.head())?;
    // Ascending by `(kind, id)`, the canonical order the result payload requires: a Term
    // (kind 1) sorts before a Proposal (kind 7).
    let affected = [
        RevPair {
            key: next_term.key,
            revision: term_next,
        },
        RevPair {
            key: next_proposal.key,
            revision: proposal_next,
        },
    ];
    let result_bytes =
        typed::result_payload(version, &affected, &[], ledger).map_err(CommandError::from)?;
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
            decided: true,
            record: &next_proposal,
        },
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::Term(typed::TermEvent::Revised, &next_term),
        ledger,
    ))?);
    events.push(map_format(typed::prepare_event(
        EventInput::Outcome(&outcome),
        ledger,
    ))?);

    // Declarations: rules (existing), the proposal payload this attempt verified (existing),
    // the body blob (either newly provided or the unchanged existing one), and the result.
    let mut blobs: Vec<BlobDeclaration> = admitted_vec(4, ledger)?;
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
    match provided_body {
        Some(body_bytes) => {
            blobs.push(BlobDeclaration {
                sha256: next_term.body_ref,
                declared_len: extent(body_bytes.len())?,
                source: BlobSource::Provide(body_bytes),
            });
        }
        None => {
            // Unchanged body: re-declare the committed blob with its verified length. The
            // store's own length is the authority, so this is a real `Existing` declaration
            // rather than a invented zero.
            // The *unchanged* body is a committed blob of arbitrary committed size, so its
            // ceiling comes from the caller's explicit resource policy. Borrowing the request
            // frame ceiling here would refuse a legal body the moment that bound moved.
            let existing = store
                .read_committed_blob(next_term.body_ref, max_existing_body_len, ledger)
                .map_err(CommandError::from)?;
            if existing.basis() != basis.basis {
                return Err(CommandError::BasisDivergence.into());
            }
            blobs.push(BlobDeclaration {
                sha256: next_term.body_ref,
                declared_len: extent(existing.len())?,
                source: BlobSource::Existing,
            });
        }
    }
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
