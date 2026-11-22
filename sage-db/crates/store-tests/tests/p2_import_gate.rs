//! An unfinished persisted import fences ordinary kernel operations after restart.
//! Physical stage fixtures below do not constitute an administrative importer.
#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;
use sage_db_kernel::{
    auth::{Capability, CapabilitySet, Grants},
    claims::{CommandError, RequestMeta},
    session::{self, SessionError, SessionRead},
    session_intent::SessionState,
};
use sage_db_store::{
    fs::CreateDir,
    handle::{HandleOutcome, Store},
    init::bootstrap,
    lease::AttemptLedger,
    open::open_existing,
    typed::{self, EventInput},
    view::{CommitReadError, ImportReadiness},
};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::{
    RecordRevision,
    framing::{FormatOutcome, Limits, Refusal},
    persist::*,
    prepared::{BlobDeclaration, BlobSource, PreparedTransaction},
    query::{ControlStop, QueryControl, RunToCompletion},
};
use std::{path::Path, rc::Rc};
fn ledger() -> AttemptLedger<'static> {
    static C: RunToCompletion = RunToCompletion;
    AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &C).unwrap()
}
fn seed(
    fs: observing_fs::ObservingFs,
    l: &mut AttemptLedger<'_>,
) -> Store<observing_fs::ObservingFs> {
    let mut s = bootstrap(
        fs,
        Path::new(fixture::ROOT),
        CreateDir::IfMissing,
        fixture::kb(),
        fixture::RULES,
        &Limits::V1,
        l,
    )
    .unwrap();
    assert!(matches!(
        s.commit(fixture::carrier(2), &Limits::V1, l),
        HandleOutcome::Committed { version: 2, .. }
    ));
    s
}
fn mark(version: u64, complete: bool, l: &mut AttemptLedger<'_>) -> PreparedTransaction {
    let plan = b"test-only import plan".to_vec();
    let result = b"test-only stage result".to_vec();
    let hash = typed::raw_hash(&plan, l);
    let FormatOutcome::Ok(plan_ref) = hash else {
        panic!("plan hash")
    };
    let FormatOutcome::Ok(result_ref) = typed::raw_hash(&result, l) else {
        panic!("result hash")
    };
    let audit = CreationAudit {
        recorded_by: Some(fixture::PRINCIPAL.into()),
        actor_origin: ActorOrigin::Authenticated,
        reason: Some("physical import gate fixture, not a verified real import".into()),
        created_at: version as i64,
    };
    let record = ImportSessionRecord {
        import_session_id: "gate-test".into(),
        stage: if complete {
            ImportStage::Activate
        } else {
            ImportStage::Prepare
        },
        source_digest: [77; 32],
        plan_ref,
        expected_counts: vec![],
        import_complete: complete,
        audit: audit.clone(),
        extensions: Extensions::new(),
    };
    let outcome = RequestOutcomeEvent {
        principal_id: fixture::PRINCIPAL.into(),
        request_id: format!("stage-{version}"),
        payload_digest: [version as u8; 32],
        result_ref,
        commit_version: version,
        audit,
        extensions: Extensions::new(),
    };
    let FormatOutcome::Ok(event) = typed::prepare_event(EventInput::ImportSession(&record), l)
    else {
        panic!("stage")
    };
    let FormatOutcome::Ok(outcome) = typed::prepare_event(EventInput::Outcome(&outcome), l) else {
        panic!("outcome")
    };
    let mut tx = fixture::carrier(3);
    tx.version = version;
    tx.prev_version = Some(version - 1);
    tx.expect_version = Some(version - 1);
    tx.tx_id = [version as u8; 16];
    tx.request_id = format!("stage-{version}");
    tx.payload_digest = [version as u8; 32];
    tx.created_at = version as i64;
    tx.events = vec![event, outcome];
    tx.blobs = vec![
        BlobDeclaration {
            sha256: tx.rules.rules_ref,
            declared_len: fixture::RULES.len() as u64,
            source: BlobSource::Existing,
        },
        BlobDeclaration {
            sha256: plan_ref,
            declared_len: plan.len() as u64,
            source: BlobSource::Provide(plan),
        },
        BlobDeclaration {
            sha256: result_ref,
            declared_len: result.len() as u64,
            source: BlobSource::Provide(result),
        },
    ];
    tx
}
fn commit_mark(
    s: &mut Store<observing_fs::ObservingFs>,
    version: u64,
    complete: bool,
    l: &mut AttemptLedger<'_>,
) {
    let tx = mark(version, complete, l);
    match s.commit(tx, &Limits::V1, l) {
        HandleOutcome::Committed { version: got, .. } => assert_eq!(got, version),
        other => panic!("physical import stage fixture {version}: {other:?}"),
    }
}
fn grants() -> Grants<'static> {
    Grants::new(
        "gate-user",
        fixture::kb(),
        CapabilitySet::of(&[Capability::Read, Capability::WriteAux]),
    )
}
fn state() -> SessionState<'static> {
    SessionState {
        concept: RecordKey::term(1),
        session_no: 1,
        content: "gated",
    }
}
fn meta() -> RequestMeta<'static> {
    RequestMeta {
        request_id: "session-gate",
        tx_id: [91; 16],
        created_at: 91,
    }
}
#[test]
fn ordinary_reads_and_new_writes_refuse_until_persisted_completion_even_after_reopen() {
    let fs =
        observing_fs::ObservingFs::new(Rc::new(ModelFileSystem::new(ModelCapabilities::full())));
    let counts = fs.counts();
    let mut l = ledger();
    let mut s = seed(fs.clone(), &mut l);
    assert!(matches!(
        s.import_readiness(&mut l),
        Ok(ImportReadiness::Ready { .. })
    ));
    commit_mark(&mut s, 3, false, &mut l);
    let before = counts.borrow().total();
    let g = grants();
    assert!(matches!(
        session::read_session_checkpoint(
            &mut s,
            &g,
            RecordKey::new(RecordKind::SessionCheckpoint, 1),
            1024,
            &mut l
        ),
        Err(SessionError::Command(CommandError::ImportInProgress))
    ));
    let read_set = [RevPair {
        key: RecordKey::term(1),
        revision: RecordRevision::FIRST,
    }];
    assert!(matches!(
        session::create_session_checkpoint(
            &mut s,
            &g,
            meta(),
            state(),
            &read_set,
            &Limits::V1,
            &mut l
        ),
        Err(SessionError::Command(CommandError::ImportInProgress))
    ));
    assert_eq!(counts.borrow().total(), before);
    drop(s);
    let mut l = ledger();
    let mut s = open_existing(
        fs,
        Path::new(fixture::ROOT),
        Some(fixture::kb()),
        &Limits::V1,
        &mut l,
    )
    .unwrap()
    .into_store();
    match s.import_readiness(&mut l).unwrap() {
        ImportReadiness::Incomplete { session, basis } => {
            assert_eq!(session.import_session_id, "gate-test");
            assert_eq!(session.stage, ImportStage::Prepare);
            assert_eq!(basis.version.get(), 3);
        }
        other => panic!("lost persisted gate: {other:?}"),
    }
    assert!(matches!(
        session::read_session_checkpoint(
            &mut s,
            &g,
            RecordKey::new(RecordKind::SessionCheckpoint, 1),
            1024,
            &mut l
        ),
        Err(SessionError::Command(CommandError::ImportInProgress))
    ));
    commit_mark(&mut s, 4, true, &mut l);
    assert!(matches!(
        session::read_session_checkpoint(
            &mut s,
            &g,
            RecordKey::new(RecordKind::SessionCheckpoint, 1),
            1024,
            &mut l
        ),
        Ok(SessionRead::Absent { .. })
    ));
    assert!(matches!(
        session::create_session_checkpoint(
            &mut s,
            &g,
            meta(),
            state(),
            &read_set,
            &Limits::V1,
            &mut l
        ),
        Ok(HandleOutcome::Committed { version: 5, .. })
    ));
}
#[test]
fn exact_retry_precedes_the_import_gate_and_performs_no_named_mutations() {
    let fs =
        observing_fs::ObservingFs::new(Rc::new(ModelFileSystem::new(ModelCapabilities::full())));
    let counts = fs.counts();
    let mut l = ledger();
    let mut s = seed(fs, &mut l);
    let g = grants();
    let rs = [RevPair {
        key: RecordKey::term(1),
        revision: RecordRevision::FIRST,
    }];
    assert!(matches!(
        session::create_session_checkpoint(&mut s, &g, meta(), state(), &rs, &Limits::V1, &mut l),
        Ok(HandleOutcome::Committed { version: 3, .. })
    ));
    commit_mark(&mut s, 4, false, &mut l);
    let before = counts.borrow().total();
    assert!(matches!(
        session::create_session_checkpoint(&mut s, &g, meta(), state(), &rs, &Limits::V1, &mut l),
        Ok(HandleOutcome::AlreadyCommitted { version: 3, .. })
    ));
    assert_eq!(counts.borrow().total(), before);
}
#[test]
fn import_metadata_work_refusal_keeps_the_real_ledger_class() {
    let fs =
        observing_fs::ObservingFs::new(Rc::new(ModelFileSystem::new(ModelCapabilities::full())));
    let mut l = ledger();
    let mut s = seed(fs, &mut l);
    commit_mark(&mut s, 3, false, &mut l);
    let control = RunToCompletion;
    let mut tiny = AttemptLedger::open(1000, 0, &control).unwrap();
    assert!(
        matches!(s.import_readiness(&mut tiny),Err(CommitReadError::Refused(Refusal::Budget(e)))if e.unit()==sage_db_types::BudgetUnit::Entries&&e.requested()==1&&e.remaining()==0)
    );
    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let stop = Stop;
    let mut stopped = AttemptLedger::open(1000, 1000, &stop).unwrap();
    assert!(matches!(
        s.import_readiness(&mut stopped),
        Err(CommitReadError::Refused(Refusal::Stopped(
            ControlStop::Cancelled
        )))
    ));
}
