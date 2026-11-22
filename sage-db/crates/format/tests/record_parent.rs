//! Independent schema boundaries for the first two record families.
use sage_db_format::{
    Reader,
    assertion::{AssertionEvent, read_assertion, write_assertion},
    event::{Field, encode_with, read_event, write_event},
};
use sage_db_types::{
    Confidence, RecordRevision, RelationId,
    ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger},
    persist::{ActorOrigin, AssertionRecordFull, CommonFields, Extensions, RecordKey, RecordKind},
};
struct L(bool);
impl SharedLedger for L {
    fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, n: u64) -> Result<(), BudgetError> {
        if self.0 {
            Ok(())
        } else {
            Err(BudgetError::new(BudgetUnit::Entries, n, 0))
        }
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}
fn sample() -> AssertionRecordFull {
    AssertionRecordFull {
        key: RecordKey::new(RecordKind::Assertion, 7),
        revision: RecordRevision::FIRST,
        retracted: false,
        from: RecordKey::term(1),
        target: RecordKey::term(2),
        relation: RelationId::ALL[0],
        confidence: Confidence::Confirmed,
        declaration_order: 0,
        common: CommonFields {
            recorded_by: None,
            actor_origin: ActorOrigin::Unavailable,
            reason: None,
            created_at: 0,
            updated_at: None,
        },
        legacy_id: None,
        legacy_content_digest: None,
        extensions: Extensions::new(),
        changed_fields: None,
    }
}
#[test]
fn revised_writer_requires_updated_at() {
    let mut r = sample();
    r.revision = RecordRevision::new(2).unwrap();
    assert!(
        encode_with(&mut L(true), |w| write_assertion(
            w,
            AssertionEvent::Revised,
            &r
        ))
        .is_err()
    );
}
#[test]
fn retraction_requires_reason() {
    let mut r = sample();
    r.revision = RecordRevision::new(2).unwrap();
    r.retracted = true;
    r.common.updated_at = Some(1);
    assert!(
        encode_with(&mut L(true), |w| write_assertion(
            w,
            AssertionEvent::Retracted,
            &r
        ))
        .is_err()
    );
}
#[test]
fn revised_cannot_use_first_revision() {
    let mut r = sample();
    r.common.updated_at = Some(1);
    assert!(
        encode_with(&mut L(true), |w| write_assertion(
            w,
            AssertionEvent::Revised,
            &r
        ))
        .is_err()
    );
}
#[test]
fn record_field_assembly_needs_entry_budget() {
    assert!(
        encode_with(&mut L(false), |w| write_assertion(
            w,
            AssertionEvent::Created,
            &sample()
        ))
        .is_err()
    );
}
#[test]
fn known_foreign_field_is_not_silently_dropped() {
    let b = encode_with(&mut L(true), |w| {
        write_assertion(w, AssertionEvent::Created, &sample())
    })
    .unwrap();
    let e = read_event(&mut Reader::new(&b), &mut L(true)).unwrap();
    let mut fields = e.fields().to_vec();
    fields.push(Field::new(64, &[0, 0]));
    let b = encode_with(&mut L(true), |w| write_event(w, 16, 1, &fields)).unwrap();
    assert!(
        read_assertion(&mut Reader::new(&b), &mut L(true)).is_err(),
        "foreign field accepted then omitted from record"
    );
}
