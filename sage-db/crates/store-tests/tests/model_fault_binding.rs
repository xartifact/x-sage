//! P1-6 test-model: declared fault injections must reach the REAL seam operation.
//!
//! A `FaultSchedule` that is declared but never consumed by the operation it names is a vacuous test: the scenario
//! claims to exercise a fault, the injection never fires, and the assertion passes on the no-fault path. Seven seam
//! methods (`create_new`, `sync_entry`, `sync_parent_of`, `read_exact_into`, `open_read`, `hash_bounded`, `remove`)
//! previously declared points with no consumer.
//!
//! These tests are DIRECT BINDING regressions: each declares exactly one injection at one point, performs that one real
//! seam operation on genuine fixture bytes/handles, and asserts
//!
//! * the declared injection actually fired,
//! * the returned error carries the injection's real class/effect (typed, not a lookalike),
//! * and, for the mutating/seam operations, that the namespace and bytes are UNCHANGED (no-effect) or that the real
//!   operation still behaves normally when nothing is injected.
//!
//! Typed `Budget`/`Cancel` injections here are TRANSPORT fixtures at the seam. They are not evidence about a real
//! caller's allowance or a real control policy; that remains P1-5 real-ledger scope.

use std::path::Path;

use sage_db_store::fs::{CreateDir, Effect, Family, FileSystem, FsErrorKind, HashReport};
use sage_db_store::names::{EntryName, Id128};
use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
use sage_db_store_tests::model::fs_model::{DirHandle, ModelCapabilities, ModelFileSystem};
use sage_db_store_tests::model::{DurableState, RelPath};

const VROOT: &str = "/p16-binding";

fn control() -> &'static sage_db_types::query::RunToCompletion {
    Box::leak(Box::new(sage_db_types::query::RunToCompletion))
}

fn rel(path: &str) -> RelPath {
    let mut out = RelPath::root();
    for part in path.split('/') {
        out = RelPath::join(&out, part);
    }
    out
}

/// A fixed 128-bit identity, so fixture names are byte-stable across runs.
fn id(seed: u8) -> Id128 {
    Id128::from_bytes([seed; 16])
}

/// The typed transaction entry name; its rendered form is the file name on disk.
fn tx_entry(seed: u8) -> EntryName {
    EntryName::Transaction(id(seed))
}

/// A restored backend over a small committed-looking layout, with a root that opens without creation.
fn backend_with(files: Vec<(&str, Vec<u8>)>) -> (ModelFileSystem, DirHandle) {
    let mut state = DurableState::default();
    state.dirs.insert(RelPath::root());
    for family in [
        Family::Manifests,
        Family::Transactions,
        Family::Blobs,
        Family::Staging,
        Family::Checkpoints,
        Family::Cache,
    ] {
        // Families live directly under the modelled root: `open_family` appends the family name to the root path.
        state.dirs.insert(rel(family.as_str()));
    }
    for (path, bytes) in files {
        state.files.insert(rel(path), bytes);
    }
    let backend = match ModelFileSystem::from_durable_state(
        ModelCapabilities::full(),
        Path::new(VROOT),
        state,
    ) {
        Ok(backend) => backend,
        Err(error) => panic!("restore must succeed: {error}"),
    };
    let root = match backend.open_root(Path::new(VROOT)) {
        Ok(root) => root,
        Err(error) => panic!("root must open: {error:?}"),
    };
    (backend, root)
}

/// The model-relative file path for an entry in `family`, using the seam's own file-name rendering.
fn file_path(family: Family, name: EntryName) -> String {
    format!("{}/{}", family.as_str(), name.to_file_name())
}

/// Whether the restored model currently binds `name` in `dir`.
fn model_has_entry(backend: &ModelFileSystem, dir: &DirHandle, name: EntryName) -> bool {
    let path = backend.dir_path(*dir).expect("open handle resolves");
    backend.model().has_entry(&path, &name.to_file_name())
}

/// Whether the restored model has anything unflushed, over its whole tree.
fn model_has_unflushed(backend: &ModelFileSystem) -> bool {
    backend.model().has_unflushed()
}

fn arm(backend: &ModelFileSystem, point: FaultPoint, injection: Injection) {
    *backend.faults_handle().borrow_mut() = FaultSchedule::none().at(point, injection, 1);
}

/// `create_new`: a declared refusal must fire and leave the namespace untouched.
#[test]
fn create_new_is_bound_before_the_effect() {
    let (backend, root) = backend_with(Vec::new());
    let staging = match backend.open_family(&root, Family::Staging, CreateDir::Never) {
        Ok(dir) => dir,
        Err(error) => panic!("staging must exist: {error:?}"),
    };
    let name = tx_entry(0x11);

    arm(&backend, FaultPoint::CreateNew, Injection::FailNoEffect);
    match backend.create_new(&staging, name) {
        Err(error) => {
            assert_eq!(error.kind(), FsErrorKind::Io(0));
            assert_eq!(error.effect(), Effect::NoEffect);
        }
        Ok(_) => panic!("the declared CreateNew fault must be refused"),
    }
    backend.assert_all_faults_fired();
    // No-effect claim: the name must not exist and nothing may be dirty.
    assert!(
        !model_has_entry(&backend, &staging, name),
        "a refused create_new must not bind the name"
    );
    assert!(
        !model_has_unflushed(&backend),
        "a refused create_new must not dirty the directory"
    );
}

/// `sync_entry`: a declared refusal fires and leaves the inode's durable bytes unchanged.
#[test]
fn sync_entry_is_bound_before_the_effect() {
    let name = tx_entry(0x11);
    let path = file_path(Family::Transactions, name);
    let (backend, root) = backend_with(vec![(&path, b"body".to_vec())]);
    let txs = match backend.open_family(&root, Family::Transactions, CreateDir::Never) {
        Ok(dir) => dir,
        Err(error) => panic!("transactions must exist: {error:?}"),
    };
    let tx_dir = rel("transactions");
    let file_name = name.to_file_name();
    let ino = backend
        .model()
        .inode_of(&tx_dir, &file_name)
        .expect("fixture file is bound");

    arm(&backend, FaultPoint::SyncEntry, Injection::FailNoEffect);
    match backend.sync_entry(&txs, name) {
        Err(error) => assert_eq!(error.effect(), Effect::NoEffect),
        Ok(()) => panic!("the declared SyncEntry fault must be refused"),
    }
    backend.assert_all_faults_fired();
    // The restored file was already durable; a refused sync must not change those bytes.
    assert_eq!(
        backend.model().durable_bytes(ino),
        Some(b"body".as_slice()),
        "a refused sync_entry must leave durable bytes unchanged"
    );
}

/// `sync_parent_of`: a declared refusal fires instead of reporting success.
#[test]
fn sync_parent_of_is_bound_before_the_effect() {
    let (backend, _root) = backend_with(Vec::new());
    arm(&backend, FaultPoint::SyncParentOf, Injection::FailNoEffect);
    match backend.sync_parent_of(Path::new(VROOT)) {
        Err(error) => assert_eq!(error.effect(), Effect::NoEffect),
        Ok(()) => panic!("the declared SyncParentOf fault must be refused"),
    }
    backend.assert_all_faults_fired();
    // With nothing injected the same call is a real success: the binding did not disable the operation.
    let (plain, _root) = backend_with(Vec::new());
    assert!(
        plain.sync_parent_of(Path::new(VROOT)).is_ok(),
        "the no-fault path must still succeed"
    );
}

/// `read_exact_into`: a refusal fires and copies ZERO bytes into the caller's buffer.
#[test]
fn read_exact_is_bound_before_copying() {
    let name = tx_entry(0x11);
    let path = file_path(Family::Transactions, name);
    let (backend, root) = backend_with(vec![(&path, b"abcdef".to_vec())]);
    let txs = match backend.open_family(&root, Family::Transactions, CreateDir::Never) {
        Ok(dir) => dir,
        Err(error) => panic!("transactions must exist: {error:?}"),
    };
    let name = tx_entry(0x11);

    let mut buf = [0xAAu8; 16];
    arm(&backend, FaultPoint::ReadExact, Injection::FailNoEffect);
    match backend.read_exact_into(&txs, name, &mut buf) {
        Err(error) => assert_eq!(error.effect(), Effect::NoEffect),
        Ok(_) => panic!("the declared ReadExact fault must be refused"),
    }
    backend.assert_all_faults_fired();
    assert_eq!(
        buf, [0xAAu8; 16],
        "a refused read must not have copied a single byte"
    );

    // The real read still works with nothing injected.
    let (plain, root2) = backend_with(vec![(&path, b"abcdef".to_vec())]);
    let txs2 = match plain.open_family(&root2, Family::Transactions, CreateDir::Never) {
        Ok(dir) => dir,
        Err(error) => panic!("transactions must exist: {error:?}"),
    };
    let mut ok_buf = [0u8; 16];
    assert_eq!(
        plain.read_exact_into(&txs2, name, &mut ok_buf),
        Ok(6),
        "the no-fault read must still return the real length"
    );
    assert_eq!(&ok_buf[..6], b"abcdef");
}

/// `open_read`: a refusal fires and hands out NO handle.
#[test]
fn open_read_is_bound_before_the_handle() {
    let name = tx_entry(0x11);
    let path = file_path(Family::Transactions, name);
    let (backend, root) = backend_with(vec![(&path, b"body".to_vec())]);
    let txs = match backend.open_family(&root, Family::Transactions, CreateDir::Never) {
        Ok(dir) => dir,
        Err(error) => panic!("transactions must exist: {error:?}"),
    };

    arm(&backend, FaultPoint::OpenRead, Injection::FailNoEffect);
    match backend.open_read(&txs, name) {
        Err(error) => assert_eq!(error.effect(), Effect::NoEffect),
        Ok(_) => panic!("the declared OpenRead fault must be refused"),
    }
    backend.assert_all_faults_fired();
}

/// `hash_bounded`: the refusal is the seam's own class, not a content verdict.
#[test]
fn hash_is_bound_before_admission_and_copy() {
    let name = tx_entry(0x11);
    let path = file_path(Family::Transactions, name);
    let (backend, root) = backend_with(vec![(&path, b"body".to_vec())]);
    let txs = match backend.open_family(&root, Family::Transactions, CreateDir::Never) {
        Ok(dir) => dir,
        Err(error) => panic!("transactions must exist: {error:?}"),
    };
    let mut ledger =
        sage_db_store::lease::AttemptLedger::open(64 * 1024 * 1024, 1_000_000, control())
            .expect("a finite ledger opens");

    // A typed seam Budget fixture: assert the exact canonical quantities survive the seam, and that this is a SEAM
    // fixture (the caller's own ledger is untouched by the injection).
    arm(
        &backend,
        FaultPoint::Hash,
        Injection::BudgetBytes {
            requested: 7,
            remaining: 3,
        },
    );
    match backend.hash_bounded(&txs, name, 1024, &mut ledger) {
        Err(error) => {
            let budget = error
                .budget()
                .expect("a seam budget fault must carry its canonical BudgetError payload");
            assert_eq!(budget.unit(), sage_db_types::ledger::BudgetUnit::Bytes);
            assert_eq!(budget.requested(), 7);
            assert_eq!(budget.remaining(), 3);
            assert_eq!(error.kind(), FsErrorKind::BudgetExceeded);
        }
        Ok(_) => panic!("the declared Hash fault must be refused"),
    }
    backend.assert_all_faults_fired();

    // Cancel is likewise a transport fixture with its own class.
    arm(&backend, FaultPoint::Hash, Injection::Cancel);
    match backend.hash_bounded(&txs, name, 1024, &mut ledger) {
        Err(error) => assert!(
            error.control() == Some(sage_db_types::query::ControlStop::Cancelled),
            "a seam cancel fault must stay a Control verdict: {error:?}"
        ),
        Ok(_) => panic!("the declared Cancel fault must be refused"),
    }
    backend.assert_all_faults_fired();

    // And the real hash still computes over the real bytes when nothing is injected.
    let (plain, root2) = backend_with(vec![(&path, b"body".to_vec())]);
    let txs2 = match plain.open_family(&root2, Family::Transactions, CreateDir::Never) {
        Ok(dir) => dir,
        Err(error) => panic!("transactions must exist: {error:?}"),
    };
    let mut plain_ledger =
        sage_db_store::lease::AttemptLedger::open(64 * 1024 * 1024, 1_000_000, control())
            .expect("a finite ledger opens");
    let report: HashReport = match plain.hash_bounded(&txs2, name, 1024, &mut plain_ledger) {
        Ok(report) => report,
        Err(error) => panic!("the no-fault hash must succeed: {error:?}"),
    };
    assert_eq!(report.len, 4, "the hash must cover the real file length");
}

/// `remove`: a refusal fires and the name is STILL bound afterwards.
#[test]
fn remove_is_bound_before_the_effect() {
    let name = tx_entry(0x11);
    let path = file_path(Family::Transactions, name);
    let (backend, root) = backend_with(vec![(&path, b"body".to_vec())]);
    let txs = match backend.open_family(&root, Family::Transactions, CreateDir::Never) {
        Ok(dir) => dir,
        Err(error) => panic!("transactions must exist: {error:?}"),
    };

    arm(&backend, FaultPoint::Remove, Injection::FailNoEffect);
    match backend.remove(&txs, name) {
        Err(error) => assert_eq!(error.effect(), Effect::NoEffect),
        Ok(()) => panic!("the declared Remove fault must be refused"),
    }
    backend.assert_all_faults_fired();
    assert!(
        model_has_entry(&backend, &txs, name),
        "a refused remove must leave the name bound"
    );

    // The real removal still works with nothing injected.
    let (plain, root2) = backend_with(vec![(&path, b"body".to_vec())]);
    let txs2 = match plain.open_family(&root2, Family::Transactions, CreateDir::Never) {
        Ok(dir) => dir,
        Err(error) => panic!("transactions must exist: {error:?}"),
    };
    assert!(plain.remove(&txs2, name).is_ok());
    assert!(
        !model_has_entry(&plain, &txs2, name),
        "the no-fault remove must really unbind the name"
    );
}
