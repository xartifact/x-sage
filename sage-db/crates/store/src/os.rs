//! The real-OS backend for the [`FileSystem`] seam.
//!
//! ## Containment: every relative operation goes through a pinned descriptor
//!
//! `OsDir` owns a `cap_std::fs::Dir`. Every child operation on a directory — open, create, sync,
//! rename, hard link, unlink, metadata, enumeration — is performed **relative to that descriptor**,
//! never by re-resolving the directory's recorded path. Consequences:
//!
//! * a crafted path cannot escape the directory, because names are engine-generated and resolution
//!   starts at the descriptor;
//! * an external `rename` of the directory cannot redirect a later operation to a replacement
//!   directory (a stored path would follow it);
//! * one shared (private) helper applies the same no-follow and
//!   regular-file guards to *every* read path, so a guard cannot exist on one sibling read and be
//!   missing on another.
//!
//! Absolute paths appear only where the caller supplies them: `open_root` / `create_root` /
//! `sync_parent_of`, which take the KB location itself.
//!
//! ## Primitives
//!
//! `cap-std` for fd-relative resolution; `std` for locking and flushing. No `unsafe` in this crate,
//! so the workspace's `unsafe_code = "forbid"` lint stands unmodified.
//!
//! | Need | Primitive | Note |
//! |---|---|---|
//! | fd-relative resolution | `cap_std::fs::Dir` (`open_with`, `open_dir`, `hard_link`, `rename`, `remove_file`, `symlink_metadata`, `dir_metadata`, `entries`) | no `openat` in stable std; `std::fs::Dir` is nightly |
//! | exclusive lock, non-blocking | `std::fs::File::try_lock` (stable 1.89) | returns `TryLockError::{Error, WouldBlock}` |
//! | file flush | `std::fs::File::sync_all` | on `target_vendor = "apple"` std calls `fcntl(F_FULLFSYNC)`, not `fsync(2)` |
//! | directory flush | the pinned descriptor's `try_clone` + `sync_all` | flushes the directory that was opened, not whatever its path names now |
//! | streaming hash | `sage_db_format::StreamingSha256` | charges and checkpoints per chunk; one SHA-256 in the workspace |
//! | same-device rule | `cap_std::fs::MetadataExt::dev` on `dir_metadata` | compares `st_dev` of the pinned directory |
//!
//! ## Honesty about durability
//!
//! The primitives differ by platform: on `target_vendor = "apple"` a file flush is
//! `fcntl(F_FULLFSYNC)` rather than `fsync(2)`; on Linux it is plain `fsync(2)`, and the
//! **directory** descriptor is flushed separately because `fsync(2)` does not cover the
//! parent entry. Both are what the seam asks for; neither is by itself a durability claim
//! about a *filesystem*.
//!
//! Three constructors exist, and only one can claim durability:
//!
//! * [`OsFileSystem::conservative`] — every primitive reported honestly,
//!   `durable_sync = false`, unconditionally. Never claims anything.
//! * [`OsFileSystem::from_platform_policy`] — consults the `os/arch` policy table, which is
//!   **empty**: an OS/arch pair alone says nothing about the filesystem actually mounted, so
//!   it cannot be the basis of a claim. It therefore also yields `NotClaimed`.
//! * [`OsFileSystem::for_root`] — the checked constructor. It reads the actual kernel
//!   release, the actual `/proc/self/mountinfo`, and the **pinned descriptor's own mount
//!   identity**, and claims durability only for a narrowly listed kernel running the one
//!   filesystem whose flush primitives were exercised on it. The bound is exact (§ the
//!   constants below) and a host outside it is refused with a typed reason.
//!
//! There is no caller-asserted override, no environment variable and no public
//! `bool`/`enum` constructor: a facility operator could otherwise manufacture the claim the
//! store exists to withhold. See `docs/design/platform-linux-btrfs.md` for the evidence
//! behind the bound and for what it still does not assert.

use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use sage_db_types::ledger::SharedLedger;

use crate::fs::{
    Capabilities, CreateDir, Each, Effect, Family, FileSystem, FsError, FsErrorKind, HashReport,
    PlatformCapability, PublishMode, Published, WriteOutcome,
};
use crate::names::EntryName as Name;

/// A handle to one directory, holding the path it was opened from.
///
/// Containment comes from the **pinned descriptor**, not from the stored path. `cap-std` exposes
/// fd-relative resolution on stable Rust (std has no `openat` and `std::fs::Dir` is nightly), so
/// every child operation — open, create, sync, rename, link, unlink, metadata, enumerate — goes
/// through `handle`. The recorded `path` is diagnostics and parent-sync input only: re-resolving
/// it would follow an external rename of the directory to a replacement with different contents.
/// Child *names* are additionally typed ([`crate::names::EntryName`]) so no caller string reaches
/// the filesystem as a component.
#[derive(Debug)]
pub struct OsDir {
    /// A pinned directory handle. Child names are resolved **relative to this descriptor**, not by
    /// re-walking an absolute path, so an external `rename` of the directory cannot redirect a
    /// subsequent operation to a replacement directory.
    handle: cap_std::fs::Dir,
    /// Kept for diagnostics and for the parent-sync path.
    path: PathBuf,
    family: Option<Family>,
    /// A descriptor for the **same inode** that `fsync` accepts.
    ///
    /// Acquired once when this directory is opened, never inside a commit: `/proc/self/fd/<fd>` has
    /// to be formatted into a path, and doing that in `sync_dir` would put a fresh allocation and a
    /// reopen inside the commit interval after `CURRENT` is replaced. `cap-std` opens directories
    /// `O_PATH` on Linux, where `fsync` is `EBADF`, so the raw handle alone cannot be flushed.
    sync_file: File,
}

impl OsDir {
    /// The absolute path this directory was opened from.
    ///
    /// Informational only: it is **not** used to resolve children, because a stored path can be
    /// invalidated by an external rename.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which family this directory is, or `None` for the KB root.
    #[must_use]
    pub const fn family(&self) -> Option<Family> {
        self.family
    }
}

/// A file open for writing.
#[derive(Debug)]
pub struct OsWriteFile {
    file: File,
    path: PathBuf,
}

impl OsWriteFile {
    /// The path being written. Kept for diagnostics and error attribution.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A file open for reading.
#[derive(Debug)]
pub struct OsReadFile {
    file: File,
}

/// The exclusive KB lock, held for the handle's lifetime.
///
/// Kept as an owned `File`: closing the last descriptor releases the lock, so this struct must not
/// be cloned (a second descriptor would not share the lock's lifetime the way BSD `flock` semantics
/// require).
#[derive(Debug)]
pub struct OsLock {
    _file: File,
}

/// Which durability semantics this host actually provides.
///
/// Determined by the backend from **platform facts it can state**, never by a caller flag. An
/// earlier revision exposed a public `with_asserted_durability()` constructor, which let a caller
/// flip APFS from "not claimed" to "claimed" with no platform fact behind it — a capability claim
/// manufactured by configuration. That is removed: real OS eligibility comes from
/// [`OsFileSystem::for_root`], and unit tests that need a qualifying `Capabilities` value
/// construct the struct directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DurabilityBasis {
    /// A narrowly identified platform + filesystem pair was validated against this exact root,
    /// and this build implements that platform's policy.
    Documented,
    /// Primitives work, durability is not claimed: the honest answer on APFS, on any host whose
    /// directory-entry durability is not documented, and on every host that was never checked.
    NotClaimed,
}

/// Platform policy: `os/arch` keys whose sync contract the **build** implements, independent of
/// which filesystem is mounted.
///
/// Deliberately empty, and it must stay that way. An `os/arch` pair says nothing about the
/// filesystem, the kernel, the mount options or the device behind a path, so no entry here can
/// justify a durability claim. [`OsFileSystem::from_platform_policy`] therefore yields
/// [`DurabilityBasis::NotClaimed`] on every host, including Linux; the checked constructor
/// [`OsFileSystem::for_root`] is the only path to `Documented`.
///
/// macOS/APFS is refused on purpose: the platform documents `F_FULLFSYNC` as a *file* barrier but
/// nothing about directory-entry durability, so APFS cannot satisfy the contract even if the
/// filesystem were mounted on a Linux host through FUSE.
const DOCUMENTED_DURABLE_PLATFORMS: &[&str] = &[];

/// The kernel release prefix this build's Linux policy was validated on.
///
/// Exact family/version boundary, not "any Linux": the fsync path below lives in
/// `btrfs_sync_file` and the log/super barrier path in `write_all_supers`, and a different
/// kernel is a different implementation of the same contract until someone revalidates it.
const POLICY_LINUX_KERNEL_PREFIX: &str = "7.2.11-arch1-";

/// One exact release admitted by the x99 follow-up qualification.
const POLICY_LINUX_KERNEL_RELEASE: &str = "7.3.5-arch1-1";

fn kernel_release_allowed(release: &str) -> bool {
    release.starts_with(POLICY_LINUX_KERNEL_PREFIX) || release == POLICY_LINUX_KERNEL_RELEASE
}

/// The only filesystem this build may claim on Linux.
const POLICY_LINUX_FILESYSTEM: &str = "btrfs";

/// Mount options that would void the claim, matched as whole option tokens.
///
/// `nobarrier` is the one that matters: it makes Btrfs downgrade its flush submissions
/// (`barrier_all_devices` is skipped and the first super block loses `REQ_FUA`), which is
/// precisely the primitive this policy depends on. Read-only and `noatime`-class options are
/// irrelevant; anything unknown is *not* treated as unsafe by omission, it simply is not in this
/// list because it does not change the flush path.
const POLICY_VOIDING_MOUNT_OPTIONS: &[&str] = &["nobarrier"];

/// Why a root could not be accepted by [`OsFileSystem::for_root`].
///
/// Every variant names a fact the constructor actually observed and could not accept. None of
/// them is a durability verdict about a filesystem that was not inspected: an unreadable
/// `/proc`, a malformed mount record and a mount that does not match the pinned descriptor are
/// three different operator situations with three different remedies.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PlatformPolicyError {
    /// This build has no checked policy for the compile-time target.
    UnsupportedTarget {
        /// `std::env::consts::OS`.
        os: &'static str,
        /// `std::env::consts::ARCH`.
        arch: &'static str,
    },
    /// The kernel release is outside the validated policy bound.
    UnsupportedKernel {
        /// What `/proc/sys/kernel/osrelease` reported.
        observed: String,
        /// The prefix this build accepts.
        accepted_prefix: &'static str,
        /// The additional exact release this build accepts.
        accepted_release: &'static str,
    },
    /// The kernel release could not be read at all.
    KernelUnreadable,
    /// `/proc/self/mountinfo` could not be read or was not parseable.
    MountTableUnreadable,
    /// The root path is not absolute, so it cannot be bound to a mount.
    RelativeRoot,
    /// The root (or its parent, on the init path) does not exist.
    RootMissing,
    /// The root exists but is not a directory.
    RootNotADirectory,
    /// No mount record covers the root path.
    NoMountForRoot,
    /// The longest matching mount record is not the filesystem this policy claims.
    UnsupportedFilesystem {
        /// The `fstype` field of the covering mount record.
        observed: String,
        /// The only filesystem this build claims.
        accepted: &'static str,
    },
    /// The covering mount carries an option that voids the flush contract.
    VoidedByMountOption {
        /// The offending option, e.g. `nobarrier`.
        option: &'static str,
    },
    /// The mount record that covers the root is not the mount the pinned descriptor is on.
    ///
    /// A bind mount, an overlay, or an external remount between the path walk and the descriptor
    /// open would otherwise let the constructor validate one filesystem and return a backend that
    /// writes to another.
    MountContextMismatch {
        /// `MAJ:MIN` claimed by the covering mount record.
        mount_device: String,
        /// The mount id the pinned descriptor actually reports.
        descriptor_mount_id: u64,
    },
    /// The descriptor's own mount identity could not be read.
    MountIdentityUnreadable,
    /// The root's direct parent could not be pinned.
    ///
    /// A root at the filesystem root has no parent, and a parent whose descriptor cannot be opened
    /// cannot be the authority for a creation or a parent flush. Refusing is the honest answer;
    /// the alternative is to re-walk the path later and claim durability for whatever directory
    /// then occupies it.
    ParentUnavailable,
    /// The pinned parent's identity no longer matches what the constructor recorded.
    ///
    /// Distinguishes "the descriptor I hold is no longer the directory I validated" from a path
    /// that merely names something else now.
    ParentIdentityChanged,
    /// A real I/O failure while binding the root.
    Filesystem(FsError),
}

impl core::fmt::Display for PlatformPolicyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedTarget { os, arch } => {
                write!(f, "no checked platform policy for {os}/{arch}")
            }
            Self::UnsupportedKernel {
                observed,
                accepted_prefix,
                accepted_release,
            } => write!(
                f,
                "kernel {observed} is outside the validated bound {accepted_prefix}* or {accepted_release}"
            ),
            Self::KernelUnreadable => f.write_str("the kernel release could not be read"),
            Self::MountTableUnreadable => f.write_str("the mount table could not be read"),
            Self::RelativeRoot => f.write_str("the root path must be absolute"),
            Self::RootMissing => f.write_str("the root does not exist"),
            Self::RootNotADirectory => f.write_str("the root is not a directory"),
            Self::NoMountForRoot => f.write_str("no mount record covers the root"),
            Self::UnsupportedFilesystem { observed, accepted } => {
                write!(f, "filesystem {observed} is not {accepted}")
            }
            Self::VoidedByMountOption { option } => {
                write!(f, "mount option {option} voids the flush contract")
            }
            Self::MountContextMismatch {
                mount_device,
                descriptor_mount_id,
            } => write!(
                f,
                "mount record {mount_device} is not the descriptor's mount {descriptor_mount_id}"
            ),
            Self::MountIdentityUnreadable => {
                f.write_str("the descriptor's mount identity could not be read")
            }
            Self::ParentUnavailable => f.write_str("the root's direct parent could not be pinned"),
            Self::ParentIdentityChanged => {
                f.write_str("the pinned parent is no longer the validated directory")
            }
            Self::Filesystem(error) => write!(f, "{error:?}"),
        }
    }
}

impl core::error::Error for PlatformPolicyError {}

impl From<FsError> for PlatformPolicyError {
    fn from(error: FsError) -> Self {
        Self::Filesystem(error)
    }
}

/// One parsed `/proc/self/mountinfo` record, reduced to the fields the policy reads.
#[derive(Clone, PartialEq, Eq, Debug)]
struct MountRecord {
    /// Mount id (field 1).
    mount_id: u64,
    /// `MAJ:MIN` of the mounted filesystem (field 3).
    device: String,
    /// Root of the mount within the filesystem (field 4), unescaped.
    root: String,
    /// Mount point (field 5), unescaped.
    mount_point: String,
    /// `fstype` from the optional-fields separator onward.
    filesystem: String,
    /// Per-mount VFS options (field 6), unescaped: `rw,noatime,shared:1`.
    options: String,
    /// Filesystem-specific **superblock** options (after the source), unescaped.
    ///
    /// This is where Btrfs puts the options that change its behaviour — `compress=`, `ssd`,
    /// `discard=async`, and `nobarrier`. Checking only the per-mount list would therefore miss
    /// exactly the option that voids the flush contract.
    super_options: String,
}

/// Identity of one pinned directory: what survives a path being re-pointed.
///
/// A mount id answers "which mount". A directory can be replaced **within** one mount by renaming
/// a sibling over it, which leaves the mount id unchanged — so the mount id alone cannot tell "the
/// directory I validated" from "whatever now sits at that path". Recording the inode number and
/// device as well makes that substitution observable instead of assumed away.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct DirIdentity {
    /// `st_dev` of the directory.
    device: u64,
    /// `st_ino` of the directory.
    inode: u64,
    /// Mount id the descriptor reports, when the host exposes one.
    ///
    /// Mandatory on Linux (the only host a validated backend can exist on); `None` on hosts without
    /// `/proc`. A same-mount replacement is caught by `inode` regardless.
    mount_id: Option<u64>,
}

/// A parent directory held open for the life of the backend.
///
/// The descriptor, not the path, is the authority: `create_dir` and the parent flush both go
/// through this handle, so a rename or a same-mount replacement of the recorded path redirects
/// neither. `identity` is re-verified on every use.
#[derive(Debug)]
struct PinnedParent {
    /// The pinned descriptor.
    handle: cap_std::fs::Dir,
    /// Identity recorded when the constructor pinned it.
    identity: DirIdentity,
    /// A descriptor for the same inode that `fsync` accepts, acquired with the handle.
    ///
    /// Acquired once here so the flush that follows `CURRENT` replacement neither allocates nor
    /// reopens a path.
    sync_file: File,
}

/// What the policy validated, so the backend can refuse a root it did not check.
///
/// Private on purpose: a caller cannot build one, so a backend constructed from a policy result
/// cannot be pointed at another path without re-running the checks.
#[derive(Debug)]
struct ValidatedRoot {
    /// The path the caller asked about.
    ///
    /// Distinct from `bound`: for an existing root they are equal, and for the initialization case
    /// `root` is the not-yet-created child while `bound` is its pinned parent. Collapsing the two
    /// let `for_root(existing)` serve the *parent* on a later `open_root`, which is a different
    /// directory and a different answer.
    root: PathBuf,
    /// The directory that was actually pinned and checked for the mount-level facts.
    bound: PathBuf,
    /// The root's direct parent, pinned and identified.
    ///
    /// `create_root` and `sync_parent_of` operate through this descriptor. It is the root's parent
    /// in **both** construction cases: when the root already exists, `bound` is the root itself and
    /// this parent is a different directory that must never be mistaken for it.
    parent: PinnedParent,
    /// Mount id the pinned descriptor reported.
    ///
    /// The mount name and device string were only ever needed to *produce* the refusals during
    /// construction (each error variant carries them). Keeping copies here would be state nobody
    /// reads, so the accepted root records only what the guards compare against.
    mount_id: u64,
}

/// The real backend.
#[derive(Debug)]
pub struct OsFileSystem {
    durability: DurabilityBasis,
    /// Present only when [`OsFileSystem::for_root`] accepted a specific root. A backend without
    /// it never claims durability, and the root guard refuses paths it did not validate.
    validated: Option<ValidatedRoot>,
}

impl OsFileSystem {
    /// A backend that reports every primitive it truly has, but **does not claim durability**.
    ///
    /// This is the honest default on APFS and on any host where directory-entry durability is not
    /// documented, and it is what every unvalidated host gets. A product handle cannot be opened
    /// against it.
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            durability: DurabilityBasis::NotClaimed,
            validated: None,
        }
    }

    /// A backend whose durability basis comes from the compile-time platform policy table.
    ///
    /// That table is empty by design, and no later edit may make it "Linux is durable": an
    /// `os/arch` key carries no filesystem, kernel, mount-option or device fact. Use
    /// [`OsFileSystem::for_root`] to obtain a backend that did check those things.
    #[must_use]
    pub fn from_platform_policy() -> Self {
        let key = format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH);
        let durability = if DOCUMENTED_DURABLE_PLATFORMS.contains(&key.as_str()) {
            DurabilityBasis::Documented
        } else {
            DurabilityBasis::NotClaimed
        };
        Self {
            durability,
            validated: None,
        }
    }
    /// The durability basis this backend will report.
    ///
    /// Read-only accessor: it exists so a caller (and the qualification test) can observe what
    /// policy actually decided instead of inferring it from a successful open.
    #[must_use]
    pub const fn durability(&self) -> DurabilityBasis {
        self.durability
    }

    /// The path [`OsFileSystem::for_root`] accepted, if any.
    ///
    /// This is the **root** the caller asked about, not necessarily the directory that was pinned:
    /// for the initialization case the root does not exist yet, so its parent was the directory
    /// checked. `None` for [`OsFileSystem::conservative`] and
    /// [`OsFileSystem::from_platform_policy`], which bound no filesystem and may claim none.
    #[must_use]
    pub fn validated_root(&self) -> Option<&Path> {
        self.validated.as_ref().map(|bound| bound.root.as_path())
    }

    /// The existing directory that was actually pinned and checked, if any.
    ///
    /// Equal to [`OsFileSystem::validated_root`] when the root already existed; equal to its direct
    /// parent when the constructor prepared an initialization.
    #[must_use]
    pub fn bound_directory(&self) -> Option<&Path> {
        self.validated.as_ref().map(|bound| bound.bound.as_path())
    }

    /// Build a backend **only after checking the actual platform facts behind `root_path`**.
    ///
    /// This is the single checked path to [`DurabilityBasis::Documented`]. It reads, in order:
    ///
    /// 1. the compile-time target — only `linux/x86_64` has a policy here;
    /// 2. the running kernel release from `/proc/sys/kernel/osrelease`, matched against the exact
    ///    validated prefix (not "any Linux");
    /// 3. the path itself, which must be absolute, and which must be either an existing directory
    ///    or a not-yet-existing entry whose **direct parent** exists (the init path);
    /// 4. `/proc/self/mountinfo`, parsed with `\\040`-style unescaping and matched by longest
    ///    component prefix, for the filesystem type and mount options covering that path;
    /// 5. the **pinned descriptor's own mount id** from `/proc/self/fdinfo/<fd>`, which must equal
    ///    the covering mount record's id.
    ///
    /// Step 5 is what makes the check honest rather than textual: a bind mount, an overlay, a
    /// symlinked component or a remount between the walk and the open makes the record and the
    /// descriptor disagree, and the constructor refuses instead of validating one filesystem and
    /// returning a backend that writes to another.
    ///
    /// The accepted releases and `POLICY_LINUX_FILESYSTEM` are checked, with
    /// none of `POLICY_VOIDING_MOUNT_OPTIONS`. Everything else — tmpfs, ext4, NFS, overlayfs,
    /// any other kernel — is refused with its own reason; none of them is assumed to work by
    /// resemblance.
    ///
    /// # Errors
    ///
    /// [`PlatformPolicyError`] naming the fact that did not hold. A caller must not translate a
    /// refusal into a retry with a laxer input: there is no laxer input.
    pub fn for_root(root_path: &Path) -> Result<Self, PlatformPolicyError> {
        // Evaluated as a runtime branch on purpose: the Linux body below stays compiled (so it is
        // type-checked and its helpers are exercised by unit tests on every host), while a
        // non-Linux build refuses before it reads any `/proc` file.
        if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            let _ = root_path;
            return Err(PlatformPolicyError::UnsupportedTarget {
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
            });
        }
        Self::for_root_linux(root_path)
    }

    /// The Linux/x86_64 policy body.
    ///
    /// Compiled on every host so [`parse_mount_table`] and its siblings have a real caller and
    /// are unit-tested where they run; reachable only through the checked target branch above.
    fn for_root_linux(root_path: &Path) -> Result<Self, PlatformPolicyError> {
        if !root_path.is_absolute() {
            return Err(PlatformPolicyError::RelativeRoot);
        }
        let release = read_kernel_release()?;
        if !kernel_release_allowed(&release) {
            return Err(PlatformPolicyError::UnsupportedKernel {
                observed: release,
                accepted_prefix: POLICY_LINUX_KERNEL_PREFIX,
                accepted_release: POLICY_LINUX_KERNEL_RELEASE,
            });
        }

        // The path must be bound to itself (existing root) or to its direct parent (init).
        let bound_path = match fs::symlink_metadata(root_path) {
            Ok(meta) => {
                if !meta.is_dir() {
                    return Err(PlatformPolicyError::RootNotADirectory);
                }
                root_path.to_path_buf()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = root_path.parent().ok_or(PlatformPolicyError::RootMissing)?;
                // Parent must exist and be a directory; a missing parent is not a policy verdict.
                match fs::symlink_metadata(parent) {
                    Ok(meta) if meta.is_dir() => parent.to_path_buf(),
                    Ok(_) => return Err(PlatformPolicyError::RootNotADirectory),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Err(PlatformPolicyError::RootMissing);
                    }
                    Err(error) => {
                        return Err(PlatformPolicyError::Filesystem(io_error(
                            &error,
                            Effect::NoEffect,
                        )));
                    }
                }
            }
            Err(error) => {
                return Err(PlatformPolicyError::Filesystem(io_error(
                    &error,
                    Effect::NoEffect,
                )));
            }
        };

        // Pin the exact directory the policy decides about. Opening is not a write.
        let handle = cap_std::fs::Dir::open_ambient_dir(&bound_path, cap_std::ambient_authority())
            .map_err(|error| PlatformPolicyError::Filesystem(io_error(&error, Effect::NoEffect)))?;

        // Pin the root's DIRECT parent, always — in both construction cases. An existing root and a
        // to-be-created root differ in which directory `bound` is, but `create_root` and
        // `sync_parent_of` must go through the parent in both, and that parent must be a
        // descriptor rather than a path that can be re-pointed before the flush.
        //
        // A root at the filesystem root has no parent, so it cannot support either operation; that
        // is `ParentUnavailable`, not a silent fallback to re-walking.
        let parent_path = root_path
            .parent()
            .ok_or(PlatformPolicyError::ParentUnavailable)?;
        let parent_handle =
            cap_std::fs::Dir::open_ambient_dir(parent_path, cap_std::ambient_authority())
                .map_err(|_| PlatformPolicyError::ParentUnavailable)?;
        let parent_identity =
            dir_identity(&parent_handle).ok_or(PlatformPolicyError::ParentUnavailable)?;
        // Acquired now, used later: the flush after `CURRENT` replacement must not allocate.
        let parent_sync = syncable_dir_file(&parent_handle)
            .map_err(|_| PlatformPolicyError::ParentUnavailable)?;
        let parent = PinnedParent {
            handle: parent_handle,
            identity: parent_identity,
            sync_file: parent_sync,
        };

        let records = read_mount_table()?;
        let covering =
            covering_mount(&records, &bound_path).ok_or(PlatformPolicyError::NoMountForRoot)?;

        // The descriptor is the authority on which mount this really is.
        let descriptor_mount_id =
            descriptor_mount_id(&handle).ok_or(PlatformPolicyError::MountIdentityUnreadable)?;
        if descriptor_mount_id != covering.mount_id {
            return Err(PlatformPolicyError::MountContextMismatch {
                mount_device: covering.device.clone(),
                descriptor_mount_id,
            });
        }

        if covering.filesystem != POLICY_LINUX_FILESYSTEM {
            return Err(PlatformPolicyError::UnsupportedFilesystem {
                observed: covering.filesystem.clone(),
                accepted: POLICY_LINUX_FILESYSTEM,
            });
        }
        for option in POLICY_VOIDING_MOUNT_OPTIONS {
            // Both regions are consulted: on Btrfs `nobarrier` is a **superblock** option and never
            // appears in field 6, so checking only the per-mount list would pass a voided mount.
            if has_mount_option(&covering.options, option)
                || has_mount_option(&covering.super_options, option)
            {
                return Err(PlatformPolicyError::VoidedByMountOption { option });
            }
        }

        Ok(Self {
            durability: DurabilityBasis::Documented,
            validated: Some(ValidatedRoot {
                root: root_path.to_path_buf(),
                bound: bound_path,
                parent,
                mount_id: covering.mount_id,
            }),
        })
    }

    /// Refuse a root path this backend did not validate.
    ///
    /// A conservative backend (no bound root) accepts any path, exactly as before: it makes no
    /// claim, so it has nothing to protect. A validated backend accepts exactly one root — the
    /// path the constructor was asked about — whether that path already existed (so it *is* the
    /// pinned directory) or did not (so its parent was pinned for initialization).
    fn guard_root_path(&self, abs: &Path) -> Result<(), FsError> {
        match self.validated.as_ref() {
            None => Ok(()),
            Some(bound) if bound.root == abs => Ok(()),
            Some(_) => Err(FsError::no_effect(FsErrorKind::Unsupported)),
        }
    }

    /// The pinned parent this backend will create under and flush, re-verified on use.
    ///
    /// Returns `Ok(None)` for an unvalidated backend (it makes no claim, so it keeps the previous
    /// behaviour of re-opening the path). For a validated backend the recorded descriptor must
    /// still identify the held directory from construction. This checks the descriptor itself;
    /// it does not re-open the old pathname or detect a directory renamed over that pathname.
    /// Operations continue to use the original held inode instead of being redirected by a rename.
    ///
    /// Using the descriptor here — not the path — is the whole point: between the guard and the
    /// flush the path can be re-pointed, and `File::open(parent)` would then flush a directory the
    /// policy never validated.
    fn pinned_parent(&self, abs: &Path) -> Result<Option<&PinnedParent>, FsError> {
        let Some(bound) = self.validated.as_ref() else {
            return Ok(None);
        };
        // Only the one root this backend was built for: "any child of a validated parent" would
        // extend the claim to siblings nobody inspected.
        if bound.root != abs {
            return Err(FsError::no_effect(FsErrorKind::Unsupported));
        }
        let current = dir_identity(&bound.parent.handle)
            .ok_or_else(|| FsError::no_effect(FsErrorKind::Unsupported))?;
        if current != bound.parent.identity {
            return Err(FsError::no_effect(FsErrorKind::Unsupported));
        }
        Ok(Some(&bound.parent))
    }

    /// Refuse a descriptor that is not on the validated mount.
    ///
    /// This is the second half of the bind: the path check says "you asked for the right name",
    /// and this says "the inode you actually got lives on the device I validated".
    fn guard_descriptor(&self, handle: &cap_std::fs::Dir) -> Result<(), FsError> {
        let Some(bound) = self.validated.as_ref() else {
            return Ok(());
        };
        match descriptor_mount_id(handle) {
            Some(observed) if observed == bound.mount_id => Ok(()),
            // Unreadable identity is not a pass: without it the bind cannot be confirmed.
            Some(_) | None => Err(FsError::no_effect(FsErrorKind::Unsupported)),
        }
    }

    /// Open a child for reading with every layout guard any read path needs.
    ///
    /// One helper, shared by `open_read`, `read_exact_into` and `hash_bounded`, so a guard cannot
    /// exist on one sibling read and be missing on another — which is exactly the defect the
    /// parent's probe found: `open_read` refused a symlinked `CURRENT` while `read_exact_into` still
    /// returned `Ok(7)`. Resolution goes through the pinned descriptor, then a symlink is refused
    /// and a non-regular file is refused.
    fn open_regular(dir: &OsDir, name: Name) -> Result<File, FsError> {
        let fname = name.to_file_name();
        let meta = dir
            .handle
            .symlink_metadata(&fname)
            .map_err(|e| io_error(&e, Effect::NoEffect))?;
        if meta.file_type().is_symlink() {
            return Err(FsError::new(FsErrorKind::Symlink, Effect::NoEffect));
        }
        if !meta.is_file() {
            return Err(FsError::new(FsErrorKind::NotRegular, Effect::NoEffect));
        }
        dir.handle
            .open(&fname)
            .map(cap_std::fs::File::into_std)
            .map_err(|e| io_error(&e, Effect::NoEffect))
    }

    /// Remove the staged name after a successful publication, reporting a truthful effect.
    ///
    /// The name-level publication already happened, so a cleanup failure must not be described as
    /// "nothing occurred". `NotFound` on cleanup is success: the name is gone either way, which is
    /// the postcondition this call owes.
    ///
    /// **`Effect::Entailed` is a statement about the filesystem operation, not about commit
    /// authority.** It says the link/unlink performed here took effect. It says nothing about
    /// whether any transaction is committed: `CURRENT` remains the sole commit authority, and no
    /// filesystem effect can promote an orphan `dst` name into a committed record.
    fn consume_staged(dir: &OsDir, name: Name) -> Result<(), FsError> {
        match dir.handle.remove_file(name.to_file_name()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            // The link/rename itself took effect (filesystem effect), so the failure cannot be
            // reported as `NoEffect`. Commit authority is unaffected: that is decided only by
            // `CURRENT`, never by a published name.
            Err(e) => Err(io_error(&e, Effect::Entailed)),
        }
    }
}

/// Read the running kernel release from the kernel's own file, never from a string built here.
fn read_kernel_release() -> Result<String, PlatformPolicyError> {
    let raw = fs::read_to_string("/proc/sys/kernel/osrelease")
        .map_err(|_| PlatformPolicyError::KernelUnreadable)?;
    let release = raw.trim();
    if release.is_empty() {
        return Err(PlatformPolicyError::KernelUnreadable);
    }
    Ok(release.to_owned())
}

/// Read and parse `/proc/self/mountinfo`.
///
/// A line that does not match the documented shape is refused, not skipped: silently dropping a
/// record could hide the mount that actually covers the root and let a different record pass as
/// the answer.
fn read_mount_table() -> Result<Vec<MountRecord>, PlatformPolicyError> {
    let raw = fs::read_to_string("/proc/self/mountinfo")
        .map_err(|_| PlatformPolicyError::MountTableUnreadable)?;
    parse_mount_table(&raw).ok_or(PlatformPolicyError::MountTableUnreadable)
}

/// Parse the whole table, or `None` if any record is malformed.
fn parse_mount_table(raw: &str) -> Option<Vec<MountRecord>> {
    let mut records = Vec::new();
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        records.push(parse_mount_line(line)?);
    }
    if records.is_empty() {
        return None;
    }
    Some(records)
}

/// Parse one mountinfo record: `id parent maj:min root point opts [optional…] - fstype src super`.
///
/// Three variable regions matter and the policy reads all of them: mandatory mount options
/// (field 6), the optional per-mount fields terminated by ` - `, and the **superblock** options
/// after the source. Btrfs puts its behaviour-changing options in the last region, so reading only
/// field 6 would silently miss `nobarrier` — the one that voids the flush contract.
fn parse_mount_line(line: &str) -> Option<MountRecord> {
    let fields: Vec<&str> = line.split(' ').collect();
    let separator = fields.iter().position(|field| *field == "-")?;
    // Field 6 (mount options) precedes the separator; fstype, source and super options follow it.
    if separator < 6 || separator + 3 >= fields.len() {
        return None;
    }
    Some(MountRecord {
        mount_id: fields[0].parse().ok()?,
        device: fields[2].to_owned(),
        root: unescape_mount_field(fields[3]),
        mount_point: unescape_mount_field(fields[4]),
        filesystem: fields[separator + 1].to_owned(),
        options: unescape_mount_field(fields[5]),
        super_options: unescape_mount_field(fields[separator + 3]),
    })
}

/// Undo the octal escaping mountinfo applies to whitespace and backslashes.
fn unescape_mount_field(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let digits = &raw[index + 1..index + 4];
            if let Ok(value) = u8::from_str_radix(digits, 8) {
                // Every escape mountinfo emits is a single ASCII byte.
                if value.is_ascii() {
                    out.push(value as char);
                    index += 4;
                    continue;
                }
            }
        }
        // A lone backslash is literal here; mountinfo only escapes as \NNN.
        let ch = raw[index..].chars().next().unwrap_or('\\');
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// The mount record with the longest mount point that contains `path`.
///
/// Component-prefix matching, not string-prefix: `/srv/kb2` must not be covered by `/srv/kb`.
/// The longest match wins, because that is the mount the kernel resolves the path through.
fn covering_mount<'a>(records: &'a [MountRecord], path: &Path) -> Option<&'a MountRecord> {
    let needle = path.to_str()?;
    records
        .iter()
        .filter(|record| mount_contains(&record.mount_point, needle))
        .max_by_key(|record| record.mount_point.len())
}

/// Whether `mount_point` contains `path` on a component boundary.
fn mount_contains(mount_point: &str, path: &str) -> bool {
    if mount_point == "/" {
        return path.starts_with('/');
    }
    let trimmed = mount_point.trim_end_matches('/');
    if path == trimmed {
        return true;
    }
    path.strip_prefix(trimmed)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The mount id the kernel associates with this pinned descriptor.
///
/// `/proc/self/fdinfo/<fd>` is the only in-band way to ask which mount a specific descriptor was
/// opened on: two paths that name the same text can resolve to different mounts after a bind or a
/// remount, and this is what makes the difference observable without `unsafe` statx calls.
fn descriptor_mount_id(handle: &cap_std::fs::Dir) -> Option<u64> {
    use std::os::fd::AsRawFd as _;
    let fd = handle.as_raw_fd();
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).ok()?;
    for line in info.lines() {
        if let Some(value) = line.strip_prefix("mnt_id:") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// Identity of a pinned directory: device, inode and (where available) mount.
///
/// `mnt_id` distinguishes mounts; `(st_dev, st_ino)` distinguishes directories **within** one
/// mount. Both matter, because a sibling renamed over the validated path keeps the mount id and
/// only the inode changes.
///
/// `mount_id` is `None` on hosts without `/proc` (macOS). It is never *required* by the identity
/// comparison that guards a validated backend, because such a backend only exists on Linux, where
/// `Some` is enforced; a `None` there degrades to device+inode rather than passing silently.
fn dir_identity(handle: &cap_std::fs::Dir) -> Option<DirIdentity> {
    use cap_std::fs::MetadataExt as _;
    let meta = handle.dir_metadata().ok()?;
    Some(DirIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        mount_id: descriptor_mount_id(handle),
    })
}

/// Acquire a descriptor for a pinned directory's **same inode** that `fsync` accepts.
///
/// Called once, when the directory is opened — never inside a commit. On Linux `cap-std` opens
/// directories `O_PATH`, and `fsync(2)` on an `O_PATH` descriptor fails with `EBADF` (a visible
/// typed error, not a successful ACK, and the pre-policy backend could not open a product handle at
/// all). `/proc/self/fd/<fd>` reopens the **same inode** through the descriptor — not through the
/// recorded path, so an external rename cannot redirect it — giving a real read-only descriptor
/// `fsync` accepts.
///
/// Doing this here rather than in the flush matters: `sync_dir`/`sync_parent_of` run after
/// `CURRENT` is replaced, and formatting a `/proc` path there would add a fresh allocation and a
/// reopen inside the commit interval. This is not a claim about power-loss behaviour.
///
/// # Errors
///
/// The descriptor could not be obtained. `Effect::NoEffect`: acquiring a descriptor writes nothing.
fn syncable_dir_file(handle: &cap_std::fs::Dir) -> Result<File, FsError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd as _;
        let fd = handle.as_raw_fd();
        File::open(format!("/proc/self/fd/{fd}")).map_err(|e| io_error(&e, Effect::NoEffect))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // macOS has no `/proc`; a cloned descriptor is already acceptable to `F_FULLFSYNC`
        // (unchanged APFS behaviour).
        let cloned = handle
            .try_clone()
            .map_err(|e| io_error(&e, Effect::NoEffect))?;
        Ok(cloned.into_std_file())
    }
}

/// Flush a pinned directory through the descriptor acquired with it.
///
/// `handle` is only consulted for the already-verified identity relationship at the call site; the
/// flush itself uses `held`, so no path is resolved and nothing is allocated.
///
/// A flush failure is `Effect::Unknown`: it may have completed.
fn flush_held(_handle: &cap_std::fs::Dir, held: &File) -> Result<(), FsError> {
    held.sync_all().map_err(|e| io_error(&e, Effect::Unknown))
}

/// Whether `options` contains `wanted` as a whole comma-separated token.
///
/// Substring matching would let `nobarrier_safe` or `xnobarrier` void (or fail to void) a claim.
fn has_mount_option(options: &str, wanted: &str) -> bool {
    options.split(',').any(|option| option == wanted)
}

/// Whether a failed *creating* call certainly left nothing behind.
///
/// These are the errors the kernel returns without publishing a directory entry: a missing or
/// forbidden parent, a wrong-type parent, a read-only mount, a full device, an unusable name, a
/// name already present. Anything else (an I/O error mid-create) may have taken effect and must
/// be reported as ambiguous rather than as "nothing happened".
fn is_definite_refusal(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::AlreadyExists
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::IsADirectory
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::ReadOnlyFilesystem
            | std::io::ErrorKind::StorageFull
            | std::io::ErrorKind::QuotaExceeded
            | std::io::ErrorKind::InvalidInput
    )
}

/// Classify an I/O error, with an explicit effect judgement.
///
/// `effect` is supplied by the caller because only the call site knows whether a partial effect was
/// possible: `create_new` cannot have created anything on `AlreadyExists`, while a `write` may have
/// accepted a prefix before failing.
fn io_error(err: &std::io::Error, effect: Effect) -> FsError {
    let kind = match err.kind() {
        std::io::ErrorKind::NotFound => FsErrorKind::NotFound,
        std::io::ErrorKind::AlreadyExists => FsErrorKind::AlreadyExists,
        std::io::ErrorKind::NotADirectory => FsErrorKind::NotADirectory,
        std::io::ErrorKind::IsADirectory => FsErrorKind::IsADirectory,
        std::io::ErrorKind::PermissionDenied => FsErrorKind::PermissionDenied,
        std::io::ErrorKind::StorageFull => FsErrorKind::NoSpace,
        std::io::ErrorKind::QuotaExceeded => FsErrorKind::QuotaExceeded,
        std::io::ErrorKind::CrossesDevices => FsErrorKind::CrossDevice,
        std::io::ErrorKind::WouldBlock => FsErrorKind::WouldBlock,
        std::io::ErrorKind::Interrupted => FsErrorKind::Interrupted,
        std::io::ErrorKind::Unsupported => FsErrorKind::Unsupported,
        _ => FsErrorKind::Io(err.raw_os_error().unwrap_or(0)),
    };
    FsError::new(kind, effect)
}

impl FileSystem for OsFileSystem {
    type Dir = OsDir;
    type WriteFile = OsWriteFile;
    type ReadFile = OsReadFile;
    type Lock = OsLock;

    fn capabilities(&self) -> Capabilities {
        // Every primitive below is genuinely available; `durable_sync` reflects the caller's
        // assertion, because the backend cannot verify a platform's documented guarantees.
        Capabilities {
            supported: vec![
                PlatformCapability::ExclusiveLock,
                PlatformCapability::FileSync,
                PlatformCapability::DirSync,
                PlatformCapability::AtomicReplace,
                PlatformCapability::NoOverwritePublish,
                PlatformCapability::SameDevice,
            ],
            durable_sync: matches!(self.durability, DurabilityBasis::Documented),
        }
    }

    fn open_root(&self, abs: &Path) -> Result<Self::Dir, FsError> {
        // A validated backend refuses any root it did not check: its durability claim is about one
        // exact directory, and honouring a different path would extend the claim to a filesystem
        // nobody inspected. The check runs BEFORE the open, so no descriptor is taken for a path
        // this backend will not serve.
        self.guard_root_path(abs)?;
        // Open only, never create: `open` on a missing database must refuse, not initialize it.
        let meta = fs::metadata(abs).map_err(|e| io_error(&e, Effect::NoEffect))?;
        if !meta.is_dir() {
            return Err(FsError::new(FsErrorKind::NotADirectory, Effect::NoEffect));
        }
        let handle = cap_std::fs::Dir::open_ambient_dir(abs, cap_std::ambient_authority())
            .map_err(|e| io_error(&e, Effect::NoEffect))?;
        // The path check says "the right name"; this says "the inode you got is on the validated
        // mount". A bind mount or a remount between the two makes them disagree.
        self.guard_descriptor(&handle)?;
        // Acquire the syncable same-inode descriptor HERE, not in `sync_dir`: the flush runs inside
        // the commit interval, and formatting `/proc/self/fd/<fd>` there would allocate after
        // `CURRENT` was replaced. Failure is a real refusal (`NoEffect`: nothing was written).
        let sync_file = syncable_dir_file(&handle)?;
        Ok(OsDir {
            handle,
            path: abs.to_path_buf(),
            family: None,
            sync_file,
        })
    }

    fn create_root(&self, abs: &Path) -> Result<Self::Dir, FsError> {
        // Order matters: the parent was pinned at construction and is re-verified here BEFORE any
        // write, and the child is created through that descriptor. `fs::create_dir(abs)` would
        // re-walk the path after the check, so a replaced or remounted parent could receive the new
        // directory and the caller would own a root this backend never validated.
        let pinned = self.pinned_parent(abs)?;
        let name = abs
            .file_name()
            .ok_or_else(|| FsError::no_effect(FsErrorKind::NotADirectory))?;
        let parent = match pinned {
            Some(parent) => parent
                .handle
                .try_clone()
                .map_err(|e| io_error(&e, Effect::NoEffect))?,
            None => {
                // Unvalidated backend: no claim, so keep the previous behaviour — but still resolve
                // the parent once and create through the descriptor, never through the full path.
                let parent_path = abs
                    .parent()
                    .ok_or_else(|| FsError::no_effect(FsErrorKind::NotADirectory))?;
                cap_std::fs::Dir::open_ambient_dir(parent_path, cap_std::ambient_authority())
                    .map_err(|e| io_error(&e, Effect::NoEffect))?
            }
        };

        match parent.create_dir(name) {
            Ok(()) => {}
            // Nothing was created by this call: an existing name is the caller's answer, and every
            // definite refusal (permissions, no space, wrong type) left no entry behind.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(io_error(&e, Effect::NoEffect));
            }
            Err(e) if is_definite_refusal(&e) => {
                return Err(io_error(&e, Effect::NoEffect));
            }
            // The entry may exist even though the call failed, so the caller must resolve rather
            // than assume "nothing happened" and retry into an `AlreadyExists` it caused itself.
            Err(e) => return Err(io_error(&e, Effect::Unknown)),
        }

        // Open the child through the same descriptor: resolving `abs` again could follow a
        // directory swapped in between, and the caller would then own a root it did not create.
        let handle = parent
            .open_dir(name)
            .map_err(|e| io_error(&e, Effect::Entailed))?;
        self.guard_descriptor(&handle)?;
        // The directory now EXISTS. If the syncable descriptor cannot be acquired, the caller must
        // still learn that something took effect — `Entailed`, never `NoEffect` — so a retry does
        // not assume it is starting from nothing.
        let sync_file = syncable_dir_file(&handle)
            .map_err(|error| FsError::new(error.kind(), Effect::Entailed))?;
        Ok(OsDir {
            handle,
            path: abs.to_path_buf(),
            family: None,
            sync_file,
        })
    }

    fn sync_parent_of(&self, abs: &Path) -> Result<(), FsError> {
        match self.pinned_parent(abs)? {
            // Flush through the descriptor acquired when the parent was pinned. Nothing here
            // allocates or reopens: the syncable same-inode file already exists, so this call is
            // safe inside the commit interval after `CURRENT` is replaced. The path is never
            // re-resolved, so a rename or same-mount replacement cannot redirect the flush.
            Some(parent) => flush_held(&parent.handle, &parent.sync_file),
            // Unvalidated backend: previous behaviour, still draining the path once, with no claim
            // attached to it.
            None => {
                let parent = abs
                    .parent()
                    .ok_or_else(|| FsError::new(FsErrorKind::NotADirectory, Effect::NoEffect))?;
                let dir = File::open(parent).map_err(|e| io_error(&e, Effect::NoEffect))?;
                // A failed flush may have completed: the caller resolves rather than assumes.
                dir.sync_all().map_err(|e| io_error(&e, Effect::Unknown))
            }
        }
    }

    fn open_family(
        &self,
        root: &Self::Dir,
        family: Family,
        create: CreateDir,
    ) -> Result<Self::Dir, FsError> {
        let name = family.as_str();
        // Every step is descriptor-relative: creation, the no-follow check, the type check and the
        // open. Using `fs::*` on a joined path here would re-resolve the root by name, so an
        // external rename of the KB could redirect the family lookup.
        if matches!(create, CreateDir::IfMissing) {
            match root.handle.create_dir(name) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                // A definite refusal created nothing; an unclassifiable one may have. Reporting
                // `NoEffect` for a failure that could have left an entry is the mistake this
                // file's own contract forbids.
                Err(e) if is_definite_refusal(&e) => {
                    return Err(io_error(&e, Effect::NoEffect));
                }
                Err(e) => return Err(io_error(&e, Effect::Unknown)),
            }
        }
        // Refuse a symlinked family directory: `metadata` follows links, so a `blobs/` pointing
        // outside the KB would look like an ordinary directory. The accepted contract (§4.8)
        // requires NOFOLLOW here.
        let link_meta = root
            .handle
            .symlink_metadata(name)
            .map_err(|e| io_error(&e, Effect::NoEffect))?;
        if link_meta.file_type().is_symlink() {
            return Err(FsError::new(FsErrorKind::Symlink, Effect::NoEffect));
        }
        if !link_meta.is_dir() {
            return Err(FsError::new(FsErrorKind::NotADirectory, Effect::NoEffect));
        }
        let handle = root
            .handle
            .open_dir(name)
            .map_err(|e| io_error(&e, Effect::NoEffect))?;
        let path = root.path.join(name);
        // Same acquire-once rule as the root: the flush must not allocate in the commit interval.
        let sync_file = syncable_dir_file(&handle)?;
        Ok(OsDir {
            handle,
            path,
            family: Some(family),
            sync_file,
        })
    }

    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), FsError> {
        // Flush the descriptor acquired with the directory, never `dir.path`: re-opening the path
        // would flush whatever directory now occupies it, and re-acquiring would allocate after
        // `CURRENT` was replaced.
        flush_held(&dir.handle, &dir.sync_file)
    }

    fn device_id(&self, dir: &Self::Dir) -> Result<u64, FsError> {
        // Ask the descriptor about itself; no path is re-resolved.
        use cap_std::fs::MetadataExt as _;
        let meta = dir
            .handle
            .dir_metadata()
            .map_err(|e| io_error(&e, Effect::NoEffect))?;
        Ok(meta.dev())
    }

    fn try_lock_exclusive(&self, root: &Self::Dir) -> Result<Self::Lock, FsError> {
        // Relative to the pinned root descriptor: the LOCK inode is the one in the KB we opened.
        let mut opts = cap_std::fs::OpenOptions::new();
        opts.create(true).read(true).write(true);
        let file = root
            .handle
            .open_with("LOCK", &opts)
            .map_err(|e| io_error(&e, Effect::NoEffect))?
            .into_std();
        match file.try_lock() {
            Ok(()) => Ok(OsLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err(FsError::new(FsErrorKind::WouldBlock, Effect::NoEffect))
            }
            Err(std::fs::TryLockError::Error(e)) => Err(io_error(&e, Effect::NoEffect)),
        }
    }

    fn create_new(&self, dir: &Self::Dir, name: Name) -> Result<Self::WriteFile, FsError> {
        // The recorded path is retained only so diagnostics can name the file; every actual
        // operation goes through the descriptor below.
        let path = dir.path.join(name.to_file_name());
        // Resolve through the pinned descriptor: `create_new` refuses an existing name, and the
        // resolution cannot be redirected by renaming the directory.
        let mut opts = cap_std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        let file = dir
            .handle
            .open_with(name.to_file_name(), &opts)
            .map_err(|e| io_error(&e, Effect::NoEffect))?;
        Ok(OsWriteFile {
            file: file.into_std(),
            path,
        })
    }

    fn write(&self, file: &mut Self::WriteFile, buf: &[u8]) -> Result<WriteOutcome, FsError> {
        match (&file.file).write(buf) {
            Ok(written) => Ok(WriteOutcome { written }),
            Err(e) => {
                // A partial prefix may have been accepted, so this is ambiguous, not no-effect.
                Err(io_error(&e, Effect::Unknown))
            }
        }
    }

    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), FsError> {
        file.file
            .sync_all()
            .map_err(|e| io_error(&e, Effect::Unknown))
    }

    fn sync_entry(&self, dir: &Self::Dir, name: Name) -> Result<(), FsError> {
        // Sync an existing file's bytes without opening it for writing. Same guards as every read.
        let file = Self::open_regular(dir, name)?;
        file.sync_all().map_err(|e| io_error(&e, Effect::Unknown))
    }

    fn publish_no_overwrite(
        &self,
        staged_dir: &Self::Dir,
        staged: Name,
        dst_dir: &Self::Dir,
        dst: Name,
        expected: &HashReport,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Published, FsError> {
        let staged_name = staged.to_file_name();
        let dst_name = dst.to_file_name();

        // Verify the staged bytes before publishing: a publish is a claim about content. The read is
        // charged, because verification is real work and must be bounded by the same ledger, and it
        // is resolved through the pinned descriptor with the shared no-follow/regular-file guards.
        ledger.checkpoint().map_err(FsError::from)?;
        let staged_file = Self::open_regular(staged_dir, staged)?;
        let actual = hash_streaming(staged_file, expected.len, expected.len, ledger)?;
        if actual.len != expected.len || actual.sha256 != expected.sha256 {
            return Err(FsError::new(FsErrorKind::HashMismatch, Effect::NoEffect));
        }

        // `hard_link` between the two pinned descriptors: neither side is re-resolved by path.
        match staged_dir
            .handle
            .hard_link(&*staged_name, &dst_dir.handle, &*dst_name)
        {
            Ok(()) => {
                // The destination name now exists. If cleanup of the staged name fails, the
                // The NAME PUBLICATION took effect: the bytes are reachable under `dst`, so the
                // error's effect is `Entailed` (or `Unknown` if unclassifiable), never `NoEffect` —
                // reporting "nothing happened" would let a caller retry a publish that already
                // exists. This is a filesystem-effect claim only. `dst` is an orphan until (and
                // unless) a manifest chain and `CURRENT` reference it; CURRENT alone commits.
                Self::consume_staged(staged_dir, staged)?;
                Ok(Published {
                    how: PublishMode::Created,
                    needs_sync: true,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Identity, not durability: the existing name must hold the same bytes.
                let dst_file = Self::open_regular(dst_dir, dst)?;
                let existing = hash_streaming(dst_file, expected.len, expected.len, ledger)?;
                if existing.len != expected.len || existing.sha256 != expected.sha256 {
                    return Err(FsError::new(FsErrorKind::HashMismatch, Effect::NoEffect));
                }
                Self::consume_staged(staged_dir, staged)?;
                Ok(Published {
                    how: PublishMode::AlreadyPresent,
                    needs_sync: true,
                })
            }
            // The link may have been created before the error was reported.
            Err(e) => Err(io_error(&e, Effect::Unknown)),
        }
    }

    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: Name,
        root: &Self::Dir,
    ) -> Result<(), FsError> {
        let staged_name = staged.to_file_name();
        let current = crate::names::RootName::Current.as_str();
        // The only overwrite in the seam, destination fixed by construction. `rename` between two
        // pinned descriptors is the same-filesystem atomic replace the contract needs, and neither
        // side is re-resolved through a stored path.
        staged_dir
            .handle
            .rename(&*staged_name, &root.handle, current)
            .map_err(|e| io_error(&e, Effect::Unknown))
    }

    fn size(&self, dir: &Self::Dir, name: Name) -> Result<u64, FsError> {
        // `symlink_metadata` on the descriptor: neither follows a link nor re-resolves a path.
        let meta = dir
            .handle
            .symlink_metadata(name.to_file_name())
            .map_err(|e| io_error(&e, Effect::NoEffect))?;
        if meta.file_type().is_symlink() {
            return Err(FsError::new(FsErrorKind::Symlink, Effect::NoEffect));
        }
        if !meta.is_file() {
            return Err(FsError::new(FsErrorKind::NotRegular, Effect::NoEffect));
        }
        Ok(meta.len())
    }

    fn read_exact_into(
        &self,
        dir: &Self::Dir,
        name: Name,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        // Descriptor-relative with the same guards as `open_read`: this is a sibling read path, and
        // the parent's probe proved a symlinked CURRENT was still readable here while `open_read`
        // correctly refused it.
        let mut file = Self::open_regular(dir, name)?;
        let (buf_len, file_len) = (
            buf.len() as u64,
            file.metadata()
                .map(|m| m.len())
                .map_err(|e| io_error(&e, Effect::NoEffect))?,
        );
        if file_len > buf_len {
            // The caller's buffer is the bound; a larger file is refused, never partially returned.
            return Err(FsError::new(FsErrorKind::TooLarge, Effect::NoEffect));
        }
        let mut read_total = 0usize;
        loop {
            match file.read(&mut buf[read_total..]) {
                Ok(0) => break,
                Ok(n) => {
                    read_total += n;
                    if read_total as u64 >= file_len {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error(&e, Effect::NoEffect)),
            }
        }
        Ok(read_total)
    }

    fn open_read(&self, dir: &Self::Dir, name: Name) -> Result<Self::ReadFile, FsError> {
        // Same helper as `read_exact_into` and `hash_bounded`: one guard cannot drift from another.
        let file = Self::open_regular(dir, name)?;
        Ok(OsReadFile { file })
    }

    fn read(&self, file: &mut Self::ReadFile, buf: &mut [u8]) -> Result<usize, FsError> {
        file.file
            .read(buf)
            .map_err(|e| io_error(&e, Effect::NoEffect))
    }

    fn hash_bounded(
        &self,
        dir: &Self::Dir,
        name: Name,
        max_len: u64,
        ledger: &mut dyn SharedLedger,
    ) -> Result<HashReport, FsError> {
        // No checkpoint here: `StreamingSha256::with_prepaid` charges first and then checkpoints, so
        // a refused admission short-circuits before ANY control point or read. An extra checkpoint
        // at this level would spend a control point on a request that was never admitted.
        // Descriptor-relative with the shared no-follow / regular-file guards.
        let file = Self::open_regular(dir, name)?;
        let stated = file
            .metadata()
            .map(|m| m.len())
            .map_err(|e| io_error(&e, Effect::NoEffect))?;
        if stated > max_len {
            // The ceiling is refused before the first read.
            return Err(FsError::new(FsErrorKind::TooLarge, Effect::NoEffect));
        }
        hash_streaming(file, stated, max_len, ledger)
    }

    fn for_each_entry(
        &self,
        dir: &Self::Dir,
        ledger: &mut dyn SharedLedger,
        visit: &mut dyn FnMut(&str) -> Each,
    ) -> Result<(), FsError> {
        // Enumerate the pinned descriptor's entries, not a stored path's current contents.
        let entries = dir
            .handle
            .entries()
            .map_err(|e| io_error(&e, Effect::NoEffect))?;
        // A walk that charged nothing must not report a clean success: an empty directory is a
        // distinct answer from "I looked and found nothing", and a cancellation before the first
        // entry must surface as a stop rather than as success. One checkpoint up front makes that
        // distinction observable.
        ledger.checkpoint().map_err(FsError::from)?;
        let mut visited = 0usize;
        for entry in entries {
            let entry = entry.map_err(|e| io_error(&e, Effect::NoEffect))?;
            ledger.try_charge_entries(1).map_err(FsError::from)?;
            ledger.checkpoint().map_err(FsError::from)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                // A non-UTF-8 name cannot be reported as a string. It is still a present entry, so
                // emptiness checks must see it; report a placeholder rather than skip it.
                if matches!(visit("<?>"), Each::Stop) {
                    return Ok(());
                }
                continue;
            };
            visited += 1;
            if matches!(visit(name), Each::Stop) {
                return Ok(());
            }
        }
        // A zero-entry walk stays `Ok` — an empty staging directory is a legitimate state after
        // cleanup. What must NOT happen is returning `Ok` without ever offering a control point:
        // that is how a cancelled request is reported as success. The `checkpoint()` above the loop
        // guarantees at least one observation, and one more here covers the exit.
        ledger.checkpoint().map_err(FsError::from)?;
        let _ = visited;
        Ok(())
    }

    fn remove(&self, dir: &Self::Dir, name: Name) -> Result<(), FsError> {
        match dir.handle.remove_file(name.to_file_name()) {
            Ok(()) => Ok(()),
            // Removing an absent name satisfies the postcondition: staging cleanup is repeatable.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(&e, Effect::Unknown)),
        }
    }
}

/// Hash a file's bytes, bounded by the caller's ceiling.
///
/// ## Allocation discipline
///
/// The read is admitted against the ledger **before** any buffer is acquired: `hash_bounded`
/// charges the file's length, then this reads into a `Vec` of exactly that length. That satisfies
/// "admit before acquiring" and is genuinely bounded — the ceiling is the caller's `max_len`, not
/// an engine constant — so a refusal costs no allocation and no I/O.
///
/// `[INFERENCE]` A streaming implementation would avoid materialising the file at all, which matters
/// for large blobs. The format crate currently exposes only whole-slice hashing (`raw_sha256`,
/// `raw_sha256_bounded`), and adding `sha2` as a direct store dependency would widen the store's
/// dependency set beyond `{types, format}` — which t61's gate is about to assert. So the tradeoff is
/// stated rather than hidden: this buffers up to `max_len`, charged; a streaming `StreamingHasher`
/// export from format is requested as a follow-up. Nothing here is unbounded.
///
/// # Errors
///
/// `TooLarge` when the file exceeds `expected_len`; read failures propagate.
fn hash_streaming(
    mut file: File,
    stated_len: u64,
    max_len: u64,
    ledger: &mut dyn SharedLedger,
) -> Result<HashReport, FsError> {
    // ORDER MATTERS: charge -> read -> feed. `with_prepaid` admits the whole length and checks
    // control at construction, BEFORE the first `read`. The previous version read a 64 KiB chunk and
    // only then called `update`, which is where the charge happened — so a zero-budget operation
    // read data before refusing. A checkpoint alone fixed cancellation, not budget-before-I/O.
    //
    // `update` on a prepaid stream does NOT charge again: it decrements the prepaid remainder and
    // refuses to overdraw. So the bytes are counted exactly once.
    //
    // ONE BOUNDED EXCEPTION, stated rather than glossed: after the admitted range is exhausted, this
    // reads at most **one** further byte to detect growth (below). That byte is a diagnostic probe —
    // it is never fed to the hasher, never included in the reported length, and never charged. So
    // the invariant is "every HASHED byte was admitted, and total == admitted at the end", not
    // "every byte read from the file was admitted". The probe costs at most 1 byte of I/O and cannot
    // be used to read content.
    let admitted = stated_len.min(max_len);
    let mut hasher =
        sage_db_format::StreamingSha256::with_prepaid(ledger, admitted).map_err(FsError::from)?;

    // Read at most what was admitted. When the admitted range is exhausted, a single probe byte
    // distinguishes "file ended exactly here" (Ok(0) -> done) from "file grew" (Ok(1) -> refuse).
    // The probe byte is read but never hashed and never charged; it is the one bounded diagnostic
    // read that is not part of the admitted-and-hashed byte count.
    let mut remaining = admitted;
    let mut total = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        if remaining == 0 {
            let mut probe = [0u8; 1];
            match file.read(&mut probe) {
                Ok(0) => break,
                Ok(_) => {
                    // The file grew past what was admitted and charged: refuse rather than read
                    // unadmitted data.
                    return Err(FsError::new(FsErrorKind::TooLarge, Effect::NoEffect));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_error(&e, Effect::NoEffect)),
            }
        }
        let take = (remaining.min(buf.len() as u64)) as usize;
        match file.read(&mut buf[..take]) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                remaining -= n as u64;
                hasher.update(&buf[..n]).map_err(FsError::from)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_error(&e, Effect::NoEffect)),
        }
    }

    // A short read means the declared length and the bytes present disagreed. That is a size
    // mismatch, not a successful hash of a smaller file.
    if total != admitted {
        return Err(FsError::new(FsErrorKind::TooLarge, Effect::NoEffect));
    }
    let sha256 = hasher.finish().map_err(FsError::from)?;
    Ok(HashReport { len: total, sha256 })
}

#[cfg(test)]
mod policy_tests {
    //! Parser and matcher tests for the platform policy.
    //!
    //! These live in-crate because the helpers are private on purpose: publishing a parser whose
    //! only consumer is a test would invite a second caller to build policy decisions from text.
    //! The host-facing assertions live in `tests/os_linux_qualification.rs`.
    //!
    //! The synthetic table uses the real `/proc/self/mountinfo` shape, including the cases that
    //! break naive parsers: escaped spaces and backslashes, a nested mount that must win over its
    //! parent, a sibling whose name is a string prefix of another, and a `nobarrier` token.
    use super::*;

    #[test]
    fn kernel_rollforward_adds_only_the_exact_qualified_release() {
        assert!(kernel_release_allowed("7.2.11-arch1-1"));
        assert!(kernel_release_allowed("7.3.5-arch1-1"));
        for other in [
            "7.3.5",
            "7.3.5-arch1-2",
            "7.3.5-arch1-1-local",
            "7.3.6-arch1-1",
        ] {
            assert!(
                !kernel_release_allowed(other),
                "{other} was never qualified"
            );
        }
    }

    const TABLE: &str = concat!(
        "42 1 0:33 /@ / rw,noatime shared:1 - btrfs /dev/nvme0n1p2 rw,compress=zstd:3\n",
        "43 42 0:34 / /srv rw,relatime shared:2 - ext4 /dev/sda1 rw\n",
        "44 42 0:35 / /srv/kb rw,relatime shared:3 - btrfs /dev/sda2 rw\n",
        "45 42 0:36 / /srv/kb2 rw,relatime shared:4 - overlay overlay rw,lowerdir=/a\n",
        "46 42 0:37 / /var/tmp nosuid,nodev shared:5 - btrfs /dev/nvme0n1p2 rw,nobarrier\n",
        "47 42 0:38 / /mnt/space\\040here rw shared:6 - tmpfs tmpfs rw\n",
        "48 42 0:39 / /mnt/back\\134slash rw shared:7 - tmpfs tmpfs rw\n",
    );

    #[test]
    fn every_record_parses_and_escapes_decode() {
        let records = parse_mount_table(TABLE).expect("a well-formed table parses");
        assert_eq!(records.len(), 7, "every record, not just the first");
        let escaped = records
            .iter()
            .find(|record| record.mount_point.starts_with("/mnt/space"))
            .expect("the escaped record is present");
        assert_eq!(
            escaped.mount_point, "/mnt/space here",
            "\\040 decodes to a space"
        );
        let backslash = records
            .iter()
            .find(|record| record.mount_point.starts_with("/mnt/back"))
            .expect("the double-escaped record is present");
        assert_eq!(
            backslash.mount_point, "/mnt/back\\slash",
            "\\\\ decodes to one backslash"
        );
        assert_eq!(records[0].mount_id, 42);
        assert_eq!(records[0].device, "0:33");
        assert_eq!(records[0].root, "/@");
        assert_eq!(records[0].filesystem, "btrfs");
    }

    #[test]
    fn a_malformed_record_makes_the_whole_table_unusable() {
        // Skipping a bad line could hide the mount that actually covers the root and let a
        // different record answer for it.
        for broken in [
            "",
            "42",
            "42 1 0:33 /@ / rw shared:1",
            "42 1 0:33 /@ / rw shared:1 - btrfs",
            "notanumber 1 0:33 /@ / rw shared:1 - btrfs /dev/x rw",
            "42 1 0:33 /@ / rw shared:1 - btrfs /dev/x", // no superblock options field
        ] {
            assert!(
                parse_mount_table(broken).is_none(),
                "{broken:?} must not parse"
            );
        }
    }

    #[test]
    fn longest_component_prefix_wins_and_string_prefixes_do_not() {
        let table = parse_mount_table(TABLE).expect("parses");

        let nested = covering_mount(&table, Path::new("/srv/kb/x")).expect("covered");
        assert_eq!(nested.mount_point, "/srv/kb");
        assert_eq!(nested.filesystem, "btrfs");

        // String-prefix matching would answer `/srv/kb`; component matching answers `/srv/kb2`.
        let sibling = covering_mount(&table, Path::new("/srv/kb2/inner")).expect("covered");
        assert_eq!(sibling.mount_point, "/srv/kb2");
        assert_eq!(sibling.filesystem, "overlay");

        let root = covering_mount(&table, Path::new("/home/x")).expect("covered by /");
        assert_eq!(root.mount_point, "/");

        let exact = covering_mount(&table, Path::new("/srv/kb")).expect("covered");
        assert_eq!(exact.mount_point, "/srv/kb");
    }

    #[test]
    fn voiding_options_match_whole_tokens_in_both_option_regions() {
        let table = parse_mount_table(TABLE).expect("parses");
        let voided = table
            .iter()
            .find(|record| record.mount_point == "/var/tmp")
            .expect("the nobarrier mount is present");
        // `nobarrier` is a superblock option on Btrfs, so it must be found in that region and NOT
        // in the per-mount one. This is the regression for reading only field 6.
        assert!(
            has_mount_option(&voided.super_options, "nobarrier"),
            "the superblock region carries it: {:?}",
            voided.super_options
        );
        assert!(
            !has_mount_option(&voided.options, "nobarrier"),
            "field 6 must not be where it appears: {:?}",
            voided.options
        );
        assert!(
            !has_mount_option(&voided.super_options, "barrier"),
            "a substring of a present token is not a match"
        );
        assert!(
            !has_mount_option("nobarrier_safe,barrier=off", "nobarrier"),
            "a longer token that merely starts with the name is not a match"
        );
        assert!(!has_mount_option("", "nobarrier"));

        // A healthy record's superblock options do not match either.
        let healthy = table
            .iter()
            .find(|record| record.mount_point == "/")
            .expect("the root record is present");
        assert!(!has_mount_option(&healthy.super_options, "nobarrier"));
        assert!(has_mount_option(&healthy.super_options, "compress=zstd:3"));
    }

    /// The sync descriptor must refer to the inode that was pinned, not to whatever the recorded
    /// path names later.
    ///
    /// The scenario is a **same-mount replacement**: `a/` is pinned, then `a` is renamed away and
    /// a *different* directory is renamed into its place. The mount id is unchanged by design, so
    /// `mnt_id` alone cannot detect it; the inode can. This asserts both halves of the fix —
    /// `dir_identity` sees the difference, and the held descriptor still flushes the original
    /// inode (which is why the identity is checked before use rather than after).
    ///
    /// Runs on any host with a writable temp dir; the Linux-specific `syncable_dir_file` path is
    /// exercised by the same code on Linux, and the assertion is about inode identity either way.
    #[test]
    fn the_pinned_inode_is_the_one_that_survives_a_same_mount_replacement() {
        let base = std::env::temp_dir().join(format!(
            "sage-db-inode-proof-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).expect("scratch base");
        let pinned_path = base.join("a");
        let replacement_path = base.join("b");
        std::fs::create_dir(&pinned_path).expect("pinned dir");
        std::fs::create_dir(&replacement_path).expect("replacement dir");

        let pinned = cap_std::fs::Dir::open_ambient_dir(&pinned_path, cap_std::ambient_authority())
            .expect("open pinned");
        let recorded = dir_identity(&pinned).expect("identity of the pinned directory");
        let held = syncable_dir_file(&pinned).expect("syncable descriptor");

        // Replace `a` with `b` WITHOUT touching the mount: rename the old one away, rename the new
        // one in. Same filesystem, same mount, different inode at the path.
        std::fs::rename(&pinned_path, base.join("a-old")).expect("move the pinned dir aside");
        std::fs::rename(&replacement_path, &pinned_path).expect("put another dir in its place");

        // Resolving the path now yields a DIFFERENT directory ...
        let by_path =
            cap_std::fs::Dir::open_ambient_dir(&pinned_path, cap_std::ambient_authority())
                .expect("open the replacement");
        let observed = dir_identity(&by_path).expect("identity of the replacement");
        assert_ne!(
            recorded, observed,
            "the replacement must be distinguishable: same mount, different inode"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            recorded.mount_id, observed.mount_id,
            "a same-mount replacement keeps the mount id, which is exactly why the inode is needed"
        );

        // ... while the descriptor acquired from the pinned directory still flushes THAT directory.
        // This is the property `sync_parent_of` relies on.
        flush_held(&pinned, &held)
            .expect("the held descriptor flushes the originally pinned inode");

        // And the recorded identity is what the guard compares against, so the replacement is a
        // refusal rather than a silent retarget.
        assert_ne!(recorded.inode, observed.inode);
        assert_eq!(recorded.device, observed.device);

        let _ = std::fs::remove_dir_all(&base);
    }
}
