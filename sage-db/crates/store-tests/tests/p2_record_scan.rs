//! Actual numeric record traversal: one durable basis and the caller's ledger.
//! Model fixtures and named mutation counters are not platform/allocator evidence.
#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;
use core::convert::Infallible;
use fixture::{BYTES, ENTRIES, ROOT, RULES, carrier, kb};
use observing_fs::ObservingFs;
use sage_db_store::{
    CreateDir,
    fold::FoldedRecord,
    handle::{HandleOutcome, Store},
    init::bootstrap,
    lease::AttemptLedger,
    open::OutcomeIncomplete,
    view::{RecordReadError, RecordScanError},
};
use sage_db_store_tests::{
    faults::{FaultPoint, FaultSchedule, Injection},
    model::fs_model::{ModelCapabilities, ModelFileSystem},
};
use sage_db_types::{
    framing::{Limits, Refusal},
    ledger::BudgetUnit,
    persist::RecordKey,
    query::{ControlStop, QueryControl, RunToCompletion},
};
use std::{path::Path, rc::Rc};
fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap()
}
fn setup() -> (Rc<ModelFileSystem>, ObservingFs, Store<ObservingFs>) {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = ObservingFs::new(Rc::clone(&inner));
    let mut l = ledger();
    let mut store = bootstrap(
        fs.clone(),
        Path::new(ROOT),
        CreateDir::IfMissing,
        kb(),
        RULES,
        &Limits::V1,
        &mut l,
    )
    .unwrap();
    assert!(matches!(
        store.commit(carrier(2), &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 2, .. }
    ));
    (inner, fs, store)
}
#[test]
fn numeric_scan_returns_complete_current_records_on_the_actual_basis() {
    let (_inner, fs, store) = setup();
    let mut l = ledger();
    let mut records = Vec::new();
    let before = fs.counts().borrow().total();
    let basis = store
        .scan_identified_records(&mut l, |record, _| {
            records.push(record);
            Ok::<_, Infallible>(())
        })
        .unwrap();
    assert_eq!(basis, store.view().unwrap().basis());
    assert_eq!(basis.version.get(), 2);
    assert_eq!(records.len(), 2);
    for (id, record) in [1, 2].into_iter().zip(records) {
        let FoldedRecord::Term(term) = record else {
            panic!("only two identified Term rows expected")
        };
        assert_eq!(term.key, RecordKey::term(id));
        assert_eq!(
            term.body_ref,
            sage_db_format::digest::raw_sha256(&fixture::body(id.into(), 1))
        );
        assert_eq!(term.atom, "process fixture atom");
        assert!(!term.retired);
    }
    assert_eq!(
        fs.counts().borrow().total(),
        before,
        "no named mutations/syncs"
    );
    assert_eq!(
        l.stats().live_bytes,
        0,
        "scanner allocated no caller-admitted table"
    );
}
struct Stop;
impl QueryControl for Stop {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        Err(ControlStop::Cancelled)
    }
}
#[test]
fn numeric_scan_preserves_real_budget_stop_and_visitor_errors() {
    let (_inner, _fs, store) = setup();
    let mut calls = 0;
    let mut tiny = AttemptLedger::open(BYTES, 0, &RunToCompletion).unwrap();
    let e = store
        .scan_identified_records(&mut tiny, |_, _| {
            calls += 1;
            Ok::<_, ()>(())
        })
        .unwrap_err();
    assert!(
        matches!(e,RecordScanError::Read(RecordReadError::Refused(Refusal::Budget(b))) if b.unit()==BudgetUnit::Entries && b.requested()==1 && b.remaining()==0)
    );
    assert_eq!(calls, 0);
    let control = Stop;
    let mut stopped = AttemptLedger::open(BYTES, ENTRIES, &control).unwrap();
    assert!(matches!(
        store.scan_identified_records(&mut stopped, |_, _| Ok::<_, ()>(())),
        Err(RecordScanError::Read(RecordReadError::Refused(
            Refusal::Stopped(ControlStop::Cancelled)
        )))
    ));
    let mut l = ledger();
    let mut visits = 0;
    let error = store
        .scan_identified_records(&mut l, |_, _| {
            visits += 1;
            Err::<(), _>("visitor-refused")
        })
        .unwrap_err();
    assert!(matches!(error, RecordScanError::Visitor("visitor-refused")));
    assert_eq!(visits, 1, "no callback after the visitor's refusal");
    assert_eq!(store.head(), Some(2));
}
#[test]
fn numeric_scan_never_serves_an_old_fold_when_authority_is_undecided() {
    let (inner, _fs, mut store) = setup();
    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    let mut l = ledger();
    assert!(matches!(
        store.commit(carrier(3), &Limits::V1, &mut l),
        HandleOutcome::Unknown
    ));
    assert!(store.authority_undecided());
    let mut calls = 0;
    let result = store.scan_identified_records(&mut l, |_, _| {
        calls += 1;
        Ok::<_, ()>(())
    });
    assert!(matches!(
        result,
        Err(RecordScanError::Incomplete(
            OutcomeIncomplete::NoCurrentView
        ))
    ));
    assert_eq!(calls, 0);
}

#[test]
fn owned_and_borrowed_current_reads_agree_and_absence_is_basis_bound() {
    use sage_db_store::view::{RecordImageLookup, RecordLookup};
    use sage_db_types::persist::RecordKind;
    let (_inner, _fs, store) = setup();
    let mut l = ledger();
    let RecordLookup::Found {
        record: borrowed,
        basis: borrowed_basis,
    } = store.record(RecordKey::term(1), &mut l).unwrap()
    else {
        panic!("exists")
    };
    let RecordImageLookup::Found { image, basis } =
        store.record_owned(RecordKey::term(1), &mut l).unwrap()
    else {
        panic!("owned exists")
    };
    assert_eq!(basis, borrowed_basis);
    assert_eq!(image.record(), borrowed);
    assert!(
        matches!(store.record_owned(RecordKey::term(999),&mut l).unwrap(),RecordImageLookup::Absent{basis:b} if b==basis)
    );
    let bad = RecordKey::new(RecordKind::Term, u128::from(u32::MAX) + 1);
    assert!(matches!(store.record_owned(bad,&mut l),Err(RecordReadError::InvalidKey(k)) if k==bad));
}

#[test]
fn identified_scan_includes_a_retired_current_after_image() {
    use sage_db_format::{
        digest::raw_sha256,
        event::{encode_with, read_event},
        term::{TermEvent, write_term},
        wire::Reader,
    };
    use sage_db_store::view::RecordLookup;
    use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent};
    let (_inner, _fs, mut store) = setup();
    let mut l = ledger();
    let mut term = match store.record(RecordKey::term(1), &mut l).unwrap() {
        RecordLookup::Found {
            record: FoldedRecord::Term(t),
            ..
        } => t.clone(),
        other => panic!("term: {other:?}"),
    };
    term.revision = term.revision.checked_next().unwrap();
    term.retired = true;
    term.common.updated_at = Some(99);
    let bytes = encode_with(&mut l, |w| write_term(w, TermEvent::Retired, &term)).unwrap();
    let decoded = read_event(&mut Reader::new(&bytes), &mut l).unwrap();
    let event = OwnedEvent {
        tag: decoded.tag(),
        fields: decoded
            .fields()
            .iter()
            .map(|f| (f.tag(), f.value().to_vec()))
            .collect(),
    };
    let mut tx = carrier(3);
    tx.events = vec![event, fixture::outcome(3)];
    let old_body = fixture::body(1, 1);
    tx.blobs.push(BlobDeclaration {
        sha256: raw_sha256(&old_body),
        declared_len: old_body.len() as u64,
        source: BlobSource::Existing,
    });
    assert!(matches!(
        store.commit(tx, &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 3, .. }
    ));
    let mut rows = Vec::new();
    store
        .scan_identified_records(&mut l, |record, _| {
            let FoldedRecord::Term(t) = record else {
                panic!("only numeric Term rows")
            };
            rows.push((t.key, t.retired, t.revision.get()));
            Ok::<_, Infallible>(())
        })
        .unwrap();
    assert_eq!(
        rows,
        [
            (RecordKey::term(1), true, 2),
            (RecordKey::term(2), false, 1)
        ]
    );
}
