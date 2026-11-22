//! Auxiliary frontier-entry lifecycle: create, revise and read.
//!
//! # What this module is
//!
//! A [`FrontierEntryRecord`] is an **auxiliary note about unexplored territory** — the
//! structured form of a `frontier.md` row. It is not graph content, not a claim, and not a
//! proposal. This module is the only writer and reader of that family, and composes, per
//! command:
//!
//! 1. exactly one explicit capability ([`Capability::WriteAux`] to write,
//!    [`Capability::Read`] to read — neither implies the other);
//! 2. caller-input validation (identity, key kinds, the reference list, read-set shape);
//! 3. the canonical P1 request frame (one operation, the caller's read set, no expected
//!    whole-KB version) and its `request`-domain digest;
//! 4. **idempotency first**: an exact retry of an already-committed request returns the
//!    original outcome before any current-state or collision check runs;
//! 5. the write fence, then the real `Store::commit_basis`;
//! 6. the authoritative current reads the decision needs (active terms, exact CAS, the
//!    immutability of nothing — every listed field of a frontier entry is revisable);
//! 7. one `PreparedTransaction` and the real `Store::commit`.
//!
//! # Identity, not content uniqueness
//!
//! A frontier entry's `RecordKey` **is** its identity. Creation allocates the next
//! `FrontierEntryId` from the actual committed high-water mark and verifies the key is free.
//! A revision never re-derives its key.
//!
//! # What is deliberately not claimed
//!
//! No query API, no enumeration, no graph mutation, no proposal semantics, no platform
//! durability. `HandleOutcome::Unknown` is returned as the ambiguous attempt it is; the
//! caller resolves before retrying, and this module never clears a fence.

use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::lease::LeaseLedger;
use sage_db_store::typed::{self, EventInput};
use sage_db_store::view::{CommittedBlob, RecordImage, ViewBasis};
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ActorOrigin, CommonFields, Extensions, FrontierEntryRecord, FrontierStatus, IdAllocationEvent,
    IdSpace, RecordKey, RecordKind, RevPair, Watermark,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};

use crate::auth::{AuthError, Capability, Grants};
use crate::claims::{
    CommandError, InputError, InternalDefect, RequestMeta, admitted_bytes, admitted_string,
    admitted_vec, check_meta, ensure_import_ready, extent, folded_current, frame_and_digest,
    map_format, outcome_event, retry_outcome, revision_next, version_next,
};
/// Bytes of an encoded `RecordKey` prefix a revise payload carries before the state.
const KEY_PREFIX_BYTES: usize = 18;

use crate::frontier_intent::{
    self, CREATE_FRONTIER, FrontierPayloadError, FrontierState, REVISE_FRONTIER,
};

/// A caller-supplied frontier value this operation cannot accept, before anything is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrontierInputError {
    /// A listed concept reference does not exist on the current basis.
    MissingConcept(RecordKey),
    /// A listed concept reference is retired; a frontier entry never resurrects it.
    ConceptRetired(RecordKey),
    /// The frontier entry does not exist on the current basis.
    MissingFrontier(RecordKey),
    /// The frontier-entry id space is exhausted.
    FrontierIdExhausted,
    /// The next frontier id is already occupied.
    FrontierIdCollision(RecordKey),
    /// A payload this module generated or was given is not a legal frontier payload.
    Payload(FrontierPayloadError),
}

/// A frontier-family defect that no caller input can cause.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrontierInternal {
    /// The canonical dispatcher read back a frontier image as another family.
    CopiedFamily,
    /// A validated read set did not contain the key the command requires.
    ///
    /// Unreachable through the public entry points, which check the exact key list first.
    /// It exists so a missing row is reported instead of being silently treated as `FIRST`.
    ReadSetInvariant(RecordKey),
}

/// Why a frontier command did not produce a commit or a typed read.
///
/// Deliberately the same shape as [`crate::session::SessionError`]: a shared admission,
/// resource or store verdict travels in [`CommandError`] — including record and blob read
/// refusals — while this family adds only its own caller-input and internal arms.
#[derive(Debug)]
pub enum FrontierError {
    /// A shared admission, resource or store verdict.
    Command(CommandError),
    /// A frontier-family caller-input verdict.
    Input(FrontierInputError),
    /// A frontier-family internal invariant.
    Internal(FrontierInternal),
}

impl From<CommandError> for FrontierError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

impl From<InputError> for FrontierError {
    fn from(error: InputError) -> Self {
        Self::Command(CommandError::Input(error))
    }
}

impl From<FrontierInputError> for FrontierError {
    fn from(error: FrontierInputError) -> Self {
        Self::Input(error)
    }
}

impl From<FrontierInternal> for FrontierError {
    fn from(error: FrontierInternal) -> Self {
        Self::Internal(error)
    }
}

impl From<AuthError> for FrontierError {
    fn from(error: AuthError) -> Self {
        Self::Command(error.into())
    }
}

impl From<Refusal> for FrontierError {
    fn from(refusal: Refusal) -> Self {
        Self::Command(refusal.into())
    }
}

impl From<FrontierPayloadError> for FrontierError {
    fn from(error: FrontierPayloadError) -> Self {
        match error {
            FrontierPayloadError::Refused(refusal) => Self::Command(refusal.into()),
            other => Self::Input(FrontierInputError::Payload(other)),
        }
    }
}

/// A completed frontier read, or a confirmed absence, on one durable basis.
pub enum FrontierRead {
    /// Complete coverage establishes that this entry is absent.
    Absent {
        /// The durable basis the absence was established on.
        basis: ViewBasis,
    },
    /// The entry exists on this durable basis, with its committed text bytes.
    Found {
        /// Full after-image behind the SAME already-admitted shared owner.
        record: RecordImage,
        /// The text blob, verified on the same basis.
        text: CommittedBlob,
    },
}

/// Reject a key whose kind is not the one this position requires.
fn check_kind(key: RecordKey, required: RecordKind) -> Result<(), FrontierError> {
    if key.kind() != required {
        return Err(CommandError::Input(InputError::WrongKeyKind {
            expected: required,
            found: key.kind(),
        })
        .into());
    }
    Ok(())
}

/// Reject a read set that is not exactly `expected`, in that order, at its current revision.
///
/// `expected` is an iterator so the caller's list is checked without materialising a second
/// copy of it: the whole point of the preflight is that no allocation happens before it.
fn expect_read_set(
    read_set: &[RevPair],
    expected: impl Iterator<Item = RecordKey>,
    expected_len: usize,
) -> Result<(), FrontierError> {
    if read_set.len() != expected_len {
        return Err(CommandError::Input(InputError::ReadSetLength {
            expected: expected_len,
            found: read_set.len(),
        })
        .into());
    }
    for (index, (row, key)) in read_set.iter().zip(expected).enumerate() {
        if row.key != key {
            return Err(CommandError::Input(InputError::ReadSetKey {
                index,
                expected: key,
                found: row.key,
            })
            .into());
        }
    }
    Ok(())
}

/// The caller's expected revision for `key` from an already validated read set.
///
/// A miss is **not** "revision 1". `expect_read_set` has already established that the set is
/// exactly the required keys, so a miss here means that invariant was violated — reported as
/// an internal defect rather than by inventing a baseline the caller never stated.
fn expected_revision(
    read_set: &[RevPair],
    key: RecordKey,
) -> Result<RecordRevision, FrontierError> {
    read_set
        .iter()
        .find(|row| row.key == key)
        .map(|row| row.revision)
        .ok_or_else(|| FrontierInternal::ReadSetInvariant(key).into())
}

/// Verify a listed concept is current and active, and that the caller pinned its revision.
fn require_active_term(
    store: &Store<impl FileSystem>,
    key: RecordKey,
    read_set: &[RevPair],
    ledger: &mut dyn SharedLedger,
) -> Result<(), FrontierError> {
    let Some(record) = folded_current(store, key, ledger)? else {
        return Err(FrontierInputError::MissingConcept(key).into());
    };
    let FoldedRecord::Term(term) = record else {
        return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
    };
    if term.retired {
        return Err(FrontierInputError::ConceptRetired(key).into());
    }
    let expected = expected_revision(read_set, key)?;
    if term.revision != expected {
        return Err(CommandError::Input(InputError::StaleBaseline { key, expected }).into());
    }
    Ok(())
}

/// Copy a frontier entry through the canonical codec rather than by owning clone.
fn frontier_copy(
    record: &FrontierEntryRecord,
    ledger: &mut dyn SharedLedger,
) -> Result<FrontierEntryRecord, FrontierError> {
    match map_format(typed::copy_payload(EventInput::Frontier(record), ledger))? {
        typed::DispatchedPayload::FrontierEntry(owned) => Ok(owned),
        _ => Err(FrontierInternal::CopiedFamily.into()),
    }
}

/// Read and canonically copy the frontier entry at `key`, or refuse its confirmed absence.
fn stored_frontier(
    store: &Store<impl FileSystem>,
    key: RecordKey,
    ledger: &mut dyn SharedLedger,
) -> Result<FrontierEntryRecord, FrontierError> {
    let Some(found) = folded_current(store, key, ledger)? else {
        return Err(FrontierInputError::MissingFrontier(key).into());
    };
    let FoldedRecord::FrontierEntry(record) = found else {
        return Err(CommandError::Internal(InternalDefect::EncodeDecodeDivergence).into());
    };
    frontier_copy(record, ledger)
}

/// Assemble the header fields shared by both writers here.
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
) -> Result<PreparedTransaction, FrontierError> {
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
fn into_outcome(outcome: HandleOutcome) -> Result<HandleOutcome, FrontierError> {
    match outcome {
        HandleOutcome::NotCommitted { reason } => Err(CommandError::Store(reason).into()),
        other => Ok(other),
    }
}

/// The full allocation-free preflight both commands run before anything else.
///
/// Order matters: the shared state check (count, per-ref kind, strict ascending uniqueness,
/// priority and status domains) runs **first**, then the encoded-size ceiling. Neither
/// allocates, so an oversized or unsorted list is refused before any payload or table
/// allocation and before any ledger admission.
fn preflight(state: &FrontierState<'_>, extra_prefix: usize) -> Result<(), FrontierError> {
    // Count and ceiling first (both allocation-free), then per-key kind so a caller who passed a
    // non-Term reference gets the key-kind refusal rather than a payload-shape verdict.
    frontier_intent::check_shape(state)?;
    for key in state.concept_refs {
        check_kind(*key, RecordKind::Term)?;
    }
    frontier_intent::check_state(state)?;
    let state_bytes = frontier_intent::state_len(state)?;
    let total = state_bytes
        .checked_add(extra_prefix)
        .ok_or(FrontierError::Input(FrontierInputError::Payload(
            FrontierPayloadError::LengthOverflow,
        )))?;
    frontier_intent::check_ceiling(total as u64)?;
    Ok(())
}

/// The exact read set a create must carry: precisely the listed refs, in the caller's order.
///
/// Borrowed, never collected. The caller's list has already been validated as strictly
/// ascending and unique, so no sorting, de-duplication or normalisation is performed here or
/// anywhere else: normalising would silently accept a list the caller stated wrongly.
fn create_read_set_keys<'a>(state: &FrontierState<'a>) -> impl Iterator<Item = RecordKey> + 'a {
    state.concept_refs.iter().copied()
}

/// The exact read set a revise must carry: the listed refs first, then the entry.
///
/// Term keys precede the `FrontierEntry` key because `RecordKind::Term` (1) sorts before
/// `RecordKind::FrontierEntry` (8), which is the canonical key order. Borrowed for the same
/// reason as [`create_read_set_keys`].
fn revise_read_set_keys<'a>(
    frontier: RecordKey,
    state: &FrontierState<'a>,
) -> impl Iterator<Item = RecordKey> + 'a {
    state
        .concept_refs
        .iter()
        .copied()
        .chain(core::iter::once(frontier))
}

/// Persist a new frontier entry as a committed `FrontierEntryChanged`.
///
/// `read_set` must be exactly the listed concept terms, ascending, each at its current
/// revision. Creation allocates the next `FrontierEntryId` from the actual committed
/// high-water mark, verifies the identity is free, and publishes the entry, the allocation
/// mark and the request outcome in **one** transaction.
///
/// # Errors
///
/// [`FrontierError`]: authorization (exactly `write_aux`), caller input (wrong key kind,
/// unsorted or non-term refs, a stale or missing term, payload ceiling), incomplete
/// authority, an idempotency mismatch, a fenced handle, an exhausted or occupied id, or a
/// real ledger/store refusal. `Unknown` is returned as `Ok`.
#[allow(clippy::too_many_arguments)]
pub fn create_frontier<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    state: FrontierState<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, FrontierError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::WriteAux, ledger)?;
    check_meta(&meta, authorized.principal())?;
    // Allocation-free preflight: count, kinds, ordering and domains, then the ceiling. An
    // unsorted, duplicated or oversized list is refused here — before any payload or table
    // allocation and before the read-set comparison needs anything materialised.
    preflight(&state, 0)?;
    let expected = create_read_set_keys(&state);
    expect_read_set(read_set, expected, state.concept_refs.len())?;

    // The payload is prepared before the outcome lookup because its digest *is* the
    // idempotency key component: an exact retry must hash to the same bytes.
    let payload = frontier_intent::encode_create(&state, ledger)?;
    let (_frame, digest) = frame_and_digest(
        CREATE_FRONTIER,
        frontier_intent::OPERATION_VERSION,
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

    let basis = store
        .commit_basis(IdSpace::FrontierEntryId, ledger)
        .map_err(CommandError::from)?;

    // Every listed concept must be current, active, and pinned at its current revision.
    for key in state.concept_refs {
        require_active_term(store, *key, read_set, ledger)?;
    }

    // Allocate from the actual committed mark, then check the identity is genuinely free.
    let frontier_id = basis
        .allocated_high_water
        .checked_add(1)
        .ok_or(FrontierInputError::FrontierIdExhausted)?;
    let frontier_key = RecordKey::new(RecordKind::FrontierEntry, frontier_id);
    if folded_current(store, frontier_key, ledger)?.is_some() {
        return Err(FrontierInputError::FrontierIdCollision(frontier_key).into());
    }

    let text_ref = map_format(typed::raw_hash(state.text.as_bytes(), ledger))?;
    let text_bytes = admitted_bytes(state.text.as_bytes(), ledger)?;
    let version = version_next(store.head())?;

    let entry = FrontierEntryRecord {
        key: frontier_key,
        revision: RecordRevision::FIRST,
        status: status_of(state.status)?,
        // Reserve the slot extent first, then copy: the reservation is charged once, and
        // the copy cannot outgrow it.
        concept_refs: {
            let mut refs = admitted_vec(state.concept_refs.len(), ledger)?;
            refs.extend_from_slice(state.concept_refs);
            refs
        },
        text_ref,
        priority: state.priority,
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
            space: IdSpace::FrontierEntryId,
            hwm: frontier_id,
        },
        reason: None,
        extensions: Extensions::new(),
    };

    let affected = [RevPair {
        key: frontier_key,
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
        EventInput::Frontier(&entry),
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
        sha256: text_ref,
        declared_len: extent(text_bytes.len())?,
        source: BlobSource::Provide(text_bytes),
    });
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: extent(result_bytes.len())?,
        source: BlobSource::Provide(result_bytes),
    });

    let prepared = header(
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
    into_outcome(store.commit(prepared, limits, ledger))
}

/// Map the caller's numeric status onto the closed enum.
fn status_of(status: u16) -> Result<FrontierStatus, FrontierError> {
    FrontierStatus::from_u16(status).ok_or(FrontierError::Input(FrontierInputError::Payload(
        FrontierPayloadError::UnknownStatus(status),
    )))
}

/// Replace the complete state of an existing frontier entry.
///
/// `read_set` must be exactly the entry plus every NEW listed concept, ascending, at current
/// revisions. Removed references do not require a current term. The record's creation audit,
/// legacy metadata and extensions are preserved verbatim; only the revision, the replaced
/// state, the current actor and the update time move.
///
/// # Audit on a revision
///
/// What a revision must carry is a **new authenticated audit and an update time** — both are
/// set here from the authorized principal and the request metadata. `CommonFields.reason` is
/// **optional** and is preserved, not required and not invented: the operation payload has no
/// reason field, and the existing session writer treats it the same way. A writer that
/// demanded a reason would have to fabricate one or change the payload, and neither is
/// acceptable.
///
/// # Errors
///
/// As [`create_frontier`], plus a missing entry, a stale either-way CAS, and a real
/// ledger/store refusal. No id allocation happens on a revision.
#[allow(clippy::too_many_arguments)]
pub fn revise_frontier<'a, F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'a>,
    meta: RequestMeta<'_>,
    frontier: RecordKey,
    state: FrontierState<'_>,
    read_set: &[RevPair],
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<HandleOutcome, FrontierError> {
    let authorized = grants.authorize(store.kb_uuid(), Capability::WriteAux, ledger)?;
    check_meta(&meta, authorized.principal())?;
    check_kind(frontier, RecordKind::FrontierEntry)?;
    // Same allocation-free preflight as create, plus the 18-byte frontier key prefix.
    preflight(&state, KEY_PREFIX_BYTES)?;
    let expected = revise_read_set_keys(frontier, &state);
    expect_read_set(read_set, expected, state.concept_refs.len() + 1)?;

    let payload = frontier_intent::encode_revise(frontier, &state, ledger)?;
    let (_frame, digest) = frame_and_digest(
        REVISE_FRONTIER,
        frontier_intent::OPERATION_VERSION,
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

    let basis = store
        .commit_basis(IdSpace::FrontierEntryId, ledger)
        .map_err(CommandError::from)?;

    // The exact CAS on the entry itself.
    let stored = stored_frontier(store, frontier, ledger)?;
    let frontier_expected = expected_revision(read_set, frontier)?;
    if stored.revision != frontier_expected {
        return Err(CommandError::Input(InputError::StaleBaseline {
            key: frontier,
            expected: frontier_expected,
        })
        .into());
    }
    // Every NEW listed concept must be current, active and pinned. Removed refs are gone from
    // the list and therefore require nothing.
    for key in state.concept_refs {
        require_active_term(store, *key, read_set, ledger)?;
    }

    let text_ref = map_format(typed::raw_hash(state.text.as_bytes(), ledger))?;
    let text_bytes = admitted_bytes(state.text.as_bytes(), ledger)?;
    let version = version_next(store.head())?;

    // A full canonical copy, then only the permitted fields move.
    let mut next = stored;
    next.revision = revision_next(next.revision)?;
    next.status = status_of(state.status)?;
    // Admitted copy, not an infallible `to_vec`: the slot extent is charged and reserved
    // through the caller's ledger exactly as creation does.
    next.concept_refs = {
        let mut refs = admitted_vec(state.concept_refs.len(), ledger)?;
        refs.extend_from_slice(state.concept_refs);
        refs
    };
    next.text_ref = text_ref;
    next.priority = state.priority;
    next.common.recorded_by = Some(admitted_string(authorized.principal(), ledger)?);
    next.common.actor_origin = ActorOrigin::Authenticated;
    next.common.updated_at = Some(meta.created_at);
    let frontier_next = next.revision;

    let affected = [RevPair {
        key: next.key,
        revision: frontier_next,
    }];
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

    let mut events: Vec<OwnedEvent> = admitted_vec(2, ledger)?;
    events.push(map_format(typed::prepare_event(
        EventInput::Frontier(&next),
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
        sha256: text_ref,
        declared_len: extent(text_bytes.len())?,
        source: BlobSource::Provide(text_bytes),
    });
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: extent(result_bytes.len())?,
        source: BlobSource::Provide(result_bytes),
    });

    let prepared = header(
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
    into_outcome(store.commit(prepared, limits, ledger))
}

/// Read one frontier entry and its text on a single durable basis.
///
/// The capability checked is exactly [`Capability::Read`]; `write_aux` does not imply it and
/// no other capability is consulted. Both presence and absence require the store's full
/// authority, durability and coverage, so an unverified view never answers "absent". The
/// record is copied through the canonical dispatcher and the text is the store's leased,
/// hash-verified [`CommittedBlob`], read on the **same** basis the record came from; a
/// mismatch is reported as a basis divergence rather than papered over. `max_text_len` is the
/// caller's ceiling: an over-long committed blob is a `BlobReadError::Ceiling`, never a
/// truncated answer.
///
/// # Errors
///
/// [`FrontierError`]: authorization, a non-frontier key, incomplete authority, a record or
/// blob read refusal (including the text ceiling), confirmed committed damage, or a real
/// ledger refusal.
pub fn read_frontier<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    key: RecordKey,
    max_text_len: u64,
    ledger: &mut dyn LeaseLedger,
) -> Result<FrontierRead, FrontierError> {
    grants.authorize(store.kb_uuid(), Capability::Read, ledger)?;
    ensure_import_ready(store, ledger)?;
    check_kind(key, RecordKind::FrontierEntry)?;

    use sage_db_store::view::RecordImageLookup;
    let (record, basis) = match store
        .record_owned(key, ledger)
        .map_err(CommandError::from)?
    {
        RecordImageLookup::Found { image, basis } => (image, basis),
        RecordImageLookup::Absent { basis } => return Ok(FrontierRead::Absent { basis }),
        RecordImageLookup::Incomplete(reason) => {
            return Err(CommandError::Incomplete(reason).into());
        }
    };
    let FoldedRecord::FrontierEntry(entry) = record.record() else {
        return Err(FrontierInternal::CopiedFamily.into());
    };
    let text_ref = entry.text_ref;
    let text = store
        .read_committed_blob(text_ref, max_text_len, ledger)
        .map_err(CommandError::from)?;
    if text.basis() != basis {
        return Err(CommandError::BasisDivergence.into());
    }
    Ok(FrontierRead::Found { record, text })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_set_keys_are_the_callers_order_never_normalised() {
        // Deliberately unsorted input: the derivation must reproduce it verbatim so that a
        // caller who stated the wrong order is *refused*, not silently sorted into agreement.
        let refs = [RecordKey::term(5), RecordKey::term(2)];
        let state = FrontierState {
            concept_refs: &refs,
            text: "",
            priority: 0,
            status: 0,
        };
        let create: Vec<_> = create_read_set_keys(&state).collect();
        assert_eq!(create, refs.to_vec());
        assert_eq!(create, vec![RecordKey::term(5), RecordKey::term(2)]);

        let frontier = RecordKey::new(RecordKind::FrontierEntry, 1);
        let revise: Vec<_> = revise_read_set_keys(frontier, &state).collect();
        // Term keys (kind 1) come first only because the caller listed them; the frontier key
        // is appended. Nothing here sorts.
        assert_eq!(
            revise,
            vec![RecordKey::term(5), RecordKey::term(2), frontier]
        );
    }

    #[test]
    fn an_unknown_status_is_a_payload_refusal() {
        assert!(matches!(
            status_of(2),
            Err(FrontierError::Input(FrontierInputError::Payload(
                FrontierPayloadError::UnknownStatus(2)
            )))
        ));
        assert!(status_of(0).is_ok());
        assert!(status_of(1).is_ok());
    }
}
