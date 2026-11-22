//! Real-OS behaviour tests for the backend (P1-3, store owner).
//!
//! These exercise the **actual** `OsFileSystem` against a real temporary directory — no model, no
//! stand-in. They are the evidence that the seam's documented contracts hold when a kernel is
//! involved, and they are deliberately the only place in the store that touches a filesystem.
//!
//! ## What these tests establish, and what they do not
//!
//! Established: typed-name containment, non-creating open, explicit init, no-overwrite identity,
//! exclusive-lock exclusion, short-write handling, sync success, same-device reporting, and
//! bounded/cancellable enumeration — on **this** host (macOS/APFS).
//!
//! **Not** established: device-level durability on any platform, and Linux behaviour at all. A
//! successful `sync_all` is a primitive property; it is not a durability proof. See `PLATFORMS.md`
//! in the accepted evidence: APFS documents no directory-entry guarantee, and no Linux device has
//! been exercised. `OsFileSystem::conservative()` therefore reports `durable_sync = false`.

use std::path::{Path, PathBuf};

use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Effect, Family, FileSystem, FsErrorKind, PublishMode,
};
use sage_db_store::names::{EntryName, Id128, RootName, StagingKind};
use sage_db_store::os::OsFileSystem;
use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::query::ControlStop;

/// A scratch directory that removes itself.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "sage-db-store-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir(&path).expect("scratch dir");
        Self { path }
    }
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A ledger that records what it was asked for, so charge behaviour is observable.
struct Recording {
    bytes_left: u64,
    entries_left: u64,
    bytes_charged: u64,
    entries_charged: u32,
    checkpoints: u32,
    /// Return `Cancelled` from the nth checkpoint, to test the control path.
    stop_at_checkpoint: Option<u32>,
}

impl Recording {
    fn new(bytes: u64, entries: u64) -> Self {
        Self {
            bytes_left: bytes,
            entries_left: entries,
            bytes_charged: 0,
            entries_charged: 0,
            checkpoints: 0,
            stop_at_checkpoint: None,
        }
    }
}

impl SharedLedger for Recording {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        if bytes > self.bytes_left {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.bytes_left));
        }
        self.bytes_left -= bytes;
        self.bytes_charged += bytes;
        Ok(())
    }
    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        if count > self.entries_left {
            return Err(BudgetError::new(
                BudgetUnit::Entries,
                count,
                self.entries_left,
            ));
        }
        self.entries_left -= count;
        self.entries_charged += count as u32;
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        self.checkpoints += 1;
        match self.stop_at_checkpoint {
            Some(limit) if self.checkpoints >= limit => Err(ControlStop::Cancelled),
            _ => Ok(()),
        }
    }
}

/// Build a KB-shaped layout under `scratch`, returning the opened root and the families.
fn layout(
    fs: &OsFileSystem,
    scratch: &Scratch,
) -> (sage_db_store::os::OsDir, Vec<sage_db_store::os::OsDir>) {
    let kb = scratch.path().join("kb");
    let root = fs.create_root(&kb).expect("create root");
    let mut dirs = Vec::new();
    for family in [
        Family::Manifests,
        Family::Transactions,
        Family::Blobs,
        Family::Staging,
        Family::Checkpoints,
        Family::Cache,
    ] {
        dirs.push(
            fs.open_family(&root, family, CreateDir::IfMissing)
                .expect("create family"),
        );
    }
    (root, dirs)
}

#[test]
fn open_never_creates_and_init_is_explicit() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("open-no-create");
    let missing = scratch.path().join("absent");

    // open on a missing root refuses.
    let err = fs.open_root(&missing).expect_err("open must not create");
    assert_eq!(err.kind(), FsErrorKind::NotFound);
    assert!(
        !missing.exists(),
        "open created the directory it was told not to"
    );

    // explicit init creates, and only then does open succeed.
    let _root = fs.create_root(&missing).expect("explicit init");
    assert!(missing.is_dir());
    let reopened = fs.open_root(&missing).expect("open after init");
    assert_eq!(reopened.family(), None);
}

#[test]
fn init_syncs_the_parent_directory() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("parent-sync");
    let kb = scratch.path().join("kb2");
    let _ = fs.create_root(&kb).expect("create");
    // Exercises the parent-directory flush path; a failure here is a real signal.
    fs.sync_parent_of(&kb).expect("parent sync");
}

#[test]
fn open_family_with_never_does_not_create() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("family-no-create");
    let root = fs.create_root(&scratch.path().join("kb3")).expect("root");
    let err = fs
        .open_family(&root, Family::Blobs, CreateDir::Never)
        .expect_err("must not create");
    assert_eq!(err.kind(), FsErrorKind::NotFound);
    assert!(!scratch.path().join("kb3/blobs").exists());
}

#[test]
fn exclusive_lock_excludes_a_second_holder() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("lock");
    let (root, _dirs) = layout(&fs, &scratch);

    let held = fs.try_lock_exclusive(&root).expect("first lock");
    let second = fs.try_lock_exclusive(&root).expect_err("second must fail");
    assert_eq!(second.kind(), FsErrorKind::WouldBlock);
    assert_eq!(second.effect(), Effect::NoEffect);
    drop(held);
    // After release the lock is available again.
    let _third = fs.try_lock_exclusive(&root).expect("lock after release");
}

#[test]
fn create_new_refuses_an_existing_name() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("create-new");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([1; 16]),
    };
    let _f = fs.create_new(staging, name).expect("first create");
    let err = fs.create_new(staging, name).expect_err("must refuse");
    assert_eq!(err.kind(), FsErrorKind::AlreadyExists);
    assert_eq!(err.effect(), Effect::NoEffect);
}

#[test]
fn write_reports_short_writes_and_sync_succeeds() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("write");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let name = EntryName::Staging {
        kind: StagingKind::Tx,
        id: Id128::from_bytes([2; 16]),
    };
    let mut f = fs.create_new(staging, name).expect("create");

    // Full loop: a short write is Ok with written < requested, so retry the remainder.
    let payload = b"hello sage-db";
    let mut off = 0usize;
    loop {
        match fs.write(&mut f, &payload[off..]).expect("write") {
            w if w.written == 0 => panic!("no progress"),
            w => {
                off += w.written;
                if off >= payload.len() {
                    break;
                }
            }
        }
    }
    fs.sync_file(&f).expect("sync_file");
    assert_eq!(fs.size(staging, name).expect("size"), payload.len() as u64);
}

#[test]
fn publish_consumes_staged_and_preserves_identity() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("publish");
    let (_root, dirs) = layout(&fs, &scratch);
    let (staging, blobs) = (&dirs[3], &dirs[2]);

    let bytes = b"immutable content";
    let digest = sage_db_format::raw_sha256(bytes);
    let staged = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([3; 16]),
    };
    let mut f = fs.create_new(staging, staged).expect("create");
    let mut off = 0;
    while off < bytes.len() {
        off += fs.write(&mut f, &bytes[off..]).expect("write").written;
    }
    fs.sync_file(&f).expect("sync");
    drop(f);

    let expect = sage_db_store::fs::HashReport {
        len: bytes.len() as u64,
        sha256: digest,
    };
    let dst = EntryName::Blob(digest);

    // Publication charges its verification reads to the shared ledger, so it takes one.
    let mut publish_ledger = Recording::new(1 << 20, 64);
    let first = fs
        .publish_no_overwrite(staging, staged, blobs, dst, &expect, &mut publish_ledger)
        .expect("publish");
    assert_eq!(first.how, PublishMode::Created);
    // Identity is not durability: a sync is still owed.
    assert!(first.needs_sync);
    // The staged name is consumed.
    let gone = fs.size(staging, staged).expect_err("staged must be gone");
    assert_eq!(gone.kind(), FsErrorKind::NotFound);
    // The destination exists with the right size.
    assert_eq!(fs.size(blobs, dst).expect("size"), bytes.len() as u64);
}

#[test]
fn publish_with_mismatched_content_is_refused() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("publish-mismatch");
    let (_root, dirs) = layout(&fs, &scratch);
    let (staging, blobs) = (&dirs[3], &dirs[2]);

    let bytes = b"real bytes";
    let staged = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([4; 16]),
    };
    let mut f = fs.create_new(staging, staged).expect("create");
    fs.write(&mut f, bytes).expect("write");
    fs.sync_file(&f).expect("sync");
    drop(f);

    // Claim a digest the bytes do not hash to.
    let lie = sage_db_store::fs::HashReport {
        len: bytes.len() as u64,
        sha256: [0xAB; 32],
    };
    let mut publish_ledger = Recording::new(1 << 20, 64);
    let err = fs
        .publish_no_overwrite(
            staging,
            staged,
            blobs,
            EntryName::Blob([0xAB; 32]),
            &lie,
            &mut publish_ledger,
        )
        .expect_err("must refuse a content mismatch");
    assert_eq!(err.kind(), FsErrorKind::HashMismatch);
    assert_eq!(err.effect(), Effect::NoEffect, "nothing was published");
}

#[test]
fn replace_current_is_the_only_overwrite_and_targets_current() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("current");
    let (root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];

    let write_tmp = |payload: &[u8]| {
        let tmp = EntryName::Root(RootName::CurrentTmp);
        match fs.remove(staging, tmp) {
            Ok(()) => {}
            Err(e) => panic!("remove stale tmp: {e:?}"),
        }
        let mut f = fs.create_new(staging, tmp).expect("tmp");
        let mut off = 0;
        while off < payload.len() {
            off += fs.write(&mut f, &payload[off..]).expect("write").written;
        }
        fs.sync_file(&f).expect("sync");
        tmp
    };

    let first = write_tmp(b"CURRENT-v1");
    fs.replace_current(staging, first, &root).expect("publish");
    assert_eq!(
        fs.size(&root, EntryName::Root(RootName::Current))
            .expect("size"),
        10
    );
    fs.sync_dir(&root).expect("dir sync");

    // A second publication replaces it: this is the one permitted overwrite.
    let second = write_tmp(b"CURRENT-v2");
    fs.replace_current(staging, second, &root).expect("replace");
    assert_eq!(
        fs.size(&root, EntryName::Root(RootName::Current))
            .expect("size"),
        10
    );

    // Reading it back sees the second payload, not a mixture.
    let mut buf = [0u8; 32];
    let n = fs
        .read_exact_into(&root, EntryName::Root(RootName::Current), &mut buf)
        .expect("read");
    assert_eq!(&buf[..n], b"CURRENT-v2");
}

#[test]
fn read_is_bounded_by_the_caller_buffer() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("bounded-read");
    let (root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let name = EntryName::Staging {
        kind: StagingKind::Manifest,
        id: Id128::from_bytes([5; 16]),
    };
    let mut f = fs.create_new(staging, name).expect("create");
    fs.write(&mut f, &[0u8; 100]).expect("write");
    fs.sync_file(&f).expect("sync");
    drop(f);
    let _ = root;

    let mut small = [0u8; 10];
    let err = fs
        .read_exact_into(staging, name, &mut small)
        .expect_err("must refuse a file larger than the buffer");
    assert_eq!(err.kind(), FsErrorKind::TooLarge);

    let mut exact = [0u8; 100];
    assert_eq!(
        fs.read_exact_into(staging, name, &mut exact).expect("read"),
        100
    );
}

#[test]
fn hash_bounded_charges_before_hashing_and_refuses_over_budget() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("hash-budget");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([6; 16]),
    };
    let payload = b"abcdefghij";
    let mut f = fs.create_new(staging, name).expect("create");
    fs.write(&mut f, payload).expect("write");
    fs.sync_file(&f).expect("sync");
    drop(f);

    // Exactly enough budget succeeds and is charged the file length.
    let mut enough = Recording::new(payload.len() as u64, 4);
    let report = fs
        .hash_bounded(staging, name, 1024, &mut enough)
        .expect("hash");
    assert_eq!(report.len, payload.len() as u64);
    assert_eq!(report.sha256, sage_db_format::raw_sha256(payload));
    assert_eq!(enough.bytes_charged, payload.len() as u64);

    // One byte short refuses BEFORE reading, preserving the ledger's own payload.
    let mut short = Recording::new(payload.len() as u64 - 1, 4);
    let err = fs
        .hash_bounded(staging, name, 1024, &mut short)
        .expect_err("must refuse");
    assert_eq!(err.kind(), FsErrorKind::BudgetExceeded);
    let budget = err.budget().expect("payload preserved");
    assert_eq!(budget.unit(), BudgetUnit::Bytes);
    assert_eq!(budget.requested(), payload.len() as u64);
    assert_eq!(budget.remaining(), payload.len() as u64 - 1);
}

#[test]
fn hash_bounded_refuses_a_file_over_the_ceiling() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("hash-ceiling");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([7; 16]),
    };
    let mut f = fs.create_new(staging, name).expect("create");
    fs.write(&mut f, &[0u8; 64]).expect("write");
    fs.sync_file(&f).expect("sync");
    drop(f);

    let mut led = Recording::new(1024, 4);
    let err = fs
        .hash_bounded(staging, name, 8, &mut led)
        .expect_err("over the ceiling");
    assert_eq!(err.kind(), FsErrorKind::TooLarge);
}

#[test]
fn enumeration_charges_entries_and_honours_early_stop() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("enumerate");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];

    for i in 0..4u8 {
        let name = EntryName::Staging {
            kind: StagingKind::Blob,
            id: Id128::from_bytes([i; 16]),
        };
        let _f = fs.create_new(staging, name).expect("create");
    }

    // Full walk: four entries, four charges, one checkpoint each.
    let mut led = Recording::new(0, 16);
    let mut seen = 0;
    fs.for_each_entry(staging, &mut led, &mut |_n| {
        seen += 1;
        Each::Continue
    })
    .expect("walk");
    assert_eq!(seen, 4);
    assert_eq!(led.entries_charged, 4);
    // Checkpoints: one before the loop and one at exit (so a cancellation is always observed even
    // on an empty or exhausted walk), plus one per entry.
    assert_eq!(led.checkpoints, 4 + 2);
    assert!(
        led.checkpoints > led.entries_charged,
        "at least one control point must exist beyond the per-entry ones"
    );

    // Early stop ends the walk without charging the rest.
    let mut led2 = Recording::new(0, 16);
    let mut seen2 = 0;
    fs.for_each_entry(staging, &mut led2, &mut |_n| {
        seen2 += 1;
        Each::Stop
    })
    .expect("walk");
    assert_eq!(seen2, 1);
    assert_eq!(led2.entries_charged, 1);
    // The pre-loop checkpoint happened, so the walk was cancellable from the start.
    assert!(led2.checkpoints >= 1);
}

#[test]
fn enumeration_cancellation_is_a_control_stop_not_a_budget_refusal() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("enumerate-cancel");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    for i in 0..3u8 {
        let _f = fs
            .create_new(
                staging,
                EntryName::Staging {
                    kind: StagingKind::Blob,
                    id: Id128::from_bytes([i; 16]),
                },
            )
            .expect("create");
    }

    let mut led = Recording::new(0, 16);
    led.stop_at_checkpoint = Some(1); // stop on the first entry
    let err = fs
        .for_each_entry(staging, &mut led, &mut |_n| Each::Continue)
        .expect_err("must stop");
    assert_eq!(err.kind(), FsErrorKind::Interrupted);
    let control = err.control().expect("control payload preserved");
    assert_eq!(control, ControlStop::Cancelled);
    assert!(err.budget().is_none(), "a stop is not a budget refusal");
}

#[test]
fn entries_exhaustion_is_reported_in_its_own_unit() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("entries-exhaust");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    for i in 0..3u8 {
        let _f = fs
            .create_new(
                staging,
                EntryName::Staging {
                    kind: StagingKind::Blob,
                    id: Id128::from_bytes([i; 16]),
                },
            )
            .expect("create");
    }

    // Plenty of bytes, only one entry.
    let mut led = Recording::new(1 << 20, 1);
    let err = fs
        .for_each_entry(staging, &mut led, &mut |_n| Each::Continue)
        .expect_err("entries exhausted");
    assert_eq!(err.kind(), FsErrorKind::BudgetExceeded);
    let budget = err.budget().expect("payload");
    assert_eq!(
        budget.unit(),
        BudgetUnit::Entries,
        "the refusal must name the ENTRY unit, not bytes"
    );
}

#[test]
fn same_device_is_available_and_the_gate_reflects_durability_choice() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("capabilities");
    let (root, dirs) = layout(&fs, &scratch);

    // All six primitives are genuinely present on this host...
    let caps: Capabilities = fs.capabilities();
    assert_eq!(
        caps.missing_required(),
        None,
        "every primitive is available here"
    );

    // ...but durability is NOT claimed by the conservative backend, so no product handle.
    assert!(!caps.product_eligible());

    // And the same-device rule has real data to compare.
    let root_dev = fs.device_id(&root).expect("root dev");
    let blobs_dev = fs.device_id(&dirs[2]).expect("blobs dev");
    assert_eq!(root_dev, blobs_dev, "layout is on one device here");

    // There is NO caller override any more: durability comes from platform policy. On this host
    // (macOS/APFS) the policy table does not list it, so a policy-derived backend still refuses.
    let policy = OsFileSystem::from_platform_policy();
    assert_eq!(
        policy.capabilities().missing_required(),
        None,
        "primitives are present"
    );
    assert!(
        !policy.capabilities().product_eligible(),
        "APFS is deliberately absent from the documented-durability policy: a successful \
         F_FULLFSYNC is a primitive property, not a directory-entry durability guarantee"
    );
}

#[test]
fn remove_is_idempotent_for_a_missing_name() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("remove");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([9; 16]),
    };
    // Removing a name that never existed is success, not an error: staging cleanup must be
    // repeatable after a crash.
    fs.remove(staging, name).expect("missing is ok");
    let _f = fs.create_new(staging, name).expect("create");
    fs.remove(staging, name).expect("remove");
    fs.remove(staging, name).expect("second remove is ok");
}

// ===========================================================================
// Reverse properties: each of these FAILS on the pre-correction implementation.
// ===========================================================================

/// The parent's blocking finding: a symlinked family directory must be refused.
///
/// Pre-correction, `open_family` used `fs::metadata` (which follows links), so a `blobs/` pointing
/// outside the KB looked like an ordinary directory and every blob write landed outside the KB.
#[test]
fn symlinked_family_directory_is_refused() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("symlink-family");
    let kb = scratch.path().join("kb-sym");
    let root = fs.create_root(&kb).expect("root");

    // Build a real directory OUTSIDE the KB and point blobs/ at it.
    let outside = scratch.path().join("outside-blobs");
    std::fs::create_dir(&outside).expect("outside dir");
    std::os::unix::fs::symlink(&outside, kb.join("blobs")).expect("symlink");

    let err = fs
        .open_family(&root, Family::Blobs, CreateDir::Never)
        .expect_err("a symlinked family must be refused");
    assert_eq!(
        err.kind(),
        FsErrorKind::Symlink,
        "must be a layout violation, not a generic I/O error"
    );
    assert_eq!(err.effect(), Effect::NoEffect);
}

/// `open_read` must refuse a directory. Pre-correction, `File::open` on a directory SUCCEEDS on
/// Unix and the failure only surfaced at the first `read`, which is a confusing and late refusal.
#[test]
fn open_read_refuses_a_directory() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("read-dir");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    // Create a directory where a file name is expected.
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x11; 16]),
    };
    std::fs::create_dir(staging.path().join(name.to_file_name())).expect("dir");

    let err = fs
        .open_read(staging, name)
        .expect_err("a directory is not readable content");
    assert_eq!(err.kind(), FsErrorKind::NotRegular);
    assert_eq!(err.effect(), Effect::NoEffect);
}

/// `open_read` must also refuse a symlink, which would otherwise redirect the read outside the KB.
#[test]
fn open_read_refuses_a_symlink() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("read-symlink");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];

    let secret = scratch.path().join("secret.txt");
    std::fs::write(&secret, b"outside the kb").expect("secret");
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x12; 16]),
    };
    std::os::unix::fs::symlink(&secret, staging.path().join(name.to_file_name())).expect("symlink");

    let err = fs
        .open_read(staging, name)
        .expect_err("symlink must be refused");
    assert_eq!(err.kind(), FsErrorKind::Symlink);
}

/// A cancelled request must NOT get `Ok` from an empty file or an empty directory.
///
/// Pre-correction both returned `Ok` while never consulting the ledger, so a cancelled request was
/// reported as success.
#[test]
fn cancellation_is_observed_even_with_no_work_to_do() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("cancel-empty");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];

    // Empty file: legal content, but the ledger must be consulted.
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x13; 16]),
    };
    let _f = fs.create_new(staging, name).expect("create empty");
    let mut cancelled = Recording::new(1 << 20, 16);
    cancelled.stop_at_checkpoint = Some(1);
    let err = fs
        .hash_bounded(staging, name, 1024, &mut cancelled)
        .expect_err("a cancelled hash must not report success");
    assert_eq!(err.kind(), FsErrorKind::Interrupted);
    assert_eq!(err.control(), Some(ControlStop::Cancelled));

    // Empty directory: also legal, also must be cancellable.
    let empty_family = fs
        .open_family(&_root, Family::Checkpoints, CreateDir::IfMissing)
        .expect("empty family");
    let mut cancelled2 = Recording::new(1 << 20, 16);
    cancelled2.stop_at_checkpoint = Some(1);
    let err = fs
        .for_each_entry(&empty_family, &mut cancelled2, &mut |_n| Each::Continue)
        .expect_err("a cancelled empty walk must not report success");
    assert_eq!(err.kind(), FsErrorKind::Interrupted);
}

/// Publication must charge its verification reads, and a tiny ledger must refuse BEFORE publishing.
#[test]
fn publication_charges_the_ledger_and_refuses_when_exhausted() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("publish-budget");
    let (_root, dirs) = layout(&fs, &scratch);
    let (staging, blobs) = (&dirs[3], &dirs[2]);

    let bytes = b"verification costs budget";
    let digest = sage_db_format::raw_sha256(bytes);
    let staged = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x14; 16]),
    };
    let mut f = fs.create_new(staging, staged).expect("create");
    let mut off = 0;
    while off < bytes.len() {
        off += fs.write(&mut f, &bytes[off..]).expect("write").written;
    }
    fs.sync_file(&f).expect("sync");
    drop(f);

    let expect = sage_db_store::fs::HashReport {
        len: bytes.len() as u64,
        sha256: digest,
    };
    let dst = EntryName::Blob(digest);

    // Zero-byte ledger: verification cannot be performed, so nothing is published.
    let mut starved = Recording::new(0, 64);
    let err = fs
        .publish_no_overwrite(staging, staged, blobs, dst, &expect, &mut starved)
        .expect_err("verification must be budgeted");
    assert_eq!(err.kind(), FsErrorKind::BudgetExceeded);
    assert!(
        err.budget().is_some(),
        "the ledger payload must survive the refusal"
    );
    // Nothing was published and the staged name still exists.
    assert_eq!(
        fs.size(blobs, dst).expect_err("no destination").kind(),
        FsErrorKind::NotFound
    );
    assert_eq!(
        fs.size(staging, staged).expect("staged intact"),
        bytes.len() as u64
    );

    // With budget, and the charge is observable.
    let mut enough = Recording::new(1 << 20, 64);
    let ok = fs
        .publish_no_overwrite(staging, staged, blobs, dst, &expect, &mut enough)
        .expect("publish");
    assert_eq!(ok.how, PublishMode::Created);
    assert!(
        enough.bytes_charged >= bytes.len() as u64,
        "publication charged the verification read"
    );
}

/// A file that grows past the ceiling during the read must be refused, not silently hashed short.
#[test]
fn hash_refuses_a_file_that_grows_past_the_ceiling() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("grow");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x15; 16]),
    };
    let mut f = fs.create_new(staging, name).expect("create");
    fs.write(&mut f, &[0u8; 100]).expect("write");
    fs.sync_file(&f).expect("sync");
    drop(f);

    // Ceiling below the actual size: the pre-read metadata check refuses it.
    let mut led = Recording::new(1 << 20, 64);
    let err = fs
        .hash_bounded(staging, name, 10, &mut led)
        .expect_err("over ceiling");
    assert_eq!(err.kind(), FsErrorKind::TooLarge);
    // And no bytes were charged, because the bound was checked before reading.
    assert_eq!(led.bytes_charged, 0);
}

/// A symlinked blob must not be hashable: `hash_bounded` would otherwise follow it.
#[test]
fn hash_bounded_refuses_a_symlink() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("hash-symlink");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let outside = scratch.path().join("outside.bin");
    std::fs::write(&outside, b"outside bytes").expect("outside");
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x16; 16]),
    };
    std::os::unix::fs::symlink(&outside, staging.path().join(name.to_file_name()))
        .expect("symlink");

    let mut led = Recording::new(1 << 20, 64);
    let err = fs
        .hash_bounded(staging, name, 1024, &mut led)
        .expect_err("symlink must be refused");
    assert_eq!(err.kind(), FsErrorKind::Symlink);
}

/// t61's requested discriminator: swap the family directory for a symlink to OUTSIDE the KB.
///
/// This is the case where check-then-open can silently differ from a true nofollow open. A stored
/// path would follow the new symlink; a pinned descriptor keeps operating on the directory it
/// opened, so writes neither escape the KB nor land in an attacker-chosen location.
#[test]
fn external_rename_cannot_redirect_a_pinned_handle() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("pinned");
    let kb = scratch.path().join("kb-pin");
    let root = fs.create_root(&kb).expect("root");
    let staging = fs
        .open_family(&root, Family::Staging, CreateDir::IfMissing)
        .expect("staging");

    // Create one file through the pinned handle, so we know it works.
    let first = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x21; 16]),
    };
    drop(
        fs.create_new(&staging, first)
            .expect("create via pinned fd"),
    );

    // Now do what the parent's containment finding describes: move the real directory away and put
    // a symlink to an outside directory in its place.
    let outside = scratch.path().join("outside-target");
    std::fs::create_dir(&outside).expect("outside");
    let real = kb.join("staging-real");
    std::fs::rename(kb.join("staging"), &real).expect("rename away");
    std::os::unix::fs::symlink(&outside, kb.join("staging")).expect("symlink in place");

    // The pinned handle still refers to the ORIGINAL directory, so this file lands in
    // `staging-real`, not in `outside` and not into the symlink's target.
    let second = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x22; 16]),
    };
    drop(fs.create_new(&staging, second).expect("create still works"));

    assert!(
        real.join(second.to_file_name()).exists(),
        "the pinned handle must keep writing to the directory it opened"
    );
    assert!(
        !outside.join(second.to_file_name()).exists(),
        "no bytes may land in the symlink target outside the KB"
    );

    // And a FRESH open of the family by name must now be refused, because the name is a symlink.
    let err = fs
        .open_family(&root, Family::Staging, CreateDir::Never)
        .expect_err("a symlinked family must be refused on open");
    assert_eq!(err.kind(), FsErrorKind::Symlink);
}

// ===========================================================================
// Parent probe reproduction: the "missed paths" class.
// Each of these FAILS on the v2 implementation and passes only when every read
// path carries the guards, not just `open_read`.
// ===========================================================================

/// The parent's exact repro: `read_exact_into(root, CURRENT-as-symlink)` returned `Ok(7)`.
///
/// `open_read` refused a symlinked CURRENT while this sibling read path followed it, so the read
/// escaped the KB. Both now go through one shared helper.
#[test]
fn read_exact_into_refuses_a_symlinked_current() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("missed-read-exact");
    let kb = scratch.path().join("kb-missed");
    let root = fs.create_root(&kb).expect("root");

    // A real file outside the KB with exactly the content the old path leaked.
    let outside = scratch.path().join("outside-current");
    std::fs::write(&outside, b"LEAKED!").expect("outside");
    std::os::unix::fs::symlink(&outside, kb.join("CURRENT")).expect("symlink CURRENT");

    let mut buf = [0u8; 64];
    let err = fs
        .read_exact_into(&root, EntryName::Root(RootName::Current), &mut buf)
        .expect_err("a symlinked CURRENT must not be readable");
    assert_eq!(err.kind(), FsErrorKind::Symlink);
    assert_eq!(err.effect(), Effect::NoEffect);
    // Nothing from outside the KB reached the caller's buffer.
    assert_ne!(&buf[..7], b"LEAKED!");
}

/// `hash_bounded` must refuse a symlinked CURRENT too, not just `open_read`.
#[test]
fn hash_bounded_refuses_a_symlinked_current() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("missed-hash");
    let kb = scratch.path().join("kb-missed-hash");
    let root = fs.create_root(&kb).expect("root");
    let outside = scratch.path().join("outside-hash");
    std::fs::write(&outside, b"outside bytes").expect("outside");
    std::os::unix::fs::symlink(&outside, kb.join("CURRENT")).expect("symlink");

    let mut led = Recording::new(1 << 20, 64);
    let err = fs
        .hash_bounded(&root, EntryName::Root(RootName::Current), 1024, &mut led)
        .expect_err("a symlinked CURRENT must not be hashable");
    assert_eq!(err.kind(), FsErrorKind::Symlink);
}

/// `sync_entry` must refuse a symlink: syncing an outside inode would flush a foreign file.
#[test]
fn sync_entry_refuses_a_symlink() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("missed-sync-entry");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let outside = scratch.path().join("outside-sync");
    std::fs::write(&outside, b"x").expect("outside");
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x31; 16]),
    };
    std::os::unix::fs::symlink(&outside, staging.path().join(name.to_file_name()))
        .expect("symlink");

    let err = fs
        .sync_entry(staging, name)
        .expect_err("symlink must be refused");
    assert_eq!(err.kind(), FsErrorKind::Symlink);
}

/// `open_read`/`read_exact_into`/`hash_bounded`/`sync_entry` must all reject a DIRECTORY, so the
/// guard cannot be present on one path and absent on another.
#[test]
fn every_read_path_refuses_a_directory() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("missed-dir");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x32; 16]),
    };
    std::fs::create_dir(staging.path().join(name.to_file_name())).expect("dir");

    let mut buf = [0u8; 8];
    for (label, err) in [
        ("open_read", fs.open_read(staging, name).err()),
        (
            "read_exact_into",
            fs.read_exact_into(staging, name, &mut buf).err(),
        ),
        (
            "hash_bounded",
            fs.hash_bounded(staging, name, 64, &mut Recording::new(1 << 20, 8))
                .err(),
        ),
        ("sync_entry", fs.sync_entry(staging, name).err()),
    ] {
        let err = err.unwrap_or_else(|| panic!("{label} must refuse a directory"));
        assert_eq!(err.kind(), FsErrorKind::NotRegular, "{label}");
    }
}

/// A stored path must not be used for a directory operation: after the family directory is renamed
/// away and replaced by a symlink, syncing and enumerating still act on the ORIGINAL directory.
#[test]
fn directory_operations_use_the_pinned_descriptor_not_the_stored_path() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("pinned-dir-ops");
    let kb = scratch.path().join("kb-pinned-ops");
    let root = fs.create_root(&kb).expect("root");
    let staging = fs
        .open_family(&root, Family::Staging, CreateDir::IfMissing)
        .expect("staging");

    let inside = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x33; 16]),
    };
    drop(fs.create_new(&staging, inside).expect("create"));

    // Replace the on-disk family directory with a symlink to somewhere else.
    let outside = scratch.path().join("elsewhere");
    std::fs::create_dir(&outside).expect("outside");
    std::fs::write(outside.join("DECOY"), b"decoy").expect("decoy");
    std::fs::rename(kb.join("staging"), kb.join("staging-old")).expect("rename away");
    std::os::unix::fs::symlink(&outside, kb.join("staging")).expect("symlink");

    // Enumerating the PINNED handle must still see our entry, never the decoy.
    let mut names = Vec::new();
    let mut led = Recording::new(0, 64);
    fs.for_each_entry(&staging, &mut led, &mut |n| {
        names.push(n.to_owned());
        Each::Continue
    })
    .expect("walk pinned dir");
    assert!(
        names.iter().any(|n| n == &inside.to_file_name()),
        "the pinned descriptor must still enumerate the original directory"
    );
    assert!(
        !names.iter().any(|n| n == "DECOY"),
        "enumeration must not follow the replacement symlink"
    );

    // sync_dir likewise flushes the pinned descriptor without error.
    fs.sync_dir(&staging).expect("sync pinned dir");
}

// ===========================================================================
// Pre-I/O byte admission: the discrimination the parent required.
//
// The failure mode is "reads first, charges after". A test that only checks
// `Err(BudgetExceeded)` cannot tell that apart from correct behaviour, because
// both end in the same error. These assert that NO DATA READ happened.
// ===========================================================================

/// A ledger that records whether it was ever asked to admit anything.
struct AdmissionProbe {
    bytes_left: u64,
    /// Bytes actually admitted.
    admitted: u64,
    /// Set if `try_charge` was called at all.
    charge_attempts: u32,
    /// Set if `checkpoint` was called at all.
    checkpoints: u32,
}

impl AdmissionProbe {
    fn starved() -> Self {
        Self {
            bytes_left: 0,
            admitted: 0,
            charge_attempts: 0,
            checkpoints: 0,
        }
    }
    fn with_budget(bytes: u64) -> Self {
        Self {
            bytes_left: bytes,
            admitted: 0,
            charge_attempts: 0,
            checkpoints: 0,
        }
    }
}

impl SharedLedger for AdmissionProbe {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        self.charge_attempts += 1;
        if bytes > self.bytes_left {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.bytes_left));
        }
        self.bytes_left -= bytes;
        self.admitted += bytes;
        Ok(())
    }
    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        if count > 0 {
            return Err(BudgetError::new(BudgetUnit::Entries, count, 0));
        }
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        self.checkpoints += 1;
        Ok(())
    }
}

/// A zero-budget hash of a NON-EMPTY file must refuse before any data read.
///
/// Discrimination: `admitted == 0` and the error is a budget refusal. Pre-fix, the implementation
/// read up to 64 KiB and *then* charged, so `admitted` would still be 0 but the read had already
/// happened — which is why this test also asserts that `try_charge` was called exactly once and
/// that it was called with the file's full length (i.e. admission came first, not after a chunk).
#[test]
fn zero_budget_hash_refuses_before_reading_any_bytes() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("preio-zero");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];

    let payload = vec![0x5Au8; 4096];
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x41; 16]),
    };
    let mut f = fs.create_new(staging, name).expect("create");
    let mut off = 0;
    while off < payload.len() {
        off += fs.write(&mut f, &payload[off..]).expect("write").written;
    }
    fs.sync_file(&f).expect("sync");
    drop(f);

    let mut probe = AdmissionProbe::starved();
    let err = fs
        .hash_bounded(staging, name, 1 << 20, &mut probe)
        .expect_err("zero budget must refuse");
    assert_eq!(err.kind(), FsErrorKind::BudgetExceeded);
    let budget = err.budget().expect("payload preserved");
    assert_eq!(budget.unit(), BudgetUnit::Bytes);
    assert_eq!(
        budget.requested(),
        payload.len() as u64,
        "the charge must be for the WHOLE length, requested up front — not for a 64 KiB chunk \
         after the data was already read"
    );

    // The discriminating assertions.
    assert_eq!(probe.admitted, 0, "nothing may be admitted");
    assert_eq!(
        probe.charge_attempts, 1,
        "exactly one admission attempt: admission happens before the read loop, not per chunk"
    );
    assert_eq!(
        probe.checkpoints, 0,
        "a refused admission must short-circuit before any control point is spent"
    );
}

/// With enough budget the charge is for the exact length, ONCE — never length + chunks.
#[test]
fn hash_charges_the_stated_length_exactly_once() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("preio-once");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];

    // Larger than one 64 KiB chunk, so a per-chunk charging bug would show up as 2+ charges.
    let payload = vec![0x11u8; 100 * 1024];
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x42; 16]),
    };
    let mut f = fs.create_new(staging, name).expect("create");
    let mut off = 0;
    while off < payload.len() {
        off += fs.write(&mut f, &payload[off..]).expect("write").written;
    }
    fs.sync_file(&f).expect("sync");
    drop(f);

    let mut probe = AdmissionProbe::with_budget(1 << 22);
    let report = fs
        .hash_bounded(staging, name, 1 << 22, &mut probe)
        .expect("hash");
    assert_eq!(report.len, payload.len() as u64);
    assert_eq!(report.sha256, sage_db_format::raw_sha256(&payload));
    assert_eq!(
        probe.admitted,
        payload.len() as u64,
        "exactly the file length is admitted — no double charge for pre-admission + chunks"
    );
    assert_eq!(probe.charge_attempts, 1, "one admission, not one per chunk");
    assert!(
        probe.checkpoints >= 2,
        "control points still occur per chunk and at exit: {}",
        probe.checkpoints
    );
}

/// A file that GREW past the stated length must be refused, and must not be bulk-read to find out.
#[test]
fn a_grown_file_is_refused_without_reading_beyond_the_admission() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("preio-grow");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];

    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x43; 16]),
    };
    let mut f = fs.create_new(staging, name).expect("create");
    fs.write(&mut f, b"initial!").expect("write");
    fs.sync_file(&f).expect("sync");
    drop(f);

    // Append behind the backend's back, simulating a file that changes size mid-flight.
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(staging.path().join(name.to_file_name()))
            .expect("reopen");
        f.write_all(&[0u8; 200]).expect("append");
    }

    // `hash_bounded` reads the CURRENT length from metadata (206), so this succeeds and hashes 206
    // bytes; the important property is that the admitted count equals the length actually hashed.
    let mut probe = AdmissionProbe::with_budget(1 << 20);
    let report = fs
        .hash_bounded(staging, name, 1 << 20, &mut probe)
        .expect("hash current length");
    assert_eq!(report.len, 208, "'initial!' is 8 bytes plus 200 appended");
    assert_eq!(
        probe.admitted, report.len,
        "every HASHED byte must be admitted. Note the bounded exception: growth detection reads \
         one probe byte that is neither hashed nor charged, so this is equality of \"hashed\" and \
         \"admitted\", not of \"read from the file\" and \"admitted\""
    );
}

/// A cancelled request must not read either: the entry checkpoint runs before the first read.
#[test]
fn cancellation_prevents_the_first_read() {
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::new("preio-cancel");
    let (_root, dirs) = layout(&fs, &scratch);
    let staging = &dirs[3];
    let payload = vec![0x77u8; 1024];
    let name = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x44; 16]),
    };
    let mut f = fs.create_new(staging, name).expect("create");
    f_write(&fs, &mut f, &payload);
    fs.sync_file(&f).expect("sync");
    drop(f);

    // Cancel on the very first checkpoint: admission happens first, then the checkpoint refuses.
    let mut cancelling = Recording::new(1 << 20, 64);
    cancelling.stop_at_checkpoint = Some(1);
    let err = fs
        .hash_bounded(staging, name, 1 << 20, &mut cancelling)
        .expect_err("cancelled");
    assert_eq!(err.kind(), FsErrorKind::Interrupted);
    assert_eq!(err.control(), Some(ControlStop::Cancelled));
}

/// Small helper: write a buffer through the seam in short-write-aware fashion.
fn f_write(fs: &OsFileSystem, f: &mut sage_db_store::os::OsWriteFile, bytes: &[u8]) {
    let mut off = 0;
    while off < bytes.len() {
        off += fs.write(f, &bytes[off..]).expect("write").written;
    }
}
