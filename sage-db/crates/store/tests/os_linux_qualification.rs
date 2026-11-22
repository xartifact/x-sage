//! Linux/Btrfs platform policy: constructor contract and qualifying-host integration.
//!
//! Two halves, deliberately separated so a wrong host cannot produce a false pass:
//!
//! # 1. Constructor contract (runs everywhere)
//!
//! What a caller can observe about the three constructors, on any host: `conservative()` and
//! `from_platform_policy()` never claim durability and never claim to have validated a root; a
//! relative root, a missing root and a root under a missing parent are refused with their own
//! reasons; and calling `for_root` on **this** host cannot silently produce a claim. That last
//! one is the false-positive check the release requires: on macOS/APFS the constructor must
//! refuse, and if it ever succeeds the assertions below require the compile-time target to be
//! `linux/x86_64`, so an accidental pass elsewhere is a failure rather than silence.
//!
//! The mountinfo parser and the option matcher are private and are covered in-crate
//! (`os::policy_tests`), so no test-only accessor is published.
//!
//! # 2. Real backend on a qualifying host (`#[ignore]`, run explicitly on x99)
//!
//! `/var/tmp` on the target host is on the same Btrfs device as `/opt/xartifact`, so a fixture
//! there exercises the real kernel path against the same filesystem the product would use, and
//! never touches the production directory. Each `#[ignore]`d test **asserts the platform facts
//! first and fails rather than skipping** when they do not hold: a green run means the
//! assertions executed.
//!
//! What this file does not claim: physical power-loss behaviour, and a full Store-level
//! bootstrap/commit/reopen matrix. A killed process and a reopened root say that committed state
//! survives process death and that no partial transaction is visible; they say nothing about a
//! device losing power mid-flush. The Store-level sequence is Root's to bind, because it needs
//! the canonical transaction fixtures that the store crate cannot depend on.
//!
//! Run the qualifying half with:
//!
//! ```text
//! cargo test -p sage-db-store --test os_linux_qualification -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};

use sage_db_store::fs::{CreateDir, Effect, Family, FileSystem, FsErrorKind, PlatformCapability};
use sage_db_store::names::{EntryName, RootName};
use sage_db_store::os::{DurabilityBasis, OsFileSystem, PlatformPolicyError};
use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::query::ControlStop;

// ---------------------------------------------------------------------------
// Ledger doubles
// ---------------------------------------------------------------------------

/// An accepting ledger with finite allowances, so admission is observable.
struct Budget {
    bytes: u64,
    entries: u64,
}

impl Budget {
    const fn new(bytes: u64, entries: u64) -> Self {
        Self { bytes, entries }
    }
}

impl SharedLedger for Budget {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        if bytes > self.bytes {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.bytes));
        }
        self.bytes -= bytes;
        Ok(())
    }
    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
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

// ---------------------------------------------------------------------------
// Scratch directories
// ---------------------------------------------------------------------------

/// A scratch directory on the real filesystem that removes itself.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn under(parent: &Path, tag: &str) -> Self {
        let unique = format!(
            "sage-db-platform-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = parent.join(unique);
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

// ---------------------------------------------------------------------------
// 1. Constructor contract — host-independent in shape
// ---------------------------------------------------------------------------

#[test]
fn conservative_and_platform_policy_never_claim_durability() {
    // Both are host-independent: whatever the platform, neither bound a filesystem, so neither may
    // claim one. This is the regression that stops `from_platform_policy` from becoming
    // "Linux is durable".
    for fs in [
        OsFileSystem::conservative(),
        OsFileSystem::from_platform_policy(),
    ] {
        assert_eq!(fs.durability(), DurabilityBasis::NotClaimed);
        assert!(
            fs.validated_root().is_none(),
            "no root was validated, so none may be reported"
        );
        let capabilities = fs.capabilities();
        assert!(
            !capabilities.durable_sync,
            "unvalidated backends must not report durable_sync"
        );
        assert!(
            !capabilities.product_eligible(),
            "and therefore cannot open a product handle"
        );
        // The primitives themselves are still reported: the refusal is about durability, not about
        // pretending the syscalls are missing.
        for required in PlatformCapability::REQUIRED_FOR_PRODUCT {
            assert!(
                capabilities.supported.contains(&required),
                "{required:?} is genuinely available on this backend"
            );
        }
    }
}

#[test]
fn a_relative_root_is_refused_before_any_platform_read() {
    let error = OsFileSystem::for_root(Path::new("kb/relative"))
        .expect_err("a relative root cannot be bound to a mount");
    match error {
        PlatformPolicyError::RelativeRoot => {}
        // On a non-Linux build the target check runs first and is the honest answer.
        PlatformPolicyError::UnsupportedTarget { .. } => {}
        other => panic!("unexpected refusal for a relative root: {other:?}"),
    }
}

#[test]
fn a_missing_root_with_a_missing_parent_is_refused() {
    let error = OsFileSystem::for_root(Path::new("/nonexistent-sage-db-parent/child"))
        .expect_err("a root under a missing parent cannot be validated");
    match error {
        PlatformPolicyError::RootMissing => {}
        // The path is absolute; on a host without a policy the target check answers first.
        PlatformPolicyError::UnsupportedTarget { .. } => {}
        other => panic!("unexpected refusal: {other:?}"),
    }
}

#[test]
fn an_existing_file_is_not_a_root() {
    let scratch = Scratch::under(&std::env::temp_dir(), "file-root");
    let file = scratch.path().join("not-a-directory");
    std::fs::write(&file, b"x").expect("write a file");
    let error =
        OsFileSystem::for_root(&file).expect_err("a regular file cannot be a root directory");
    match error {
        PlatformPolicyError::RootNotADirectory => {}
        PlatformPolicyError::UnsupportedTarget { .. } => {}
        other => panic!("unexpected refusal for a file root: {other:?}"),
    }
}

#[test]
fn this_host_is_either_qualified_or_honestly_refused() {
    // The false-positive check: calling the constructor on **this** host must never produce a
    // durability claim unless the host really is inside the policy bound.
    let scratch = Scratch::under(&std::env::temp_dir(), "policy-probe");
    match OsFileSystem::for_root(scratch.path()) {
        Ok(fs) => {
            assert_eq!(
                fs.durability(),
                DurabilityBasis::Documented,
                "a successful constructor must report the basis it validated"
            );
            assert_eq!(
                fs.validated_root(),
                Some(scratch.path()),
                "the validated root must be the path that was checked"
            );
            // Runtime, not `cfg!`: clippy rejects a constant assertion, and this must be an
            // observation about the build that ran.
            assert_eq!(
                std::env::consts::OS,
                "linux",
                "only linux may ever be accepted, on any host"
            );
            assert_eq!(
                std::env::consts::ARCH,
                "x86_64",
                "only x86_64 may ever be accepted, on any host"
            );
        }
        Err(error) => {
            // The expected arm on macOS/APFS. Printing keeps the observation available in the log
            // instead of hiding which fact refused.
            eprintln!("platform policy refused this host, as designed: {error}");
            assert!(
                !matches!(error, PlatformPolicyError::RootMissing),
                "the scratch directory exists, so this cannot be a missing-root answer"
            );
        }
    }
}

#[test]
fn a_conservative_backend_accepts_any_root_and_binds_none() {
    // The guard is only meaningful once a root is bound. An unvalidated backend claims nothing, so
    // it may serve any path — that is the pre-existing behaviour the rest of the suite depends on.
    let fs = OsFileSystem::conservative();
    let scratch = Scratch::under(&std::env::temp_dir(), "guard-shape");
    let root = fs.create_root(&scratch.path().join("kb")).expect("create");
    let first = fs.device_id(&root).expect("device identity");
    let second = fs.device_id(&root).expect("device identity");
    assert_eq!(first, second, "the same directory reports one device");
    assert!(
        fs.validated_root().is_none(),
        "creating a root is not validating a platform"
    );
}

// ---------------------------------------------------------------------------
// 2. Qualifying-host integration — explicitly run on x99
// ---------------------------------------------------------------------------

/// `/var/tmp` on the target host: the same Btrfs device as the production KB root, writable by
/// the ordinary user, so no elevated path and no production directory is touched.
const QUALIFYING_PARENT: &str = "/var/tmp";

/// Assert the platform facts a qualifying run depends on.
///
/// Fails with the observation when the host is outside the bound. This is the opposite of a skip:
/// asking for qualification evidence on a non-qualifying host is a failed request.
fn require_qualifying(fs: &OsFileSystem) {
    assert_eq!(
        std::env::consts::OS,
        "linux",
        "qualification requires linux; this host is not"
    );
    assert_eq!(
        std::env::consts::ARCH,
        "x86_64",
        "qualification requires x86_64; this host is not"
    );
    assert_eq!(
        fs.durability(),
        DurabilityBasis::Documented,
        "the constructor must have accepted the qualifying root"
    );
    let release =
        std::fs::read_to_string("/proc/sys/kernel/osrelease").expect("kernel release readable");
    assert!(
        release.trim().starts_with("7.2.11-arch1-") || release.trim() == "7.3.5-arch1-1",
        "kernel {} is outside the validated bound",
        release.trim()
    );
    assert!(
        fs.validated_root().is_some(),
        "an accepted backend must report the root it bound"
    );
}

/// The device id of the directory a path names, read without going through the policy.
fn raw_device(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).expect("metadata").dev()
}

#[test]
#[ignore = "qualifying-host integration: run explicitly on x99"]
fn for_root_accepts_the_real_target_filesystem_and_the_backend_works_on_it() {
    let scratch = Scratch::under(Path::new(QUALIFYING_PARENT), "real-root");
    let fs =
        OsFileSystem::for_root(scratch.path()).expect("the qualifying filesystem must be accepted");
    require_qualifying(&fs);
    assert_eq!(fs.validated_root(), Some(scratch.path()));

    // The fixture really is on the same device as the production KB root, so this exercises the
    // filesystem the product would use rather than a different mount that happens to be Btrfs.
    assert_eq!(
        raw_device(scratch.path()),
        raw_device(Path::new("/opt/xartifact/x-sage/data/kb")),
        "/var/tmp and the production KB root must share one device"
    );

    let root = fs
        .open_root(scratch.path())
        .expect("open the validated root");
    let lock = fs
        .try_lock_exclusive(&root)
        .expect("the first lock is granted");
    let device = fs.device_id(&root).expect("device identity is readable");
    let mut ledger = Budget::new(1 << 20, 1 << 10);

    // A real file: written, flushed, read back through the guarded read path, then hashed.
    let name = EntryName::Root(RootName::CurrentTmp);
    let mut file = fs.create_new(&root, name).expect("create");
    let payload = b"platform-qualification-bytes";
    let outcome = fs.write(&mut file, payload).expect("write");
    assert_eq!(outcome.written, payload.len());
    fs.sync_file(&file).expect("file flush");
    drop(file);

    let mut buffer = [0u8; 64];
    let read = fs
        .read_exact_into(&root, name, &mut buffer)
        .expect("read back");
    assert_eq!(&buffer[..read], payload);

    let report = fs
        .hash_bounded(&root, name, payload.len() as u64, &mut ledger)
        .expect("bounded hash");
    assert_eq!(report.len, payload.len() as u64);
    assert_eq!(report.sha256, sage_db_format::digest::raw_sha256(payload));

    // Directory flush of the pinned descriptor — the half `fsync(2)` on a file does not cover.
    fs.sync_dir(&root).expect("directory flush");

    // A walk sees the created entry as a name, not as an engine identity.
    let mut names = Vec::new();
    fs.for_each_entry(&root, &mut ledger, &mut |name| {
        names.push(name.to_owned());
        sage_db_store::fs::Each::Continue
    })
    .expect("walk the root");
    assert!(
        names.iter().any(|seen| seen == "CURRENT.tmp"),
        "the created entry is visible to a walk: {names:?}"
    );

    // The same-filesystem rule still holds on the real backend: a family created under the root
    // reports the root's device.
    let family = fs
        .open_family(&root, Family::Blobs, CreateDir::IfMissing)
        .expect("create a family on the real backend");
    assert_eq!(
        fs.device_id(&family).expect("family device"),
        device,
        "the family must live on the same device as the root"
    );

    // Releasing the lock and taking it again is legal; a second holder is not (the separate
    // process test covers that half).
    drop(lock);
    let _relocked = fs
        .try_lock_exclusive(&root)
        .expect("the lock is released with its descriptor");
}

#[test]
#[ignore = "qualifying-host integration: run explicitly on x99"]
fn for_root_binds_one_root_and_refuses_another() {
    let first = Scratch::under(Path::new(QUALIFYING_PARENT), "bind-a");
    let second = Scratch::under(Path::new(QUALIFYING_PARENT), "bind-b");
    let fs = OsFileSystem::for_root(first.path()).expect("first root qualifies");
    require_qualifying(&fs);

    fs.open_root(first.path())
        .expect("the validated root opens");

    // Any other root is refused with `Unsupported`, and the refusal claims no effect: nothing was
    // opened, written or flushed for a path this backend did not validate.
    let error = fs
        .open_root(second.path())
        .expect_err("an unvalidated root must be refused");
    assert_eq!(error.kind(), FsErrorKind::Unsupported);
    assert_eq!(error.effect(), Effect::NoEffect);

    // Creating outside the validated parent is refused for the same reason.
    let outside = second.path().join("kb");
    let error = fs
        .create_root(&outside)
        .expect_err("an unvalidated parent must be refused");
    assert_eq!(error.kind(), FsErrorKind::Unsupported);
    assert_eq!(error.effect(), Effect::NoEffect);
    assert!(
        !outside.exists(),
        "a refused creation must not have made the directory"
    );
}

#[test]
#[ignore = "qualifying-host integration: run explicitly on x99"]
fn for_root_validates_the_parent_when_the_root_does_not_exist_yet() {
    let scratch = Scratch::under(Path::new(QUALIFYING_PARENT), "init-parent");
    let target = scratch.path().join("kb");
    let fs = OsFileSystem::for_root(&target).expect("the parent qualifies for initialization");
    require_qualifying(&fs);
    // Two distinct facts, both load-bearing: `validated_root` is the path the caller asked about
    // (the not-yet-created child); `bound_directory` is what was actually pinned (its parent).
    // Conflating them let a validated backend serve the parent as if it were the root.
    assert_eq!(
        fs.validated_root(),
        Some(target.as_path()),
        "the accepted root is the path the caller asked about"
    );
    assert_eq!(
        fs.bound_directory(),
        Some(scratch.path()),
        "the pinned directory is the parent that exists"
    );

    let root = fs.create_root(&target).expect("create the root");
    assert_eq!(
        raw_device(&target),
        raw_device(Path::new(QUALIFYING_PARENT)),
        "a creation through the pinned parent stays on the validated device"
    );
    fs.sync_parent_of(&target).expect("flush the parent");

    let reopened = fs.open_root(&target).expect("reopen the root");
    assert_eq!(
        fs.device_id(&root).expect("device"),
        fs.device_id(&reopened).expect("device")
    );

    // A second creation of the same name is `AlreadyExists` with no effect claimed.
    let error = fs.create_root(&target).expect_err("the name exists now");
    assert_eq!(error.kind(), FsErrorKind::AlreadyExists);
    assert_eq!(error.effect(), Effect::NoEffect);
}

/// The R2 property, proven against the real filesystem: after the parent is replaced at its path,
/// a flush still addresses the directory that was pinned — and the guard refuses rather than
/// silently retargeting.
///
/// Two facts are separated here, because only the second one is the fix:
///
/// * a held descriptor keeps pointing at the original inode after the path is re-pointed, so a
///   flush through it cannot be redirected;
/// * the backend's identity check notices the replacement, so a validated backend never flushes a
///   directory that appeared at the path afterwards.
#[test]
#[ignore = "qualifying-host integration: run explicitly on x99"]
fn sync_parent_is_descriptor_bound_not_path_bound() {
    let scratch = Scratch::under(Path::new(QUALIFYING_PARENT), "parent-replaced");
    let target = scratch.path().join("kb");
    let fs = OsFileSystem::for_root(&target).expect("the parent qualifies for initialization");
    require_qualifying(&fs);
    assert_eq!(
        fs.bound_directory(),
        Some(scratch.path()),
        "the parent is what was pinned"
    );

    // Create through the pinned descriptor, then flush — the normal init sequence.
    fs.create_root(&target).expect("create the root");
    fs.sync_parent_of(&target).expect("flush the pinned parent");

    // Replace the parent AT ITS PATH with a different directory on the same mount: the
    // same-mount substitution an mnt_id comparison cannot see. The root path stays textually
    // identical; only the directory it resolves through changes.
    let moved = scratch.path().with_extension("moved");
    std::fs::rename(scratch.path(), &moved).expect("move the validated parent aside");
    std::fs::create_dir(scratch.path()).expect("put a different directory at the same path");

    // The backend still holds the descriptor for the original parent, so a flush of "the parent"
    // remains descriptor-bound. The identity check decides whether it is allowed: either the flush
    // succeeds against the original inode, or it refuses. It can never flush the replacement.
    match fs.sync_parent_of(&target) {
        Ok(()) => {}
        Err(error) => {
            assert_eq!(
                error.kind(),
                FsErrorKind::Unsupported,
                "a refused flush must be a policy verdict, not an I/O verdict: {error:?}"
            );
            assert_eq!(error.effect(), Effect::NoEffect);
        }
    }

    // A NEW backend on the same path binds the replacement: each backend binds what it checked.
    // That is the boundary — the claim covers one construction, not a path forever.
    let rebound = OsFileSystem::for_root(&target)
        .expect("the replacement directory is still on the qualifying filesystem");
    assert_eq!(
        rebound.bound_directory(),
        Some(scratch.path()),
        "a new constructor pins the directory present at its own construction time"
    );

    let _ = std::fs::remove_dir_all(&moved);
}

/// A checked backend must still initialize a root that **already exists and is empty**.
///
/// This is the semantic control for the M1 defect: the earlier guard treated the bound *root* as if
/// it were the parent, so `create_root(existing)` answered `Unsupported` instead of the normal
/// `AlreadyExists`, and `bootstrap(CreateDir::IfMissing)` — which relies on exactly that
/// `AlreadyExists` → `open_root` fallback — could not initialize a pre-created directory.
///
/// `init` permits an existing empty root by design and `for_root` accepts an existing path, so the
/// two must compose. This asserts the seam contract first, then drives the real Store bootstrap
/// over the real filesystem, because only the second one proves the composition.
#[test]
#[ignore = "qualifying-host integration: run explicitly on x99"]
fn an_existing_empty_root_still_initializes_through_the_checked_backend() {
    use sage_db_store::init::bootstrap;
    use sage_db_store::lease::AttemptLedger;
    use sage_db_types::framing::Limits;
    use sage_db_types::identity::KbUuid;
    use sage_db_types::query::RunToCompletion;
    use sage_db_types::relation::RuleProfile;

    let scratch = Scratch::under(Path::new(QUALIFYING_PARENT), "existing-empty-root");
    // The caller pre-creates the root and leaves it empty — exactly what the harness did.
    let target = scratch.path().join("kb");
    std::fs::create_dir(&target).expect("pre-create the root");

    let checked =
        OsFileSystem::for_root(&target).expect("an existing empty root is a valid context");
    require_qualifying(&checked);
    assert_eq!(
        checked.validated_root(),
        Some(target.as_path()),
        "the existing directory itself was validated"
    );
    assert_eq!(
        checked.bound_directory(),
        Some(target.as_path()),
        "for an existing root the bound directory is the root, not its parent"
    );

    // Seam contract: creating an existing root reports the truth, not a policy refusal.
    let error = checked
        .create_root(&target)
        .expect_err("the directory already exists");
    assert_eq!(
        error.kind(),
        FsErrorKind::AlreadyExists,
        "an existing root must answer AlreadyExists, never Unsupported"
    );
    assert_eq!(
        error.effect(),
        Effect::NoEffect,
        "nothing was created by the refused call"
    );
    // ... and the fallback `init` performs must work on it.
    checked.open_root(&target).expect("open the existing root");
    checked
        .sync_parent_of(&target)
        .expect("flush the parent of the existing root");

    // Composition: the real Store bootstrap over the real filesystem.
    let control = RunToCompletion;
    let mut ledger =
        AttemptLedger::open(64 * 1024 * 1024, 1_000_000, &control).expect("finite ledger");
    let rules = sage_db_format::rules::CanonicalRules::new(RuleProfile::FormalV1)
        .encode(&mut ledger)
        .expect("canonical rules bytes");
    let store = bootstrap(
        OsFileSystem::for_root(&target).expect("checked backend"),
        &target,
        CreateDir::IfMissing,
        KbUuid::from_bytes([0x7A; 16]),
        &rules,
        &Limits::V1,
        &mut ledger,
    );
    match store {
        Ok(store) => {
            assert_eq!(
                store.head(),
                Some(1),
                "Genesis committed on the pre-created empty root"
            );
            assert!(
                store.coverage_complete(),
                "the committed root is usable, so this is real initialization"
            );
        }
        Err(refusal) => {
            panic!("bootstrap must initialize a pre-created empty root; refused: {refusal:?}")
        }
    }
}

#[test]
#[ignore = "qualifying-host integration: run explicitly on x99"]
fn a_non_qualifying_filesystem_on_the_same_host_is_refused() {
    // `/tmp` on the target host is tmpfs. Same kernel, same user, same binary, different
    // filesystem: the policy must refuse it, which is what proves the check is about the mounted
    // filesystem and not about the host.
    let scratch = Scratch::under(Path::new("/tmp"), "tmpfs-refusal");
    match OsFileSystem::for_root(scratch.path()) {
        Ok(fs) => panic!(
            "tmpfs must not be accepted; the constructor claimed {:?}",
            fs.durability()
        ),
        Err(PlatformPolicyError::UnsupportedFilesystem { observed, accepted }) => {
            assert_eq!(observed, "tmpfs");
            assert_eq!(accepted, "btrfs");
        }
        Err(other) => panic!("expected an unsupported-filesystem refusal, got {other:?}"),
    }
}
