//! Real Store metadata/blob-read controls. Model transport, not device durability.
#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;

use fixture::{BYTES, ENTRIES, ROOT, RULES, body, carrier, kb};
use observing_fs::ObservingFs;
use sage_db_format::digest::{Domain, digest, raw_sha256};
use sage_db_store::{
    CreateDir, EntryName, Family, FileSystem,
    handle::{HandleOutcome, Store},
    init::bootstrap,
    lease::{AttemptLedger, LeaseLedger},
    open::{Opened, OutcomeIncomplete, open_existing},
    view::{BlobCorruption, BlobReadError, BlobReadStep, CommitReadError, ViewDurability},
};
use sage_db_store_tests::{
    faults::{FaultPoint, FaultSchedule, Injection},
    model::fs_model::{ModelCapabilities, ModelFileSystem},
};
use sage_db_types::{
    framing::{Limits, Refusal},
    ledger::{BudgetUnit, ControlStop},
    persist::IdSpace,
    query::{QueryControl, RunToCompletion},
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
fn basis_uses_actual_rule_record_blob_length_and_unused_allocation_mark() {
    let (_inner, fs, store) = setup();
    let before = fs.counts().borrow().total();
    let basis = store
        .commit_basis(IdSpace::ProposalId, &mut ledger())
        .unwrap();
    assert_eq!(basis.basis, store.view().unwrap().basis());
    assert_eq!(basis.rules.rules_ref, raw_sha256(RULES));
    assert_eq!(basis.rules.rules_digest, digest(Domain::Rules, RULES));
    assert_eq!(basis.rules_len, RULES.len() as u64);
    assert_eq!(basis.allocated_high_water, 0);
    assert_eq!(fs.counts().borrow().total(), before);
}

#[test]
fn blob_bytes_and_reservation_survive_attempt_and_release_with_the_owner() {
    let (_inner, fs, mut store) = setup();
    let before = fs.counts().borrow().total();
    let expected = body(1, 1);
    let mut l = ledger();
    let observer = l.reserve_owned(0).unwrap();
    let blob = store
        .read_committed_blob(raw_sha256(&expected), expected.len() as u64, &mut l)
        .unwrap();
    assert_eq!(blob.as_bytes(), expected);
    assert_eq!(blob.basis(), store.view().unwrap().basis());
    assert_eq!(blob.reserved_bytes(), expected.len() as u64);
    drop(l);
    assert_eq!(blob.as_bytes(), expected);
    assert_eq!(observer.stats().live_bytes, expected.len() as u64);
    assert_eq!(observer.stats().held_bytes, expected.len() as u64);
    assert_eq!(observer.stats().untransferred_bytes, 0);
    drop(blob);
    assert_eq!(observer.stats().live_bytes, 0);
    assert_eq!(fs.counts().borrow().total(), before);
}

#[test]
fn actual_ceiling_and_byte_budget_refuse_without_invalidating_valid_authority() {
    let (_inner, _fs, mut store) = setup();
    let bytes = body(1, 1);
    let h = raw_sha256(&bytes);
    let mut l = ledger();
    assert!(
        matches!(store.read_committed_blob(h, bytes.len() as u64-1, &mut l),
        Err(BlobReadError::Ceiling { size, ceiling }) if size==bytes.len() as u64 && ceiling+1==size)
    );
    assert_eq!(l.stats().live_bytes, 0);
    let mut zero = AttemptLedger::open(0, ENTRIES, &RunToCompletion).unwrap();
    assert!(matches!(store.read_committed_blob(h, BYTES, &mut zero),
        Err(BlobReadError::Refused(Refusal::Budget(e))) if e.unit()==BudgetUnit::Bytes && e.requested()==bytes.len() as u64 && e.remaining()==0));
    assert_eq!(zero.stats().held_bytes, 0);
    assert_eq!(store.view().unwrap().durability(), ViewDurability::Durable);
    assert!(!store.is_terminal_corrupt());
}

#[test]
fn unreferenced_digest_leaves_real_read_fault_pending_then_referenced_read_fires_it() {
    let (inner, _fs, mut store) = setup();
    *inner.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::ReadExact, Injection::FailNoEffect, 1);
    assert!(matches!(
        store.read_committed_blob([0x91; 32], BYTES, &mut ledger()),
        Err(BlobReadError::NotReferenced(_))
    ));
    assert!(inner.faults_handle().borrow().fired().is_empty());
    let mut l = ledger();
    match store.read_committed_blob(raw_sha256(&body(1, 1)), BYTES, &mut l) {
        Err(BlobReadError::Filesystem {
            step: BlobReadStep::Read,
            error,
        }) => {
            assert_eq!(error.kind(), sage_db_store::FsErrorKind::Io(0));
            assert_eq!(error.effect(), sage_db_store::Effect::NoEffect);
        }
        other => panic!("expected the actual exact-read error, got {other:?}"),
    }
    assert!(inner.faults_handle().borrow().all_fired());
    assert_eq!(l.stats().held_bytes, 0);
    assert_eq!(l.stats().live_bytes, 0);
    assert!(store.view().is_some());
}

#[test]
fn missing_committed_blob_is_terminal_and_does_not_become_absence() {
    let (inner, _fs, mut store) = setup();
    let digest = raw_sha256(&body(1, 1));
    let root = inner.open_root(Path::new(ROOT)).unwrap();
    let dir = inner
        .open_family(&root, Family::Blobs, CreateDir::Never)
        .unwrap();
    inner.remove(&dir, EntryName::Blob(digest)).unwrap();
    assert!(matches!(
        store.read_committed_blob(digest, BYTES, &mut ledger()),
        Err(BlobReadError::Corrupt(BlobCorruption::Missing))
    ));
    assert!(store.is_terminal_corrupt());
    assert!(store.view().is_none());
    assert!(store.published_pin().is_none());
    assert!(matches!(
        store.commit_basis(IdSpace::ProposalId, &mut ledger()),
        Err(CommitReadError::Incomplete(
            OutcomeIncomplete::NoCurrentView
        ))
    ));
}

#[test]
fn same_length_wrong_bytes_fail_the_returned_bytes_hash_and_release_reservations() {
    let (inner, _fs, mut store) = setup();
    let mut bytes = body(1, 1);
    let digest = raw_sha256(&bytes);
    let root = inner.open_root(Path::new(ROOT)).unwrap();
    let dir = inner
        .open_family(&root, Family::Blobs, CreateDir::Never)
        .unwrap();
    let name = EntryName::Blob(digest);
    inner.remove(&dir, name).unwrap();
    let mut file = inner.create_new(&dir, name).unwrap();
    bytes[0] ^= 0xff;
    let wrote = inner.write(&mut file, &bytes).unwrap();
    assert_eq!(wrote.written, bytes.len());
    let mut l = ledger();
    assert!(matches!(
        store.read_committed_blob(digest, BYTES, &mut l),
        Err(BlobReadError::Corrupt(BlobCorruption::HashMismatch))
    ));
    assert!(store.is_terminal_corrupt());
    assert_eq!(l.stats().held_bytes, 0);
}

#[test]
fn cancellation_and_unsynced_view_do_not_claim_a_durable_blob_or_commit_basis() {
    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let (inner, fs, mut store) = setup();
    let mut stopped = AttemptLedger::open(BYTES, ENTRIES, &Stop).unwrap();
    assert!(matches!(
        store.read_committed_blob(raw_sha256(RULES), BYTES, &mut stopped),
        Err(BlobReadError::Refused(Refusal::Stopped(
            ControlStop::Cancelled
        )))
    ));
    drop(store);
    *inner.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::SyncEntry, Injection::FailNoEffect, 1);
    let mut store =
        match open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger()).unwrap() {
            Opened::ReadOnly { store, .. } => store,
            _ => panic!("a real sync_entry fault must yield Unverified"),
        };
    assert!(matches!(
        store.commit_basis(IdSpace::ProposalId, &mut ledger()),
        Err(CommitReadError::Incomplete(OutcomeIncomplete::NotDurable))
    ));
    assert!(matches!(
        store.read_committed_blob(raw_sha256(RULES), BYTES, &mut ledger()),
        Err(BlobReadError::Incomplete(OutcomeIncomplete::NotDurable))
    ));
}
