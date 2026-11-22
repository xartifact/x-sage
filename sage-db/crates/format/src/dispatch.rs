//! The one semantic dispatch boundary between transport framing and record policy.
//!
//! [`crate::event::read_event`] is transport-only by design: it frames a 6-byte
//! envelope and validates field ordering, lengths and flags. It deliberately knows
//! nothing about which event tags exist, which fields each one admits, or what a
//! record means. Record modules ([`crate::term`], [`crate::claim`], ...) each apply
//! their own family policy on top.
//!
//! A file-level reader needs "parse this event and hand me its validated payload"
//! **without** owning a second copy of the family tables. That is what this module
//! is: one seam, dispatching to the already-existing per-family readers, so a new
//! family cannot be added in one place and forgotten in another.
//!
//! # What this module is not
//!
//! It does not validate lifecycle semantics (revision monotonicity across
//! transactions, proposal admission, retracted-target rules). Those need the folded
//! state and belong to the store's replay, which is P2. A single event's complete
//! after-image rules are enforced by the family reader that this dispatch calls.

use crate::event::Event;
use crate::registry::SchemaError;
use crate::wire::{DecodeError, ReadError, Reader, WriteError};
use sage_db_types::framing::{
    BlobRefSink, CorruptReason, FormatError, FormatOutcome, InternalReason, LimitKind, Refusal,
    UnsupportedReason,
};
use sage_db_types::persist::RecordKind;
use sage_db_types::query::ControlStop;

/// A decoded record from one of the natural-key or structural families.
///
/// **Inline, not boxed, and that is a resource decision.** A `Box::new` is an
/// allocation that cannot fail fallibly on stable Rust, so it can neither be admitted
/// for its retained bytes nor refused when the allocator says no. The previous boxed
/// form therefore added an *unmetered* heap allocation per decoded event: a caller
/// whose ledger was already spent still paid the peak, and no `Refusal::Allocation`
/// could ever describe it.
///
/// Inlining moves the payload onto the stack of the value that already exists. The
/// cost is that this enum is sized for its largest variant (a `TermRecord`, 448 B
/// here); the gain is that dispatching a record allocates nothing at all. Stack bytes
/// are not an allocation the ledger is meant to police, and a wrong-but-cheap `size_of`
/// charge would have been a fabricated number rather than a measurement.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DispatchedPayload {
    /// `64 AssetDeclared`.
    Asset(sage_db_types::persist::AssetRecord),
    /// `144 KbMetadataUpdated`.
    KbMetadata(sage_db_types::persist::KbMetadataRecord),
    /// `145`/`146`/`147`.
    Model(sage_db_types::persist::ModelRecord),
    /// `160 RuleProfileChanged`.
    RuleProfile(sage_db_types::persist::RuleConfigRecord),
    /// `176 IdAllocation`.
    IdAllocation(sage_db_types::persist::IdAllocationEvent),
    /// `192 RequestOutcomeRecorded`.
    RequestOutcome(sage_db_types::persist::RequestOutcomeEvent),
    /// `208 ImportBookkeeping`.
    ImportBookkeeping(sage_db_types::persist::ImportSessionRecord),
    /// `224 Genesis`.
    Genesis(sage_db_types::persist::GenesisRecord),
    /// `TermCreated`/`TermRevised`/`TermRetired`.
    Term(sage_db_types::persist::TermRecord),
    /// `AssertionCreated`/`AssertionRevised`/`AssertionRetracted`.
    Assertion(sage_db_types::persist::AssertionRecordFull),
    /// `ClaimCreated`/`ClaimRevised`/`ClaimSuperseded`/`ClaimRejected`.
    Claim(sage_db_types::persist::ClaimRecord),
    /// `EvidenceCreated`/`EvidenceRevised`.
    Evidence(sage_db_types::persist::EvidenceRecord),
    /// `EvidenceLinkCreated`/`EvidenceLinkRetracted`.
    EvidenceLink(sage_db_types::persist::EvidenceLinkRecord),
    /// `VectorUpserted`/`VectorRetired`.
    Vector(sage_db_types::persist::VectorRecord),
    /// `ProposalSubmitted`/`ProposalDecided`.
    Proposal(sage_db_types::persist::ProposalRecord),
    /// `FrontierEntryChanged`.
    FrontierEntry(sage_db_types::persist::FrontierEntryRecord),
    /// `SessionCheckpointSaved`.
    SessionCheckpoint(sage_db_types::persist::SessionCheckpointRecord),
}

/// Which family's codec owns a given event tag./// Which family's codec owns a given event tag.
///
/// Derivable from the registry, so it cannot disagree with it: the mapping is a
/// function of the tag, and the registry is the only table consulted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodecFamily {
    /// `TermCreated` / `TermRevised` / `TermRetired`.
    Term,
    /// `AssertionCreated` / `AssertionRevised` / `AssertionRetracted`.
    Assertion,
    /// `ClaimCreated` / `ClaimRevised` / `ClaimSuperseded` / `ClaimRejected`.
    Claim,
    /// `EvidenceCreated` / `EvidenceRevised`.
    Evidence,
    /// `EvidenceLinkCreated` / `EvidenceLinkRetracted`.
    EvidenceLink,
    /// `VectorUpserted` / `VectorRetired`.
    Vector,
    /// `ProposalSubmitted` / `ProposalDecided`.
    Proposal,
    /// `FrontierEntryChanged`.
    FrontierEntry,
    /// `SessionCheckpointSaved`.
    SessionCheckpoint,
    /// `AssetDeclared`.
    Asset,
    /// `KbMetadataUpdated`.
    KbMetadata,
    /// `ModelRegistered` / `ModelUpdated` / `ModelRetired`.
    Model,
    /// `RuleProfileChanged`.
    RuleProfile,
    /// `RequestOutcomeRecorded`.
    RequestOutcome,
    /// `ImportBookkeeping`.
    ImportBookkeeping,
    /// `IdAllocation`.
    IdAllocation,
    /// `Genesis`.
    Genesis,
    /// A registered event whose codec is not part of this build.
    ///
    /// Not damage and not a foreign tag: the registry knows it, this binary has no
    /// implementation yet. Reported as [`UnsupportedReason::NoCodec`] so an operator
    /// sees "incomplete build", never "restore from backup".
    Unlanded,
}

impl CodecFamily {
    /// The family that owns `tag`, consulting only the closed registry.
    ///
    /// # Errors
    ///
    /// [`UnsupportedReason::EventTag`] when the tag is absent from this major.
    pub fn of(tag: u16) -> Result<Self, UnsupportedReason> {
        Ok(match tag {
            1..=3 => Self::Term,
            16..=18 => Self::Assertion,
            32..=35 => Self::Claim,
            48 | 49 => Self::Evidence,
            50 | 51 => Self::EvidenceLink,
            80 | 81 => Self::Vector,
            96 | 97 => Self::Proposal,
            112 => Self::FrontierEntry,
            128 => Self::SessionCheckpoint,
            64 => Self::Asset,
            144 => Self::KbMetadata,
            145..=147 => Self::Model,
            160 => Self::RuleProfile,
            176 => Self::IdAllocation,
            192 => Self::RequestOutcome,
            208 => Self::ImportBookkeeping,
            224 => Self::Genesis,
            other => {
                // Fall through to the registry: if it is registered but unlisted
                // above, that is a defect in this match, not a foreign file.
                if is_registered_event(other) {
                    Self::Unlanded
                } else {
                    return Err(UnsupportedReason::EventTag(other));
                }
            }
        })
    }
}

/// Whether the closed registry knows this event tag.
///
/// Kept private: the public answer to "is this tag real" is
/// [`crate::registry::accept_event`], which also enforces the REQUIRED flag.
fn is_registered_event(tag: u16) -> bool {
    crate::registry::EVENTS.iter().any(|spec| spec.tag == tag)
}

/// The record kind an identified event's `record_key` must carry.
///
/// `None` for natural-key and structural events, which have no `RecordKey` at all.
/// This is the cheap cross-field check from `storage-format.md` §7.2, kept here so a
/// caller does not restate the tag-to-kind mapping.
#[must_use]
pub const fn expected_record_kind(tag: u16) -> Option<RecordKind> {
    match tag {
        1..=3 => Some(RecordKind::Term),
        16..=18 => Some(RecordKind::Assertion),
        32..=35 => Some(RecordKind::Claim),
        48 | 49 => Some(RecordKind::Evidence),
        50 | 51 => Some(RecordKind::EvidenceLink),
        80 | 81 => Some(RecordKind::Vector),
        96 | 97 => Some(RecordKind::Proposal),
        112 => Some(RecordKind::FrontierEntry),
        128 => Some(RecordKind::SessionCheckpoint),
        _ => None,
    }
}

/// One event, validated by the family that owns its tag.
///
/// The payload stays as the framed field stream: a file-level reader forwards it,
/// while a record-level reader calls the family decoder. Keeping the raw event here
/// is what makes this seam usable by both without a second parse.
#[derive(Clone, PartialEq, Eq, Debug)]
// Inline payloads make this enum as large as its biggest record. That is deliberate:
// a boxed payload would be an allocation that cannot fail fallibly, so it could neither
// be admitted for its retained bytes nor refused when the allocator says no - an
// unmetered heap allocation per decoded event. The lint's alternative (boxing) is the
// defect being avoided, so the allow is narrow and reasoned rather than blanket.
#[allow(clippy::large_enum_variant)]
pub enum DispatchedEvent<'a> {
    /// A natural-key or structural event, decoded to its record.
    ///
    /// These families have no `record_key`, so this seam can produce the whole record
    /// and there is nothing left for the caller to resolve.
    Record {
        /// The family whose rules were applied.
        family: CodecFamily,
        /// The decoded record.
        payload: DispatchedPayload,
    },
    /// An identified event, validated but not decoded here.
    ///
    /// Its record reader needs the `record_key`, which is the caller's to supply; the
    /// family reader returns the event alongside the record, so decoding here as well
    /// would parse twice for no gain. The field stream is forwarded so a file decoder
    /// can enumerate references without re-reading.
    Validated {
        /// The family whose rules were applied.
        family: CodecFamily,
        /// The validated event, borrowed from the caller's buffer.
        event: Event<'a>,
    },
}

impl DispatchedEvent<'_> {
    /// The family whose rules were applied.
    #[must_use]
    pub const fn family(&self) -> CodecFamily {
        match self {
            Self::Record { family, .. } | Self::Validated { family, .. } => *family,
        }
    }

    /// The decoded record, when this seam produced one.
    #[must_use]
    pub const fn payload(&self) -> Option<&DispatchedPayload> {
        match self {
            Self::Record { payload, .. } => Some(payload),
            Self::Validated { .. } => None,
        }
    }

    /// The validated event, for an identified family.
    #[must_use]
    pub const fn event(&self) -> Option<&Event<'_>> {
        match self {
            Self::Validated { event, .. } => Some(event),
            Self::Record { .. } => None,
        }
    }
}

/// Read one event and apply the registry plus family-ownership checks.
///
/// This is the seam a file decoder should call instead of
/// [`crate::event::read_event`] + a hand-written `match`. It applies, in order:
///
/// 1. transport framing ([`crate::event::read_event`]);
/// 2. registry acceptance: the tag is registered and carries the REQUIRED flag;
/// 3. family ownership: the tag maps to a codec this build implements;
/// 4. the advisory/required compatibility answer for every field present.
///
/// Per-event *record* policy (permitted/required/forbidden field sets, value
/// domains, revision shape) stays with the family reader, because that is where the
/// D10 row for the event already lives; duplicating it here would be a second
/// authority.
///
/// # Errors
///
/// Whatever framing returned; [`UnsupportedReason`] for an unregistered tag, a
/// missing REQUIRED flag, or a registered tag whose codec is not in this build.
pub fn read_dispatched<'a>(
    reader: &mut Reader<'a>,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> FormatOutcome<DispatchedEvent<'a>> {
    // Peek first: the family reader parses the envelope itself, so consuming here
    // would either double the parse or make the two disagree.
    let tag = match reader.peek_event_tag() {
        Ok(tag) => tag,
        Err(error) => return FormatOutcome::Format(decode_error_class(error)),
    };
    let family = match CodecFamily::of(tag) {
        Ok(family) => family,
        Err(reason) => return FormatOutcome::Format(FormatError::UnsupportedFormat(reason)),
    };
    if family == CodecFamily::Unlanded {
        return FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::NoCodec {
            tag,
        }));
    }

    // Identified families stop at validation here; the caller supplies the record key
    // it already knows and calls the record reader. Their readers return the event
    // alongside the record, so nothing is lost by not decoding twice.
    let payload = match family {
        CodecFamily::Asset => match crate::natural::read_asset(reader, ledger) {
            Ok(record) => Some(DispatchedPayload::Asset(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::KbMetadata => match crate::natural::read_kb_metadata(reader, ledger) {
            Ok(record) => Some(DispatchedPayload::KbMetadata(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::Model => match crate::natural::read_model(reader, ledger) {
            Ok((_, record)) => Some(DispatchedPayload::Model(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::RuleProfile => {
            match crate::natural::read_rule_profile_changed(reader, ledger) {
                Ok(record) => Some(DispatchedPayload::RuleProfile(record)),
                Err(error) => return from_read_error(error),
            }
        }
        CodecFamily::RequestOutcome => match crate::natural::read_request_outcome(reader, ledger) {
            Ok(record) => Some(DispatchedPayload::RequestOutcome(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::ImportBookkeeping => {
            match crate::natural::read_import_bookkeeping(reader, ledger) {
                Ok(record) => Some(DispatchedPayload::ImportBookkeeping(record)),
                Err(error) => return from_read_error(error),
            }
        }
        CodecFamily::IdAllocation => match crate::structural::read_id_allocation(reader, ledger) {
            Ok(record) => Some(DispatchedPayload::IdAllocation(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::Genesis => match crate::structural::read_genesis(reader, ledger) {
            Ok(record) => Some(DispatchedPayload::Genesis(record)),
            Err(error) => return from_read_error(error),
        },
        // Identified families: the family's OWN reader, in the seam.
        //
        // Replacing this with a namespace check was wrong and the coordinator's probe
        // proved it: a `TermCreated` carrying only `17 atom` passed here while
        // `term::read_term` refused it. The record key is not an obstacle - the family
        // reader extracts it from the field stream itself - so there is no reason to
        // leave the caller to decode what this seam can decode.
        CodecFamily::Term => match crate::term::read_term(reader, ledger) {
            Ok((_, record)) => Some(DispatchedPayload::Term(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::Assertion => match crate::assertion::read_assertion(reader, ledger) {
            Ok((_, record)) => Some(DispatchedPayload::Assertion(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::Claim => match crate::claim::read_claim(reader, ledger) {
            Ok((_, record)) => Some(DispatchedPayload::Claim(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::Evidence => match crate::evidence::read_evidence(reader, ledger) {
            Ok((_, record)) => Some(DispatchedPayload::Evidence(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::EvidenceLink => match crate::evidence::read_link(reader, ledger) {
            Ok((_, record)) => Some(DispatchedPayload::EvidenceLink(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::Vector => match crate::collection::read_vector(reader, ledger) {
            Ok((_, record)) => Some(DispatchedPayload::Vector(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::Proposal => match crate::collection::read_proposal(reader, ledger) {
            Ok((_, record)) => Some(DispatchedPayload::Proposal(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::FrontierEntry => match crate::collection::read_frontier(reader, ledger) {
            Ok(record) => Some(DispatchedPayload::FrontierEntry(record)),
            Err(error) => return from_read_error(error),
        },
        CodecFamily::SessionCheckpoint => {
            match crate::collection::read_checkpoint(reader, ledger) {
                Ok(record) => Some(DispatchedPayload::SessionCheckpoint(record)),
                Err(error) => return from_read_error(error),
            }
        }
        // Already refused above; listed so adding a family is a deliberate edit here too.
        CodecFamily::Unlanded => {
            return FormatOutcome::Format(FormatError::UnsupportedFormat(
                UnsupportedReason::NoCodec { tag },
            ));
        }
    };
    match payload {
        Some(payload) => FormatOutcome::Ok(DispatchedEvent::Record { family, payload }),
        // Every non-identified family above returns a payload; reaching here means a
        // new family was added to `CodecFamily` without a decode arm, which is a
        // build defect rather than a file problem.
        None => FormatOutcome::Format(FormatError::Internal(
            InternalReason::EncodeDecodeDivergence,
        )),
    }
}

/// Enumerate the blob references an event declares, as this build counts them.
///
/// A reference is a `bytes32` field whose tag names a content address. The list is
/// the D10 field rows that R6 fixed as references; it is deliberately explicit rather
/// than "every bytes32", because `sha256` on an asset is the key itself and a
/// digest of a value is not a blob to fetch.
///
/// # Errors
///
/// [`Refusal`] when the sink's ledger refuses an admission.
pub fn for_each_blob_ref(
    event: &Event<'_>,
    sink: &mut dyn BlobRefSink,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<(), Refusal> {
    for_each_blob_ref_fields(event.fields(), sink, ledger)
}

/// Enumerate blob references from a raw field slice.
///
/// This is the ONE implementation; [`for_each_blob_ref`] delegates to it. Two copies would
/// drift, and the tag predicate is only authoritative in one place.
///
/// # Cancellation on an empty set
///
/// The entry `checkpoint()` runs **before** the loop, so a caller's cancellation is observed
/// even when the field list contains no references at all. Without it, "no refs" and "the
/// caller cancelled" would both return `Ok(())` for an event whose refs are simply absent —
/// which is the failure the empty-operation rule exists to prevent.
///
/// # Errors
///
/// [`Refusal`] from the entry checkpoint or from the sink, including cancellation.
pub fn for_each_blob_ref_fields(
    fields: &[crate::event::Field<'_>],
    sink: &mut dyn BlobRefSink,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<(), Refusal> {
    // Entry checkpoint: a caller's cancellation is observed even when nothing is enumerated.
    ledger.checkpoint()?;
    for field in fields {
        // Per-field checkpoint, kept from the delivered component. This public helper does NOT
        // require a validated event, so it has no MAX_EVENT_FIELDS precondition to lean on: the
        // bound on the loop is the caller's slice, and an arbitrary-length slice must stay
        // interruptible. Cheap per field, and it is the difference between "bounded" and
        // "bounded because the caller happened to validate".
        ledger.checkpoint()?;
        if !is_blob_ref_tag(field.tag()) {
            continue;
        }
        let value = field.value();
        let Ok(bytes) = <[u8; 32]>::try_from(value) else {
            // A ref tag with the wrong width is a corrupted field, and the family
            // decoder reports it as such; enumeration only forwards well-formed refs.
            continue;
        };
        sink.push(field.tag(), bytes, ledger)?;
    }
    Ok(())
}

/// Whether a field tag names a content-addressed blob.
///
/// `storage-digests.md` §2 fixes the set. Each member, with the field it is:
///
/// | tag | field | tag | field |
/// |---|---|---|---|
/// | 23 | term `body_ref` | 133 | checkpoint `content_ref` |
/// | 50 | claim `text_ref` | 162 | rule-config `dry_run_report_ref` |
/// | 67 | evidence `excerpt_ref` | **166** | **rule-config `rules_ref`** |
/// | 68 | evidence `asset_sha256` | 180 | outcome `result_ref` |
/// | 101 | vector `values_ref` | 185 | import `plan_ref` |
/// | 115 | proposal `payload_ref` | 129 | frontier `text_ref` |
/// | 153 | KB metadata `legacy_frontier_ref` | | |
///
/// Excluded on purpose, and the exclusion is the point: `84 sha256` **is** the asset's
/// key, `100 input_digest` and `178 payload_digest` are digests *of* values rather than
/// addresses to fetch, and `12/169/184` are legacy/source digests. Treating any of them
/// as a fetchable blob would make a reader try to open a path that never exists.
///
/// Tag 166 was missing here while this doc claimed rules were included - the
/// enumeration silently dropped the rules blob from an accepted Genesis event. The set
/// and this table are now the same list; a tag added to one without the other is the bug
/// this comment exists to prevent.
#[must_use]
pub const fn is_blob_ref_tag(tag: u16) -> bool {
    matches!(
        tag,
        23 | 50 | 67 | 68 | 101 | 115 | 129 | 133 | 153 | 162 | 166 | 180 | 185
    )
}

/// Map a transport read failure to this crate's classification.
///
/// Budget, cancellation and allocation keep their own variants: they are not
/// verdicts about the bytes, and folding them into `Corrupt` would tell an operator
/// to restore a backup because a request ran out of allowance.
#[must_use]
pub fn from_read_error<T>(error: ReadError) -> FormatOutcome<T> {
    match error {
        ReadError::Data(inner) => FormatOutcome::Format(decode_error_class(inner)),
        ReadError::Uninterpretable(schema) => FormatOutcome::Format(schema_to_format(schema)),
        ReadError::Budget(budget) => FormatOutcome::Refused(Refusal::Budget(budget)),
        ReadError::Stopped(stop) => FormatOutcome::Refused(Refusal::Stopped(stop)),
        ReadError::Allocation { requested } => {
            FormatOutcome::Refused(Refusal::Allocation { requested })
        }
    }
}

/// Map a transport write failure; used when a file encoder wraps a family encoder.
///
/// A family write failure is never a bytes verdict about an existing file: it says the
/// value the caller supplied cannot be encoded. That is [`FormatError::Internal`] with
/// [`InternalReason::EncodeDecodeDivergence`] only when the encoder and decoder
/// disagree; a plain data error is reported as such and stays distinguishable from a
/// refusal.
#[must_use]
pub fn from_write_error(error: WriteError) -> FormatOutcome<core::convert::Infallible> {
    match error {
        WriteError::Data(inner) => {
            FormatOutcome::Format(FormatError::Internal(encode_internal(inner)))
        }
        WriteError::Budget(budget) => FormatOutcome::Refused(Refusal::Budget(budget)),
        WriteError::Stopped(stop) => FormatOutcome::Refused(Refusal::Stopped(stop)),
        WriteError::Allocation { requested } => {
            FormatOutcome::Refused(Refusal::Allocation { requested })
        }
        WriteError::MissingRequiredField { event, field } => {
            FormatOutcome::Format(FormatError::Internal(InternalReason::MissingRequired {
                event: Some(event),
                tag: field,
            }))
        }
        WriteError::Internal { .. } => FormatOutcome::Format(FormatError::Internal(
            InternalReason::EncodeDecodeDivergence,
        )),
    }
}

/// The class for a schema refusal.
#[must_use]
pub fn schema_to_format(schema: SchemaError) -> FormatError {
    match schema {
        SchemaError::UnknownEvent { tag } => {
            FormatError::UnsupportedFormat(UnsupportedReason::EventTag(tag))
        }
        SchemaError::MissingRequiredFlag { tag } => {
            FormatError::UnsupportedFormat(UnsupportedReason::EventTag(tag))
        }
        SchemaError::UnknownField { field, .. } => {
            FormatError::UnsupportedFormat(UnsupportedReason::FieldTag(field))
        }
    }
}

/// The corrupt class for a transport decode failure.
///
/// Deliberately exhaustive: a new [`DecodeError`] variant must be classified by hand
/// rather than falling into a default, because the class decides the operator action.
#[must_use]
pub fn decode_error_class(error: DecodeError) -> FormatError {
    // §9.2 fixes one class per observation, and each class prescribes a different
    // operator action. Returning a `CorruptReason` for everything was wrong by
    // construction: an over-long string is a refusal to admit a legal file, not
    // damage, and telling an operator to restore a backup for it is a false alarm.
    match error {
        // Bytes that cannot be any legal file.
        DecodeError::Truncated { .. } => FormatError::Corrupt(CorruptReason::TruncatedHeader),
        DecodeError::Trailing { .. } => FormatError::Corrupt(CorruptReason::PayloadLenMismatch),
        DecodeError::FieldOrder { .. } => FormatError::Corrupt(CorruptReason::FieldOrder),
        DecodeError::ZeroLengthField { tag } => {
            FormatError::Corrupt(CorruptReason::ZeroLengthField { tag })
        }
        DecodeError::FieldFlags { .. } => FormatError::Corrupt(CorruptReason::HeaderFlags),
        DecodeError::EventFlags { .. } => FormatError::Corrupt(CorruptReason::HeaderFlags),
        DecodeError::NotUtf8 | DecodeError::NulInString => {
            FormatError::Corrupt(CorruptReason::InvalidString)
        }
        DecodeError::InvalidCheckpointLocation => {
            FormatError::Corrupt(CorruptReason::InvalidCheckpointLocation)
        }
        DecodeError::MissingField { event, field } => {
            FormatError::Corrupt(CorruptReason::MissingField { event, field })
        }
        DecodeError::ForbiddenField { event, field } => {
            FormatError::Corrupt(CorruptReason::ForbiddenField { event, field })
        }
        DecodeError::StatusMismatch { event } => {
            FormatError::Corrupt(CorruptReason::StatusMismatch { event })
        }
        DecodeError::ActorOriginMismatch { .. } => {
            FormatError::Corrupt(CorruptReason::ActorOriginMismatch)
        }
        DecodeError::WrongRevision { event, revision } => {
            FormatError::Corrupt(CorruptReason::WrongRevision { event, revision })
        }
        DecodeError::RecordKeyKindMismatch { event, key_kind } => {
            FormatError::Corrupt(CorruptReason::RecordKeyKindMismatch { event, key_kind })
        }
        DecodeError::InvalidRevision { value } => {
            FormatError::Corrupt(CorruptReason::ZeroRevision { value })
        }
        DecodeError::InvalidExtensionKey => {
            FormatError::Corrupt(CorruptReason::InvalidExtensionKey)
        }

        // Legal structure, value outside a closed domain: §9.2 says UNSUPPORTED_FORMAT.
        DecodeError::UnknownEnum { tag, value } => {
            FormatError::UnsupportedFormat(UnsupportedReason::EnumValue { tag, value })
        }
        DecodeError::UnregisteredField { tag } => {
            FormatError::UnsupportedFormat(UnsupportedReason::FieldTag(tag))
        }
        DecodeError::WatermarkOutOfRange { space, value } => {
            FormatError::UnsupportedFormat(UnsupportedReason::WatermarkOutOfRange { space, value })
        }
        DecodeError::IdOutOfRange { kind } => {
            FormatError::UnsupportedFormat(UnsupportedReason::IdOutOfRange { kind })
        }

        // Structurally legal, over a declared ceiling: §9.2 says FORMAT_LIMIT.
        // The transport length readers do not carry a field tag, so the diagnostic
        // says `None` instead of naming field 0 - a fabricated tag would make two
        // different over-long fields indistinguishable in an operator report.
        DecodeError::StringTooLong { len, max } => FormatError::Limit(LimitKind::String {
            tag: None,
            len,
            max,
        }),
        DecodeError::CountTooLong { count, max } => FormatError::Limit(LimitKind::ListItems {
            tag: None,
            count,
            max,
        }),
    }
}

/// The internal class for a value the caller could not have supplied legally.
#[must_use]
pub fn encode_internal(error: crate::wire::EncodeError) -> InternalReason {
    use crate::wire::EncodeError;
    match error {
        EncodeError::MissingRequiredField { event, field } => InternalReason::MissingRequired {
            event: Some(event),
            tag: field,
        },
        EncodeError::FieldOrder { .. }
        | EncodeError::EventFlags { .. }
        | EncodeError::FieldFlags { .. }
        | EncodeError::EmptyField { .. } => InternalReason::EncodeDecodeDivergence,
        EncodeError::NulInString
        | EncodeError::StatusMismatch { .. }
        | EncodeError::StringTooLong
        | EncodeError::BlobTooLong
        | EncodeError::CountTooLong
        | EncodeError::UnknownEnumValue { .. }
        | EncodeError::IdOutOfRange { .. }
        | EncodeError::WrongRevision { .. }
        | EncodeError::ForbiddenField { .. }
        | EncodeError::ActorOriginMismatch { .. }
        | EncodeError::InvalidCheckpointLocation => InternalReason::EncodeDecodeDivergence,
    }
}

/// A control stop, in the shape a file decoder returns it.
#[must_use]
pub const fn stopped(stop: ControlStop) -> Refusal {
    Refusal::Stopped(stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::encode_with;
    use crate::term::{TERM_CREATED, TermEvent, write_term};
    use sage_db_types::confidence::Confidence;
    use sage_db_types::framing::{OperatorAction, classify};
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::layer::SemanticLayer;
    use sage_db_types::ledger::{BudgetError, SharedLedger};
    use sage_db_types::persist::{
        ActorOrigin, CommonFields, Extensions, RecordKey, TermRecord, TermSource,
    };

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

    fn term() -> TermRecord {
        TermRecord {
            key: RecordKey::term(1),
            revision: RecordRevision::FIRST,
            retired: false,
            slug: "s".into(),
            atom: "a".into(),
            layer: SemanticLayer::L1,
            domains: vec!["d".into()],
            source: TermSource::Personal,
            confidence: Confidence::Confirmed,
            triggers: vec!["t".into()],
            body_ref: [1; 32],
            common: CommonFields {
                recorded_by: None,
                actor_origin: ActorOrigin::Unavailable,
                reason: None,
                created_at: 1,
                updated_at: None,
            },
            legacy_id: None,
            legacy_source_path: None,
            legacy_content_digest: None,
            legacy_source_file_digest: None,
            legacy_source_revision: None,
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    #[test]
    fn dispatch_names_the_family_that_owns_the_tag() {
        assert_eq!(CodecFamily::of(TERM_CREATED), Ok(CodecFamily::Term));
        assert_eq!(
            CodecFamily::of(64),
            Ok(CodecFamily::Asset),
            "the natural codecs are landed, so tag 64 is no longer Unlanded"
        );
        assert_eq!(CodecFamily::of(224), Ok(CodecFamily::Genesis));
        assert_eq!(
            CodecFamily::of(7),
            Err(UnsupportedReason::EventTag(7)),
            "tag 7 is not registered in this major"
        );
    }

    #[test]
    fn a_registered_tag_without_a_codec_is_not_damage() {
        assert_eq!(
            expected_record_kind(64),
            None,
            "an asset has no numeric identity"
        );
        assert_eq!(expected_record_kind(TERM_CREATED), Some(RecordKind::Term));
    }

    #[test]
    fn a_dispatched_term_event_reaches_its_own_family() {
        let bytes = encode_with(&mut Open, |w| write_term(w, TermEvent::Created, &term())).unwrap();
        let mut led = Open;
        let outcome = read_dispatched(&mut Reader::new(&bytes), &mut led);
        let dispatched = match outcome {
            FormatOutcome::Ok(dispatched) => dispatched,
            other => panic!("expected a dispatched event, got {other:?}"),
        };
        assert_eq!(dispatched.family(), CodecFamily::Term);
        match dispatched
            .payload()
            .expect("the seam decodes identified families too")
        {
            DispatchedPayload::Term(record) => {
                assert_eq!(
                    record.key,
                    RecordKey::term(1),
                    "the family reader decoded it"
                );
            }
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn an_unregistered_tag_is_unsupported_not_corrupt() {
        // Envelope for an event nobody registered.
        let mut bytes = 7u16.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        let mut led = Open;
        let outcome = read_dispatched(&mut Reader::new(&bytes), &mut led);
        assert!(
            matches!(
                outcome,
                FormatOutcome::Format(FormatError::UnsupportedFormat(UnsupportedReason::EventTag(
                    7
                )))
            ),
            "got {outcome:?}"
        );
    }

    #[test]
    fn a_budget_refusal_is_not_a_format_verdict() {
        let outcome: FormatOutcome<DispatchedEvent<'_>> = from_read_error(ReadError::Budget(
            BudgetError::new(sage_db_types::ledger::BudgetUnit::Bytes, 10, 0),
        ));
        assert!(
            outcome.as_format_error().is_none(),
            "not a verdict on bytes"
        );
        assert!(matches!(
            outcome,
            FormatOutcome::Refused(Refusal::Budget(_))
        ));
    }

    #[test]
    fn a_structural_event_decodes_through_the_seam() {
        // An IdAllocation is the smallest structural record: two fields, no actor.
        let record = sage_db_types::persist::IdAllocationEvent {
            allocation: sage_db_types::persist::Watermark {
                space: sage_db_types::persist::IdSpace::TermId,
                hwm: 7,
            },
            reason: None,
            extensions: sage_db_types::persist::Extensions::new(),
        };
        let bytes = encode_with(&mut Open, |w| {
            crate::structural::write_id_allocation(w, &record)
        })
        .expect("encode");
        let outcome = read_dispatched(&mut Reader::new(&bytes), &mut Open);
        match outcome {
            FormatOutcome::Ok(DispatchedEvent::Record { family, payload }) => {
                assert_eq!(family, CodecFamily::IdAllocation);
                match payload {
                    DispatchedPayload::IdAllocation(got) => {
                        assert_eq!(got.allocation.hwm, 7, "the seam decodes, not just routes");
                    }
                    other => panic!("wrong payload: {other:?}"),
                }
            }
            other => panic!("expected a decoded record, got {other:?}"),
        }
    }

    #[test]
    fn over_limit_and_closed_domain_refusals_are_not_corruption() {
        // §9.2: an over-long field is FORMAT_LIMIT (reject input), an unknown enum value
        // is UNSUPPORTED_FORMAT (migrate). Reporting either as Corrupt tells an operator
        // to restore a backup for a legal file, which is the defect this asserts against.
        let over_long = decode_error_class(DecodeError::StringTooLong { len: 65, max: 64 });
        assert!(
            matches!(over_long, FormatError::Limit(_)),
            "over-long string became {over_long:?}"
        );
        assert_eq!(classify_of(over_long), OperatorAction::RejectInput);

        let too_many = decode_error_class(DecodeError::CountTooLong {
            count: 300,
            max: 256,
        });
        assert!(matches!(too_many, FormatError::Limit(_)));

        let bad_enum = decode_error_class(DecodeError::UnknownEnum { tag: 85, value: 9 });
        assert!(
            matches!(
                bad_enum,
                FormatError::UnsupportedFormat(UnsupportedReason::EnumValue { .. })
            ),
            "unknown enum became {bad_enum:?}"
        );

        let unregistered = decode_error_class(DecodeError::UnregisteredField { tag: 250 });
        assert!(matches!(
            unregistered,
            FormatError::UnsupportedFormat(UnsupportedReason::FieldTag(250))
        ));

        // A genuine framing defect stays Corrupt.
        assert!(matches!(
            decode_error_class(DecodeError::ZeroLengthField { tag: 8 }),
            FormatError::Corrupt(_)
        ));
    }

    fn classify_of(error: FormatError) -> OperatorAction {
        classify(&body_of(error))
    }

    fn body_of(error: FormatError) -> FormatOutcome<u8> {
        FormatOutcome::Format(error)
    }

    #[test]
    fn a_registered_but_foreign_field_is_refused_by_the_seam_too() {
        // t67's counterexample: evidence_kind (64) and model_dimensions (152) are both
        // registered tags, but neither belongs to TermCreated. The seam previously said
        // Ok while term::read_term said ForbiddenField - two public paths disagreeing
        // about the same bytes, with the lenient one accepting and dropping data.
        let bytes = encode_with(&mut Open, |w| write_term(w, TermEvent::Created, &term()))
            .expect("base image");
        for foreign in [64u16, 152u16] {
            let mutated = with_extra_field_bytes(&bytes, foreign, &[0, 0]);
            let outcome = read_dispatched(&mut Reader::new(&mutated), &mut Open);
            match outcome {
                FormatOutcome::Format(FormatError::Corrupt(CorruptReason::ForbiddenField {
                    field,
                    ..
                })) => assert_eq!(field, foreign, "wrong field named"),
                other => panic!("seam accepted foreign tag {foreign}: {other:?}"),
            }
        }
    }

    /// Rebuild an event image with one extra field, keeping the envelope consistent.
    fn with_extra_field_bytes(bytes: &[u8], tag: u16, value: &[u8]) -> Vec<u8> {
        let mut l = Open;
        let event = crate::event::read_event(&mut Reader::new(bytes), &mut l).expect("re-read");
        let mut fields: Vec<(u16, Vec<u8>)> = event
            .fields()
            .iter()
            .map(|f| (f.tag(), f.value().to_vec()))
            .collect();
        fields.push((tag, value.to_vec()));
        fields.sort_by_key(|p| p.0);
        let mut out = Vec::new();
        out.extend_from_slice(&event.tag().to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        for (t, v) in fields {
            out.extend_from_slice(&t.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&(v.len() as u32).to_le_bytes());
            out.extend_from_slice(&v);
        }
        out
    }

    #[test]
    fn blob_ref_tags_exclude_keys_and_value_digests() {
        assert!(is_blob_ref_tag(23), "term body is a blob");
        assert!(!is_blob_ref_tag(84), "asset sha256 IS the key");
        assert!(!is_blob_ref_tag(100), "input_digest is a value digest");
        assert!(!is_blob_ref_tag(178), "payload_digest is not fetched");
    }
}
