//! P5 offline backup and fresh-root restore, on the real model backend.
//!
//! # Fixture shape, and why
//!
//! The model backend shares ONE path namespace across every root it registers, so two roots in
//! one instance collide on the empty relative path. Each root here therefore gets its own
//! `ModelFileSystem`: a source backend (which the artifact also lives in is impossible), and
//! separate artifact and restore backends where the copy needs both ends live at once.
//!
//! This is a property of the TEST backend, not of the product: `OsFileSystem` has no such limit.
//!
//! # What this file does not claim
//!
//! Device power-loss behaviour, RSS, or a phase/readiness judgement. Test-local allocations are
//! not allocator evidence.

#![allow(dead_code)]

#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::path::Path;
use std::rc::Rc;

use fixture::{BYTES, ENTRIES, carrier, kb};
use observing_fs::ObservingFs;

use sage_db_store::backup::{BackupError, BackupLimits, create, restore};
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::{CreateDir, Family, FileSystem, FsErrorKind};
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::names::{EntryName, RootName};
use sage_db_store::open::{OpenRefusal, OutcomeLookup};
use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};
use sage_db_types::ledger::BudgetUnit;
use sage_db_types::persist::{IdAllocationEvent, IdSpace, RecordKey, Watermark};
use sage_db_types::query::RunToCompletion;
use sage_db_types::query::{ControlStop, QueryControl};

fn ledger() -> AttemptLedger<'static> {
    static CONTROL: RunToCompletion = RunToCompletion;
    AttemptLedger::open(BYTES, ENTRIES, &CONTROL).expect("a finite ledger opens")
}

/// A two-version source on its OWN backend: v2 has body(1,1), v3 revises term 1 to body(1,2), so
/// the v2 body blob is no longer referenced by any CURRENT record.
fn source_world(root: &str) -> (Rc<ModelFileSystem>, ObservingFs, Store<ObservingFs>) {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = ObservingFs::new(Rc::clone(&inner));
    let store = seed_source(fs.clone(), Path::new(root));
    (inner, fs, store)
}

fn seed_source<F: FileSystem>(fs: F, root: &Path) -> Store<F> {
    let mut l = ledger();
    let mut store = bootstrap(
        fs,
        root,
        CreateDir::IfMissing,
        kb(),
        fixture::RULES,
        &Limits::V1,
        &mut l,
    )
    .expect("bootstrap");
    assert!(matches!(
        store.commit(carrier(2), &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 2, .. }
    ));
    let mut latest = carrier(3);
    let mark = IdAllocationEvent {
        allocation: Watermark {
            space: IdSpace::TermId,
            hwm: 3,
        },
        reason: None,
        extensions: Default::default(),
    };
    let FormatOutcome::Ok(event) = sage_db_store::typed::prepare_event(
        sage_db_store::typed::EventInput::IdAllocation(&mark),
        &mut l,
    ) else {
        panic!("encode the high-water mark")
    };
    latest.events.insert(latest.events.len() - 1, event);
    assert!(matches!(
        store.commit(latest, &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 3, .. }
    ));
    store
}

/// A backend of its own, so its root cannot collide with any other.
fn fresh_fs() -> (Rc<ModelFileSystem>, ObservingFs) {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = ObservingFs::new(Rc::clone(&inner));
    (inner, fs)
}

fn key_of(record: &FoldedRecord) -> Option<sage_db_types::persist::RecordKey> {
    match record {
        FoldedRecord::Term(r) => Some(r.key),
        FoldedRecord::Assertion(r) => Some(r.key),
        FoldedRecord::Claim(r) => Some(r.key),
        FoldedRecord::Proposal(r) => Some(r.key),
        _ => None,
    }
}

#[test]
fn a_backup_and_restore_round_trip_every_current_record() {
    let (_src_inner, _src_fs, mut source) = source_world("/p5-src-a");
    // The artifact lives in its OWN backend; the copy reads source bytes and writes here.
    let (_art_inner, art_fs) = fresh_fs();
    let mut l = ledger();
    let head = source.head().expect("head");
    let report = create(
        &mut source,
        &art_fs,
        Path::new("/p5-art-a"),
        head,
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("the latest version backs up");
    assert_eq!(report.version, 3);
    assert!(report.files > 0);
    assert!(report.bytes > 0);

    // A THIRD backend for the restore target, which is fresh.
    let (_dst_inner, dst_fs) = fresh_fs();
    let (mut restored, restored_report) = restore(
        &art_fs,
        Path::new("/p5-art-a"),
        dst_fs,
        Path::new("/p5-rest-a"),
        kb(),
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("the artifact restores");
    assert_eq!(restored_report.version, 3);
    assert_eq!(restored_report.kb_uuid, kb());
    assert_eq!(
        restored.head(),
        source.head(),
        "the restored root must serve the same head"
    );

    // Every current record of the source is present in the restored root, by full VALUE.
    let mut mismatched = 0u32;
    let mut scan_ledger = ledger();
    source
        .scan_current_records(&mut scan_ledger, |record, _| {
            if let Some(key) = key_of(record) {
                let mut read = ledger();
                match restored.record(key, &mut read) {
                    Ok(sage_db_store::view::RecordLookup::Found { record: other, .. }) => {
                        if other != record {
                            mismatched += 1;
                        }
                    }
                    _ => mismatched += 1,
                }
            }
            Ok::<(), ()>(())
        })
        .expect("scan");
    assert_eq!(mismatched, 0, "every restored record must match by value");

    let source_basis = source
        .commit_basis(IdSpace::TermId, &mut l)
        .expect("source basis");
    let restored_basis = restored
        .commit_basis(IdSpace::TermId, &mut l)
        .expect("restored basis");
    assert_eq!(source_basis.basis, restored_report.basis);
    assert_eq!(source_basis.basis, restored_basis.basis);
    assert_eq!(source_basis.allocated_high_water, 3);
    assert_eq!(restored_basis.allocated_high_water, 3);
    for version in [2, 3] {
        let request = fixture::request(version);
        let OutcomeLookup::Found {
            outcome,
            original_version,
            ..
        } = source.outcome(fixture::PRINCIPAL, request)
        else {
            panic!("source must retain outcome for {request}");
        };
        let OutcomeLookup::Found {
            outcome: copy,
            original_version: copied_version,
            ..
        } = restored.outcome(fixture::PRINCIPAL, request)
        else {
            panic!("restore must retain outcome for {request}");
        };
        assert_eq!(original_version, version);
        assert_eq!(copied_version, original_version);
        assert_eq!(copy, outcome);
    }
    use sage_db_store::fold::RecordIdentity;
    use sage_db_store::history::{HistoryBudget, HistoryOutcome};
    let identity = RecordIdentity::Identified {
        key: RecordKey::term(1),
    };
    let budget = HistoryBudget::new(64);
    let HistoryOutcome::Found(old) = source.history(&identity, 1, budget, &Limits::V1, &mut l)
    else {
        panic!("source must expose term 1 revision 1");
    };
    let HistoryOutcome::Found(copy) = restored.history(&identity, 1, budget, &Limits::V1, &mut l)
    else {
        panic!("restore must expose term 1 revision 1");
    };
    assert_eq!(copy.source_version, 2);
    assert_eq!(copy.source_version, old.source_version);
    assert!(copy.image.value_eq(&old.image));
    for expected in [
        fixture::RULES.to_vec(),
        fixture::result(2),
        fixture::result(3),
        fixture::body(1, 1),
        fixture::body(2, 1),
        fixture::body(1, 2),
        fixture::body(3, 1),
    ] {
        let digest = sage_db_format::digest::raw_sha256(&expected);
        let blob = restored
            .read_committed_blob(digest, expected.len() as u64, &mut l)
            .expect("restored historical blob");
        assert_eq!(blob.as_bytes(), expected);
    }
}

#[test]
fn an_older_ancestor_uses_its_own_current_not_the_source_head() {
    let (_src_inner, _src_fs, mut source) = source_world("/p5-src-b");
    let (_art_inner, art_fs) = fresh_fs();
    let mut l = ledger();
    create(
        &mut source,
        &art_fs,
        Path::new("/p5-art-b"),
        2,
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("an ancestor backs up");

    // The artifact's CURRENT names v2, not the source's head (v3).
    let root = art_fs
        .open_root(Path::new("/p5-art-b"))
        .expect("artifact root");
    assert_eq!(
        read_current_version(&art_fs, &root, &mut l),
        2,
        "the artifact's CURRENT must name the SELECTED version"
    );
    let blobs = art_fs
        .open_family(&root, Family::Blobs, CreateDir::Never)
        .expect("artifact blobs");
    for newer in [fixture::body(1, 2), fixture::body(3, 1), fixture::result(3)] {
        let digest = sage_db_format::digest::raw_sha256(&newer);
        assert_eq!(
            art_fs
                .size(&blobs, EntryName::Blob(digest))
                .unwrap_err()
                .kind(),
            FsErrorKind::NotFound
        );
    }

    let (_dst_inner, dst_fs) = fresh_fs();
    let (restored, report) = restore(
        &art_fs,
        Path::new("/p5-art-b"),
        dst_fs,
        Path::new("/p5-rest-b"),
        kb(),
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("the v2 artifact restores");
    assert_eq!(report.version, 2);
    assert_eq!(restored.head(), Some(2), "the restored root serves v2");
}

fn read_current_version<F: FileSystem>(
    fs: &F,
    root: &F::Dir,
    ledger: &mut AttemptLedger<'_>,
) -> u64 {
    let size = fs
        .size(root, EntryName::Root(RootName::Current))
        .expect("CURRENT exists");
    // `read_exact_into` fills a caller-sized slice exactly.
    let mut bytes = vec![0u8; size as usize];
    fs.read_exact_into(root, EntryName::Root(RootName::Current), &mut bytes[..])
        .expect("read CURRENT");
    match sage_db_format::current::decode_current(&bytes, &Limits::V1, ledger) {
        sage_db_types::framing::FormatOutcome::Ok(current) => current.version,
        other => panic!("the artifact CURRENT must decode: {other:?}"),
    }
}

#[test]
fn a_future_or_zero_version_is_refused_before_any_target_is_created() {
    let (_src_inner, _src_fs, mut source) = source_world("/p5-src-c");
    let (_art_inner, art_fs) = fresh_fs();
    let mut l = ledger();
    for asked in [0u64, 4] {
        match create(
            &mut source,
            &art_fs,
            Path::new("/p5-art-c"),
            asked,
            &BackupLimits::DEFAULT,
            &mut l,
        ) {
            Err(BackupError::NotAnAncestor { requested, served }) => {
                assert_eq!(requested, asked);
                assert_eq!(served, 3);
            }
            Err(other) => panic!("version {asked} must be refused as non-ancestor, got {other:?}"),
            Ok(_) => panic!("version {asked} must be refused"),
        }
    }
    assert!(
        art_fs.open_root(Path::new("/p5-art-c")).is_err(),
        "a refused request must not create the artifact root"
    );
}

#[test]
fn a_missing_completion_marker_is_refused_without_creating_the_target() {
    let (_src_inner, _src_fs, mut source) = source_world("/p5-src-d");
    let (_art_inner, art_fs) = fresh_fs();
    let mut l = ledger();
    let head = source.head().expect("head");
    create(
        &mut source,
        &art_fs,
        Path::new("/p5-art-d"),
        head,
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("backup");
    let root = art_fs.open_root(Path::new("/p5-art-d")).expect("root");
    art_fs
        .remove(&root, EntryName::Root(RootName::BackupComplete))
        .expect("remove marker");

    let (_dst_inner, dst_fs) = fresh_fs();
    match restore(
        &art_fs,
        Path::new("/p5-art-d"),
        dst_fs,
        Path::new("/p5-rest-d"),
        kb(),
        &BackupLimits::DEFAULT,
        &mut l,
    ) {
        Err(BackupError::MissingComplete) => {}
        Err(other) => panic!("a missing completion marker must refuse, got {other:?}"),
        Ok(_) => panic!("a missing completion marker must refuse"),
    }
}

#[test]
fn a_foreign_kb_is_refused_before_creation() {
    let (_src_inner, _src_fs, mut source) = source_world("/p5-src-e");
    let (_art_inner, art_fs) = fresh_fs();
    let mut l = ledger();
    let head = source.head().expect("head");
    create(
        &mut source,
        &art_fs,
        Path::new("/p5-art-e"),
        head,
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("backup");

    let other = sage_db_types::identity::KbUuid::from_bytes([0x77; 16]);
    let (_dst_inner, dst_fs) = fresh_fs();
    match restore(
        &art_fs,
        Path::new("/p5-art-e"),
        dst_fs,
        Path::new("/p5-rest-e"),
        other,
        &BackupLimits::DEFAULT,
        &mut l,
    ) {
        Err(BackupError::KbMismatch { found, expected }) => {
            assert_eq!(found, kb());
            assert_eq!(expected, other);
        }
        Err(other) => panic!("a foreign KB must refuse, got {other:?}"),
        Ok(_) => panic!("a foreign KB must refuse"),
    }
}

#[test]
fn a_configured_ceiling_below_the_source_is_refused_before_creation() {
    let (_src_inner, _src_fs, mut source) = source_world("/p5-src-f");
    let (_art_inner, art_fs) = fresh_fs();
    let mut l = ledger();
    let tight = BackupLimits {
        max_total_bytes: 16,
        ..BackupLimits::DEFAULT
    };
    let head = source.head().expect("head");
    match create(
        &mut source,
        &art_fs,
        Path::new("/p5-art-f"),
        head,
        &tight,
        &mut l,
    ) {
        // A CONFIGURED ceiling is its own verdict, never a source refusal: fencing a healthy
        // source because the operator asked for too little would be wrong.
        Err(BackupError::LimitExceeded(_)) => {}
        Err(other) => panic!("an over-tight ceiling must be a limit verdict, got {other:?}"),
        Ok(_) => panic!("an over-tight ceiling must refuse"),
    }
    assert!(
        art_fs.open_root(Path::new("/p5-art-f")).is_err(),
        "a preflight refusal must not create the artifact root"
    );
}

#[test]
fn the_artifact_excludes_orphans_and_includes_superseded_content() {
    let (_src_inner, src_fs, mut source) = source_world("/p5-src-g");
    let (_art_inner, art_fs) = fresh_fs();
    let source_root = src_fs
        .open_root(Path::new("/p5-src-g"))
        .expect("source root");
    let source_blobs = src_fs
        .open_family(&source_root, Family::Blobs, CreateDir::Never)
        .expect("source blobs");
    let orphan_bytes = b"never-referenced-orphan";
    let orphan_sha = sage_db_format::digest::raw_sha256(orphan_bytes);
    write_fixture_file(
        &src_fs,
        &source_blobs,
        EntryName::Blob(orphan_sha),
        orphan_bytes,
    );
    let mut l = ledger();
    create(
        &mut source,
        &art_fs,
        Path::new("/p5-art-g"),
        3,
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("head backs up");

    let root = art_fs.open_root(Path::new("/p5-art-g")).expect("root");
    let blobs = art_fs
        .open_family(&root, sage_db_store::fs::Family::Blobs, CreateDir::Never)
        .expect("blobs family");

    // The v2 body blob (body(1,1)) IS carried, even though no CURRENT v3 record references it.
    let old_body = fixture::body(1, 1);
    let old_sha = sage_db_format::digest::raw_sha256(&old_body);
    assert!(
        art_fs.size(&blobs, EntryName::Blob(old_sha)).is_ok(),
        "superseded content must be carried by the artifact"
    );

    // A blob that no version ever referenced is NOT carried.
    assert!(
        art_fs.size(&blobs, EntryName::Blob(orphan_sha)).is_err(),
        "an unreferenced blob must not be carried"
    );
}

fn write_fixture_file(
    fs: &ObservingFs,
    dir: &<ObservingFs as FileSystem>::Dir,
    name: EntryName,
    content: &[u8],
) {
    let mut file = fs.create_new(dir, name).expect("new fixture file");
    assert_eq!(
        fs.write(&mut file, content).expect("write fixture").written,
        content.len()
    );
    fs.sync_file(&file).expect("sync fixture content");
    fs.sync_dir(dir).expect("sync fixture name");
}

#[test]
fn the_inventory_has_its_exact_literal_shape() {
    let (_src_inner, _src_fs, mut source) = source_world("/p5-src-h");
    let (_art_inner, art_fs) = fresh_fs();
    let mut l = ledger();
    let head = source.head().expect("head");
    create(
        &mut source,
        &art_fs,
        Path::new("/p5-art-h"),
        head,
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("backup");

    let root = art_fs.open_root(Path::new("/p5-art-h")).expect("root");

    // Completion marker: magic SGBDONE1, 48 bytes total.
    let mut complete = [0u8; 48];
    art_fs
        .read_exact_into(
            &root,
            EntryName::Root(RootName::BackupComplete),
            &mut complete[..],
        )
        .expect("read completion");
    assert_eq!(&complete[0..8], b"SGBDONE1", "completion magic");
    assert_eq!(complete.len(), 48, "completion length");

    // Inventory: magic SGBKUP\0\x01, then kb_uuid, version, manifest sha, row count.
    let inv_size = art_fs
        .size(&root, EntryName::Root(RootName::BackupInventory))
        .expect("inventory size");
    let mut inv = vec![0u8; inv_size as usize];
    art_fs
        .read_exact_into(
            &root,
            EntryName::Root(RootName::BackupInventory),
            &mut inv[..],
        )
        .expect("read inventory");
    assert_eq!(&inv[0..8], b"SGBKUP\0\x01", "inventory magic and major");
    assert_eq!(
        &inv[8..24],
        kb().as_bytes(),
        "inventory carries the KB identity"
    );
    assert_eq!(
        u64::from_le_bytes(inv[24..32].try_into().expect("version bytes")),
        3,
        "inventory names the exported version"
    );
    let rows = u64::from_le_bytes(inv[64..72].try_into().expect("count bytes"));
    // Header 72 + rows * 65 must equal the file exactly.
    assert_eq!(
        inv.len() as u64,
        72 + rows * 65,
        "inventory length must be header + rows*65"
    );

    // The completion marker's declared hash and length describe THIS inventory.
    assert_eq!(
        &complete[8..40],
        &sage_db_format::digest::raw_sha256(&inv),
        "hash binds the inventory"
    );
    assert_eq!(
        u64::from_le_bytes(complete[40..48].try_into().expect("len bytes")),
        inv.len() as u64,
        "declared inventory length"
    );
}

// ---------------------------------------------------------------------------
// R3: fault/crash matrix, incomplete-import roundtrip, budget/stop, corrupt inventory
// ---------------------------------------------------------------------------

#[test]
fn every_sampled_artifact_world_is_either_restorable_or_typed_incomplete() {
    let (_, _, mut source) = source_world("/p5-src-control");
    let (clean_inner, clean_fs) = fresh_fs();
    let clean = Path::new("/p5-art-control");
    create(
        &mut source,
        &clean_fs,
        clean,
        3,
        &BackupLimits::DEFAULT,
        &mut ledger(),
    )
    .expect("unfaulted artifact");
    let counts = clean_fs.counts();
    let counts = counts.borrow();
    // Copy writes are direct create/write/sync operations. There is no
    // rename/publish seam in this artifact format. Exercise the first and
    // final occurrences, including the completion marker and its directory.
    let stages = [
        (FaultPoint::SyncParentOf, counts.sync_parent_of),
        (FaultPoint::CreateNew, counts.create_new), // Count the root create in the observer's ordinal.
        (FaultPoint::Write, counts.write),
        (FaultPoint::SyncFile, counts.sync_file),
        (FaultPoint::SyncEntry, counts.sync_entry),
        (FaultPoint::SyncDir, counts.sync_dir),
    ];
    let mut complete_worlds = 0;
    let mut incomplete_worlds = 0;
    for state in clean_inner.model().crash_states().states {
        assert!(check_artifact_world(clean, state));
        complete_worlds += 1;
    }
    assert!(complete_worlds > 0);

    let mut schedules = 0;
    for (point, count) in stages {
        assert!(count > 0, "unexercised stage {point:?}");
        let first = if point == FaultPoint::CreateNew { 2 } else { 1 };
        let occurrences = if count == first {
            vec![first]
        } else {
            vec![first, count]
        };
        for occurrence in occurrences {
            let mut injections = vec![Injection::FailNoEffect, Injection::FailUnknown];
            if matches!(point, FaultPoint::SyncFile | FaultPoint::SyncDir) {
                injections.push(Injection::FailUnknownAfterEffect);
            }
            for injection in injections {
                schedules += 1;
                let (inner, artifact) = fresh_fs();
                let path = Path::new("/p5-art-world");
                let artifact = artifact.fault_at(
                    point,
                    occurrence,
                    FaultSchedule::none().at(point, injection, 1),
                );
                let result = create(
                    &mut source,
                    &artifact,
                    path,
                    3,
                    &BackupLimits::DEFAULT,
                    &mut ledger(),
                );
                assert!(
                    result.is_err(),
                    "fault must be reported: {point:?}/{occurrence}/{injection:?}"
                );
                assert!(
                    inner.faults_handle().borrow().fired() == [(point, injection)],
                    "{point:?}/{occurrence}/{injection:?} was never reached"
                );
                let worlds = inner.model().crash_states();
                assert!(!worlds.is_empty());
                assert_eq!(worlds.sampled, worlds.states.len());
                for state in worlds.states {
                    if check_artifact_world(path, state) {
                        complete_worlds += 1;
                    } else {
                        incomplete_worlds += 1;
                    }
                }
            }
        }
    }
    // The last write starts the completion marker. A short positive write must
    // retry the remainder, and a failure on that retry cannot publish a backup.
    let point = FaultPoint::Write;
    let schedule = FaultSchedule::none()
        .at(point, Injection::ShortWrite(12), 1)
        .at(point, Injection::FailNoEffect, 1);
    let (inner, artifact) = fresh_fs();
    let artifact = artifact.fault_at(point, counts.write, schedule);
    let path = Path::new("/p5-art-short-marker");
    let result = create(
        &mut source,
        &artifact,
        path,
        3,
        &BackupLimits::DEFAULT,
        &mut ledger(),
    );
    assert!(matches!(result, Err(BackupError::Filesystem { .. })));
    assert_eq!(
        inner.faults_handle().borrow().fired(),
        [
            (point, Injection::ShortWrite(12)),
            (point, Injection::FailNoEffect)
        ],
    );
    schedules += 1;
    let worlds = inner.model().crash_states();
    assert!(!worlds.is_empty());
    assert_eq!(worlds.sampled, worlds.states.len());
    for state in worlds.states {
        assert!(!check_artifact_world(path, state));
        incomplete_worlds += 1;
    }

    assert!(incomplete_worlds > 0);
    eprintln!(
        "MATRIX schedules={schedules} complete={complete_worlds} incomplete={incomplete_worlds}"
    );
}

/// Return whether the sampled world had a complete marker and restored.
fn check_artifact_world(path: &Path, state: sage_db_store_tests::model::DurableState) -> bool {
    use sage_db_store_tests::model::RelPath;
    let marker = RelPath::join(&RelPath::root(), RootName::BackupComplete.as_str());
    let complete = state
        .read(&marker)
        .is_some_and(|bytes| bytes.len() == 48 && bytes.starts_with(b"SGBDONE1"));
    let backend = ModelFileSystem::from_durable_state(ModelCapabilities::full(), path, state)
        .expect("every sampled artifact world must be opened");
    let artifact = ObservingFs::new(Rc::new(backend));
    let (_, dst) = fresh_fs();
    let result = restore(
        &artifact,
        path,
        dst.clone(),
        Path::new("/p5-rest-world"),
        kb(),
        &BackupLimits::DEFAULT,
        &mut ledger(),
    );
    if complete {
        let (store, report) =
            result.unwrap_or_else(|error| panic!("complete marker must restore: {error:?}"));
        assert_eq!(report.version, 3);
        assert_eq!(store.head(), Some(3));
        return true;
    }
    match result {
        Err(BackupError::MissingComplete | BackupError::CompleteMismatch) => {}
        Err(BackupError::Filesystem { error, .. }) if error.kind() == FsErrorKind::NotFound => {}
        Err(other) => panic!("incomplete marker must refuse as incomplete: {other:?}"),
        Ok(_) => panic!("restore accepted a world without a complete marker"),
    }
    assert_eq!(
        dst.counts().borrow().total(),
        0,
        "incomplete world created a target"
    );
    false
}

#[test]
fn an_incomplete_import_round_trips_as_still_incomplete_and_gated() {
    // A KB whose committed import session is unfinished must come back unfinished: a backup must
    // never turn an in-progress import into a writable root.
    let (_src_inner, _src_fs, mut source) = source_world("/p5-src-inc");
    {
        let mut l = ledger();
        commit_unfinished_import(&mut source, &mut l);
    }
    let (_art_inner, art_fs) = fresh_fs();
    let mut l = ledger();
    let head = source.head().expect("head");
    create(
        &mut source,
        &art_fs,
        Path::new("/p5-art-inc"),
        head,
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("an unfinished-import KB still backs up");

    let (_dst_inner, dst_fs) = fresh_fs();
    let (restored, _report) = restore(
        &art_fs,
        Path::new("/p5-art-inc"),
        dst_fs,
        Path::new("/p5-rest-inc"),
        kb(),
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("the artifact restores");

    // The restored root reports the SAME unfinished state, not Ready.
    let mut read = ledger();
    match restored.import_readiness(&mut read) {
        Ok(sage_db_store::view::ImportReadiness::Incomplete { session, .. }) => {
            assert_eq!(session.import_session_id, "p5-gate");
            assert!(!session.import_complete, "it must stay incomplete");
        }
        Ok(sage_db_store::view::ImportReadiness::Ready { .. }) => {
            panic!("an unfinished import must not become Ready after restore")
        }
        Err(error) => panic!("readiness must be answerable: {error:?}"),
    }
}

/// Commit an `import_complete = false` session using the canonical typed bridge.
fn commit_unfinished_import(store: &mut Store<ObservingFs>, l: &mut AttemptLedger<'_>) {
    use sage_db_store::typed::EventInput;
    use sage_db_types::persist::{CreationAudit, ImportSessionRecord, ImportStage};

    let version = store.head().expect("head") + 1;
    let basis = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, l)
        .expect("basis");
    let plan = b"p5-backup-import-plan".to_vec();
    let plan_ref = sage_db_format::digest::raw_sha256(&plan);
    let result = b"p5-backup-import-result".to_vec();
    let result_ref = sage_db_format::digest::raw_sha256(&result);
    let audit = CreationAudit {
        recorded_by: Some("gate".to_owned()),
        actor_origin: sage_db_types::persist::ActorOrigin::Authenticated,
        reason: Some("incomplete import fixture".to_owned()),
        created_at: 1_700_000_000_000_000 + version as i64,
    };
    let record = ImportSessionRecord {
        import_session_id: "p5-gate".to_owned(),
        stage: ImportStage::Prepare,
        source_digest: [9; 32],
        plan_ref,
        expected_counts: vec![],
        import_complete: false,
        audit: audit.clone(),
        extensions: Default::default(),
    };
    let outcome = sage_db_types::persist::RequestOutcomeEvent {
        principal_id: "gate".to_owned(),
        request_id: format!("p5-gate-{version}"),
        payload_digest: [version as u8; 32],
        result_ref,
        commit_version: version,
        audit,
        extensions: Default::default(),
    };
    let events = vec![
        match sage_db_store::typed::prepare_event(EventInput::ImportSession(&record), l) {
            sage_db_types::framing::FormatOutcome::Ok(event) => event,
            other => panic!("import session event: {other:?}"),
        },
        match sage_db_store::typed::prepare_event(EventInput::Outcome(&outcome), l) {
            sage_db_types::framing::FormatOutcome::Ok(event) => event,
            other => panic!("outcome event: {other:?}"),
        },
    ];
    let blobs = vec![
        sage_db_types::prepared::BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: sage_db_types::prepared::BlobSource::Existing,
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: plan_ref,
            declared_len: plan.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(plan),
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: result_ref,
            declared_len: result.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(result),
        },
    ];
    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: [0xb7; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: "gate".to_owned(),
        request_id: format!("p5-gate-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: None,
        extensions: Default::default(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, l) {
        HandleOutcome::Committed { .. } => {}
        other => panic!("incomplete-import fixture commit failed: {other:?}"),
    }
}

#[test]
fn a_real_budget_refusal_and_a_real_stop_keep_their_classes() {
    for (bytes, entries, expected) in [
        (64, ENTRIES, BudgetUnit::Bytes),
        (BYTES, 1, BudgetUnit::Entries),
    ] {
        let (_, _, mut source) = source_world("/p5-src-bud");
        let (_, artifact) = fresh_fs();
        let head = source.head().expect("head");
        let mut tight = AttemptLedger::open(bytes, entries, &RunToCompletion).expect("ledger");
        let error = create(
            &mut source,
            &artifact,
            Path::new("/p5-art-bud"),
            head,
            &BackupLimits::DEFAULT,
            &mut tight,
        )
        .unwrap_err();
        let refusal = match error {
            BackupError::Refused(refusal)
            | BackupError::SourceRefused(OpenRefusal::Refused(refusal)) => refusal,
            BackupError::SourceRefused(OpenRefusal::Filesystem { error, .. }) => {
                Refusal::Budget(error.budget().expect("budget details"))
            }
            other => panic!("expected {expected:?} refusal, got {other:?}"),
        };
        let Refusal::Budget(budget) = refusal else {
            panic!("not a budget error: {refusal:?}")
        };
        assert_eq!(budget.unit(), expected);
        assert!(budget.requested() > budget.remaining());
    }

    // A real stopping control, on its own SOURCE and destination: the byte-exhausted attempt above
    // already withdrew the first source, so reusing it would measure the wrong class.
    let (_src2_inner, _src2_fs, mut source2) = source_world("/p5-src-stop");
    let head2 = source2.head().expect("head");
    struct StopNow;
    impl QueryControl for StopNow {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let (_art2_inner, art2_fs) = fresh_fs();
    let mut stopping = AttemptLedger::open(BYTES, ENTRIES, &StopNow).expect("ledger");
    match create(
        &mut source2,
        &art2_fs,
        Path::new("/p5-art-stop"),
        head2,
        &BackupLimits::DEFAULT,
        &mut stopping,
    ) {
        Err(BackupError::Refused(Refusal::Stopped(ControlStop::Cancelled)))
        | Err(BackupError::SourceRefused(OpenRefusal::Refused(Refusal::Stopped(
            ControlStop::Cancelled,
        )))) => {}
        Err(other) => panic!("a stop must refuse as a refusal class, got {other:?}"),
        Ok(_) => panic!("a stopped ledger cannot complete a backup"),
    }
}

#[test]
fn a_corrupt_inventory_is_refused_by_shape_not_just_by_hash() {
    // Rebuild the completion marker so it MATCHES the tampered inventory: this defeats the
    // hash check and leaves only the structural checks to catch the corruption.
    for (case, mutate) in [
        ("magic", 0usize),
        ("tag", 72usize),
        ("count", 64usize),
        ("field", 73usize),
        ("manifest", 32usize),
        ("duplicate", 72usize),
    ] {
        let (_src_inner, _src_fs, mut source) = source_world(&format!("/p5-src-c-{case}"));
        let (_art_inner, art_fs) = fresh_fs();
        let mut l = ledger();
        let head = source.head().expect("head");
        let art = format!("/p5-art-c-{case}");
        create(
            &mut source,
            &art_fs,
            Path::new(&art),
            head,
            &BackupLimits::DEFAULT,
            &mut l,
        )
        .expect("backup");

        let root = art_fs.open_root(Path::new(&art)).expect("root");
        let inv_size = art_fs
            .size(&root, EntryName::Root(RootName::BackupInventory))
            .expect("inventory");
        let mut inv = vec![0u8; inv_size as usize];
        art_fs
            .read_exact_into(
                &root,
                EntryName::Root(RootName::BackupInventory),
                &mut inv[..],
            )
            .expect("read inventory");
        match case {
            "magic" => inv[mutate] ^= 0xff,
            "count" => inv[64] = inv[64].wrapping_add(1),
            "tag" => inv[72] = 0xfe,
            "manifest" => inv[mutate] ^= 0xff,
            "duplicate" => {
                // Two names for the same first manifest, even though the
                // rows declare different raw file hashes.
                let (rows, remainder) = inv[72..].as_chunks::<65>();
                assert!(remainder.is_empty());
                let at = rows.iter().position(|row| row[0] == 1).expect("manifest");
                let first = 72 + at * 65;
                let mut duplicate = inv[first..first + 65].to_vec();
                duplicate[25] ^= 1;
                inv.splice(first + 65..first + 65, duplicate);
                let count = u64::from_le_bytes(inv[64..72].try_into().unwrap()) + 1;
                inv[64..72].copy_from_slice(&count.to_le_bytes());
            }
            // Corrupt a field that must be zero for its tag (CURRENT carries zero id bytes).
            _ => inv[72 + 1 + 8] = 0x7f,
        }
        art_fs
            .remove(&root, EntryName::Root(RootName::BackupInventory))
            .expect("remove");
        let mut writer = art_fs
            .create_new(&root, EntryName::Root(RootName::BackupInventory))
            .expect("recreate");
        art_fs.write(&mut writer, &inv).expect("write");
        art_fs.sync_file(&writer).expect("sync");

        // Rewrite the completion marker so its hash and length MATCH the tampered inventory.
        art_fs
            .remove(&root, EntryName::Root(RootName::BackupComplete))
            .expect("remove marker");
        let mut complete = [0u8; 48];
        complete[0..8].copy_from_slice(b"SGBDONE1");
        complete[8..40].copy_from_slice(&sage_db_format::digest::raw_sha256(&inv));
        complete[40..48].copy_from_slice(&(inv.len() as u64).to_le_bytes());
        let mut marker = art_fs
            .create_new(&root, EntryName::Root(RootName::BackupComplete))
            .expect("recreate marker");
        art_fs.write(&mut marker, &complete).expect("write marker");
        art_fs.sync_file(&marker).expect("sync marker");

        let (_dst_inner, dst_fs) = fresh_fs();
        match restore(
            &art_fs,
            Path::new(&art),
            dst_fs.clone(),
            Path::new(&format!("/p5-rest-c-{case}")),
            kb(),
            &BackupLimits::DEFAULT,
            &mut l,
        ) {
            Err(BackupError::InventoryCorrupt(_)) => {}
            Err(other) => {
                panic!("case {case}: expected a structural inventory refusal, got {other:?}")
            }
            Ok(_) => panic!("case {case}: a structurally corrupt inventory must be refused"),
        }
        assert_eq!(
            dst_fs
                .open_root(Path::new(&format!("/p5-rest-c-{case}")))
                .unwrap_err()
                .kind(),
            FsErrorKind::NotFound
        );
    }
}

#[test]
fn same_length_current_or_blob_corruption_never_creates_the_restore_root() {
    let (_, _, mut source) = source_world("/p5-src-corruption");
    let mut l = ledger();
    for tag in [0, 3] {
        let (_, artifact) = fresh_fs();
        let path = Path::new("/p5-art-corruption");
        create(
            &mut source,
            &artifact,
            path,
            3,
            &BackupLimits::DEFAULT,
            &mut l,
        )
        .expect("artifact");
        let root = artifact.open_root(path).expect("root");
        let (dir, name) = if tag == 0 {
            (root, EntryName::Root(RootName::Current))
        } else {
            let dir = artifact
                .open_family(&root, Family::Blobs, CreateDir::Never)
                .expect("artifact blobs");
            let hash = sage_db_format::digest::raw_sha256(&fixture::body(1, 1));
            (dir, EntryName::Blob(hash))
        };
        let mut corrupt = vec![0; artifact.size(&dir, name).unwrap() as usize];
        artifact
            .read_exact_into(&dir, name, &mut corrupt)
            .expect("listed file");
        corrupt[0] ^= 1;
        artifact
            .remove(&dir, name)
            .expect("replace the fixture file");
        write_fixture_file(&artifact, &dir, name, &corrupt);
        let (_, dst) = fresh_fs();
        let dest = Path::new("/p5-rest-corruption");
        assert!(matches!(
            restore(&artifact, path, dst.clone(), dest, kb(), &BackupLimits::DEFAULT, &mut l),
            Err(BackupError::FileMismatch { tag: found }) if found == tag
        ));
        assert_eq!(dst.counts().borrow().total(), 0);
        assert_eq!(
            dst.open_root(dest).unwrap_err().kind(),
            FsErrorKind::NotFound
        );
    }
}

#[test]
fn an_existing_restore_root_is_left_byte_identical() {
    let (_, _, mut source) = source_world("/p5-src-existing");
    let (_, artifact) = fresh_fs();
    let mut l = ledger();
    let path = Path::new("/p5-art-existing");
    create(
        &mut source,
        &artifact,
        path,
        3,
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("artifact");
    for populated in [false, true] {
        let (dst_inner, dst) = fresh_fs();
        let dest = Path::new("/p5-rest-existing");
        let root = dst.create_root(dest).expect("existing root");
        dst.sync_parent_of(dest).expect("root name durable");
        if populated {
            write_fixture_file(
                &dst,
                &root,
                EntryName::Root(RootName::Current),
                b"keep these exact bytes",
            );
        }
        let before = dst_inner.model().crash_states().states;
        let counts = dst.counts().borrow().total();
        assert!(matches!(
            restore(
                &artifact,
                path,
                dst.clone(),
                dest,
                kb(),
                &BackupLimits::DEFAULT,
                &mut l
            ),
            Err(BackupError::DestinationExists)
        ));
        assert_eq!(
            dst.counts().borrow().total(),
            counts + 1,
            "only the exclusive create attempt"
        );
        assert_eq!(dst_inner.model().crash_states().states, before);
    }
}

#[test]
fn restore_refuses_a_contended_artifact_before_creating_a_destination() {
    let (_, _, mut source) = source_world("/p5-src-lock");
    let (_, artifact) = fresh_fs();
    let mut l = ledger();
    let path = Path::new("/p5-art-lock");
    create(
        &mut source,
        &artifact,
        path,
        3,
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("artifact");
    let root = artifact.open_root(path).expect("root");
    let held = artifact
        .try_lock_exclusive(&root)
        .expect("hold artifact lock");
    let (_, dst) = fresh_fs();
    let dest = Path::new("/p5-rest-lock");
    let result = restore(
        &artifact,
        path,
        dst.clone(),
        dest,
        kb(),
        &BackupLimits::DEFAULT,
        &mut l,
    );
    assert!(
        matches!(result, Err(BackupError::Filesystem { error, .. }) if error.kind() == FsErrorKind::WouldBlock)
    );
    assert_eq!(dst.counts().borrow().total(), 0);
    drop(held);
    restore(
        &artifact,
        path,
        dst,
        dest,
        kb(),
        &BackupLimits::DEFAULT,
        &mut l,
    )
    .expect("restore after releasing lock");
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
#[ignore = "operator-only: requires a private parent on a qualified Linux/Btrfs mount"]
fn qualified_disk_backup_and_restore_reopen_both_versions() {
    use sage_db_store::open::{Opened, open_existing};
    use sage_db_store::os::OsFileSystem;

    let parent = std::path::PathBuf::from(
        std::env::var_os("SAGE_DB_BACKUP_PROBE_DIR").expect("private scratch directory required"),
    );
    assert!(parent.is_absolute() && parent.starts_with("/var/tmp") && parent.is_dir());
    assert!(
        parent
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("sage-backup-r4-")
    );
    let root = parent.join("source");
    assert!(!root.exists());
    let mut source = seed_source(
        OsFileSystem::for_root(&root).expect("qualified source"),
        &root,
    );
    for version in [2, 3] {
        let artifact = parent.join(format!("backup-{version}"));
        let target = parent.join(format!("restore-{version}"));
        assert!(!artifact.exists() && !target.exists());
        let artifact_fs = OsFileSystem::for_root(&artifact).expect("qualified backup target");
        let target_fs = OsFileSystem::for_root(&target).expect("qualified restore target");
        let report = create(
            &mut source,
            &artifact_fs,
            &artifact,
            version,
            &BackupLimits::DEFAULT,
            &mut ledger(),
        )
        .expect("backup on the qualified mount");
        assert_eq!(report.version, version);
        let (restored, report) = restore(
            &artifact_fs,
            &artifact,
            target_fs,
            &target,
            kb(),
            &BackupLimits::DEFAULT,
            &mut ledger(),
        )
        .expect("restore on the qualified mount");
        drop(restored);
        let Opened::Ready(mut reopened) = open_existing(
            OsFileSystem::for_root(&target).expect("qualify restored root again"),
            &target,
            Some(kb()),
            &Limits::V1,
            &mut ledger(),
        )
        .expect("reopen from disk") else {
            panic!("restored root must reopen as Ready");
        };
        assert_eq!(reopened.head(), Some(version));
        let basis = reopened
            .commit_basis(IdSpace::TermId, &mut ledger())
            .expect("reopened basis");
        assert_eq!(basis.basis, report.basis);
        if version == 3 {
            assert_eq!(basis.allocated_high_water, 3);
        }
        let old = fixture::body(1, 1);
        let digest = sage_db_format::digest::raw_sha256(&old);
        assert_eq!(
            reopened
                .read_committed_blob(digest, old.len() as u64, &mut ledger())
                .expect("superseded body survives reopen")
                .as_bytes(),
            old,
        );
        let OutcomeLookup::Found {
            original_version, ..
        } = reopened.outcome(fixture::PRINCIPAL, fixture::request(version))
        else {
            panic!("selected version's outcome survives reopen");
        };
        assert_eq!(original_version, version);
        eprintln!("BACKUP_REOPEN version={version} files={}", report.files);
    }
    assert_eq!(source.head(), Some(3));
}
