//! Assertion after-image codec.
//!
//! An assertion is an authored support with its own identity, revision and direction.
//! Two assertions may share `(from, relation, target)`; they are separate records and
//! are never merged, so this codec never normalises direction - even for symmetric
//! relations, where a reader may *derive* the reverse but the stored bytes keep what the
//! author wrote.
//!
//! Field tags (see `storage-format` §6):
//!
//! | tag | value |
//! |---|---|
//! | 1 | record key, kind must be Assertion |
//! | 2 | revision |
//! | 3 | declaration_order |
//! | 4 | status (0 active, 1 retracted) |
//! | 5 | recorded_by |
//! | 6 | actor_origin |
//! | 7 | reason |
//! | 8 | created_at |
//! | 9 | updated_at |
//! | 10, 12 | legacy id, legacy content digest |
//! | 13 | changed_fields (advisory) |
//! | 14 | extensions |
//! | 21 | confidence |
//! | 32 | from (record key, kind Term) |
//! | 33 | target (record key, kind Term) |
//! | 34 | relation |

use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    ActorOrigin, AssertionRecordFull, CommonFields, Extensions, RecordKind,
};

use crate::event::read_event;
use crate::field::{
    MAX_IDENTITY_BYTES, MAX_TEXT_BYTES, read_actor_origin, read_confidence, read_extensions,
    read_opt_str, read_relation, write_actor_origin, write_confidence, write_extensions,
    write_record_key, write_relation, write_revision, write_str,
};
use crate::record::{FieldSet, decode_field, decode_opt_field, forbid};
use crate::registry::{SchemaError, accept_event};
use crate::wire::{DecodeError, ReadError, Reader, WriteError, Writer};

/// Event tag: first revision of an assertion.
pub const ASSERTION_CREATED: u16 = 16;
/// Event tag: a later revision.
pub const ASSERTION_REVISED: u16 = 17;
/// Event tag: retraction.
pub const ASSERTION_RETRACTED: u16 = 18;

mod t {
    pub(crate) const RECORD_KEY: u16 = 1;
    pub(crate) const REVISION: u16 = 2;
    pub(crate) const DECLARATION_ORDER: u16 = 3;
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
    pub(crate) const FROM: u16 = 32;
    pub(crate) const TARGET: u16 = 33;
    pub(crate) const RELATION: u16 = 34;
}

/// Field tags an assertion event may carry.
///
/// Anything outside this list is foreign to this family: globally-known tags such as
/// `evidence_kind` (64) belong to another record and must be refused rather than accepted
/// and then not stored.
const ASSERTION_FIELDS: &[u16] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 13, 14, 21, 32, 33, 34];

/// Fields every assertion event must carry.
const ASSERTION_REQUIRED: &[u16] = &[1, 2, 3, 4, 6, 8, 21, 32, 33, 34];

/// The assertion family's field policy.
/// The permitted/required sets for this family, for a dispatcher that must apply the
/// same policy instead of keeping a second copy of the table.
pub fn assertion_policy() -> crate::record::FieldPolicy {
    crate::record::FieldPolicy::new(ASSERTION_FIELDS, ASSERTION_REQUIRED)
}

/// Which assertion event is being encoded or decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AssertionEvent {
    /// First revision.
    Created,
    /// A later revision with the complete after-image.
    Revised,
    /// Retraction with the complete after-image.
    Retracted,
}

impl AssertionEvent {
    /// The wire tag.
    #[must_use]
    pub const fn tag(self) -> u16 {
        match self {
            Self::Created => ASSERTION_CREATED,
            Self::Revised => ASSERTION_REVISED,
            Self::Retracted => ASSERTION_RETRACTED,
        }
    }

    /// Decode from a wire tag.
    #[must_use]
    pub const fn from_tag(tag: u16) -> Option<Self> {
        match tag {
            ASSERTION_CREATED => Some(Self::Created),
            ASSERTION_REVISED => Some(Self::Revised),
            ASSERTION_RETRACTED => Some(Self::Retracted),
            _ => None,
        }
    }

    const fn is_creation(self) -> bool {
        matches!(self, Self::Created)
    }
}

/// Write an assertion event: the complete after-image of `record`.
///
/// # Errors
///
/// [`WriteError::Data`] for a wrong record kind, a creation event carrying revision
/// fields, a status that contradicts the event, an actor/origin mismatch, a term endpoint
/// id out of range, or a forbidden relation value; plus budget and allocation refusals.
pub fn write_assertion(
    writer: &mut Writer<'_>,
    event: AssertionEvent,
    record: &AssertionRecordFull,
) -> Result<(), WriteError> {
    // Cross-field rules before any bytes are produced.
    if record.key.kind() != RecordKind::Assertion {
        return Err(WriteError::Data(crate::wire::EncodeError::IdOutOfRange {
            kind: record.key.kind().as_u16(),
        }));
    }
    if record.from.kind() != RecordKind::Term || record.target.kind() != RecordKind::Term {
        return Err(WriteError::Data(crate::wire::EncodeError::IdOutOfRange {
            kind: record.from.kind().as_u16(),
        }));
    }
    // Revision facts a single event can decide: the first revision belongs to creation,
    // and a non-creation revision cannot be 1. Whether revision N follows N-1 is a fold
    // question for replay, not something a codec may guess from one event.
    if event.is_creation() {
        if record.revision != RecordRevision::FIRST {
            return Err(WriteError::Data(crate::wire::EncodeError::WrongRevision {
                event: event.tag(),
                revision: record.revision.get(),
            }));
        }
    } else if record.revision == RecordRevision::FIRST {
        return Err(WriteError::Data(crate::wire::EncodeError::WrongRevision {
            event: event.tag(),
            revision: record.revision.get(),
        }));
    }
    if event.is_creation() && record.common.updated_at.is_some() {
        return Err(WriteError::Data(crate::wire::EncodeError::ForbiddenField {
            event: event.tag(),
            field: t::UPDATED_AT,
        }));
    }
    if !event.is_creation() && record.common.updated_at.is_none() {
        return Err(WriteError::Data(
            crate::wire::EncodeError::MissingRequiredField {
                event: event.tag(),
                field: t::UPDATED_AT,
            },
        ));
    }
    // Terminal and administrative events must record why they happened.
    if crate::record::reason_required(event.tag()) && record.common.reason.is_none() {
        return Err(WriteError::Data(
            crate::wire::EncodeError::MissingRequiredField {
                event: event.tag(),
                field: t::REASON,
            },
        ));
    }
    if event.is_creation() && record.changed_fields.is_some() {
        return Err(WriteError::Data(crate::wire::EncodeError::ForbiddenField {
            event: event.tag(),
            field: t::CHANGED_FIELDS,
        }));
    }
    let expect_retracted = event == AssertionEvent::Retracted;
    if record.retracted != expect_retracted {
        return Err(WriteError::Data(crate::wire::EncodeError::StatusMismatch {
            event: event.tag(),
        }));
    }
    match (
        record.common.actor_origin,
        record.common.recorded_by.is_some(),
    ) {
        (ActorOrigin::Authenticated, false) | (ActorOrigin::Unavailable, true) => {
            return Err(WriteError::Data(
                crate::wire::EncodeError::ActorOriginMismatch {
                    tag: t::RECORDED_BY,
                },
            ));
        }
        _ => {}
    }

    let ledger = writer.ledger();
    let mut set = FieldSet::with_capacity(ASSERTION_FIELDS.len(), ledger)?;
    set.push(ledger, t::RECORD_KEY, record.key, write_record_key)?;
    set.push(ledger, t::REVISION, record.revision, write_revision)?;
    set.push(
        ledger,
        t::DECLARATION_ORDER,
        &record.declaration_order,
        |w, v| w.u32(*v),
    )?;
    let status: u16 = u16::from(record.retracted);
    set.push(ledger, t::STATUS, &status, |w, v| w.u16(*v))?;
    set.push_opt(
        ledger,
        t::RECORDED_BY,
        record.common.recorded_by.as_ref(),
        |w, v| write_str(w, v, MAX_IDENTITY_BYTES),
    )?;
    set.push(
        ledger,
        t::ACTOR_ORIGIN,
        record.common.actor_origin,
        write_actor_origin,
    )?;
    set.push_opt(ledger, t::REASON, record.common.reason.as_ref(), |w, v| {
        write_str(w, v, MAX_TEXT_BYTES)
    })?;
    set.push(ledger, t::CREATED_AT, &record.common.created_at, |w, v| {
        w.i64(*v)
    })?;
    set.push_opt(
        ledger,
        t::UPDATED_AT,
        record.common.updated_at.as_ref(),
        |w, v| w.i64(*v),
    )?;
    set.push_opt(ledger, t::LEGACY_ID, record.legacy_id.as_ref(), |w, v| {
        write_str(w, v, MAX_IDENTITY_BYTES)
    })?;
    set.push_opt(
        ledger,
        t::LEGACY_CONTENT,
        record.legacy_content_digest.as_ref(),
        |w, v| w.bytes(v),
    )?;
    set.push_opt(
        ledger,
        t::CHANGED_FIELDS,
        record.changed_fields.as_ref(),
        |w, v| {
            w.count(v.len())?;
            for tag in v {
                w.u16(*tag)?;
            }
            Ok(())
        },
    )?;
    if !record.extensions.entries().is_empty() {
        set.push(ledger, t::EXTENSIONS, &record.extensions, write_extensions)?;
    }
    set.push(ledger, t::CONFIDENCE, record.confidence, write_confidence)?;
    set.push(ledger, t::FROM, record.from, write_record_key)?;
    set.push(ledger, t::TARGET, record.target, write_record_key)?;
    set.push(ledger, t::RELATION, record.relation, write_relation)?;
    set.write_event(writer, event.tag())
}

/// Decode an assertion event into its complete after-image.
///
/// # Errors
///
/// [`ReadError::Data`] for a missing, forbidden or inconsistent field;
/// [`ReadError::Uninterpretable`] for a tag the registry rejects; budget, control and
/// allocation refusals otherwise.
pub fn read_assertion(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<(AssertionEvent, AssertionRecordFull), ReadError> {
    let event = read_event(reader, ledger)?;
    accept_event(&event)?;
    crate::record::check_policy(&event, &assertion_policy())?;
    let kind = AssertionEvent::from_tag(event.tag()).ok_or(ReadError::Uninterpretable(
        SchemaError::UnknownEvent { tag: event.tag() },
    ))?;

    let key = decode_field(&event, t::RECORD_KEY, crate::field::read_record_key)?;
    if key.kind() != RecordKind::Assertion {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: key.kind().as_u16(),
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
    let from = decode_field(&event, t::FROM, crate::field::read_record_key)?;
    let target = decode_field(&event, t::TARGET, crate::field::read_record_key)?;
    if from.kind() != RecordKind::Term || target.kind() != RecordKind::Term {
        return Err(ReadError::Data(DecodeError::RecordKeyKindMismatch {
            event: event.tag(),
            key_kind: from.kind().as_u16(),
        }));
    }

    let status = decode_field(&event, t::STATUS, |r| r.u16())?;
    let retracted = match status {
        0 => false,
        1 => true,
        other => {
            return Err(ReadError::Data(DecodeError::UnknownEnum {
                tag: t::STATUS,
                value: other,
            }));
        }
    };
    if retracted != (kind == AssertionEvent::Retracted) {
        return Err(ReadError::Data(DecodeError::StatusMismatch {
            event: event.tag(),
        }));
    }

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

    let record = AssertionRecordFull {
        key,
        revision,
        retracted,
        from,
        relation: decode_field(&event, t::RELATION, read_relation)?,
        target,
        confidence: decode_field(&event, t::CONFIDENCE, read_confidence)?,
        declaration_order: decode_field(&event, t::DECLARATION_ORDER, |r| r.u32())?,
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
        changed_fields: crate::record::decode_changed_fields(&event, t::CHANGED_FIELDS, ledger)?,
    };
    Ok((kind, record))
}

/// Convenience for callers that only need the record.
///
/// # Errors
///
/// As [`read_assertion`].
pub fn read_assertion_record(
    reader: &mut Reader<'_>,
    ledger: &mut dyn SharedLedger,
) -> Result<AssertionRecordFull, ReadError> {
    let (_, record) = read_assertion(reader, ledger)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::encode_with;
    use sage_db_types::confidence::Confidence;
    use sage_db_types::ledger::{BudgetError, ControlStop};
    use sage_db_types::persist::RecordKey;
    use sage_db_types::relation::RelationId;

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

    fn sample(revision: u64, retracted: bool) -> AssertionRecordFull {
        AssertionRecordFull {
            key: RecordKey::new(RecordKind::Assertion, 9),
            revision: RecordRevision::new(revision).unwrap(),
            retracted,
            from: RecordKey::term(1),
            relation: RelationId::IsA,
            target: RecordKey::term(2),
            confidence: Confidence::Confirmed,
            declaration_order: 3,
            common: CommonFields {
                recorded_by: Some("tester".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: if retracted {
                    Some("retracted after review".to_owned())
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

    fn roundtrip(event: AssertionEvent, record: &AssertionRecordFull) -> AssertionRecordFull {
        let mut led = Open;
        let bytes = encode_with(&mut led, |w| write_assertion(w, event, record)).expect("encode");
        let mut reader = Reader::new(&bytes);
        let (decoded_event, decoded) = read_assertion(&mut reader, &mut led).expect("decode");
        assert_eq!(decoded_event, event);
        assert!(reader.is_empty());
        decoded
    }

    #[test]
    fn a_created_assertion_round_trips() {
        let record = sample(1, false);
        assert_eq!(roundtrip(AssertionEvent::Created, &record), record);
    }

    #[test]
    fn a_revised_assertion_carries_legacy_and_changes() {
        let mut record = sample(2, false);
        record.legacy_id = Some("legacy-a".to_owned());
        record.legacy_content_digest = Some([0x11; 32]);
        record.changed_fields = Some(vec![21]);
        record
            .extensions
            .insert("legacy.provider", b"x".to_vec())
            .unwrap();
        record.confidence = Confidence::Hypothetical;
        assert_eq!(roundtrip(AssertionEvent::Revised, &record), record);
    }

    #[test]
    fn a_retracted_assertion_round_trips() {
        let record = sample(2, true);
        let decoded = roundtrip(AssertionEvent::Retracted, &record);
        assert!(decoded.retracted);
    }

    /// Direction is authored data: a symmetric relation must not be normalised.
    #[test]
    fn symmetric_direction_is_preserved_not_normalised() {
        let mut record = sample(1, false);
        record.relation = RelationId::Contradicts;
        record.from = RecordKey::term(5);
        record.target = RecordKey::term(9);
        let decoded = roundtrip(AssertionEvent::Created, &record);
        assert_eq!(
            (decoded.from, decoded.target),
            (RecordKey::term(5), RecordKey::term(9)),
            "the stored assertion keeps the author's direction"
        );
    }

    #[test]
    fn two_assertions_may_share_endpoints_and_stay_distinct() {
        let a = sample(1, false);
        let mut b = sample(1, false);
        b.key = RecordKey::new(RecordKind::Assertion, 10);
        b.declaration_order = 4;
        assert_eq!(a.from, b.from);
        assert_eq!(a.target, b.target);
        assert_ne!(
            roundtrip(AssertionEvent::Created, &a),
            roundtrip(AssertionEvent::Created, &b),
            "separate identities must not collapse when endpoints match"
        );
    }

    #[test]
    fn status_must_match_the_event() {
        let mut led = Open;
        // Give the fixture its mandatory reason, so the only rule that can reject it is
        // the status contradiction this test is about.
        let mut active_but_retracted = sample(2, false);
        active_but_retracted.common.reason = Some("reviewed".to_owned());
        assert!(matches!(
            encode_with(&mut led, |w| write_assertion(
                w,
                AssertionEvent::Retracted,
                &active_but_retracted
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::StatusMismatch { .. }
            ))
        ));
        let retracted_but_created = sample(1, true);
        assert!(matches!(
            encode_with(&mut led, |w| write_assertion(
                w,
                AssertionEvent::Created,
                &retracted_but_created
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::StatusMismatch { .. }
            ))
        ));
    }

    #[test]
    fn non_term_endpoints_are_refused() {
        let mut led = Open;
        let mut record = sample(1, false);
        record.target = RecordKey::new(RecordKind::Claim, 4);
        assert!(matches!(
            encode_with(&mut led, |w| write_assertion(
                w,
                AssertionEvent::Created,
                &record
            )),
            Err(WriteError::Data(
                crate::wire::EncodeError::IdOutOfRange { .. }
            ))
        ));
    }

    #[test]
    fn a_missing_declaration_order_is_refused_on_decode() {
        let mut led = Open;
        let record = sample(1, false);
        let bytes = encode_with(&mut led, |w| {
            write_assertion(w, AssertionEvent::Created, &record)
        })
        .expect("encode");
        let mut reader = Reader::new(&bytes);
        let event = read_event(&mut reader, &mut led).expect("framing");
        let kept: Vec<crate::event::Field<'_>> = event
            .fields()
            .iter()
            .copied()
            .filter(|f| f.tag() != t::DECLARATION_ORDER)
            .collect();
        let mut rebuilt = Vec::new();
        {
            let mut w = Writer::new(&mut rebuilt, &mut led);
            crate::event::write_event(
                &mut w,
                event.tag(),
                crate::event::EVENT_FLAG_REQUIRED,
                &kept,
            )
            .expect("re-encode");
        }
        let mut reader = Reader::new(&rebuilt);
        assert!(matches!(
            read_assertion(&mut reader, &mut led),
            Err(ReadError::Data(DecodeError::MissingField { field: 3, .. }))
        ));
    }
}
