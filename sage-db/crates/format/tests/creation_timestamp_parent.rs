//! An accepted encode must not produce an image its own record decoder rejects.
use sage_db_format::{
    collection::*,
    event::encode_with,
    wire::{EncodeError, Reader, WriteError},
};
use sage_db_types::{
    identity::RecordRevision as R,
    ledger::{BudgetError, SharedLedger},
    persist::*,
    query::ControlStop,
};
struct L;
impl SharedLedger for L {
    fn try_charge(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}
fn common() -> CommonFields {
    CommonFields {
        recorded_by: None,
        actor_origin: ActorOrigin::Unavailable,
        reason: None,
        created_at: 1,
        updated_at: Some(2),
    }
}
#[test]
fn first_upsert_never_emits_a_self_rejected_update_time() {
    let record = VectorRecord {
        key: RecordKey::new(RecordKind::Vector, 1),
        revision: R::FIRST,
        status: VectorStatus::Active,
        subject: RecordKey::term(1),
        subject_revision: R::FIRST,
        model_id: "m".into(),
        dimensions: 1,
        input_digest: [1; 32],
        values_ref: [2; 32],
        embedding_revision: 1,
        common: common(),
        extensions: Extensions::new(),
        changed_fields: None,
    };
    match encode_with(&mut L, |w| write_vector(w, false, &record)) {
        Ok(bytes) => {
            read_vector(&mut Reader::new(&bytes), &mut L)
                .expect("successful encoder emitted a self-rejected first upsert");
        }
        Err(WriteError::Data(EncodeError::ForbiddenField { field: 9, .. })) => {}
        other => panic!("unexpected refusal: {other:?}"),
    }
}
#[test]
fn submission_never_emits_a_self_rejected_update_time() {
    let record = ProposalRecord {
        key: RecordKey::new(RecordKind::Proposal, 1),
        revision: R::FIRST,
        status: ProposalStatus::Pending,
        kind: ProposalKind::ClaimEdit,
        subject: RecordKey::term(1),
        base_revisions: vec![RevPair {
            key: RecordKey::term(1),
            revision: R::FIRST,
        }],
        payload_ref: [1; 32],
        review_note: None,
        declared_author: "a".into(),
        common: common(),
        legacy_id: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    match encode_with(&mut L, |w| write_proposal(w, false, &record)) {
        Ok(bytes) => {
            read_proposal(&mut Reader::new(&bytes), &mut L)
                .expect("successful encoder emitted a self-rejected submission");
        }
        Err(WriteError::Data(EncodeError::ForbiddenField { field: 9, .. })) => {}
        other => panic!("unexpected refusal: {other:?}"),
    }
}
