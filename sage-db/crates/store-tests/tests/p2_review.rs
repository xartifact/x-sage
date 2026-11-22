//! P2 read-only admission integration on a real public Store and model backend.
//! Raw prepared events seed proposals through the P1 boundary; no business
//! submission/review writer, payload planner or platform durability is claimed.

#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::path::Path;
use std::rc::Rc;

use fixture::{BYTES, ENTRIES, ROOT, RULES, carrier, kb};
use observing_fs::ObservingFs;
use sage_db_format::{
    collection::write_proposal,
    digest::raw_sha256,
    event::{encode_with, read_event},
    wire::Reader,
};
use sage_db_kernel::{
    auth::{AuthError, Capability, CapabilitySet, Grants},
    review::{ReviewError, assess_approval},
    revision::{BaselineVerdict, RevisionError},
};
use sage_db_store::{
    CreateDir,
    fold::FoldedRecord,
    handle::{HandleOutcome, Store},
    init::bootstrap,
    lease::AttemptLedger,
    open::{Opened, OutcomeIncomplete, open_existing},
    view::{RecordLookup, RecordReadError, ViewDurability},
};
use sage_db_store_tests::{
    faults::{FaultPoint, FaultSchedule, Injection},
    model::fs_model::{ModelCapabilities, ModelFileSystem},
};
use sage_db_types::{
    framing::{Limits, Refusal},
    identity::RecordRevision,
    ledger::{BudgetError, BudgetUnit},
    persist::{
        ActorOrigin, CommonFields, Extensions, ProposalKind, ProposalRecord, ProposalStatus,
        RecordKey, RecordKind, RevPair,
    },
    prepared::{BlobDeclaration, BlobSource, OwnedEvent},
    query::{ControlStop, QueryControl, RunToCompletion},
};

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap()
}

fn proposal(id: u128, subject: u32) -> ProposalRecord {
    ProposalRecord {
        key: RecordKey::new(RecordKind::Proposal, id),
        revision: RecordRevision::FIRST,
        status: ProposalStatus::Pending,
        kind: ProposalKind::ConceptMarkdown,
        subject: RecordKey::term(subject),
        base_revisions: vec![RevPair {
            key: RecordKey::term(subject),
            revision: RecordRevision::FIRST,
        }],
        payload_ref: raw_sha256(b"opaque-proposal-content"),
        review_note: None,
        declared_author: "declared-author-is-not-reviewer".into(),
        common: CommonFields {
            recorded_by: Some("submitter".into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_002,
            updated_at: None,
        },
        legacy_id: None,
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

fn proposal_event(record: &ProposalRecord) -> OwnedEvent {
    let mut l = ledger();
    let bytes = encode_with(&mut l, |writer| write_proposal(writer, false, record)).unwrap();
    let decoded = read_event(&mut Reader::new(&bytes), &mut l).unwrap();
    OwnedEvent {
        tag: decoded.tag(),
        fields: decoded
            .fields()
            .iter()
            .map(|f| (f.tag(), f.value().to_vec()))
            .collect(),
    }
}

fn setup() -> (Rc<ModelFileSystem>, ObservingFs, Store<ObservingFs>) {
    setup_with_terms(2)
}

fn setup_with_terms(last: u32) -> (Rc<ModelFileSystem>, ObservingFs, Store<ObservingFs>) {
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
    let mut tx = carrier(2);
    let outcome = tx.events.pop().unwrap();
    for id in 3..=last {
        tx.events.push(fixture::term(u128::from(id), 1));
        let bytes = fixture::body(u128::from(id), 1);
        tx.blobs.push(BlobDeclaration {
            sha256: raw_sha256(&bytes),
            declared_len: bytes.len() as u64,
            source: BlobSource::Provide(bytes),
        });
    }
    tx.events.push(proposal_event(&proposal(1, 1)));
    tx.events.push(proposal_event(&proposal(2, 2)));
    tx.events.push(outcome);
    let bytes = b"opaque-proposal-content".to_vec();
    tx.blobs.push(BlobDeclaration {
        sha256: raw_sha256(&bytes),
        declared_len: bytes.len() as u64,
        source: BlobSource::Provide(bytes),
    });
    assert!(matches!(
        store.commit(tx, &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 2, .. }
    ));
    (inner, fs, store)
}

#[test]
fn current_lookup_reaches_last_of_64_terms_with_a_20_entry_budget() {
    let (_inner, _fs, store) = setup_with_terms(64);
    // Both sorted indexes fit within 7 comparisons each for this fixture.
    // The former linear walk fails this real finite ledger before reaching 64.
    let mut l = AttemptLedger::open(BYTES, 20, &RunToCompletion).unwrap();
    match store.record(RecordKey::term(64), &mut l).unwrap() {
        RecordLookup::Found {
            record: FoldedRecord::Term(term),
            ..
        } => {
            assert_eq!(term.key, RecordKey::term(64));
            assert_eq!(term.revision, RecordRevision::FIRST);
            assert_eq!(term.body_ref, raw_sha256(&fixture::body(64, 1)));
        }
        other => panic!("expected last term on a bounded read: {other:?}"),
    }
    assert!(
        l.stats()
            .admitted_entries
            .is_some_and(|entries| entries <= 20)
    );
    assert_eq!(l.stats().live_bytes, 0);
}

fn reviewer() -> Grants<'static> {
    Grants::new(
        "real-reviewer",
        kb(),
        CapabilitySet::of(&[Capability::Review]),
    )
}

#[test]
fn real_stored_proposals_use_object_revisions_and_assessment_never_writes() {
    let (_inner, fs, mut store) = setup();
    let original = store.published_pin().unwrap();
    let before = fs.counts().borrow().total();
    let mut l = ledger();
    let assessment = assess_approval(
        &store,
        &reviewer(),
        RecordKey::new(RecordKind::Proposal, 1),
        &[RecordKey::term(1)],
        &mut l,
    )
    .unwrap();
    assert_eq!(assessment.reviewer, "real-reviewer");
    assert_eq!(assessment.basis, store.view().unwrap().basis());
    assert_eq!(assessment.proposal.revision, RecordRevision::FIRST);
    assert_eq!(assessment.verdict, BaselineVerdict::Current);
    assert_eq!(fs.counts().borrow().total(), before);
    // A pending proposal is deliberately stale after this unrelated raw P1
    // commit; the other proposal remains current at the new whole-KB version.
    assert!(matches!(
        store.commit(carrier(3), &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 3, .. }
    ));
    let before = fs.counts().borrow().total();
    let stale = assess_approval(
        &store,
        &reviewer(),
        RecordKey::new(RecordKind::Proposal, 1),
        &[RecordKey::term(1)],
        &mut l,
    )
    .unwrap();
    assert!(
        matches!(stale.verdict, BaselineVerdict::Conflict(c) if c.key == RecordKey::term(1) && c.expected == RecordRevision::FIRST && c.found == RecordRevision::new(2))
    );
    let unrelated = assess_approval(
        &store,
        &reviewer(),
        RecordKey::new(RecordKind::Proposal, 2),
        &[RecordKey::term(2)],
        &mut l,
    )
    .unwrap();
    assert_eq!(unrelated.verdict, BaselineVerdict::Current);
    assert_eq!(unrelated.basis.version.get(), 3);
    assert_eq!(fs.counts().borrow().total(), before);
    assert_eq!(original.head().get(), 2);
}

#[test]
fn proposer_cannot_review_and_missing_dependency_cannot_be_omitted() {
    let (_inner, _fs, store) = setup();
    let grants = Grants::new("submitter", kb(), CapabilitySet::of(&[Capability::Propose]));
    let mut l = ledger();
    assert_eq!(
        assess_approval(
            &store,
            &grants,
            RecordKey::new(RecordKind::Proposal, 1),
            &[RecordKey::term(1)],
            &mut l
        ),
        Err(ReviewError::Authorization(AuthError::MissingCapability {
            capability: Capability::Review
        }))
    );
    assert_eq!(
        assess_approval(
            &store,
            &reviewer(),
            RecordKey::new(RecordKind::Proposal, 1),
            &[RecordKey::term(1), RecordKey::term(2)],
            &mut l
        ),
        Err(ReviewError::Revision(RevisionError::MissingBaseline(
            RecordKey::term(2)
        )))
    );
}

#[test]
fn current_record_returns_full_after_image_without_mutating_seam_calls() {
    let (_inner, fs, store) = setup();
    let before = fs.counts().borrow().total();
    let mut l = ledger();
    match store
        .record(RecordKey::new(RecordKind::Proposal, 1), &mut l)
        .unwrap()
    {
        RecordLookup::Found {
            record: FoldedRecord::Proposal(found),
            basis,
        } => {
            assert_eq!(found, &proposal(1, 1));
            assert_eq!(basis, store.view().unwrap().basis());
        }
        other => panic!("expected full proposal, got {other:?}"),
    }
    assert!(
        matches!(store.record(RecordKey::term(999), &mut l).unwrap(), RecordLookup::Absent { basis } if basis.version.get() == 2)
    );
    assert_eq!(fs.counts().borrow().total(), before);
    let mut zero = AttemptLedger::open(BYTES, 0, &RunToCompletion).unwrap();
    assert_eq!(
        store.record(RecordKey::term(1), &mut zero).unwrap_err(),
        RecordReadError::Refused(Refusal::Budget(BudgetError::new(BudgetUnit::Entries, 1, 0)))
    );
    assert_eq!(zero.stats().live_bytes, 0);
}

#[test]
fn unverified_and_attempted_unknown_never_claim_current_record_membership() {
    let (inner, fs, store) = setup();
    drop(store); // The owned root lock must be released before a real reopen.
    *inner.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::SyncEntry, Injection::FailNoEffect, 1);
    let mut l = ledger();
    let mut store =
        match open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut l).unwrap() {
            Opened::ReadOnly { store, .. } => store,
            _ => panic!("actual sync_entry failure must retain an Unverified handle"),
        };
    assert!(inner.faults_handle().borrow().all_fired());
    assert_eq!(
        store.view().unwrap().durability(),
        ViewDurability::Unverified
    );
    for key in [RecordKey::term(1), RecordKey::term(999)] {
        assert!(matches!(
            store.record(key, &mut l).unwrap(),
            RecordLookup::Incomplete(OutcomeIncomplete::NotDurable)
        ));
    }
    assert!(matches!(
        store.resolve(&Limits::V1, &mut l),
        sage_db_store::open::Resolution::Ready
    ));
    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    assert!(matches!(
        store.commit(carrier(3), &Limits::V1, &mut l),
        HandleOutcome::Unknown
    ));
    assert!(store.authority_undecided());
    for key in [RecordKey::term(1), RecordKey::term(999)] {
        assert!(matches!(
            store.record(key, &mut l).unwrap(),
            RecordLookup::Incomplete(OutcomeIncomplete::NoCurrentView)
        ));
    }
    assert_eq!(
        assess_approval(
            &store,
            &reviewer(),
            RecordKey::new(RecordKind::Proposal, 1),
            &[RecordKey::term(1)],
            &mut l
        ),
        Err(ReviewError::Incomplete(OutcomeIncomplete::NoCurrentView))
    );
}

#[test]
fn caller_cancellation_and_invalid_key_are_typed() {
    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let (_inner, _fs, store) = setup();
    let mut stopped = AttemptLedger::open(BYTES, ENTRIES, &Stop).unwrap();
    assert_eq!(
        store
            .record(RecordKey::term(999), &mut stopped)
            .unwrap_err(),
        RecordReadError::Refused(Refusal::Stopped(ControlStop::Cancelled))
    );
    let mut l = ledger();
    let key = RecordKey::new(RecordKind::Term, u128::from(u32::MAX) + 1);
    assert_eq!(
        store.record(key, &mut l).unwrap_err(),
        RecordReadError::InvalidKey(key)
    );
}

#[test]
fn orphan_unknown_keeps_known_durable_record_reads_while_fencing_writes() {
    let (inner, _fs, mut store) = setup();
    let basis = store.view().unwrap().basis();
    *inner.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::CreateNew, Injection::FailUnknown, 1);
    let mut l = ledger();
    assert!(matches!(
        store.commit(carrier(3), &Limits::V1, &mut l),
        HandleOutcome::NotCommitted { .. }
    ));
    assert!(inner.faults_handle().borrow().all_fired());
    assert!(store.is_fenced());
    assert!(!store.authority_undecided());
    match store
        .record(RecordKey::new(RecordKind::Proposal, 1), &mut l)
        .unwrap()
    {
        RecordLookup::Found {
            record: FoldedRecord::Proposal(found),
            basis: actual,
        } => {
            assert_eq!(found, &proposal(1, 1));
            assert_eq!(actual, basis);
        }
        other => panic!("orphan health fence must not erase known current: {other:?}"),
    }
}

#[test]
fn failed_diagnostic_withdraws_current_record_serving_and_preserves_old_pin() {
    let (inner, _fs, mut store) = setup();
    let pin = store.published_pin().unwrap();
    let before = pin.entries().to_vec();
    *inner.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::ReadExact, Injection::FailNoEffect, 1);
    let mut l = ledger();
    assert!(matches!(
        store.verify(&Limits::V1, &mut l),
        Err(sage_db_store::open::OpenRefusal::Filesystem { .. })
    ));
    assert!(inner.faults_handle().borrow().all_fired());
    assert!(!store.is_terminal_corrupt());
    assert!(matches!(
        store.record(RecordKey::term(1), &mut l).unwrap(),
        RecordLookup::Incomplete(OutcomeIncomplete::NoCurrentView)
    ));
    assert_eq!(pin.entries(), before);
    assert_eq!(pin.head().get(), 2);
    assert!(matches!(
        store.resolve(&Limits::V1, &mut l),
        sage_db_store::open::Resolution::Ready
    ));
    assert!(matches!(
        store.record(RecordKey::term(1), &mut l).unwrap(),
        RecordLookup::Found { .. }
    ));
}
