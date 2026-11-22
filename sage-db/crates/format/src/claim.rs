//! Claim after-image codec.
//!
//! A claim is prose about a term, with an argumentative role and a lifecycle state. The
//! prose itself is a blob: `text_ref` is a 32-byte hash, for the same reason a term body
//! is - a revision must not rewrite the document, and reading one revision must not pull
//! in unrelated records.
//!
//! Field tags (`storage-format` §6):
//!
//! | tag | value |
//! |---|---|
//! | 1 | record key, kind must be Claim |
//! | 2 | revision |
//! | 4 | status (0 accepted, 1 superseded, 2 rejected) |
//! | 5 | recorded_by |
//! | 6 | actor_origin |
//! | 7 | reason (mandatory on supersede and reject) |
//! | 8 | created_at |
//! | 9 | updated_at |
//! | 10, 12 | legacy id, legacy content digest |
//! | 13 | changed_fields (advisory) |
//! | 14 | extensions |
//! | 21 | confidence |
//! | 48 | concept (record key, kind Term) |
//! | 49 | claim kind |
//! | 50 | text_ref |
//!
//! ## Status versus event
//!
//! `ClaimSuperseded` and `ClaimRejected` are separate events rather than one "status
//! changed" event because the two have different meaning for a reader: superseded says
//! "a newer claim replaced this", rejected says "a reviewer refused this". A single event
//! with a status payload would let either be written under the other's tag.

use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ActorOrigin, ClaimKind, ClaimRecord, ClaimStatus, CommonFields, Extensions, RecordKind,
};

use crate::event::read_event;
use crate::field::{
    MAX_IDENTITY_BYTES, MAX_TEXT_BYTES, read_actor_origin, read_confidence, read_extensions,
    read_opt_str, write_actor_origin, write_confidence, write_extensions, write_record_key,
    write_revision, write_str,
};
use crate::record::{
    FieldSet, check_policy, decode_changed_fields, decode_field, decode_opt_field, forbid,
    reason_required, write_changed_fields,
};
use crate::registry::{SchemaError, accept_event};
use crate::wire::{DecodeError, ReadError, Reader, WriteError, Writer};

/// Event tag: first revision of a claim.
pub const CLAIM_CREATED: u16 = 32;
/// Event tag: a later revision.
pub const CLAIM_REVISED: u16 = 33;
/// Event tag: superseded by another claim.
pub const CLAIM_SUPERSEDED: u16 = 34;
/// Event tag: refused by review.
pub const CLAIM_REJECTED: u16 = 35;

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
    pub(crate) const LEGACY_CONTENT: u16 = 12;
    pub(crate) const CHANGED_FIELDS: u16 = 13;
    pub(crate) const EXTENSIONS: u16 = 14;
    pub(crate) const CONFIDENCE: u16 = 21;
    pub(crate) const CONCEPT: u16 = 48;
    pub(crate) const CLAIM_KIND: u16 = 49;
    pub(crate) const TEXT_REF: u16 = 50;
}

/// Field tags a claim event may carry.
const CLAIM_FIELDS: &[u16] = &[1, 2, 4, 5, 6, 7, 8, 9, 10, 12, 13, 14, 21, 48, 49, 50];

/// Fields every claim event must carry.
const CLAIM_REQUIRED: &[u16] = &[1, 2, 4, 6, 8, 21, 48, 49, 50];

/// The permitted/required sets for this family, for a dispatcher that must apply the
/// same policy instead of keeping a second copy of the table.
pub fn claim_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(CLAIM_FIELDS, CLAIM_REQUIRED)
}

/// Which claim event is being encoded or decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClaimEvent {
    /// First revision, status accepted.
    Created,
    /// A later revision that keeps the current status.
    Revised,
    /// Superseded by another claim.
    Superseded,
    /// Refused by review.
    Rejected,
}

impl ClaimEvent {
    /// The wire tag.
    #[must_use]
    pub const fn tag(self) -> u16 {
        match self {
            Self::Created => CLAIM_CREATED,
            Self::Revised => CLAIM_REVISED,
            Self::Superseded => CLAIM_SUPERSEDED,
            Self::Rejected => CLAIM_REJECTED,
        }
    }

    /// Decode from a wire tag.
    #[must_use]
    pub const fn from_tag(tag: u16) -> Option<Self> {
        match tag {
            CLAIM_CREATED => Some(Self::Created),
            CLAIM_REVISED => Some(Self::Revised),
            CLAIM_SUPERSEDED => Some(Self::Superseded),
            CLAIM_REJECTED => Some(Self::Rejected),
            _ => None,
        }
    }

    const fn is_creation(self) -> bool {
        matches!(self, Self::Created)
    }

    /// The statuses this event may produce.
    ///
    /// `Revised` deliberately allows both `Accepted` and `Superseded`, because a revision
    /// may restore a superseded claim to accepted; what it may *not* do is carry a
    /// terminal change, which has its own event.
    const fn allows(self, status: ClaimStatus) -> bool {
        match self {
            Self::Created | Self::Revised => {
                matches!(status, ClaimStatus::Accepted | ClaimStatus::Superseded)
            }
            Self::Superseded => matches!(status, ClaimStatus::Superseded),
            Self::Rejected => matches!(status, ClaimStatus::Rejected),
        }
    }
}

/// Write a claim event: the complete after-image of `record`.
///
/// # Errors
///
/// [`WriteError::Data`] for a wrong record kind, a concept that is not a term, a status
/// the event does not allow, a missing reason where §7.2 requires one, revision rules,
/// or an actor/origin mismatch; plus budget, control and allocation refusals.
pub fn write_claim(
    writer: &mut Writer<'_>,
    event: ClaimEvent,
    record: &ClaimRecord,
) -> Result<(), WriteError> {
    use crate::wire::EncodeError;

    if record.key.kind() != RecordKind::Claim {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.key.kind().as_u16(),
        }));
    }
    if record.concept.kind() != RecordKind::Term {
        return Err(WriteError::Data(EncodeError::IdOutOfRange {
            kind: record.concept.kind().as_u16(),
        }));
    }
    if event.is_creation() {
        if record.revision != RecordRevision::FIRST {
            return Err(WriteError::Data(EncodeError::WrongRevision {
                event: event.tag(),
                revision: record.revision.get(),
            }));
        }
    } else if record.revision == RecordRevision::FIRST {
        return Err(WriteError::Data(EncodeError::WrongRevision {
            event: event.tag(),
            revision: record.revision.get(),
        }));
    }
    if !event.allows(record.status) {
        return Err(WriteError::Data(EncodeError::StatusMismatch {
            event: event.tag(),
        }));
    }
    if event.is_creation() && record.common.updated_at.is_some() {
        return Err(WriteError::Data(EncodeError::ForbiddenField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    if event.is_creation() && record.changed_fields.is_some() {
        // A creation has no predecessor to diff against, so `changed_fields` would be a
        // claim the reader cannot justify. Refused before any byte is produced.
        return Err(WriteError::Data(EncodeError::ForbiddenField {
            event: event.tag(),
            field: t::CHANGED_FIELDS,
        }));
    }
    if !event.is_creation() && record.common.updated_at.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    if reason_required(event.tag()) && record.common.reason.is_none() {
        return Err(WriteError::Data(EncodeError::MissingRequiredField {
            event: event.tag(),
            field: t::REASON,
        }));
    }
    match (
        record.common.actor_origin,
        record.common.recorded_by.is_some(),
    ) {
        (ActorOrigin::Authenticated, false) | (ActorOrigin::Unavailable, true) => {
            return Err(WriteError::Data(EncodeError::ActorOriginMismatch {
                tag: t::RECORDED_BY,
            }));
        }
        _ => {}
    }

    let ledger = writer.ledger();
    let mut set = FieldSet::with_capacity(CLAIM_FIELDS.len(), ledger)?;
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
        t::LEGACY_CONTENT,
        record.legacy_content_digest,
        |w, v| w.bytes(&v),
    )?;
    set.push_opt(
        ledger,
        t::CHANGED_FIELDS,
        record.changed_fields.as_deref(),
        write_changed_fields,
    )?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(ledger, t::CONFIDENCE, record.confidence, write_confidence)?;
    set.push(ledger, t::CONCEPT, record.concept, write_record_key)?;
    set.push(ledger, t::CLAIM_KIND, record.kind.as_u16(), |w, v| w.u16(v))?;
    set.push(ledger, t::TEXT_REF, record.text_ref, |w, v| w.bytes(&v))?;
    set.write_event(writer, event.tag())
}

/// Decode a claim event into its complete after-image.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing, forbidden or inconsistent field; budget, control
/// and allocation refusals otherwise.
pub fn read_claim(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<(ClaimEvent, ClaimRecord), ReadError> {
    let event = read_event(reader, ledger)?;
    accept_event(&event)?;
    check_policy(&event, &claim_policy())?;
    let kind = ClaimEvent::from_tag(event.tag()).ok_or(ReadError::Uninterpretable(
        SchemaError::UnknownEvent { tag: event.tag() },
    ))?;

    let key = decode_field(&event, t::RECORD_KEY, crate::field::read_record_key)?;
    if key.kind() != RecordKind::Claim {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: key.kind().as_u16(),
        }));
    }
    let concept = decode_field(&event, t::CONCEPT, crate::field::read_record_key)?;
    if concept.kind() != RecordKind::Term {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: concept.kind().as_u16(),
        }));
    }
    let revision = decode_field(&event, t::REVISION, crate::field::read_revision)?;
    if kind.is_creation() && revision != RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: event.tag(),
            revision: revision.get(),
        }));
    }
    if !kind.is_creation() && revision == RecordRevision::FIRST {
        return Err(ReadError::Data(DecodeError::WrongRevision {
            event: event.tag(),
            revision: revision.get(),
        }));
    }

    let status_raw = decode_field(&event, t::STATUS, |r| r.u16())?;
    let status =
        ClaimStatus::from_u16(status_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::STATUS,
            value: status_raw,
        }))?;
    if !kind.allows(status) {
        return Err(ReadError::Data(DecodeError::StatusMismatch {
            event: event.tag(),
        }));
    }
    let claim_kind_raw = decode_field(&event, t::CLAIM_KIND, |r| r.u16())?;
    let claim_kind =
        ClaimKind::from_u16(claim_kind_raw).ok_or(ReadError::Data(DecodeError::UnknownEnum {
            tag: t::CLAIM_KIND,
            value: claim_kind_raw,
        }))?;

    let actor_origin = decode_field(&event, t::ACTOR_ORIGIN, read_actor_origin)?;
    let recorded_by = decode_opt_field(&event, t::RECORDED_BY, |r| {
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

    if kind.is_creation() {
        forbid(&event, t::UPDATED_AT)?;
        forbid(&event, t::CHANGED_FIELDS)?;
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

    Ok((
        kind,
        ClaimRecord {
            key,
            revision,
            status,
            concept,
            kind: claim_kind,
            text_ref: decode_field(&event, t::TEXT_REF, |r| r.digest32())?,
            confidence: decode_field(&event, t::CONFIDENCE, read_confidence)?,
            common: CommonFields {
                recorded_by,
                actor_origin,
                reason: decode_opt_field(&event, t::REASON, |r| {
                    read_opt_str(r, MAX_TEXT_BYTES, ledger)
                })?,
                created_at: decode_field(&event, t::CREATED_AT, |r| r.i64())?,
                updated_at: decode_opt_field(&event, t::UPDATED_AT, |r| r.i64())?,
            },
            legacy_id: decode_opt_field(&event, t::LEGACY_ID, |r| {
                read_opt_str(r, MAX_IDENTITY_BYTES, ledger)
            })?,
            legacy_content_digest: decode_opt_field(&event, t::LEGACY_CONTENT, |r| r.digest32())?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::encode_with;
    use sage_db_types::confidence::Confidence;
    use sage_db_types::ledger::{BudgetError, ControlStop};
    use sage_db_types::persist::RecordKey;

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

    fn sample(revision: u64, status: ClaimStatus) -> ClaimRecord {
        ClaimRecord {
            key: RecordKey::new(RecordKind::Claim, 3),
            revision: RecordRevision::new(revision).unwrap(),
            status,
            concept: RecordKey::term(1),
            kind: ClaimKind::Decision,
            text_ref: [0x33; 32],
            confidence: Confidence::Confirmed,
            common: CommonFields {
                recorded_by: Some("tester".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: if revision > 1 {
                    Some("reviewed".to_owned())
                } else {
                    None
                },
                created_at: 1_763_424_000_000_000,
                updated_at: if revision > 1 {
                    Some(1_763_424_000_001_000)
                } else {
                    None
                },
            },
            legacy_id: None,
            legacy_content_digest: None,
            extensions: Extensions::new(),
            changed_fields: None,
        }
    }

    fn roundtrip(event: ClaimEvent, record: &ClaimRecord) -> ClaimRecord {
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_claim(w, event, record)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let (decoded_event, decoded) = read_claim(&mut reader, &mut led).expect("decode");
        assert_eq!(decoded_event, event);
        assert!(reader.is_empty());
        decoded
    }

    #[test]
    fn a_created_claim_round_trips() {
        let record = sample(1, ClaimStatus::Accepted);
        assert_eq!(roundtrip(ClaimEvent::Created, &record), record);
    }

    #[test]
    fn a_revised_claim_carries_its_legacy_and_extensions() {
        let mut record = sample(2, ClaimStatus::Accepted);
        record.legacy_id = Some("legacy-claim".to_owned());
        record.legacy_content_digest = Some([0x44; 32]);
        record
            .extensions
            .insert("legacy.provider", b"z".to_vec())
            .unwrap();
        record.changed_fields = Some(vec![21]);
        assert_eq!(roundtrip(ClaimEvent::Revised, &record), record);
    }

    #[test]
    fn supersede_and_reject_round_trip_with_their_own_status() {
        let superseded = sample(2, ClaimStatus::Superseded);
        assert_eq!(
            roundtrip(ClaimEvent::Superseded, &superseded).status,
            ClaimStatus::Superseded
        );
        let rejected = sample(2, ClaimStatus::Rejected);
        assert_eq!(
            roundtrip(ClaimEvent::Rejected, &rejected).status,
            ClaimStatus::Rejected
        );
    }

    /// The two terminal events are not interchangeable: each has its own meaning.
    #[test]
    fn a_terminal_status_cannot_be_written_under_the_other_event() {
        let mut led = Open;
        let rejected = sample(2, ClaimStatus::Rejected);
        assert!(matches!(
            encode_with(&mut led, |w| write_claim(
                w,
                ClaimEvent::Superseded,
                &rejected
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::StatusMismatch { .. }
            ))
        ));
        let superseded = sample(2, ClaimStatus::Superseded);
        assert!(matches!(
            encode_with(&mut led, |w| write_claim(
                w,
                ClaimEvent::Rejected,
                &superseded
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::StatusMismatch { .. }
            ))
        ));
    }

    #[test]
    fn terminal_events_require_a_reason() {
        let mut led = Open;
        let mut no_reason = sample(2, ClaimStatus::Rejected);
        no_reason.common.reason = None;
        assert!(matches!(
            encode_with(&mut led, |w| write_claim(
                w,
                ClaimEvent::Rejected,
                &no_reason
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::MissingRequiredField { field: 7, .. }
            ))
        ));
    }

    #[test]
    fn a_non_term_concept_is_refused() {
        let mut led = Open;
        let mut record = sample(1, ClaimStatus::Accepted);
        record.concept = RecordKey::new(RecordKind::Evidence, 5);
        assert!(matches!(
            encode_with(&mut led, |w| write_claim(w, ClaimEvent::Created, &record)),
            Err(WriteError::Data(
                crate::wire::EncodeError::IdOutOfRange { .. }
            ))
        ));
    }

    #[test]
    fn a_foreign_field_is_refused() {
        // Append tag 64 (evidence_kind) to a valid claim event: globally registered,
        // foreign to this family.
        let mut led = Open;
        let record = sample(1, ClaimStatus::Accepted);
        let bytes = encode_with(&mut led, |w| write_claim(w, ClaimEvent::Created, &record))
            .expect("encode");
        let mut reader = Reader::new(&bytes);
        let event = crate::event::read_event(&mut reader, &mut led).expect("framing");
        let mut fields: Vec<crate::event::Field<'_>> = event.fields().to_vec();
        fields.push(crate::event::Field::new(64, &[0, 0]));
        let mut rebuilt = Vec::new();
        {
            let mut w = Writer::new(&mut rebuilt, &mut led);
            crate::event::write_event(
                &mut w,
                event.tag(),
                crate::event::EVENT_FLAG_REQUIRED,
                &fields,
            )
            .expect("re-encode");
        }
        let mut reader = Reader::new(&rebuilt);
        assert!(matches!(
            read_claim(&mut reader, &mut led),
            Err(ReadError::Data(DecodeError::ForbiddenField {
                field: 64,
                ..
            }))
        ));
    }

    #[test]
    fn the_claim_text_is_a_reference_not_prose() {
        let record = sample(1, ClaimStatus::Accepted);
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_claim(w, ClaimEvent::Created, &record))
            .expect("encode");
        assert!(
            bytes.len() < 400,
            "a claim event carries a hash, not the text"
        );
    }
}
