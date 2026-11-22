//! The narrow filesystem seam between the store and any backend.
//!
//! **Ownership handed off to the store owner (t62).** This file was created by t61 as
//! the P1-2 scaffold so both peers could bind real types; from here t62 owns it and
//! aligns it to the accepted S2 contract. The ledger boundary is the canonical
//! [`sage_db_types::ledger::SharedLedger`] — the earlier `Charge` trait was withdrawn
//! in S2 and must not be revived, adapted or shadowed.
//!
//! Deliberately small and total: every operation the commit protocol needs exists
//! here, and nothing else does. A backend implements this trait; the store reasons
//! about effects, never about syscalls.
//!
//! Three rules shape every method:
//!
//! * **Effect is not authority.** An `FsError` carries [`Effect`], which says whether
//!   the operation certainly did nothing, certainly took effect, or cannot say.
//!   Returning "failed" is not the same as "did not happen".
//! * **Admit before acting.** Read and hash paths charge the shared ledger before they
//!   acquire anything, so a refusal costs no I/O and no allocation.
//! * **No allocation on the caller's behalf.** Reads fill caller-provided buffers and
//!   directory walks visit names; nothing returns an unbounded collection.

use std::path::Path;

use sage_db_types::ledger::{BudgetError, SharedLedger};
use sage_db_types::query::ControlStop;

use crate::names::EntryName;

/// How much is known about whether an operation took effect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// The operation certainly did not take effect.
    NoEffect,
    /// The operation certainly took effect.
    Entailed,
    /// The platform cannot say whether the operation took effect.
    ///
    /// This is **not** "assume it worked". For the commit decision it means *unresolved*: the
    /// caller must re-read `CURRENT` and decide from the observed root, never infer a durable
    /// success from a failed-but-possibly-applied operation. Treating `Unknown` as
    /// [`Effect::Entailed`] would let a store claim a commit that may not exist, which is the
    /// defect the resolve-instead-of-assume rule exists to prevent.
    Unknown,
}

/// A typed filesystem failure.
///
/// Two kinds of non-I/O refusal travel here so a caller can distinguish them by value rather
/// than by string: a **budget** refusal carries the ledger's own [`BudgetError`] (unit, requested,
/// remaining), and a **control** stop carries the caller's [`ControlStop`] (cancelled vs
/// deadline). Both are "the operation did not run", never data verdicts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FsError {
    kind: FsErrorKind,
    effect: Effect,
    budget: Option<BudgetError>,
    control: Option<ControlStop>,
}

impl FsError {
    /// Construct a plain I/O failure (no budget or control payload).
    #[must_use]
    pub const fn new(kind: FsErrorKind, effect: Effect) -> Self {
        Self {
            kind,
            effect,
            budget: None,
            control: None,
        }
    }

    /// A failure that certainly had no effect.
    #[must_use]
    pub const fn no_effect(kind: FsErrorKind) -> Self {
        Self::new(kind, Effect::NoEffect)
    }

    /// What went wrong.
    #[must_use]
    pub const fn kind(&self) -> FsErrorKind {
        self.kind
    }

    /// What is known about the effect.
    #[must_use]
    pub const fn effect(&self) -> Effect {
        self.effect
    }

    /// Whether the caller must resolve rather than assume failure.
    #[must_use]
    pub const fn is_ambiguous(&self) -> bool {
        matches!(self.effect, Effect::Unknown)
    }
}

impl From<BudgetError> for FsError {
    fn from(error: BudgetError) -> Self {
        Self {
            kind: FsErrorKind::BudgetExceeded,
            effect: Effect::NoEffect,
            budget: Some(error),
            control: None,
        }
    }
}

/// Convert a format-side bounded-hash refusal, **preserving its payload**.
///
/// The format crate reports a hash refusal as `Budget` or `Stopped`; flattening either into a bare
/// kind would lose whether bytes or entries ran out, and whether the caller cancelled or timed out —
/// the distinction the seam promises to keep.
impl From<sage_db_format::BoundedHashError> for FsError {
    fn from(error: sage_db_format::BoundedHashError) -> Self {
        match error {
            sage_db_format::BoundedHashError::Budget(b) => Self::from(b),
            sage_db_format::BoundedHashError::Stopped(s) => Self::from(s),
        }
    }
}

impl From<ControlStop> for FsError {
    fn from(stop: ControlStop) -> Self {
        Self {
            kind: FsErrorKind::Interrupted,
            effect: Effect::NoEffect,
            budget: None,
            control: Some(stop),
        }
    }
}

impl FsError {
    /// The ledger refusal behind this failure, if it was a budget refusal.
    ///
    /// Preserved rather than flattened: the unit tells a caller whether bytes or directory
    /// entries ran out, and the amounts tell it what to ask for. Dropping them would leave
    /// `BudgetExceeded` as the only trace, which cannot distinguish the two units.
    #[must_use]
    pub const fn budget(&self) -> Option<BudgetError> {
        self.budget
    }

    /// The caller's stop signal behind this failure, if it was a cancellation.
    ///
    /// Kept distinct from a budget refusal: "you cancelled" and "you ran out of allowance" have
    /// different remedies, and `ControlStop` distinguishes cancelled from deadline.
    #[must_use]
    pub const fn control(&self) -> Option<ControlStop> {
        self.control
    }
}

/// Failure classes a caller must be able to distinguish.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsErrorKind {
    /// No such entry.
    NotFound,
    /// The entry exists but is not a regular file.
    NotRegular,
    /// The entry exists but is not a directory.
    NotADirectory,
    /// The entry is a directory.
    IsADirectory,
    /// The entry already exists.
    AlreadyExists,
    /// A bounded operation would have exceeded its limit.
    TooLarge,
    /// The caller may not perform this operation.
    PermissionDenied,
    /// The device has no space left.
    NoSpace,
    /// A quota would be exceeded.
    QuotaExceeded,
    /// Two directories of one KB layout report different devices.
    CrossDevice,
    /// Interrupted before any byte was consumed or produced.
    Interrupted,
    /// An exclusive lock is held elsewhere.
    WouldBlock,
    /// The feature is unavailable on this filesystem or platform.
    Unsupported,
    /// Content did not match the declared digest or length.
    HashMismatch,
    /// This request's shared budget refused the operation before any I/O.
    BudgetExceeded,
    /// Any other I/O failure; the payload is a platform errno.
    Io(i32),
    /// The entry is a symbolic link where the layout requires a real one.
    ///
    /// A symlinked family directory or file would redirect reads and writes outside the KB, so the
    /// seam refuses it rather than following it. Distinct from `NotRegular` because the operator
    /// remedy differs: a symlink is a layout violation, not a wrong file type.
    Symlink,
}

/// Whether a directory should be created when opened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CreateDir {
    /// Never create; a missing directory is [`FsErrorKind::NotFound`].
    Never,
    /// Create when missing. Legal only on the init path.
    IfMissing,
}

/// One of the fixed top-level families.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    /// Immutable manifests.
    Manifests,
    /// Immutable transaction files.
    Transactions,
    /// Content-addressed blobs.
    Blobs,
    /// Reserved; refused in this phase.
    Checkpoints,
    /// Never authoritative; only the writing attempt may read it.
    Staging,
    /// Derived and disposable.
    Cache,
}

impl Family {
    /// The on-disk directory name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manifests => "manifests",
            Self::Transactions => "transactions",
            Self::Blobs => "blobs",
            Self::Checkpoints => "checkpoints",
            Self::Staging => "staging",
            Self::Cache => "cache",
        }
    }
}

/// Result of one write call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WriteOutcome {
    /// Bytes accepted; may be fewer than requested.
    pub written: usize,
}

/// Identity of bytes that were read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HashReport {
    /// Bytes actually read.
    pub len: u64,
    /// SHA-256 of exactly those bytes.
    pub sha256: [u8; 32],
}

/// Whether a directory walk should continue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Each {
    /// Keep scanning.
    Continue,
    /// Stop early; this is success, not truncation.
    Stop,
}

/// What this backend can actually do here.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Capabilities {
    /// Affirmatively supported capabilities. Absence means unsupported, never "probably fine".
    pub supported: Vec<PlatformCapability>,
    /// Whether this backend's sync primitives provide the declared durability semantics
    /// on this platform. A strict product handle requires `true`.
    pub durable_sync: bool,
}

impl Capabilities {
    /// The first required capability this backend does not affirmatively support.
    #[must_use]
    pub fn missing_required(&self) -> Option<PlatformCapability> {
        PlatformCapability::REQUIRED_FOR_PRODUCT
            .into_iter()
            .find(|required| !self.supported.contains(required))
    }

    /// Whether a **product** handle may be opened on this backend.
    ///
    /// This is the single gate. `durable_sync` is a separate axis from the primitive list: a
    /// filesystem can expose every syscall and still not be documented to honour the declared
    /// durability semantics, so both must hold. Nothing else may grant eligibility, and no
    /// per-operation downgrade exists.
    #[must_use]
    pub fn product_eligible(&self) -> bool {
        self.durable_sync && self.missing_required().is_none()
    }
}

/// A capability a strict product handle requires.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlatformCapability {
    /// An exclusive lock can be taken for the life of the handle.
    ExclusiveLock,
    /// A file's data and metadata can be flushed.
    FileSync,
    /// A directory's entries can be flushed.
    DirSync,
    /// A same-device rename of a root pointer is atomic.
    AtomicReplace,
    /// An immutable file can be published without overwriting different bytes.
    NoOverwritePublish,
    /// `st_dev` is meaningful, so the same-filesystem rule can be enforced.
    SameDevice,
}

impl PlatformCapability {
    /// The capabilities a **strict product** handle requires, in one place.
    ///
    /// Six entries, matching the accepted S2 set. A missing entry is a typed refusal at
    /// open/init; there is no per-operation subset and no "probably fine" default.
    pub const REQUIRED_FOR_PRODUCT: [Self; 6] = [
        Self::ExclusiveLock,
        Self::FileSync,
        Self::DirSync,
        Self::AtomicReplace,
        Self::NoOverwritePublish,
        Self::SameDevice,
    ];
}

/// The filesystem seam.
///
/// Implementors provide real I/O or a fault model; the store cannot tell the
/// difference, which is the point.
pub trait FileSystem {
    /// Directory handle.
    type Dir;
    /// Writable file handle.
    type WriteFile;
    /// Readable file handle.
    type ReadFile;
    /// Exclusive lock handle.
    type Lock;

    /// What this backend supports. Called once at open or init.
    fn capabilities(&self) -> Capabilities;

    /// Open an existing root directory. Must never create anything.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotADirectory`, `PermissionDenied`, `Io`.
    fn open_root(&self, abs: &Path) -> Result<Self::Dir, FsError>;

    /// Create a root directory. Init only; the caller then syncs its parent.
    ///
    /// # Errors
    ///
    /// `AlreadyExists`, `NoSpace`, `PermissionDenied`, `Io`.
    fn create_root(&self, abs: &Path) -> Result<Self::Dir, FsError>;

    /// Flush the directory that contains `abs`, so a new root entry is durable.
    ///
    /// # Errors
    ///
    /// Any failure is `Effect::Unknown`: the entry may or may not be visible.
    fn sync_parent_of(&self, abs: &Path) -> Result<(), FsError>;

    /// Open a declared family under the root.
    ///
    /// `create` is explicit because "open repair" and "init creation" are different
    /// operations; a normal open must never create.
    ///
    /// # Errors
    ///
    /// `NotFound` when not creating, `NoSpace`, `PermissionDenied`, `Io`.
    fn open_family(
        &self,
        root: &Self::Dir,
        family: Family,
        create: CreateDir,
    ) -> Result<Self::Dir, FsError>;

    /// Flush a directory's entries. Says nothing about the durability of the file
    /// contents those entries point at.
    ///
    /// # Errors
    ///
    /// Any failure is `Effect::Unknown`.
    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), FsError>;

    /// Device identity, used to enforce the same-device rule.
    ///
    /// # Errors
    ///
    /// `Io`.
    fn device_id(&self, dir: &Self::Dir) -> Result<u64, FsError>;

    /// Take the exclusive KB lock without waiting.
    ///
    /// # Errors
    ///
    /// `WouldBlock`, `Unsupported`, `Io`.
    fn try_lock_exclusive(&self, root: &Self::Dir) -> Result<Self::Lock, FsError>;

    /// Create a file that must not already exist.
    ///
    /// # Errors
    ///
    /// `AlreadyExists`, `NoSpace`, `PermissionDenied`, `Io`.
    fn create_new(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::WriteFile, FsError>;

    /// Write at the current position.
    ///
    /// # Contract: short writes and mid-stream failure
    ///
    /// Both outcomes are legal and they mean different things:
    ///
    /// * `Ok(WriteOutcome { written })` with `written < buf.len()` is a **short write**: those
    ///   `written` bytes were accepted (visible) and the caller must retry the remainder. This is
    ///   not an error and carries no durability implication; nothing is durable until a sync.
    /// * `Err(_)` after some bytes were accepted is reported as `Effect::Unknown`, **not**
    ///   `NoEffect`. A fault model may therefore express "k bytes reached the page cache, then
    ///   ENOSPC": those k bytes are *visible but not durable*, and the caller must abandon the
    ///   artifact rather than treat it as untouched.
    ///
    /// Rule of thumb: a `write` error never means "no bytes were written" when a partial prefix
    /// could have been accepted. Callers that need the strict version should treat every artifact
    /// as suspect after a write error, which is what the staging/publication protocol does.
    ///
    /// # Errors
    ///
    /// `NoSpace`, `QuotaExceeded`, `Interrupted`, `Io` — with `Effect::Unknown` when a partial
    /// prefix may have been accepted, `Effect::NoEffect` only when nothing was written.
    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError>;

    /// Flush a writable file's data and metadata.
    ///
    /// # Contract: what `Ok` means
    ///
    /// `Ok(())` means **exactly the state visible in this file at the moment of the call is
    /// durable**, under the declared platform failure model. It is not a partial guarantee and
    /// there is no "mostly flushed" outcome: every byte written to this file before the call is
    /// covered, and nothing after it is implied. A model therefore snapshots the file's visible
    /// contents at each successful `sync_file` and treats that snapshot as the durable set.
    ///
    /// # Errors
    ///
    /// Any failure is `Effect::Unknown`: a flush that returned an error may nonetheless have
    /// completed, so the caller resolves rather than assumes.
    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError>;

    /// Flush an existing, already-named file's content.
    ///
    /// Needed because reuse and resolution must make bytes durable that this attempt
    /// did not write.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotRegular`, or `Effect::Unknown` on a flush failure.
    fn sync_entry(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError>;

    /// Publish staged bytes under `dst` without replacing different bytes.
    ///
    /// An existing name is not an error when the bytes match; different bytes are
    /// [`FsErrorKind::HashMismatch`].
    ///
    /// # Errors
    ///
    /// `HashMismatch`, `NoSpace`, `CrossDevice`, `Io`.
    /// # Contract: one ledger covers the verification too
    ///
    /// Verifying the staged bytes and any existing destination bytes is real work — it reads
    /// content — so it is **charged to the caller's ledger**, the same instance that bounds the
    /// write side. An earlier revision hashed without any ledger, which meant an unbounded amount of
    /// reading happened outside every budget; a publish is not exempt just because it is a rename.
    ///
    /// Contract: `staged` is **consumed** by a successful publication.
    ///
    /// A successful `Ok` removes the staged name — the bytes now live under `dst`. The only
    /// exception is the identity case (`Published { how: AlreadyPresent }`), where the staged
    /// copy is still removed because the destination already held identical bytes; either way the
    /// caller must not assume `staged` still exists. `needs_sync` reports that durability is still
    /// owed even when the bytes were already present: identity is not durability.
    ///
    /// # Errors
    ///
    /// `HashMismatch`, `NoSpace`, `CrossDevice`, `Io`.
    fn publish_no_overwrite(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        dst_dir: &Self::Dir,
        dst: EntryName,
        expected: &HashReport,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Published, FsError>;

    /// Atomically replace the root pointer. The only overwrite in the seam.
    ///
    /// # Contract: the destination is `CURRENT`, by construction
    ///
    /// There is no destination-name parameter because the destination is **always** the root's
    /// `CURRENT` file: it is `EntryName::Root(RootName::Current)` inside `root`. That is why the
    /// signature cannot be misused to overwrite some other name, and why a model may treat the
    /// only possible target as `CURRENT`. `staged` is the staged file to move (normally
    /// `EntryName::Root(RootName::CurrentTmp)`); it is consumed by the move.
    ///
    /// # Errors
    ///
    /// `CrossDevice` with [`Effect::NoEffect`]; every I/O failure is
    /// [`Effect::Unknown`], which the protocol must resolve rather than assume.
    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: EntryName,
        root: &Self::Dir,
    ) -> Result<(), FsError>;

    /// Size of an existing file.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotRegular`, `Io`.
    fn size(&self, dir: &Self::Dir, name: EntryName) -> Result<u64, FsError>;

    /// Read a whole small file into a caller-provided buffer.
    ///
    /// A file larger than the buffer is [`FsErrorKind::TooLarge`] and must not be
    /// partially returned.
    ///
    /// # Errors
    ///
    /// `NotFound`, `TooLarge`, `NotRegular`, `Io`.
    fn read_exact_into(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        buf: &mut [u8],
    ) -> Result<usize, FsError>;

    /// Open a file for streaming reads.
    ///
    /// # Errors
    ///
    /// `NotFound`, `NotRegular`, `Io`.
    fn open_read(&self, dir: &Self::Dir, name: EntryName) -> Result<Self::ReadFile, FsError>;

    /// Fill `buf`; `Ok(0)` means end of file. The caller owns the buffer.
    ///
    /// # Errors
    ///
    /// `Io`.
    fn read(&self, file: &mut Self::ReadFile, buf: &mut [u8]) -> Result<usize, FsError>;

    /// Stream a file through SHA-256, charging every chunk to the shared ledger.
    ///
    /// # Errors
    ///
    /// `NotFound`, `TooLarge` relative to `max_len`, `BudgetExceeded`, `Io`.
    fn hash_bounded(
        &self,
        dir: &Self::Dir,
        name: EntryName,
        max_len: u64,
        ledger: &mut dyn SharedLedger,
    ) -> Result<HashReport, FsError>;

    /// Visit a directory's names, charging one entry each.
    ///
    /// # Contract: why the visitor gets `&str`
    ///
    /// This is the one place the seam yields an untyped string, deliberately. The walk reports
    /// what is *on disk*, including names the engine did not choose — a hostile entry, a stale
    /// staging file, or an operator's stray file. Fabricating an [`EntryName`] for such an entry
    /// would be a lie, and refusing to report it would hide exactly the condition an orphan or
    /// emptiness check exists to find.
    ///
    /// Callers therefore must not parse the string back into an `EntryName`. Two uses are
    /// permitted, and both treat the name as opaque:
    ///
    /// 1. **emptiness** — "is there any entry other than `LOCK`?", a predicate over presence;
    /// 2. **orphan reporting** — a list for an operator, never an input to a commit decision.
    ///
    /// A test may assert on counts and on the presence of a known name; it must not depend on the
    /// walk producing only engine-generated names. It must never select a commit root: only the
    /// content of `CURRENT` may name a root.
    ///
    /// # Errors
    ///
    /// `BudgetExceeded`, `Io`.
    fn for_each_entry(
        &self,
        dir: &Self::Dir,
        ledger: &mut dyn SharedLedger,
        visit: &mut dyn FnMut(&str) -> Each,
    ) -> Result<(), FsError>;

    /// Remove a visible entry. A missing entry is success.
    ///
    /// # Errors
    ///
    /// `Io`, `IsADirectory`.
    fn remove(&self, dir: &Self::Dir, name: EntryName) -> Result<(), FsError>;
}

/// Outcome of an immutable publication.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Published {
    /// How the destination name came to exist.
    pub how: PublishMode,
    /// Whether the caller must still sync the file and its directory before referencing it.
    pub needs_sync: bool,
}

/// How a destination name came to exist.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PublishMode {
    /// This call created the name.
    Created,
    /// The name already held identical bytes.
    AlreadyPresent,
}
