//! Preparation of the record families needed by the administrative import path.
//! The accepted P1 binary/hex vectors are the expected field bytes for five
//! cases; the two authored full-record cases prove projection agreement only.
use sage_db_format::{
    dispatch::{DispatchedEvent, read_dispatched},
    event::read_event,
    wire::Reader,
};
use sage_db_store::{
    lease::AttemptLedger,
    typed::{self, AssertionEvent, DispatchedPayload, EventInput, TermEvent},
};
use sage_db_types::{
    Confidence, RecordRevision, RelationId,
    framing::{FormatOutcome, Refusal},
    persist::*,
    prepared::OwnedEvent,
    query::RunToCompletion,
};

fn ledger() -> AttemptLedger<'static> {
    static CONTROL: RunToCompletion = RunToCompletion;
    AttemptLedger::open(32 * 1024 * 1024, 100_000, &CONTROL).unwrap()
}
fn decode(bytes: &[u8]) -> DispatchedPayload {
    let mut l = ledger();
    match read_dispatched(&mut Reader::new(bytes), &mut l) {
        FormatOutcome::Ok(DispatchedEvent::Record { payload, .. }) => payload,
        other => panic!("accepted vector decode: {other:?}"),
    }
}
fn unhex(s: &str) -> Vec<u8> {
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    assert_eq!(compact.len() % 2, 0);
    (0..compact.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&compact[i..i + 2], 16).unwrap())
        .collect()
}
fn input(payload: &DispatchedPayload, tag: u16) -> EventInput<'_> {
    match payload {
        DispatchedPayload::Term(r) => EventInput::Term(
            if tag == 1 {
                TermEvent::Created
            } else {
                TermEvent::Revised
            },
            r,
        ),
        DispatchedPayload::Assertion(r) => EventInput::Assertion(AssertionEvent::Created, r),
        DispatchedPayload::Asset(r) => EventInput::Asset(r),
        DispatchedPayload::FrontierEntry(r) => EventInput::Frontier(r),
        DispatchedPayload::KbMetadata(r) => EventInput::KbMetadata(r),
        DispatchedPayload::ImportBookkeeping(r) => EventInput::ImportSession(r),
        other => panic!("not one of the new bridge families: {other:?}"),
    }
}
fn prepared(payload: &DispatchedPayload, tag: u16) -> OwnedEvent {
    let mut l = ledger();
    let e = match typed::prepare_event(input(payload, tag), &mut l) {
        FormatOutcome::Ok(event) => event,
        other => panic!("prepare tag {tag}: {other:?}"),
    };
    assert_eq!(e.tag, tag);
    assert_eq!(
        typed::copy_payload(input(payload, tag), &mut l),
        FormatOutcome::Ok(payload.clone())
    );
    e
}
#[test]
fn accepted_term_asset_metadata_and_import_vectors_keep_every_raw_field() {
    let fixtures: Vec<(u16, Vec<u8>)> = vec![
        (
            1,
            include_bytes!("../../format/tests/fixtures/record-projection/v2.e0.tag1.bin").to_vec(),
        ),
        (
            2,
            include_bytes!("../../format/tests/fixtures/record-projection/v3.e0.tag2.bin").to_vec(),
        ),
        (
            64,
            unhex(include_str!(
                "../../format/tests/fixtures/natural-structural/asset-full.hex"
            )),
        ),
        (
            144,
            unhex(include_str!(
                "../../format/tests/fixtures/natural-structural/kbmeta-full.hex"
            )),
        ),
        (
            208,
            unhex(include_str!(
                "../../format/tests/fixtures/natural-structural/import-full.hex"
            )),
        ),
    ];
    for (tag, bytes) in fixtures {
        let payload = decode(&bytes);
        let got = prepared(&payload, tag);
        let mut l = ledger();
        let expected = read_event(&mut Reader::new(&bytes), &mut l).unwrap();
        assert_eq!(got.fields.len(), expected.fields().len());
        for ((got_tag, value), expected) in got.fields.iter().zip(expected.fields()) {
            assert_eq!(*got_tag, expected.tag());
            assert_eq!(value.as_slice(), expected.value());
        }
    }
}
fn common() -> CommonFields {
    CommonFields {
        recorded_by: None,
        actor_origin: ActorOrigin::Unavailable,
        reason: Some("imported source; original actor unavailable".into()),
        created_at: 123,
        updated_at: None,
    }
}
#[test]
fn authored_assertion_and_frontier_keep_full_audit_extensions_and_optional_fields() {
    let mut extensions = Extensions::new();
    extensions.insert("import.source", vec![0, 255, 1]).unwrap();
    let assertion = DispatchedPayload::Assertion(AssertionRecordFull {
        key: RecordKey::new(RecordKind::Assertion, 11),
        revision: RecordRevision::FIRST,
        retracted: false,
        from: RecordKey::term(1),
        relation: RelationId::DependsOn,
        target: RecordKey::term(2),
        confidence: Confidence::Contested,
        declaration_order: 17,
        common: common(),
        legacy_id: Some("source:17".into()),
        legacy_content_digest: Some([4; 32]),
        extensions: extensions.clone(),
        changed_fields: None,
    });
    let frontier = DispatchedPayload::FrontierEntry(FrontierEntryRecord {
        key: RecordKey::new(RecordKind::FrontierEntry, 9),
        revision: RecordRevision::FIRST,
        status: FrontierStatus::Open,
        concept_refs: vec![RecordKey::term(1), RecordKey::term(2)],
        text_ref: [5; 32],
        priority: 2,
        common: common(),
        extensions,
        changed_fields: None,
    });
    prepared(&assertion, 16);
    prepared(&frontier, 112);
}
#[test]
fn caller_stop_and_real_budget_stay_distinct_for_a_new_bridge_family() {
    struct Stopped;
    impl sage_db_types::QueryControl for Stopped {
        fn checkpoint(&self) -> Result<(), sage_db_types::ControlStop> {
            Err(sage_db_types::ControlStop::Cancelled)
        }
    }
    let payload = decode(&unhex(include_str!(
        "../../format/tests/fixtures/natural-structural/import-full.hex"
    )));
    let stopped = Stopped;
    let mut l = AttemptLedger::open(1_000_000, 1000, &stopped).unwrap();
    assert!(matches!(
        typed::prepare_event(input(&payload, 208), &mut l),
        FormatOutcome::Refused(Refusal::Stopped(_))
    ));
    let control = RunToCompletion;
    let mut l = AttemptLedger::open(0, 1000, &control).unwrap();
    assert!(matches!(
        typed::prepare_event(input(&payload, 208), &mut l),
        FormatOutcome::Refused(Refusal::Budget(_))
    ));
}
#[test]
fn event_revision_mismatch_is_not_laundered_into_allocation_or_budget() {
    let payload = decode(include_bytes!(
        "../../format/tests/fixtures/record-projection/v2.e0.tag1.bin"
    ));
    let DispatchedPayload::Term(record) = payload else {
        unreachable!()
    };
    let mut l = ledger();
    assert!(matches!(
        typed::prepare_event(EventInput::Term(TermEvent::Revised, &record), &mut l),
        FormatOutcome::Format(_)
    ));
}
