//! `ModelFs` — the independent model implementing the **real** `sage_db_store::fs::FileSystem`.
//!
//! This file is the single binding point between the trait-free model in the parent module and the
//! production seam. Nothing else in this crate names the trait, so a seam change is a change here
//! and nowhere else.
//!
//! # The two contracts this adapter relies on, both stated by the seam
//!
//! 1. **`Ok(())` from a sync means "exactly the state visible at the moment of the call is
//!    durable"** — not a partial flush, and nothing after the call is implied. The adapter
//!    therefore snapshots visible state at the instant of each successful sync; it never invents
//!    a durable frontier the seam did not promise.
//! 2. **A write error means `Effect::Unknown`, not "nothing was written"**, when a partial prefix
//!    could have been accepted; and `Ok(WriteOutcome { written })` with `written < buf.len()` is a
//!    legal short write whose bytes are visible but not durable.
//!
//! Consequences the model must be able to represent, and does: content durable while its name is
//! not; a directory's own binding durable only through its parent; and a publish that consumes the
//! staged name while `needs_sync` still reports that durability is owed.
//!
//! Handles are opaque indices: the trait leaves the handle types to the backend, and modelling
//! them as indices keeps this file free of platform detail. The trait takes `&self` throughout, so
//! mutable model state lives behind a `RefCell` — this is a test backend, not production code, and
//! the borrow is never held across a callback that could re-enter.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use sage_db_store::fs::{
    Capabilities, CreateDir, Each, Effect, Family, FileSystem, FsError, FsErrorKind, HashReport,
    PlatformCapability, PublishMode, Published, WriteOutcome,
};
use sage_db_store::names::{EntryName, RootName};
use sage_db_types::ledger::{BudgetError, SharedLedger};
use sage_db_types::query::ControlStop;

use super::{DurableState, ModelFs, RelPath, RestoreError};
use crate::faults::{FaultPoint, FaultSchedule, Injection};

thread_local! {
    static COPY_PROBE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Read the current retained-copy count.
///
/// **It has no increment site in this implementation, deliberately.** `hash_bounded` holds the
/// model's `Ref` and hashes the borrowed slice, so nothing is retained and the counter never moves.
///
/// **Scope of this probe, stated precisely.** Nothing in this implementation increments the
/// counter: `hash_bounded` borrows the model's bytes and never retains a copy. So the value staying
/// zero is a property of the *current* borrow-only code, not an allocator oracle. It was
/// demonstrated to be discriminating only against one specific injected mutation (a `to_vec()`
/// added before admission, which called an increment site). An arbitrary copy introduced elsewhere
/// would not be observed, and no test here should be read as proving "no allocation ever happens".
#[must_use]
pub fn copies_made() -> u64 {
    COPY_PROBE.with(std::cell::Cell::get)
}

/// A directory handle: an index into the model's path table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DirHandle(pub usize);

/// A writable file handle: the inode it appends to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WriteHandle(pub u64);

/// A readable file handle: an opaque token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
/// An open read handle: the inode plus the cursor.
///
/// The cursor is the point. `read` copied `bytes[..n]` on every call, so it never advanced and never
/// reported EOF for a non-empty file. Any reader that loops "read until 0" - the commit path does when
/// hashing a rules blob - then accumulated more bytes than the file holds and produced a spurious
/// `BlobMismatch`. A `ReadFile` is a cursor; a bare inode is not this seam's shape.
pub struct ReadHandle {
    /// The inode being read.
    pub inode: u64,
    /// Bytes already consumed by THIS handle. Per-handle, never global.
    pub offset: usize,
}

impl ReadHandle {
    /// A fresh handle over `inode`, positioned at the start.
    #[must_use]
    pub const fn new(inode: u64) -> Self {
        Self { inode, offset: 0 }
    }
}

/// An owned exclusive-lock guard for one modelled root.
///
/// Holding it is what makes a second `try_lock_exclusive` refuse: the guard owns a token in the shared
/// per-root lock registry, and dropping it returns the token. `Clone` is deliberately NOT implemented —
/// a copyable lock handle cannot express exclusivity, and the previous unconditional fresh-ID handle
/// is exactly what made this model unable to represent contention.
///
/// This is a test-model lock: it models the seam's *observable* exclusivity contract (second attempt
/// refuses with no effect, release makes it available again). It is not an OS `flock`, and no claim
/// about kernel lock durability follows from it.
#[derive(Debug)]
pub struct LockHandle {
    /// Which modelled root this guard holds.
    root: PathBuf,
    /// The registry this token must be returned to.
    registry: std::rc::Rc<RefCell<BTreeSet<PathBuf>>>,
    /// The token, kept for identity in test assertions.
    id: u64,
}

impl LockHandle {
    /// The held root, for tests that need to state which root is locked.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The lock token identity.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for LockHandle {
    fn drop(&mut self) {
        // Release is the ONLY way a second lock can succeed: there is no timeout and no forced takeover.
        self.registry.borrow_mut().remove(&self.root);
    }
}

/// Declared capability profile for a simulated backend.
///
/// A fault model must be able to simulate a backend that lacks a capability, so the profile is
/// data rather than a compile-time constant. Note the two independent parts: a host can expose
/// every primitive and still not honour the durability contract.
#[derive(Clone, Debug)]
pub struct ModelCapabilities {
    /// Capabilities the simulated backend affirmatively supports.
    pub supported: Vec<PlatformCapability>,
    /// Whether the simulated backend's sync primitives honour the declared durability model.
    pub durable_sync: bool,
}

impl ModelCapabilities {
    /// Every capability, durable: the profile a protocol test uses by default.
    #[must_use]
    pub fn full() -> Self {
        Self {
            supported: vec![
                PlatformCapability::ExclusiveLock,
                PlatformCapability::FileSync,
                PlatformCapability::DirSync,
                PlatformCapability::AtomicReplace,
                PlatformCapability::NoOverwritePublish,
                PlatformCapability::SameDevice,
            ],
            durable_sync: true,
        }
    }

    /// Nothing supported and no durable sync: an unsupported host.
    #[must_use]
    pub fn unsupported() -> Self {
        Self {
            supported: Vec::new(),
            durable_sync: false,
        }
    }

    /// Every primitive present, `durable_sync` false.
    ///
    /// This is the case that separates "the syscall exists" from "the platform honours the
    /// contract", and it must be unable to produce a durable claim.
    #[must_use]
    pub fn primitives_without_durability() -> Self {
        Self {
            durable_sync: false,
            ..Self::full()
        }
    }
}

/// Inner mutable state, behind one `RefCell` because the trait takes `&self`.
#[derive(Debug)]
struct Inner {
    model: ModelFs,
    /// Root paths this backend has been told about, by handle.
    roots: BTreeMap<usize, PathBuf>,
    /// Directory handles → the model path they denote.
    dirs: BTreeMap<usize, RelPath>,
    next_handle: usize,
    next_lock: u64,
    /// Roots currently held by a live [`LockHandle`]. Shared with the guards so `Drop` can release.
    locks: std::rc::Rc<RefCell<BTreeSet<PathBuf>>>,
}

/// The model backend.
#[derive(Debug)]
pub struct ModelFileSystem {
    caps: ModelCapabilities,
    inner: RefCell<Inner>,
    /// Declared fault injections, consumed at named points.
    ///
    /// This is data, not a behaviour switch: a scenario that expects a failure declares it, and the
    /// tests assert every declared injection fired. A backend that silently failed "sometimes"
    /// would make every assertion unfalsifiable.
    faults: RefCell<FaultSchedule>,
}

impl ModelFileSystem {
    /// Wrap a fresh model with the given simulated capabilities.
    #[must_use]
    pub fn new(caps: ModelCapabilities) -> Self {
        Self {
            caps,
            inner: RefCell::new(Inner {
                model: ModelFs::new(),
                roots: BTreeMap::new(),
                dirs: BTreeMap::new(),
                next_handle: 0,
                next_lock: 0,
                locks: std::rc::Rc::new(RefCell::new(BTreeSet::new())),
            }),
            faults: RefCell::new(FaultSchedule::none()),
        }
    }

    /// Rebuild a backend over a sampled durable state, bound to an explicit virtual root path.
    ///
    /// `root_path` is the absolute path this backend will answer `open_root` for. A restored backend cannot guess it:
    /// `DurableState` is root-RELATIVE, and `open_root` only resolves paths already in the root registry. So the caller
    /// supplies the harness context explicitly, and a FRESH registry/handle is installed for that existing root.
    ///
    /// What is deliberately NOT done: no `create_root`, no missing-family creation, no path defaulting or substitution,
    /// no normalization of the sample, and no carry-over of handles, cursors, faults or locks from the live run. Fresh
    /// registration is not restoration of a previous handle identity; the relative files and directories are exactly
    /// the supplied sample.
    ///
    /// # Errors
    ///
    /// [`RestoreError`] from the pure model, or [`RestoreError::RootNotAbsolute`] when the supplied context is not
    /// absolute (an ambiguous virtual root cannot be the key of a lock or an `open_root` match).
    pub fn from_durable_state(
        caps: ModelCapabilities,
        root_path: &Path,
        state: DurableState,
    ) -> Result<Self, RestoreError> {
        if !root_path.is_absolute() {
            return Err(RestoreError::RootNotAbsolute);
        }
        let model = ModelFs::from_durable_state(state)?;
        let mut roots = BTreeMap::new();
        // Handle 0 for the restored root: fresh registration, keyed by the caller's explicit context.
        roots.insert(0usize, root_path.to_path_buf());
        Ok(Self {
            caps,
            inner: RefCell::new(Inner {
                model,
                roots,
                dirs: BTreeMap::new(),
                next_handle: 1,
                next_lock: 0,
                locks: std::rc::Rc::new(RefCell::new(BTreeSet::new())),
            }),
            faults: RefCell::new(FaultSchedule::none()),
        })
    }

    /// Whether this backend claims to be able to honour durability. Mirrors
    /// `Capabilities::product_eligible` without requiring the caller to reach into the trait.
    #[must_use]
    pub fn product_eligible(&self) -> bool {
        let required = [
            PlatformCapability::ExclusiveLock,
            PlatformCapability::FileSync,
            PlatformCapability::DirSync,
            PlatformCapability::AtomicReplace,
            PlatformCapability::NoOverwritePublish,
            PlatformCapability::SameDevice,
        ];
        self.caps.durable_sync && required.iter().all(|c| self.caps.supported.contains(c))
    }

    /// The model, for crash enumeration.
    ///
    /// # Panics
    ///
    /// Panics if called while a backend method holds the borrow, which would be a harness bug:
    /// every method releases the borrow before returning.
    #[must_use]
    pub fn model(&self) -> std::cell::Ref<'_, ModelFs> {
        std::cell::Ref::map(self.inner.borrow(), |i| &i.model)
    }

    /// Mutable model access, for arranging a test's initial state.
    pub fn model_mut(&mut self) -> &mut ModelFs {
        &mut self.inner.get_mut().model
    }

    /// Install a declared fault schedule.
    pub fn with_faults(self, faults: FaultSchedule) -> Self {
        *self.faults.borrow_mut() = faults;
        self
    }

    /// Direct access to the fault schedule, for tests that must arm an injection after setup.
    ///
    /// Setup operations are real seam calls and would otherwise consume a declared fault intended
    /// for the operation under test.
    #[must_use]
    pub fn faults_handle(&self) -> &RefCell<FaultSchedule> {
        &self.faults
    }

    /// Take one injection for `point`, if the schedule declares one now.
    fn inject(&self, point: FaultPoint) -> Option<Injection> {
        self.faults.borrow_mut().take(point)
    }

    /// Whether every declared injection fired.
    ///
    /// # Panics
    ///
    /// Panics if the schedule still holds an injection: a scenario that declared a fault and never
    /// reached it has tested less than it claims.
    pub fn assert_all_faults_fired(&self) {
        let unfired = self.faults.borrow().unfired();
        assert!(
            unfired.is_empty(),
            "declared injections never fired: {unfired:?} - the scenario did not reach them"
        );
    }

    /// The path a directory handle denotes.
    #[must_use]
    pub fn dir_path(&self, dir: DirHandle) -> Option<RelPath> {
        self.inner.borrow().dirs.get(&dir.0).cloned()
    }
}

/// Translate a declared injection into the typed refusal it stands for.
///
/// `ShortWrite` is handled by the caller because it is not an error: it is a partial success.
fn injection_error(injection: Injection) -> Option<FsError> {
    match injection {
        Injection::ShortWrite(_) => None,
        Injection::FailNoEffect => Some(FsError::no_effect(FsErrorKind::Io(0))),
        Injection::FailUnknown => Some(FsError::new(FsErrorKind::Io(0), Effect::Unknown)),
        // Same error class as `FailUnknown`; the difference is WHERE it is raised, which the caller
        // decides so the model can apply the change first.
        Injection::FailUnknownAfterEffect => {
            Some(FsError::new(FsErrorKind::Io(0), Effect::Unknown))
        }
        Injection::Cancel => Some(FsError::from(ControlStop::Cancelled)),
        Injection::BudgetBytes {
            requested,
            remaining,
        } => Some(FsError::from(BudgetError::new(
            sage_db_types::ledger::BudgetUnit::Bytes,
            requested,
            remaining,
        ))),
        Injection::BudgetEntries {
            requested,
            remaining,
        } => Some(FsError::from(BudgetError::new(
            sage_db_types::ledger::BudgetUnit::Entries,
            requested,
            remaining,
        ))),
    }
}

impl Inner {
    fn register(&mut self, path: RelPath) -> DirHandle {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.dirs.insert(handle, path);
        DirHandle(handle)
    }

    fn path_of(&self, dir: DirHandle) -> Result<RelPath, FsError> {
        self.dirs
            .get(&dir.0)
            .cloned()
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))
    }

    /// Resolve an engine name to the model key inside `dir`.
    ///
    /// The trait gives typed names precisely so nobody parses strings; this is the only place a
    /// name becomes a model key, and it uses the enum's own renderer.
    fn key(dir: &RelPath, name: EntryName) -> (RelPath, String) {
        // `dir` is already the family directory when the caller opened one with `open_family`,
        // or the root when the name is a root file. The family must NOT be appended again: doing
        // so produced `staging/staging/<name>` and every create failed with NotFound.
        match name.family() {
            // A root file lives beside the family directories, i.e. in the root itself.
            None => (dir.clone(), name.to_file_name()),
            // A family file lives in the family directory the caller passed.
            Some(_) => (dir.clone(), name.to_file_name()),
        }
    }
}

impl FileSystem for ModelFileSystem {
    type Dir = DirHandle;
    type WriteFile = WriteHandle;
    type ReadFile = ReadHandle;
    type Lock = LockHandle;

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supported: self.caps.supported.clone(),
            durable_sync: self.caps.durable_sync,
        }
    }

    fn open_root(&self, abs: &Path) -> Result<Self::Dir, FsError> {
        let mut inner = self.inner.borrow_mut();
        let handle = inner
            .roots
            .iter()
            .find(|(_, p)| p.as_path() == abs)
            .map(|(h, _)| *h)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))?;
        // Opening never creates: only a root this backend already knows can be opened.
        inner.dirs.insert(handle, RelPath::root());
        Ok(DirHandle(handle))
    }

    fn create_root(&self, abs: &Path) -> Result<Self::Dir, FsError> {
        let mut inner = self.inner.borrow_mut();
        if inner.roots.values().any(|p| p.as_path() == abs) {
            return Err(FsError::no_effect(FsErrorKind::AlreadyExists));
        }
        let handle = inner.next_handle;
        inner.next_handle += 1;
        inner.roots.insert(handle, abs.to_path_buf());
        inner.dirs.insert(handle, RelPath::root());
        Ok(DirHandle(handle))
    }

    fn sync_parent_of(&self, abs: &Path) -> Result<(), FsError> {
        // Before-effect: a declared refusal must not be able to make the parent binding durable.
        if let Some(injection) = self.inject(FaultPoint::SyncParentOf)
            && let Some(error) = injection_error(injection)
        {
            return Err(error);
        }
        let inner = self.inner.borrow();
        // Syncing the parent of a root makes the root's own binding durable. In the model the
        // root is the top-level path, so this is the root directory's binding itself.
        if inner.roots.values().any(|p| p.as_path() == abs) {
            drop(inner);
            let mut inner = self.inner.borrow_mut();
            // A root's parent is outside the modelled tree; the observable effect is that the
            // root path exists durably, which the model records by keeping the root table.
            let _ = &mut inner;
            return Ok(());
        }
        Err(FsError::no_effect(FsErrorKind::NotFound))
    }

    fn open_family(
        &self,
        root: &Self::Dir,
        family: Family,
        create: CreateDir,
    ) -> Result<Self::Dir, FsError> {
        let mut inner = self.inner.borrow_mut();
        let root_path = inner.path_of(*root)?;
        let path = RelPath::join(&root_path, family.as_str());
        if create == CreateDir::IfMissing {
            if !inner.model.has_dir(&path) {
                inner.model.create_dir(&root_path, family.as_str());
            }
        } else if !inner.model.has_dir(&path) {
            return Err(FsError::no_effect(FsErrorKind::NotFound));
        }
        Ok(inner.register(path))
    }

    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), FsError> {
        // Same pairing as `sync_file`: an after-effect failure makes the entry really durable and
        // still reports `Unknown`. The two branches are separated here rather than left to the
        // caller's imagination, because "the directory entries are durable but unacknowledged" is
        // precisely the state that breaks a store which treats an error as proof of no change.
        let pending = self.faults.borrow().peek(FaultPoint::SyncDir);
        let after = matches!(pending, Some(Injection::FailUnknownAfterEffect));
        if let Some(injection) = pending {
            let _ = self.inject(FaultPoint::SyncDir);
            if after {
                let mut inner = self.inner.borrow_mut();
                let path = inner.path_of(*dir)?;
                inner.model.sync_dir(&path);
                return Err(FsError::new(FsErrorKind::Io(0), Effect::Unknown));
            }
            if let Some(error) = injection_error(injection) {
                return Err(error);
            }
        }
        let mut inner = self.inner.borrow_mut();
        let path = inner.path_of(*dir)?;
        inner.model.sync_dir(&path);
        Ok(())
    }

    fn device_id(&self, _dir: &Self::Dir) -> Result<u64, FsError> {
        // One modelled device; a cross-device case is a second backend instance, not a fake id.
        Ok(1)
    }

    fn try_lock_exclusive(&self, root: &Self::Dir) -> Result<Self::Lock, FsError> {
        let mut inner = self.inner.borrow_mut();
        // The root's own path is the lock identity. A contended lock refuses with NO effect: nothing about the
        // modelled filesystem changes, so a caller cannot mistake contention for a partial acquisition.
        let abs = inner
            .roots
            .get(&root.0)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))?
            .clone();
        let registry = std::rc::Rc::clone(&inner.locks);
        let mut held = registry.borrow_mut();
        if held.contains(&abs) {
            return Err(FsError::no_effect(FsErrorKind::WouldBlock));
        }
        held.insert(abs.clone());
        drop(held);
        let handle = inner.next_lock;
        inner.next_lock += 1;
        Ok(LockHandle {
            root: abs,
            registry,
            id: handle,
        })
    }

    fn create_new(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::WriteFile, FsError> {
        // Declared fault BEFORE the effect: a refusal here must leave the namespace untouched. `ShortWrite` has no
        // meaning for a create (there are no bytes yet) and falls through to the real operation.
        if let Some(injection) = self.inject(FaultPoint::CreateNew)
            && let Some(error) = injection_error(injection)
        {
            return Err(error);
        }
        let mut inner = self.inner.borrow_mut();
        let base = inner.path_of(*dir)?;
        let (dir_path, key) = Inner::key(&base, name);
        if inner.model.has_entry(&dir_path, &key) {
            return Err(FsError::no_effect(FsErrorKind::AlreadyExists));
        }
        if !inner.model.has_dir(&dir_path) {
            return Err(FsError::no_effect(FsErrorKind::NotFound));
        }
        let ino = inner.model.create_new(&dir_path, &key);
        Ok(WriteHandle(ino))
    }

    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError> {
        if let Some(injection) = self.inject(FaultPoint::Write) {
            match injection {
                Injection::ShortWrite(k) => {
                    // A short write is a partial SUCCESS: the accepted prefix is really visible and
                    // nothing is durable yet. Returning here is the whole point - falling through
                    // to the normal path would accept the full buffer and silently discard the
                    // declared fault.
                    let accepted = k.min(buf.len());
                    let mut inner = self.inner.borrow_mut();
                    inner.model.write(file.0, &buf[..accepted]);
                    return Ok(WriteOutcome { written: accepted });
                }
                other => {
                    if let Some(error) = injection_error(other) {
                        return Err(error);
                    }
                }
            }
        }
        let mut inner = self.inner.borrow_mut();
        inner.model.write(file.0, buf);
        Ok(WriteOutcome { written: buf.len() })
    }

    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError> {
        // Two distinct unknown shapes, and they must stay distinct:
        //   FailUnknown              -> nothing is made durable, caller must resolve.
        //   FailUnknownAfterEffect   -> the content IS durable in the model, and the caller still
        //                               receives the same `Effect::Unknown`. Filesystem effect is
        //                               not an acknowledgement, and this branch exists so a store
        //                               that infers "error means nothing happened" is caught.
        let pending = self.faults.borrow().peek(FaultPoint::SyncFile);
        let after = matches!(pending, Some(Injection::FailUnknownAfterEffect));
        if let Some(injection) = pending {
            let _ = self.inject(FaultPoint::SyncFile);
            if after {
                let mut inner = self.inner.borrow_mut();
                inner.model.sync_file(file.0);
                return Err(FsError::new(FsErrorKind::Io(0), Effect::Unknown));
            }
            if let Some(error) = injection_error(injection) {
                return Err(error);
            }
        }
        let mut inner = self.inner.borrow_mut();
        inner.model.sync_file(file.0);
        Ok(())
    }

    fn sync_entry(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        // Before-effect: after a NoEffect refusal the inode's durable bytes must be exactly what they were.
        if let Some(injection) = self.inject(FaultPoint::SyncEntry)
            && let Some(error) = injection_error(injection)
        {
            return Err(error);
        }
        let mut inner = self.inner.borrow_mut();
        let base = inner.path_of(*dir)?;
        let (dir_path, key) = Inner::key(&base, name);
        let Some(ino) = inner.model.inode_of(&dir_path, &key) else {
            return Err(FsError::no_effect(FsErrorKind::NotFound));
        };
        inner.model.sync_file(ino);
        Ok(())
    }

    fn publish_no_overwrite(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        dst_dir: &Self::Dir,
        dst: EntryName,
        expected: &HashReport,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Published, FsError> {
        if let Some(injection) = self.inject(FaultPoint::Publish)
            && let Some(error) = injection_error(injection)
        {
            return Err(error);
        }
        let mut inner = self.inner.borrow_mut();
        let src_base = inner.path_of(*staged_dir)?;
        let dst_base = inner.path_of(*dst_dir)?;
        let (src_dir, src_key) = Inner::key(&src_base, staged);
        let (dst_dir_path, dst_key) = Inner::key(&dst_base, dst);

        let src_ino = inner
            .model
            .inode_of(&src_dir, &src_key)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))?;

        // Publication re-reads and re-hashes the staged bytes, so that work must enter the very
        // same request budget. The caller's `expected` is a claim about content; verifying it is
        // an operation, not a free comparison. Charging happens before the read for the declared
        // length, exactly like `hash_bounded`.
        let staged_len = inner
            .model
            .visible_bytes(src_ino)
            .map_or(0, |b| b.len() as u64);
        ledger.try_charge(staged_len).map_err(FsError::from)?;
        ledger.checkpoint().map_err(FsError::from)?;
        if expected.len != staged_len {
            return Err(FsError::no_effect(FsErrorKind::HashMismatch));
        }
        let staged_sha = inner
            .model
            .visible_bytes(src_ino)
            .map(sage_db_format::raw_sha256)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))?;
        if staged_sha != expected.sha256 {
            return Err(FsError::no_effect(FsErrorKind::HashMismatch));
        }

        let how = match inner.model.inode_of(&dst_dir_path, &dst_key) {
            Some(existing) => {
                if inner.model.visible_bytes(existing) != inner.model.visible_bytes(src_ino) {
                    return Err(FsError::no_effect(FsErrorKind::HashMismatch));
                }
                PublishMode::AlreadyPresent
            }
            None => {
                inner.model.link(&dst_dir_path, &dst_key, src_ino);
                PublishMode::Created
            }
        };

        // Contract: the staged name is consumed on success, including the identical-bytes case.
        inner.model.remove(&src_dir, &src_key);
        Ok(Published {
            how,
            // Identity is not durability: even `AlreadyPresent` still owes a sync.
            needs_sync: true,
        })
    }

    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        root: &Self::Dir,
    ) -> Result<(), FsError> {
        // Exactly one lookup, for this operation's own point. An earlier revision also consulted
        // `FaultPoint::Publish` here, which meant a planned publish failure fired at the root
        // replace instead: the test then saw an error at an operation that was never told to fail.
        //
        // `peek` first, because the two unknown variants differ in WHEN the error is raised: the
        // before-effect one must not mutate anything, and the after-effect one must mutate first.
        // Consuming the injection up front would make the after-effect branch unreachable, which is
        // exactly the bug this ordering fixes.
        let pending = self.faults.borrow().peek(FaultPoint::ReplaceCurrent);
        let after_effect = matches!(pending, Some(Injection::FailUnknownAfterEffect));
        if let Some(injection) = pending {
            if let Some(error) = injection_error(injection) {
                if after_effect {
                    // Defer: apply the change below, then raise the same Unknown error.
                } else {
                    let _ = self.inject(FaultPoint::ReplaceCurrent);
                    return Err(error);
                }
            } else {
                // Not an error at this point (a short write here would be a schedule bug, which the
                // constructor rejects), so consume it rather than leave it armed.
                let _ = self.inject(FaultPoint::ReplaceCurrent);
            }
        }
        let mut inner = self.inner.borrow_mut();
        let src_base = inner.path_of(*staged_dir)?;
        let root_base = inner.path_of(*root)?;
        let (src_dir, src_key) = Inner::key(&src_base, staged);
        // The seam takes `staged_dir` because the temp file need not live in the root: the OS
        // backend opens the source under that directory and renames into the root. An earlier
        // revision of this model ignored `staged_dir` and looked the staged name up in the root
        // table, which made a cross-directory replace panic instead of returning an error. That
        // was a model defect: the seam is general, so `staged_dir` must actually be honoured.
        let Some(src_ino) = inner.model.inode_of(&src_dir, &src_key) else {
            // A missing staged entry is a typed refusal, never a panic: the caller is allowed to
            // be wrong, and I/O errors have to be expressible.
            return Err(FsError::no_effect(FsErrorKind::NotFound));
        };
        // The destination is `CURRENT` by construction, as the seam's contract states.
        let dst_key = RootName::Current.as_str();
        if src_dir == root_base {
            // Same-directory replace: the model primitive handles it directly.
            inner.model.replace_current(&src_base, &src_key, dst_key);
        } else {
            // Cross-directory: ONE primitive. rename(2) resolves the destination name onto the
            // source entry and consumes the source, replacing any existing destination. `link` +
            // `remove` was wrong because `link` asserts the destination is absent (so a stored
            // CURRENT panicked), not because of any crash-durability property.
            let _ = src_ino;
            inner
                .model
                .rename_across(&src_dir, &src_key, &root_base, dst_key);
        }
        drop(inner);
        if after_effect {
            // Consume the injection now that its effect has been applied, then report the failure.
            let _ = self.inject(FaultPoint::ReplaceCurrent);
            return Err(FsError::new(FsErrorKind::Io(0), Effect::Unknown));
        }
        Ok(())
    }

    fn size(&self, dir: &Self::Dir, name: EntryName) -> Result<u64, FsError> {
        let inner = self.inner.borrow();
        let base = inner.path_of(*dir)?;
        let (dir_path, key) = Inner::key(&base, name);
        let ino = inner
            .model
            .inode_of(&dir_path, &key)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))?;
        Ok(inner.model.visible_bytes(ino).map_or(0, |b| b.len() as u64))
    }

    fn read_exact_into(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        // Before-effect: a declared refusal must copy ZERO bytes into the caller's buffer.
        if let Some(injection) = self.inject(FaultPoint::ReadExact)
            && let Some(error) = injection_error(injection)
        {
            return Err(error);
        }
        let inner = self.inner.borrow();
        let base = inner.path_of(*dir)?;
        let (dir_path, key) = Inner::key(&base, name);
        let ino = inner
            .model
            .inode_of(&dir_path, &key)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))?;
        let bytes = inner
            .model
            .visible_bytes(ino)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))?;
        if bytes.len() > buf.len() {
            // A file larger than the buffer is refused whole; no partial return.
            return Err(FsError::no_effect(FsErrorKind::TooLarge));
        }
        buf[..bytes.len()].copy_from_slice(bytes);
        Ok(bytes.len())
    }

    fn open_read(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::ReadFile, FsError> {
        // Before-effect: a refusal yields no read handle at all.
        if let Some(injection) = self.inject(FaultPoint::OpenRead)
            && let Some(error) = injection_error(injection)
        {
            return Err(error);
        }
        let inner = self.inner.borrow();
        let base = inner.path_of(*dir)?;
        let (dir_path, key) = Inner::key(&base, name);
        inner
            .model
            .inode_of(&dir_path, &key)
            .map(ReadHandle::new)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))
    }

    fn read(&self, file: &mut Self::ReadFile, buf: &mut [u8]) -> Result<usize, FsError> {
        let inner = self.inner.borrow();
        let bytes = inner
            .model
            .visible_bytes(file.inode)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))?;
        // EOF is 0, not a repeated prefix: a read-until-zero loop depends on this.
        if file.offset >= bytes.len() {
            file.offset = bytes.len();
            return Ok(0);
        }
        let n = (bytes.len() - file.offset).min(buf.len());
        buf[..n].copy_from_slice(&bytes[file.offset..file.offset + n]);
        file.offset += n;
        Ok(n)
    }

    fn hash_bounded(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        max_len: u64,
        ledger: &mut dyn SharedLedger,
    ) -> Result<HashReport, FsError> {
        // Before-effect. A hash fault is placed before the admission and the copy, so a refusal performs no charge and
        // produces no digest — the caller cannot mistake a seam fault for a real budget or a real content verdict.
        if let Some(injection) = self.inject(FaultPoint::Hash)
            && let Some(error) = injection_error(injection)
        {
            return Err(error);
        }
        // This function holds the model's borrow for the whole operation and never retains a copy,
        // so there is no window in which bytes exist outside the caller's budget.
        //
        // Order, and why each step is where it is:
        //   1. bound check on the length (no allocation, no charge);
        //   2. `try_charge(len)` — the admission the caller's budget demands, before any copy;
        //   3. `checkpoint()` — before the copy, so a cancelled attempt cannot make us copy at all,
        //      and so an EMPTY file is still a cancellable operation;
        //   4. hash the borrowed slice with the real SHA-256, checking control per chunk.
        //
        // An earlier revision cloned the whole file *before* the admission (which reintroduced
        // exactly the allocation-before-budget defect this harness exists to detect), and the
        // revision before that charged and hashed without ever consulting control. Both are gone:
        // the slice is borrowed, not cloned, so neither failure mode can recur here.
        let inner = self.inner.borrow();
        let base = inner.path_of(*dir)?;
        let (dir_path, key) = Inner::key(&base, name);
        let ino = inner
            .model
            .inode_of(&dir_path, &key)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))?;
        let bytes = inner
            .model
            .visible_bytes(ino)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotFound))?;
        let len = bytes.len() as u64;
        if len > max_len {
            return Err(FsError::no_effect(FsErrorKind::TooLarge));
        }

        // Control before any work: an already-cancelled attempt must not even start feeding, and
        // an empty file must still be cancellable. Charging is left to the stream so the same bytes
        // are never admitted twice.
        ledger.checkpoint().map_err(FsError::from)?;

        // Feed the borrowed bytes through format's streaming hasher in chunks. That wrapper charges
        // each chunk and checks control per chunk (and once more on `finish`), which is real
        // cancellation *during* hashing. The previous version looped checkpoint calls up front and
        // then hashed the whole slice in one call, which observed cancellation only before the
        // work - a cancellation point that never fires during the scan is not a cancellation point.
        //
        // The stream also charges per chunk, so the admission above is prepayment for the exact
        // length and the stream's own charges must not be added on top. `StreamingSha256` charges
        // what it feeds, so this function must NOT have charged the same bytes twice: the entry
        // `try_charge` here is removed in favour of the stream's per-chunk accounting.
        let sha256 = {
            let mut stream = sage_db_format::StreamingSha256::new(ledger);
            const CHUNK: usize = 4096;
            for chunk in bytes.chunks(CHUNK) {
                stream.update(chunk).map_err(|e| match e {
                    sage_db_format::BoundedHashError::Budget(b) => FsError::from(b),
                    sage_db_format::BoundedHashError::Stopped(s) => FsError::from(s),
                })?;
            }
            stream.finish().map_err(|e| match e {
                sage_db_format::BoundedHashError::Budget(b) => FsError::from(b),
                sage_db_format::BoundedHashError::Stopped(s) => FsError::from(s),
            })?
        };
        Ok(HashReport { len, sha256 })
    }

    fn for_each_entry(
        &self,
        dir: &Self::Dir,
        ledger: &mut dyn SharedLedger,
        visit: &mut dyn FnMut(&str) -> Each,
    ) -> Result<(), FsError> {
        // No up-front collection: the earlier version built a `Vec<String>` of every name before
        // charging a single entry, which both bypassed the request budget and made the cost of a
        // refusal proportional to the directory. Here each name is charged, then visited, and the
        // walk stops as soon as the caller or the ledger says stop.
        if let Some(injection) = self.inject(FaultPoint::Enumerate)
            && let Some(error) = injection_error(injection)
        {
            return Err(error);
        }
        let path = {
            let inner = self.inner.borrow();
            inner.path_of(*dir)?
        };
        // Walk by index over the model's ordered name table. `entry_len` is the only up-front
        // number read, and it is a count, not a copy of the names.
        let len = {
            let inner = self.inner.borrow();
            inner.model.entry_len(&path)
        };
        for index in 0..len {
            let name = {
                let inner = self.inner.borrow();
                inner.model.entry_name_at(&path, index)
            };
            let Some(name) = name else { continue };
            // Charge, then check control, then visit: a refusal must not do the work.
            ledger.try_charge_entries(1)?;
            ledger.checkpoint()?;
            if visit(&name) == Each::Stop {
                return Ok(());
            }
        }
        Ok(())
    }

    fn remove(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError> {
        // Before-effect: after a refusal the name is still bound. Removal is the one mutating operation whose
        // failure must be provably namespace-preserving.
        if let Some(injection) = self.inject(FaultPoint::Remove)
            && let Some(error) = injection_error(injection)
        {
            return Err(error);
        }
        let mut inner = self.inner.borrow_mut();
        let base = inner.path_of(*dir)?;
        let (dir_path, key) = Inner::key(&base, name);
        inner.model.remove(&dir_path, &key);
        Ok(())
    }
}
