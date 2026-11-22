//! Current document-content seam, not an editable projection or a query phase claim.
#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;
use observing_fs::ObservingFs;
use sage_db_kernel::{
    auth::{AuthError, Capability, CapabilitySet, Grants},
    claims::CommandError,
    term_read::{TermRead, TermReadError, read_term},
};
use sage_db_store::{
    fs::CreateDir,
    handle::{HandleOutcome, Store},
    init::bootstrap,
    lease::{AttemptLedger, LeaseLedger},
    open::open_existing,
    view::{BlobReadError, RecordReadError},
};
use sage_db_store_tests::{
    faults::{FaultPoint, FaultSchedule, Injection},
    model::fs_model::{ModelCapabilities, ModelFileSystem},
};
use sage_db_types::{
    framing::{Limits, Refusal},
    persist::{RecordKey, RecordKind},
    query::{ControlStop, QueryControl, RunToCompletion},
};
use std::{path::Path, rc::Rc};
fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &RunToCompletion).unwrap()
}
fn grants(capability: Capability) -> Grants<'static> {
    Grants::new(
        "document-reader",
        fixture::kb(),
        CapabilitySet::of(&[capability]),
    )
}
fn world() -> (
    Rc<ModelFileSystem>,
    ObservingFs,
    Store<ObservingFs>,
    AttemptLedger<'static>,
) {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = ObservingFs::new(Rc::clone(&inner));
    let mut l = ledger();
    let mut store = bootstrap(
        fs.clone(),
        Path::new(fixture::ROOT),
        CreateDir::IfMissing,
        fixture::kb(),
        fixture::RULES,
        &Limits::V1,
        &mut l,
    )
    .unwrap();
    assert!(matches!(
        store.commit(fixture::carrier(2), &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 2, .. }
    ));
    (inner, fs, store, l)
}
#[test]
fn current_term_body_and_complete_record_survive_reopen_and_all_facades() {
    let (_inner, fs, mut store, mut setup) = world();
    let setup_counter = setup.reserve_owned(0).unwrap();
    let mut l = ledger();
    let body_counter = l.reserve_owned(0).unwrap();
    let before = fs.counts().borrow().total();
    let expected_basis = store.view().unwrap().basis();
    let TermRead::Found { record, body } = read_term(
        &mut store,
        &grants(Capability::Read),
        RecordKey::term(1),
        fixture::BYTES,
        &mut l,
    )
    .unwrap() else {
        panic!("present term")
    };
    assert_eq!(record.record(), &fixture::expected_history_a1());
    assert_eq!(body.as_bytes(), fixture::body(1, 1));
    assert_eq!(body.basis(), expected_basis);
    assert_eq!(fs.counts().borrow().total(), before);
    let record_bytes = record.reserved_bytes();
    let body_bytes = body.as_bytes().len() as u64;
    drop(store);
    drop(setup);
    drop(l);
    assert_eq!(setup_counter.stats().held_bytes, record_bytes);
    assert_eq!(setup_counter.stats().live_bytes, record_bytes);
    assert_eq!(body_counter.stats().held_bytes, body_bytes);
    assert_eq!(body_counter.stats().live_bytes, body_bytes);
    assert_eq!(record.record(), &fixture::expected_history_a1());
    assert_eq!(body.as_bytes(), fixture::body(1, 1));
    drop(record);
    drop(body);
    assert_eq!(setup_counter.stats().live_bytes, 0);
    assert_eq!(body_counter.stats().live_bytes, 0);
    let mut l = ledger();
    let mut reopened = open_existing(
        fs,
        Path::new(fixture::ROOT),
        Some(fixture::kb()),
        &Limits::V1,
        &mut l,
    )
    .unwrap()
    .into_store();
    let TermRead::Found { record, body } = read_term(
        &mut reopened,
        &grants(Capability::Read),
        RecordKey::term(1),
        fixture::BYTES,
        &mut l,
    )
    .unwrap() else {
        panic!("present after restart")
    };
    assert_eq!(record.record(), &fixture::expected_history_a1());
    assert_eq!(body.as_bytes(), fixture::body(1, 1));
    assert_eq!(body.basis(), expected_basis);
}
#[test]
fn read_permission_is_explicit_and_absence_is_basis_bound() {
    let (_inner, fs, mut store, _setup) = world();
    let mut l = ledger();
    let before = fs.counts().borrow().total();
    for capability in [
        Capability::Admin,
        Capability::WriteAux,
        Capability::Review,
        Capability::Propose,
    ] {
        assert!(matches!(
            read_term(
                &mut store,
                &grants(capability),
                RecordKey::term(1),
                fixture::BYTES,
                &mut l
            ),
            Err(TermReadError::Command(CommandError::Auth(
                AuthError::MissingCapability {
                    capability: Capability::Read
                }
            )))
        ));
    }
    let expected = store.view().unwrap().basis();
    assert!(
        matches!(read_term(&mut store,&grants(Capability::Read),RecordKey::term(99),fixture::BYTES,&mut l),Ok(TermRead::Absent{basis}) if basis==expected)
    );
    assert!(matches!(
        read_term(
            &mut store,
            &grants(Capability::Read),
            RecordKey::new(RecordKind::Term, u32::MAX as u128 + 1),
            fixture::BYTES,
            &mut l
        ),
        Err(TermReadError::Command(CommandError::Record(
            RecordReadError::InvalidKey(_)
        )))
    ));
    assert!(matches!(
        read_term(
            &mut store,
            &grants(Capability::Read),
            RecordKey::new(RecordKind::Claim, 1),
            fixture::BYTES,
            &mut l
        ),
        Err(TermReadError::Command(CommandError::Input(_)))
    ));
    assert_eq!(fs.counts().borrow().total(), before);
}
#[test]
fn body_ceiling_and_actual_io_effect_are_preserved() {
    use sage_db_store::{
        fs::{Effect, FsErrorKind},
        view::BlobReadStep,
    };
    let (inner, _fs, mut store, _setup) = world();
    let mut l = ledger();
    let len = fixture::body(1, 1).len() as u64;
    assert!(
        matches!(read_term(&mut store,&grants(Capability::Read),RecordKey::term(1),len-1,&mut l),Err(TermReadError::Command(CommandError::Blob(BlobReadError::Ceiling{size,ceiling}))) if size==len&&ceiling==len-1)
    );
    *inner.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::ReadExact, Injection::FailUnknown, 1);
    match read_term(
        &mut store,
        &grants(Capability::Read),
        RecordKey::term(1),
        len,
        &mut l,
    ) {
        Err(TermReadError::Command(CommandError::Blob(BlobReadError::Filesystem {
            step,
            error,
        }))) => {
            assert_eq!(step, BlobReadStep::Read);
            assert_eq!(error.kind(), FsErrorKind::Io(0));
            assert_eq!(error.effect(), Effect::Unknown);
        }
        other => panic!("preserve actual blob failure: {other:?}"),
    }
    inner.assert_all_faults_fired();
    assert!(matches!(
        read_term(
            &mut store,
            &grants(Capability::Read),
            RecordKey::term(1),
            len,
            &mut l
        ),
        Ok(TermRead::Found { .. })
    ));
}
struct Stop;
impl QueryControl for Stop {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        Err(ControlStop::Cancelled)
    }
}
#[test]
fn caller_stop_remains_distinct_from_real_byte_and_entry_limits() {
    use sage_db_types::ledger::BudgetUnit;
    let (_inner, _fs, mut store, _setup) = world();
    let mut stopped = AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &Stop).unwrap();
    assert!(matches!(
        read_term(
            &mut store,
            &grants(Capability::Read),
            RecordKey::term(1),
            fixture::BYTES,
            &mut stopped
        ),
        Err(TermReadError::Command(CommandError::Auth(
            AuthError::Refused(Refusal::Stopped(ControlStop::Cancelled))
        )))
    ));
    let mut entries = AttemptLedger::open(fixture::BYTES, 0, &RunToCompletion).unwrap();
    assert!(
        matches!(read_term(&mut store,&grants(Capability::Read),RecordKey::term(1),fixture::BYTES,&mut entries),Err(TermReadError::Command(CommandError::Record(RecordReadError::Refused(Refusal::Budget(e))))) if e.unit()==BudgetUnit::Entries)
    );
    let mut bytes = AttemptLedger::open(0, fixture::ENTRIES, &RunToCompletion).unwrap();
    assert!(
        matches!(read_term(&mut store,&grants(Capability::Read),RecordKey::term(1),fixture::BYTES,&mut bytes),Err(TermReadError::Command(CommandError::Blob(BlobReadError::Refused(Refusal::Budget(e))))) if e.unit()==BudgetUnit::Bytes)
    );
}

#[test]
fn unknown_authority_never_returns_old_content_or_confirmed_absence() {
    use sage_db_store::open::Resolution;
    let (inner, fs, mut store, _setup) = world();
    let old_pin = store.published_pin().unwrap();
    let mut l = ledger();
    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    assert!(matches!(
        store.commit(fixture::carrier(3), &Limits::V1, &mut l),
        HandleOutcome::Unknown
    ));
    inner.assert_all_faults_fired();
    assert!(store.authority_undecided());
    let before = fs.counts().borrow().total();
    for key in [RecordKey::term(1), RecordKey::term(99)] {
        assert!(matches!(
            read_term(
                &mut store,
                &grants(Capability::Read),
                key,
                fixture::BYTES,
                &mut l
            ),
            Err(TermReadError::Command(CommandError::Basis(
                sage_db_store::view::CommitReadError::Incomplete(_)
            )))
        ));
    }
    assert_eq!(fs.counts().borrow().total(), before);
    assert_eq!(old_pin.id().version.get(), 2);
    assert!(matches!(
        store.resolve(&Limits::V1, &mut l),
        Resolution::Ready
    ));
    let TermRead::Found { body, .. } = read_term(
        &mut store,
        &grants(Capability::Read),
        RecordKey::term(1),
        fixture::BYTES,
        &mut l,
    )
    .unwrap() else {
        panic!("resolved term")
    };
    assert_eq!(body.as_bytes(), fixture::body(1, 2));
    assert_eq!(body.basis().version.get(), 3);
}

#[test]
fn persisted_import_prepare_prevents_content_and_absence_reads_after_restart() {
    use sage_db_store::typed::{self, EventInput};
    use sage_db_types::{
        framing::FormatOutcome,
        persist::{
            ActorOrigin, CreationAudit, Extensions, IdSpace, ImportSessionRecord, ImportStage,
            RequestOutcomeEvent,
        },
        prepared::{BlobDeclaration, BlobSource, PreparedTransaction},
    };
    let (_inner, fs, mut store, _setup) = world();
    let mut l = ledger();
    let basis = store.commit_basis(IdSpace::TermId, &mut l).unwrap();
    let plan = b"test-only term-read import plan".to_vec();
    let plan_ref = sage_db_format::digest::raw_sha256(&plan);
    let result = typed::result_payload(3, &[], &[], &mut l).unwrap();
    let result_ref = sage_db_format::digest::raw_sha256(&result);
    let audit = CreationAudit {
        recorded_by: Some(fixture::PRINCIPAL.into()),
        actor_origin: ActorOrigin::Authenticated,
        reason: Some("physical unfinished-import fixture; not an importer".into()),
        created_at: 3,
    };
    let stage = ImportSessionRecord {
        import_session_id: "term-read-import".into(),
        stage: ImportStage::Prepare,
        source_digest: [0x66; 32],
        plan_ref,
        expected_counts: vec![],
        import_complete: false,
        audit: audit.clone(),
        extensions: Extensions::new(),
    };
    let outcome = RequestOutcomeEvent {
        principal_id: fixture::PRINCIPAL.into(),
        request_id: "term-read-stage".into(),
        payload_digest: [0x55; 32],
        result_ref,
        commit_version: 3,
        audit,
        extensions: Extensions::new(),
    };
    let events = [
        EventInput::ImportSession(&stage),
        EventInput::Outcome(&outcome),
    ]
    .into_iter()
    .map(|input| match typed::prepare_event(input, &mut l) {
        FormatOutcome::Ok(event) => event,
        other => panic!("canonical fixture event: {other:?}"),
    })
    .collect();
    let tx = PreparedTransaction {
        version: 3,
        prev_version: Some(2),
        tx_id: [0x31; 16],
        kb_uuid: fixture::kb(),
        created_at: 3,
        principal_id: fixture::PRINCIPAL.into(),
        request_id: "term-read-stage".into(),
        payload_digest: [0x55; 32],
        expect_version: Some(2),
        extensions: Extensions::new(),
        events,
        blobs: vec![
            BlobDeclaration {
                sha256: basis.rules.rules_ref,
                declared_len: basis.rules_len,
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
        ],
        rules: basis.rules,
    };
    assert!(matches!(
        store.commit(tx, &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 3, .. }
    ));
    drop(store);
    let mut reopened = open_existing(
        fs.clone(),
        Path::new(fixture::ROOT),
        Some(fixture::kb()),
        &Limits::V1,
        &mut l,
    )
    .unwrap()
    .into_store();
    let before = fs.counts().borrow().total();
    for key in [RecordKey::term(1), RecordKey::term(99)] {
        assert!(matches!(
            read_term(
                &mut reopened,
                &grants(Capability::Read),
                key,
                fixture::BYTES,
                &mut l
            ),
            Err(TermReadError::Command(CommandError::ImportInProgress))
        ));
    }
    assert_eq!(fs.counts().borrow().total(), before);
}

#[test]
fn retired_term_after_image_and_unchanged_body_remain_readable_after_restart() {
    use sage_db_store::{
        fold::FoldedRecord,
        typed::{self, EventInput},
    };
    use sage_db_types::{framing::FormatOutcome, identity::RecordRevision};
    let (_inner, fs, mut store, _setup) = world();
    let mut l = ledger();
    let FoldedRecord::Term(mut expected) = fixture::expected_history_a1() else {
        panic!("fixture must be Term");
    };
    expected.revision = RecordRevision::new(2).unwrap();
    expected.retired = true;
    expected.common.updated_at = Some(99);
    let event = match typed::prepare_event(
        EventInput::Term(typed::TermEvent::Retired, &expected),
        &mut l,
    ) {
        FormatOutcome::Ok(event) => event,
        other => panic!("canonical retirement: {other:?}"),
    };
    // Use the public fixture carrier for its real outcome/rules/blob declarations.
    // Replace only A's event: retirement retains its committed original body reference.
    let mut tx = fixture::carrier(3);
    tx.events[0] = event;
    tx.blobs.push(sage_db_types::prepared::BlobDeclaration {
        sha256: sage_db_format::digest::raw_sha256(&fixture::body(1, 1)),
        declared_len: fixture::body(1, 1).len() as u64,
        source: sage_db_types::prepared::BlobSource::Existing,
    });
    tx.blobs
        .retain(|blob| blob.sha256 != sage_db_format::digest::raw_sha256(&fixture::body(1, 2)));
    assert!(matches!(
        store.commit(tx, &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 3, .. }
    ));
    let expected_basis = store.view().unwrap().basis();
    drop(store);
    let mut reopened = open_existing(
        fs,
        Path::new(fixture::ROOT),
        Some(fixture::kb()),
        &Limits::V1,
        &mut l,
    )
    .unwrap()
    .into_store();
    let TermRead::Found { record, body } = read_term(
        &mut reopened,
        &grants(Capability::Read),
        RecordKey::term(1),
        fixture::BYTES,
        &mut l,
    )
    .unwrap() else {
        panic!("retired after-image remains present");
    };
    assert_eq!(record.record(), &FoldedRecord::Term(expected));
    assert_eq!(body.as_bytes(), fixture::body(1, 1));
    assert_eq!(body.basis(), expected_basis);
}
