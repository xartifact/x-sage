//! The seam must reject exactly what the family reader rejects.
//!
//! Source of the counterexample: coordinator probe
//! `parallel-01/probes/shared-boundary-v2`, which caught the seam returning
//! `Ok(Validated{Term, event})` for a `TermCreated` carrying only `17 atom`.
use sage_db_format::{
    dispatch::{CodecFamily, DispatchedEvent, read_dispatched},
    event::encode_with,
    term::{TermEvent, write_term},
    wire::Reader,
};
use sage_db_types::{
    confidence::Confidence,
    framing::{CorruptReason, FormatError, FormatOutcome},
    identity::RecordRevision,
    layer::SemanticLayer,
    ledger::{BudgetError, SharedLedger},
    persist::{ActorOrigin, CommonFields, Extensions, RecordKey, TermRecord, TermSource},
    query::ControlStop,
};

struct Open;
impl SharedLedger for Open {
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

/// One event carrying a single field, exactly as the probe built it.
fn single_field_event(tag: u16, field_tag: u16, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&field_tag.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value);
    out
}

#[test]
fn a_partial_registered_record_is_refused_by_the_seam() {
    // Tag 1 TermCreated, only field 17 (atom). The real reader requires the whole
    // after-image, so a namespace-only check is not enough.
    let bytes = single_field_event(1, 17, &[1, 0, 0, 0, b'x']);
    let outcome = read_dispatched(&mut Reader::new(&bytes), &mut Open);
    match outcome {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::MissingField {
            event,
            field,
        })) => {
            assert_eq!(event, 1);
            assert_ne!(
                field, 17,
                "the missing field is one the partial image omitted"
            );
        }
        other => panic!("seam accepted a partial record: {other:?}"),
    }
}

#[test]
fn the_seam_and_the_family_reader_agree_on_a_complete_record() {
    let bytes =
        encode_with(&mut Open, |w| write_term(w, TermEvent::Created, &term())).expect("encode");
    let outcome = read_dispatched(&mut Reader::new(&bytes), &mut Open);
    match outcome {
        FormatOutcome::Ok(DispatchedEvent::Record { family, .. }) => {
            assert_eq!(family, CodecFamily::Term)
        }
        other => panic!("complete record refused: {other:?}"),
    }
    // The family reader accepts the same bytes; the two paths must not disagree.
    assert!(sage_db_format::term::read_term(&mut Reader::new(&bytes), &mut Open).is_ok());
}
