//! Vector, proposal, frontier-entry and session-checkpoint codecs.
//!
//! These four share a shape: an identified record whose heavy content is a blob
//! reference. They differ in lifecycle, and each difference is enforced rather than
//! documented:
//!
//! * **Vector** carries both `subject_revision` and `input_digest`. Retrieval needs both:
//!   one says which object revision the embedding describes, the other says which text
//!   was embedded. A vector whose subject moved on is stale, not wrong, so it is excluded
//!   rather than deleted - hence `Retired` exists instead of removal.
//! * **Proposal** carries a *baseline vector*, not a version number. A baseline constrains
//!   exactly the objects the proposal reads or writes, so an unrelated stale proposal
//!   cannot block a commit. The baseline must be ascending and unique or it is corruption.
//! * **FrontierEntry** and **SessionCheckpoint** are deliberately simple; their content is
//!   prose in a blob, and their value is that they are *records* rather than files.
//!
//! Field tags follow `storage-format` §6: 96–101 vector, 112–116 proposal, 128–130
//! frontier, 132–133 checkpoint, plus the common identity/audit tags.

use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ActorOrigin, CheckpointLocation, CommonFields, Extensions, FrontierEntryRecord, FrontierStatus,
    ProposalKind, ProposalRecord, ProposalStatus, RecordKey, RecordKind, RevPair,
    SessionCheckpointRecord, VectorRecord, VectorStatus,
};

use crate::event::read_event;
use crate::field::{
    MAX_DECLARED_AUTHOR, MAX_IDENTITY_BYTES, MAX_MODEL_ID_BYTES, MAX_TEXT_BYTES, read_actor_origin,
    read_extensions, read_opt_str, read_str, write_actor_origin, write_extensions,
    write_record_key, write_revision, write_str,
};
use crate::record::{
    FieldSet, check_policy, decode_changed_fields, decode_field, decode_opt_field, forbid,
    reason_required, write_changed_fields,
};
use crate::registry::{SchemaError, accept_event};
use crate::wire::{DecodeError, ReadError, Reader, WriteError, Writer};

/// Event tag: a vector was stored or replaced.
pub const VECTOR_UPSERTED: u16 = 80;
/// Event tag: a vector was retired from retrieval.
pub const VECTOR_RETIRED: u16 = 81;
/// Event tag: a proposal was submitted.
pub const PROPOSAL_SUBMITTED: u16 = 96;
/// Event tag: a proposal received a decision.
pub const PROPOSAL_DECIDED: u16 = 97;
/// Event tag: a frontier entry changed.
pub const FRONTIER_CHANGED: u16 = 112;
/// Event tag: a session checkpoint was saved.
pub const CHECKPOINT_SAVED: u16 = 128;

mod t {
    pub(crate) const RECORD_KEY: u16 = 1;
    pub(crate) const REVISION: u16 = 2;
    pub(crate) const STATUS: u16 = 4;
    pub(crate) const RECORDED_BY: u16 = 5;
    pub(crate) const ACTOR_ORIGIN: u16 = 6;
    pub(crate) const REASON: u16 = 7;
    pub(crate) const CREATED_AT: u16 = 8;
    pub(crate) const UPDATED_AT: u16 = 9;
    pub(crate) const LEGACY_ID: u16 = 10;
    pub(crate) const CHANGED_FIELDS: u16 = 13;
    pub(crate) const EXTENSIONS: u16 = 14;
    // Vector.
    pub(crate) const SUBJECT: u16 = 96;
    pub(crate) const SUBJECT_REVISION: u16 = 97;
    pub(crate) const MODEL_ID: u16 = 98;
    pub(crate) const DIMENSIONS: u16 = 99;
    pub(crate) const INPUT_DIGEST: u16 = 100;
    pub(crate) const VALUES_REF: u16 = 101;
    // Proposal.
    pub(crate) const PROPOSAL_KIND: u16 = 112;
    pub(crate) const PROPOSAL_SUBJECT: u16 = 113;
    pub(crate) const BASE_REVISIONS: u16 = 114;
    pub(crate) const PAYLOAD_REF: u16 = 115;
    pub(crate) const REVIEW_NOTE: u16 = 116;
    // Frontier.
    pub(crate) const CONCEPT_REFS: u16 = 128;
    pub(crate) const TEXT_REF: u16 = 129;
    pub(crate) const PRIORITY: u16 = 130;
    // Session checkpoint.
    pub(crate) const CONCEPT: u16 = 48;
    pub(crate) const SESSION_NO: u16 = 132;
    pub(crate) const LEGACY_CHECKPOINT_LOCATION: u16 = 154;
    pub(crate) const CONTENT_REF: u16 = 133;
    // Vector revision counter (tag 171).
    pub(crate) const EMBEDDING_REVISION: u16 = 171;
    // Proposal declared author (tag 168).
    pub(crate) const DECLARED_AUTHOR: u16 = 168;
}

const VECTOR_FIELDS: &[u16] = &[
    1, 2, 4, 5, 6, 7, 8, 9, 13, 14, 96, 97, 98, 99, 100, 101, 171,
];
const VECTOR_REQUIRED: &[u16] = &[1, 2, 4, 6, 8, 96, 97, 98, 99, 100, 101, 171];
const PROPOSAL_FIELDS: &[u16] = &[
    1, 2, 4, 5, 6, 7, 8, 9, 10, 13, 14, 112, 113, 114, 115, 116, 168,
];
const PROPOSAL_REQUIRED: &[u16] = &[1, 2, 4, 6, 8, 112, 113, 114, 115, 168];
// D10's FrontierEntryChanged row grants `legacy_id` (10) to no event of this family and
// `changed_fields` (13) only on a revise revision, so 10 is forbidden here and 13 is kept.
const FRONTIER_FIELDS: &[u16] = &[1, 2, 4, 5, 6, 7, 8, 9, 13, 14, 128, 129, 130];
const FRONTIER_REQUIRED: &[u16] = &[1, 2, 4, 6, 8, 128, 129, 130];
// No tag 4: storage-format §7.3 defines no status domain for kind 9, and D10's
// SessionCheckpointSaved row lists no status field. Emitting an invented `status = 0`
// would be a value no reader could check.
// Same alignment for SessionCheckpointSaved: no `legacy_id`, and `changed_fields` is
// retained because D10's shared rule permits it on a revise revision.
const CHECKPOINT_FIELDS: &[u16] = &[1, 2, 5, 6, 7, 8, 9, 13, 14, 48, 132, 133, 154];
const CHECKPOINT_REQUIRED: &[u16] = &[1, 2, 6, 8, 133];

/// The permitted/required sets for the vector events, for a dispatcher.
pub fn vector_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(VECTOR_FIELDS, VECTOR_REQUIRED)
}

/// The permitted/required sets for the proposal events, for a dispatcher.
pub fn proposal_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(PROPOSAL_FIELDS, PROPOSAL_REQUIRED)
}

/// The permitted/required sets for the frontier events, for a dispatcher.
pub fn frontier_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(FRONTIER_FIELDS, FRONTIER_REQUIRED)
}

/// The permitted/required sets for the session-checkpoint events, for a dispatcher.
pub fn checkpoint_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(CHECKPOINT_FIELDS, CHECKPOINT_REQUIRED)
}

/// Common cross-field checks shared by these four families.
///
/// Extracted because writing them four times is how one family ends up missing a rule:
/// the actor/origin constraint and the creation-revision rule do not depend on what the
/// record means.
fn check_common(
    event_tag: u16,
    revision: RecordRevision,
    is_creation: bool,
    changed_fields: bool,
    common: &CommonFields,
) -> Result<(), WriteError> {
    check_common_with(
        event_tag,
        revision,
        is_creation,
        true,
        changed_fields,
        common,
    )
}

/// As [`check_common`], with the revision-kind rule made explicit.
///
/// `distinct_creation_tag` is false for single-event families: one tag carries both the
/// first and later revisions, so neither "revision must be 1" nor "revision must not be 1"
/// applies. Every other rule - actor/origin, mandatory reason, mandatory update time on
/// The common tail every record family shares: origin, actor, reason, and timestamps.
type CommonTail = (
    ActorOrigin,
    Option<String>,
    Option<String>,
    i64,
    Option<i64>,
);

/// later revisions - is unchanged.
fn check_common_with(
    event_tag: u16,
    revision: RecordRevision,
    is_creation: bool,
    distinct_creation_tag: bool,
    changed_fields: bool,
    common: &CommonFields,
) -> Result<(), WriteError> {
    use crate::wire::EncodeError;

    if distinct_creation_tag {
        if is_creation {
            if revision != RecordRevision::FIRST {
                return Err(WriteError::Data(EncodeError::WrongRevision {
                    event: event_tag,
                    revision: revision.get(),
                }));
            }
        } else if revision == RecordRevision::FIRST {
            return Err(WriteError::Data(EncodeError::WrongRevision {
                event: event_tag,
                revision: revision.get(),
            }));
        }
    }
    if is_creation && changed_fields {
        return Err(WriteError::Data(EncodeError::ForbiddenField {
            event: event_tag,
            field: t::CHANGED_FIELDS,
        }));
    }
    // Mirror of the read side: a family whose creation event is a distinct tag has no
    // change time to stamp at creation. Without this the writer accepted an image its own
    // decoder refuses with ForbiddenField{field: 9}. Single-event families keep stamping
    // their first revision, exactly as before.
    if is_creation && distinct_creation_tag && common.updated_at.is_some() {
        return Err(WriteError::Data(EncodeError::ForbiddenField {
            event: event_tag,
            field: t::UPDATED_AT,
        }));
    }
    match (common.actor_origin, common.recorded_by.is_some()) {
        (ActorOrigin::Authenticated, false) | (ActorOrigin::Unavailable, true) => {
            return Err(WriteError::Data(EncodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            }));
        }
        _ => {}
    }
    if reason_required(event_tag) && common.reason.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: event_tag,
            field: t::REASON,
        }));
    }
    // A LATER revision must say when it happened. Keyed on the revision, not on the
    // creation-tag shape: a single-event family's second revision is still a later one.
    if revision != RecordRevision::FIRST && common.updated_at.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: event_tag,
            field: t::UPDATED_AT,
        }));
    }
    Ok(())
}

/// Enforce the common read-side policy for these families.
///
/// Mirrors [`check_common`] on the write side. Every family calls it, because the
/// alternative - repeating the checks per decoder - is how `VectorRetired` came to accept
/// a missing reason and `ProposalDecided` a missing update time.
fn read_common(
    event: &crate::event::Event<'_>,
    is_creation: bool,
    ledger: &mut dyn SharedLedger,
) -> Result<CommonTail, ReadError> {
    read_common_with(event, is_creation, true, ledger)
}

/// As [`read_common`], with the distinct-creation-tag rule made explicit.
///
/// Single-event families pass `distinct_creation_tag = false`: one tag carries every
/// revision of that record, so tag 9 is optional on the first revision and mandatory from
/// the second one onward.
fn read_common_with(
    event: &crate::event::Event<'_>,
    is_creation: bool,
    distinct_creation_tag: bool,
    ledger: &mut dyn SharedLedger,
) -> Result<CommonTail, ReadError> {
    // `is_creation` means "this is revision 1". For single-event families the caller passes
    // that fact directly, since one tag carries both the first and later revisions.
    let actor_origin = decode_field(event, t::ACTOR_ORIGIN, read_actor_origin)?;
    let recorded_by = decode_opt_field(event, t::RECORDED_BY, |r| {
        read_opt_str(r, MAX_IDENTITY_BYTES, ledger)
    })?;
    match (actor_origin, recorded_by.is_some()) {
        (ActorOrigin::Authenticated, false) | (ActorOrigin::Unavailable, true) => {
            return Err(ReadError::Data(DecodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            }));
        }
        _ => {}
    }
    if is_creation {
        // A first revision has nothing to have changed from. Only a family with a distinct
        // creation tag also forbids a change time, because there the creation event is
        // never also a later revision; single-event families share one tag and may stamp it.
        if distinct_creation_tag {
            forbid(event, t::UPDATED_AT)?;
        }
        forbid(event, t::CHANGED_FIELDS)?;
    } else if event.field(t::UPDATED_AT).is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    if reason_required(event.tag()) && event.field(t::REASON).is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: t::REASON,
        }));
    }
    let reason = decode_opt_field(event, t::REASON, |r| {
        read_opt_str(r, MAX_TEXT_BYTES, ledger)
    })?;
    let created_at = decode_field(event, t::CREATED_AT, |r| r.i64())?;
    let updated_at = decode_opt_field(event, t::UPDATED_AT, |r| r.i64())?;
    Ok((actor_origin, recorded_by, reason, created_at, updated_at))
}

/// Decode the baseline vector: strictly ascending, unique.
///
/// # Errors
///
/// [`ReadError::Data`] for an unsorted or duplicated baseline; the sort order is part of
/// the canonical form because the same set written in two orders would be two encodings
/// of one baseline. An empty baseline round-trips: tag 114 declares an upper bound only.
fn read_base_revisions(
    event: &crate::event::Event<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<Vec<RevPair>, ReadError> {
    let raw = crate::record::require(event, t::BASE_REVISIONS)?;
    let mut reader = Reader::new(raw);
    // A present field carrying count 0 is a present empty baseline, not a missing field:
    // tag 114 declares no lower bound, and whether an empty baseline is admissible is
    // proposal-admission policy, not a codec rule.
    let count = reader.count(256)?;
    // Each pair is a key (18) and a revision (8).
    let minimum = (count as usize).saturating_mul(26);
    if minimum > reader.remaining() {
        return Err(ReadError::Data(DecodeError::Truncated {
            offset: reader.offset(),
            wanted: minimum,
        }));
    }
    ledger.try_charge_entries(u64::from(count))?;
    // Two different sizes: `minimum` is the WIRE size used for the feasibility check
    // above, `slot_bytes` is the RETAINED memory the Vec will hold. Charging the wire size
    // under-charged the table (26 B/element against a 48 B slot here).
    let slot_bytes = (count as u64).saturating_mul(core::mem::size_of::<RevPair>() as u64);
    ledger.try_charge(slot_bytes)?;
    ledger.checkpoint()?;
    let mut pairs = Vec::new();
    pairs
        .try_reserve_exact(count as usize)
        .map_err(|_| ReadError::Allocation {
            requested: slot_bytes as usize,
        })?;
    let mut previous: Option<RevPair> = None;
    for _ in 0..count {
        let key = crate::field::read_record_key(&mut reader)?;
        let revision = crate::field::read_revision(&mut reader)?;
        let pair = RevPair { key, revision };
        if previous.is_some_and(|prev| prev >= pair) {
            return Err(ReadError::Data(DecodeError::FieldOrder {
                tag: t::BASE_REVISIONS,
            }));
        }
        previous = Some(pair);
        pairs.push(pair);
    }
    reader.finish()?;
    Ok(pairs)
}

/// Write a vector event.
///
/// # Errors
///
/// [`WriteError::Data`] for a wrong record kind, a non-positive or over-large dimension
/// count, creation rules, or an actor/origin mismatch; plus budget and control refusals.
pub fn write_vector(
    writer: &mut Writer<'_>,
    retired: bool,
    record: &VectorRecord,
) -> Result<(), WriteError> {
    use crate::wire::EncodeError;

    let event_tag = if retired {
        VECTOR_RETIRED
    } else {
        VECTOR_UPSERTED
    };
    if record.key.kind() != RecordKind::Vector {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.key.kind().as_u16(),
        }));
    }
    if record.embedding_revision == 0 {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: RecordKind::Vector.as_u16(),
        }));
    }
    if record.dimensions == 0 || record.dimensions > crate::field::MAX_DIMENSIONS {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: RecordKind::Vector.as_u16(),
        }));
    }
    if !matches!(
        record.subject.kind(),
        RecordKind::Term | RecordKind::Assertion | RecordKind::Claim | RecordKind::Evidence
    ) {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.subject.kind().as_u16(),
        }));
    }
    // A retired vector is a revision of the same record, never a creation.
    if retired && record.revision == RecordRevision::FIRST {
        return Err(WriteError::Data(EncodeError::WrongRevision {
            event: event_tag,
            revision: record.revision.get(),
        }));
    }
    if record.status
        != if retired {
            VectorStatus::Retired
        } else {
            VectorStatus::Active
        }
    {
        return Err(WriteError::Data(EncodeError::StatusMismatch {
            event: event_tag,
        }));
    }
    // `is_creation` must come from the REVISION, not from the event kind: an upsert at
    // revision 2 is a legitimate later revision of an existing vector, and treating
    // "not retired" as "creation" rejected it.
    let is_creation = record.revision == RecordRevision::FIRST;
    if retired && is_creation {
        return Err(WriteError::Data(EncodeError::WrongRevision {
            event: event_tag,
            revision: record.revision.get(),
        }));
    }
    check_common(
        event_tag,
        record.revision,
        is_creation,
        record.changed_fields.is_some(),
        &record.common,
    )?;

    let ledger = writer.ledger();
    let mut set = FieldSet::with_capacity(VECTOR_FIELDS.len(), ledger)?;
    set.push(ledger, t::RECORD_KEY, record.key, write_record_key)?;
    set.push(ledger, t::REVISION, record.revision, write_revision)?;
    set.push(ledger, t::STATUS, record.status.as_u16(), |w, v| w.u16(v))?;
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.common.recorded_by.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(
        ledger,
        t::ACTOR_ORIGIN,
        record.common.actor_origin,
        write_actor_origin,
    )?;
    set.push_opt(
        ledger,
        t::REASON,
        record.common.reason.as_deref(),
        |w, v| write_str(w, v, MAX_TEXT_BYTES),
    )?;
    set.push(ledger, t::CREATED_AT, record.common.created_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(ledger, t::UPDATED_AT, record.common.updated_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(
        ledger,
        t::CHANGED_FIELDS,
        record.changed_fields.as_deref(),
        write_changed_fields,
    )?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(ledger, t::SUBJECT, record.subject, write_record_key)?;
    set.push(
        ledger,
        t::SUBJECT_REVISION,
        record.subject_revision,
        write_revision,
    )?;
    set.push(ledger, t::MODEL_ID, record.model_id.as_str(), |w, v| {
        write_str(w, v, MAX_MODEL_ID_BYTES)
    })?;
    set.push(ledger, t::DIMENSIONS, record.dimensions, |w, v| w.u32(v))?;
    set.push(ledger, t::INPUT_DIGEST, record.input_digest, |w, v| {
        w.bytes(&v)
    })?;
    set.push(ledger, t::VALUES_REF, record.values_ref, |w, v| w.bytes(&v))?;
    set.push(
        ledger,
        t::EMBEDDING_REVISION,
        record.embedding_revision,
        |w, v| w.u64(v),
    )?;
    set.write_event(writer, event_tag)
}

/// Decode a vector event.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing, forbidden or inconsistent field; budget, control
/// and allocation refusals otherwise.
pub fn read_vector(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<(bool, VectorRecord), ReadError> {
    let event = read_event(reader, ledger)?;
    accept_event(&event)?;
    check_policy(&event, &vector_policy())?;
    let retired = match event.tag() {
        VECTOR_UPSERTED => false,
        VECTOR_RETIRED => true,
        other => {
            return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
                tag: other,
            }));
        }
    };

    let key = decode_field(&event, t::RECORD_KEY, crate::field::read_record_key)?;
    if key.kind() != RecordKind::Vector {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: key.kind().as_u16(),
        }));
    }
    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
    if retired && revision == RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: event.tag(),
            revision: revision.get(),
        }));
    }
    let status_raw = decode_field(&event, t::STATUS, |r| r.u16())?;
    let status =
        VectorStatus::from_u16(status_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::STATUS,
            value: status_raw,
        }))?;
    if status
        != if retired {
            VectorStatus::Retired
        } else {
            VectorStatus::Active
        }
    {
        return Err(ReadError::Data(DecodeError::StatusMismatch {
            event: event.tag(),
        }));
    }
    let dimensions = decode_field(&event, t::DIMENSIONS, |r| r.u32())?;
    if dimensions == 0 || dimensions > crate::field::MAX_DIMENSIONS {
        return Err(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::DIMENSIONS,
            value: u16::try_from(dimensions).unwrap_or(u16::MAX),
        }));
    }
    let subject = decode_field(&event, t::SUBJECT, crate::field::read_record_key)?;
    // Only the four content-bearing kinds can be embedded. An EvidenceLink or Vector
    // subject would describe something that has no text to embed.
    if !matches!(
        subject.kind(),
        RecordKind::Term | RecordKind::Assertion | RecordKind::Claim | RecordKind::Evidence
    ) {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: subject.kind().as_u16(),
        }));
    }
    let embedding_revision = decode_field(&event, t::EMBEDDING_REVISION, |r| r.u64())?;
    if embedding_revision == 0 {
        return Err(ReadError::Data(DecodeError::InvalidRevision {
            value: embedding_revision,
        }));
    }
    let (actor_origin, recorded_by, reason, created_at, updated_at) = read_common(
        &event,
        !retired && revision == RecordRevision::FIRST,
        ledger,
    )?;

    Ok((
        retired,
        VectorRecord {
            key,
            revision,
            status,
            subject,
            subject_revision: decode_field(
                &event,
                t::SUBJECT_REVISION,
                crate::field::read_revision,
            )?,
            model_id: decode_field(&event, t::MODEL_ID, |r| {
                read_str(r, MAX_MODEL_ID_BYTES, ledger)
            })?,
            dimensions,
            input_digest: decode_field(&event, t::INPUT_DIGEST, |r| r.digest32())?,
            values_ref: decode_field(&event, t::VALUES_REF, |r| r.digest32())?,
            embedding_revision,
            common: CommonFields {
                recorded_by,
                actor_origin,
                reason,
                created_at,
                updated_at,
            },
            extensions: match event.field(t::EXTENSIONS) {
                Some(raw) => {
                    let mut r = Reader::new(raw);
                    let ext = read_extensions(&mut r, ledger)?;
                    r.finish()?;
                    ext
                }
                None => Extensions::new(),
            },
            changed_fields: decode_changed_fields(&event, t::CHANGED_FIELDS, ledger)?,
        },
    ))
}

/// Write a proposal event.
///
/// # Errors
///
/// [`WriteError::Data`] for a wrong record kind, an empty or unsorted baseline, a
/// submission carrying a decision, a decision without one, or the common rules.
pub fn write_proposal(
    writer: &mut Writer<'_>,
    decided: bool,
    record: &ProposalRecord,
) -> Result<(), WriteError> {
    use crate::wire::EncodeError;

    let event_tag = if decided {
        PROPOSAL_DECIDED
    } else {
        PROPOSAL_SUBMITTED
    };
    if record.key.kind() != RecordKind::Proposal {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.key.kind().as_u16(),
        }));
    }
    // A submission is a creation with status pending; a decision is a later revision.
    if decided {
        if !record.status.is_decided() {
            return Err(WriteError::Data(EncodeError::StatusMismatch {
                event: event_tag,
            }));
        }
        if record.revision == RecordRevision::FIRST {
            return Err(WriteError::Data(EncodeError::WrongRevision {
                event: event_tag,
                revision: record.revision.get(),
            }));
        }
    } else {
        if record.status != ProposalStatus::Pending {
            return Err(WriteError::Data(EncodeError::StatusMismatch {
                event: event_tag,
            }));
        }
        if record.revision != RecordRevision::FIRST {
            return Err(WriteError::Data(EncodeError::WrongRevision {
                event: event_tag,
                revision: record.revision.get(),
            }));
        }
    }
    if !matches!(record.subject.kind(), RecordKind::Term | RecordKind::Claim)
        || (record.kind == ProposalKind::TermCreate && record.subject.kind() != RecordKind::Term)
    {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.subject.kind().as_u16(),
        }));
    }
    if record.base_revisions.len() > 256 {
        return Err(WriteError::Data(EncodeError::CountTooLong));
    }
    if !record.base_revisions.windows(2).all(|w| w[0] < w[1]) {
        return Err(WriteError::Data(EncodeError::FieldOrder {
            tag: t::BASE_REVISIONS,
        }));
    }
    if decided && record.review_note.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: event_tag,
            field: t::REVIEW_NOTE,
        }));
    }
    if u32::try_from(record.declared_author.len()).map_or(true, |n| n > MAX_DECLARED_AUTHOR) {
        return Err(WriteError::Data(EncodeError::StringTooLong));
    }
    check_common(
        event_tag,
        record.revision,
        !decided,
        record.changed_fields.is_some(),
        &record.common,
    )?;

    let ledger = writer.ledger();
    let mut set = FieldSet::with_capacity(PROPOSAL_FIELDS.len(), ledger)?;
    set.push(ledger, t::RECORD_KEY, record.key, write_record_key)?;
    set.push(ledger, t::REVISION, record.revision, write_revision)?;
    set.push(ledger, t::STATUS, record.status.as_u16(), |w, v| w.u16(v))?;
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.common.recorded_by.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(
        ledger,
        t::ACTOR_ORIGIN,
        record.common.actor_origin,
        write_actor_origin,
    )?;
    set.push_opt(
        ledger,
        t::REASON,
        record.common.reason.as_deref(),
        |w, v| write_str(w, v, MAX_TEXT_BYTES),
    )?;
    set.push(ledger, t::CREATED_AT, record.common.created_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(ledger, t::UPDATED_AT, record.common.updated_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(ledger, t::LEGACY_ID, record.legacy_id.as_deref(), |w, v| {
        write_str(w, v, MAX_IDENTITY_BYTES)
    })?;
    set.push_opt(
        ledger,
        t::CHANGED_FIELDS,
        record.changed_fields.as_deref(),
        write_changed_fields,
    )?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(ledger, t::PROPOSAL_KIND, record.kind.as_u16(), |w, v| {
        w.u16(v)
    })?;
    set.push(
        ledger,
        t::PROPOSAL_SUBJECT,
        record.subject,
        write_record_key,
    )?;
    set.push(
        ledger,
        t::BASE_REVISIONS,
        record.base_revisions.as_slice(),
        |w, v| {
            w.count(v.len())?;
            for pair in v {
                write_record_key(w, pair.key)?;
                write_revision(w, pair.revision)?;
            }
            Ok(())
        },
    )?;
    set.push(ledger, t::PAYLOAD_REF, record.payload_ref, |w, v| {
        w.bytes(&v)
    })?;
    set.push_opt(
        ledger,
        t::REVIEW_NOTE,
        record.review_note.as_deref(),
        |w, v| write_str(w, v, MAX_TEXT_BYTES),
    )?;
    set.push(
        ledger,
        t::DECLARED_AUTHOR,
        record.declared_author.as_str(),
        |w, v| write_str(w, v, MAX_DECLARED_AUTHOR),
    )?;
    set.write_event(writer, event_tag)
}

/// Decode a proposal event.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing, forbidden or inconsistent field; budget, control
/// and allocation refusals otherwise.
pub fn read_proposal(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<(bool, ProposalRecord), ReadError> {
    let event = read_event(reader, ledger)?;
    accept_event(&event)?;
    check_policy(&event, &proposal_policy())?;
    let decided = match event.tag() {
        PROPOSAL_SUBMITTED => false,
        PROPOSAL_DECIDED => true,
        other => {
            return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
                tag: other,
            }));
        }
    };

    let key = decode_field(&event, t::RECORD_KEY, crate::field::read_record_key)?;
    if key.kind() != RecordKind::Proposal {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: key.kind().as_u16(),
        }));
    }
    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
    if decided && revision == RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: event.tag(),
            revision: revision.get(),
        }));
    }
    if !decided && revision != RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: event.tag(),
            revision: revision.get(),
        }));
    }
    let status_raw = decode_field(&event, t::STATUS, |r| r.u16())?;
    let status =
        ProposalStatus::from_u16(status_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::STATUS,
            value: status_raw,
        }))?;
    if decided != status.is_decided() {
        return Err(ReadError::Data(DecodeError::StatusMismatch {
            event: event.tag(),
        }));
    }
    let kind_raw = decode_field(&event, t::PROPOSAL_KIND, |r| r.u16())?;
    let kind =
        ProposalKind::from_u16(kind_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::PROPOSAL_KIND,
            value: kind_raw,
        }))?;
    let subject = decode_field(&event, t::PROPOSAL_SUBJECT, crate::field::read_record_key)?;
    if !matches!(subject.kind(), RecordKind::Term | RecordKind::Claim)
        || (kind == ProposalKind::TermCreate && subject.kind() != RecordKind::Term)
    {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: subject.kind().as_u16(),
        }));
    }
    // Tag 116 is required by the ProposalDecided row.
    if decided && event.field(t::REVIEW_NOTE).is_none() {
        return Err(ReadError::Data(DecodeError::MissingField {
            event: event.tag(),
            field: t::REVIEW_NOTE,
        }));
    }
    let (actor_origin, recorded_by, reason, created_at, updated_at) =
        read_common(&event, !decided, ledger)?;

    Ok((
        decided,
        ProposalRecord {
            key,
            revision,
            status,
            kind,
            subject,
            base_revisions: read_base_revisions(&event, ledger)?,
            payload_ref: decode_field(&event, t::PAYLOAD_REF, |r| r.digest32())?,
            review_note: decode_opt_field(&event, t::REVIEW_NOTE, |r| {
                read_opt_str(r, MAX_TEXT_BYTES, ledger)
            })?,
            declared_author: decode_field(&event, t::DECLARED_AUTHOR, |r| {
                read_str(r, MAX_DECLARED_AUTHOR, ledger)
            })?,
            common: CommonFields {
                recorded_by,
                actor_origin,
                reason,
                created_at,
                updated_at,
            },
            legacy_id: decode_opt_field(&event, t::LEGACY_ID, |r| {
                read_opt_str(r, MAX_IDENTITY_BYTES, ledger)
            })?,
            extensions: match event.field(t::EXTENSIONS) {
                Some(raw) => {
                    let mut r = Reader::new(raw);
                    let ext = read_extensions(&mut r, ledger)?;
                    r.finish()?;
                    ext
                }
                None => Extensions::new(),
            },
            changed_fields: decode_changed_fields(&event, t::CHANGED_FIELDS, ledger)?,
        },
    ))
}

/// Write a frontier-entry event.
///
/// # Errors
///
/// [`WriteError::Data`] for a wrong record kind, a non-term concept reference, creation
/// rules, or an actor/origin mismatch.
pub fn write_frontier(
    writer: &mut Writer<'_>,
    record: &FrontierEntryRecord,
) -> Result<(), WriteError> {
    use crate::wire::EncodeError;

    if record.key.kind() != RecordKind::FrontierEntry {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.key.kind().as_u16(),
        }));
    }
    if record.concept_refs.len() > 64 {
        return Err(WriteError::Data(EncodeError::CountTooLong));
    }
    if record
        .concept_refs
        .iter()
        .any(|r| r.kind() != RecordKind::Term)
    {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: RecordKind::Term.as_u16(),
        }));
    }
    // The reader requires ascending unique keys, so the writer must not emit anything it
    // would refuse - and it must never sort the caller's data to make it legal.
    if !record.concept_refs.windows(2).all(|w| w[0] < w[1]) {
        return Err(WriteError::Data(EncodeError::FieldOrder {
            tag: t::CONCEPT_REFS,
        }));
    }
    // Tag 130 has a closed domain: 0 high, 1 medium, 2 low.
    if record.priority > 2 {
        return Err(WriteError::Data(EncodeError::UnknownEnumValue {
            tag: t::PRIORITY,
            value: u32::from(record.priority),
        }));
    }
    // This family has one event kind: the first revision and later revisions are both
    // reached through it, so `is_creation` only decides the changed_fields rule.
    // Single-event family: one tag carries both the first and later revisions, so the
    // revision-kind rule does not apply, but a later revision still needs a change time.
    check_common_with(
        FRONTIER_CHANGED,
        record.revision,
        record.revision == RecordRevision::FIRST,
        false,
        record.changed_fields.is_some(),
        &record.common,
    )?;

    let ledger = writer.ledger();
    let mut set = FieldSet::with_capacity(FRONTIER_FIELDS.len(), ledger)?;
    set.push(ledger, t::RECORD_KEY, record.key, write_record_key)?;
    set.push(ledger, t::REVISION, record.revision, write_revision)?;
    set.push(ledger, t::STATUS, record.status.as_u16(), |w, v| w.u16(v))?;
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.common.recorded_by.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(
        ledger,
        t::ACTOR_ORIGIN,
        record.common.actor_origin,
        write_actor_origin,
    )?;
    set.push_opt(
        ledger,
        t::REASON,
        record.common.reason.as_deref(),
        |w, v| write_str(w, v, MAX_TEXT_BYTES),
    )?;
    set.push(ledger, t::CREATED_AT, record.common.created_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(ledger, t::UPDATED_AT, record.common.updated_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(
        ledger,
        t::CHANGED_FIELDS,
        record.changed_fields.as_deref(),
        write_changed_fields,
    )?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(
        ledger,
        t::CONCEPT_REFS,
        record.concept_refs.as_slice(),
        |w, v| {
            w.count(v.len())?;
            for key in v {
                write_record_key(w, *key)?;
            }
            Ok(())
        },
    )?;
    set.push(ledger, t::TEXT_REF, record.text_ref, |w, v| w.bytes(&v))?;
    set.push(ledger, t::PRIORITY, record.priority, |w, v| w.u16(v))?;
    set.write_event(writer, FRONTIER_CHANGED)
}

/// Decode a frontier-entry event.
///
/// # Errors
///
/// As [`read_vector`], plus non-term concept references.
pub fn read_frontier(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<FrontierEntryRecord, ReadError> {
    let event = read_event(reader, ledger)?;
    accept_event(&event)?;
    // A single-event family still has to check WHICH event it is: a registered tag from
    // another family would otherwise decode into this record's shape.
    if event.tag() != FRONTIER_CHANGED {
        return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
            tag: event.tag(),
        }));
    }
    check_policy(&event, &frontier_policy())?;

    let key = decode_field(&event, t::RECORD_KEY, crate::field::read_record_key)?;
    if key.kind() != RecordKind::FrontierEntry {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: key.kind().as_u16(),
        }));
    }
    let status_raw = decode_field(&event, t::STATUS, |r| r.u16())?;
    let status =
        FrontierStatus::from_u16(status_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::STATUS,
            value: status_raw,
        }))?;

    let raw = crate::record::require(&event, t::CONCEPT_REFS)?;
    let mut r = Reader::new(raw);
    let count = r.count(64)?;
    let minimum = (count as usize).saturating_mul(18);
    if minimum > r.remaining() {
        return Err(ReadError::Data(DecodeError::Truncated {
            offset: r.offset(),
            wanted: minimum,
        }));
    }
    ledger.try_charge_entries(u64::from(count))?;
    // Wire size for feasibility, retained slot size for the charge.
    let slot_bytes = (count as u64).saturating_mul(core::mem::size_of::<RecordKey>() as u64);
    ledger.try_charge(slot_bytes)?;
    ledger.checkpoint()?;
    let mut concept_refs = Vec::new();
    concept_refs
        .try_reserve_exact(count as usize)
        .map_err(|_| ReadError::Allocation {
            requested: slot_bytes as usize,
        })?;
    let mut previous: Option<RecordKey> = None;
    for _ in 0..count {
        let next = crate::field::read_record_key(&mut r)?;
        if next.kind() != RecordKind::Term {
            return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
                event: event.tag(),
                key_kind: next.kind().as_u16(),
            }));
        }
        // Ascending and unique: a set has one canonical encoding.
        if previous.is_some_and(|prev| prev >= next) {
            return Err(ReadError::Data(DecodeError::FieldOrder {
                tag: t::CONCEPT_REFS,
            }));
        }
        previous = Some(next);
        concept_refs.push(next);
    }
    r.finish()?;

    let priority = decode_field(&event, t::PRIORITY, |r| r.u16())?;
    if priority > 2 {
        return Err(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::PRIORITY,
            value: priority,
        }));
    }
    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
    let (actor_origin, recorded_by, reason, created_at, updated_at) =
        read_common_with(&event, revision == RecordRevision::FIRST, false, ledger)?;

    Ok(FrontierEntryRecord {
        key,
        revision,
        status,
        concept_refs,
        text_ref: decode_field(&event, t::TEXT_REF, |r| r.digest32())?,
        priority,
        common: CommonFields {
            recorded_by,
            actor_origin,
            reason,
            created_at,
            updated_at,
        },
        extensions: match event.field(t::EXTENSIONS) {
            Some(raw) => {
                let mut r = Reader::new(raw);
                let ext = read_extensions(&mut r, ledger)?;
                r.finish()?;
                ext
            }
            None => Extensions::new(),
        },
        changed_fields: decode_changed_fields(&event, t::CHANGED_FIELDS, ledger)?,
    })
}

/// Write a session-checkpoint event.
///
/// # Errors
///
/// [`WriteError::Data`] for a wrong record kind, a zero session number, a non-term
/// concept, creation rules, or an actor/origin mismatch.
pub fn write_checkpoint(
    writer: &mut Writer<'_>,
    record: &SessionCheckpointRecord,
) -> Result<(), WriteError> {
    use crate::wire::EncodeError;

    if record.key.kind() != RecordKind::SessionCheckpoint {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.key.kind().as_u16(),
        }));
    }
    match &record.location {
        CheckpointLocation::Term {
            concept,
            session_no,
        } => {
            if concept.kind() != RecordKind::Term {
                return Err(WriteError::Data(EncodeError::IdOutOfRange {
                    kind: concept.kind().as_u16(),
                }));
            }
            if *session_no == 0 {
                return Err(WriteError::Data(EncodeError::IdOutOfRange {
                    kind: RecordKind::SessionCheckpoint.as_u16(),
                }));
            }
        }
        CheckpointLocation::Slug { slug, session_no } => {
            if !u32::try_from(slug.len()).is_ok_and(|len| len <= crate::request::MAX_PAYLOAD)
                || !CheckpointLocation::valid_public_parts(slug, *session_no)
            {
                return Err(WriteError::Data(EncodeError::InvalidCheckpointLocation));
            }
        }
    }
    check_common_with(
        CHECKPOINT_SAVED,
        record.revision,
        record.revision == RecordRevision::FIRST,
        false,
        record.changed_fields.is_some(),
        &record.common,
    )?;

    let ledger = writer.ledger();
    let mut set = FieldSet::with_capacity(CHECKPOINT_FIELDS.len(), ledger)?;
    set.push(ledger, t::RECORD_KEY, record.key, write_record_key)?;
    set.push(ledger, t::REVISION, record.revision, write_revision)?;
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.common.recorded_by.as_deref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(
        ledger,
        t::ACTOR_ORIGIN,
        record.common.actor_origin,
        write_actor_origin,
    )?;
    set.push_opt(
        ledger,
        t::REASON,
        record.common.reason.as_deref(),
        |w, v| write_str(w, v, MAX_TEXT_BYTES),
    )?;
    set.push(ledger, t::CREATED_AT, record.common.created_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(ledger, t::UPDATED_AT, record.common.updated_at, |w, v| {
        w.i64(v)
    })?;
    set.push_opt(
        ledger,
        t::CHANGED_FIELDS,
        record.changed_fields.as_deref(),
        write_changed_fields,
    )?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    if let CheckpointLocation::Term {
        concept,
        session_no,
    } = &record.location
    {
        set.push(ledger, t::CONCEPT, *concept, write_record_key)?;
        set.push(ledger, t::SESSION_NO, *session_no, |w, v| w.u32(v))?;
    }
    set.push(ledger, t::CONTENT_REF, record.content_ref, |w, v| {
        w.bytes(&v)
    })?;
    if let CheckpointLocation::Slug { slug, session_no } = &record.location {
        set.push(ledger, t::LEGACY_CHECKPOINT_LOCATION, (), |w, ()| {
            write_str(w, slug, crate::request::MAX_PAYLOAD)?;
            w.u64(*session_no)
        })?;
    }
    set.write_event(writer, CHECKPOINT_SAVED)
}

/// Decode a session-checkpoint event.
///
/// # Errors
///
/// As [`read_frontier`].
pub fn read_checkpoint(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<SessionCheckpointRecord, ReadError> {
    let event = read_event(reader, ledger)?;
    accept_event(&event)?;
    if event.tag() != CHECKPOINT_SAVED {
        return Err(ReadError::Uninterpretable(SchemaError::UnknownEvent {
            tag: event.tag(),
        }));
    }
    check_policy(&event, &checkpoint_policy())?;

    let key = decode_field(&event, t::RECORD_KEY, crate::field::read_record_key)?;
    if key.kind() != RecordKind::SessionCheckpoint {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: key.kind().as_u16(),
        }));
    }
    let location = if let Some(raw) = event.field(t::LEGACY_CHECKPOINT_LOCATION) {
        for field in [t::CONCEPT, t::SESSION_NO] {
            if event.field(field).is_some() {
                return Err(ReadError::Data(DecodeError::ForbiddenField {
                    event: event.tag(),
                    field,
                }));
            }
        }
        let mut reader = Reader::new(raw);
        let slug = crate::field::read_str(&mut reader, crate::request::MAX_PAYLOAD, ledger)?;
        let session_no = reader.u64()?;
        reader.finish()?;
        if !CheckpointLocation::valid_public_parts(&slug, session_no) {
            return Err(ReadError::Data(DecodeError::InvalidCheckpointLocation));
        }
        CheckpointLocation::Slug { slug, session_no }
    } else {
        let concept = decode_field(&event, t::CONCEPT, crate::field::read_record_key)?;
        if concept.kind() != RecordKind::Term {
            return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
                event: event.tag(),
                key_kind: concept.kind().as_u16(),
            }));
        }
        let session_no = decode_field(&event, t::SESSION_NO, |r| r.u32())?;
        if session_no == 0 {
            return Err(ReadError::Data(DecodeError::UnknownEnum {
                tag: t::SESSION_NO,
                value: 0,
            }));
        }
        CheckpointLocation::Term {
            concept,
            session_no,
        }
    };
    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
    let (actor_origin, recorded_by, reason, created_at, updated_at) =
        read_common_with(&event, revision == RecordRevision::FIRST, false, ledger)?;
    Ok(SessionCheckpointRecord {
        key,
        revision,
        location,
        content_ref: decode_field(&event, t::CONTENT_REF, |r| r.digest32())?,
        common: CommonFields {
            recorded_by,
            actor_origin,
            reason,
            created_at,
            updated_at,
        },
        extensions: match event.field(t::EXTENSIONS) {
            Some(raw) => {
                let mut r = Reader::new(raw);
                let ext = read_extensions(&mut r, ledger)?;
                r.finish()?;
                ext
            }
            None => Extensions::new(),
        },
        changed_fields: decode_changed_fields(&event, t::CHANGED_FIELDS, ledger)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::encode_with;
    use sage_db_types::ledger::{BudgetError, ControlStop};

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

    fn common(updated: bool) -> CommonFields {
        CommonFields {
            recorded_by: Some("tester".to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1,
            updated_at: if updated { Some(2) } else { None },
        }
    }

    fn vector(retired: bool) -> VectorRecord {
        VectorRecord {
            key: RecordKey::new(RecordKind::Vector, 1),
            revision: if retired {
                RecordRevision::new(2).unwrap()
            } else {
                RecordRevision::FIRST
            },
            status: if retired {
                VectorStatus::Retired
            } else {
                VectorStatus::Active
            },
            subject: RecordKey::term(1),
            subject_revision: RecordRevision::FIRST,
            model_id: "provider/model".to_owned(),
            dimensions: 8,
            input_digest: [9; 32],
            values_ref: [8; 32],
            embedding_revision: 1,
            // Retiring a vector is a terminal event, so §7.2 requires a reason.
            common: CommonFields {
                reason: if retired {
                    Some("model retired".to_owned())
                } else {
                    None
                },
                ..common(retired)
            },
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    fn proposal(decided: bool) -> ProposalRecord {
        ProposalRecord {
            key: RecordKey::new(RecordKind::Proposal, 2),
            revision: if decided {
                RecordRevision::new(2).unwrap()
            } else {
                RecordRevision::FIRST
            },
            status: if decided {
                ProposalStatus::Approved
            } else {
                ProposalStatus::Pending
            },
            kind: ProposalKind::ClaimCreate,
            subject: RecordKey::term(1),
            base_revisions: vec![RevPair {
                key: RecordKey::term(1),
                revision: RecordRevision::FIRST,
            }],
            payload_ref: [7; 32],
            review_note: if decided {
                Some("approved".to_owned())
            } else {
                None
            },
            declared_author: "author".to_owned(),
            common: CommonFields {
                reason: if decided {
                    Some("looks right".to_owned())
                } else {
                    None
                },
                ..common(decided)
            },
            legacy_id: None,
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    fn frontier() -> FrontierEntryRecord {
        FrontierEntryRecord {
            key: RecordKey::new(RecordKind::FrontierEntry, 1),
            revision: RecordRevision::FIRST,
            status: FrontierStatus::Open,
            concept_refs: vec![RecordKey::term(1), RecordKey::term(2)],
            text_ref: [6; 32],
            priority: 1,
            common: common(true),
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    fn checkpoint() -> SessionCheckpointRecord {
        SessionCheckpointRecord {
            key: RecordKey::new(RecordKind::SessionCheckpoint, 1),
            revision: RecordRevision::FIRST,
            location: sage_db_types::persist::CheckpointLocation::Term {
                concept: RecordKey::term(1),
                session_no: 3,
            },
            content_ref: [5; 32],
            common: common(true),
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    #[test]
    fn advisory_changed_fields_survive_both_single_event_families() {
        // The permitted set admits tag 13 on a revise revision. Admitting it and then
        // dropping it is the failure this asserts against: the value must come back.
        let mut f = frontier();
        f.revision = RecordRevision::new(2).unwrap();
        f.common.updated_at = Some(2);
        f.changed_fields = Some(vec![4, 128]);
        let bytes = encode_with(&mut Open, |w| write_frontier(w, &f)).expect("encode");
        let got = read_frontier(&mut Reader::new(&bytes), &mut Open).expect("decode");
        assert_eq!(got.changed_fields, Some(vec![4, 128]));

        let mut c = checkpoint();
        c.revision = RecordRevision::new(2).unwrap();
        c.common.updated_at = Some(2);
        c.changed_fields = Some(vec![48]);
        let bytes = encode_with(&mut Open, |w| write_checkpoint(w, &c)).expect("encode");
        let got = read_checkpoint(&mut Reader::new(&bytes), &mut Open).expect("decode");
        assert_eq!(got.changed_fields, Some(vec![48]));
    }

    /// Rebuild a valid event image with one extra field appended, so a policy test is
    /// not confounded by a missing required field.
    fn with_extra_field<F>(encode: F, tag: u16, value: &[u8]) -> Vec<u8>
    where
        F: FnOnce(&mut Writer<'_>) -> Result<(), WriteError>,
    {
        let bytes = encode_with(&mut Open, encode).expect("base image encodes");
        let mut l = Open;
        let e = crate::event::read_event(&mut Reader::new(&bytes), &mut l).expect("re-read");
        let mut fields: Vec<(u16, Vec<u8>)> = e
            .fields()
            .iter()
            .map(|f| (f.tag(), f.value().to_vec()))
            .collect();
        fields.push((tag, value.to_vec()));
        fields.sort_by_key(|p| p.0);
        let mut out = Vec::new();
        out.extend_from_slice(&e.tag().to_le_bytes());
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
    fn legacy_id_is_refused_on_both_families() {
        // D10 grants `legacy_id` (tag 10) to no event of either family. A reader that
        // discarded it silently would accept data it cannot return.
        let f = frontier();
        let out = with_extra_field(|w| write_frontier(w, &f), t::LEGACY_ID, b"x");
        let got = read_frontier(&mut Reader::new(&out), &mut Open);
        assert!(
            matches!(
                got,
                Err(ReadError::Data(DecodeError::ForbiddenField {
                    field: 10,
                    ..
                }))
            ),
            "frontier accepted legacy_id: {got:?}"
        );

        let c = checkpoint();
        let out = with_extra_field(|w| write_checkpoint(w, &c), t::LEGACY_ID, b"x");
        let got = read_checkpoint(&mut Reader::new(&out), &mut Open);
        assert!(
            matches!(
                got,
                Err(ReadError::Data(DecodeError::ForbiddenField {
                    field: 10,
                    ..
                }))
            ),
            "checkpoint accepted legacy_id: {got:?}"
        );
    }

    #[test]
    fn a_first_revision_cannot_carry_changed_fields() {
        let mut r = frontier();
        r.changed_fields = Some(vec![128]);
        assert!(matches!(
            encode_with(&mut Open, |w| write_frontier(w, &r)),
            Err(WriteError::Data(crate::wire::EncodeError::ForbiddenField {
                field: 13,
                ..
            }))
        ));
    }

    #[test]
    fn a_vector_round_trips_and_keeps_both_revision_bindings() {
        let record = vector(false);
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_vector(w, false, &record)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let (retired, decoded) = read_vector(&mut reader, &mut led).expect("decode");
        assert!(!retired);
        assert_eq!(decoded, record);
        assert_eq!(decoded.subject_revision, RecordRevision::FIRST);
        assert_eq!(decoded.embedding_revision, 1);
    }

    #[test]
    fn a_retired_vector_keeps_its_history_and_status() {
        let record = vector(true);
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_vector(w, true, &record)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let (retired, decoded) = read_vector(&mut reader, &mut led).expect("decode");
        assert!(retired);
        assert_eq!(decoded.status, VectorStatus::Retired);
        assert_eq!(decoded.revision, RecordRevision::new(2).unwrap());
    }

    #[test]
    fn a_vector_cannot_retire_at_the_first_revision() {
        let mut led = Open;
        let mut record = vector(true);
        record.revision = RecordRevision::FIRST;
        assert!(matches!(
            encode_with(&mut led, |w| write_vector(w, true, &record)),
            Err(WriteError::Data(
                crate::wire::EncodeError::WrongRevision { .. }
            ))
        ));
    }

    #[test]
    fn vector_dimensions_are_bounded() {
        let mut led = Open;
        let mut record = vector(false);
        record.dimensions = 0;
        assert!(encode_with(&mut led, |w| write_vector(w, false, &record)).is_err());
        record.dimensions = crate::field::MAX_DIMENSIONS + 1;
        assert!(encode_with(&mut led, |w| write_vector(w, false, &record)).is_err());
        record.dimensions = crate::field::MAX_DIMENSIONS;
        assert!(encode_with(&mut led, |w| write_vector(w, false, &record)).is_ok());
    }

    #[test]
    fn a_submitted_proposal_round_trips_with_its_baseline() {
        let record = proposal(false);
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_proposal(w, false, &record)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let (decided, decoded) = read_proposal(&mut reader, &mut led).expect("decode");
        assert!(!decided);
        assert_eq!(decoded, record);
        assert_eq!(
            decoded.base_revisions.len(),
            1,
            "the baseline vector survives"
        );
    }

    #[test]
    fn a_decided_proposal_requires_a_reason() {
        let mut led = Open;
        let mut record = proposal(true);
        record.common.reason = None;
        assert!(matches!(
            encode_with(&mut led, |w| write_proposal(w, true, &record)),
            Err(WriteError::Data(
                crate::wire::EncodeError::MissingRequiredField { field: 7, .. }
            ))
        ));
    }

    #[test]
    fn an_empty_baseline_round_trips() {
        // Storage-format tag 114 declares only an upper bound (<= 256), ascending order and
        // no duplicates. Whether an empty baseline is admissible is proposal-admission
        // policy (P2), not a major-0 codec rule, so the codec must not invent a refusal.
        let mut led = Open;
        let mut record = proposal(false);
        record.base_revisions.clear();
        let out = encode_with(&mut led, |w| write_proposal(w, false, &record))
            .expect("empty baseline encodes");
        let mut l = Open;
        let (_, got) = read_proposal(&mut Reader::new(&out), &mut l).expect("decodes");
        assert!(got.base_revisions.is_empty());
    }

    #[test]
    fn an_unsorted_baseline_is_refused() {
        let mut led = Open;
        let mut record = proposal(false);
        record.base_revisions = vec![
            RevPair {
                key: RecordKey::term(2),
                revision: RecordRevision::FIRST,
            },
            RevPair {
                key: RecordKey::term(1),
                revision: RecordRevision::FIRST,
            },
        ];
        assert!(matches!(
            encode_with(&mut led, |w| write_proposal(w, false, &record)),
            Err(WriteError::Data(crate::wire::EncodeError::FieldOrder {
                tag: 114
            }))
        ));
    }

    #[test]
    fn a_submission_carrying_a_decision_is_refused() {
        let mut led = Open;
        let record = proposal(true);
        assert!(matches!(
            encode_with(&mut led, |w| write_proposal(w, false, &record)),
            Err(WriteError::Data(
                crate::wire::EncodeError::StatusMismatch { .. }
            ))
        ));
    }

    #[test]
    fn a_frontier_entry_round_trips_in_canonical_order() {
        let record = frontier();
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_frontier(w, &record)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let decoded = read_frontier(&mut reader, &mut led).expect("decode");
        assert_eq!(decoded, record);
    }

    #[test]
    fn frontier_concept_refs_must_be_terms() {
        let mut led = Open;
        let mut record = frontier();
        record.concept_refs = vec![RecordKey::new(RecordKind::Claim, 1)];
        assert!(matches!(
            encode_with(&mut led, |w| write_frontier(w, &record)),
            Err(WriteError::Data(
                crate::wire::EncodeError::IdOutOfRange { .. }
            ))
        ));
    }

    #[test]
    fn a_session_checkpoint_round_trips() {
        let record = checkpoint();
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_checkpoint(w, &record)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let decoded = read_checkpoint(&mut reader, &mut led).expect("decode");
        assert_eq!(decoded, record);
    }

    #[test]
    fn a_public_checkpoint_has_a_separate_literal_slot_field() {
        let mut record = checkpoint();
        record.location = CheckpointLocation::Slug {
            slug: "orphan".to_owned(),
            session_no: 4_294_967_300,
        };
        let mut led = Open;
        let bytes = encode_with(&mut led, |writer| write_checkpoint(writer, &record)).unwrap();
        let event = read_event(&mut Reader::new(&bytes), &mut led).unwrap();
        assert!(event.field(48).is_none());
        assert!(event.field(132).is_none());
        let literal = b"\x06\0\0\0orphan\x04\0\0\0\x01\0\0\0";
        assert_eq!(event.field(154), Some(literal.as_slice()));
        assert_eq!(
            read_checkpoint(&mut Reader::new(&bytes), &mut led).unwrap(),
            record
        );
    }

    #[test]
    fn a_zero_session_number_is_refused() {
        let mut led = Open;
        let mut record = checkpoint();
        record.location = CheckpointLocation::Term {
            concept: RecordKey::term(1),
            session_no: 0,
        };
        assert!(encode_with(&mut led, |w| write_checkpoint(w, &record)).is_err());
    }

    /// Every one of these families must refuse a foreign recognized field.
    #[test]
    fn foreign_fields_are_refused_across_all_four_families() {
        fn append_foreign(bytes: Vec<u8>, ledger: &mut dyn SharedLedger, foreign: u16) -> Vec<u8> {
            // Collect owned tag/value pairs first: a `Field` borrows the input, so the
            // output buffer has to be separate from the one being read.
            let mut reader = Reader::new(&bytes);
            let event = crate::event::read_event(&mut reader, ledger).expect("framing");
            let mut pairs: Vec<(u16, Vec<u8>)> = event
                .fields()
                .iter()
                .map(|f| (f.tag(), f.value().to_vec()))
                .collect();
            pairs.push((foreign, vec![0u8, 0]));
            pairs.sort_by_key(|(tag, _)| *tag);

            let owned: Vec<(u16, Vec<u8>)> = pairs;
            let fields: Vec<crate::event::Field<'_>> = owned
                .iter()
                .map(|(tag, value)| crate::event::Field::new(*tag, value))
                .collect();
            let mut out = Vec::new();
            let mut w = Writer::new(&mut out, ledger);
            crate::event::write_event(
                &mut w,
                event.tag(),
                crate::event::EVENT_FLAG_REQUIRED,
                &fields,
            )
            .expect("re-encode");
            out
        }

        let mut led = Open;

        let v = encode_with(&mut led, |w| write_vector(w, false, &vector(false))).unwrap();
        let v = append_foreign(v, &mut led, 64);
        assert!(matches!(
            read_vector(&mut Reader::new(&v), &mut led),
            Err(ReadError::Data(DecodeError::ForbiddenField {
                field: 64,
                ..
            }))
        ));

        let p = encode_with(&mut led, |w| write_proposal(w, false, &proposal(false))).unwrap();
        let p = append_foreign(p, &mut led, 64);
        assert!(matches!(
            read_proposal(&mut Reader::new(&p), &mut led),
            Err(ReadError::Data(DecodeError::ForbiddenField {
                field: 64,
                ..
            }))
        ));

        let f = encode_with(&mut led, |w| write_frontier(w, &frontier())).unwrap();
        let f = append_foreign(f, &mut led, 64);
        assert!(matches!(
            read_frontier(&mut Reader::new(&f), &mut led),
            Err(ReadError::Data(DecodeError::ForbiddenField {
                field: 64,
                ..
            }))
        ));

        let c = encode_with(&mut led, |w| write_checkpoint(w, &checkpoint())).unwrap();
        let c = append_foreign(c, &mut led, 64);
        assert!(matches!(
            read_checkpoint(&mut Reader::new(&c), &mut led),
            Err(ReadError::Data(DecodeError::ForbiddenField {
                field: 64,
                ..
            }))
        ));
    }
}
