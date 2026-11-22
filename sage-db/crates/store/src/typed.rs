//! The typed-event preparation bridge a committing caller goes through.
//!
//! # What this module is
//!
//! A writer holds typed records — full concept/assertion after-images, source assets,
//! frontier and session state, proposals, allocation marks and request outcomes — and needs the attempt-lifetime form the store
//! actually persists: [`OwnedEvent`], plus the typed decode of the event it just
//! prepared. This module is that boundary, and it is deliberately **thin**: every
//! record is encoded by the existing canonical family writer and every payload is
//! produced by the existing dispatch seam. There is no second record definition, no
//! second field table and no second serialisation here.
//!
//! # Why preparation is "encode, then read back"
//!
//! The owned form must contain the **exact raw field value bytes** a reader will see,
//! not a value the caller's struct re-renders. The only way to guarantee that is to run
//! the same encoder that later runs for the file and then read its output through the
//! canonical envelope reader. A hand-written "struct to field bytes" mapping here would
//! be a second encoding path that could disagree with the file bytes while looking
//! right — exactly the drift this crate refuses. The cost is one admitted scratch
//! buffer; the benefit is that "prepared" and "persisted" cannot diverge.
//!
//! [`copy_payload`] is the same path with the canonical dispatcher at the end instead
//! of an owned copy: it is **deliberately fallible** and admitted through the ledger,
//! never an owning `.clone()` with a charge applied afterwards. The caller's typed
//! input selects which event is encoded; nothing about the record's fields or audit is
//! altered on the way through.
//!
//! # Requests are re-exported, not rebuilt
//!
//! [`encode_request`], [`decode_request`], [`request_digest_of`] and their types are
//! re-exported from `sage_db_format::request`. They are the one P1 request codec; a
//! store-local copy would be a second frame authority.
//!
//! # Result payloads
//!
//! [`result_payload`] encodes the §4.2 result blob: `result_kind = 0`,
//! `commit_version`, the affected `(RecordKey, revision)` pairs and the advanced
//! `(space, high_water_mark)` counters, with no timestamps and no digests. Its failures
//! are [`ResultPreparationError`], not a format class: an unordered generated list or a
//! derived extent that does not fit is a preparing defect, and dressing it as
//! `Corruption`, `Budget` or an allocation refusal would send an operator to the wrong
//! remedy. A `Rejected`/`Conflicted` proposal is still a committed transaction and
//! therefore still `kind 0`; this function does not reject on proposal status.

use sage_db_types::framing::{FormatError, FormatOutcome, InternalReason, Refusal};
use sage_db_types::ledger::{BudgetError, SharedLedger};
use sage_db_types::persist::{
    IdAllocationEvent, ProposalRecord, RequestOutcomeEvent, RevPair, Watermark,
};
use sage_db_types::prepared::OwnedEvent;
use sage_db_types::query::ControlStop;

/// Re-export: the canonical assertion lifecycle selector.
pub use sage_db_format::assertion::AssertionEvent;
/// Re-export: the canonical claim event selector.
pub use sage_db_format::claim::ClaimEvent;
/// Re-export: the canonical decoded payload for one dispatched event.
pub use sage_db_format::dispatch::DispatchedPayload;
/// Re-export: identities declared by the SAME committed rule configuration.
pub use sage_db_format::rules_read::DeclaredRules;
/// Re-export: verified rule profile and both independently checked digest scopes.
pub use sage_db_format::rules_read::VerifiedRules;
/// Re-export: the canonical Rules parser, integrity and support verifier.
/// This reads real blob bytes; it is not a comparison against a caller-selected profile.
pub use sage_db_format::rules_read::verify_rules;
/// Re-export: the canonical term lifecycle selector.
pub use sage_db_format::term::TermEvent;

/// Canonical persisted-string ceilings, re-exported for preparing-layer input
/// checks so a client error does not reach the encoder as an internal defect.
pub use sage_db_format::field::{
    MAX_ATOM_BYTES, MAX_DECLARED_AUTHOR, MAX_DOMAINS, MAX_IDENTITY_BYTES, MAX_PRINCIPAL_ID_BYTES,
    MAX_REQUEST_ID_BYTES, MAX_SLUG_BYTES, MAX_TERM_DOMAIN_BYTES, MAX_TEXT_BYTES, MAX_TRIGGER_BYTES,
    MAX_TRIGGERS,
};
/// Re-export: the canonical-request version this build implements.
pub use sage_db_format::request::CANON_VERSION;
/// Re-export: a decoded request frame.
pub use sage_db_format::request::DecodedRequest;
/// Re-export: which canonical-form rule a request input broke.
pub use sage_db_format::request::InvalidArgument;
/// Re-export: the registered whole-frame byte ceiling.
pub use sage_db_format::request::MAX_FRAME_BYTES;
/// Re-export: the registered per-operation payload ceiling.
pub use sage_db_format::request::MAX_PAYLOAD;
/// Re-export: why a request frame is not acceptable.
pub use sage_db_format::request::RequestError;
/// Re-export: a request frame as a caller supplies it.
pub use sage_db_format::request::RequestFrame;
/// Re-export: which §4.7.2 ceiling a request exceeded.
pub use sage_db_format::request::RequestLimit;
/// Re-export: one opaque operation in a request frame.
pub use sage_db_format::request::RequestOp;
/// Re-export: a request operation's complete result.
pub use sage_db_format::request::RequestOutcome;
/// Re-export: the one P1 request decoder.
pub use sage_db_format::request::decode_request;
/// Re-export: the one P1 request encoder.
pub use sage_db_format::request::encode_request;
/// Re-export: the one P1 request digest.
pub use sage_db_format::request::request_digest_of;

/// Borrowed, fully validated canonical persisted result payload.
pub use sage_db_format::result::ResultPayload;
/// Precise persisted-result decoding refusal.
pub use sage_db_format::result::ResultReadError;
/// Parse canonical persisted result bytes without allocating or copying rows.
pub use sage_db_format::result::read_result_payload;

/// One typed event a caller wants prepared for an attempt.
///
/// The variant **is** the event selection: a claim's event tag, whether a proposal is a
/// submission or a decision, and the two structural events each come from the caller's
/// own typed value. Nothing here reinterprets a record's fields.
#[derive(Clone, Copy, Debug)]
pub enum EventInput<'a> {
    /// A complete term after-image, encoded by the canonical term writer.
    Term(TermEvent, &'a sage_db_types::persist::TermRecord),
    /// An authored assertion after-image; this bridge performs no graph admission.
    Assertion(
        AssertionEvent,
        &'a sage_db_types::persist::AssertionRecordFull,
    ),
    /// An immutable asset declaration referring to its actual content hash.
    Asset(&'a sage_db_types::persist::AssetRecord),
    /// A complete frontier after-image.
    Frontier(&'a sage_db_types::persist::FrontierEntryRecord),
    /// An update to the metadata singleton established by Genesis.
    KbMetadata(&'a sage_db_types::persist::KbMetadataRecord),
    /// A stage record for a separately admitted administrative import.
    /// Encoding this record alone neither fences ordinary commands nor completes an import.
    ImportSession(&'a sage_db_types::persist::ImportSessionRecord),
    /// A claim event and its complete after-image.
    Claim(ClaimEvent, &'a sage_db_types::persist::ClaimRecord),
    /// A proposal submission (`decided == false`) or decision (`decided == true`).
    Proposal {
        /// Which proposal event this is.
        decided: bool,
        /// The complete after-image.
        record: &'a ProposalRecord,
    },
    /// A user-facing session checkpoint, distinct from a storage checkpoint.
    SessionCheckpoint(&'a sage_db_types::persist::SessionCheckpointRecord),
    /// One allocation space's advanced high-water mark.
    IdAllocation(&'a IdAllocationEvent),
    /// A recorded request outcome.
    Outcome(&'a RequestOutcomeEvent),
}

/// Why a result payload could not be prepared.
///
/// Store-local because none of these is a verdict about stored bytes and none is a
/// resource refusal in the ledger's sense: an unordered or over-wide *generated* list is
/// the preparing layer's own defect, and reporting it as `Budget`/`Allocation`/`Corrupt`
/// would name the wrong remedy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResultPreparationError {
    /// The ledger refused, or the caller asked to stop.
    Refused(Refusal),
    /// A generated value its own canonical encoder rejects.
    Format(FormatError),
    /// `commit_version == 0`, which no persisted outcome may claim.
    InvalidVersion,
    /// `affected` was not strictly ascending, or a pair repeated.
    UnorderedAffected,
    /// `counters` was not strictly ascending by space, or a space repeated.
    UnorderedCounters,
    /// A generated `affected` key fell outside its kind's bound.
    InvalidAffectedKey {
        /// Position in the caller's slice.
        index: usize,
    },
    /// A generated watermark exceeded its space's maximum id.
    WatermarkBeyondSpace {
        /// Position in the caller's slice.
        index: usize,
    },
    /// A declared count does not fit the `u32` prefix.
    CountOverflow,
    /// A derived byte extent does not fit `u64`.
    ExtentOverflow,
}

impl From<BudgetError> for ResultPreparationError {
    fn from(error: BudgetError) -> Self {
        Self::Refused(Refusal::Budget(error))
    }
}

impl From<ControlStop> for ResultPreparationError {
    fn from(stop: ControlStop) -> Self {
        Self::Refused(Refusal::Stopped(stop))
    }
}

/// Encode one typed input into an attempt-owned event.
///
/// The event is written by the canonical family writer and then read back through the
/// canonical envelope reader, so `fields` holds the exact raw value bytes a later reader
/// of the file will see. Optional and advisory fields are preserved because the writer
/// emits them and the copy keeps them verbatim — nothing is dropped, defaulted or
/// normalised.
///
/// # Errors
///
/// [`FormatOutcome::Format`] when the caller's typed value is not a legal after-image for
/// the selected event (the encoder's own refusal, mapped by the canonical error mapper);
/// [`FormatOutcome::Refused`] for a budget, cancellation or allocation refusal.
pub fn prepare_event(
    input: EventInput<'_>,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<OwnedEvent> {
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }
    let scratch = match encode_input(&input, ledger) {
        FormatOutcome::Ok(bytes) => bytes,
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    };

    let event = match sage_db_format::event::read_event(
        &mut sage_db_format::wire::Reader::new(&scratch),
        ledger,
    ) {
        Ok(event) => event,
        Err(error) => return post_encode_read_error(error),
    };

    // Field count is bounded by `MAX_EVENT_FIELDS` (64) because the reader above already
    // refused anything larger, so the retained slot product is at most 64 * 32 bytes and
    // cannot overflow: the arithmetic is real but provably small, and inventing an
    // overflow arm here would be an error no input can reach.
    let count = event.fields().len();
    let slot_bytes = (count as u64) * (core::mem::size_of::<(u16, Vec<u8>)>() as u64);
    if let Err(error) = ledger.try_charge_entries(count as u64) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(error) = ledger.try_charge(slot_bytes) {
        return FormatOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }

    let mut fields: Vec<(u16, Vec<u8>)> = Vec::new();
    if fields.try_reserve_exact(count).is_err() {
        return FormatOutcome::Refused(Refusal::Allocation {
            requested: slot_bytes as usize,
        });
    }
    for field in event.fields() {
        let value = field.value();
        // Each value vector is its own retained allocation, admitted for its exact
        // extent before it is reserved.
        if let Err(error) = ledger.try_charge(value.len() as u64) {
            return FormatOutcome::Refused(Refusal::Budget(error));
        }
        if let Err(stop) = ledger.checkpoint() {
            return FormatOutcome::Refused(Refusal::Stopped(stop));
        }
        let mut owned = Vec::new();
        if owned.try_reserve_exact(value.len()).is_err() {
            return FormatOutcome::Refused(Refusal::Allocation {
                requested: value.len(),
            });
        }
        owned.extend_from_slice(value);
        fields.push((field.tag(), owned));
    }
    FormatOutcome::Ok(OwnedEvent {
        tag: event.tag(),
        fields,
    })
}

/// Encode one typed input and return the canonical decoded payload.
///
/// The decode runs the real dispatcher over the real bytes this call just wrote, so the
/// returned payload is exactly what a fresh decode of the same event yields. The clone is
/// fallible and admitted: a payload is built only after the ledger has charged the field
/// table and every retained value, never as an owning `.clone()` charged afterwards.
///
/// # Errors
///
/// As [`prepare_event`], plus [`FormatError::Internal`] if the dispatcher reports a
/// validated-only event for a family this module encodes to completion (a build defect,
/// never a file verdict).
pub fn copy_payload(
    input: EventInput<'_>,
    ledger: &mut dyn SharedLedger,
) -> FormatOutcome<DispatchedPayload> {
    if let Err(stop) = ledger.checkpoint() {
        return FormatOutcome::Refused(Refusal::Stopped(stop));
    }
    let scratch = match encode_input(&input, ledger) {
        FormatOutcome::Ok(bytes) => bytes,
        FormatOutcome::Format(error) => return FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    };
    let mut reader = sage_db_format::wire::Reader::new(&scratch);
    let dispatched = match sage_db_format::dispatch::read_dispatched(&mut reader, ledger) {
        FormatOutcome::Ok(dispatched) => dispatched,
        // These bytes were produced by this module's own canonical encoder, so a format
        // verdict here describes our frame, not the caller's disk: reporting a
        // restore-the-backup class would be a false alarm. Real refusals keep their
        // remedy; no budget or allocation refusal is invented.
        FormatOutcome::Format(_) => {
            return FormatOutcome::Format(FormatError::Internal(
                InternalReason::EncodeDecodeDivergence,
            ));
        }
        FormatOutcome::Refused(refusal) => return FormatOutcome::Refused(refusal),
    };
    match dispatched {
        sage_db_format::dispatch::DispatchedEvent::Record { payload, .. } => {
            FormatOutcome::Ok(payload)
        }
        // Every family encoded here has a complete decoder in the seam; reaching the
        // validated-only arm means the dispatcher and this module disagree, which is a
        // build defect and not a statement about the bytes.
        sage_db_format::dispatch::DispatchedEvent::Validated { .. } => FormatOutcome::Format(
            FormatError::Internal(InternalReason::EncodeDecodeDivergence),
        ),
    }
}

/// Undomained SHA-256, preserving budget and cancellation as distinct refusals.
///
/// # Errors
///
/// [`FormatOutcome::Refused`] with [`Refusal::Budget`] or [`Refusal::Stopped`]: a hash is
/// not a verdict about the bytes, so neither is reported as a format class.
pub fn raw_hash(bytes: &[u8], ledger: &mut dyn SharedLedger) -> FormatOutcome<[u8; 32]> {
    match sage_db_format::digest::raw_sha256_bounded(bytes, ledger) {
        Ok(digest) => FormatOutcome::Ok(digest),
        Err(sage_db_format::digest::BoundedHashError::Budget(error)) => {
            FormatOutcome::Refused(Refusal::Budget(error))
        }
        Err(sage_db_format::digest::BoundedHashError::Stopped(stop)) => {
            FormatOutcome::Refused(Refusal::Stopped(stop))
        }
    }
}

/// Encode the §4.2 result payload for a committed transaction.
///
/// Layout (little-endian, normative widths):
///
/// ```text
/// result_kind:u16 = 0
/// commit_version:u64
/// affected_count:u32 | (RecordKey:u16+u128, revision:u64)[affected_count]
/// counter_count:u32  | (space:u16, high_water_mark:u128)[counter_count]
/// ```
///
/// `affected` must be strictly ascending and `counters` strictly ascending by space: both
/// are sets, and a silently sorted copy would make two different caller sequences hash
/// equal. No timestamps, no digests and no `result_kind` other than `0` appear: a
/// rejected or conflicted proposal is still a committed transaction whose result blob has
/// kind `0`.
///
/// # Errors
///
/// [`ResultPreparationError`]. A budget/control refusal keeps its own remedy; an
/// unordered or over-wide generated list and a derived extent that does not fit are
/// preparing defects, never `Allocation` or `Corruption`.
pub fn result_payload(
    version: u64,
    affected: &[RevPair],
    counters: &[Watermark],
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<u8>, ResultPreparationError> {
    // An empty operation is still an operation: an already-cancelled attempt must be
    // observed even when both lists are empty.
    ledger.checkpoint()?;
    if version == 0 {
        return Err(ResultPreparationError::InvalidVersion);
    }
    if !affected.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err(ResultPreparationError::UnorderedAffected);
    }
    if !counters
        .windows(2)
        .all(|pair| pair[0].space < pair[1].space)
    {
        return Err(ResultPreparationError::UnorderedCounters);
    }
    for (index, pair) in affected.iter().enumerate() {
        if !pair.key.id_is_in_range() {
            return Err(ResultPreparationError::InvalidAffectedKey { index });
        }
    }
    for (index, counter) in counters.iter().enumerate() {
        if !counter.is_in_range() {
            return Err(ResultPreparationError::WatermarkBeyondSpace { index });
        }
    }

    let affected_count =
        u32::try_from(affected.len()).map_err(|_| ResultPreparationError::CountOverflow)?;
    let counter_count =
        u32::try_from(counters.len()).map_err(|_| ResultPreparationError::CountOverflow)?;
    // 2 (kind) + 8 (version) + 4 (affected count) + 4 (counter count), then 26 bytes per
    // affected pair and 18 per counter. Every step is checked: a real extent that does
    // not fit `usize` is a genuine `ExtentOverflow`, not an allocation refusal.
    let total = 2usize
        .checked_add(8)
        .and_then(|n| n.checked_add(4))
        .and_then(|n| n.checked_add(4))
        .and_then(|n| {
            (affected_count as usize)
                .checked_mul(26)
                .and_then(|pairs| n.checked_add(pairs))
        })
        .and_then(|n| {
            (counter_count as usize)
                .checked_mul(18)
                .and_then(|marks| n.checked_add(marks))
        })
        .ok_or(ResultPreparationError::ExtentOverflow)?;

    ledger.try_charge(total as u64)?;
    ledger.checkpoint()?;
    let mut out = Vec::new();
    if out.try_reserve_exact(total).is_err() {
        return Err(ResultPreparationError::Refused(Refusal::Allocation {
            requested: total,
        }));
    }
    {
        // The extent was admitted above; the writer draws from that credit and refuses
        // rather than overdrawing, so nothing is charged twice.
        let mut writer = sage_db_format::wire::Writer::prepaid(&mut out, ledger, total);
        let written = (|| -> Result<(), sage_db_format::wire::WriteError> {
            writer.u16(0)?;
            writer.u64(version)?;
            writer.count(affected.len())?;
            for pair in affected {
                sage_db_format::field::write_record_key(&mut writer, pair.key)?;
                sage_db_format::field::write_revision(&mut writer, pair.revision)?;
            }
            writer.count(counters.len())?;
            for counter in counters {
                sage_db_format::field::write_watermark(&mut writer, *counter)?;
            }
            Ok(())
        })();
        match written {
            Ok(()) => {}
            Err(sage_db_format::wire::WriteError::Data(inner)) => {
                // Every value was validated above, so a data refusal here would be a
                // boundary defect rather than caller input.
                return Err(ResultPreparationError::Format(FormatError::Internal(
                    sage_db_format::dispatch::encode_internal(inner),
                )));
            }
            Err(sage_db_format::wire::WriteError::Budget(error)) => {
                return Err(ResultPreparationError::Refused(Refusal::Budget(error)));
            }
            Err(sage_db_format::wire::WriteError::Stopped(stop)) => {
                return Err(ResultPreparationError::Refused(Refusal::Stopped(stop)));
            }
            Err(sage_db_format::wire::WriteError::Allocation { requested }) => {
                return Err(ResultPreparationError::Refused(Refusal::Allocation {
                    requested,
                }));
            }
            Err(
                sage_db_format::wire::WriteError::MissingRequiredField { .. }
                | sage_db_format::wire::WriteError::Internal { .. },
            ) => {
                return Err(ResultPreparationError::Format(FormatError::Internal(
                    InternalReason::EncodeDecodeDivergence,
                )));
            }
        }
    }
    // A prepayout check: the derived extent and the emitted bytes must agree, or the
    // `u32` counts would describe a different payload than the one hashed.
    if out.len() != total {
        return Err(ResultPreparationError::Format(FormatError::Internal(
            InternalReason::EncodeDecodeDivergence,
        )));
    }
    Ok(out)
}

/// Run the canonical family writer for one typed input.
fn encode_input(input: &EventInput<'_>, ledger: &mut dyn SharedLedger) -> FormatOutcome<Vec<u8>> {
    let result = match input {
        EventInput::Term(event, record) => sage_db_format::event::encode_with(ledger, |writer| {
            sage_db_format::term::write_term(writer, *event, record)
        }),
        EventInput::Assertion(event, record) => {
            sage_db_format::event::encode_with(ledger, |writer| {
                sage_db_format::assertion::write_assertion(writer, *event, record)
            })
        }
        EventInput::Asset(record) => sage_db_format::event::encode_with(ledger, |writer| {
            sage_db_format::natural::write_asset(writer, record)
        }),
        EventInput::Frontier(record) => sage_db_format::event::encode_with(ledger, |writer| {
            sage_db_format::collection::write_frontier(writer, record)
        }),
        EventInput::KbMetadata(record) => sage_db_format::event::encode_with(ledger, |writer| {
            sage_db_format::natural::write_kb_metadata(writer, record)
        }),
        EventInput::ImportSession(record) => sage_db_format::event::encode_with(ledger, |writer| {
            sage_db_format::natural::write_import_bookkeeping(writer, record)
        }),
        EventInput::Claim(event, record) => sage_db_format::event::encode_with(ledger, |writer| {
            sage_db_format::claim::write_claim(writer, *event, record)
        }),
        EventInput::Proposal { decided, record } => {
            sage_db_format::event::encode_with(ledger, |writer| {
                sage_db_format::collection::write_proposal(writer, *decided, record)
            })
        }
        EventInput::SessionCheckpoint(record) => {
            sage_db_format::event::encode_with(ledger, |writer| {
                sage_db_format::collection::write_checkpoint(writer, record)
            })
        }
        EventInput::IdAllocation(record) => sage_db_format::event::encode_with(ledger, |writer| {
            sage_db_format::structural::write_id_allocation(writer, record)
        }),
        EventInput::Outcome(record) => sage_db_format::event::encode_with(ledger, |writer| {
            sage_db_format::natural::write_request_outcome(writer, record)
        }),
    };
    match result {
        Ok(bytes) => FormatOutcome::Ok(bytes),
        Err(error) => from_write(error),
    }
}

/// Map a family write failure through the canonical mapper, keeping the type parameter.
fn from_write<T>(error: sage_db_format::wire::WriteError) -> FormatOutcome<T> {
    match sage_db_format::dispatch::from_write_error(error) {
        FormatOutcome::Format(error) => FormatOutcome::Format(error),
        FormatOutcome::Refused(refusal) => FormatOutcome::Refused(refusal),
        // `from_write_error` returns `Infallible` on success; this arm is unreachable by
        // construction and cannot fabricate a value.
        FormatOutcome::Ok(never) => match never {},
    }
}

/// Classify a read failure on bytes **this module** just encoded.
///
/// The source is our own encoder's output, so a data/format verdict is not a statement
/// about the caller's disk and must not carry an operator action like "restore a backup".
/// It means the encoder emitted an image its own reader refuses: a build defect. Budget,
/// cancellation and allocation are not verdicts about bytes at all, so they keep their own
/// remedy instead of being folded into the internal class.
fn post_encode_read_error<T>(error: sage_db_format::wire::ReadError) -> FormatOutcome<T> {
    use sage_db_format::wire::ReadError;
    match error {
        ReadError::Data(_) | ReadError::Uninterpretable(_) => FormatOutcome::Format(
            FormatError::Internal(InternalReason::EncodeDecodeDivergence),
        ),
        ReadError::Budget(budget) => FormatOutcome::Refused(Refusal::Budget(budget)),
        ReadError::Stopped(stop) => FormatOutcome::Refused(Refusal::Stopped(stop)),
        ReadError::Allocation { requested } => {
            FormatOutcome::Refused(Refusal::Allocation { requested })
        }
    }
}

/// Canonical active graph digest, not a local projection codec.
pub use sage_db_format::graph::graph_digest;
/// Canonical graph/state assembly error, preserving structure and real refusals.
pub use sage_db_format::state::DigestAssemblyError;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::AttemptLedger;
    use sage_db_types::confidence::Confidence;
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{
        ActorOrigin, ClaimKind, ClaimRecord, ClaimStatus, CommonFields, CreationAudit, Extensions,
        IdSpace, ProposalKind, ProposalStatus, RecordKey, RecordKind,
    };
    use sage_db_types::query::{QueryControl, RunToCompletion};

    /// A control that stops immediately, so "stopped" is never confused with "budget".
    struct Stop;

    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }

    fn open() -> AttemptLedger<'static> {
        static CONTROL: RunToCompletion = RunToCompletion;
        AttemptLedger::open(1 << 20, 1 << 20, &CONTROL).expect("counter allocates")
    }

    fn claim(revision: u64) -> ClaimRecord {
        let mut extensions = Extensions::new();
        extensions
            .insert("legacy.provider", b"z".to_vec())
            .expect("canonical key");
        ClaimRecord {
            key: RecordKey::new(RecordKind::Claim, 7),
            revision: RecordRevision::new(revision).expect("non-zero"),
            status: ClaimStatus::Accepted,
            concept: RecordKey::term(1),
            kind: ClaimKind::Decision,
            text_ref: [0x33; 32],
            confidence: Confidence::Confirmed,
            common: CommonFields {
                recorded_by: Some("tester".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: (revision > 1).then(|| "reviewed".to_owned()),
                created_at: 1_763_424_000_000_000,
                updated_at: (revision > 1).then_some(1_763_424_000_001_000),
            },
            legacy_id: (revision > 1).then(|| "legacy-claim".to_owned()),
            legacy_content_digest: (revision > 1).then_some([0x44; 32]),
            extensions,
            changed_fields: (revision > 1).then(|| vec![21]),
        }
    }

    fn proposal(decided: bool) -> ProposalRecord {
        let mut extensions = Extensions::new();
        extensions
            .insert("review.queue", b"q1".to_vec())
            .expect("canonical key");
        ProposalRecord {
            key: RecordKey::new(RecordKind::Proposal, 9),
            revision: RecordRevision::new(if decided { 2 } else { 1 }).expect("non-zero"),
            status: if decided {
                ProposalStatus::Rejected
            } else {
                ProposalStatus::Pending
            },
            kind: ProposalKind::ClaimEdit,
            subject: RecordKey::term(1),
            base_revisions: vec![RevPair {
                key: RecordKey::term(1),
                revision: RecordRevision::FIRST,
            }],
            payload_ref: [0x55; 32],
            review_note: decided.then(|| "not now".to_owned()),
            declared_author: "author".to_owned(),
            common: CommonFields {
                recorded_by: Some("tester".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: decided.then(|| "decided".to_owned()),
                created_at: 1_763_424_000_000_000,
                updated_at: decided.then_some(1_763_424_000_002_000),
            },
            legacy_id: None,
            extensions,
            changed_fields: decided.then(|| vec![4]),
        }
    }

    fn allocation() -> IdAllocationEvent {
        let mut extensions = Extensions::new();
        extensions
            .insert("import.batch", b"b1".to_vec())
            .expect("canonical key");
        IdAllocationEvent {
            allocation: Watermark {
                space: IdSpace::TermId,
                hwm: 3,
            },
            reason: Some("allocated".to_owned()),
            extensions,
        }
    }

    fn outcome() -> RequestOutcomeEvent {
        let mut extensions = Extensions::new();
        extensions
            .insert("trace.id", b"t1".to_vec())
            .expect("canonical key");
        RequestOutcomeEvent {
            principal_id: "tester".to_owned(),
            request_id: "req-1".to_owned(),
            payload_digest: [0x66; 32],
            result_ref: [0x77; 32],
            commit_version: 2,
            audit: CreationAudit {
                recorded_by: Some("tester".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: Some("committed".to_owned()),
                created_at: 1_763_424_000_000_000,
            },
            extensions,
        }
    }

    #[test]
    fn session_checkpoints_preserve_full_after_images_and_literal_tag_128() {
        use sage_db_types::persist::SessionCheckpointRecord;
        for revision in [1, 2] {
            let mut ledger = open();
            let mut extensions = Extensions::new();
            extensions.insert("session.note", vec![0, 255, 8]).unwrap();
            let record = SessionCheckpointRecord {
                key: RecordKey::new(RecordKind::SessionCheckpoint, 91),
                revision: RecordRevision::new(revision).unwrap(),
                location: sage_db_types::persist::CheckpointLocation::Term {
                    concept: RecordKey::term(8),
                    session_no: 17,
                },
                content_ref: [0x93; 32],
                common: CommonFields {
                    recorded_by: Some("actual-session-writer".to_owned()),
                    actor_origin: ActorOrigin::Authenticated,
                    reason: Some("saved exact content".to_owned()),
                    created_at: 1_763_424_000_000_000,
                    updated_at: (revision > 1).then_some(1_763_424_000_000_001),
                },
                extensions,
                changed_fields: (revision > 1).then(|| vec![70]),
            };
            let FormatOutcome::Ok(event) =
                prepare_event(EventInput::SessionCheckpoint(&record), &mut ledger)
            else {
                panic!("checkpoint prepares");
            };
            assert_eq!(event.tag, 128);
            let FormatOutcome::Ok(DispatchedPayload::SessionCheckpoint(copied)) =
                copy_payload(EventInput::SessionCheckpoint(&record), &mut ledger)
            else {
                panic!("checkpoint decodes");
            };
            assert_eq!(copied, record);
        }
    }

    /// Every family round-trips to its **own** typed value, including the advisory and
    /// optional fields, and the prepared event carries the literal event tag.
    #[test]
    fn all_four_families_round_trip_through_the_canonical_readers() {
        let mut ledger = open();

        let revised = claim(2);
        let prepared = prepare_event(
            EventInput::Claim(ClaimEvent::Revised, &revised),
            &mut ledger,
        );
        let FormatOutcome::Ok(event) = prepared else {
            panic!("claim prepares: {prepared:?}");
        };
        assert_eq!(event.tag, 33, "ClaimRevised");
        let copied = copy_payload(
            EventInput::Claim(ClaimEvent::Revised, &revised),
            &mut ledger,
        );
        assert_eq!(
            copied,
            FormatOutcome::Ok(DispatchedPayload::Claim(revised.clone()))
        );

        let submitted = proposal(false);
        let FormatOutcome::Ok(event) = prepare_event(
            EventInput::Proposal {
                decided: false,
                record: &submitted,
            },
            &mut ledger,
        ) else {
            panic!("proposal submission prepares");
        };
        assert_eq!(event.tag, 96, "ProposalSubmitted");
        assert_eq!(
            copy_payload(
                EventInput::Proposal {
                    decided: false,
                    record: &submitted
                },
                &mut ledger
            ),
            FormatOutcome::Ok(DispatchedPayload::Proposal(submitted.clone()))
        );

        let decided = proposal(true);
        let FormatOutcome::Ok(event) = prepare_event(
            EventInput::Proposal {
                decided: true,
                record: &decided,
            },
            &mut ledger,
        ) else {
            panic!("proposal decision prepares");
        };
        assert_eq!(event.tag, 97, "ProposalDecided");
        assert_eq!(
            copy_payload(
                EventInput::Proposal {
                    decided: true,
                    record: &decided
                },
                &mut ledger
            ),
            FormatOutcome::Ok(DispatchedPayload::Proposal(decided.clone()))
        );

        let mark = allocation();
        let FormatOutcome::Ok(event) = prepare_event(EventInput::IdAllocation(&mark), &mut ledger)
        else {
            panic!("allocation prepares");
        };
        assert_eq!(event.tag, 176, "IdAllocation");
        assert_eq!(
            copy_payload(EventInput::IdAllocation(&mark), &mut ledger),
            FormatOutcome::Ok(DispatchedPayload::IdAllocation(mark.clone()))
        );

        let recorded = outcome();
        let FormatOutcome::Ok(event) = prepare_event(EventInput::Outcome(&recorded), &mut ledger)
        else {
            panic!("outcome prepares");
        };
        assert_eq!(event.tag, 192, "RequestOutcomeRecorded");
        assert_eq!(
            copy_payload(EventInput::Outcome(&recorded), &mut ledger),
            FormatOutcome::Ok(DispatchedPayload::RequestOutcome(recorded.clone()))
        );
    }

    /// The prepared field bytes are the reader's bytes: re-reading the owned event
    /// through the canonical seam yields the same typed value, so nothing was normalised.
    #[test]
    fn prepared_bytes_decode_through_the_dispatch_seam() {
        let mut ledger = open();
        let revised = claim(2);
        let FormatOutcome::Ok(event) = prepare_event(
            EventInput::Claim(ClaimEvent::Revised, &revised),
            &mut ledger,
        ) else {
            panic!("claim prepares");
        };
        let decoded = sage_db_format::prepared::decode_owned_event(
            &event,
            &sage_db_types::framing::Limits::V1,
            &mut ledger,
        );
        assert_eq!(
            decoded,
            FormatOutcome::Ok(DispatchedPayload::Claim(revised))
        );
    }

    /// A spent byte allowance is `Budget`; a caller's stop is `Stopped`. Two different
    /// remedies, never collapsed.
    #[test]
    fn budget_and_stop_are_distinct_refusals() {
        static CONTROL: RunToCompletion = RunToCompletion;
        let mut broke = AttemptLedger::open(0, 0, &CONTROL).expect("counter allocates");
        let sample = outcome();
        assert!(matches!(
            prepare_event(EventInput::Outcome(&sample), &mut broke),
            FormatOutcome::Refused(Refusal::Budget(_))
        ));

        let mut cancelled =
            AttemptLedger::open(1 << 20, 1 << 20, &Stop).expect("counter allocates");
        assert_eq!(
            prepare_event(EventInput::Outcome(&sample), &mut cancelled),
            FormatOutcome::Refused(Refusal::Stopped(ControlStop::Cancelled))
        );
        assert_eq!(
            raw_hash(b"", &mut cancelled),
            FormatOutcome::Refused(Refusal::Stopped(ControlStop::Cancelled))
        );
    }

    /// The result payload is byte-for-byte the §4.2 layout, composed here from the
    /// normative widths rather than from the implementation's own constants.
    #[test]
    fn result_payload_matches_the_normative_tiny_layout() {
        let mut ledger = open();
        let affected = [RevPair {
            key: RecordKey::term(1),
            revision: RecordRevision::FIRST,
        }];
        let counters = [Watermark {
            space: IdSpace::TermId,
            hwm: 1,
        }];
        let payload = result_payload(2, &affected, &counters, &mut ledger).expect("prepares");

        let mut expected = Vec::new();
        expected.extend_from_slice(&0u16.to_le_bytes()); // result_kind committed
        expected.extend_from_slice(&2u64.to_le_bytes()); // commit_version
        expected.extend_from_slice(&1u32.to_le_bytes()); // affected count
        expected.extend_from_slice(&1u16.to_le_bytes()); // RecordKey kind: Term
        expected.extend_from_slice(&1u128.to_le_bytes()); // key id
        expected.extend_from_slice(&1u64.to_le_bytes()); // revision
        expected.extend_from_slice(&1u32.to_le_bytes()); // counter count
        expected.extend_from_slice(&0u16.to_le_bytes()); // space TermId
        expected.extend_from_slice(&1u128.to_le_bytes()); // hwm
        assert_eq!(payload, expected);
        assert_eq!(payload.len(), 2 + 8 + 4 + 26 + 4 + 18);
    }

    /// A rejected proposal decision is still a committed transaction: kind `0`.
    #[test]
    fn rejected_proposal_and_kind_zero_result_preparation_are_supported() {
        let mut ledger = open();
        let rejected = proposal(true);
        assert_eq!(rejected.status, ProposalStatus::Rejected);
        let FormatOutcome::Ok(_) = prepare_event(
            EventInput::Proposal {
                decided: true,
                record: &rejected,
            },
            &mut ledger,
        ) else {
            panic!("a rejected decision still prepares");
        };
        let payload = result_payload(3, &[], &[], &mut ledger).expect("prepares");
        assert_eq!(&payload[..2], &[0, 0]);
    }

    /// Unordered generated lists are preparing defects, not allocation or budget.
    #[test]
    fn unordered_inputs_are_typed_preparation_defects() {
        let mut ledger = open();
        let unordered = [
            RevPair {
                key: RecordKey::term(2),
                revision: RecordRevision::FIRST,
            },
            RevPair {
                key: RecordKey::term(1),
                revision: RecordRevision::FIRST,
            },
        ];
        assert_eq!(
            result_payload(2, &unordered, &[], &mut ledger),
            Err(ResultPreparationError::UnorderedAffected)
        );
        let counters = [
            Watermark {
                space: IdSpace::ClaimId,
                hwm: 1,
            },
            Watermark {
                space: IdSpace::TermId,
                hwm: 1,
            },
        ];
        assert_eq!(
            result_payload(2, &[], &counters, &mut ledger),
            Err(ResultPreparationError::UnorderedCounters)
        );
        assert_eq!(
            result_payload(0, &[], &[], &mut ledger),
            Err(ResultPreparationError::InvalidVersion)
        );
        // An empty operation still observes a cancelled attempt.
        let mut cancelled =
            AttemptLedger::open(1 << 20, 1 << 20, &Stop).expect("counter allocates");
        assert_eq!(
            result_payload(2, &[], &[], &mut cancelled),
            Err(ResultPreparationError::Refused(Refusal::Stopped(
                ControlStop::Cancelled
            )))
        );
    }
}

#[cfg(test)]
mod result_reader_agreement {
    use super::*;
    use crate::lease::AttemptLedger;
    use sage_db_types::query::RunToCompletion;

    #[test]
    fn generated_result_is_read_by_the_same_canonical_public_boundary() {
        let mut l = AttemptLedger::open(1_000_000, 1_000, &RunToCompletion).unwrap();
        let affected = [RevPair {
            key: sage_db_types::persist::RecordKey::term(17),
            revision: sage_db_types::identity::RecordRevision::new(3).unwrap(),
        }];
        let counters = [Watermark {
            space: sage_db_types::persist::IdSpace::FrontierEntryId,
            hwm: u128::MAX,
        }];
        let bytes = result_payload(9, &affected, &counters, &mut l).unwrap();
        let result = read_result_payload(&bytes, bytes.len() as u64, &mut l).unwrap();
        assert_eq!(result.version(), 9);
        assert_eq!(result.affected_count(), 1);
        assert_eq!(result.counter_count(), 1);
        assert_eq!(result.affected_at(0, &mut l).unwrap(), Some(affected[0]));
        assert_eq!(result.counter_at(0, &mut l).unwrap(), Some(counters[0]));
    }
}
