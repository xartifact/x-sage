//! Explicit genesis initialization.
//!
//! `init` is the one operation allowed to create a KB, and it creates it the only way the protocol permits: an
//! **empty** directory, then the genesis transaction, its manifest, and `CURRENT`, published through the same
//! steps a commit uses. There is no separate "initialize" path that writes files directly, because a second path
//! is a second chance to get the ordering wrong.
//!
//! ## Refusals that matter
//!
//! * A directory that already has content is **not** initialized over. `CURRENT` missing on a non-empty
//!   directory is `CORRUPT`, never "uninitialized" — treating it as empty would publish a genesis over data that
//!   some earlier attempt left behind.
//! * `CURRENT` present means the KB already exists; `init` refuses rather than overwriting a commit root.
//! * The root is created, then its **parent** is synced, so the directory entry itself is durable before anything
//!   inside it claims to be.

use sage_db_types::framing::Limits;
use sage_db_types::ledger::SharedLedger;

use crate::fs::{CreateDir, Each, Family, FileSystem, FsError, FsErrorKind};
use crate::names::{EntryName, RootName};

/// Why initialization refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InitRefusal {
    /// The directory holds entries and no `CURRENT`: not empty, so not a candidate.
    NotEmpty,
    /// `CURRENT` already exists: this KB is initialized.
    AlreadyInitialized,
    /// A filesystem operation failed.
    ///
    /// Carries the seam's own error, so `Effect` (NoEffect vs Unknown), `FsErrorKind`, and any budget or control
    /// payload survive to the caller. Flattening these into a step name would discard the distinction between
    /// "definitely did nothing" and "may have done something".
    Filesystem {
        /// Which step.
        step: InitStep,
        /// The seam's error, preserved intact.
        error: FsError,
    },
    /// The caller's ledger refused.
    Budget,
    /// The caller stopped.
    Stopped(sage_db_types::query::ControlStop),
}

/// The steps `init` performs, so a refusal identifies its own step.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitStep {
    /// Create or open the root directory.
    Root,
    /// Sync the root's parent so its entry is durable.
    RootParentSync,
    /// Take the exclusive lock.
    Lock,
    /// Create the fixed families.
    Families,
    /// Sync the root directory after creating families.
    RootSync,
}

/// A proven-fresh root whose exclusive lock is still held by this attempt.
///
/// The lock and the root travel TOGETHER, which is the point. `init` must not drop the lock it took: a CURRENT-less
/// directory at that moment does not distinguish a fresh root from a torn non-empty one, and anyone could change it while
/// the lock is released. The bootstrap therefore continues — families, root sync, then the private Genesis commit — under
/// one continuous observation, rather than re-locking and re-inspecting a directory that may have changed.
///
/// This is a handoff of already-proven ownership, not a new recovery path: no second lock is taken and no filesystem
/// trait is added.
pub struct FreshRoot<F: FileSystem> {
    /// The root directory handle, opened under the held lock. Private: the bootstrap is the only consumer.
    dir: F::Dir,
    /// The exclusive lock. Private for the same reason — a public field lets a caller drop the lock and later
    /// re-acquire one, which is exactly the observation split this token exists to prevent.
    lock: F::Lock,
}

impl<F: FileSystem> FreshRoot<F> {
    /// Consume the token, yielding the root and its still-held lock to the bootstrap.
    ///
    /// `pub(crate)` and consuming: there is no way to read the fields out of a `FreshRoot` without ending its life, and no
    /// way to construct one except by passing through [`prepare_fresh`].
    pub(crate) fn into_parts(self) -> (F::Dir, F::Lock) {
        (self.dir, self.lock)
    }
}

impl<F: FileSystem> core::fmt::Debug for FreshRoot<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("FreshRoot { dir, lock }")
    }
}

/// The result of preparing a root directory.
///
/// Deliberately **not** called "initialized": this reports a prepared, durable container. A KB becomes initialized
/// only once a genesis transaction, its manifest and `CURRENT` are published through the commit path. Reporting
/// `Initialized` here would claim a state this function does not produce.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PreparedRoot {
    /// The root and its fixed families exist and their entries are durable.
    ///
    /// **This is not a published KB and carries no version claim.** There is no genesis transaction, no manifest and
    /// no `CURRENT`, so the directory cannot be opened as a database. "Prepared" names a durable *container*, and
    /// reading it as "published" or "initialized" would be a claim this function does not make. The transition to a
    /// usable KB is `commit` reporting `Committed`, which is a different type from a different module on purpose.
    Prepared {
        /// Steps completed, in order, for a caller that wants to report progress.
        completed: [InitStep; 3],
    },
    /// Nothing authoritative was created.
    Refused(InitRefusal),
}

/// Initialize a KB root that must be **empty**, returning ownership of the root and its still-held lock.
///
/// This is the bootstrap path. The exclusive lock is taken here and travels with the root, so freshness is one continuous
/// observation rather than two.
///
/// # Errors
///
/// [`InitRefusal`] when the root is not a legitimate candidate. Nothing authoritative is published.
pub fn prepare_fresh<F: FileSystem>(
    fs: &F,
    abs: &std::path::Path,
    create: CreateDir,
    ledger: &mut dyn SharedLedger,
) -> Result<FreshRoot<F>, InitRefusal> {
    prepare_impl(fs, abs, create, true, ledger)
}

/// Prepare a new directory for a backup or restore. Existing entries are left untouched.
pub(crate) fn prepare_new<F: FileSystem>(
    fs: &F,
    abs: &std::path::Path,
    ledger: &mut dyn SharedLedger,
) -> Result<FreshRoot<F>, InitRefusal> {
    prepare_impl(fs, abs, CreateDir::IfMissing, false, ledger)
}

/// Prepare the fixed container, reporting only success or a refusal.
///
/// Kept because a caller may legitimately want just the durable container. It drops the lock on return, so it must NOT be
/// used as the first half of a bootstrap: use [`prepare_fresh`] for that.
///
/// # Errors
///
/// Never `Err`; every failure is a [`PreparedRoot::Refused`].
pub fn init<F: FileSystem>(
    fs: &F,
    abs: &std::path::Path,
    create: CreateDir,
    limits: &Limits,
    ledger: &mut dyn SharedLedger,
) -> PreparedRoot {
    let _ = limits;
    match prepare_impl(fs, abs, create, true, ledger) {
        Ok(_fresh) => PreparedRoot::Prepared {
            completed: [InitStep::Root, InitStep::Lock, InitStep::RootSync],
        },
        Err(refusal) => PreparedRoot::Refused(refusal),
    }
}

/// Initialize a KB root that must be **empty**.
///
/// Ordering, each step a real durability edge:
///
/// 1. open the root (creating it only when `create` is set), then sync its parent so the directory entry is
///    durable — a root that exists in cache only cannot hold a durable KB;
/// 2. take the exclusive lock, so nothing else initializes concurrently;
/// 3. refuse if `CURRENT` exists (already initialized) or if any entry exists (not a candidate);
/// 4. create the fixed families with `CreateDir::IfMissing`, then sync the root so those entries are durable.
///
/// Genesis transaction and manifest publication then follow through the commit path, so `init` and `commit`
/// share one ordering rather than two.
///
/// # Errors
///
/// Never `Err`; every failure is a [`PreparedRoot::Refused`], because "not initialized" is an answer a caller acts on.
fn prepare_impl<F: FileSystem>(
    fs: &F,
    abs: &std::path::Path,
    create: CreateDir,
    allow_existing: bool,
    ledger: &mut dyn SharedLedger,
) -> Result<FreshRoot<F>, InitRefusal> {
    if let Err(stop) = ledger.checkpoint() {
        // The payload distinguishes cancellation from an expired deadline, which matter differently to a retrying caller.
        return Err(InitRefusal::Stopped(stop));
    }

    // 1. Root directory, then its parent synced so the entry is durable.
    let root = match create {
        CreateDir::IfMissing => match fs.create_root(abs) {
            Ok(root) => root,
            Err(error) => {
                // An existing root is not a failure of `init`; it is opened instead.
                if error.kind() == FsErrorKind::AlreadyExists {
                    if !allow_existing {
                        return Err(InitRefusal::AlreadyInitialized);
                    }
                    match fs.open_root(abs) {
                        Ok(root) => root,
                        Err(error) => return refused_with(InitStep::Root, error),
                    }
                } else {
                    // The seam returned a real error with a real `Effect`. Replacing it with a synthetic
                    // `Io(0)`/`NoEffect` would discard whether the attempt could have created anything.
                    return refused_with(InitStep::Root, error);
                }
            }
        },
        CreateDir::Never => match fs.open_root(abs) {
            Ok(root) => root,
            Err(error) => return refused_with(InitStep::Root, error),
        },
    };
    if let Err(error) = fs.sync_parent_of(abs) {
        // Preserve the seam's own error, including `Effect`: a bare step name would discard whether the failed sync
        // could have created the entry.
        return refused_with(InitStep::RootParentSync, error);
    }

    // 2. Exclusive lock: nothing else may initialize this root.
    let _lock = match fs.try_lock_exclusive(&root) {
        Ok(lock) => lock,
        Err(error) => return refused_with(InitStep::Lock, error),
    };

    // 3. Refuse a root that already holds state, whether or not it is initialized.
    match fs.open_family(&root, Family::Manifests, CreateDir::Never) {
        Ok(_) => {
            // The family exists, so this directory has been used as a KB before.
            return Err(InitRefusal::AlreadyInitialized);
        }
        Err(error) if error.kind() == FsErrorKind::NotFound => {}
        Err(error) => return refused_with(InitStep::Families, error),
    }
    // A `CURRENT` in the root is the definitive marker. Only `NotFound` establishes absence; any other error means the
    // root could not be inspected, which is not the same as "empty" and must not be treated as a candidate.
    match fs.size(&root, EntryName::Root(RootName::Current)) {
        Ok(_) => return Err(InitRefusal::AlreadyInitialized),
        Err(error) if error.kind() == FsErrorKind::NotFound => {}
        Err(error) => return refused_with(InitStep::Root, error),
    }
    match is_empty(&root, fs, ledger) {
        Ok(true) => {}
        Ok(false) => return Err(InitRefusal::NotEmpty),
        // Inability to inspect is its own refusal, never "NotEmpty".
        Err(error) => return refused_with(InitStep::Root, error),
    }

    // 4. Fixed families, then the root synced so their entries are durable.
    for family in [
        Family::Staging,
        Family::Blobs,
        Family::Transactions,
        Family::Manifests,
    ] {
        if let Err(error) = fs.open_family(&root, family, CreateDir::IfMissing) {
            return refused_with(InitStep::Families, error);
        }
    }
    if let Err(error) = fs.sync_dir(&root) {
        return refused_with(InitStep::RootSync, error);
    }

    Ok(FreshRoot {
        dir: root,
        lock: _lock,
    })
}

/// Whether the root holds no entries besides `LOCK`, which this attempt holds.
///
/// A directory we cannot read is **not** empty: refusing to initialize over an unreadable root is the safe
/// answer, because the alternative is publishing a genesis over data we could not see.
/// Whether the root holds no entries besides `LOCK`, which this attempt holds.
///
/// Returns `Err` when the traversal could not complete. An unreadable or interrupted traversal is **not** an empty
/// directory: reporting it as "not empty" would assert a fact we do not have, and would disguise a budget or control stop
/// as a populated root.
fn is_empty<F: FileSystem>(
    root: &F::Dir,
    fs: &F,
    ledger: &mut dyn SharedLedger,
) -> Result<bool, FsError> {
    let mut saw_any = false;
    let mut visit = |name: &str| {
        if name == RootName::Lock.as_str() {
            Each::Continue
        } else {
            saw_any = true;
            Each::Stop
        }
    };
    // The traversal is the only thing that can tell us; its error is propagated rather than reinterpreted.
    fs.for_each_entry(root, ledger, &mut visit)?;
    Ok(!saw_any)
}

/// Open a **newly created KB**: prepare the container under one continuously held lock, commit Genesis through the
/// accepted format and commit path, and return a Store that is only usable because Genesis actually succeeded.
///
/// This is the only public way to obtain a writable `Store`, and that is deliberate:
///
/// * the exclusive lock is taken once and travels with the root ([`FreshRoot`]), so freshness is one continuous
///   observation — a CURRENT-less directory in between proves nothing, and nobody can re-lock and re-inspect;
/// * Genesis is not a client request, so it skips the client idempotency lookup, but it still goes through the same
///   validation and durable publication as any other transaction;
/// * the returned handle is constructed only after `Committed`, so a pre-Genesis container can never be handed out as a
///   ready Store and cannot claim definitive absence.
///
/// # Errors
///
/// [`BootstrapRefusal`] for a container refusal, a Genesis refusal, or a handle that could not be built.
///
/// # What a refusal does NOT promise
///
/// The lock is released on every refusal, but authority is a different question. A *definite* refusal (container rejected,
/// format rejected, budget refused before persistence) leaves nothing published. An **ambiguous** outcome — `Unknown`,
/// where the durable point may or may not have been reached — leaves the root in a state nobody can summarise from here, so
/// no `Ready` handle is returned and no absence is claimed. Saying "nothing authoritative remains" would be false for that
/// case, and it is exactly the case a recovery path exists for.
pub fn bootstrap<F: FileSystem>(
    fs: F,
    abs: &std::path::Path,
    create: CreateDir,
    kb_uuid: sage_db_types::identity::KbUuid,
    rules: &[u8],
    limits: &Limits,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<crate::handle::Store<F>, BootstrapRefusal> {
    bootstrap_with_profile(
        fs,
        abs,
        create,
        kb_uuid,
        (sage_db_types::relation::RuleProfile::FormalV1, rules),
        limits,
        ledger,
    )
}

/// Initialize a fresh root with an explicit profile and its actual rule bytes.
///
/// New ordinary roots continue to use [`bootstrap`]'s formal-v1 default. Administrative
/// migration may select legacy-v1 without relabeling the rule blob or changing the verifier.
/// `declared_rules` keeps that selection and its bytes together; a contradictory pair is
/// refused by the same canonical commit path. All freshness, locking and durability ordering
/// is shared with [`bootstrap`]. No existing root is upgraded or rewritten.
///
/// # Errors
/// [`BootstrapRefusal`], with the same authority and ambiguity limits as [`bootstrap`].
pub fn bootstrap_with_profile<F: FileSystem>(
    fs: F,
    abs: &std::path::Path,
    create: CreateDir,
    kb_uuid: sage_db_types::identity::KbUuid,
    declared_rules: (sage_db_types::relation::RuleProfile, &[u8]),
    limits: &Limits,
    ledger: &mut dyn crate::lease::LeaseLedger,
) -> Result<crate::handle::Store<F>, BootstrapRefusal> {
    let (profile, rules) = declared_rules;
    // 1. Container + lock, one observation. The token keeps root and lock together.
    let fresh = prepare_fresh(&fs, abs, create, ledger).map_err(BootstrapRefusal::Container)?;
    let (dir, lock) = fresh.into_parts();

    // 2. Families, opened under the same held lock.
    let families =
        crate::commit::open_families(&fs, &dir).map_err(|error| BootstrapRefusal::Filesystem {
            step: InitStep::Families,
            error,
        })?;

    // 3. A handle that is NOT yet ready: coverage is false until Genesis commits, so no ordinary request can be answered
    //    from it and definitive absence is impossible.
    let mut store = crate::handle::Store::open_initializing(fs, dir, families, lock, kb_uuid)
        .map_err(BootstrapRefusal::Handle)?;

    // 4. Genesis through the same durable path. `commit_genesis` is crate-private precisely so this is the only route.
    let genesis = crate::handle::genesis_candidate_with_profile(kb_uuid, profile, rules, ledger)
        .map_err(BootstrapRefusal::Genesis)?;
    match store.commit_genesis(genesis, limits, ledger) {
        // Genesis is version 1 by construction (`genesis_candidate`), so a `Committed` here IS the version-1 commit; any
        // other outcome means the container is not a KB.
        crate::handle::HandleOutcome::Committed { .. } => {}
        other => return Err(BootstrapRefusal::GenesisNotCommitted(Box::new(other))),
    }

    // 5. Ready only now: coverage is established by the commit, not by the container looking empty.
    store.mark_ready();
    Ok(store)
}

/// Why a bootstrap could not produce a usable Store.
#[derive(Debug)]
pub enum BootstrapRefusal {
    /// The container was not a legitimate candidate.
    Container(InitRefusal),
    /// A filesystem step failed while opening the fixed families.
    Filesystem {
        /// Which step.
        step: InitStep,
        /// The seam's own error, with `Effect` intact.
        error: FsError,
    },
    /// The handle could not be constructed.
    Handle(crate::handle::HandleRefusal),
    /// The Genesis candidate could not be built.
    Genesis(crate::commit::CommitRefusal),
    /// Genesis did not commit, so the container is not a KB.
    GenesisNotCommitted(Box<crate::handle::HandleOutcome>),
}

fn refused_with<F: FileSystem>(
    step: InitStep,
    error: FsError,
) -> Result<FreshRoot<F>, InitRefusal> {
    Err(InitRefusal::Filesystem { step, error })
}
