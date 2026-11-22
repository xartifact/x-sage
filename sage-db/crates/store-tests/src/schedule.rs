//! Operation and fault schedules as **plain data**.
//!
//! A schedule is a list of operations plus the crash and failure points the harness will inject.
//! Nothing here calls a store: the point of this module is to let a scenario be written down,
//! reviewed and diffed before any commit API exists, so that when P1-4/P1-5 are released the same
//! scenario runs unchanged against the real store instead of being invented at that moment.
//!
//! Two rules shape the design:
//!
//! * **A failure point is named, not statistical.** `Crash` and `Fail` carry the operation index
//!   they interrupt, so every scenario produces a fixed, enumerable set of outcomes rather than
//!   "run it a thousand times and hope".
//! * **The schedule is the input, not the observation.** What the store did is recorded separately
//!   (see [`super::oracle::Observation`]); a schedule must never be able to claim an outcome on the
//!   store's behalf.
//!
//! # What these schedules are not (scope limits, stated so they cannot be mistaken later)
//!
//! 1. **Executing an [`Op`] list against a `FileSystem` is a model/seam test, not evidence of
//!    Store commit correctness.** It shows the seam behaves as specified under a fault; it says
//!    nothing about whether the commit protocol built on top of it is correct. Only the real
//!    `Store` API can answer that, and only once P1-4/P1-5 are released.
//! 2. **A schedule supplies fault points and observation constraints; it is never a commit
//!    routine.** There must be no handwritten driver that performs the protocol and then asserts
//!    its own ordering — such a driver's happy order makes itself pass. For acceptance, the real
//!    store is invoked and the faults are injected underneath it.
//! 3. **Expected outcomes come from the external acknowledgement/log observation**, never from
//!    production fold or replay. Comparing an actual filesystem trace against the ordering below is
//!    useful diagnostics; it is not the oracle.
//! 4. **This catalogue covers the core known protocol only.** It is not a coverage claim: new
//!    cases must be added as candidate record types and protocol steps stabilise, and "the
//!    scenarios still run" must never be read as "the protocol is fully covered".

use std::collections::BTreeSet;

use super::model::RelPath;
use sage_db_store::names::{EntryName, Id128, RootName, StagingKind};

/// One filesystem-level operation a commit protocol performs.
///
/// These are deliberately the operations the accepted seam exposes, in the order the accepted
/// commit protocol uses them, so a scenario reads like the protocol step list rather than like an
/// implementation detail.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Op {
    /// Create a directory (init only).
    CreateDir {
        /// Parent directory.
        parent: RelPath,
        /// New directory name.
        name: String,
    },
    /// Open a declared family directory, optionally creating it.
    OpenFamily {
        /// Family directory name, e.g. `manifests`.
        family: String,
        /// Whether a missing directory may be created.
        create: bool,
    },
    /// Create a new file that must not exist.
    CreateNew {
        /// Directory the file lives in.
        dir: RelPath,
        /// Engine name of the file.
        name: EntryName,
    },
    /// Append bytes to the current writable file.
    Write {
        /// Bytes to append.
        bytes: Vec<u8>,
    },
    /// Persist the current writable file's content.
    SyncFile,
    /// Persist an already-named file's content (reuse and resolution paths).
    SyncEntry {
        /// Directory the file lives in.
        dir: RelPath,
        /// Engine name of the file.
        name: EntryName,
    },
    /// Persist a directory's entries.
    SyncDir {
        /// Directory to sync.
        dir: RelPath,
    },
    /// Persist the directory that contains a root path, making the root's own binding durable.
    SyncParentOfRoot,
    /// Publish staged bytes under a destination name without replacing different bytes.
    PublishNoOverwrite {
        /// Directory holding the staged file.
        staging: RelPath,
        /// Staged engine name.
        staged: EntryName,
        /// Destination directory.
        dst_dir: RelPath,
        /// Destination engine name.
        dst: EntryName,
    },
    /// Atomically replace the commit root pointer.
    ReplaceCurrent {
        /// Directory holding the staged root file.
        staging: RelPath,
        /// Staged engine name.
        staged: EntryName,
    },
    /// Enumerate a directory's entries.
    ForEachEntry {
        /// Directory to enumerate.
        dir: RelPath,
    },
    /// Remove a name.
    Remove {
        /// Directory holding the name.
        dir: RelPath,
        /// Engine name to remove.
        name: EntryName,
    },
}

/// A failure injected at one point in the schedule.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fault {
    /// The backend refuses the operation (typed error, no effect).
    Fail {
        /// Index into [`Schedule::ops`] of the operation that fails.
        at: usize,
        /// Which refusal, kept as a name so schedules stay comparable.
        kind: FaultKind,
    },
    /// The operation is reported as having failed with an ambiguous effect.
    ///
    /// This is the case a store must *resolve* rather than assume: on-disk state may or may not
    /// include the operation, so the recovery outcome is genuinely unknown at the call site.
    Ambiguous {
        /// Index into [`Schedule::ops`].
        at: usize,
    },
    /// The process stops after the operation at `at` completes.
    ///
    /// Modelled as a clean stop of the driver: whatever the backend had made durable persists, and
    /// anything not synced may be lost according to [`super::model`].
    CrashAfter {
        /// Index into [`Schedule::ops`].
        at: usize,
    },
    /// Cancellation is requested while the operation at `at` runs.
    Cancel {
        /// Index into [`Schedule::ops`].
        at: usize,
    },
    /// A budget refusal with the given unit and amounts.
    Budget {
        /// Index into [`Schedule::ops`].
        at: usize,
        /// Whether the refusal was measured in bytes or entries.
        unit: BudgetUnitName,
        /// Amount requested.
        requested: u64,
        /// Amount remaining.
        remaining: u64,
    },
}

/// The unit a budget refusal was measured in, as schedule data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BudgetUnitName {
    /// Bytes.
    Bytes,
    /// Directory entries.
    Entries,
}

/// The refusal class a [`Fault::Fail`] names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FaultKind {
    /// No such entry.
    NotFound,
    /// The name already exists.
    AlreadyExists,
    /// The entry is a symbolic link where a real one is required.
    Symlink,
    /// The entry exists but is not a regular file.
    NotRegular,
    /// Out of space.
    NoSpace,
    /// A digest did not match the bytes.
    HashMismatch,
}

/// A named scenario: operations plus planned faults.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Schedule {
    /// Human-readable name, used in assertion messages.
    pub name: String,
    /// The operations in execution order.
    pub ops: Vec<Op>,
    /// Planned failures and crashes.
    pub faults: Vec<Fault>,
}

impl Schedule {
    /// Start a named schedule with no operations.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ops: Vec::new(),
            faults: Vec::new(),
        }
    }

    /// Append an operation.
    #[must_use]
    pub fn op(mut self, op: Op) -> Self {
        self.ops.push(op);
        self
    }

    /// Append a fault.
    ///
    /// # Panics
    ///
    /// Panics when the fault names an operation index that does not exist. A fault pointing past
    /// the end would silently never fire, which is the failure mode that makes a fault matrix look
    /// complete while testing nothing.
    #[must_use]
    pub fn fault(mut self, fault: Fault) -> Self {
        let at = match &fault {
            Fault::Fail { at, .. }
            | Fault::Ambiguous { at }
            | Fault::CrashAfter { at }
            | Fault::Cancel { at }
            | Fault::Budget { at, .. } => *at,
        };
        assert!(
            at < self.ops.len(),
            "fault targets op {at} but the schedule has only {} operations",
            self.ops.len()
        );
        self.faults.push(fault);
        self
    }

    /// Every crash point the schedule declares, as an ordered set.
    #[must_use]
    pub fn crash_points(&self) -> BTreeSet<usize> {
        self.faults
            .iter()
            .filter_map(|f| match f {
                Fault::CrashAfter { at } => Some(*at),
                _ => None,
            })
            .collect()
    }

    /// Whether any fault makes the outcome ambiguous at the call site.
    #[must_use]
    pub fn has_ambiguity(&self) -> bool {
        self.faults
            .iter()
            .any(|f| matches!(f, Fault::Ambiguous { .. }))
    }

    /// Operations that must be durable before an acknowledgement, as a readable summary.
    ///
    /// This is *not* the protocol's own claim; it is the set of sync-shaped operations in the
    /// schedule, so a test can assert that a schedule which would ack a commit actually contains a
    /// root sync. A schedule that publishes `CURRENT` without syncing its directory is a schedule
    /// bug, and this makes it visible before any store exists to blame.
    #[must_use]
    pub fn sync_ops(&self) -> Vec<usize> {
        self.ops
            .iter()
            .enumerate()
            .filter(|(_, op)| {
                matches!(
                    op,
                    Op::SyncFile | Op::SyncEntry { .. } | Op::SyncDir { .. } | Op::SyncParentOfRoot
                )
            })
            .map(|(i, _)| i)
            .collect()
    }
}

/// A short catalogue of schedules the harness will run once the commit API exists.
///
/// Kept here rather than in tests so they can be reviewed as scenarios: each one encodes a
/// correctness argument, and the ordering of syncs is the argument.
#[must_use]
pub fn catalogue(staging: &RelPath, manifests: &RelPath, blobs: &RelPath) -> Vec<Schedule> {
    let tx = EntryName::Staging {
        kind: StagingKind::Tx,
        id: Id128::from_bytes([0x03; 16]),
    };
    let man = EntryName::Staging {
        kind: StagingKind::Manifest,
        id: Id128::from_bytes([0x03; 16]),
    };
    let blob = EntryName::Staging {
        kind: StagingKind::Blob,
        id: Id128::from_bytes([0x03; 16]),
    };
    let current_tmp = EntryName::Root(RootName::CurrentTmp);

    vec![
        // The accepted commit order: content before name, name before pointer, pointer last.
        Schedule::new("commit-happy-path")
            .op(Op::CreateNew {
                dir: blobs.clone(),
                name: blob,
            })
            .op(Op::Write {
                bytes: b"blob bytes".to_vec(),
            })
            .op(Op::SyncFile)
            .op(Op::PublishNoOverwrite {
                staging: staging.clone(),
                staged: blob,
                dst_dir: blobs.clone(),
                dst: EntryName::Blob([0x11; 32]),
            })
            .op(Op::SyncDir { dir: blobs.clone() })
            .op(Op::CreateNew {
                dir: staging.clone(),
                name: tx,
            })
            .op(Op::Write {
                bytes: b"tx bytes".to_vec(),
            })
            .op(Op::SyncFile)
            .op(Op::PublishNoOverwrite {
                staging: staging.clone(),
                staged: tx,
                dst_dir: manifests.clone(),
                dst: EntryName::Transaction(Id128::from_bytes([0x01; 16])),
            })
            .op(Op::SyncDir {
                dir: manifests.clone(),
            })
            .op(Op::CreateNew {
                dir: staging.clone(),
                name: man,
            })
            .op(Op::Write {
                bytes: b"manifest bytes".to_vec(),
            })
            .op(Op::SyncFile)
            .op(Op::PublishNoOverwrite {
                staging: staging.clone(),
                staged: man,
                dst_dir: manifests.clone(),
                dst: EntryName::Manifest {
                    version: 1,
                    id: Id128::from_bytes([0x02; 16]),
                },
            })
            .op(Op::SyncDir {
                dir: manifests.clone(),
            })
            .op(Op::CreateNew {
                dir: staging.clone(),
                name: current_tmp,
            })
            .op(Op::Write {
                bytes: b"CURRENT bytes".to_vec(),
            })
            .op(Op::SyncFile)
            .op(Op::ReplaceCurrent {
                staging: staging.clone(),
                staged: current_tmp,
            })
            .op(Op::SyncDir {
                dir: staging.clone(),
            }),
        // A crash between the pointer replace and the directory sync: the case where the new root
        // is visible but its durability is unproven, so the store must not ack.
        Schedule::new("crash-after-current-rename")
            .op(Op::CreateNew {
                dir: staging.clone(),
                name: current_tmp,
            })
            .op(Op::Write {
                bytes: b"CURRENT bytes".to_vec(),
            })
            .op(Op::SyncFile)
            .op(Op::ReplaceCurrent {
                staging: staging.clone(),
                staged: current_tmp,
            })
            .fault(Fault::CrashAfter { at: 3 }),
        // An ambiguous publish: the name may or may not exist afterwards.
        Schedule::new("ambiguous-publish")
            .op(Op::CreateNew {
                dir: blobs.clone(),
                name: blob,
            })
            .op(Op::Write {
                bytes: b"blob bytes".to_vec(),
            })
            .op(Op::SyncFile)
            .op(Op::PublishNoOverwrite {
                staging: staging.clone(),
                staged: blob,
                dst_dir: blobs.clone(),
                dst: EntryName::Blob([0x22; 32]),
            })
            .fault(Fault::Ambiguous { at: 3 }),
        // A budget refusal must happen before the write, so no bytes are retained.
        Schedule::new("budget-refuses-write")
            .op(Op::CreateNew {
                dir: staging.clone(),
                name: tx,
            })
            .op(Op::Write {
                bytes: vec![0u8; 1024],
            })
            .fault(Fault::Budget {
                at: 1,
                unit: BudgetUnitName::Bytes,
                requested: 1024,
                remaining: 0,
            }),
    ]
}

#[cfg(test)]
mod tests;
