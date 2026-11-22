//! Auxiliary session-checkpoint lifecycle: create, revise and read.
//!
//! # What this module is
//!
//! A [`SessionCheckpointRecord`] is **user-session state** — the serialised state of one
//! reasoning session on one concept. It is not a storage checkpoint, not a snapshot and not
//! graph content. This module is the only writer and reader of that family, and it
//! composes, per command:
//!
//! 1. exactly one explicit capability ([`Capability::WriteAux`] to write,
//!    [`Capability::Read`] to read — neither implies the other);
//! 2. caller-input validation (identity, session ordinal, key kinds, read-set shape);
//! 3. the canonical P1 request frame (one operation, the caller's read set, no expected
//!    whole-KB version) and its `request`-domain digest;
//! 4. **idempotency first**: an exact retry of an already-committed request returns the
//!    original outcome before any current-state or collision check runs;
//! 5. the write fence, then the real `Store::commit_basis`;
//! 6. the authoritative current reads the decision needs (active term, exact CAS, the
//!    immutability of `concept`/`session_no` on a revision);
//! 7. one `PreparedTransaction` and the real `Store::commit`.
//!
//! # Identity, not content uniqueness
//!
//! A checkpoint's `RecordKey` **is** its identity. Creation allocates the next
//! `SessionCheckpointId` from the actual committed high-water mark and verifies the key is
//! free; it never scans every record looking for a matching `(concept, session_no)` tuple,
//! and a revision never re-derives its key from that tuple. `concept` and `session_no` are
//! immutable for the life of the record: a revision naming a different tuple is a caller
//! error, not a new record.
//!
//! # Resources
//!
//! The caller's ledger spans the whole command; nothing here opens a second one. The
//! content blob is hashed and copied only after every admission check has passed, and the
//! copy goes through [`sage_db_store::typed::copy_payload`] rather than an owning
//! `.clone()`. A read holds the store's leased [`CommittedBlob`], so its bytes stay charged
//! and its basis is the same durable publication the record came from.
//!
//! # What is deliberately not claimed
//!
//! No query API, no concept/session enumeration, no graph mutation, no platform
//! durability. `HandleOutcome::Unknown` is returned as the ambiguous attempt it is; the
//! caller resolves before retrying, and this module never clears a fence.

use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::lease::LeaseLedger;
use sage_db_store::typed::{self, EventInput};
use sage_db_store::view::{
    BlobReadError, CommitBasis, CommitReadError, CommittedBlob, RecordReadError, RecordScanError,
    ViewBasis,
};
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::{BudgetError, SharedLedger};
use sage_db_types::persist::{
    ActorOrigin, CheckpointLocation, CommonFields, Extensions, IdAllocationEvent, IdSpace,
    RecordKey, RecordKind, RevPair, SessionCheckpointRecord, Watermark,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};

use crate::auth::{AuthError, Capability, Grants};
use crate::claims::{
    CommandError, InputError, InternalDefect, RequestMeta, admitted_bytes, admitted_string,
    admitted_vec, check_meta, ensure_import_ready, extent, folded_current, frame_and_digest,
    map_format, map_request, outcome_event, retry_outcome, revision_next, version_next,
};
use crate::session_intent::{
    self, CREATE_SESSION_CHECKPOINT, REVISE_SESSION_CHECKPOINT, SessionPayloadError, SessionState,
};

/// A caller-supplied session value this operation cannot accept, before anything is written.
///
/// These are session-family verdicts. Shape problems every command shares (read-set
/// length/key/revision, wrong key kind, stale baseline) are reported through the nested
/// [`CommandError::Input`] so one convention covers both families.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionInputError {
    /// Public checkpoint slug or session ordinal is invalid.
    InvalidLegacyLocation,
    /// The caller must re-read before computing the next session or replacing the slot.
    LegacyVersionChanged {
        /// The version pinned by the caller.
        expected: u64,
        /// The definitive current version.
        actual: u64,
    },
    /// Session ordinals start at 1; zero is not a session.
    ZeroSessionNo,
    /// The concept term does not exist on the current basis.
    MissingConcept(RecordKey),
    /// The concept term is retired; a session never resurrects it.
    ConceptRetired(RecordKey),
    /// The checkpoint does not exist on the current basis.
    MissingCheckpoint(RecordKey),
    /// A revision named a different immutable tuple member than the stored record.
    ImmutableField {
        /// Which member was contradicted.
        field: SessionField,
    },
    /// The session-checkpoint id space is exhausted.
    CheckpointIdExhausted,
    /// The next checkpoint id is already occupied.
    CheckpointIdCollision(RecordKey),
    /// A payload this module generated or was given is not a legal session payload.
    Payload(SessionPayloadError),
}

/// Which immutable member a revision contradicted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionField {
    /// The checkpoint's `concept`.
    Concept,
    /// The checkpoint's `session_no`.
    SessionNo,
}

/// A session-family defect that no caller input can cause.
///
/// Named separately from [`CommandError::Internal`] because it reports this family's own
/// read-back invariant rather than a shared writer arithmetic limit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionInternal {
    /// The canonical dispatcher read back a session image as another family.
    CopiedFamily,
}

/// Why a session command did not produce a commit or a typed read.
///
/// This is **not** the commit result: a successful path returns the real
/// [`HandleOutcome`], including `Unknown`, which is an ambiguous attempt and never a
/// fabricated error.
#[derive(Debug)]
pub enum SessionError {
    /// A shared admission, resource or store verdict, exactly as the claims writer reports it.
    Command(CommandError),
    /// A session-family caller-input verdict.
    Input(SessionInputError),
    /// A session-family internal invariant.
    Internal(SessionInternal),
}

impl From<CommandError> for SessionError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

impl From<InputError> for SessionError {
    fn from(error: InputError) -> Self {
        Self::Command(CommandError::Input(error))
    }
}

impl From<SessionInputError> for SessionError {
    fn from(error: SessionInputError) -> Self {
        Self::Input(error)
    }
}

impl From<SessionInternal> for SessionError {
    fn from(error: SessionInternal) -> Self {
        Self::Internal(error)
    }
}

impl From<AuthError> for SessionError {
    fn from(error: AuthError) -> Self {
        Self::Command(CommandError::Auth(error))
    }
}

impl From<Refusal> for SessionError {
    fn from(refusal: Refusal) -> Self {
        Self::Command(CommandError::Refused(refusal))
    }
}

impl From<BudgetError> for SessionError {
    fn from(error: BudgetError) -> Self {
        Self::Command(CommandError::Refused(Refusal::Budget(error)))
    }
}

impl From<sage_db_types::query::ControlStop> for SessionError {
    fn from(stop: sage_db_types::query::ControlStop) -> Self {
        Self::Command(CommandError::Refused(Refusal::Stopped(stop)))
    }
}

impl From<CommitReadError> for SessionError {
    fn from(error: CommitReadError) -> Self {
        Self::Command(CommandError::Basis(error))
    }
}

impl From<BlobReadError> for SessionError {
    fn from(error: BlobReadError) -> Self {
        Self::Command(CommandError::Blob(error))
    }
}

impl From<RecordReadError> for SessionError {
    fn from(error: RecordReadError) -> Self {
        Self::Command(CommandError::Record(error))
    }
}

impl From<sage_db_store::typed::ResultPreparationError> for SessionError {
    fn from(error: sage_db_store::typed::ResultPreparationError) -> Self {
        Self::Command(CommandError::ResultPreparation(error))
    }
}

impl From<SessionPayloadError> for SessionError {
    fn from(error: SessionPayloadError) -> Self {
        match error {
            SessionPayloadError::Refused(refusal) => Self::Command(CommandError::Refused(refusal)),
            // A generated payload whose own layout arithmetic disagreed with the bytes it
            // emitted is this module's invariant, not the caller's input.
            SessionPayloadError::InternalInvariant => Self::Command(CommandError::Internal(
                InternalDefect::PayloadLengthInvariant,
            )),
            other => Self::Input(SessionInputError::Payload(other)),
        }
    }
}

/// A typed session-checkpoint read: present with its verified content, or a proven absence.
///
/// Both alternatives name the one durable publication they were decided on, so a caller
/// cannot mistake a read of one basis for a read of another. The content's basis is that
/// same publication: the read asserts it before returning.
#[derive(Debug)]
pub enum SessionRead {
    /// Complete coverage establishes that this checkpoint is absent.
    Absent {
        /// The durable basis the absence was established on.
        basis: ViewBasis,
    },
    /// The checkpoint exists on this durable basis, with its committed content bytes.
    Found {
        /// Full after-image behind the SAME already-admitted shared owner.
        /// Borrow it through RecordImage::record(), retaining its reservation.
        record: sage_db_store::view::RecordImage,
        /// The content blob, verified on the same basis.
        content: CommittedBlob,
    },
}

impl SessionRead {
    /// The durable publication this read was decided on.
    #[must_use]
    pub fn basis(&self) -> ViewBasis {
        match self {
            Self::Absent { basis } => *basis,
            Self::Found { content, .. } => content.basis(),
        }
    }
}

/// Check one key's id range as caller input.
fn check_key(key: RecordKey) -> Result<(), SessionError> {
    if !key.id_is_in_range() {
        return Err(InputError::KeyOutOfRange { key }.into());
    }
    Ok(())
}

/// Require `key` to occupy a position of `required` kind.
fn check_kind(key: RecordKey, required: RecordKind) -> Result<(), SessionError> {
    check_key(key)?;
    if key.kind() != required {
        return Err(InputError::WrongKeyKind {
            expected: required,
            found: key.kind(),
        }
        .into());
    }
    Ok(())
}

/// Require the read set to be exactly `expected`, in that order, ascending and unique.
fn expect_read_set(read_set: &[RevPair], expected: &[RecordKey]) -> Result<(), SessionError> {
    if read_set.len() != expected.len() {
        return Err(InputError::ReadSetLength {
            expected: expected.len(),
            found: read_set.len(),
        }
        .into());
    }
    for (index, row) in read_set.iter().enumerate() {
        check_key(row.key)?;
        if row.revision.get() == 0 {
            return Err(InputError::ZeroRevision { index }.into());
        }
        if row.key != expected[index] {
            return Err(InputError::ReadSetKey {
                index,
                expected: expected[index],
                found: row.key,
            }
            .into());
        }
    }
    Ok(())
}

/// The caller's expected revision for `key` from an already validated read set.
///
/// `read_set` has been checked against the exact key list, so the row exists.
fn expected_revision(read_set: &[RevPair], key: RecordKey) -> RecordRevision {
    read_set
        .iter()
        .find(|row| row.key == key)
        .map_or(RecordRevision::FIRST, |row| row.revision)
}

/// The current revision of an active concept term, or the honest refusal.
fn active_concept(
    store: &Store<impl FileSystem>,
    concept: RecordKey,
    ledger: &mut dyn SharedLedger,
) -> Result<RecordRevision, SessionError> {
    let Some(record) = folded_current(store, concept, ledger)? else {
        return Err(SessionInputError::MissingConcept(concept).into());
    };
    let FoldedRecord::Term(term) = record else {
        return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
    };
    if term.retired {
        return Err(SessionInputError::ConceptRetired(concept).into());
    }
    Ok(term.revision)
}

/// Copy a session checkpoint through the canonical codec rather than by owning clone.
fn checkpoint_copy(
    record: &SessionCheckpointRecord,
    ledger: &mut dyn SharedLedger,
) -> Result<SessionCheckpointRecord, SessionError> {
    match map_format(typed::copy_payload(
        EventInput::SessionCheckpoint(record),
        ledger,
    ))? {
        typed::DispatchedPayload::SessionCheckpoint(owned) => Ok(owned),
        _ => Err(SessionInternal::CopiedFamily.into()),
    }
}

/// Read and canonically copy the checkpoint at `key`, or refuse its confirmed absence.
fn stored_checkpoint(
    store: &Store<impl FileSystem>,
    key: RecordKey,
    ledger: &mut dyn SharedLedger,
) -> Result<SessionCheckpointRecord, SessionError> {
    let Some(found) = folded_current(store, key, ledger)? else {
        return Err(SessionInputError::MissingCheckpoint(key).into());
    };
    let FoldedRecord::SessionCheckpoint(record) = found else {
        return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
    };
    checkpoint_copy(record, ledger)
}

/// Assemble the header fields shared by both writers here.
///
/// `tx_id` and `created_at` come from trusted executor metadata and are excluded from the
/// canonical request digest by construction: the digest is computed over the frame alone.
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
) -> Result<PreparedTransaction, SessionError> {
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
fn into_outcome(outcome: HandleOutcome) -> Result<HandleOutcome, SessionError> {
    match outcome {
        HandleOutcome::NotCommitted { reason } => Err(CommandError::Store(reason).into()),
        other => Ok(other),
    }
}

/// Persist a new session checkpoint as a committed `SessionCheckpointSaved`.
///
/// `read_set` must be exactly the current concept `Term` and its revision. Creation
/// allocates the next `SessionCheckpointId` from the actual committed high-water mark,
/// verifies the identity is free, and publishes the checkpoint, the allocation mark and
/// the request outcome in **one** transaction.
///
/// # Errors
///
/// [`SessionError`]: authorization (exactly `write_aux`), caller input (wrong key kind,
/// zero session number, stale concept revision, payload ceiling), incomplete authority, an
/// idempotency mismatch, a fenced handle, an exhausted or occupied checkpoint id, or a real
/// ledger/store refusal. An over-large content string is refused before any content
/// allocation. `Unknown` is returned as `Ok`.
#[allow(clippy::too_many_arguments)]
pub fn create_session_checkpoint<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    state: SessionState<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, SessionError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::WriteAux, ledger)?;
    check_meta(&meta, authorized.principal())?;
    check_kind(state.concept, RecordKind::Term)?;
    if state.session_no == 0 {
        return Err(SessionInputError::ZeroSessionNo.into());
    }
    let expected = [state.concept];
    expect_read_set(read_set, &expected)?;

    // The payload is prepared before the outcome lookup because its digest *is* the
    // idempotency key component: an exact retry must hash to the same bytes.
    let payload = session_intent::encode_create(&state, ledger)?;
    let (_frame, digest) = frame_and_digest(
        CREATE_SESSION_CHECKPOINT,
        session_intent::OPERATION_VERSION,
        &payload,
        read_set,
        ledger,
    )?;
    if let Some(retry) = retry_outcome(store, authorized.principal(), meta.request_id, digest)? {
        return Ok(retry);
    }
    if store.is_fenced() {
        return Err(CommandError::Fenced.into());
    }
    ensure_import_ready(store, ledger)?;

    let basis = store.commit_basis(IdSpace::SessionCheckpointId, ledger)?;

    // The concept must be current and active, and the caller's revision must be the
    // current one: a submission never silently rewrites a stale baseline.
    let concept_expected = expected_revision(read_set, state.concept);
    if active_concept(store, state.concept, ledger)? != concept_expected {
        return Err(InputError::StaleBaseline {
            key: state.concept,
            expected: concept_expected,
        }
        .into());
    }

    // Allocate from the actual committed mark, then check the identity is genuinely free.
    let checkpoint_id = basis
        .allocated_high_water
        .checked_add(1)
        .ok_or(SessionInputError::CheckpointIdExhausted)?;
    let checkpoint_key = RecordKey::new(RecordKind::SessionCheckpoint, checkpoint_id);
    if folded_current(store, checkpoint_key, ledger)?.is_some() {
        return Err(SessionInputError::CheckpointIdCollision(checkpoint_key).into());
    }

    let content_ref = map_format(typed::raw_hash(state.content.as_bytes(), ledger))?;
    let content_bytes = admitted_bytes(state.content.as_bytes(), ledger)?;
    let version = version_next(store.head())?;

    let checkpoint = SessionCheckpointRecord {
        key: checkpoint_key,
        revision: RecordRevision::FIRST,
        location: sage_db_types::persist::CheckpointLocation::Term {
            concept: state.concept,
            session_no: state.session_no,
        },
        content_ref,
        common: CommonFields {
            recorded_by: Some(admitted_string(authorized.principal(), ledger)?),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: meta.created_at,
            updated_at: None,
        },
        extensions: Extensions::new(),
        changed_fields: None,
    };
    let allocation = IdAllocationEvent {
        allocation: Watermark {
            space: IdSpace::SessionCheckpointId,
            hwm: checkpoint_id,
        },
        reason: None,
        extensions: Extensions::new(),
    };

    persist_checkpoint(
        store,
        authorized.principal(),
        &meta,
        digest,
        version,
        &checkpoint,
        content_bytes,
        Some(&allocation),
        basis,
        None,
        limits,
        ledger,
    )
}

/// Replace the content of an existing session checkpoint.
///
/// `read_set` must be exactly the concept `Term` and the checkpoint, in canonical ascending
/// order, with both current revisions. `concept` and `session_no` are immutable: the state
/// names them for verification only. The record's creation audit, legacy metadata,
/// extensions and advisory `changed_fields` are preserved verbatim; only the revision, the
/// content reference, the current actor and the update time move.
///
/// # Errors
///
/// As [`create_session_checkpoint`], plus a missing checkpoint, a contradicted immutable
/// tuple member, and a stale either-way CAS. No id allocation happens on a revision.
#[allow(clippy::too_many_arguments)]
pub fn revise_session_checkpoint<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    checkpoint: RecordKey,
    state: SessionState<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, SessionError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::WriteAux, ledger)?;
    check_meta(&meta, authorized.principal())?;
    check_kind(checkpoint, RecordKind::SessionCheckpoint)?;
    check_kind(state.concept, RecordKind::Term)?;
    if state.session_no == 0 {
        return Err(SessionInputError::ZeroSessionNo.into());
    }
    // Canonical ascending order, and no whole-version pin: this is an object CAS.
    let expected = if checkpoint <= state.concept {
        [checkpoint, state.concept]
    } else {
        [state.concept, checkpoint]
    };
    expect_read_set(read_set, &expected)?;

    let payload = session_intent::encode_revise(checkpoint, &state, ledger)?;
    let (_frame, digest) = frame_and_digest(
        REVISE_SESSION_CHECKPOINT,
        session_intent::OPERATION_VERSION,
        &payload,
        read_set,
        ledger,
    )?;
    if let Some(retry) = retry_outcome(store, authorized.principal(), meta.request_id, digest)? {
        return Ok(retry);
    }
    if store.is_fenced() {
        return Err(CommandError::Fenced.into());
    }
    ensure_import_ready(store, ledger)?;

    let basis = store.commit_basis(IdSpace::SessionCheckpointId, ledger)?;

    // Both current reads, then the exact two-object CAS before anything is derived.
    let stored = stored_checkpoint(store, checkpoint, ledger)?;
    let checkpoint_expected = expected_revision(read_set, checkpoint);
    if stored.revision != checkpoint_expected {
        return Err(InputError::StaleBaseline {
            key: checkpoint,
            expected: checkpoint_expected,
        }
        .into());
    }
    let concept_expected = expected_revision(read_set, state.concept);
    if active_concept(store, state.concept, ledger)? != concept_expected {
        return Err(InputError::StaleBaseline {
            key: state.concept,
            expected: concept_expected,
        }
        .into());
    }

    // Identity is not content: the tuple is immutable for the record's whole life.
    let CheckpointLocation::Term {
        concept,
        session_no,
    } = stored.location
    else {
        return Err(SessionInputError::ImmutableField {
            field: SessionField::Concept,
        }
        .into());
    };
    if concept != state.concept {
        return Err(SessionInputError::ImmutableField {
            field: SessionField::Concept,
        }
        .into());
    }
    if session_no != state.session_no {
        return Err(SessionInputError::ImmutableField {
            field: SessionField::SessionNo,
        }
        .into());
    }

    let content_ref = map_format(typed::raw_hash(state.content.as_bytes(), ledger))?;
    let content_bytes = admitted_bytes(state.content.as_bytes(), ledger)?;
    let version = version_next(store.head())?;

    // A full canonical copy, then only the permitted fields move.
    let mut next = stored;
    next.revision = revision_next(next.revision)?;
    next.content_ref = content_ref;
    next.common.recorded_by = Some(admitted_string(authorized.principal(), ledger)?);
    next.common.actor_origin = ActorOrigin::Authenticated;
    next.common.updated_at = Some(meta.created_at);
    persist_checkpoint(
        store,
        authorized.principal(),
        &meta,
        digest,
        version,
        &next,
        content_bytes,
        None,
        basis,
        None,
        limits,
        ledger,
    )
}

#[allow(clippy::too_many_arguments)]
fn persist_checkpoint(
    store: &mut Store<impl FileSystem>,
    principal: &str,
    meta: &RequestMeta<'_>,
    digest: [u8; 32],
    version: u64,
    checkpoint: &SessionCheckpointRecord,
    content_bytes: Vec<u8>,
    allocation: Option<&IdAllocationEvent>,
    basis: CommitBasis,
    expected_version: Option<u64>,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, SessionError> {
    let affected = [RevPair {
        key: checkpoint.key,
        revision: checkpoint.revision,
    }];
    let counters = allocation.map(|event| event.allocation);
    let result_bytes = typed::result_payload(version, &affected, counters.as_slice(), ledger)?;
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

    let mut events: Vec<OwnedEvent> = admitted_vec(2 + usize::from(allocation.is_some()), ledger)?;
    events.push(map_format(typed::prepare_event(
        EventInput::SessionCheckpoint(checkpoint),
        ledger,
    ))?);
    if let Some(allocation) = allocation {
        events.push(map_format(typed::prepare_event(
            EventInput::IdAllocation(allocation),
            ledger,
        ))?);
    }
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
        sha256: checkpoint.content_ref,
        declared_len: extent(content_bytes.len())?,
        source: BlobSource::Provide(content_bytes),
    });
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: extent(result_bytes.len())?,
        source: BlobSource::Provide(result_bytes),
    });

    let mut prepared = header(
        store,
        principal,
        meta,
        digest,
        version,
        events,
        blobs,
        basis.rules,
        ledger,
    )?;
    prepared.expect_version = expected_version;
    into_outcome(store.commit(prepared, limits, ledger))
}

/// The public checkpoint slot and its opaque, Bun-serialized state.
pub struct LegacyCheckpointWrite<'a> {
    /// Valid public slug, including one that has no Term.
    pub slug: &'a str,
    /// Positive JavaScript-safe session ordinal.
    pub session_no: u64,
    /// Entire opaque checkpoint document.
    pub content: &'a [u8],
    /// Definitive version used to read or allocate the slot.
    pub expected_version: u64,
}

/// Operation kind 16 binds a slug, ordinal, and physical content digest to a version.
pub const UPSERT_LEGACY_CHECKPOINT: u16 = 16;

fn checkpoint_scan_error(error: RecordScanError<Refusal>) -> SessionError {
    match error {
        RecordScanError::Incomplete(reason) => CommandError::Incomplete(reason).into(),
        RecordScanError::Read(error) => CommandError::from(error).into(),
        RecordScanError::Visitor(refusal) => refusal.into(),
    }
}

fn valid_legacy_location(slug: &str, ordinal: u64) -> bool {
    const FIXED_PAYLOAD_BYTES: u32 = 4 + 8 + 32;
    u32::try_from(slug.len()).is_ok_and(|len| len <= typed::MAX_PAYLOAD - FIXED_PAYLOAD_BYTES)
        && CheckpointLocation::valid_public_parts(slug, ordinal)
}

fn charged_slug_eq(
    candidate: &str,
    requested: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<bool, Refusal> {
    ledger.checkpoint().map_err(Refusal::Stopped)?;
    if candidate.len() != requested.len() {
        return Ok(false);
    }
    for bytes in candidate
        .as_bytes()
        .chunks(256)
        .zip(requested.as_bytes().chunks(256))
    {
        let (left, right) = bytes;
        let count = left.len() as u64;
        ledger.try_charge_entries(count).map_err(Refusal::Budget)?;
        ledger.checkpoint().map_err(Refusal::Stopped)?;
        if left != right {
            return Ok(false);
        }
    }
    Ok(true)
}

fn locate_legacy_checkpoint(
    store: &Store<impl FileSystem>,
    slug: &str,
    ordinal: Option<u64>,
    ledger: &mut dyn LeaseLedger,
) -> Result<LegacyCheckpointSlot, SessionError> {
    let mut term = None;
    let expected_basis = store
        .scan_identified_records(ledger, |record, ledger| {
            if let FoldedRecord::Term(row) = record
                && !row.retired
                && charged_slug_eq(&row.slug, slug, ledger)?
            {
                term = Some(row.key);
            }
            Ok(())
        })
        .map_err(checkpoint_scan_error)?;

    let mut latest: Option<(u64, bool, RecordKey)> = None;
    let basis = store
        .scan_identified_records(ledger, |record, ledger| {
            let FoldedRecord::SessionCheckpoint(row) = record else {
                return Ok(());
            };
            let candidate = match &row.location {
                CheckpointLocation::Slug {
                    slug: stored,
                    session_no,
                } if ordinal.is_none_or(|value| value == *session_no)
                    && charged_slug_eq(stored, slug, ledger)? =>
                {
                    Some((*session_no, true, row.key))
                }
                CheckpointLocation::Term {
                    concept,
                    session_no,
                } if Some(*concept) == term
                    && ordinal.is_none_or(|value| value == u64::from(*session_no)) =>
                {
                    Some((u64::from(*session_no), false, row.key))
                }
                _ => None,
            };
            if let Some(candidate) = candidate
                && latest.is_none_or(|current| candidate > current)
            {
                latest = Some(candidate);
            }
            Ok(())
        })
        .map_err(checkpoint_scan_error)?;
    if expected_basis != basis {
        return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
    }
    Ok(LegacyCheckpointSlot {
        basis,
        selected: latest.map(|(ordinal, _, key)| (ordinal, key)),
    })
}

/// Latest or selected public checkpoint key and the durable observation used to find it.
pub struct LegacyCheckpointSlot {
    /// Durable publication used for selection, including when no slot exists.
    pub basis: ViewBasis,
    /// Session ordinal and checkpoint key, when found.
    pub selected: Option<(u64, RecordKey)>,
}

/// Find one public slot, or the latest slot for a slug.
///
/// # Errors
///
/// An unauthorized, invalid, incomplete, or refused lookup.
pub fn read_legacy_checkpoint_slot(
    store: &Store<impl FileSystem>,
    grants: &Grants<'_>,
    slug: &str,
    ordinal: Option<u64>,
    ledger: &mut dyn LeaseLedger,
) -> Result<LegacyCheckpointSlot, SessionError> {
    grants.authorize(store.kb_uuid(), Capability::Read, ledger)?;
    if !valid_legacy_location(slug, ordinal.unwrap_or(1)) {
        return Err(SessionInputError::InvalidLegacyLocation.into());
    }
    ensure_import_ready(store, ledger)?;
    locate_legacy_checkpoint(store, slug, ordinal, ledger)
}

/// Atomically replace or create the addressed checkpoint on the caller's observed basis.
///
/// An exact replay answers before the whole-version comparison. Bun can derive the next
/// ordinal from a slot read and retry a novel request after an intervening publication.
///
/// # Errors
///
/// An unauthorized, invalid, obsolete, fenced, or refused write.
pub fn upsert_legacy_checkpoint(
    store: &mut Store<impl FileSystem>,
    grants: &Grants<'_>,
    meta: RequestMeta<'_>,
    write: LegacyCheckpointWrite<'_>,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, SessionError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::WriteAux, ledger)?;
    check_meta(&meta, authorized.principal())?;
    let (slug, ordinal) = (write.slug, write.session_no);
    if !valid_legacy_location(slug, ordinal) || write.expected_version == 0 {
        return Err(SessionInputError::InvalidLegacyLocation.into());
    }
    let content_ref = map_format(typed::raw_hash(write.content, ledger))?;
    let overhead = 4 + 8 + content_ref.len();
    let length = slug
        .len()
        .checked_add(overhead)
        .ok_or(CommandError::Internal(InternalDefect::ExtentOverflow))?;
    let slug_len = u32::try_from(slug.len())
        .map_err(|_| CommandError::Internal(InternalDefect::ExtentOverflow))?;
    let mut payload = admitted_vec(length, ledger)?;
    payload.extend_from_slice(&slug_len.to_le_bytes());
    payload.extend_from_slice(slug.as_bytes());
    payload.extend_from_slice(&ordinal.to_le_bytes());
    payload.extend_from_slice(&content_ref);
    let ops = [typed::RequestOp::new(UPSERT_LEGACY_CHECKPOINT, 1, &payload)];
    let frame = typed::RequestFrame::new(&ops, &[], Some(write.expected_version));
    let bytes = map_request(typed::encode_request(&frame, ledger))?;
    let digest = map_request(typed::request_digest_of(&bytes, ledger))?;
    if let Some(retry) = retry_outcome(store, authorized.principal(), meta.request_id, digest)? {
        return Ok(retry);
    }
    if store.is_fenced() {
        return Err(CommandError::Fenced.into());
    }
    ensure_import_ready(store, ledger)?;
    let basis = store.commit_basis(IdSpace::SessionCheckpointId, ledger)?;
    if basis.basis.version.get() != write.expected_version {
        return Err(SessionInputError::LegacyVersionChanged {
            expected: write.expected_version,
            actual: basis.basis.version.get(),
        }
        .into());
    }
    let found = locate_legacy_checkpoint(store, slug, Some(ordinal), ledger)?;
    if found.basis != basis.basis {
        return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
    }
    let (checkpoint, allocation) = if let Some((_, key)) = found.selected {
        let mut checkpoint = stored_checkpoint(store, key, ledger)?;
        checkpoint.revision = revision_next(checkpoint.revision)?;
        checkpoint.content_ref = content_ref;
        checkpoint.common.recorded_by = Some(admitted_string(authorized.principal(), ledger)?);
        checkpoint.common.actor_origin = ActorOrigin::Authenticated;
        checkpoint.common.updated_at = Some(meta.created_at);
        (checkpoint, None)
    } else {
        let hwm = basis
            .allocated_high_water
            .checked_add(1)
            .ok_or(SessionInputError::CheckpointIdExhausted)?;
        let key = RecordKey::new(RecordKind::SessionCheckpoint, hwm);
        if folded_current(store, key, ledger)?.is_some() {
            return Err(SessionInputError::CheckpointIdCollision(key).into());
        }
        let checkpoint = SessionCheckpointRecord {
            key,
            revision: RecordRevision::FIRST,
            location: CheckpointLocation::Slug {
                slug: admitted_string(slug, ledger)?,
                session_no: ordinal,
            },
            content_ref,
            common: CommonFields {
                recorded_by: Some(admitted_string(authorized.principal(), ledger)?),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: meta.created_at,
                updated_at: None,
            },
            extensions: Extensions::new(),
            changed_fields: None,
        };
        let allocation = IdAllocationEvent {
            allocation: Watermark {
                space: IdSpace::SessionCheckpointId,
                hwm,
            },
            reason: None,
            extensions: Extensions::new(),
        };
        (checkpoint, Some(allocation))
    };
    let content_bytes = admitted_bytes(write.content, ledger)?;
    let version = version_next(store.head())?;
    persist_checkpoint(
        store,
        authorized.principal(),
        &meta,
        digest,
        version,
        &checkpoint,
        content_bytes,
        allocation.as_ref(),
        basis,
        Some(write.expected_version),
        limits,
        ledger,
    )
}

/// Read one session checkpoint and its content on a single durable basis.
///
/// The capability checked is exactly [`Capability::Read`]; `write_aux` does not imply it and
/// no other capability is consulted. Both presence and absence require the store's full
/// authority, durability and coverage, so an unverified view never answers "absent". The
/// record is copied through the canonical dispatcher and the content is the store's leased,
/// hash-verified [`CommittedBlob`], read on the **same** basis the record came from; a
/// mismatch is reported as a basis divergence rather than papered over. `max_content_len`
/// is the caller's ceiling: an over-long committed blob is a `BlobReadError::Ceiling`, never
/// a truncated answer.
///
/// # Errors
///
/// [`SessionError`]: authorization, a non-checkpoint key, incomplete authority, a record or
/// blob read refusal (including the content ceiling), confirmed committed damage, or a real
/// ledger refusal.
pub fn read_session_checkpoint<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    key: RecordKey,
    max_content_len: u64,
    ledger: &mut dyn LeaseLedger,
) -> Result<SessionRead, SessionError> {
    grants.authorize(store.kb_uuid(), Capability::Read, ledger)?;
    ensure_import_ready(store, ledger)?;
    check_kind(key, RecordKind::SessionCheckpoint)?;

    use sage_db_store::view::RecordImageLookup;
    let (record, basis) = match store.record_owned(key, ledger)? {
        RecordImageLookup::Found { image, basis } => (image, basis),
        RecordImageLookup::Absent { basis } => return Ok(SessionRead::Absent { basis }),
        RecordImageLookup::Incomplete(reason) => {
            return Err(CommandError::Incomplete(reason).into());
        }
    };
    let FoldedRecord::SessionCheckpoint(checkpoint) = record.record() else {
        return Err(SessionInternal::CopiedFamily.into());
    };
    let content = store.read_committed_blob(checkpoint.content_ref, max_content_len, ledger)?;
    if content.basis() != basis {
        return Err(CommandError::BasisDivergence.into());
    }
    Ok(SessionRead::Found { record, content })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_set_must_be_exactly_the_named_keys_in_order() {
        let concept = RecordKey::term(4);
        let row = RevPair {
            key: concept,
            revision: RecordRevision::FIRST,
        };
        assert!(expect_read_set(&[row], &[concept]).is_ok());
        assert_eq!(expected_revision(&[row], concept), RecordRevision::FIRST);
        assert!(matches!(
            expect_read_set(&[], &[concept]),
            Err(SessionError::Command(CommandError::Input(
                InputError::ReadSetLength { .. }
            )))
        ));
        let mut both = [row, row];
        both[1].key = RecordKey::new(RecordKind::Claim, 1);
        assert!(matches!(
            expect_read_set(&both, &[concept, RecordKey::term(9)]),
            Err(SessionError::Command(CommandError::Input(
                InputError::ReadSetKey { index: 1, .. }
            )))
        ));
    }

    #[test]
    fn wrong_key_kinds_are_refused_before_any_read() {
        assert!(matches!(
            check_kind(RecordKey::term(1), RecordKind::SessionCheckpoint),
            Err(SessionError::Command(CommandError::Input(
                InputError::WrongKeyKind { .. }
            )))
        ));
        assert!(check_kind(RecordKey::term(1), RecordKind::Term).is_ok());
    }

    #[test]
    fn a_generated_layout_invariant_is_internal_not_caller_input() {
        assert!(matches!(
            SessionError::from(SessionPayloadError::InternalInvariant),
            SessionError::Command(CommandError::Internal(
                InternalDefect::PayloadLengthInvariant
            ))
        ));
        assert!(matches!(
            SessionError::from(SessionPayloadError::Truncated),
            SessionError::Input(SessionInputError::Payload(SessionPayloadError::Truncated))
        ));
    }
}
