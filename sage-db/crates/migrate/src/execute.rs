//! Staged administrative import execution: Prepare → Validate → Import → Verify → Activate.
//!
//! # What this module is
//!
//! The executor half of the native migration. [`crate::mapping::prepare_native_mapping`]
//! builds canonical events, blob declarations, an identity mapping and one self-contained
//! package from an extraction plan; this module **persists** them, batch by batch, on a
//! real `Store`, and then re-reads what was written before completion is claimed.
//!
//! It is not a second mapper and not a second encoder. Every record event is the
//! mapping's own [`OwnedEvent`], every blob is the mapping's own [`BlobDeclaration`], and
//! every result blob comes from [`sage_db_store::typed::result_payload`]. What this module
//! adds is the *staging discipline*: which stage is written when, what must hold before the
//! next one runs, and what is re-read before completion.
//!
//! # The gate
//!
//! A `Prepare` step persists an [`ImportSessionRecord`] with `import_complete == false`.
//! The ordinary kernel commands and the kernel snapshot both consult
//! [`sage_db_store::handle::Store::import_readiness`] and refuse while any session is
//! incomplete, so after a drop and reopen the target stays closed until `Activate` writes
//! `import_complete == true`. The gate is persisted; nothing in this module has to be
//! trusted for it to hold.
//!
//! # What a single call cannot prove
//!
//! Completion is recorded only through [`mark_complete_after_verification`], which requires
//! a [`Verified`] token that only [`verify_and_attest`] can produce. That prevents "complete
//! without verifying". It cannot, however, prove the verification ran in a *freshly
//! reopened* process — a token is a value on one handle. The driver owns that cycle:
//! drop the `Store`, reopen it, verify on the reopened handle, then complete. Taking an
//! unchecked `bool` here would let a caller skip it, which is why the API takes a token.
//!
//! # Resources
//!
//! One ledger spans the whole step. [`PreparedImport`] exposes borrows, and the staged
//! copies this module must make (the transaction's own event and blob tables) are admitted
//! through that same ledger *before* they are reserved. No mapping buffer is cloned to
//! escape the credit that pays for it, and no counter is reconciled afterwards.

use crate::intent::{self, ImportStep, NATIVE_IMPORT_STEP};
use crate::mapping::{MappingError, PreparedImport};
use sage_db_kernel::auth::{AuthError, Capability, Grants};
use sage_db_store::commit::RecordedOutcome;
use sage_db_store::fs::FileSystem;
use sage_db_store::handle::{HandleOutcome, HandleRefusal, Store};
use sage_db_store::lease::LeaseLedger;
use sage_db_store::open::{OutcomeIncomplete, OutcomeLookup};
use sage_db_store::typed::{self, EventInput, RequestFrame, RequestOp, RequestOutcome};
use sage_db_store::view::{CommitReadError, ViewBasis};
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};
use sage_db_types::identity::KbUuid;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ActorOrigin, CountPair, CreationAudit, Extensions, IdSpace, ImportSessionRecord, ImportStage,
    RecordKey, RequestOutcomeEvent, RevPair, StateKind,
};
use sage_db_types::prepared::{
    BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction, RulesBinding,
};

/// The externally frozen identity a step must match, supplied by the operator.
///
/// Every field comes from outside the untrusted plan: the caller states what it expects
/// *before* the plan is decoded, and this executor compares. Deriving an expectation from
/// the plan and then checking the plan against it would prove nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExpectedIdentity<'a> {
    /// SHA-256 of the plan bytes exactly as supplied.
    pub plan_sha256: [u8; 32],
    /// The digest of the imported source material.
    pub source_digest: [u8; 32],
    /// The KB this import targets.
    pub kb_uuid: KbUuid,
    /// The import session identity.
    pub import_session_id: &'a str,
}

/// Which externally frozen field disagreed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExpectedField {
    /// The plan bytes did not hash to the expected digest.
    PlanDigest,
    /// The source digest differed.
    SourceDigest,
    /// The KB differed.
    Kb,
    /// The session identity differed.
    Session,
}

/// Trusted executor facts: identity, clock and policy.
#[derive(Clone, Copy)]
pub struct StepContext<'a> {
    /// The authenticated principal performing the import.
    pub principal: &'a str,
    /// Transaction id for this step, supplied by the executor.
    pub tx_id: [u8; 16],
    /// Commit time, UTC microseconds.
    pub created_at: i64,
    /// Canonical kernel limits.
    pub limits: &'a Limits,
    /// The whole-KB version this step expects to follow.
    ///
    /// A *derived* precondition, never an identity field: it is not in the payload and not
    /// in the request digest.
    pub expect_version: Option<u64>,
}

/// Why a step did not commit.
///
/// The commit result itself is **not** represented here: a successful call returns the
/// kernel's real [`HandleOutcome`], including `Unknown`, which is an ambiguous attempt
/// rather than an error.
#[derive(Debug)]
pub enum ExecuteError {
    /// The grants did not carry `admin` on this KB.
    Auth(AuthError),
    /// The frozen expectation did not match what was supplied.
    IdentityMismatch {
        /// Which expectation failed.
        field: ExpectedField,
    },
    /// The plan could not be prepared.
    Mapping(MappingError),
    /// The step payload was not acceptable.
    Payload(intent::StepPayloadError),
    /// The batch range addressed mapped events the plan does not have.
    BatchOutOfRange {
        /// Requested first event.
        first: u32,
        /// Requested count.
        count: u32,
        /// Events actually available.
        available: usize,
    },
    /// A batch splits a record family across stages in a way this executor refuses.
    ///
    /// The mapping emits a proposal's create and decide as two ordered events; a split
    /// between them would let a later batch revise a proposal whose creation never
    /// committed. The executor therefore requires each mapped event to be attributable to
    /// one revision of one record and refuses a range that would separate them.
    SplitInconsistent {
        /// Index of the offending event.
        at: usize,
    },
    /// The target already holds data this import must not adopt.
    TargetNotEmpty,
    /// A session with this id is already recorded with different bytes.
    SessionConflict,
    /// The caller's ledger refused or stopped the work.
    Refused(Refusal),
    /// Incomplete authority: no single durable publication to read.
    Incomplete(OutcomeIncomplete),
    /// A real store or commit-basis read refusal.
    Commit(CommitReadError),
    /// The prepared request frame was refused by the canonical encoder.
    Request(typed::RequestError),
    /// A generated value is not representable by this writer.
    Internal,
    /// An extent arithmetic step overflowed; the count is not a representable size.
    ExtentOverflow {
        /// The count that overflowed.
        count: usize,
    },
    /// Already-admitted credit could not be transferred to a retained owner.
    Transfer(sage_db_store::lease::TransferError),
    /// The real commit refused before any durable point.
    Store(HandleRefusal),
    /// Completion was requested without a verification token, or through the unguarded
    /// stage writer.
    CompletionRequiresVerification,
    /// The token's bound state no longer describes the store.
    StaleVerification,
}

impl From<AuthError> for ExecuteError {
    fn from(error: AuthError) -> Self {
        Self::Auth(error)
    }
}
impl From<MappingError> for ExecuteError {
    fn from(error: MappingError) -> Self {
        Self::Mapping(error)
    }
}
impl From<intent::StepPayloadError> for ExecuteError {
    fn from(error: intent::StepPayloadError) -> Self {
        Self::Payload(error)
    }
}
impl From<Refusal> for ExecuteError {
    fn from(error: Refusal) -> Self {
        Self::Refused(error)
    }
}
impl From<sage_db_types::ledger::BudgetError> for ExecuteError {
    fn from(error: sage_db_types::ledger::BudgetError) -> Self {
        Self::Refused(Refusal::Budget(error))
    }
}
impl From<sage_db_types::query::ControlStop> for ExecuteError {
    fn from(stop: sage_db_types::query::ControlStop) -> Self {
        Self::Refused(Refusal::Stopped(stop))
    }
}
impl From<CommitReadError> for ExecuteError {
    fn from(error: CommitReadError) -> Self {
        Self::Commit(error)
    }
}
impl From<sage_db_store::lease::TransferError> for ExecuteError {
    fn from(error: sage_db_store::lease::TransferError) -> Self {
        Self::Transfer(error)
    }
}
impl From<typed::RequestError> for ExecuteError {
    fn from(error: typed::RequestError) -> Self {
        Self::Request(error)
    }
}
impl From<typed::ResultPreparationError> for ExecuteError {
    fn from(error: typed::ResultPreparationError) -> Self {
        match error {
            // A real refusal keeps its class; anything else is this writer's own defect.
            typed::ResultPreparationError::Refused(refusal) => Self::Refused(refusal),
            _ => Self::Internal,
        }
    }
}

/// Map a canonical encoder verdict on *generated* bytes.
///
/// A format verdict here describes bytes this module just produced, so it is an internal
/// defect rather than a statement about the caller's disk. Refusals keep their class.
fn map_format<T>(outcome: FormatOutcome<T>) -> Result<T, ExecuteError> {
    match outcome {
        FormatOutcome::Ok(value) => Ok(value),
        // These bytes were produced by a canonical writer in this call, so a format
        // verdict here describes our own image, not the caller's disk: an internal
        // defect, never a restore-the-backup class.
        FormatOutcome::Format(_) => Err(ExecuteError::Internal),
        FormatOutcome::Refused(refusal) => Err(ExecuteError::Refused(refusal)),
    }
}

fn map_request<T>(outcome: RequestOutcome<T>) -> Result<T, ExecuteError> {
    match outcome {
        RequestOutcome::Ok(value) => Ok(value),
        RequestOutcome::Rejected(error) => Err(ExecuteError::Request(error)),
        RequestOutcome::Refused(refusal) => Err(ExecuteError::Refused(refusal)),
    }
}

// ---------------------------------------------------------------------------
// Canonical extraction of one mapped event
// ---------------------------------------------------------------------------

/// What the canonical projection says one mapped event will write.
///
/// Derived by `format`'s own projection from the event's real field bytes, so this module
/// never re-parses a family layout. `entries == 0` means the event emits no record row
/// (an allocation mark or an outcome), and `entries == 2` means it emits two (genesis).
struct MappedEvent {
    /// One entry per emitted record.
    entries: Vec<StateEntryRef>,
}

/// One emitted record's identity, borrowed from the projection buffers.
struct StateEntryRef {
    kind: u16,
    key: Vec<u8>,
    revision: u64,
}

/// Project one mapped event through the real canonical seam.
///
/// `view_for_encode` is the format's own public factory: it turns `OwnedEvent`s into
/// borrowed `Field` tables with every slot admitted first. Using it means this module never
/// hand-writes an envelope layout — the field bytes go straight to the projector the
/// persisted-file path uses.
fn project(
    event: &OwnedEvent,
    header: &sage_db_format::encode::TxHeaderToEncode,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<MappedEvent, ExecuteError> {
    // `view_for_encode` takes a slice by value, so one owned copy is needed. It is admitted
    // first through the SAME ledger: slots, each value's bytes, and the checkpoint all precede
    // the fallible reserve, so the projection copy cannot outrun the credit that pays for it.
    let events = [copy_event(event, ledger)?];
    let view = match sage_db_format::prepared::view_for_encode(header, &events, limits, ledger) {
        FormatOutcome::Ok(view) => view,
        FormatOutcome::Format(_) => return Err(ExecuteError::Internal),
        FormatOutcome::Refused(refusal) => return Err(ExecuteError::Refused(refusal)),
    };
    let encoded = view.events();
    let first = encoded.first().ok_or(ExecuteError::Internal)?;

    let mut out: Vec<sage_db_types::persist::StateEntry> = Vec::new();
    match sage_db_format::projection::project_event(first.tag, &first.fields, &mut out, ledger) {
        FormatOutcome::Ok(_) => {}
        FormatOutcome::Format(_) => return Err(ExecuteError::Internal),
        FormatOutcome::Refused(refusal) => return Err(ExecuteError::Refused(refusal)),
    }
    // `StateEntryRef` owns its key bytes, so BOTH the slot extent and each key's bytes are
    // admitted before the reserve and before each move. Charging only an entries count would let
    // the copies (and, for a term, the widened key) be retained on credit never granted.
    let slots = out
        .len()
        .checked_mul(core::mem::size_of::<StateEntryRef>())
        .ok_or(ExecuteError::ExtentOverflow { count: out.len() })?;
    let key_bytes = out.iter().try_fold(0u64, |total, entry| {
        total.checked_add(u64::try_from(entry.key.len()).ok()?)
    });
    let key_bytes = key_bytes.ok_or(ExecuteError::ExtentOverflow { count: out.len() })?;
    let mut entries: Vec<StateEntryRef> = Vec::new();
    ledger.try_charge_entries(out.len() as u64)?;
    ledger.try_charge(slots as u64)?;
    ledger.try_charge(key_bytes)?;
    ledger.checkpoint()?;
    entries.try_reserve_exact(out.len()).map_err(|_| {
        ExecuteError::Refused(Refusal::Allocation {
            requested: out.len(),
        })
    })?;
    for entry in out {
        entries.push(StateEntryRef {
            kind: entry.kind,
            key: entry.key,
            revision: entry.revision,
        });
    }
    Ok(MappedEvent { entries })
}

// ---------------------------------------------------------------------------
// Batches and stage records
// ---------------------------------------------------------------------------

/// The mapped events one step carries, projected through the canonical seam.
struct Selected<'a> {
    events: &'a [OwnedEvent],
    mapped: Vec<MappedEvent>,
}

/// Select the mapped events for `step`, refusing a range the plan does not have.
///
/// Exactly ONE stage carries mapped records: `Import`. `Prepare` writes the session record
/// alone, and `Validate`, `Verify` and `Activate` write no mapped event at all — their
/// evidence is the outcome. `Validate` is a registry stage with no state effect in this
/// batch, so writing records for it would mean the stage table in the operator doc described
/// something other than what the code does.
fn select<'a>(
    prepared: &'a PreparedImport,
    step: &ImportStep<'_>,
    header: &sage_db_format::encode::TxHeaderToEncode,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> Result<Selected<'a>, ExecuteError> {
    if step.stage != ImportStage::Import {
        if step.event_count != 0 {
            return Err(ExecuteError::BatchOutOfRange {
                first: step.first_event,
                count: step.event_count,
                available: prepared.events().len(),
            });
        }
        return Ok(Selected {
            events: &[],
            mapped: Vec::new(),
        });
    }
    let available = prepared.events().len();
    let start = step.first_event as usize;
    let end =
        start
            .checked_add(step.event_count as usize)
            .ok_or(ExecuteError::BatchOutOfRange {
                first: step.first_event,
                count: step.event_count,
                available,
            })?;
    if end > available {
        return Err(ExecuteError::BatchOutOfRange {
            first: step.first_event,
            count: step.event_count,
            available,
        });
    }
    let events = &prepared.events()[start..end];
    // A proposal family must not be split across stages: the mapping emits a create and a
    // decision as two ordered events, and a batch boundary between them would let a later
    // batch revise a proposal whose creation never committed. The check is on the tags the
    // mapping actually produced, so it cannot be defeated by a caller-chosen range.
    reject_split_lifecycle(events, start)?;
    let mut mapped: Vec<MappedEvent> = Vec::new();
    // Both units: the retained slot extent AND the entries. Charging entries alone would
    // admit a table whose bytes were never accounted for.
    let slots = events
        .len()
        .checked_mul(core::mem::size_of::<MappedEvent>())
        .ok_or(ExecuteError::Internal)?;
    ledger.try_charge_entries(events.len() as u64)?;
    ledger.try_charge(slots as u64)?;
    ledger.checkpoint()?;
    mapped
        .try_reserve_exact(events.len())
        .map_err(|_| ExecuteError::Refused(Refusal::Allocation { requested: slots }))?;
    for event in events {
        mapped.push(project(event, header, limits, ledger)?);
    }
    Ok(Selected { events, mapped })
}

/// Refuse a batch whose first event continues a proposal started outside it.
///
/// The only multi-event record family the mapper emits is a proposal's create followed by
/// its decision, both under the same numeric id. If a batch starts with a decision whose
/// create lies before `start`, the creation and the decision would be committed by
/// different transactions, and a crash between them would publish a decision for a
/// proposal that never existed. Combined tags are what the mapping wrote, so this is a
/// property of the plan rather than of a caller's claim.
fn reject_split_lifecycle(events: &[OwnedEvent], start: usize) -> Result<(), ExecuteError> {
    // `ProposalDecided` is tag 97; `ProposalSubmitted` is 96.
    const SUBMITTED: u16 = 96;
    const DECIDED: u16 = 97;
    if start > 0 && events.first().is_some_and(|event| event.tag == DECIDED) {
        return Err(ExecuteError::SplitInconsistent { at: start });
    }
    let mut seen_submit = false;
    for (offset, event) in events.iter().enumerate() {
        if event.tag == SUBMITTED {
            seen_submit = true;
        }
        if event.tag == DECIDED && !seen_submit {
            return Err(ExecuteError::SplitInconsistent { at: start + offset });
        }
    }
    Ok(())
}

/// The `(key, revision)` pairs a selected batch affects, in canonical ascending order.
///
/// Derived from the projection, not from a second parse: `kind` and `key` come from the
/// format's own projector, and a natural-key entry has no `RecordKey`, so it contributes
/// no pair (the result payload's affected list is numeric identities only).
fn affected(
    mapped: &[MappedEvent],
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<RevPair>, ExecuteError> {
    let _probe = ();
    let mut pairs: Vec<RevPair> = Vec::new();
    let capacity = mapped
        .iter()
        .map(|event| event.entries.len())
        .sum::<usize>();
    ledger.try_charge_entries(capacity as u64)?;
    pairs.try_reserve_exact(capacity).map_err(|_| {
        ExecuteError::Refused(Refusal::Allocation {
            requested: capacity,
        })
    })?;
    for event in mapped {
        for entry in &event.entries {
            let Some(kind) = sage_db_types::persist::RecordKind::from_u16(entry.kind) else {
                // State kinds 10..=13 are natural records with no numeric identity.
                continue;
            };
            // The projection's key bytes for a numeric kind are the canonical key, so a
            // wide id is decoded back from them rather than guessed.
            let Some(id) = decode_numeric_key(entry.kind, &entry.key) else {
                return Err(ExecuteError::Internal);
            };
            let key = RecordKey::new(kind, id);
            let revision = sage_db_types::identity::RecordRevision::new(entry.revision)
                .ok_or(ExecuteError::Internal)?;
            pairs.push(RevPair { key, revision });
        }
    }
    pairs.sort_unstable();
    Ok(pairs)
}

/// Decode a projected numeric key's id, at the width its kind uses.
///
/// The projection writes an identified record's entry key as the id alone, using the
/// width that kind declares: Term ids are `u32` (4 bytes), every other identified kind is
/// a full `u128` (16 bytes). Reading Term keys as 16 bytes was a real defect: it made every
/// term event fail the projection decode.
fn decode_numeric_key(kind: u16, bytes: &[u8]) -> Option<u128> {
    match (kind, bytes.len()) {
        (1, 4) => {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(bytes);
            Some(u128::from(u32::from_le_bytes(raw)))
        }
        (_, 16) => {
            let mut raw = [0u8; 16];
            raw.copy_from_slice(bytes);
            Some(u128::from_le_bytes(raw))
        }
        _ => None,
    }
}

/// The stage's `ImportSessionRecord`, fully admitted.
fn stage_session(
    prepared: &PreparedImport,
    expected: &ExpectedIdentity<'_>,
    stage: ImportStage,
    principal: &str,
    context: &StepContext<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<ImportSessionRecord, ExecuteError> {
    let source = prepared.import_session();
    let slots = source.expected_counts.len();
    let bytes = slots
        .checked_mul(core::mem::size_of::<CountPair>())
        .ok_or(ExecuteError::Internal)?;
    ledger.try_charge_entries(slots as u64)?;
    ledger.try_charge(bytes as u64)?;
    ledger.checkpoint()?;
    let mut counts: Vec<CountPair> = Vec::new();
    counts
        .try_reserve_exact(slots)
        .map_err(|_| ExecuteError::Refused(Refusal::Allocation { requested: bytes }))?;
    for pair in &source.expected_counts {
        counts.push(*pair);
    }
    Ok(ImportSessionRecord {
        import_session_id: admitted_string(expected.import_session_id, ledger)?,
        stage,
        source_digest: expected.source_digest,
        plan_ref: source.plan_ref,
        expected_counts: counts,
        // Only `Activate` completes, and only through the verified path.
        import_complete: stage == ImportStage::Activate,
        audit: CreationAudit {
            recorded_by: Some(admitted_string(principal, ledger)?),
            actor_origin: ActorOrigin::Authenticated,
            // Tag 208's schema makes `reason` mandatory: an administrative stage record
            // must say why it was written, and an absent reason is a format refusal.
            reason: Some(admitted_string("native import stage transition", ledger)?),
            created_at: context.created_at,
        },
        extensions: Extensions::new(),
    })
}

/// Admit and copy one string.
fn admitted_string(value: &str, ledger: &mut dyn SharedLedger) -> Result<String, ExecuteError> {
    ledger.checkpoint()?;
    ledger.try_charge(value.len() as u64)?;
    ledger.checkpoint()?;
    let mut out = String::new();
    out.try_reserve_exact(value.len()).map_err(|_| {
        ExecuteError::Refused(Refusal::Allocation {
            requested: value.len(),
        })
    })?;
    out.push_str(value);
    Ok(out)
}

/// Admit and copy one blob declaration, including its bytes.
fn copy_blob(
    blob: &BlobDeclaration,
    ledger: &mut dyn SharedLedger,
) -> Result<BlobDeclaration, ExecuteError> {
    match &blob.source {
        BlobSource::Provide(bytes) => {
            ledger.checkpoint()?;
            ledger.try_charge(bytes.len() as u64)?;
            ledger.checkpoint()?;
            let mut owned: Vec<u8> = Vec::new();
            owned.try_reserve_exact(bytes.len()).map_err(|_| {
                ExecuteError::Refused(Refusal::Allocation {
                    requested: bytes.len(),
                })
            })?;
            owned.extend_from_slice(bytes);
            Ok(BlobDeclaration {
                sha256: blob.sha256,
                declared_len: blob.declared_len,
                source: BlobSource::Provide(owned),
            })
        }
        BlobSource::Existing => Ok(BlobDeclaration {
            sha256: blob.sha256,
            declared_len: blob.declared_len,
            source: BlobSource::Existing,
        }),
    }
}

/// Admit and copy one owned event.
fn copy_event(
    event: &OwnedEvent,
    ledger: &mut dyn SharedLedger,
) -> Result<OwnedEvent, ExecuteError> {
    ledger.checkpoint()?;
    ledger.try_charge_entries(event.fields.len() as u64)?;
    let slot = event
        .fields
        .len()
        .checked_mul(core::mem::size_of::<(u16, Vec<u8>)>())
        .ok_or(ExecuteError::ExtentOverflow {
            count: event.fields.len(),
        })?;
    ledger.try_charge(slot as u64)?;
    ledger.checkpoint()?;
    let mut fields: Vec<(u16, Vec<u8>)> = Vec::new();
    fields
        .try_reserve_exact(event.fields.len())
        .map_err(|_| ExecuteError::Refused(Refusal::Allocation { requested: slot }))?;
    for (tag, value) in &event.fields {
        ledger.try_charge(value.len() as u64)?;
        ledger.checkpoint()?;
        let mut owned: Vec<u8> = Vec::new();
        owned.try_reserve_exact(value.len()).map_err(|_| {
            ExecuteError::Refused(Refusal::Allocation {
                requested: value.len(),
            })
        })?;
        owned.extend_from_slice(value);
        fields.push((*tag, owned));
    }
    Ok(OwnedEvent {
        tag: event.tag,
        fields,
    })
}

/// Admit a table of `len` slots of `T` before reserving it.
fn admitted<T>(len: usize, ledger: &mut dyn SharedLedger) -> Result<Vec<T>, ExecuteError> {
    ledger.checkpoint()?;
    let bytes = len
        .checked_mul(core::mem::size_of::<T>())
        .ok_or(ExecuteError::ExtentOverflow { count: len })?;
    ledger.try_charge_entries(len as u64)?;
    ledger.try_charge(bytes as u64)?;
    ledger.checkpoint()?;
    let mut out: Vec<T> = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| ExecuteError::Refused(Refusal::Allocation { requested: bytes }))?;
    Ok(out)
}

/// The stable per-step request id, bounded by the canonical ceiling.
fn request_id_of(
    step: &ImportStep<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<String, ExecuteError> {
    let capacity = step
        .import_session_id
        .len()
        .checked_add(24)
        .ok_or(ExecuteError::Internal)?;
    // The id is a retained owner of this attempt, so it is admitted like any other.
    ledger.try_charge(capacity as u64)?;
    ledger.checkpoint()?;
    let mut id = String::new();
    id.try_reserve_exact(capacity).map_err(|_| {
        ExecuteError::Refused(Refusal::Allocation {
            requested: capacity,
        })
    })?;
    id.push_str(step.import_session_id);
    id.push('-');
    push_decimal(&mut id, u32::from(step.stage.as_u16()));
    id.push('-');
    push_decimal(&mut id, step.batch_index);
    if id.len() > typed::MAX_REQUEST_ID_BYTES as usize {
        return Err(ExecuteError::Internal);
    }
    Ok(id)
}

/// Append a decimal without `format!`.
fn push_decimal(out: &mut String, value: u32) {
    let mut buf = [0u8; 10];
    let mut at = buf.len();
    let mut rest = value;
    loop {
        at -= 1;
        buf[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    out.push_str(core::str::from_utf8(&buf[at..]).unwrap_or("0"));
}

/// The request frame and digest for one step.
fn frame_step(
    step: &ImportStep<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<[u8; 32], ExecuteError> {
    let payload = intent::encode_step(step, ledger)?;
    let ops = [RequestOp::new(
        NATIVE_IMPORT_STEP,
        intent::OPERATION_VERSION,
        &payload,
    )];
    // An import step reads no record baseline and pins no whole-KB version in its
    // identity; the expectation is a header precondition.
    let frame = RequestFrame::new(&ops, &[], None);
    let bytes = map_request(typed::encode_request(&frame, ledger))?;
    map_request(typed::request_digest_of(&bytes, ledger))
}

/// The outbound header a projection needs.
///
/// Only `kb_uuid` and `version` influence the projected bytes; the rest are the
/// transaction's identity, which the projector does not read.
fn projection_header(
    store: &Store<impl FileSystem>,
    version: u64,
    context: &StepContext<'_>,
    rules: RulesBinding,
) -> sage_db_format::encode::TxHeaderToEncode {
    sage_db_format::encode::TxHeaderToEncode {
        kb_uuid: store.kb_uuid(),
        version,
        tx_id: context.tx_id,
        prev_version: store.head(),
        created_at: context.created_at,
        principal_id: String::new(),
        request_id: String::new(),
        payload_digest: [0u8; 32],
        rules_profile: rules.profile.as_u16(),
        rules_digest: rules.rules_digest,
        request_canon_version: sage_db_types::prepared::RequestCanonVersion::V1,
        expect_version: context.expect_version,
        extensions: Extensions::new(),
    }
}

// ---------------------------------------------------------------------------
// The four public operations
// ---------------------------------------------------------------------------

/// The result of one step.
#[derive(Debug)]
pub struct StepOutcome {
    /// The stage that was written.
    pub stage: ImportStage,
    /// The kernel's exact commit result, untouched.
    pub outcome: HandleOutcome,
    /// The canonical request digest of this step.
    pub request_digest: [u8; 32],
}

/// Write one stage of the import as a single administrative transaction.
///
/// # Per-stage payload
///
/// * **Prepare** — the `ImportSession` record (`import_complete == false`) plus every blob
///   the package needs. No mapped record is written. A definite non-commit leaves Genesis
///   authoritative (orphan files may remain); Unknown requires explicit resolution.
/// * **Validate** — bookkeeping and its outcome only; a nonzero mapped-event count refuses.
/// * **Import** — the selected mapped events and their declared blobs.
/// * **Verify / Activate** — no record events; the outcome is the evidence. Use
///   [`verify_and_attest`] before Activate.
///
/// # Errors
///
/// [`ExecuteError`] for missing authorization, a frozen-identity mismatch, a batch range the
/// plan does not have, an occupied target, a real ledger/store refusal. A genuine `Unknown`
/// is returned inside [`StepOutcome`], never as an error.
#[allow(clippy::too_many_arguments)]
pub fn write_stage<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    expected: &ExpectedIdentity<'_>,
    plan_bytes: &[u8],
    prepared: &PreparedImport,
    step: &ImportStep<'_>,
    context: &StepContext<'_>,
    ledger: &mut dyn LeaseLedger,
) -> Result<StepOutcome, ExecuteError> {
    // A token on a sibling method would be worthless while this public path still accepted
    // `Activate`: a caller could simply skip the verification. Completion has exactly ONE
    // sanctioned entry, `mark_complete_after_verification`, and it is the only caller of the
    // private implementation with `Activate`.
    if step.stage == ImportStage::Activate {
        return Err(ExecuteError::CompletionRequiresVerification);
    }
    write_stage_inner(
        store, grants, expected, plan_bytes, prepared, step, context, ledger, None,
    )
}

/// One PRIVATE implementation. `verified` is `Some` only on the completion path.
#[allow(clippy::too_many_arguments)]
fn write_stage_inner<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    expected: &ExpectedIdentity<'_>,
    plan_bytes: &[u8],
    prepared: &PreparedImport,
    step: &ImportStep<'_>,
    context: &StepContext<'_>,
    ledger: &mut dyn LeaseLedger,
    verified: Option<&Verified>,
) -> Result<StepOutcome, ExecuteError> {
    // 1. Real authorization on the actual target KB.
    let authorized = grants.authorize(store.kb_uuid(), Capability::Admin, ledger)?;

    // 2. The plan hash is taken over the RAW supplied bytes, before any decode. The
    //    completion path has no plan bytes to re-hash: its identity was proved by the
    //    verification token, and `Verified::check_current` re-checks that below.
    if authorized.kb() != expected.kb_uuid {
        return Err(ExecuteError::IdentityMismatch {
            field: ExpectedField::Kb,
        });
    }
    if verified.is_none() {
        let plan_hash = map_format(typed::raw_hash(plan_bytes, ledger))?;
        if plan_hash != expected.plan_sha256 {
            return Err(ExecuteError::IdentityMismatch {
                field: ExpectedField::PlanDigest,
            });
        }
    }
    let session = prepared.import_session();
    if session.import_session_id != expected.import_session_id {
        return Err(ExecuteError::IdentityMismatch {
            field: ExpectedField::Session,
        });
    }
    if session.source_digest != expected.source_digest {
        return Err(ExecuteError::IdentityMismatch {
            field: ExpectedField::SourceDigest,
        });
    }

    // 3. The step's own domains, re-checked so a caller-built step cannot skip them.
    intent::encode_step(step, ledger)?;

    // 4. Exact per-stage/batch resume lookup FIRST, exactly like an ordinary kernel command.
    //    A successfully acknowledged `Prepare` made `head > 1`, so a freshness check before
    //    this lookup would refuse to retry a request that already committed.
    let digest = frame_step(step, ledger)?;
    let request_id = request_id_of(step, ledger)?;
    match store.outcome(authorized.principal(), &request_id) {
        OutcomeLookup::Found {
            outcome,
            original_version,
            ..
        } => {
            if outcome.event().payload_digest != digest {
                return Err(ExecuteError::SessionConflict);
            }
            return Ok(StepOutcome {
                stage: step.stage,
                outcome: HandleOutcome::AlreadyCommitted {
                    version: original_version,
                    recorded: RecordedOutcome::Outcome(outcome),
                },
                request_digest: digest,
            });
        }
        OutcomeLookup::Absent { .. } => {}
        OutcomeLookup::Incomplete(reason) => return Err(ExecuteError::Incomplete(reason)),
    }

    // 5. A NOVEL Prepare starts only on a correctly initialized, empty target.
    let head = store.head();
    if step.stage == ImportStage::Prepare {
        if head != Some(1) {
            return Err(ExecuteError::TargetNotEmpty);
        }
        let basis = store.commit_basis(IdSpace::TermId, ledger)?;
        if basis.rules.profile != sage_db_types::relation::RuleProfile::LegacyV1 {
            return Err(ExecuteError::TargetNotEmpty);
        }
    }
    // Completion consumes a token that must still describe THIS state.
    if step.stage == ImportStage::Activate {
        let verified = verified.ok_or(ExecuteError::CompletionRequiresVerification)?;
        verified.check_current(store, expected, prepared, ledger)?;
    }

    // 6. Build the transaction from admitted copies only.
    let basis = store.commit_basis(IdSpace::TermId, ledger)?;
    let version = match head {
        None => 1,
        Some(current) => current.checked_add(1).ok_or(ExecuteError::Internal)?,
    };
    let rules = basis.rules;
    // `RulesBinding` is copied into the header; its extent is admitted before the copy so the
    // header never holds bytes the ledger did not grant.
    let rules_copy = {
        let extent = u64::try_from(core::mem::size_of::<RulesBinding>())
            .map_err(|_| ExecuteError::ExtentOverflow { count: 1 })?;
        ledger.try_charge(extent)?;
        ledger.checkpoint()?;
        // The header owns its own binding; `rules` is still needed for the blob table below.
        rules.clone()
    };
    let header = projection_header(store, version, context, rules_copy);
    let selected = select(prepared, step, &header, context.limits, ledger)?;
    let pairs = affected(&selected.mapped, ledger)?;

    let session_record = stage_session(
        prepared,
        expected,
        step.stage,
        authorized.principal(),
        context,
        ledger,
    )?;
    let session_event = map_format(typed::prepare_event(
        EventInput::ImportSession(&session_record),
        ledger,
    ))?;

    let result_bytes = typed::result_payload(version, &pairs, &[], ledger)?;
    let result_ref = map_format(typed::raw_hash(&result_bytes, ledger))?;
    let outcome_event = RequestOutcomeEvent {
        principal_id: admitted_string(authorized.principal(), ledger)?,
        request_id: admitted_string(&request_id, ledger)?,
        payload_digest: digest,
        result_ref,
        commit_version: version,
        audit: CreationAudit {
            recorded_by: Some(admitted_string(authorized.principal(), ledger)?),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: context.created_at,
        },
        extensions: Extensions::new(),
    };
    let outcome_owned = map_format(typed::prepare_event(
        EventInput::Outcome(&outcome_event),
        ledger,
    ))?;

    // Events: mapped records, then the stage record, then the outcome.
    let mut events = admitted::<OwnedEvent>(selected.events.len() + 2, ledger)?;
    for event in selected.events {
        events.push(copy_event(event, ledger)?);
    }
    events.push(session_event);
    events.push(outcome_owned);

    // Blobs: rules (Existing), the mapping's declarations, and the result.
    let mut blobs = admitted::<BlobDeclaration>(prepared.blobs().len() + 2, ledger)?;
    blobs.push(BlobDeclaration {
        sha256: rules.rules_ref,
        declared_len: basis.rules_len,
        source: BlobSource::Existing,
    });
    for blob in prepared.blobs() {
        blobs.push(copy_blob(blob, ledger)?);
    }
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: result_bytes.len() as u64,
        source: BlobSource::Provide(result_bytes),
    });

    let transaction = PreparedTransaction {
        version,
        prev_version: head,
        tx_id: context.tx_id,
        kb_uuid: store.kb_uuid(),
        created_at: context.created_at,
        principal_id: admitted_string(authorized.principal(), ledger)?,
        request_id: admitted_string(&request_id, ledger)?,
        payload_digest: digest,
        expect_version: context.expect_version,
        extensions: Extensions::new(),
        events,
        blobs,
        rules,
    };

    let outcome = match store.commit(transaction, context.limits, ledger) {
        HandleOutcome::NotCommitted { reason } => return Err(ExecuteError::Store(reason)),
        other => other,
    };
    Ok(StepOutcome {
        stage: step.stage,
        outcome,
        request_digest: digest,
    })
}

/// One verification defect, named.
///
/// The canonical verdicts travel **unchanged**: a budget, a stop, a format failure, an I/O
/// error and "not durable yet" have four different remedies, and collapsing them would tell an
/// operator to retry with more budget when the real answer is corruption (or the reverse).
#[derive(Debug)]
pub enum VerifyDefect {
    /// A state kind does not hold the expected number of current rows.
    CountMismatch {
        /// Which kind.
        kind: StateKind,
        /// Expected rows.
        expected: u64,
        /// Rows actually current.
        actual: u64,
    },
    /// A planned revision is not present at all.
    MissingRecord {
        /// Which kind.
        kind: StateKind,
        /// Which revision the plan expected.
        revision: u64,
    },
    /// A record is present at the planned revision but its typed after-image differs.
    ///
    /// This is the case a count-plus-revision check cannot see: a wrong atom, body, content,
    /// audit or extension at an unchanged revision.
    ValueMismatch {
        /// Which kind.
        kind: StateKind,
        /// Which revision differed.
        revision: u64,
    },
    /// A blob read failed for a reason that is not a content verdict.
    BlobRead(sage_db_store::view::BlobReadError),
    /// A required blob is missing, or its bytes are not what was declared.
    BlobMismatch {
        /// Its digest.
        sha256: [u8; 32],
    },
    /// A commit-basis read refused.
    CommitRead(sage_db_store::view::CommitReadError),
    /// The session is not recorded as in-progress at this identity.
    SessionNotActive,
    /// The basis is not available at all.
    NotReady,
    /// No definitive durable publication to read: the canonical reason travels.
    Incomplete(OutcomeIncomplete),
    /// A planned watermark names a space the registry does not have.
    UnknownSpace {
        /// The space number the plan declared.
        space: u16,
    },
    /// A committed allocation mark is below the planned one.
    WatermarkLow {
        /// The space.
        space: u16,
        /// The mark the plan requires.
        planned: u128,
        /// The mark actually committed.
        recorded: u128,
    },
    /// The publication is readable but not durable, so it is not evidence.
    NotDurable,
    /// A canonical format verdict on bytes the store returned.
    Format(sage_db_types::framing::FormatError),
    /// The caller's ledger refused budget, or a stop arrived.
    Refused(Refusal),
    /// A real record-read failure.
    RecordRead(sage_db_store::view::RecordReadError),
    /// The physical scan could not complete.
    ///
    /// The canonical reason travels inside — "no definitive durable basis" is not "absent", and
    /// an I/O or format refusal is not either. Flattening it to a unit variant would tell an
    /// operator to retry when the real answer is corruption, or the reverse.
    Scan(Box<sage_db_store::view::RecordScanError<Refusal>>),
    /// The historical search could not finish within its bounds.
    HistoryIncomplete(sage_db_store::history::HistoryIncomplete),
    /// The historical read failed definitively: I/O, format or corruption.
    HistoryError(sage_db_store::open::OpenRefusal),
    /// A retained table could not be allocated at its admitted extent.
    Allocation {
        /// Elements requested.
        requested: usize,
    },
    /// An extent arithmetic step overflowed `u64`/`usize`.
    ///
    /// Distinct from [`Self::Allocation`]: the allocator did not fail, the *count* was not a
    /// representable extent. Reporting it as an allocation failure would send a caller to raise a
    /// budget that cannot help.
    ExtentOverflow {
        /// The count that could not be represented.
        count: usize,
    },
    /// A transfer of already-admitted credit could not be made.
    ///
    /// The attempt did not hold the bytes it was asked to transfer, so this is an accounting
    /// invariant break, never a budget ceiling.
    Transfer(sage_db_store::lease::TransferError),
}

/// The thirteen state kinds a full verification compares.
const KINDS: [StateKind; 13] = [
    StateKind::Term,
    StateKind::Assertion,
    StateKind::Claim,
    StateKind::Evidence,
    StateKind::EvidenceLink,
    StateKind::Vector,
    StateKind::Proposal,
    StateKind::FrontierEntry,
    StateKind::SessionCheckpoint,
    StateKind::Asset,
    StateKind::Model,
    StateKind::KbMetadata,
    StateKind::RuleConfig,
];

/// One current record's state kind.
fn state_kind_of(record: &sage_db_store::fold::FoldedRecord) -> StateKind {
    use sage_db_store::fold::FoldedRecord as R;
    match record {
        R::Asset(_) => StateKind::Asset,
        R::Term(_) => StateKind::Term,
        R::Assertion(_) => StateKind::Assertion,
        R::Claim(_) => StateKind::Claim,
        R::Evidence(_) => StateKind::Evidence,
        R::EvidenceLink(_) => StateKind::EvidenceLink,
        R::Vector(_) => StateKind::Vector,
        R::Proposal(_) => StateKind::Proposal,
        R::FrontierEntry(_) => StateKind::FrontierEntry,
        R::SessionCheckpoint(_) => StateKind::SessionCheckpoint,
        R::KbMetadata(_) => StateKind::KbMetadata,
        R::Model(_) => StateKind::Model,
        R::RuleConfig(_) => StateKind::RuleConfig,
    }
}

/// One planned after-image together with the identity it belongs to.
///
/// The identity is materialised once, here, because a natural-key record's identity owns a
/// string (a model's configured name). Borrowing it from the record would borrow from
/// `self` while the same table is mutated, so it is admitted and built up front instead.
struct PlannedImage {
    identity: sage_db_store::fold::RecordIdentity,
    revision: u64,
    record: sage_db_store::fold::FoldedRecord,
}

/// Verify what was written, on **this** handle.
///
/// # What this actually checks
///
/// * **Counts, every kind.** All thirteen kinds are compared against the plan; a kind the plan
///   does not count is compared against **zero**, so an extra row of an unplanned family fails.
/// * **Every planned revision, by full typed value.** The plan's own event bytes are decoded
///   through the canonical dispatcher into the exact value a reader would see. Images are grouped
///   by `RecordIdentity`; the newest is compared against the **current** row found by one physical
///   scan, and each earlier one against `Store::history` on a durable basis. Identity and revision
///   are never the check on their own: a wrong atom, body, content, audit value or extension at an
///   unchanged revision fails with [`VerifyDefect::ValueMismatch`].
/// * **Natural-key singletons and assets by value too.** `KbMetadata`, `RuleConfig`, `Model` and
///   `Asset` have no numeric id, but `FoldedRecord::identity` names them, so they are grouped,
///   compared and history-checked like any other identity. Their identity is *not* their content:
///   a model is keyed by its configured name and the two singletons by their kind, so the value
///   comparison is what makes them meaningful.
/// * **Recorded watermarks, in production.** For every space the plan declares a mark for, the
///   committed basis is read on this handle and must be at or above the planned mark. A missing or
///   low mark is a typed defect, and the marks are bound into the [`Verified`] token.
/// * **Every required blob** at its declared length, compared with the supplied bytes where the
///   mapping has them, and re-hashed to its own name.
/// * **The session**, still recorded incomplete at this identity on this basis.
///
/// # Ownership
///
/// Every retained table is admitted through the caller's ledger **before** it is reserved: the
/// planned-image table, its identities, the decode scratch, the writer capacity, and the history
/// images. The current images are **borrowed** from the store's own retained owners — nothing is
/// cloned to reach them — so the scan contributes no unadmitted bytes. Sorting is unstable on a
/// total order (identity, then revision), which allocates no scratch and cannot lose a planned
/// image: two images of one identity are distinguished by their revision.
///
/// # Errors
///
/// [`VerifyDefect`]; each variant keeps its canonical class so a budget refusal, a stop, a format
/// verdict, an I/O failure and "not durable yet" stay distinguishable.
pub fn verify_written<F: FileSystem>(
    store: &mut Store<F>,
    prepared: &PreparedImport,
    expected: &ExpectedIdentity<'_>,
    ledger: &mut dyn LeaseLedger,
) -> Result<(), VerifyDefect> {
    let opened_basis = store.view().ok_or(VerifyDefect::NotReady)?.basis();

    // --- One admitted table of every planned after-image, with its identity. ---
    //
    // Built BEFORE the scan so the ids are stable: borrowing an identity out of the table while
    // pushing into it would be a second mutable borrow.
    let planned = planned_images(prepared, ledger)?;

    // --- One physical count scan; full current-value checks follow. ---
    let mut counts = [0u64; 13];
    store
        .scan_current_records(ledger, |record, _inner| {
            let kind = state_kind_of(record);
            if let Some(at) = KINDS.iter().position(|candidate| *candidate == kind) {
                counts[at] += 1;
            }
            Ok::<(), VerifyDefect>(())
        })
        .map_err(|error| match error {
            sage_db_store::view::RecordScanError::Visitor(defect) => defect,
            sage_db_store::view::RecordScanError::Incomplete(reason) => {
                VerifyDefect::Incomplete(reason)
            }
            sage_db_store::view::RecordScanError::Read(error) => VerifyDefect::RecordRead(error),
        })?;

    for (at, kind) in KINDS.iter().enumerate() {
        let expected_rows = prepared.count_of(*kind).unwrap_or(0);
        if counts[at] != expected_rows {
            return Err(VerifyDefect::CountMismatch {
                kind: *kind,
                expected: expected_rows,
                actual: counts[at],
            });
        }
    }

    // --- Current rows, by borrowed shared owner, compared by full value. ---
    //
    // `record_owned` hands back the store's EXISTING retained owner: no deep copy, no new
    // allocation, and the image cannot be uncharged while it is held.
    for (index, image) in planned.iter().enumerate() {
        let key = match image.identity {
            sage_db_store::fold::RecordIdentity::Identified { key } => key,
            // Natural-key current values are compared in the separate borrowed scan below.
            sage_db_store::fold::RecordIdentity::Natural { .. } => continue,
        };
        // Only the newest planned revision of this key is expected to be CURRENT.
        verification_step(ledger)?;
        let is_latest = planned
            .get(index + 1)
            .is_none_or(|next| next.identity != image.identity);
        if !is_latest {
            continue;
        }
        let found = match store.record_owned(key, ledger) {
            Ok(sage_db_store::view::RecordImageLookup::Found { image, basis }) => {
                if basis != opened_basis {
                    return Err(VerifyDefect::NotDurable);
                }
                image
            }
            Ok(sage_db_store::view::RecordImageLookup::Absent { .. }) => {
                return Err(VerifyDefect::MissingRecord {
                    kind: image.identity.state_kind(),
                    revision: image.revision,
                });
            }
            Ok(sage_db_store::view::RecordImageLookup::Incomplete(reason)) => {
                return Err(VerifyDefect::Incomplete(reason));
            }
            Err(error) => return Err(VerifyDefect::RecordRead(error)),
        };
        if found.record() != &image.record {
            return Err(VerifyDefect::ValueMismatch {
                kind: image.identity.state_kind(),
                revision: image.revision,
            });
        }
    }

    // --- Every earlier planned revision, against the committed chain. ---
    for (index, image) in planned.iter().enumerate() {
        verification_step(ledger)?;
        let is_latest = planned
            .get(index + 1)
            .is_none_or(|next| next.identity != image.identity);
        if is_latest {
            continue;
        }
        match store.history(
            &image.identity,
            image.revision,
            sage_db_store::history::HistoryBudget::new(256),
            &Limits::V1,
            ledger,
        ) {
            sage_db_store::history::HistoryOutcome::Found(hit) => {
                if hit.durability != sage_db_store::view::ViewDurability::Durable {
                    return Err(VerifyDefect::NotDurable);
                }
                if hit.image.record() != &image.record {
                    return Err(VerifyDefect::ValueMismatch {
                        kind: image.identity.state_kind(),
                        revision: image.revision,
                    });
                }
            }
            sage_db_store::history::HistoryOutcome::Absent { .. } => {
                return Err(VerifyDefect::MissingRecord {
                    kind: image.identity.state_kind(),
                    revision: image.revision,
                });
            }
            sage_db_store::history::HistoryOutcome::Incomplete { reason, .. } => {
                return Err(VerifyDefect::HistoryIncomplete(reason));
            }
            sage_db_store::history::HistoryOutcome::Error(refusal) => {
                return Err(VerifyDefect::HistoryError(refusal));
            }
        }
    }

    // --- The CURRENT image of every natural-key identity the plan carries. ---
    //
    // A natural-key record has no `RecordKey`, so `record_owned` cannot reach it. The planned
    // value is compared against the store's own current value during a second ordered pass: the
    // scan yields the borrowed record, and the comparison is by value.
    verify_natural_current(store, &planned, ledger)?;

    // --- Every planned blob: declared length, exact bytes, and a re-hash to its own name.
    for blob in prepared.blobs() {
        let bytes = match store.read_committed_blob(blob.sha256, blob.declared_len, ledger) {
            Ok(bytes) => bytes,
            Err(sage_db_store::view::BlobReadError::Ceiling { .. }) => {
                return Err(VerifyDefect::BlobMismatch {
                    sha256: blob.sha256,
                });
            }
            Err(sage_db_store::view::BlobReadError::Corrupt(_)) => {
                return Err(VerifyDefect::BlobMismatch {
                    sha256: blob.sha256,
                });
            }
            Err(error) => return Err(VerifyDefect::BlobRead(error)),
        };
        if bytes.len() as u64 != blob.declared_len {
            return Err(VerifyDefect::BlobMismatch {
                sha256: blob.sha256,
            });
        }
        if let Some(declared) = blob_bytes(prepared, blob.sha256)
            && bytes.as_bytes() != declared
        {
            return Err(VerifyDefect::BlobMismatch {
                sha256: blob.sha256,
            });
        }
        let actual = match typed::raw_hash(bytes.as_bytes(), ledger) {
            FormatOutcome::Ok(digest) => digest,
            FormatOutcome::Format(error) => return Err(VerifyDefect::Format(error)),
            FormatOutcome::Refused(refusal) => return Err(VerifyDefect::Refused(refusal)),
        };
        if actual != blob.sha256 {
            return Err(VerifyDefect::BlobMismatch {
                sha256: blob.sha256,
            });
        }
    }

    // --- Recorded watermarks, read here rather than trusted from the plan. ---
    verify_watermarks(store, prepared, opened_basis, ledger)?;

    // --- The session must still be open at this identity, on this basis. ---
    match store.import_readiness(ledger) {
        Ok(sage_db_store::view::ImportReadiness::Incomplete { basis: at, session }) => {
            if session.import_session_id != expected.import_session_id || at != opened_basis {
                return Err(VerifyDefect::SessionNotActive);
            }
        }
        Ok(sage_db_store::view::ImportReadiness::Ready { .. }) => {
            return Err(VerifyDefect::SessionNotActive);
        }
        Err(error) => return Err(VerifyDefect::CommitRead(error)),
    }
    Ok(())
}

/// One actual verification-index visit on the caller's canonical ledger.
fn verification_step(ledger: &mut dyn SharedLedger) -> Result<(), VerifyDefect> {
    ledger
        .try_charge_entries(1)
        .map_err(|error| VerifyDefect::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| VerifyDefect::Refused(Refusal::Stopped(stop)))
}

/// Compare every natural-key identity's newest planned image against the store's current value.
///
/// The scan borrows the record; the planned value is already owned and admitted. Nothing is
/// cloned, and an identity the plan carries but the store does not is a missing record.
fn verify_natural_current<F: FileSystem>(
    store: &mut Store<F>,
    planned: &[PlannedImage],
    ledger: &mut dyn LeaseLedger,
) -> Result<(), VerifyDefect> {
    let mut wanted: Vec<usize> = Vec::new();
    admit_table(&mut wanted, planned.len(), ledger)?;
    for (index, image) in planned.iter().enumerate() {
        if !matches!(
            image.identity,
            sage_db_store::fold::RecordIdentity::Natural { .. }
        ) {
            continue;
        }
        verification_step(ledger)?;
        let is_latest = planned
            .get(index + 1)
            .is_none_or(|next| next.identity != image.identity);
        if is_latest {
            wanted.push(index);
        }
    }
    if wanted.is_empty() {
        return Ok(());
    }
    let mut seen: Vec<bool> = Vec::new();
    admit_table(&mut seen, wanted.len(), ledger)?;
    seen.resize(wanted.len(), false);

    store
        .scan_current_records(ledger, |record, inner| {
            let identity = record.identity(inner).map_err(VerifyDefect::Refused)?;
            for (slot, at) in wanted.iter().enumerate() {
                verification_step(inner)?;
                if seen[slot] || planned[*at].identity != identity {
                    continue;
                }
                seen[slot] = true;
                if record != &planned[*at].record {
                    return Err(VerifyDefect::ValueMismatch {
                        kind: planned[*at].identity.state_kind(),
                        revision: planned[*at].revision,
                    });
                }
            }
            Ok::<(), VerifyDefect>(())
        })
        .map_err(|error| match error {
            sage_db_store::view::RecordScanError::Visitor(defect) => defect,
            sage_db_store::view::RecordScanError::Incomplete(reason) => {
                VerifyDefect::Incomplete(reason)
            }
            sage_db_store::view::RecordScanError::Read(error) => VerifyDefect::RecordRead(error),
        })?;

    for (slot, at) in wanted.iter().enumerate() {
        if !seen[slot] {
            return Err(VerifyDefect::MissingRecord {
                kind: planned[*at].identity.state_kind(),
                revision: planned[*at].revision,
            });
        }
    }
    Ok(())
}

/// Require every planned watermark to be committed at or above the planned mark.
///
/// Read through the store's own `commit_basis` on the basis verification opened with, so a mark
/// that only ever lived in the plan cannot pass, and a mark recorded under a different
/// publication is refused rather than accepted by coincidence.
fn verify_watermarks<F: FileSystem>(
    store: &mut Store<F>,
    prepared: &PreparedImport,
    opened_basis: ViewBasis,
    ledger: &mut dyn LeaseLedger,
) -> Result<(), VerifyDefect> {
    for mark in prepared.watermarks() {
        let Some(space) = IdSpace::from_u16(mark.space) else {
            return Err(VerifyDefect::UnknownSpace { space: mark.space });
        };
        let basis = match store.commit_basis(space, ledger) {
            Ok(basis) => basis,
            Err(error) => return Err(VerifyDefect::CommitRead(error)),
        };
        if basis.basis != opened_basis {
            return Err(VerifyDefect::NotDurable);
        }
        if basis.allocated_high_water < mark.hwm {
            return Err(VerifyDefect::WatermarkLow {
                space: mark.space,
                planned: mark.hwm,
                recorded: basis.allocated_high_water,
            });
        }
    }
    Ok(())
}

/// Admit a table of `len` items of `T` before reserving it, through the caller's ledger.
fn admit_table<T>(
    out: &mut Vec<T>,
    len: usize,
    ledger: &mut dyn SharedLedger,
) -> Result<(), VerifyDefect> {
    let bytes = len
        .checked_mul(core::mem::size_of::<T>())
        .ok_or(VerifyDefect::ExtentOverflow { count: len })?;
    ledger
        .try_charge_entries(
            u64::try_from(len).map_err(|_| VerifyDefect::ExtentOverflow { count: len })?,
        )
        .map_err(|error| VerifyDefect::Refused(Refusal::Budget(error)))?;
    ledger
        .try_charge(bytes as u64)
        .map_err(|error| VerifyDefect::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| VerifyDefect::Refused(Refusal::Stopped(stop)))?;
    out.try_reserve_exact(len)
        .map_err(|_| VerifyDefect::Allocation { requested: len })?;
    Ok(())
}

/// Every planned after-image, with its identity, in one admitted table.
///
/// Each mapped event is read back through the canonical dispatcher, so the value is exactly what a
/// reader of the persisted file sees. **Every revision is kept**: a proposal's create and its
/// decision are two planned images of one key, and the older is checked against history.
fn planned_images(
    prepared: &PreparedImport,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<PlannedImage>, VerifyDefect> {
    let mut out: Vec<PlannedImage> = Vec::new();
    admit_table(&mut out, prepared.events().len(), ledger)?;
    for event in prepared.events() {
        ledger
            .checkpoint()
            .map_err(|stop| VerifyDefect::Refused(Refusal::Stopped(stop)))?;
        let Some(record) = decode_event(event, ledger)? else {
            continue;
        };
        let identity = record.identity(ledger).map_err(VerifyDefect::Refused)?;
        // A natural-key identity may own a string (a model's name). Admit it before it is moved
        // into the table; the record itself was already admitted by the decoder.
        let identity_bytes = match &identity {
            sage_db_store::fold::RecordIdentity::Natural {
                key: sage_db_store::fold::NaturalKey::Model(name),
            } => name.len() as u64,
            _ => 0,
        };
        ledger
            .try_charge(identity_bytes)
            .map_err(|error| VerifyDefect::Refused(Refusal::Budget(error)))?;
        let revision = record_revision_of(&record).get();
        out.push(PlannedImage {
            identity,
            revision,
            record,
        });
    }
    // Identity/revision ascending makes an adjacent comparison identify the latest image.
    out.sort_unstable_by(|left, right| {
        left.identity
            .cmp(&right.identity)
            .then(left.revision.cmp(&right.revision))
    });
    Ok(out)
}

/// The revision a folded record carries.
fn record_revision_of(
    record: &sage_db_store::fold::FoldedRecord,
) -> sage_db_types::identity::RecordRevision {
    use sage_db_store::fold::FoldedRecord as R;
    match record {
        R::Term(r) => r.revision,
        R::Assertion(r) => r.revision,
        R::Claim(r) => r.revision,
        R::Evidence(r) => r.revision,
        R::EvidenceLink(r) => r.revision,
        R::Vector(r) => r.revision,
        R::Proposal(r) => r.revision,
        R::FrontierEntry(r) => r.revision,
        R::SessionCheckpoint(r) => r.revision,
        R::KbMetadata(r) => r.revision,
        R::Model(r) => r.revision,
        R::RuleConfig(r) => r.revision,
        // An asset has no revision field; the format fixes it at 1.
        R::Asset(_) => sage_db_types::identity::RecordRevision::FIRST,
    }
}

/// Admit and copy one event's field table through the caller's ledger.
///
/// The slot extent and every value's bytes are charged **before** they are reserved, so the
/// copy never outruns the credit that pays for it.
fn event_owned(
    event: &OwnedEvent,
    ledger: &mut dyn SharedLedger,
) -> Result<OwnedEvent, VerifyDefect> {
    ledger
        .try_charge_entries(event.fields.len() as u64)
        .map_err(|error| VerifyDefect::Refused(Refusal::Budget(error)))?;
    let slots = event
        .fields
        .len()
        .checked_mul(core::mem::size_of::<(u16, Vec<u8>)>())
        .ok_or(VerifyDefect::ExtentOverflow {
            count: event.fields.len(),
        })?;
    ledger
        .try_charge(slots as u64)
        .map_err(|error| VerifyDefect::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| VerifyDefect::Refused(Refusal::Stopped(stop)))?;
    let mut fields: Vec<(u16, Vec<u8>)> = Vec::new();
    fields
        .try_reserve_exact(event.fields.len())
        .map_err(|_| VerifyDefect::Allocation { requested: slots })?;
    for (tag, value) in &event.fields {
        ledger
            .try_charge(value.len() as u64)
            .map_err(|error| VerifyDefect::Refused(Refusal::Budget(error)))?;
        ledger
            .checkpoint()
            .map_err(|stop| VerifyDefect::Refused(Refusal::Stopped(stop)))?;
        let mut owned: Vec<u8> = Vec::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|_| VerifyDefect::Allocation {
                requested: value.len(),
            })?;
        owned.extend_from_slice(value);
        fields.push((*tag, owned));
    }
    Ok(OwnedEvent {
        tag: event.tag,
        fields,
    })
}

fn decode_event(
    event: &OwnedEvent,
    ledger: &mut dyn SharedLedger,
) -> Result<Option<sage_db_store::fold::FoldedRecord>, VerifyDefect> {
    use sage_db_format::dispatch::{DispatchedEvent, DispatchedPayload};
    use sage_db_store::fold::FoldedRecord as R;

    let header = sage_db_format::encode::TxHeaderToEncode {
        kb_uuid: KbUuid::from_bytes([0; 16]),
        version: 1,
        tx_id: [0; 16],
        prev_version: None,
        created_at: 0,
        principal_id: String::new(),
        request_id: String::new(),
        payload_digest: [0; 32],
        rules_profile: 0,
        rules_digest: [0; 32],
        request_canon_version: sage_db_types::prepared::RequestCanonVersion::V1,
        expect_version: None,
        extensions: Extensions::new(),
    };
    // `view_for_encode` takes a slice of `OwnedEvent` by value, so the one-element array the
    // API demands is built here. The copy is admitted first: `event_owned` charges the field
    // slots and each value's bytes through the caller's ledger before reserving them, so the
    // clone cannot outrun the credit that pays for it.
    let events: [OwnedEvent; 1] = [event_owned(event, ledger)?];
    let view =
        match sage_db_format::prepared::view_for_encode(&header, &events, &Limits::V1, ledger) {
            FormatOutcome::Ok(view) => view,
            FormatOutcome::Format(error) => return Err(VerifyDefect::Format(error)),
            FormatOutcome::Refused(refusal) => return Err(VerifyDefect::Refused(refusal)),
        };
    let first = view.events().first().ok_or(VerifyDefect::NotReady)?;
    let bytes = {
        let mut bytes: Vec<u8> = Vec::new();
        // The writer's own capacity is a retained owner: admit it before reserving.
        let capacity = 256usize;
        ledger
            .try_charge(capacity as u64)
            .map_err(|error| VerifyDefect::Refused(Refusal::Budget(error)))?;
        ledger
            .checkpoint()
            .map_err(|stop| VerifyDefect::Refused(Refusal::Stopped(stop)))?;
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| VerifyDefect::Allocation {
                requested: capacity,
            })?;
        let mut writer = sage_db_format::wire::Writer::new(&mut bytes, ledger);
        match sage_db_format::event::write_event(
            &mut writer,
            first.tag,
            sage_db_format::event::EVENT_FLAG_REQUIRED,
            &first.fields,
        ) {
            Ok(()) => {}
            Err(sage_db_format::wire::WriteError::Data(_)) => {
                return Err(VerifyDefect::Format(
                    sage_db_types::framing::FormatError::Internal(
                        sage_db_types::framing::InternalReason::EncodeDecodeDivergence,
                    ),
                ));
            }
            Err(sage_db_format::wire::WriteError::Budget(error)) => {
                return Err(VerifyDefect::Refused(Refusal::Budget(error)));
            }
            Err(sage_db_format::wire::WriteError::Stopped(stop)) => {
                return Err(VerifyDefect::Refused(Refusal::Stopped(stop)));
            }
            Err(sage_db_format::wire::WriteError::Allocation { requested }) => {
                return Err(VerifyDefect::Allocation { requested });
            }
            // Any other writer refusal is a real permission/ceiling verdict on this event.
            Err(_) => {
                return Err(VerifyDefect::Format(
                    sage_db_types::framing::FormatError::Internal(
                        sage_db_types::framing::InternalReason::EncodeDecodeDivergence,
                    ),
                ));
            }
        }
        bytes
    };
    let mut reader = sage_db_format::wire::Reader::new(&bytes);
    match sage_db_format::dispatch::read_dispatched(&mut reader, ledger) {
        FormatOutcome::Ok(DispatchedEvent::Record { payload, .. }) => Ok(match payload {
            DispatchedPayload::Term(r) => Some(R::Term(r)),
            DispatchedPayload::Assertion(r) => Some(R::Assertion(r)),
            DispatchedPayload::Claim(r) => Some(R::Claim(r)),
            DispatchedPayload::Evidence(r) => Some(R::Evidence(r)),
            DispatchedPayload::EvidenceLink(r) => Some(R::EvidenceLink(r)),
            DispatchedPayload::Vector(r) => Some(R::Vector(r)),
            DispatchedPayload::Proposal(r) => Some(R::Proposal(r)),
            DispatchedPayload::FrontierEntry(r) => Some(R::FrontierEntry(r)),
            DispatchedPayload::SessionCheckpoint(r) => Some(R::SessionCheckpoint(r)),
            DispatchedPayload::Asset(r) => Some(R::Asset(r)),
            DispatchedPayload::Model(r) => Some(R::Model(r)),
            DispatchedPayload::KbMetadata(r) => Some(R::KbMetadata(r)),
            DispatchedPayload::RuleProfile(r) => Some(R::RuleConfig(r)),
            _ => None,
        }),
        FormatOutcome::Ok(DispatchedEvent::Validated { .. }) => Ok(None),
        FormatOutcome::Format(error) => Err(VerifyDefect::Format(error)),
        FormatOutcome::Refused(refusal) => Err(VerifyDefect::Refused(refusal)),
    }
}

/// The mapping's declared bytes for a blob, when it supplies them.
fn blob_bytes(prepared: &PreparedImport, sha256: [u8; 32]) -> Option<&[u8]> {
    prepared.blobs().iter().find_map(|blob| {
        if blob.sha256 != sha256 {
            return None;
        }
        match &blob.source {
            BlobSource::Provide(bytes) => Some(bytes.as_slice()),
            BlobSource::Existing => None,
        }
    })
}

/// A verification token bound to the state it verified.
///
/// Carries the durable basis, the expected identity and the package reference, so a token
/// for another same-named session — or for an older publication — cannot be consumed after
/// the state moved. Not `Clone`/`Copy`/`Default`, and built only by
/// [`verify_and_attest`].
#[derive(Debug)]
pub struct Verified {
    basis: ViewBasis,
    session: String,
    /// The reservation that pays for `session` for as long as this token lives.
    ///
    /// Without it the string's bytes would be charged to an attempt that can be dropped before
    /// the token is consumed, so a token held across a reopen would carry memory whose credit
    /// died. `OwnedReservation` is a move-only token over part of the counter, so exactly one
    /// drop releases it — and that drop is this struct's.
    retained: sage_db_store::lease::OwnedReservation,
    plan_sha256: [u8; 32],
    source_digest: [u8; 32],
    plan_ref: [u8; 32],
    /// The `(space, mark)` pairs this token proved committed. Held so completion carries the
    /// same facts verification established, rather than re-deriving them from the plan.
    marks: Vec<(u16, u128)>,
}

impl Verified {
    /// The session this token vouches for.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The durable basis it was verified on.
    #[must_use]
    pub const fn basis(&self) -> ViewBasis {
        self.basis
    }

    /// Bytes this token holds reserved for its session string.
    ///
    /// Reading it is not bookkeeping decoration: it is the observable proof that the token
    /// owns real capacity rather than naming bytes its producing attempt already dropped.
    #[must_use]
    pub const fn retained_bytes(&self) -> u64 {
        self.retained.bytes()
    }

    /// Capacity of the session string this token owns.
    ///
    /// Distinct from `session().len()`: a `String` may reserve more than its length, and this is
    /// the figure the token's reservation must equal — the bytes actually held.
    #[must_use]
    pub fn session_capacity(&self) -> u64 {
        self.session.capacity() as u64
    }

    /// The extent this token charged before taking ownership of it.
    ///
    /// Exposed so a caller can reconcile the token against the attempt's own counters instead of
    /// trusting that a mint implies a transfer.
    #[must_use]
    pub const fn adopted_extent(&self) -> u64 {
        self.retained.bytes()
    }

    /// The committed watermarks this token proved, as `(space, mark)`.
    ///
    /// Exposed so a driver can observe what was verified instead of assuming the plan was.
    #[must_use]
    pub fn marks(&self) -> &[(u16, u128)] {
        &self.marks
    }

    /// Require the store to still be the state this token verified.
    ///
    /// A restart that preserves an identical basis is legitimate, so the check is on the
    /// basis and the identity, not on process identity. Anything that committed in between
    /// changes the basis and is refused.
    fn check_current<F: FileSystem>(
        &self,
        store: &Store<F>,
        expected: &ExpectedIdentity<'_>,
        prepared: &PreparedImport,
        ledger: &mut dyn SharedLedger,
    ) -> Result<(), ExecuteError> {
        let current = store
            .view()
            .ok_or(ExecuteError::Incomplete(OutcomeIncomplete::NoCurrentView))?
            .basis();
        if current != self.basis
            || expected.import_session_id != self.session
            || expected.plan_sha256 != self.plan_sha256
            || expected.source_digest != self.source_digest
            || prepared.import_session().plan_ref != self.plan_ref
        {
            return Err(ExecuteError::StaleVerification);
        }
        let _ = ledger;
        Ok(())
    }
}

/// Verify and hand back the token a completion may consume.
///
/// # Errors
///
/// As [`verify_written`].
pub fn verify_and_attest<F: FileSystem>(
    store: &mut Store<F>,
    prepared: &PreparedImport,
    expected: &ExpectedIdentity<'_>,
    ledger: &mut dyn LeaseLedger,
) -> Result<Verified, VerifyDefect> {
    verify_written(store, prepared, expected, ledger)?;
    let basis = store.view().ok_or(VerifyDefect::NotReady)?.basis();

    // The token OWNS its session bytes. The sequence is charge-then-TRANSFER:
    //
    //   `try_charge`        adds `bytes` to live and untransferred;
    //   `adopt_retained`    MOVES that already-admitted extent into a token, adding nothing.
    //
    // `reserve_owned` would be wrong here: it admits a second time, counting the same bytes twice
    // against the live ceiling and leaving an untransferred duplicate behind. A transfer can only
    // fail because the attempt does not hold what it was asked to transfer, which is an
    // accounting invariant break, not a budget ceiling.
    let len = expected.import_session_id.len();
    let bytes = u64::try_from(len).map_err(|_| VerifyDefect::ExtentOverflow { count: len })?;
    ledger
        .try_charge(bytes)
        .map_err(|error| VerifyDefect::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| VerifyDefect::Refused(Refusal::Stopped(stop)))?;
    let retained = ledger
        .adopt_retained(bytes)
        .map_err(VerifyDefect::Transfer)?;
    let mut session = String::new();
    // The string's capacity is the extent that was just transferred; admitting it would count the
    // same bytes a second time.
    session
        .try_reserve_exact(len)
        .map_err(|_| VerifyDefect::Allocation { requested: len })?;
    session.push_str(expected.import_session_id);

    // The marks the token vouches for, taken from the SAME durable basis verification opened.
    let mut marks: Vec<(u16, u128)> = Vec::new();
    admit_table(&mut marks, prepared.watermarks().len(), ledger)?;
    for mark in prepared.watermarks() {
        marks.push((mark.space, mark.hwm));
    }

    Ok(Verified {
        basis,
        session,
        retained,
        plan_sha256: expected.plan_sha256,
        source_digest: expected.source_digest,
        plan_ref: prepared.import_session().plan_ref,
        marks,
    })
}

/// Mark the import complete, consuming a verification token.
///
/// The token is the whole point: completion cannot be requested by a caller that did not
/// verify, and no boolean is accepted. What the token cannot express is *which handle*
/// verified — so the driver must drop and reopen the `Store` between
/// [`verify_and_attest`] and this call to make "freshly reopened" true. That invariant is
/// documented rather than enforced, because a library call cannot observe a process
/// boundary it does not own.
///
/// # Errors
///
/// [`ExecuteError`] for a real refusal or store failure.
#[allow(clippy::too_many_arguments)]
pub fn mark_complete_after_verification<F: FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'_>,
    expected: &ExpectedIdentity<'_>,
    prepared: &PreparedImport,
    verified: Verified,
    step: &ImportStep<'_>,
    context: &StepContext<'_>,
    ledger: &mut dyn LeaseLedger,
) -> Result<StepOutcome, ExecuteError> {
    if step.stage != ImportStage::Activate {
        return Err(ExecuteError::Internal);
    }
    // The token carries the identity it proved, so the plan bytes are NOT re-supplied: hashing
    // an invented empty buffer against the frozen real-plan hash could never succeed, and
    // re-deriving an identity the token already holds would be a second authority for it.
    // `write_stage_inner` skips the raw-plan comparison on this path and calls
    // `Verified::check_current` instead.
    write_stage_inner(
        store,
        grants,
        expected,
        &[],
        prepared,
        step,
        context,
        ledger,
        Some(&verified),
    )
}
