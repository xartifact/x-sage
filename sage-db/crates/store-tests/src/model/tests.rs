//! Regression tests for the four model axes and the two counterexamples that caught real
//! defects during P1-1 contract review.
//!
//! These are **model** tests. They assert properties of the model itself: if a property here
//! fails, the model has become unable to discriminate the corresponding defect class, and every
//! later store test built on it is worthless. They say nothing about production code.

use super::{Entry, ModelFs, RelPath};

fn root() -> RelPath {
    RelPath::root()
}

/// Counterexample R-1: a directory's own binding must be durable for its contents to survive.
///
/// The earlier model derived directories from the files inside them, so forgetting
/// `sync_parent_of` was undetectable: every crash state still contained `kb/CURRENT`.
#[test]
fn missing_parent_sync_makes_the_whole_subtree_unreachable() {
    let mut fs = ModelFs::new();
    fs.create_dir(&root(), "kb");

    let ino = fs.create_new(&RelPath::join(&root(), "kb"), "CURRENT");
    fs.write(ino, b"root pointer");
    fs.sync_file(ino);
    // Contents of `kb` are durable...
    fs.sync_dir(&RelPath::join(&root(), "kb"));
    // ...but `kb`'s own binding in the root is NOT synced.

    let states = fs.crash_states();
    assert!(
        states.sampled > 1,
        "a pending parent binding must admit several states"
    );
    let lost = states
        .iter()
        .filter(|s| {
            s.read(&RelPath::join(&RelPath::join(&root(), "kb"), "CURRENT"))
                .is_none()
        })
        .count();
    assert!(
        lost > 0,
        "at least one legal state must lose kb/CURRENT when the parent binding is unsynced"
    );

    // And syncing the parent removes the loss entirely.
    fs.sync_parent_of(&RelPath::join(&root(), "kb"));
    let states = fs.crash_states();
    assert!(
        states.iter().all(|s| s
            .read(&RelPath::join(&RelPath::join(&root(), "kb"), "CURRENT"))
            .is_some()),
        "with the parent synced, every state must retain the entry"
    );
}

/// Counterexample R-2: `link` must not replace an existing name, and aliasing must share content.
///
/// The earlier model silently overwrote the destination, which would have hidden any defect
/// where a store published onto a live name.
#[test]
#[should_panic(expected = "AlreadyExists")]
fn link_to_an_existing_name_is_refused() {
    let mut fs = ModelFs::new();
    let a = fs.create_new(&root(), "a");
    fs.write(a, b"A");
    let b = fs.create_new(&root(), "b");
    fs.write(b, b"B");
    fs.link(&root(), "b", a);
}

#[test]
fn hardlink_aliases_share_content_in_both_directions() {
    let mut fs = ModelFs::new();
    let a = fs.create_new(&root(), "a");
    fs.write(a, b"A");
    fs.link(&root(), "c", a);

    // One inode, two names: the model has no per-name content, so a write through either name is
    // visible through both. `write` appends, mirroring the seam's position-based write.
    fs.write(a, b"2");
    assert_eq!(fs.visible_bytes(a), Some(b"A2".as_slice()));

    // Sync through the original name and both bindings still point at the same inode.
    fs.sync_file(a);
    fs.sync_dir(&root());
    let states = fs.crash_states();
    assert_eq!(states.sampled, 1);
    let state = states.iter().next().expect("one state");
    assert_eq!(
        state.read(&RelPath::join(&root(), "a")),
        Some(b"A2".as_slice())
    );
    assert_eq!(
        state.read(&RelPath::join(&root(), "c")),
        Some(b"A2".as_slice())
    );
}

/// Axis 1 vs axis 2: content and names become durable independently.
#[test]
fn content_and_name_durability_are_independent_axes() {
    let mut fs = ModelFs::new();
    let ino = fs.create_new(&root(), "f");
    fs.write(ino, b"data");

    // Sync content only: the name may still vanish.
    fs.sync_file(ino);
    let states = fs.crash_states();
    assert!(
        states
            .iter()
            .any(|s| s.read(&RelPath::join(&root(), "f")).is_none()),
        "an unsynced name may be absent even though the content is durable"
    );

    // Sync the name too: no state may lose it, and the bytes must be the synced ones.
    fs.sync_dir(&root());
    let states = fs.crash_states();
    assert!(
        states
            .iter()
            .all(|s| s.read(&RelPath::join(&root(), "f")) == Some(b"data".as_slice())),
        "after syncing both axes every state must expose the file with its content"
    );
}

/// A crash state must never expose bytes that were never written, nor resurrect a removed name.
#[test]
fn states_only_contain_bytes_that_were_actually_written() {
    let mut fs = ModelFs::new();
    let ino = fs.create_new(&root(), "f");
    fs.write(ino, b"v1");
    fs.sync_file(ino);
    fs.sync_dir(&root());

    fs.remove(&root(), "f");
    let states = fs.crash_states();
    // Removal is pending until the directory is synced, so both outcomes are legal.
    let present = states
        .iter()
        .filter(|s| s.read(&RelPath::join(&root(), "f")).is_some())
        .count();
    assert!(present > 0, "an unsynced removal may not have taken effect");
    for state in states.iter() {
        if let Some(bytes) = state.read(&RelPath::join(&root(), "f")) {
            assert_eq!(bytes, b"v1", "no state may expose bytes nobody wrote");
        }
    }
}

/// `replace_current` is the only overwrite, and either target is legal after a crash.
#[test]
fn replace_current_can_leave_either_target() {
    let mut fs = ModelFs::new();
    let old = fs.create_new(&root(), "CURRENT");
    fs.write(old, b"old");
    fs.sync_file(old);
    fs.sync_dir(&root());

    let staged = fs.create_new(&root(), "CURRENT.tmp");
    fs.write(staged, b"new");
    fs.sync_file(staged);
    fs.replace_current(&root(), "CURRENT.tmp", "CURRENT");

    let states = fs.crash_states();
    let mut saw_old = false;
    let mut saw_new = false;
    for state in states.iter() {
        match state
            .read(&RelPath::join(&root(), "CURRENT"))
            .map(<[u8]>::to_vec)
        {
            Some(b) if b == b"old" => saw_old = true,
            Some(b) if b == b"new" => saw_new = true,
            Some(other) => panic!("unexpected CURRENT content: {other:?}"),
            None => panic!("CURRENT must always be present in this scenario"),
        }
    }
    assert!(saw_old, "the pre-rename target is a legal post-crash state");
    assert!(
        saw_new,
        "the post-rename target is a legal post-crash state"
    );
}

/// Directory bindings are per directory: syncing one must not launder another.
#[test]
fn syncing_one_directory_does_not_sync_another() {
    let mut fs = ModelFs::new();
    fs.create_dir(&root(), "a");
    fs.create_dir(&root(), "b");
    fs.sync_dir(&root());

    let in_a = fs.create_new(&RelPath::join(&root(), "a"), "f");
    fs.write(in_a, b"x");
    fs.sync_file(in_a);
    let in_b = fs.create_new(&RelPath::join(&root(), "b"), "f");
    fs.write(in_b, b"y");
    fs.sync_file(in_b);

    fs.sync_dir(&RelPath::join(&root(), "a"));
    let states = fs.crash_states();
    for state in states.iter() {
        assert_eq!(
            state.read(&RelPath::join(&RelPath::join(&root(), "a"), "f")),
            Some(b"x".as_slice()),
            "synced directory must keep its entry in every state"
        );
    }
    assert!(
        states.iter().any(|s| s
            .read(&RelPath::join(&RelPath::join(&root(), "b"), "f"))
            .is_none()),
        "the unsynced sibling directory must still be able to lose its entry"
    );
}

/// A fresh model with nothing unflushed admits exactly one state.
#[test]
fn no_pending_writes_means_one_state() {
    let mut fs = ModelFs::new();
    let ino = fs.create_new(&root(), "f");
    fs.write(ino, b"stable");
    fs.sync_file(ino);
    fs.sync_dir(&root());
    assert!(!fs.has_unflushed());

    let states = fs.crash_states();
    assert_eq!(states.sampled, 1);
    assert_eq!(
        states
            .iter()
            .next()
            .and_then(|s| s.read(&RelPath::join(&root(), "f"))),
        Some(b"stable".as_slice())
    );
}

/// The model must record that directories exist as entries, not merely as file containers.
#[test]
fn empty_directory_survives_only_if_its_binding_is_durable() {
    let mut fs = ModelFs::new();
    fs.create_dir(&root(), "blobs");
    let states = fs.crash_states();
    assert!(
        states
            .iter()
            .any(|s| !s.has_dir(&RelPath::join(&root(), "blobs"))),
        "an unsynced empty directory may be absent"
    );

    fs.sync_dir(&root());
    let states = fs.crash_states();
    assert!(
        states
            .iter()
            .all(|s| s.has_dir(&RelPath::join(&root(), "blobs"))),
        "a synced empty directory must survive; nothing else records its existence"
    );
}

/// Binding tables must not be shared between directories by accident.
#[test]
fn directory_tables_are_per_directory() {
    let mut fs = ModelFs::new();
    fs.create_dir(&root(), "d1");
    let ino = fs.create_new(&RelPath::join(&root(), "d1"), "f");
    fs.write(ino, b"1");
    fs.sync_file(ino);
    fs.sync_dir(&RelPath::join(&root(), "d1"));
    fs.sync_dir(&root());

    let states = fs.crash_states();
    assert_eq!(states.sampled, 1);
    let state = states.iter().next().expect("one state");
    assert_eq!(
        state.read(&RelPath::join(&RelPath::join(&root(), "d1"), "f")),
        Some(b"1".as_slice())
    );
    assert!(!state.files.contains_key(&RelPath::join(&root(), "f")));
}

/// The model refuses to bind a name twice, so it cannot describe a state the seam cannot produce.
#[test]
#[should_panic(expected = "AlreadyExists")]
fn create_new_refuses_an_existing_name() {
    let mut fs = ModelFs::new();
    fs.create_new(&root(), "f");
    fs.create_new(&root(), "f");
}

/// A path component containing `/` would alias two logical paths onto one string.
#[test]
#[should_panic(expected = "component must not contain")]
fn path_components_must_not_contain_separators() {
    let _ = RelPath::join(&root(), "a/b");
}

/// Root has no parent and no name; guards the reachability walk against a root self-parent.
#[test]
fn root_has_no_parent_or_name() {
    let root = RelPath::root();
    assert!(root.is_root());
    assert_eq!(root.parent(), None);
    assert_eq!(root.name(), None);
}

/// `Entry` ordering must be total so directory tables compare deterministically.
#[test]
fn entry_ordering_is_total_and_deterministic() {
    let mut entries = [Entry::Dir, Entry::File(2), Entry::File(1)];
    entries.sort();
    assert_eq!(entries, [Entry::File(1), Entry::File(2), Entry::Dir]);
}
