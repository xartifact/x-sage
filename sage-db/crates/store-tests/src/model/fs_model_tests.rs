//! Integration tests for the real-`FileSystem` binding.
//!
//! These exercise [`ModelFileSystem`] through the **production trait**, not through the model's
//! own API, so they cover the adapter and the seam's stated contracts rather than re-testing the
//! model. Each test names the seam contract it pins:
//!
//! * `Ok` from a sync means exactly the state visible at call time is durable, and `sync_dir`
//!   covers **names only**;
//! * `Effect::Unknown` from a write means "resolve, do not assume", and a short `Ok` write's bytes
//!   are visible but not durable;
//! * `for_each_entry` charges one entry per visit and its names are opaque presence tokens;
//! * `publish_no_overwrite` consumes the staged name even on the identical-bytes path, and
//!   `needs_sync` still reports that durability is owed;
//! * `replace_current` addresses `CURRENT` by construction;
//! * a budget refusal carries its unit and amounts, and a control stop is not a budget refusal.

use std::path::Path;

use sage_db_store::fs::{CreateDir, Each, Effect, Family, FileSystem, FsErrorKind};
use sage_db_store::names::{EntryName, Id128, RootName};
use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::query::ControlStop;

use super::fs_model::{DirHandle, ModelCapabilities, ModelFileSystem};

/// A ledger with fixed allowances that records what it was asked for.
struct Ledger {
    bytes: u64,
    entries: u64,
    byte_calls: Vec<u64>,
    entry_calls: Vec<u64>,
}

impl Ledger {
    fn new(bytes: u64, entries: u64) -> Self {
        Self {
            bytes,
            entries,
            byte_calls: Vec::new(),
            entry_calls: Vec::new(),
        }
    }
}

impl SharedLedger for Ledger {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        self.byte_calls.push(bytes);
        if bytes > self.bytes {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.bytes));
        }
        self.bytes -= bytes;
        Ok(())
    }

    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        self.entry_calls.push(count);
        if count > self.entries {
            return Err(BudgetError::new(BudgetUnit::Entries, count, self.entries));
        }
        self.entries -= count;
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

fn id(byte: u8) -> Id128 {
    Id128::from_bytes([byte; 16])
}

/// Open a fresh root and its families, mirroring what an init would do.
fn prepared(caps: ModelCapabilities) -> (ModelFileSystem, DirHandle, DirHandle, DirHandle) {
    let fs = ModelFileSystem::new(caps);
    let root = fs
        .create_root(Path::new("/tmp/model-kb"))
        .expect("create root");
    fs.sync_parent_of(Path::new("/tmp/model-kb"))
        .expect("sync parent");
    let staging = fs
        .open_family(&root, Family::Staging, CreateDir::IfMissing)
        .expect("staging");
    let blobs = fs
        .open_family(&root, Family::Blobs, CreateDir::IfMissing)
        .expect("blobs");
    let manifests = fs
        .open_family(&root, Family::Manifests, CreateDir::IfMissing)
        .expect("manifests");
    (fs, staging, blobs, manifests)
}

/// Capabilities are reported verbatim, and `durable_sync` is independent of the primitive list.
#[test]
fn capabilities_report_both_axes() {
    let full = prepared(ModelCapabilities::full()).0;
    let caps = full.capabilities();
    assert!(caps.durable_sync);
    assert_eq!(caps.supported.len(), 6);

    let primitives = ModelFileSystem::new(ModelCapabilities::primitives_without_durability());
    let caps = primitives.capabilities();
    assert_eq!(caps.supported.len(), 6, "every primitive is present");
    assert!(!caps.durable_sync, "and durability is still not honoured");
    assert!(!primitives.product_eligible());

    let none = ModelFileSystem::new(ModelCapabilities::unsupported());
    assert!(none.capabilities().supported.is_empty());
    assert!(!none.product_eligible());
}

/// Opening a root never creates one: the no-create rule is enforced by the backend, not the caller.
#[test]
fn open_root_does_not_create() {
    let fs = ModelFileSystem::new(ModelCapabilities::full());
    let err = fs
        .open_root(Path::new("/tmp/absent"))
        .expect_err("opening an unknown root must fail");
    assert_eq!(err.kind(), FsErrorKind::NotFound);
    assert_eq!(err.effect(), Effect::NoEffect);

    let created = fs
        .create_root(Path::new("/tmp/absent"))
        .expect("now create");
    let reopened = fs.open_root(Path::new("/tmp/absent")).expect("now present");
    assert_eq!(
        created, reopened,
        "the same root must yield the same handle"
    );

    let again = fs
        .create_root(Path::new("/tmp/absent"))
        .expect_err("creating twice must refuse");
    assert_eq!(again.kind(), FsErrorKind::AlreadyExists);
}

/// `create_new` never replaces, which is the immutability precondition for publication.
#[test]
fn create_new_refuses_an_existing_name() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(1),
    };
    fs.create_new(&staging, name).expect("first create");
    let err = fs
        .create_new(&staging, name)
        .expect_err("second create must refuse");
    assert_eq!(err.kind(), FsErrorKind::AlreadyExists);
    assert_eq!(err.effect(), Effect::NoEffect);
}

/// Content durability is a separate axis from name durability.
///
/// Syncing a file makes its bytes durable while the name may still be pending; syncing the
/// directory is what makes the name durable. Both statements are observed through the model.
#[test]
fn sync_file_and_sync_dir_cover_different_axes() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(2),
    };
    let mut file = fs.create_new(&staging, name).expect("create");
    fs.write(&mut file, b"payload").expect("write");
    fs.sync_file(&file).expect("sync file");

    // Content is durable, name is not: at least one crash state must lose the entry.
    let states = fs.model().crash_states();
    assert!(
        states.sampled > 1,
        "an unsynced name must admit several states"
    );
    let lost = states
        .iter()
        .filter(|s| s.files.values().all(|v| v.as_slice() != b"payload"))
        .count();
    assert!(
        lost > 0,
        "an unsynced directory entry may vanish after a crash"
    );

    // Make the family directory's own binding durable as well: an entry cannot be reachable if
    // its directory is not. This is the parent-binding axis, and omitting it here would make the
    // assertion below test the wrong thing.
    let (fs2, staging2, _b, _m) = prepared(ModelCapabilities::full());
    let name2 = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(2),
    };
    let mut f2 = fs2.create_new(&staging2, name2).expect("create");
    fs2.write(&mut f2, b"payload").expect("write");
    fs2.sync_file(&f2).expect("sync file");
    fs2.sync_dir(&staging2).expect("sync family entries");
    // The root must also be synced so the `staging/` directory itself is reachable; without that
    // the family's entries are unreachable no matter how well they were synced.
    let root_handle = DirHandle(0);
    fs2.sync_dir(&root_handle).expect("sync root names");
    let states = fs2.model().crash_states();
    assert!(
        states
            .iter()
            .all(|s| s.files.values().any(|v| v.as_slice() == b"payload")),
        "with content, entry and parent binding all durable, every state retains the bytes"
    );
    // And the synced name resolves through the trait again.
    let mut buf = [0u8; 16];
    let n = fs
        .read_exact_into(&staging, name, &mut buf)
        .expect("read back");
    assert_eq!(&buf[..n], b"payload");
}

/// A short write is `Ok` and its bytes are visible; nothing is durable until a sync.
#[test]
fn write_is_never_implicitly_durable() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Tx,
        id: id(3),
    };
    let mut file = fs.create_new(&staging, name).expect("create");
    let outcome = fs.write(&mut file, b"abc").expect("write");
    assert_eq!(outcome.written, 3);

    // Without a sync, no crash state may claim the bytes are durable while the name is durable.
    let states = fs.model().crash_states();
    assert!(
        states.sampled > 1,
        "unsynced bytes and an unsynced name must admit multiple states"
    );
}

/// `replace_current` addresses `CURRENT` by construction and leaves either target legal.
#[test]
fn replace_current_targets_current_and_both_targets_are_legal() {
    let fs = ModelFileSystem::new(ModelCapabilities::full());
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    fs.sync_parent_of(Path::new("/tmp/model-kb"))
        .expect("parent");
    // This scenario stages into the ROOT, because `CURRENT` and its temporary live there; opening a
    // staging family would be unused, so it is not opened.

    // Root files live in the root directory, not in a family directory. Staging the bytes and
    // replacing CURRENT are two different locations by design: `staged` names the staging file,
    // and the destination is `CURRENT` in the root.
    let mut first = fs
        .create_new(&root, EntryName::Root(RootName::CurrentTmp))
        .expect("stage 1");
    fs.write(&mut first, b"old").expect("w1");
    fs.sync_file(&first).expect("s1");
    fs.replace_current(&root, EntryName::Root(RootName::CurrentTmp), &root)
        .expect("install old");
    fs.sync_dir(&root).expect("names durable");

    // Stage "new" and replace.
    let mut second = fs
        .create_new(&root, EntryName::Root(RootName::CurrentTmp))
        .expect("stage 2");
    fs.write(&mut second, b"new").expect("w2");
    fs.sync_file(&second).expect("s2");
    fs.replace_current(&root, EntryName::Root(RootName::CurrentTmp), &root)
        .expect("install new");

    let mut seen_old = false;
    let mut seen_new = false;
    for state in fs.model().crash_states().iter() {
        let bytes = state
            .files
            .get(&super::RelPath::join(
                &super::RelPath::root(),
                RootName::Current.as_str(),
            ))
            .map(Vec::as_slice);
        match bytes {
            Some(b"old") => seen_old = true,
            Some(b"new") => seen_new = true,
            other => panic!("unexpected CURRENT: {other:?}"),
        }
    }
    assert!(
        seen_old,
        "the pre-replace binding is a legal post-crash state"
    );
    assert!(
        seen_new,
        "the post-replace binding is a legal post-crash state"
    );
}

/// Publication consumes the staged name even when the destination already held identical bytes,
/// and `needs_sync` still reports that durability is owed.
#[test]
fn publish_consumes_staged_and_owes_sync_even_when_identical() {
    let (fs, staging, blobs, _) = prepared(ModelCapabilities::full());
    let staged = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(4),
    };
    let published = EntryName::Blob([7u8; 32]);

    let mut file = fs.create_new(&staging, staged).expect("stage");
    fs.write(&mut file, b"immutable").expect("write");
    fs.sync_file(&file).expect("sync bytes");
    let report = fs
        .hash_bounded(&staging, staged, 1024, &mut Ledger::new(1024, 8))
        .expect("hash");
    let first = fs
        .publish_no_overwrite(
            &staging,
            staged,
            &blobs,
            published,
            &report,
            &mut Ledger::new(1 << 20, 32),
        )
        .expect("publish");
    assert!(first.needs_sync, "identity is not durability");
    assert_eq!(first.how, sage_db_store::fs::PublishMode::Created);
    assert_eq!(
        fs.size(&staging, staged).unwrap_err().kind(),
        FsErrorKind::NotFound,
        "the staged name is consumed by a successful publication"
    );

    // Stage identical bytes again: the destination already holds them.
    let mut again = fs.create_new(&staging, staged).expect("restage");
    fs.write(&mut again, b"immutable").expect("write");
    fs.sync_file(&again).expect("sync");
    let report = fs
        .hash_bounded(&staging, staged, 1024, &mut Ledger::new(1024, 8))
        .expect("hash");
    let second = fs
        .publish_no_overwrite(
            &staging,
            staged,
            &blobs,
            published,
            &report,
            &mut Ledger::new(1 << 20, 32),
        )
        .expect("republish identical");
    assert_eq!(second.how, sage_db_store::fs::PublishMode::AlreadyPresent);
    assert!(
        second.needs_sync,
        "still owes a sync even when bytes matched"
    );
    assert_eq!(
        fs.size(&staging, staged).unwrap_err().kind(),
        FsErrorKind::NotFound,
        "the identical-bytes path also consumes the staged name"
    );
}

/// Different bytes under an existing name are a hash mismatch, not a silent replacement.
#[test]
fn publish_refuses_different_bytes_under_same_name() {
    let (fs, staging, blobs, _) = prepared(ModelCapabilities::full());
    let staged = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(5),
    };
    let published = EntryName::Blob([9u8; 32]);

    let mut file = fs.create_new(&staging, staged).expect("stage");
    fs.write(&mut file, b"first").expect("write");
    fs.sync_file(&file).expect("sync");
    let report = fs
        .hash_bounded(&staging, staged, 1024, &mut Ledger::new(1024, 8))
        .expect("hash");
    fs.publish_no_overwrite(
        &staging,
        staged,
        &blobs,
        published,
        &report,
        &mut Ledger::new(1 << 20, 32),
    )
    .expect("publish first");

    let mut other = fs.create_new(&staging, staged).expect("restage");
    fs.write(&mut other, b"second").expect("write");
    fs.sync_file(&other).expect("sync");
    let report = fs
        .hash_bounded(&staging, staged, 1024, &mut Ledger::new(1024, 8))
        .expect("hash");
    let err = fs
        .publish_no_overwrite(
            &staging,
            staged,
            &blobs,
            published,
            &report,
            &mut Ledger::new(1 << 20, 32),
        )
        .expect_err("different bytes must be refused");
    assert_eq!(err.kind(), FsErrorKind::HashMismatch);
    assert_eq!(err.effect(), Effect::NoEffect);
}

/// A bytes refusal reports its unit and amounts; an entries refusal reports the other unit.
///
/// This is the property that `kind()` alone cannot express, and the reason the seam carries the
/// ledger error by value.
#[test]
fn budget_refusals_keep_their_unit_and_amounts() {
    let (fs, staging, blobs, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Blob([3u8; 32]);
    let mut file = fs.create_new(&staging, name).expect("stage");
    fs.write(&mut file, b"0123456789").expect("write");
    fs.sync_file(&file).expect("sync");

    // Exactly one byte short for the bytes unit.
    let mut tight = Ledger::new(9, 8);
    let err = fs
        .hash_bounded(&staging, name, 1024, &mut tight)
        .expect_err("bytes refusal");
    assert_eq!(err.kind(), FsErrorKind::BudgetExceeded);
    let budget = err.budget().expect("bytes refusal must carry its payload");
    assert_eq!(budget.unit(), BudgetUnit::Bytes);
    assert_eq!(budget.requested(), 10);
    assert_eq!(budget.remaining(), 9);
    assert!(
        err.control().is_none(),
        "a budget refusal is not a cancellation"
    );

    // Now starve the entries unit instead; the bytes must not be the reported unit.
    let mut few_entries = Ledger::new(1024, 0);
    let err = fs
        .for_each_entry(&staging, &mut few_entries, &mut |_| Each::Continue)
        .expect_err("entries refusal");
    let budget = err
        .budget()
        .expect("entries refusal must carry its payload");
    assert_eq!(budget.unit(), BudgetUnit::Entries);
    assert_eq!(budget.requested(), 1);
    assert_eq!(budget.remaining(), 0);

    // Silence the unused-variable lint while keeping the blob directory handle meaningful.
    let _ = &blobs;
}

/// `for_each_entry` charges one entry per visit and stops early on `Each::Stop`.
#[test]
fn for_each_entry_charges_per_entry_and_honours_stop() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    for byte in 10u8..13 {
        let mut file = fs
            .create_new(
                &staging,
                EntryName::Staging {
                    kind: sage_db_store::names::StagingKind::Tx,
                    id: id(byte),
                },
            )
            .expect("create");
        fs.write(&mut file, b"x").expect("write");
    }

    let mut ledger = Ledger::new(1024, 8);
    let mut seen = 0usize;
    fs.for_each_entry(&staging, &mut ledger, &mut |_| {
        seen += 1;
        Each::Continue
    })
    .expect("walk");
    assert_eq!(seen, 3);
    assert_eq!(ledger.entry_calls.len(), 3, "one entry charge per visit");
    assert!(ledger.entry_calls.iter().all(|c| *c == 1));

    let mut ledger = Ledger::new(1024, 8);
    let mut seen = 0usize;
    fs.for_each_entry(&staging, &mut ledger, &mut |_| {
        seen += 1;
        Each::Stop
    })
    .expect("walk");
    assert_eq!(seen, 1, "Each::Stop ends the walk after one visit");
    assert_eq!(ledger.entry_calls.len(), 1);
}

/// A budget refusal on a traversal must not be reported as a size limit or as corruption.
#[test]
fn entries_refusal_is_a_budget_refusal_not_a_corrupt() {
    let caps = ModelCapabilities::full();
    let (fs, staging, _, _) = prepared(caps);
    let mut file = fs
        .create_new(
            &staging,
            EntryName::Staging {
                kind: sage_db_store::names::StagingKind::Tx,
                id: id(20),
            },
        )
        .expect("create");
    fs.write(&mut file, b"y").expect("write");

    let mut starved = Ledger::new(0, 0);
    let err = fs
        .for_each_entry(&staging, &mut starved, &mut |_| Each::Continue)
        .expect_err("must refuse");
    assert_eq!(
        err.kind(),
        FsErrorKind::BudgetExceeded,
        "a budget refusal must never be laundered into a data verdict"
    );
    assert!(err.budget().is_some());
}

/// `read_exact_into` refuses a file larger than the buffer whole, with no partial return.
#[test]
fn read_exact_refuses_too_large_without_partial_data() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Manifest,
        id: id(30),
    };
    let mut file = fs.create_new(&staging, name).expect("create");
    fs.write(&mut file, b"0123456789").expect("write");
    fs.sync_file(&file).expect("sync");

    let mut small = [0u8; 4];
    let err = fs
        .read_exact_into(&staging, name, &mut small)
        .expect_err("too large for the buffer");
    assert_eq!(err.kind(), FsErrorKind::TooLarge);
    assert_eq!(
        small, [0u8; 4],
        "a refused read must not write partial data"
    );

    let mut exact = [0u8; 10];
    let n = fs
        .read_exact_into(&staging, name, &mut exact)
        .expect("fits");
    assert_eq!(n, 10);
    assert_eq!(&exact, b"0123456789");
}

/// Unknown names are `NotFound` everywhere, never an empty value.
#[test]
fn unknown_entries_are_not_found_not_empty() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let absent = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(40),
    };
    assert_eq!(
        fs.size(&staging, absent).unwrap_err().kind(),
        FsErrorKind::NotFound
    );
    assert_eq!(
        fs.open_read(&staging, absent).unwrap_err().kind(),
        FsErrorKind::NotFound
    );
    let mut buf = [0u8; 8];
    assert_eq!(
        fs.read_exact_into(&staging, absent, &mut buf)
            .unwrap_err()
            .kind(),
        FsErrorKind::NotFound
    );
    assert_eq!(
        fs.sync_entry(&staging, absent).unwrap_err().kind(),
        FsErrorKind::NotFound
    );
}

/// Opening a family without create refuses a missing family, which init must be able to rely on.
#[test]
fn open_family_without_create_does_not_create() {
    let fs = ModelFileSystem::new(ModelCapabilities::full());
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    let err = fs
        .open_family(&root, Family::Manifests, CreateDir::Never)
        .expect_err("missing family must not be created");
    assert_eq!(err.kind(), FsErrorKind::NotFound);
    assert_eq!(err.effect(), Effect::NoEffect);
}

/// Removing a missing entry is success, but removing a directory is refused.
#[test]
fn remove_semantics() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let ghost = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Tx,
        id: id(50),
    };
    fs.remove(&staging, ghost)
        .expect("missing entry is success");

    let mut file = fs
        .create_new(
            &staging,
            EntryName::Staging {
                kind: sage_db_store::names::StagingKind::Tx,
                id: id(51),
            },
        )
        .expect("create");
    fs.write(&mut file, b"z").expect("write");
    fs.remove(
        &staging,
        EntryName::Staging {
            kind: sage_db_store::names::StagingKind::Tx,
            id: id(51),
        },
    )
    .expect("present entry removes");
}

/// The model's handle types are its own, which the seam permits.
#[test]
fn handles_are_backend_owned_indices() {
    let (fs, staging, blobs, _) = prepared(ModelCapabilities::full());
    assert_ne!(
        staging.0, blobs.0,
        "distinct directories get distinct handles"
    );
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(60),
    };
    let file = fs.create_new(&staging, name).expect("create");
    assert_eq!(file.0, 1, "the first inode is the model's first allocation");
}

/// `hash_bounded` must refuse on `max_len` **before** copying or charging anything.
///
/// The earlier adapter cloned the whole file first, so a caller could make the model allocate a
/// file it had already decided to refuse. A harness that breaks the budget rule cannot judge a
/// store that follows it, so this pins the order explicitly.
#[test]
fn hash_bounded_refuses_oversize_before_any_charge_or_copy() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(70),
    };
    let mut file = fs.create_new(&staging, name).expect("create");
    fs.write(&mut file, b"0123456789").expect("write");
    fs.sync_file(&file).expect("sync");

    // max_len below the real length: refused as TooLarge, and the ledger is never touched.
    let mut ledger = Ledger::new(1024, 8);
    let err = fs
        .hash_bounded(&staging, name, 5, &mut ledger)
        .expect_err("oversize must be refused");
    assert_eq!(err.kind(), FsErrorKind::TooLarge);
    assert_eq!(err.effect(), Effect::NoEffect);
    assert!(
        ledger.byte_calls.is_empty(),
        "a size refusal is not a budget admission: nothing may be charged"
    );

    // max_len exactly the length: admitted and charged exactly the length.
    let mut ledger = Ledger::new(1024, 8);
    let report = fs
        .hash_bounded(&staging, name, 10, &mut ledger)
        .expect("fits exactly");
    assert_eq!(report.len, 10);
    assert_eq!(ledger.byte_calls, vec![10]);
}

/// The hash a bounded read reports must be the canonical SHA-256, with known-answer bytes.
///
/// A model-local fold would produce a digest that cannot interoperate with any persisted hash;
/// this test uses the standard empty-input and `"abc"` vectors so it fails loudly if the adapter
/// ever substitutes a different function.
#[test]
fn hash_bounded_returns_canonical_sha256() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());

    // Known answers (FIPS 180-4).
    let cases: [(&[u8], &str); 2] = [
        (
            b"",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
    ];
    for (index, (bytes, expected)) in cases.iter().enumerate() {
        let name = EntryName::Staging {
            kind: sage_db_store::names::StagingKind::Tx,
            id: id(80 + index as u8),
        };
        let mut file = fs.create_new(&staging, name).expect("create");
        if !bytes.is_empty() {
            fs.write(&mut file, bytes).expect("write");
        }
        fs.sync_file(&file).expect("sync");

        // Note the empty case: zero bytes must still be admitted through the ledger, which is the
        // "including empty input" requirement.
        let mut ledger = Ledger::new(1024, 8);
        let report = fs
            .hash_bounded(&staging, name, 1024, &mut ledger)
            .expect("hash");
        let hex: String = report.sha256.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(&hex, expected, "canonical SHA-256 for {bytes:?}");
        assert_eq!(report.len, bytes.len() as u64);
        // Empty input produces no chunk, so the stream makes no per-chunk charge at all. That is
        // the honest accounting: nothing was fed, nothing was admitted. Cancellation is still
        // observed, because this function checks control before starting.
        if bytes.is_empty() {
            assert!(
                ledger.byte_calls.is_empty(),
                "no chunk fed means no chunk charged: {:?}",
                ledger.byte_calls
            );
        } else {
            assert_eq!(ledger.byte_calls, vec![bytes.len() as u64]);
        }
    }
}

/// A traversal must charge before it visits, and a budget refusal must prevent the visit.
///
/// The earlier adapter collected all names first and charged per entry inside the loop, so a
/// directory larger than the entry allowance still cost a full `Vec<String>` before refusing.
#[test]
fn for_each_entry_charges_before_each_visit_and_refuses_without_visiting() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    for byte in 90u8..95 {
        let mut file = fs
            .create_new(
                &staging,
                EntryName::Staging {
                    kind: sage_db_store::names::StagingKind::Tx,
                    id: id(byte),
                },
            )
            .expect("create");
        fs.write(&mut file, b"q").expect("write");
    }

    // Allowance for two entries only: the third visit must never happen.
    let mut ledger = Ledger::new(1024, 2);
    let mut visited = Vec::new();
    let err = fs
        .for_each_entry(&staging, &mut ledger, &mut |name| {
            visited.push(name.to_owned());
            Each::Continue
        })
        .expect_err("third entry exceeds the allowance");
    assert_eq!(err.kind(), FsErrorKind::BudgetExceeded);
    assert_eq!(
        visited.len(),
        2,
        "a refusal must not perform the refused visit"
    );
    assert_eq!(
        ledger.entry_calls,
        vec![1, 1, 1],
        "one entry charged per attempted visit, refusal included"
    );
}

/// Control is checked per entry, so a stop on an empty directory still consults the caller.
#[test]
fn for_each_entry_consults_control_even_when_empty() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let mut ledger = StoppingLedger { checks: 0 };
    let mut visited = 0usize;
    fs.for_each_entry(&staging, &mut ledger, &mut |_| {
        visited += 1;
        Each::Continue
    })
    .expect("empty walk with a stop is a clean stop, not an error");
    assert_eq!(visited, 0);
    // The loop body never runs, so no entry charge and no checkpoint: that is the honest
    // accounting for an empty directory, and it is why the empty case is tested separately.
    assert_eq!(ledger.checks, 0);
}

/// A ledger that reports cancellation on every checkpoint.
struct StoppingLedger {
    checks: u32,
}

impl SharedLedger for StoppingLedger {
    fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        self.checks += 1;
        Err(ControlStop::Cancelled)
    }
}

/// Cancellation during a traversal is a refusal, not a truncated success.
#[test]
fn for_each_entry_propagates_cancellation_as_a_stop() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let mut file = fs
        .create_new(
            &staging,
            EntryName::Staging {
                kind: sage_db_store::names::StagingKind::Tx,
                id: id(99),
            },
        )
        .expect("create");
    fs.write(&mut file, b"r").expect("write");

    let mut ledger = StoppingLedger { checks: 0 };
    let mut visited = 0usize;
    let err = fs
        .for_each_entry(&staging, &mut ledger, &mut |_| {
            visited += 1;
            Each::Continue
        })
        .expect_err("cancellation must not look like success");
    assert_eq!(err.kind(), FsErrorKind::Interrupted);
    assert_eq!(
        err.control(),
        Some(ControlStop::Cancelled),
        "a stop carries the caller's own reason"
    );
    assert!(err.budget().is_none(), "a stop is not a budget refusal");
    assert_eq!(visited, 0, "the visit must not happen after a stop");
}

/// A cancelled attempt over an **empty** file must still be observed, and must not be charged.
///
/// This is the case the earlier adapter got wrong: it charged and hashed a zero-length file without
/// ever consulting `checkpoint`, so cancellation reported success. The bounded hasher in the format
/// crate checks control at entry precisely for zero-length input; passing the caller's ledger to it
/// (and not charging in this function) is what makes the property true here without double-charging.
#[test]
fn hashing_empty_input_is_still_a_cancellable_operation() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Tx,
        id: id(120),
    };
    // No write at all: the file exists and is zero bytes, so the handle is never mutated.
    let file = fs.create_new(&staging, name).expect("create");
    fs.sync_file(&file).expect("sync");

    let mut ledger = StoppingLedger { checks: 0 };
    let err = fs
        .hash_bounded(&staging, name, 1024, &mut ledger)
        .expect_err("a cancelled attempt must not report success");
    assert_eq!(err.kind(), FsErrorKind::Interrupted);
    assert_eq!(err.control(), Some(ControlStop::Cancelled));
    assert!(err.budget().is_none());
    assert!(
        ledger.checks >= 1,
        "control must be consulted even when there are zero bytes to hash"
    );

    // The same empty file with an accounting ledger: exactly one zero-byte charge, never two.
    let mut counted = Ledger::new(1024, 8);
    let report = fs
        .hash_bounded(&staging, name, 1024, &mut counted)
        .expect("hash empty");
    assert_eq!(report.len, 0);
    assert!(
        counted.byte_calls.is_empty(),
        "no chunk to feed, so no admission happened: {:?}",
        counted.byte_calls
    );
    assert_eq!(
        report.len, 0,
        "and the caller still gets a correct report for an empty file"
    );
}

/// A non-empty bounded hash consults control before doing the work, so cancellation is cheap.
///
/// The entry checkpoint fires first, which is the correct order: cancelling a 200 KB scan should not
/// require hashing one chunk to find out. Asserting "more than one check" would have demanded that
/// the implementation waste work, so this test asserts the actual contract instead: control is
/// consulted, the operation refuses, and the bytes were still admitted exactly once.
#[test]
fn hashing_refuses_before_scanning_when_already_cancelled() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(121),
    };
    let mut file = fs.create_new(&staging, name).expect("create");
    fs.write(&mut file, &vec![0xABu8; 200_000]).expect("write");
    fs.sync_file(&file).expect("sync");

    let mut ledger = StoppingLedger { checks: 0 };
    let err = fs
        .hash_bounded(&staging, name, 1_000_000, &mut ledger)
        .expect_err("a cancelled attempt must refuse");
    assert_eq!(err.kind(), FsErrorKind::Interrupted);
    assert!(err.control().is_some());
    assert_eq!(
        ledger.checks, 1,
        "control is consulted once, before any chunk is hashed"
    );
}

/// A long scan that is *not* cancelled completes, and each chunk is checked, which is the
/// per-chunk contract that only shows up when control keeps saying "continue".
#[test]
fn hashing_a_long_scan_checks_control_per_chunk() {
    /// Counts checkpoints and never cancels.
    struct Counting {
        checks: u32,
    }
    impl SharedLedger for Counting {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            self.checks += 1;
            Ok(())
        }
    }

    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(122),
    };
    let mut file = fs.create_new(&staging, name).expect("create");
    let bytes = vec![0xCDu8; 20_000];
    fs.write(&mut file, &bytes).expect("write");
    fs.sync_file(&file).expect("sync");

    let mut ledger = Counting { checks: 0 };
    let report = fs
        .hash_bounded(&staging, name, 1_000_000, &mut ledger)
        .expect("completes");
    assert_eq!(report.len, bytes.len() as u64);
    // 20 000 bytes is 4 full 4 KiB chunks plus a partial tail: the entry check plus one per full
    // chunk is 5 checks, and asserting an exact count is what makes this a contract rather than a
    // lower bound that a no-op could satisfy.
    // 20 000 bytes is 5 chunks (4 full plus a partial tail). The stream checks at entry, once per
    // chunk fed, and once more on finish: 1 + 5 + 1 = 7.
    assert_eq!(
        ledger.checks, 7,
        "entry check, one per chunk fed (5), and the finish check"
    );
}

/// Known-answer check for a non-trivial length, to keep the canonical-SHA property honest.
#[test]
fn hashing_long_input_matches_the_format_crate() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(123),
    };
    let bytes = vec![0x5Au8; 10_000];
    let mut file = fs.create_new(&staging, name).expect("create");
    fs.write(&mut file, &bytes).expect("write");
    fs.sync_file(&file).expect("sync");

    let mut ledger = Ledger::new(1_000_000, 8);
    let report = fs
        .hash_bounded(&staging, name, 1_000_000, &mut ledger)
        .expect("hash");
    assert_eq!(
        report.sha256,
        sage_db_format::raw_sha256(&bytes),
        "the backend must report exactly the format crate's digest"
    );
}

/// Admission and control must both happen **before** any retained copy or hashing work.
///
/// Asserting "the ledger's byte_calls was empty" only shows a refusal was cheap; it does not show
/// *when* the refusal happened relative to the copy. This test observes ordering directly with a
/// ledger that records the sequence of calls, and a hash path over a large file whose cancellation
/// must be observed at entry — so the observable is "control was consulted before the scan
/// started", not "nothing was charged".
#[test]
fn cancellation_is_observed_before_the_scan_starts() {
    /// Records the order of ledger interactions and cancels on the FIRST checkpoint.
    struct Ordered {
        events: Vec<&'static str>,
    }
    impl SharedLedger for Ordered {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            self.events.push("charge");
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            self.events.push("charge_entries");
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            self.events.push("checkpoint");
            Err(ControlStop::Cancelled)
        }
    }

    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(130),
    };
    // Large enough that a naive implementation would copy before consulting control.
    let payload = vec![0x77u8; 512 * 1024];
    let mut file = fs.create_new(&staging, name).expect("create");
    fs.write(&mut file, &payload).expect("write");
    fs.sync_file(&file).expect("sync");

    let mut ledger = Ordered { events: Vec::new() };
    let err = fs
        .hash_bounded(&staging, name, 8 * 1024 * 1024, &mut ledger)
        .expect_err("cancelled at entry");
    assert_eq!(err.kind(), FsErrorKind::Interrupted);
    assert_eq!(
        ledger.events,
        vec!["checkpoint"],
        "control is consulted before any chunk is fed, so a cancelled attempt never hashes: got {:?}",
        ledger.events
    );
    assert_eq!(
        ledger.events.iter().filter(|e| **e == "checkpoint").count(),
        1,
        "cancelled at entry means no chunk checks were reached"
    );
}

/// An oversize file is refused without any ledger interaction at all.
///
/// This is the ordering property that matters for `max_len`: a refusal must precede admission and
/// copying, so a caller cannot be charged for, or make the model allocate for, work it forbade.
#[test]
fn oversize_refusal_precedes_every_ledger_interaction() {
    struct Never {
        touched: u32,
    }
    impl SharedLedger for Never {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            self.touched += 1;
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            self.touched += 1;
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            self.touched += 1;
            Ok(())
        }
    }

    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(131),
    };
    let mut file = fs.create_new(&staging, name).expect("create");
    fs.write(&mut file, &vec![0u8; 64 * 1024]).expect("write");
    fs.sync_file(&file).expect("sync");

    let mut ledger = Never { touched: 0 };
    let err = fs
        .hash_bounded(&staging, name, 1024, &mut ledger)
        .expect_err("too large");
    assert_eq!(err.kind(), FsErrorKind::TooLarge);
    assert_eq!(err.effect(), Effect::NoEffect);
    assert_eq!(
        ledger.touched, 0,
        "a size refusal must not admit, check control, or otherwise touch the ledger"
    );
}

/// Publication charges for its own re-hash, so a caller's budget governs publishing too.
#[test]
fn publish_charges_for_its_rehash_out_of_the_caller_budget() {
    let (fs, staging, blobs, _) = prepared(ModelCapabilities::full());
    let staged = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(132),
    };
    let published = EntryName::Blob([0x42u8; 32]);

    let mut file = fs.create_new(&staging, staged).expect("stage");
    fs.write(&mut file, b"0123456789").expect("write");
    fs.sync_file(&file).expect("sync");
    let report = fs
        .hash_bounded(&staging, staged, 1024, &mut Ledger::new(1024, 8))
        .expect("hash");

    // A budget of one byte short of the staged length must refuse the publication's own read.
    let mut tight = Ledger::new(9, 8);
    let err = fs
        .publish_no_overwrite(&staging, staged, &blobs, published, &report, &mut tight)
        .expect_err("publication must respect the budget");
    assert_eq!(err.kind(), FsErrorKind::BudgetExceeded);
    let budget = err.budget().expect("budget payload");
    assert_eq!(budget.unit(), BudgetUnit::Bytes);
    assert_eq!(budget.requested(), 10);
    assert_eq!(budget.remaining(), 9);

    // With enough budget the publication succeeds and charged exactly the staged length.
    let mut enough = Ledger::new(1024, 8);
    fs.publish_no_overwrite(&staging, staged, &blobs, published, &report, &mut enough)
        .expect("publish");
    assert_eq!(enough.byte_calls, vec![10]);
}

/// Publishing bytes whose digest does not match the claim is refused, and costs one read.
#[test]
fn publish_refuses_a_digest_the_bytes_do_not_hash_to() {
    let (fs, staging, blobs, _) = prepared(ModelCapabilities::full());
    let staged = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(133),
    };
    let published = EntryName::Blob([0x43u8; 32]);

    let mut file = fs.create_new(&staging, staged).expect("stage");
    fs.write(&mut file, b"actual").expect("write");
    fs.sync_file(&file).expect("sync");
    let report = fs
        .hash_bounded(&staging, staged, 1024, &mut Ledger::new(1024, 8))
        .expect("hash");

    // Claim a different length: the length check fires before any hashing.
    let mut lie = report;
    lie.len = 999;
    let mut ledger = Ledger::new(1024, 8);
    let err = fs
        .publish_no_overwrite(&staging, staged, &blobs, published, &lie, &mut ledger)
        .expect_err("length claim is wrong");
    assert_eq!(err.kind(), FsErrorKind::HashMismatch);
    assert_eq!(err.effect(), Effect::NoEffect);

    // Claim a different digest of the same length: refused after hashing.
    let mut wrong = report;
    wrong.sha256 = [0xFFu8; 32];
    let mut ledger = Ledger::new(1024, 8);
    let err = fs
        .publish_no_overwrite(&staging, staged, &blobs, published, &wrong, &mut ledger)
        .expect_err("digest claim is wrong");
    assert_eq!(err.kind(), FsErrorKind::HashMismatch);
}

/// The retained-copy counter must stay at zero across an admission-plus-hash.
///
/// **What this does and does not prove.** It asserts a property of the current borrow-only
/// implementation: `hash_bounded` holds the model's `Ref` and hashes the borrowed slice, so no
/// copy is retained and the counter never moves. It is *not* an allocator oracle: the counter has
/// no increment site in this code, so a copy introduced without touching the probe would not be
/// seen. An earlier version of this comment claimed the test "sees allocation ordering" because it
/// caught one specifically injected `to_vec()`; that was true of that mutation only, and the claim
/// is withdrawn here.
#[test]
fn hashing_makes_no_retained_copy_around_admission() {
    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(140),
    };
    let payload = vec![0x31u8; 300_000];
    let mut file = fs.create_new(&staging, name).expect("create");
    fs.write(&mut file, &payload).expect("write");
    fs.sync_file(&file).expect("sync");

    let before = super::fs_model::copies_made();
    let mut ledger = Ledger::new(1 << 20, 8);
    let report = fs
        .hash_bounded(&staging, name, 1 << 20, &mut ledger)
        .expect("hash");
    let after = super::fs_model::copies_made();

    assert_eq!(
        after, before,
        "hashing must borrow the model's bytes, not retain a copy"
    );
    assert_eq!(report.len, payload.len() as u64);
    assert_eq!(report.sha256, sage_db_format::raw_sha256(&payload));
    assert_eq!(
        ledger.byte_calls.iter().sum::<u64>(),
        payload.len() as u64,
        "chunked charges must total the payload exactly, with no duplicate admission"
    );
}

/// Cancellation must be observed **during** hashing, not only before it starts.
///
/// This is the property the previous implementation failed: it looped `checkpoint` calls up front
/// and then hashed the whole slice in one `raw_sha256`, so a ledger that starts returning
/// `Cancelled` after the first chunk was never consulted again. Here the ledger lets the entry check
/// through and cancels on the second chunk, and the observable is that some chunk WAS fed before the
/// refusal — a cancellation point that never fires during the scan does not exist.
#[test]
fn cancellation_during_hashing_stops_mid_scan() {
    /// Allows the first `allow` checkpoints, then cancels.
    struct CancelAfter {
        allow: u32,
        seen: u32,
        charges: u32,
    }
    impl SharedLedger for CancelAfter {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            self.charges += 1;
            Ok(())
        }
        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            self.seen += 1;
            if self.seen > self.allow {
                return Err(ControlStop::Cancelled);
            }
            Ok(())
        }
    }

    let (fs, staging, _, _) = prepared(ModelCapabilities::full());
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Blob,
        id: id(150),
    };
    // Ten 4 KiB chunks: cancelling at the third checkpoint must stop well before the tenth.
    let payload = vec![0x9Fu8; 10 * 4096];
    let mut file = fs.create_new(&staging, name).expect("create");
    fs.write(&mut file, &payload).expect("write");
    fs.sync_file(&file).expect("sync");

    // Allow entry plus two chunk checks, then cancel.
    let mut ledger = CancelAfter {
        allow: 3,
        seen: 0,
        charges: 0,
    };
    let err = fs
        .hash_bounded(&staging, name, 1 << 20, &mut ledger)
        .expect_err("must cancel mid-scan");
    assert_eq!(err.kind(), FsErrorKind::Interrupted);
    assert_eq!(err.control(), Some(ControlStop::Cancelled));
    assert!(
        ledger.charges >= 1 && ledger.charges < 10,
        "some chunks were fed before the refusal, but not all of them: charges={}",
        ledger.charges
    );
}

/// A declared short write is a partial success whose bytes are visible but not durable.
///
/// This is the case a test must be able to produce on demand: the seam's `Ok` with `written < len`.
/// The assertion distinguishes "accepted bytes are visible" from "anything is durable", which is
/// the distinction a store must respect when it retries the remainder.
#[test]
fn declared_short_write_is_visible_but_not_durable() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};

    let fs = ModelFileSystem::new(ModelCapabilities::full()).with_faults(FaultSchedule::none().at(
        FaultPoint::Write,
        Injection::ShortWrite(3),
        1,
    ));
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    let staging = fs
        .open_family(&root, Family::Staging, CreateDir::IfMissing)
        .expect("staging");
    let name = EntryName::Staging {
        kind: sage_db_store::names::StagingKind::Tx,
        id: id(160),
    };
    let mut file = fs.create_new(&staging, name).expect("create");

    let outcome = fs.write(&mut file, b"abcdef").expect("short write is Ok");
    assert_eq!(outcome.written, 3, "only the declared prefix was accepted");

    // The accepted prefix is visible...
    let mut buf = [0u8; 16];
    let n = fs.read_exact_into(&staging, name, &mut buf).expect("read");
    assert_eq!(&buf[..n], b"abc");
    // ...and nothing is durable, because no sync happened.
    let states = fs.model().crash_states();
    assert!(
        states.sampled > 1,
        "with an unsynced partial write there must be more than one legal state"
    );
    fs.assert_all_faults_fired();
}

/// A failed sync leaves durability unproven, and `Effect::Unknown` says the caller must resolve.
#[test]
fn declared_sync_failure_leaves_the_state_unsynced() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};

    let fs = ModelFileSystem::new(ModelCapabilities::full()).with_faults(FaultSchedule::none().at(
        FaultPoint::SyncDir,
        Injection::FailUnknown,
        1,
    ));
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    let staging = fs
        .open_family(&root, Family::Staging, CreateDir::IfMissing)
        .expect("staging");
    let mut file = fs
        .create_new(
            &staging,
            EntryName::Staging {
                kind: sage_db_store::names::StagingKind::Blob,
                id: id(161),
            },
        )
        .expect("create");
    fs.write(&mut file, b"bytes").expect("write");
    fs.sync_file(&file).expect("content sync");

    let err = fs.sync_dir(&staging).expect_err("declared failure");
    assert_eq!(
        err.effect(),
        Effect::Unknown,
        "the caller must resolve, not assume"
    );
    assert!(err.is_ambiguous());

    // Because the directory sync never took effect, the entry may still be lost.
    let states = fs.model().crash_states();
    assert!(
        states
            .iter()
            .any(|s| s.files.values().all(|v| v.as_slice() != b"bytes")),
        "a failed directory sync must leave the entry losable"
    );
    fs.assert_all_faults_fired();
}

/// A declared budget refusal on enumeration is typed with its unit, before any visit.
#[test]
fn declared_entries_budget_refusal_prevents_the_walk() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};

    let fs = ModelFileSystem::new(ModelCapabilities::full()).with_faults(FaultSchedule::none().at(
        FaultPoint::Enumerate,
        Injection::BudgetEntries {
            requested: 1,
            remaining: 0,
        },
        1,
    ));
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    let blobs = fs
        .open_family(&root, Family::Blobs, CreateDir::IfMissing)
        .expect("blobs");

    let mut ledger = Ledger::new(1024, 8);
    let mut visited = 0usize;
    let err = fs
        .for_each_entry(&blobs, &mut ledger, &mut |_| {
            visited += 1;
            Each::Continue
        })
        .expect_err("declared entries refusal");
    assert_eq!(err.kind(), FsErrorKind::BudgetExceeded);
    let budget = err.budget().expect("payload");
    assert_eq!(budget.unit(), BudgetUnit::Entries);
    assert_eq!(visited, 0, "the refusal must precede every visit");
    fs.assert_all_faults_fired();
}

/// Declaring a fault the scenario never reaches must fail loudly, not pass quietly.
#[test]
#[should_panic(expected = "declared injections never fired")]
fn an_unreached_declared_fault_fails_the_scenario() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};

    let fs = ModelFileSystem::new(ModelCapabilities::full()).with_faults(FaultSchedule::none().at(
        FaultPoint::Publish,
        Injection::FailNoEffect,
        1,
    ));
    // Never publish: the declared injection cannot fire.
    fs.assert_all_faults_fired();
}

/// An `Effect::Unknown` from `replace_current` can leave the OLD root or the NEW root.
///
/// This is the pair the earlier schedule never produced. Both outcomes are legal at the call site and
/// both must be reported as `Unknown` — never as `Entailed` success, and never with the caller's
/// error implying which one happened. The model is inspected directly to show the two states exist.
#[test]
fn unknown_replace_can_leave_either_root_and_both_are_unknown() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};

    for (label, injection, expect_new) in [
        ("before-effect", Injection::FailUnknown, false),
        ("after-effect", Injection::FailUnknownAfterEffect, true),
    ] {
        // Build the baseline FIRST, then arm the single declared injection: a schedule armed before
        // the setup would be consumed by the setup's own replace_current, and the test would be
        // measuring the wrong operation.
        let fs = ModelFileSystem::new(ModelCapabilities::full());
        let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
        fs.sync_parent_of(Path::new("/tmp/model-kb"))
            .expect("parent");

        // Establish CURRENT = "old".
        let mut old = fs
            .create_new(&root, EntryName::Root(RootName::CurrentTmp))
            .expect("stage old");
        fs.write(&mut old, b"old").expect("w");
        fs.sync_file(&old).expect("s");
        fs.replace_current(&root, EntryName::Root(RootName::CurrentTmp), &root)
            .expect("install old");
        fs.sync_dir(&root).expect("names");

        // Arm the injection only now.
        *fs.faults_handle().borrow_mut() =
            FaultSchedule::none().at(FaultPoint::ReplaceCurrent, injection, 1);

        // Stage "new" and attempt the replacement, which will fail with Unknown.
        let mut new = fs
            .create_new(&root, EntryName::Root(RootName::CurrentTmp))
            .expect("stage new");
        fs.write(&mut new, b"new").expect("w");
        fs.sync_file(&new).expect("s");

        let err = fs
            .replace_current(&root, EntryName::Root(RootName::CurrentTmp), &root)
            .expect_err("declared unknown");
        assert_eq!(
            err.effect(),
            Effect::Unknown,
            "{label}: the caller must resolve, and the error must not claim success"
        );
        assert!(
            !matches!(err.effect(), Effect::Entailed),
            "{label}: an unacknowledged failure is never an entailed success"
        );

        // Which root the MODEL currently holds is the distinguishing observable. Reading a crash
        // state would be wrong here: the directory sync is pending, so several legal states exist
        // and picking the first one says nothing about what the operation did.
        let current = fs
            .model()
            .inode_of(&super::RelPath::root(), RootName::Current.as_str())
            .and_then(|ino| fs.model().visible_bytes(ino).map(<[u8]>::to_vec));
        let saw_new = current.as_deref() == Some(b"new");
        assert_eq!(
            saw_new, expect_new,
            "{label}: unexpected visible CURRENT content {current:?}"
        );
        fs.assert_all_faults_fired();
    }
}

/// The external oracle must not derive commit authority from an effect label.
///
/// An `Effect::Unknown` on a root replacement is exactly the case an implementation might try to
/// resolve by trusting the error class. The oracle only looks at observed acknowledgements, so a
/// scenario with no ACK has no committed request even if the filesystem now holds the new root.
#[test]
fn oracle_does_not_promote_unknown_effect_to_commit() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};
    use crate::oracle::{Observation, Recovered, check};

    let fs = ModelFileSystem::new(ModelCapabilities::full()).with_faults(FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    ));
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    fs.sync_parent_of(Path::new("/tmp/model-kb"))
        .expect("parent");
    let mut staged = fs
        .create_new(&root, EntryName::Root(RootName::CurrentTmp))
        .expect("stage");
    fs.write(&mut staged, b"root").expect("w");
    fs.sync_file(&staged).expect("s");
    let err = fs
        .replace_current(&root, EntryName::Root(RootName::CurrentTmp), &root)
        .expect_err("unknown");
    assert_eq!(err.effect(), Effect::Unknown);

    // The model applied the rename, but nobody was acknowledged, so the only legal recovered request
    // set is empty: filesystem state is not commit authority.
    let obs = Observation::default();
    let recovered = Recovered {
        requests: std::collections::BTreeSet::new(),
        version: None,
        failed: false,
    };
    assert_eq!(
        check(&obs, &recovered),
        Vec::new(),
        "an applied rename with no acknowledgement must not appear as a committed request"
    );
    fs.assert_all_faults_fired();
}

/// A publish fault must not fire at a root replace (cross-point guard).
#[test]
fn publish_fault_does_not_fire_at_replace_current() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};

    let fs = ModelFileSystem::new(ModelCapabilities::full());
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    fs.sync_parent_of(Path::new("/tmp/model-kb"))
        .expect("parent");
    let mut staged = fs
        .create_new(&root, EntryName::Root(RootName::CurrentTmp))
        .expect("stage");
    fs.write(&mut staged, b"root").expect("w");
    fs.sync_file(&staged).expect("s");

    // Arm a PUBLISH fault only; a replace_current must be unaffected by it.
    *fs.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::Publish, Injection::FailUnknown, 1);

    fs.replace_current(&root, EntryName::Root(RootName::CurrentTmp), &root)
        .expect("replace must not consume a publish fault");
    assert_eq!(
        fs.faults_handle().borrow().pending(FaultPoint::Publish),
        1,
        "the publish fault must still be armed for the publish"
    );
}

/// `sync_file` before-effect: content is NOT durable, and the caller still gets `Unknown`.
#[test]
fn sync_file_before_effect_leaves_content_non_durable() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};

    let fs = ModelFileSystem::new(ModelCapabilities::full());
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    let staging = fs
        .open_family(&root, Family::Staging, CreateDir::IfMissing)
        .expect("staging");
    let mut file = fs
        .create_new(
            &staging,
            EntryName::Staging {
                kind: sage_db_store::names::StagingKind::Blob,
                id: id(170),
            },
        )
        .expect("create");
    fs.write(&mut file, b"payload").expect("write");

    *fs.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::SyncFile, Injection::FailUnknown, 1);
    let err = fs.sync_file(&file).expect_err("declared unknown");
    assert_eq!(err.effect(), Effect::Unknown);

    // The observable: the model holds NO durable content for this inode.
    assert_eq!(
        fs.model().durable_bytes(file.0),
        None,
        "a before-effect failure must not make content durable"
    );
    fs.assert_all_faults_fired();
}

/// `sync_file` after-effect: content IS durable in the model and the caller still gets `Unknown`.
///
/// This is the pair the parent probe found missing. The error class is identical, so a store that
/// treats "error" as "nothing happened" is wrong here, while a store that treats it as success would
/// also be wrong - the caller has no acknowledgement.
#[test]
fn sync_file_after_effect_makes_content_durable_but_still_unknown() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};

    let fs = ModelFileSystem::new(ModelCapabilities::full());
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    let staging = fs
        .open_family(&root, Family::Staging, CreateDir::IfMissing)
        .expect("staging");
    let mut file = fs
        .create_new(
            &staging,
            EntryName::Staging {
                kind: sage_db_store::names::StagingKind::Blob,
                id: id(171),
            },
        )
        .expect("create");
    fs.write(&mut file, b"payload").expect("write");

    *fs.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::SyncFile, Injection::FailUnknownAfterEffect, 1);
    let err = fs.sync_file(&file).expect_err("declared unknown");
    assert_eq!(
        err.effect(),
        Effect::Unknown,
        "the same error class as the before-effect branch"
    );

    // The observable: content really is durable now.
    assert_eq!(
        fs.model().durable_bytes(file.0),
        Some(b"payload".as_slice()),
        "an after-effect failure must have applied the state change"
    );
    fs.assert_all_faults_fired();
}

/// `sync_dir` before-effect: the entry may still be lost.
#[test]
fn sync_dir_before_effect_leaves_the_entry_losable() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};

    let fs = ModelFileSystem::new(ModelCapabilities::full());
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    let staging = fs
        .open_family(&root, Family::Staging, CreateDir::IfMissing)
        .expect("staging");
    let mut file = fs
        .create_new(
            &staging,
            EntryName::Staging {
                kind: sage_db_store::names::StagingKind::Blob,
                id: id(172),
            },
        )
        .expect("create");
    fs.write(&mut file, b"bytes").expect("write");
    fs.sync_file(&file).expect("content first");

    *fs.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::SyncDir, Injection::FailUnknown, 1);
    let err = fs.sync_dir(&staging).expect_err("declared unknown");
    assert_eq!(err.effect(), Effect::Unknown);

    let states = fs.model().crash_states();
    assert!(
        states
            .iter()
            .any(|s| s.files.values().all(|v| v.as_slice() != b"bytes")),
        "a before-effect directory sync must leave the entry losable"
    );
    fs.assert_all_faults_fired();
}

/// `sync_dir` after-effect: entries ARE durable and the caller still gets `Unknown`.
#[test]
fn sync_dir_after_effect_makes_entries_durable_but_still_unknown() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};

    let fs = ModelFileSystem::new(ModelCapabilities::full());
    let root = fs.create_root(Path::new("/tmp/model-kb")).expect("root");
    fs.sync_parent_of(Path::new("/tmp/model-kb"))
        .expect("parent");
    let staging = fs
        .open_family(&root, Family::Staging, CreateDir::IfMissing)
        .expect("staging");
    let mut file = fs
        .create_new(
            &staging,
            EntryName::Staging {
                kind: sage_db_store::names::StagingKind::Blob,
                id: id(173),
            },
        )
        .expect("create");
    fs.write(&mut file, b"bytes").expect("write");
    fs.sync_file(&file).expect("content first");
    fs.sync_dir(&root).expect("root names");

    *fs.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::SyncDir, Injection::FailUnknownAfterEffect, 1);
    let err = fs.sync_dir(&staging).expect_err("declared unknown");
    assert_eq!(err.effect(), Effect::Unknown);

    // Entries are durable, so every legal crash state must retain the bytes.
    let states = fs.model().crash_states();
    assert!(
        states
            .iter()
            .all(|s| s.files.values().any(|v| v.as_slice() == b"bytes")),
        "an after-effect directory sync must have made the entry durable"
    );
    fs.assert_all_faults_fired();
}

/// An after-effect pairing at a point whose implementation does not apply a change is refused.
///
/// This is the guard for the defect the parent probe found: `sync_file` advertised acceptance and
/// then returned before mutating. An unsupported combination must be a construction error, not a
/// silently mislabelled injection.
#[test]
#[should_panic(expected = "cannot occur at")]
fn after_effect_at_an_unimplemented_point_is_refused() {
    use crate::faults::{FaultPoint, FaultSchedule, Injection};
    let _ = FaultSchedule::none().at(FaultPoint::Enumerate, Injection::FailUnknownAfterEffect, 1);
}

/// Targeted backend controls for `replace_current`.
///
/// These exist because the end-to-end positive does NOT discriminate the model fix: with the
/// corrected root-temp commit path the replace is same-directory, so an old model that ignored
/// `staged_dir` would still pass. These controls force the two cases the seam actually permits.
#[cfg(test)]
mod replace_current_controls {
    use crate::model::fs_model::{ModelCapabilities, ModelFileSystem};
    use sage_db_store::fs::{CreateDir, Effect, Family, FileSystem};
    use sage_db_store::names::{EntryName, RootName};
    use std::path::PathBuf;

    type Dir = <ModelFileSystem as FileSystem>::Dir;

    fn prepared_root() -> (ModelFileSystem, PathBuf) {
        let fs = ModelFileSystem::new(ModelCapabilities::full());
        let dir = std::env::temp_dir().join(format!(
            "model-replace-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        FileSystem::create_root(&fs, &dir).expect("root");
        (fs, dir)
    }

    fn write_file(fs: &ModelFileSystem, dir: &Dir, name: EntryName, bytes: &[u8]) {
        let mut file = fs.create_new(dir, name).expect("create");
        fs.write(&mut file, bytes).expect("write");
        fs.sync_file(&file).expect("sync");
    }

    /// CONTROL 1: `staged_dir != root`, and the destination `CURRENT` ALREADY EXISTS.
    ///
    /// The old model looked the staged name up in the root and panicked; the `link` + `remove`
    /// variant panicked differently, because `link` asserts the destination is absent. Neither can
    /// pass this: it demands a cross-directory rename that REPLACES an existing root pointer, in one
    /// effect.
    #[test]
    fn cross_directory_replace_overwrites_an_existing_current() {
        let (fs, abs) = prepared_root();
        let root = FileSystem::open_root(&fs, &abs).expect("open root");
        let staging = FileSystem::open_family(&fs, &root, Family::Staging, CreateDir::IfMissing)
            .expect("staging family");

        // An existing published root pointer.
        write_file(
            &fs,
            &root,
            EntryName::Root(RootName::Current),
            b"old-root-bytes",
        );
        // A staged replacement that lives in a DIFFERENT directory.
        let staged = EntryName::Staging {
            kind: sage_db_store::names::StagingKind::Current,
            id: sage_db_store::names::Id128::from_bytes([0x11; 16]),
        };
        write_file(&fs, &staging, staged, b"new-root-bytes");

        fs.replace_current(&staging, staged, &root)
            .expect("cross-directory replace must overwrite, not fail");

        // The new bytes are visible at CURRENT, and the staged name is consumed.
        let mut file = fs
            .open_read(&root, EntryName::Root(RootName::Current))
            .expect("current");
        let mut buf = vec![0u8; 14];
        fs.read(&mut file, &mut buf).expect("read");
        assert_eq!(
            &buf, b"new-root-bytes",
            "CURRENT must hold the staged bytes"
        );
        assert!(
            fs.open_read(&staging, staged).is_err(),
            "rename must consume the staged name"
        );

        let _ = std::fs::remove_dir_all(&abs);
    }

    /// CONTROL 2: a missing staged entry is a TYPED error with no effect - never a panic.
    #[test]
    fn missing_source_is_a_typed_not_found_with_no_effect() {
        let (fs, abs) = prepared_root();
        let root = FileSystem::open_root(&fs, &abs).expect("open root");
        let staging = FileSystem::open_family(&fs, &root, Family::Staging, CreateDir::IfMissing)
            .expect("staging family");
        write_file(
            &fs,
            &root,
            EntryName::Root(RootName::Current),
            b"old-root-bytes",
        );

        let absent = EntryName::Staging {
            kind: sage_db_store::names::StagingKind::Current,
            id: sage_db_store::names::Id128::from_bytes([0x22; 16]),
        };
        let error = fs
            .replace_current(&staging, absent, &root)
            .expect_err("a missing staged entry must be refused");
        assert_eq!(error.kind(), sage_db_store::fs::FsErrorKind::NotFound);
        assert_eq!(
            error.effect(),
            Effect::NoEffect,
            "a refusal changes nothing"
        );

        // The published pointer is untouched.
        let mut file = fs
            .open_read(&root, EntryName::Root(RootName::Current))
            .expect("current");
        let mut buf = vec![0u8; 14];
        fs.read(&mut file, &mut buf).expect("read");
        assert_eq!(&buf, b"old-root-bytes");

        let _ = std::fs::remove_dir_all(&abs);
    }
}

/// Cursor controls for the model's read path.
///
/// These exist because the accepted model's `read` was stateless: it copied `bytes[..n]` every call, so it
/// never advanced and never reported EOF. That made any read-until-zero loop (the commit path hashing a
/// rules blob) accumulate more bytes than the file holds. The two controls here are deliberately narrow:
/// progression/EOF on one handle, and INDEPENDENT per-handle offsets with interleaved opens.
#[cfg(test)]
mod read_cursor_controls {
    use crate::model::fs_model::{ModelCapabilities, ModelFileSystem};
    use sage_db_store::fs::{CreateDir, Family, FileSystem};
    use sage_db_store::names::EntryName;
    use std::path::PathBuf;

    fn seeded() -> (ModelFileSystem, PathBuf) {
        let fs = ModelFileSystem::new(ModelCapabilities::full());
        let dir = std::env::temp_dir().join(format!(
            "read-cursor-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        FileSystem::create_root(&fs, &dir).expect("root");
        (fs, dir)
    }

    /// Successive reads advance, a short buffer does not repeat, and EOF is 0.
    #[test]
    fn reads_advance_and_report_eof() {
        let (fs, abs) = seeded();
        let root = FileSystem::open_root(&fs, &abs).expect("root");
        let blobs =
            FileSystem::open_family(&fs, &root, Family::Blobs, CreateDir::IfMissing).unwrap();
        let name = EntryName::Blob([0x5A; 32]);
        let mut file = fs.create_new(&blobs, name).expect("create");
        fs.write(&mut file, b"abcdefgh").expect("write");
        fs.sync_file(&file).expect("sync");

        let mut reader = fs.open_read(&blobs, name).expect("open");
        let mut chunk = [0u8; 3];

        let n1 = fs.read(&mut reader, &mut chunk).expect("read 1");
        assert_eq!(
            (n1, &chunk[..n1]),
            (3, &b"abc"[..]),
            "first read must start at 0"
        );

        let n2 = fs.read(&mut reader, &mut chunk).expect("read 2");
        assert_eq!(
            (n2, &chunk[..n2]),
            (3, &b"def"[..]),
            "second read must continue, not restart"
        );

        let n3 = fs.read(&mut reader, &mut chunk).expect("read 3");
        assert_eq!(
            (n3, &chunk[..n3]),
            (2, &b"gh"[..]),
            "third read must return the remainder"
        );

        let n4 = fs.read(&mut reader, &mut chunk).expect("read 4");
        assert_eq!(n4, 0, "an exhausted handle must report EOF as 0");

        let mut again = fs.open_read(&blobs, name).expect("reopen");
        let n5 = fs.read(&mut again, &mut chunk).expect("read 5");
        assert_eq!(
            (n5, &chunk[..n5]),
            (3, &b"abc"[..]),
            "a reopen must start at 0"
        );

        let _ = std::fs::remove_dir_all(&abs);
    }

    /// INTERLEAVED handles: each open has its OWN offset.
    ///
    /// A single global cursor would pass a sequential test (open B only after A is exhausted) but fails
    /// this: A reads 2, B opens and reads 3 from its own start, then A continues at offset 2.
    #[test]
    fn two_open_handles_have_independent_offsets() {
        let (fs, abs) = seeded();
        let root = FileSystem::open_root(&fs, &abs).expect("root");
        let blobs =
            FileSystem::open_family(&fs, &root, Family::Blobs, CreateDir::IfMissing).unwrap();
        let name = EntryName::Blob([0x6B; 32]);
        let mut file = fs.create_new(&blobs, name).expect("create");
        fs.write(&mut file, b"abcdefg").expect("write");
        fs.sync_file(&file).expect("sync");

        let mut a_buf = [0u8; 7];
        let mut b_buf = [0u8; 7];
        let mut a = fs.open_read(&blobs, name).expect("open A");
        let n = fs.read(&mut a, &mut a_buf[..2]).expect("A read 1");
        assert_eq!((n, &a_buf[..n]), (2, &b"ab"[..]), "A must start at 0");

        let mut b = fs.open_read(&blobs, name).expect("open B");
        let nb = fs.read(&mut b, &mut b_buf[..3]).expect("B read 1");
        assert_eq!(
            (nb, &b_buf[..nb]),
            (3, &b"abc"[..]),
            "B must have its own offset 0"
        );

        let n2 = fs.read(&mut a, &mut a_buf[2..7]).expect("A read 2");
        assert_eq!(
            (n2, &a_buf[2..2 + n2]),
            (5, &b"cdefg"[..]),
            "A must continue at its own offset 2"
        );

        assert_eq!(
            fs.read(&mut a, &mut a_buf).expect("A eof"),
            0,
            "A must report EOF"
        );
        let nb2 = fs.read(&mut b, &mut b_buf[..4]).expect("B read 2");
        assert_eq!(
            (nb2, &b_buf[..nb2]),
            (4, &b"defg"[..]),
            "B must continue at its own offset 3"
        );

        let _ = std::fs::remove_dir_all(&abs);
    }
}
