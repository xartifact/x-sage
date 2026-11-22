//! P1-6 test-model: durable-state restoration and real exclusive-lock modelling.
//!
//! These tests exercise the TEST HARNESS, not the product. Two claims are separated deliberately:
//!
//! 1. **Restoration is exact.** A sampled [`DurableState`] rebuilt through the constructors yields a model whose visible
//!    AND durable namespace equals the sample byte-for-byte, with nothing dirty and no carried-over handle/fault/lock.
//! 2. **Exclusive locking is modelled.** A second `try_lock_exclusive` on a held root refuses with `WouldBlock` and no
//!    effect; dropping the guard releases it; a freshly restored backend starts with no prior live lock.
//!
//! Neither claim says anything about OS `flock` behaviour, device durability, or power loss. The lock is a test-model
//! lock over a virtual root, and restoration is crash-STATE transport over a simulated backend.

use std::path::Path;

use sage_db_store::fs::{CreateDir, FileSystem, FsErrorKind};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_store_tests::model::{DurableState, ModelFs, RelPath, RestoreError};

/// A root path used as harness context. Absolute, and never touched on the real filesystem.
const VROOT: &str = "/p16-restore";

fn rel(path: &str) -> RelPath {
    let mut out = RelPath::root();
    for part in path.split('/') {
        out = RelPath::join(&out, part);
    }
    out
}

/// A small but structurally complete KB-shaped sample: root, two families, nested dirs, and files at several depths.
fn sample() -> DurableState {
    let mut state = DurableState::default();
    state.dirs.insert(RelPath::root());
    for dir in [
        "sage-db",
        "sage-db/manifests",
        "sage-db/transactions",
        "sage-db/staging",
    ] {
        state.dirs.insert(rel(dir));
    }
    state
        .files
        .insert(rel("sage-db/CURRENT"), b"current-bytes".to_vec());
    state
        .files
        .insert(rel("sage-db/manifests/v1-a.sgm"), b"manifest-one".to_vec());
    state.files.insert(
        rel("sage-db/manifests/v2-b.sgm"),
        b"manifest-two-longer".to_vec(),
    );
    state
        .files
        .insert(rel("sage-db/transactions/tx-a.sgl"), b"tx-bytes".to_vec());
    // A truly empty file is a legal sample value and must survive restoration as empty, not vanish.
    state.files.insert(rel("sage-db/staging/empty"), Vec::new());
    state
}

/// The restored model's observed namespace and bytes, collected through the model's own accessors.
fn observe(model: &ModelFs) -> DurableState {
    // Walk from the root using the model's public, non-collecting accessors so the observation comes from the
    // model's tables rather than from the input sample.
    let mut dirs = std::collections::BTreeSet::new();
    let mut files = std::collections::BTreeMap::new();
    let mut frontier = vec![RelPath::root()];
    while let Some(path) = frontier.pop() {
        if !model.has_dir(&path) {
            continue;
        }
        dirs.insert(path.clone());
        let count = model.entry_len(&path);
        for index in 0..count {
            let Some(name) = model.entry_name_at(&path, index) else {
                panic!("index below entry_len must resolve");
            };
            let child = RelPath::join(&path, &name);
            match model.inode_of(&path, &name) {
                Some(ino) => {
                    let bytes = model
                        .visible_bytes(ino)
                        .expect("bound file inode has visible bytes")
                        .to_vec();
                    files.insert(child, bytes);
                }
                None => frontier.push(child),
            }
        }
    }
    DurableState { files, dirs }
}

/// Restoration is byte-exact in BOTH namespaces and leaves nothing unflushed.
#[test]
fn restore_reproduces_the_sampled_state_exactly() {
    let state = sample();
    let model = match ModelFs::from_durable_state(state.clone()) {
        Ok(model) => model,
        Err(error) => panic!("a structurally valid sample must restore: {error}"),
    };

    assert_eq!(
        observe(&model),
        state,
        "restored visible namespace and bytes must equal the sample exactly"
    );

    // The durable namespace must equal the sample too: a restored crash state is already "on disk".
    let mut durable_files = std::collections::BTreeMap::new();
    let mut frontier = vec![RelPath::root()];
    while let Some(path) = frontier.pop() {
        let count = model.entry_len(&path);
        for index in 0..count {
            let name = model.entry_name_at(&path, index).expect("resolves");
            let child = RelPath::join(&path, &name);
            match model.inode_of(&path, &name) {
                Some(ino) => {
                    durable_files.insert(
                        child,
                        model
                            .durable_bytes(ino)
                            .expect("restored inode has durable bytes")
                            .to_vec(),
                    );
                }
                None => frontier.push(child),
            }
        }
    }
    assert_eq!(
        durable_files, state.files,
        "restored DURABLE bytes must equal the sample exactly"
    );

    assert!(
        !model.has_unflushed(),
        "a restored state must have nothing pending: no dirty inode or directory may be inherited"
    );
}

/// The empty file survives as empty, and no family-completeness assumption is imposed.
#[test]
fn restore_preserves_empty_files_and_partial_family_sets() {
    let mut state = DurableState::default();
    state.dirs.insert(RelPath::root());
    state.dirs.insert(rel("only-one-family"));
    state.files.insert(rel("only-one-family/zero"), Vec::new());

    let model = match ModelFs::from_durable_state(state.clone()) {
        Ok(model) => model,
        // A crash may legally leave only some families; completeness must NOT be required.
        Err(error) => panic!("a partial family set is a legal crash state: {error}"),
    };
    assert_eq!(
        observe(&model),
        state,
        "empty file must stay present and empty"
    );
}

/// Structurally impossible samples are refused with a specific typed error, never repaired.
#[test]
fn restore_refuses_impossible_samples_with_typed_errors() {
    // No root: nothing is reachable.
    let mut no_root = DurableState::default();
    no_root.files.insert(rel("orphan"), b"x".to_vec());
    assert_eq!(
        ModelFs::from_durable_state(no_root).map(|_| ()),
        Err(RestoreError::MissingRoot),
        "a sample without its root must be refused, not rooted implicitly"
    );

    // A file whose parent did not survive.
    let mut orphan_file = DurableState::default();
    orphan_file.dirs.insert(RelPath::root());
    orphan_file
        .files
        .insert(rel("missing-dir/file"), b"x".to_vec());
    assert_eq!(
        ModelFs::from_durable_state(orphan_file).map(|_| ()),
        Err(RestoreError::ParentMissing {
            path: rel("missing-dir/file")
        }),
        "an orphaned file must be refused, not re-parented or dropped"
    );

    // A directory whose own parent did not survive.
    let mut orphan_dir = DurableState::default();
    orphan_dir.dirs.insert(RelPath::root());
    orphan_dir.dirs.insert(rel("missing-dir/child"));
    assert_eq!(
        ModelFs::from_durable_state(orphan_dir).map(|_| ()),
        Err(RestoreError::DirParentMissing {
            path: rel("missing-dir/child")
        }),
        "an orphaned directory must be refused"
    );

    // One path claiming to be both.
    let mut collision = DurableState::default();
    collision.dirs.insert(RelPath::root());
    collision.dirs.insert(rel("both"));
    collision.files.insert(rel("both"), b"x".to_vec());
    assert_eq!(
        ModelFs::from_durable_state(collision).map(|_| ()),
        Err(RestoreError::FileDirCollision { path: rel("both") }),
        "a file/directory collision must be refused"
    );
}

/// The filesystem constructor binds the explicit root, and a relative context is refused.
#[test]
fn filesystem_restore_binds_an_explicit_absolute_root() {
    let caps = ModelCapabilities::full();

    // Relative context is ambiguous: refused before any state is constructed.
    assert_eq!(
        ModelFileSystem::from_durable_state(caps.clone(), Path::new("relative/root"), sample())
            .map(|_| ()),
        Err(RestoreError::RootNotAbsolute)
    );

    let backend = match ModelFileSystem::from_durable_state(caps, Path::new(VROOT), sample()) {
        Ok(backend) => backend,
        Err(error) => panic!("an absolute context must restore: {error}"),
    };

    // The existing restored root opens WITHOUT create_root: the registry entry was installed for it.
    let root = match backend.open_root(Path::new(VROOT)) {
        Ok(root) => root,
        Err(error) => panic!("a restored root must open without creation: {error:?}"),
    };
    // An unknown path must NOT resolve: there is exactly one registered root, no defaulting.
    assert!(
        backend.open_root(Path::new("/somewhere/else")).is_err(),
        "no path defaulting: an unregistered root must not open"
    );
    // And opening with create semantics is not needed for a restored directory.
    assert!(
        backend.open_root(Path::new(VROOT)).is_ok(),
        "opening is repeatable for a registered root"
    );
    let _ = (root, CreateDir::Never);
}

/// Two-sided real lock contention: the second attempt refuses with no effect, release makes it available.
#[test]
fn exclusive_lock_refuses_second_holder_then_releases_on_drop() {
    let backend = match ModelFileSystem::from_durable_state(
        ModelCapabilities::full(),
        Path::new(VROOT),
        sample(),
    ) {
        Ok(backend) => backend,
        Err(error) => panic!("restore must succeed: {error}"),
    };
    let root = match backend.open_root(Path::new(VROOT)) {
        Ok(root) => root,
        Err(error) => panic!("root must open: {error:?}"),
    };

    // Side 1: the first lock is granted.
    let first = match backend.try_lock_exclusive(&root) {
        Ok(lock) => lock,
        Err(error) => panic!("first lock must be granted: {error:?}"),
    };
    assert_eq!(first.root(), Path::new(VROOT));

    // Side 2: while it is alive, a second attempt refuses with WouldBlock and NO effect.
    match backend.try_lock_exclusive(&root) {
        Err(error) => assert_eq!(
            error.kind(),
            FsErrorKind::WouldBlock,
            "contention must be reported as WouldBlock, not as a generic I/O failure"
        ),
        Ok(_) => panic!("a second exclusive lock must not be granted while the first is held"),
    }

    // Release on Drop: only now may a new caller acquire it.
    drop(first);
    let second = match backend.try_lock_exclusive(&root) {
        Ok(lock) => lock,
        Err(error) => panic!("after release the root must be lockable again: {error:?}"),
    };
    assert!(
        second.id() > 0,
        "a fresh acquisition is a distinct lock identity, not a resurrected handle"
    );
}

/// A restarted backend starts with no surviving lock, and is still genuinely exclusive within itself.
///
/// The registry is per-backend, so this models ONE process's view of its own locks. It does NOT model cross-process
/// exclusion: two `ModelFileSystem` instances are two processes, and a real OS lock would contend across them while this
/// model deliberately does not. That limitation is stated here rather than papered over by asserting exclusion it cannot
/// provide.
#[test]
fn restored_backend_starts_with_no_prior_live_lock() {
    // A "process" that locks the root and then dies at the end of this block.
    {
        let live = match ModelFileSystem::from_durable_state(
            ModelCapabilities::full(),
            Path::new(VROOT),
            sample(),
        ) {
            Ok(backend) => backend,
            Err(error) => panic!("restore must succeed: {error}"),
        };
        let live_root = match live.open_root(Path::new(VROOT)) {
            Ok(root) => root,
            Err(error) => panic!("root must open: {error:?}"),
        };
        let _held = match live.try_lock_exclusive(&live_root) {
            Ok(lock) => lock,
            Err(error) => panic!("lock must be granted: {error:?}"),
        };
    }

    // The restart over the same sampled state inherits no lock at all.
    let restarted = match ModelFileSystem::from_durable_state(
        ModelCapabilities::full(),
        Path::new(VROOT),
        sample(),
    ) {
        Ok(backend) => backend,
        Err(error) => panic!("restore must succeed: {error}"),
    };
    let root = match restarted.open_root(Path::new(VROOT)) {
        Ok(root) => root,
        Err(error) => panic!("restored root must open: {error:?}"),
    };
    let first = match restarted.try_lock_exclusive(&root) {
        Ok(lock) => lock,
        Err(error) => panic!("a restarted backend must acquire its own root lock: {error:?}"),
    };
    // Within THIS backend the lock is genuinely exclusive, which is the property the model can actually provide.
    match restarted.try_lock_exclusive(&root) {
        Err(error) => assert_eq!(
            error.kind(),
            FsErrorKind::WouldBlock,
            "within one backend a restored root must be genuinely exclusive"
        ),
        Ok(_) => panic!("a backend must not hand out the same root twice"),
    }
    drop(first);
    assert!(
        restarted.try_lock_exclusive(&root).is_ok(),
        "release must make the restored root lockable again"
    );
}
