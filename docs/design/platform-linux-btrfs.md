# Linux/Btrfs platform policy (checked, v1 candidate)

> Status: **candidate policy**, not an accepted phase. Owns the question "may this build open a
> product handle on this host?" for exactly one platform, and refuses everywhere else.
> Code: `sage-db/crates/store/src/os.rs` (`OsFileSystem::for_root`), tests:
> `crates/store/src/os.rs::policy_tests`, `crates/store/tests/os_linux_qualification.rs`.

## 1. What changed and why

`DOCUMENTED_DURABLE_PLATFORMS` is **still empty**, and that is deliberate. An `os/arch` key cannot
justify a durability claim: it says nothing about the mounted filesystem, the kernel release, the
mount options or the device behind a path. `from_platform_policy()` therefore keeps returning
`NotClaimed` on Linux just as on macOS, and `conservative()` still never claims anything.

A claim now has one source: `OsFileSystem::for_root(path)`, which checks the actual facts behind
that one path and binds the result. There is no public `bool`/`enum` constructor, no environment
variable and no caller-asserted override — the store must not be able to manufacture the property
it exists to withhold.

## 2. The accepted bound (exact)

| Fact | Accepted value | Where it comes from |
|---|---|---|
| Target | `linux` + `x86_64` | compile-time `cfg`, re-checked at runtime |
| Kernel release | prefix `7.2.11-arch1-` or exact `7.3.5-arch1-1` | `/proc/sys/kernel/osrelease`, read and parsed |
| Filesystem | `btrfs` (exact `fstype`) | longest component-prefix record in `/proc/self/mountinfo` |
| Mount identity | covering record's mount id == pinned descriptor's `mnt_id` | `/proc/self/fdinfo/<fd>` |
| Voiding mount options | `nobarrier` must be absent from **either** the per-mount or the superblock option region | same record |

Everything else is refused with its own reason: tmpfs, ext4, NFS, overlayfs, FUSE, another kernel,
a bind-mount mismatch, an unreadable `/proc`, a relative path. None of them is accepted by
resemblance, and none is refused as "corruption" — these are typed policy verdicts
(`PlatformPolicyError`), distinct from an I/O failure.

### Why the mount id matters

Checking `/proc/self/mountinfo` alone is a *textual* check: a bind mount, an overlay, or a remount
between resolving the path and opening it can make the record describe one filesystem while the
opened directory lives on another. The constructor opens the directory first, then requires the
descriptor's own `mnt_id` to equal the covering record's mount id. Two independent sources must
agree before a claim is made, and `open_root`/`create_root`/`sync_parent_of` re-check the mount id
on every descriptor they accept.

## 3. Why Btrfs on Linux satisfies the store's contract

The store asks for two things (`storage.md` §1): a file flush that puts the file's bytes and
metadata on the device, and a **directory** flush that puts the parent entry there. `fsync(2)`
documents exactly that split — "calling `fsync()` does not necessarily ensure that the entry in the
directory containing the file has also reached disk. For that an explicit `fsync()` on a file
descriptor for the directory is also needed"
(<https://man7.org/linux/man-pages/man2/fsync.2.html>). The seam already performs both halves
(`sync_file` / `sync_entry`, and `sync_dir` on the pinned descriptor).

What the *filesystem* does with them, per the Linux source:

1. `fs/btrfs/file.c` — `.fsync = btrfs_sync_file` for regular files
   (<https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/fs/btrfs/file.c>).
   It logs the inode, syncs the log, and on the full-commit path waits for ordered extents and
   commits the transaction.
2. `fs/btrfs/inode.c` — the **directory** file operations also use `btrfs_sync_file`
   (`.fsync = btrfs_sync_file` in `btrfs_dir_file_operations`, ~line 10687), so a directory fsync
   takes the same log/commit path rather than a no-op.
3. `fs/btrfs/tree-log.c` — `btrfs_sync_log` ends in `write_all_supers(trans)`, which makes the log
   root reachable through the superblock
   (<https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/fs/btrfs/tree-log.c>).
4. `fs/btrfs/disk-io.c` — `write_all_supers` calls `barrier_all_devices` when barriers are on,
   submitting a `REQ_OP_WRITE | REQ_SYNC | REQ_PREFLUSH` bio per device (`write_dev_flush`) and
   waiting for completion; then it writes the super blocks, with `REQ_FUA` on the first mirror
   (`if (i == 0 && !btrfs_test_opt(…, NOBARRIER)) bio->bi_opf |= REQ_FUA`)
   (<https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/fs/btrfs/disk-io.c>).
5. `fs/btrfs/super.c` — `nobarrier` sets `NOBARRIER`, which makes both of the above conditional
   clauses false. That is why the option voids the claim rather than being ignored: without it the
   fsync path has no device flush at all
   (<https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/fs/btrfs/super.c>).

So the chain "file fsync → log → superblock with a preceding device flush" and "directory fsync →
same path" is visible in the implementation the policy names, and the policy refuses the one mount
option that removes the device flush.

### What this still does not assert

Per the project's own evidence discipline:

* **A device flush is a request, not proof.** `REQ_PREFLUSH` asks the device to flush its cache; a
  lying or non-volatile write cache defeats it. The policy's assumption is "the device honours
  flush and FUA", which is exactly the assumption `storage.md` §1 already excludes from fsync's
  scope.
* **No power-loss evidence.** Nothing here was measured by cutting power. Process-kill and reopen
  show that committed state survives process death and that no partial transaction is visible;
  they say nothing about a device losing power mid-flush.
* **Named kernel releases on Btrfs, single device.** The original `7.2.11-arch1-*` bound and exact
  `7.3.5-arch1-1`, no
  RAID profile change, no `nobarrier`. A different kernel or a multi-device Btrfs profile is a
  different implementation of the same contract until revalidated.
* **No vendor promise is claimed.** There is no upstream statement of the form "Btrfs guarantees X
  across a power cut"; the kernel documentation describes features and torn-write detection, not
  fsync ordering (<https://docs.kernel.org/filesystems/btrfs.html>,
  <https://btrfs.readthedocs.io/en/latest/Hardware.html>). The claim rests on the implementation
  above plus the stated assumptions, and it is scoped to that.

### Arch Linux 7.3.5 source review

The separately named release uses the same relevant interfaces in Arch's
[`v7.3.5-arch1` source](https://github.com/archlinux/linux/tree/v7.3.5-arch1/fs/btrfs).
Regular-file and directory fsync use `btrfs_sync_file`. Log sync writes the superblocks,
and the barrier path issues `REQ_PREFLUSH` and uses `REQ_FUA` when barriers are enabled.
The constructor still checks the actual mount and rejects `nobarrier`. Qualification of
this exact build depends on the device honoring flushes and on the host test results.

## 4. Runtime binding

`for_root` accepts either the intended root (an existing directory) or its **direct existing
parent**, which is the initialization case: the root does not exist yet, so the parent is what can
be checked. Two things are pinned and enforced afterwards:

* the **root's direct parent**, in *both* cases — when the root already exists, `bound` is the root
  and the parent is a *different* directory that must never be mistaken for it;
* the **syncable descriptor** for every directory handle, acquired when the handle is opened.

| Operation | Guard |
|---|---|
| `open_root(path)` | path must equal the validated root, then the descriptor's mount id must match; the syncable descriptor is acquired here |
| `create_root(path)` | path must equal the validated root; `create_dir` goes through the **pinned parent descriptor**, never a re-walked absolute path. If the syncable descriptor cannot then be acquired, the error is `Effect::Entailed` — the directory exists |
| `sync_parent_of(path)` | only the validated root; the flush uses the parent's **held** descriptor, after re-verifying its recorded device/inode/mount identity |
| `sync_dir(dir)` | flushes the held descriptor; no path is resolved and nothing is allocated |
| `open_family` / child operations | descriptor-relative as before; the handle they descend from was already checked |

Ordering on the init path is: validate → pin parent (with its syncable descriptor) → `create_dir`
through the pinned descriptor → reopen through the same descriptor → `sync_parent_of`. A failed
creation that could have taken effect is reported `Effect::Unknown`, never `NoEffect`.

A refused path is `FsErrorKind::Unsupported` with `Effect::NoEffect` — no descriptor was taken, no
write was attempted. That is a policy verdict, not `Corrupt` and not `BudgetExceeded`. If the
constructor cannot pin the parent at all (for example the root *is* the filesystem root), it
refuses with `ParentUnavailable` rather than falling back to re-walking the path later.

### 4.1 Why the flush is descriptor-bound, and what identity means

`sync_parent_of` used to re-open the parent's absolute path and flush whatever it got. Between the
guard and that open, a rename or a remount can make the path name a directory the policy never
validated, and the resulting `fsync` would report durability for a *different* directory's entries.
It now flushes the descriptor acquired at construction.

Two levels of identity are needed, and only one is about mounts:

* the **mount id** (`/proc/self/fdinfo/<fd>`) catches a bind mount, overlay or remount;
* **(st_dev, st_ino)** identifies the held parent inode. The guard reads that same held descriptor,
  so it does not detect a sibling renamed over the descriptor's old pathname. Creation and flush
  keep using the original inode; they are not redirected to the replacement. The unit test compares
  a fresh path open against the held descriptor and demonstrates this binding, not path-substitution
  detection. `open_root` takes a fresh descriptor and checks its mount id only; no global promise
  that the original pathname still identifies the construction-time root inode is made.

The mount id is stored as an `Option` because macOS has no `/proc`; a validated backend can only
exist on Linux, where the value is required to construct the identity.

### 4.2 Why the syncable descriptor is acquired once

`sync_dir` and `sync_parent_of` run inside the commit interval, after `CURRENT` is replaced.
Acquiring the descriptor there would mean formatting `/proc/self/fd/<fd>` — a fresh `String`
allocation and a reopen — after the commit root moved. Each directory handle therefore carries the
descriptor from open time, and the flush is a single `sync_all` on it.

This is an ordering/latency property of the flush path. It is **not** a power-loss claim, and not
an argument that the pre-fix behaviour would have been safe under one.

### 4.3 Correction to an earlier narrative

An earlier delivery described the pre-fix Linux `sync_dir` as having flushed "silently" nothing.
That was wrong in one word: `try_clone().into_std_file().sync_all()` returned a **visible typed
error** (`EBADF`, `Io(9)`), not a successful durability acknowledgement. The pre-fix Linux backend
could not open a product handle at all, so no accepted Linux phase was in effect and no ACK was
ever issued on that path. What was true is that the call could not succeed, so that call never
flushed a directory entry — a broken primitive, not a silent ACK loss.

## 5. Evidence in this tree

| What | Command | Result |
|---|---|---|
| Parser/matcher + pinned-inode units | `cargo test -p sage-db-store --lib` | 48 passed |
| Existing backend behaviour unchanged | `cargo test -p sage-db-store --test os_backend` | 34 passed |
| Constructor contract + host false-positive check | `cargo test -p sage-db-store --test os_linux_qualification` | 6 passed, 6 ignored (qualifying-host) |
| Real backend on the target filesystem | `cargo test -p sage-db-store --test os_linux_qualification -- --ignored` **on x99** | see `PLATFORM-R2-DELIVERY.md` |

The `#[ignore]`d tests assert `/var/tmp` and `/opt/xartifact/x-sage/data/kb` share one `st_dev`
before they run, so "the fixture is on the target device" is checked rather than assumed. On a host
outside the bound they **fail**, they do not skip: asking for qualification evidence on a
non-qualifying host is a failed request.

## 6. Deliberate non-goals

* No `tmpfs`/`ext4`/`xfs`/`NFS`/`overlayfs`/`FUSE` support, and no "probably fine" fallback.
* No macOS/APFS claim: `F_FULLFSYNC` is a file barrier, and directory-entry durability is not
  documented. APFS stays `NotClaimed`.
* No mount-option parser beyond what changes the flush path; `noatime`, `compress=`, `ssd`,
  `discard=async` are recorded by the platform but do not affect the claim.
* No power-loss, RSS, or all-input allocation claim. This document is about *one* eligibility
  decision, not about the commit protocol's resource bounds.
