//! Offline versioned backup and fresh-root restore.
//!
//! # What this is
//!
//! A **physical storage primitive**, like `verify`: it exports one committed version of a KB and
//! all the bytes that version's history refers to into a self-contained directory, and it restores
//! such a directory into a fresh root. It mints no grants, authenticates nothing, and routes
//! nothing — a future admin adapter authorizes `Admin` on the actual KB before calling in.
//!
//! # Authority is never selected by listing a directory
//!
//! The source is bound through the existing locked `Store` (`backup_source`), so the export holds
//! a borrow that safe callers cannot mutate across. `CURRENT` stays the only commit root:
//! `open::observe_root` reads it once and walks the recorded predecessor chain, so an orphan or a
//! higher-numbered file can never become authority. For an older version the selection is the real
//! `ChainRow` in that verified chain, and that version's `CURRENT` is built through the canonical
//! `CurrentToEncode`/`encode_current` rather than hand-serialized.
//!
//! # The artifact is an offline format, not a second interpretation of the store
//!
//! The inventory is 72 bytes of header plus fixed 65-byte rows; `backup.complete` is 48 bytes and
//! must be written **last**, after the destination has been verified by replay under its own lock.
//! A directory without a valid completion marker is not a usable backup, however many files it
//! holds.
//!
//! # Blobs come from history, not from current rows
//!
//! `backup_blob_union` delegates to the same replay engine the store opens with, so the export
//! carries every blob any committed version referenced — including content a record has since
//! replaced. Selecting from current rows would silently drop exactly that.
//!
//! # Cost
//!
//! One caller `LeaseLedger` spans planning, validation, inventory, every streaming copy, every
//! hash and every sync. Data files stream through a stack chunk. Metadata held whole is bounded,
//! and every retained table is admitted before it is reserved.

use sage_db_format::digest::StreamingSha256;
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::identity::KbUuid;

use crate::commit::Families;
use crate::fs::{CreateDir, Family, FileSystem, FsError, FsErrorKind, WriteOutcome};
use crate::handle::Store;
use crate::lease::LeaseLedger;
use crate::names::{EntryName, Id128, RootName};
use crate::open::OpenRefusal;
use crate::view::ViewBasis;

/// Inventory header magic, including the format's major version.
const INVENTORY_MAGIC: [u8; 8] = *b"SGBKUP\0\x01";

/// Inventory header size in bytes.
const HEADER_BYTES: usize = 72;

/// One inventory row: `tag:u8 | version:u64 | id16 | file_sha32 | file_len:u64`.
const ROW_BYTES: usize = 65;

/// Completion-file magic.
const COMPLETE_MAGIC: [u8; 8] = *b"SGBDONE1";

/// Completion-file size in bytes: magic, inventory digest, inventory length.
const COMPLETE_BYTES: usize = 48;

/// Streaming copy chunk. Small and on the stack: no file is ever buffered whole.
const CHUNK: usize = 1024;

/// Which kind of file a row describes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum RowTag {
    Current = 0,
    Manifest = 1,
    Transaction = 2,
    Blob = 3,
}

impl RowTag {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Current),
            1 => Some(Self::Manifest),
            2 => Some(Self::Transaction),
            3 => Some(Self::Blob),
            _ => None,
        }
    }
}

/// One file the artifact carries, with the length it must have on disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Row {
    tag: RowTag,
    version: u64,
    id: [u8; 16],
    sha256: [u8; 32],
    file_len: u64,
}

impl Row {
    /// The canonical typed name of the file this row describes.
    ///
    /// Derived through `EntryName`, so no path grammar or filename string is duplicated here.
    fn name(&self) -> Option<EntryName> {
        match self.tag {
            RowTag::Current => Some(EntryName::Root(RootName::Current)),
            RowTag::Manifest => Some(EntryName::Manifest {
                version: self.version,
                id: Id128::from_bytes(self.id),
            }),
            RowTag::Transaction => Some(EntryName::Transaction(Id128::from_bytes(self.id))),
            RowTag::Blob => Some(EntryName::Blob(self.sha256)),
        }
    }

    /// Which family the file lives in, or `None` for the root.
    fn family(&self) -> Option<Family> {
        match self.tag {
            RowTag::Current => None,
            RowTag::Manifest => Some(Family::Manifests),
            RowTag::Transaction => Some(Family::Transactions),
            RowTag::Blob => Some(Family::Blobs),
        }
    }

    /// The canonical ordering: tag, then version/id, then blob hash.
    fn sort_key(&self) -> (u8, u64, u128, [u8; 32]) {
        (
            self.tag as u8,
            self.version,
            u128::from_be_bytes(self.id),
            if self.tag == RowTag::Blob {
                self.sha256
            } else {
                [0; 32]
            },
        )
    }
}

/// The configured ceilings a backup or restore must fit before anything is created.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BackupLimits {
    /// Maximum files the artifact may contain.
    pub max_files: u64,
    /// Maximum inventory bytes.
    pub max_inventory_bytes: u64,
    /// Maximum bytes in any single file.
    pub max_file_bytes: u64,
    /// Maximum bytes across all files.
    pub max_total_bytes: u64,
}

impl BackupLimits {
    /// Generous defaults for a local operator backup.
    pub const DEFAULT: Self = Self {
        max_files: 1_000_000,
        max_inventory_bytes: 256 * 1024 * 1024,
        max_file_bytes: 4 * 1024 * 1024 * 1024,
        max_total_bytes: 1 << 40,
    };
}

/// Why a backup or restore did not complete.
///
/// Every variant carries a scalar or an existing canonical heap-free reason: the error path
/// allocates nothing, so a refusal cannot itself fail under the ledger that caused it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BackupError {
    /// The source has no definitive durable basis to bind.
    NoDurableBasis(crate::open::OutcomeIncomplete),
    /// The requested version is not an ancestor of the served one (or is zero/future).
    NotAnAncestor {
        /// The version the caller asked for.
        requested: u64,
        /// The version the source actually serves.
        served: u64,
    },
    /// A configured ceiling would be exceeded, decided before any allocation.
    LimitExceeded(BackupLimit),
    /// The caller's ledger refused, or the caller stopped.
    Refused(Refusal),
    /// A counted extent is not representable.
    ExtentOverflow,
    /// A real allocator refusal at an already-admitted extent.
    Allocation {
        /// Bytes requested.
        requested: usize,
    },
    /// Destination preparation refused for lack of budget.
    ///
    /// `InitRefusal::Budget` is deliberately **detail-less**: the canonical unit `BudgetError`
    /// carries figures this refusal does not have, so inventing `(unit, 0, 0)` would fabricate a
    /// measurement. The class is named instead, and the payload is recoverable from the
    /// filesystem error `init` reports when a real unit is involved.
    InitializerBudget,
    /// A real filesystem refusal, with the step that produced it.
    ///
    /// Carries the **whole** `FsError`, not just its kind: `Effect` (NoEffect vs Unknown), a
    /// budget payload and a control stop are three different remedies, and collapsing them to a
    /// kind would lose exactly that.
    Filesystem {
        /// Which step.
        step: BackupStep,
        /// The seam's own error, preserved intact.
        error: crate::fs::FsError,
    },
    /// The artifact carries no valid completion marker.
    MissingComplete,
    /// The completion marker does not describe this inventory.
    CompleteMismatch,
    /// The inventory is corrupt: wrong magic, length, tag, count or ordering.
    InventoryCorrupt(InventoryDefect),
    /// A file's bytes do not match the inventory's declared length or hash.
    FileMismatch {
        /// The row that disagreed.
        tag: u8,
    },
    /// The destination is not fresh.
    DestinationExists,
    /// The backup names a different KB than the caller expects.
    KbMismatch {
        /// What the artifact declares.
        found: KbUuid,
        /// What the caller requires.
        expected: KbUuid,
    },
    /// The replayed DESTINATION refused, so the copy is not a usable root.
    Replay(OpenRefusal),
    /// A confirmed SOURCE read/replay failure. This is the only class that withdraws serving.
    SourceRefused(OpenRefusal),
    /// An internal invariant this module was supposed to maintain did not hold.
    Invariant {
        /// Fixed, non-localised description.
        detail: &'static str,
    },
}

/// Which backup step produced a filesystem refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackupStep {
    /// Observing the source.
    Observe,
    /// Reading a source file.
    ReadSource,
    /// Creating the destination root.
    CreateDestination,
    /// Writing a destination file.
    WriteDestination,
    /// Syncing a destination file or directory.
    SyncDestination,
    /// Verifying the destination by replay.
    VerifyDestination,
}

/// A configured ceiling that was exceeded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackupLimit {
    /// Too many files.
    Files,
    /// Inventory too large.
    InventoryBytes,
    /// One file too large.
    FileBytes,
    /// Total too large.
    TotalBytes,
}

/// Why an inventory could not be decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InventoryDefect {
    /// The magic or format major does not match.
    Magic,
    /// The length is not header + `count * 65`, or the declared count disagrees.
    Length,
    /// A row tag is not registered.
    Tag,
    /// A row's scalar fields are not legal for its tag.
    FieldShape,
    /// Rows are not in canonical order, or a typed name repeats.
    Order,
    /// There is not exactly one CURRENT row.
    Current,
    /// The selected chain is incomplete.
    Chain,
    /// The declared file length is zero where a file must exist.
    EmptyFile,
}

impl From<Refusal> for BackupError {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// The result of a successful export.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BackupReport {
    /// The version that was exported.
    pub version: u64,
    /// How many files the artifact carries (excluding the inventory and completion marker).
    pub files: u64,
    /// Total bytes of those files.
    pub bytes: u64,
    /// SHA-256 of the inventory file as written.
    pub inventory_sha256: [u8; 32],
}

/// The result of a successful restore.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RestoreReport {
    /// The KB the restored root belongs to.
    pub kb_uuid: KbUuid,
    /// The version the restored root serves.
    pub version: u64,
    /// How many files were copied.
    pub files: u64,
    /// The durable basis of the restored root.
    pub basis: ViewBasis,
}

/// Export `version` of `source` and its complete historical blob set into a fresh `destination`.
///
/// `backup.complete` is written last, after the destination has been verified by the canonical
/// replay path under its own held lock, so an interrupted export never looks usable.
///
/// # Errors
///
/// [`BackupError`]: an unbound durable basis, a version that is not an ancestor, a configured
/// ceiling, a real ledger/allocator/filesystem refusal, or a failed destination verification. A
/// confirmed *source* read or replay failure also withdraws the source, exactly as diagnostic
/// verify does; destination failures never do.
pub fn create<F: FileSystem>(
    source: &mut Store<F>,
    destination_fs: &F,
    destination: &std::path::Path,
    version: u64,
    limits: &BackupLimits,
    ledger: &mut dyn LeaseLedger,
) -> Result<BackupReport, BackupError> {
    // ALL source work happens inside this borrow: `backup_source` borrows the store, while
    // `backup_source_refused` needs `&mut`. So the block yields either a report or an error plus
    // the optional SOURCE refusal that justifies fencing, and the fence happens afterwards.
    let outcome: Result<BackupReport, BackupError> =
        create_inner(source, destination_fs, destination, version, limits, ledger);

    match outcome {
        Ok(report) => Ok(report),
        Err(error) => {
            // A source-side replay failure is the ONLY thing that fences serving; destination
            // failures are plain errors and never reach this arm.
            let refusal = match &error {
                BackupError::SourceRefused(refusal) => Some(refusal.clone()),
                _ => None,
            };
            // Only a CONFIRMED SOURCE read/replay failure withdraws serving, exactly as the
            // diagnostic verify path does. Destination failures carry `None` and never fence.
            if let Some(refusal) = refusal {
                source.backup_source_refused(&refusal);
            }
            Err(error)
        }
    }
}

/// The borrowed body of [`create`].
fn create_inner<F: FileSystem>(
    source: &mut Store<F>,
    destination_fs: &F,
    destination: &std::path::Path,
    version: u64,
    limits: &BackupLimits,
    ledger: &mut dyn LeaseLedger,
) -> Result<BackupReport, BackupError> {
    let borrowed = match source.backup_source() {
        Ok(borrowed) => borrowed,
        Err(incomplete) => return Err(BackupError::NoDurableBasis(incomplete)),
    };
    let served = borrowed.basis.version.get();
    if version == 0 || version > served {
        return Err(BackupError::NotAnAncestor {
            requested: version,
            served,
        });
    }

    // Observe CURRENT once and require it to match the basis actually served, BEFORE selecting
    // an ancestor.
    let observed = match crate::open::observe_root(
        borrowed.fs,
        borrowed.root,
        borrowed.families,
        &Limits::V1,
        ledger,
    ) {
        Ok(observed) => observed,
        Err(error) => return Err(BackupError::SourceRefused(error)),
    };
    if observed.basis() != borrowed.basis {
        return Err(BackupError::SourceRefused(OpenRefusal::Corrupt(
            crate::open::CorruptReason::ChainLinkMismatch,
        )));
    }
    let mut ancestor = None;
    for (index, row) in observed.chain.iter().enumerate() {
        ledger
            .try_charge_entries(1)
            .map_err(|error| BackupError::Refused(Refusal::Budget(error)))?;
        ledger
            .checkpoint()
            .map_err(|stop| BackupError::Refused(Refusal::Stopped(stop)))?;
        if row.version == version {
            ancestor = Some(&observed.chain[index..]);
            break;
        }
    }
    let chain = ancestor.ok_or(BackupError::NotAnAncestor {
        requested: version,
        served,
    })?;

    // Select the real ancestor rows and the FULL historical blob union, delegating replay.
    let selected = &chain[0];
    let current = encode_selected_current(
        borrowed.basis.kb_uuid,
        version,
        selected.manifest_name,
        selected.manifest_sha256,
        ledger,
    )?;
    let plan = match plan_export(
        borrowed.fs,
        borrowed.root,
        borrowed.families,
        borrowed.basis.kb_uuid,
        chain,
        version,
        current,
        limits,
        ledger,
    ) {
        Ok(plan) => plan,
        // A configured ceiling is a LIMIT verdict and must never fence a healthy source; a real
        // ledger/allocator/read refusal keeps its own class and does.
        Err(PlanError::Limit(limit)) => return Err(BackupError::LimitExceeded(limit)),
        Err(PlanError::Source(error)) => return Err(BackupError::SourceRefused(error)),
        Err(PlanError::Local(error)) => return Err(error),
    };

    // ---- Destination only from here: no source refusal for anything below. ----
    // A destination must affirmatively support the durable primitives this copy relies on.
    let caps = destination_fs.capabilities();
    let can_write = caps.durable_sync
        && caps
            .supported
            .contains(&crate::fs::PlatformCapability::FileSync)
        && caps
            .supported
            .contains(&crate::fs::PlatformCapability::DirSync);
    if !can_write {
        return Err(BackupError::Invariant {
            detail: "destination filesystem is not writable",
        });
    }
    let fresh = crate::init::prepare_new(destination_fs, destination, ledger).map_err(map_init)?;
    let (root_dir, _lock) = fresh.into_parts();
    let dest_families = open_destination_families(destination_fs, &root_dir, ledger)?;

    // Real backends may create LOCK when acquiring it. Backends whose logical lock
    // lives separately still need the same, empty on-disk artifact entry.
    let lock_name = EntryName::Root(RootName::Lock);
    match destination_fs.size(&root_dir, lock_name) {
        Ok(0) => {}
        Ok(_) => {
            return Err(BackupError::Invariant {
                detail: "artifact LOCK is not empty",
            });
        }
        Err(error) if error.kind() == FsErrorKind::NotFound => {
            write_whole(
                destination_fs,
                &root_dir,
                lock_name,
                &[],
                BackupStep::WriteDestination,
                ledger,
            )?;
        }
        Err(error) => return Err(filesystem_error(BackupStep::ReadSource, error)),
    }

    for row in &plan.rows {
        if row.tag == RowTag::Current {
            write_whole(
                destination_fs,
                &root_dir,
                EntryName::Root(RootName::Current),
                &plan.current,
                BackupStep::WriteDestination,
                ledger,
            )?;
        } else {
            copy_row(
                borrowed.fs,
                borrowed.root,
                borrowed.families,
                destination_fs,
                &root_dir,
                &dest_families,
                row,
                ledger,
            )?;
        }
    }

    let inventory_bytes = encode_inventory(&plan, ledger)?;
    if inventory_bytes.len() as u64 > limits.max_inventory_bytes {
        return Err(BackupError::LimitExceeded(BackupLimit::InventoryBytes));
    }
    write_whole(
        destination_fs,
        &root_dir,
        EntryName::Root(RootName::BackupInventory),
        &inventory_bytes,
        BackupStep::WriteDestination,
        ledger,
    )?;
    let inventory_sha = raw_sha(&inventory_bytes, ledger)?;

    // Verify the destination through the CANONICAL observe path under the SAME held lock.
    match crate::open::observe_root(
        destination_fs,
        &root_dir,
        &dest_families,
        &Limits::V1,
        ledger,
    ) {
        Ok(observed) => {
            if observed.version() != version
                || observed.current.kb_uuid != plan.kb_uuid
                || observed.current.manifest_sha256 != plan.manifest_sha256
            {
                return Err(BackupError::Invariant {
                    detail: "destination CURRENT does not name the exported version",
                });
            }
            // `observe_root` verifies the MANIFEST CHAIN only: it does not replay transactions or
            // check that every referenced blob arrived. A current-only blob selection would pass
            // that check and then fail at restore, so this runs the full canonical replay and
            // requires its referenced set to match the plan.
            let union = match crate::handle::backup_blob_union(
                destination_fs,
                &dest_families,
                &observed.chain,
                &Limits::V1,
                ledger,
            ) {
                Ok(union) => union,
                Err(error) => return Err(BackupError::Replay(error)),
            };
            let blobs = plan.rows.iter().filter(|row| row.tag == RowTag::Blob);
            if blobs.clone().count() != union.len() {
                return Err(BackupError::Invariant {
                    detail: "destination replay blob count differs from the plan",
                });
            }
            for (row, digest) in blobs.zip(union) {
                ledger
                    .try_charge_entries(1)
                    .map_err(|error| BackupError::Refused(Refusal::Budget(error)))?;
                ledger
                    .checkpoint()
                    .map_err(|stop| BackupError::Refused(Refusal::Stopped(stop)))?;
                if row.sha256 != digest {
                    return Err(BackupError::Invariant {
                        detail: "destination replay references a blob the artifact lacks",
                    });
                }
            }
        }
        Err(error) => return Err(BackupError::Replay(error)),
    }

    // Persist every copied name and the inventory before a complete marker can
    // become visible in a recovered artifact.
    sync_layout(destination_fs, &root_dir, &dest_families, ledger)?;

    // `backup.complete` LAST, then sync the file and the root.
    let complete = encode_complete(inventory_sha, inventory_bytes.len() as u64)?;
    write_whole(
        destination_fs,
        &root_dir,
        EntryName::Root(RootName::BackupComplete),
        &complete,
        BackupStep::WriteDestination,
        ledger,
    )?;
    destination_fs
        .sync_entry(&root_dir, EntryName::Root(RootName::BackupComplete))
        .map_err(|error| filesystem_error(BackupStep::SyncDestination, error))?;
    destination_fs
        .sync_dir(&root_dir)
        .map_err(|error| filesystem_error(BackupStep::SyncDestination, error))?;

    Ok(BackupReport {
        version,
        files: plan.rows.len() as u64,
        bytes: plan.total_bytes,
        inventory_sha256: inventory_sha,
    })
}

/// Why planning refused, keeping a configured ceiling apart from a real refusal.
enum PlanError {
    /// An operator-configured ceiling would be exceeded. Never fences the source.
    Limit(BackupLimit),
    /// A source read/replay refusal.
    Source(OpenRefusal),
    /// A planning invariant unrelated to source authority.
    Local(BackupError),
}

impl From<OpenRefusal> for PlanError {
    fn from(error: OpenRefusal) -> Self {
        Self::Source(error)
    }
}

/// A planned export: the rows, their total size, and the held source identity.
struct ExportPlan {
    /// The KB the export belongs to.
    kb_uuid: KbUuid,
    /// The version exported; the inventory header names it.
    version: u64,
    /// The selected version's manifest FILE hash.
    manifest_sha256: [u8; 32],
    /// Bytes of the selected version's canonically encoded CURRENT.
    current: Vec<u8>,
    rows: Vec<Row>,
    total_bytes: u64,
}

/// Build the row list for `version`, including every historical blob reference.
#[allow(clippy::too_many_arguments)]
fn plan_export<F: FileSystem>(
    fs: &F,
    root: &F::Dir,
    families: &Families<F>,
    kb_uuid: KbUuid,
    chain: &[crate::open::ChainRow],
    version: u64,
    current: Vec<u8>,
    limits: &BackupLimits,
    ledger: &mut dyn LeaseLedger,
) -> Result<ExportPlan, PlanError> {
    // The selected ancestor's own rows, from the real verified chain.
    let mut rows: Vec<Row> = Vec::new();
    let selected_row = chain.first().ok_or(OpenRefusal::InvalidObservation)?;
    let union = crate::handle::backup_blob_union(fs, families, chain, &Limits::V1, ledger)?;
    let count = chain
        .len()
        .checked_mul(2)
        .and_then(|count| count.checked_add(union.len()))
        .and_then(|count| count.checked_add(1))
        .ok_or(OpenRefusal::ExtentOverflow)?;
    let inventory_len = count
        .checked_mul(ROW_BYTES)
        .and_then(|size| size.checked_add(HEADER_BYTES))
        .ok_or(OpenRefusal::ExtentOverflow)?;
    if count as u64 > limits.max_files {
        return Err(PlanError::Limit(BackupLimit::Files));
    }
    if inventory_len as u64 > limits.max_inventory_bytes {
        return Err(PlanError::Limit(BackupLimit::InventoryBytes));
    }
    crate::open::reserve_slots(&mut rows, count, ledger)?;
    if rows.capacity() != count {
        return Err(PlanError::Local(BackupError::Invariant {
            detail: "export row capacity differs from the admitted count",
        }));
    }
    for row in chain {
        let manifest_id = manifest_id_of(&row.manifest_name)?;
        charge_plan_entry(ledger)?;
        rows.push(Row {
            tag: RowTag::Manifest,
            version: row.version,
            id: manifest_id,
            sha256: row.manifest_sha256,
            file_len: 0,
        });
        let tx_id = tx_id_of(&row.tx_name)?;
        charge_plan_entry(ledger)?;
        rows.push(Row {
            tag: RowTag::Transaction,
            version: 0,
            id: tx_id,
            sha256: row.tx_file_sha256,
            file_len: 0,
        });
    }
    // The selected version's CURRENT, built canonically from the chain row.
    charge_plan_entry(ledger)?;
    rows.push(Row {
        tag: RowTag::Current,
        version: 0,
        id: [0u8; 16],
        sha256: sage_db_format::digest::raw_sha256_bounded(&current, ledger)
            .map_err(crate::open::refusal_from_hash)?,
        file_len: current.len() as u64,
    });

    // EVERY historical blob the selected observation referenced.
    for digest in &union {
        charge_plan_entry(ledger)?;
        rows.push(Row {
            tag: RowTag::Blob,
            version: 0,
            id: [0u8; 16],
            sha256: *digest,
            file_len: 0,
        });
    }

    // Canonical order. Done by an ATTENDED sort so every comparison is charged and interruptible;
    // a std sort comparator cannot fail, charge or be cancelled.
    attended_sort_rows(&mut rows, ledger)?;
    for window in rows.windows(2) {
        charge_plan_entry(ledger)?;
        if window[0].sort_key() == window[1].sort_key() {
            return Err(PlanError::Source(OpenRefusal::Corrupt(
                crate::open::CorruptReason::ChainLinkMismatch,
            )));
        }
    }

    // Fill the real lengths from the source, and preflight the configured ceilings BEFORE any
    // allocation or target creation.
    if rows.len() as u64 > limits.max_files {
        return Err(PlanError::Limit(BackupLimit::Files));
    }
    let mut total = 0u64;
    for row in &mut rows {
        let name = row.name().ok_or(OpenRefusal::Corrupt(
            crate::open::CorruptReason::ChainLinkMismatch,
        ))?;
        let dir = family_dir(root, families, row.family());
        let len = if row.tag == RowTag::Current {
            row.file_len
        } else {
            fs.size(dir, name)
                .map_err(|error| OpenRefusal::Filesystem {
                    step: crate::open::OpenStep::ReadCurrent,
                    error,
                })?
        };
        ledger
            .try_charge_entries(1)
            .map_err(|error| PlanError::Source(OpenRefusal::Refused(Refusal::Budget(error))))?;
        ledger
            .checkpoint()
            .map_err(|stop| PlanError::Source(OpenRefusal::Refused(Refusal::Stopped(stop))))?;
        if len > limits.max_file_bytes {
            return Err(PlanError::Limit(BackupLimit::FileBytes));
        }
        total = total
            .checked_add(len)
            .ok_or(PlanError::Source(OpenRefusal::ExtentOverflow))?;
        if total > limits.max_total_bytes {
            return Err(PlanError::Limit(BackupLimit::TotalBytes));
        }
        row.file_len = len;
    }

    Ok(ExportPlan {
        kb_uuid,
        version,
        manifest_sha256: selected_row.manifest_sha256,
        current,
        rows,
        total_bytes: total,
    })
}

// ---------------------------------------------------------------------------
// Small canonical helpers
// ---------------------------------------------------------------------------

fn charge_plan_entry(ledger: &mut dyn LeaseLedger) -> Result<(), OpenRefusal> {
    ledger
        .try_charge_entries(1)
        .map_err(|error| OpenRefusal::Refused(Refusal::Budget(error)))?;
    ledger
        .checkpoint()
        .map_err(|stop| OpenRefusal::Refused(Refusal::Stopped(stop)))
}

/// The family directory for a row, or the root for CURRENT.
fn family_dir<'a, F: FileSystem>(
    root: &'a F::Dir,
    families: &'a Families<F>,
    family: Option<Family>,
) -> &'a F::Dir {
    match family {
        // CURRENT lives in the root, not in a family.
        None => root,
        Some(Family::Manifests) => &families.manifests,
        Some(Family::Transactions) => &families.transactions,
        Some(Family::Blobs) => &families.blobs,
        Some(_) => root,
    }
}

/// A manifest `EntryName`'s identity bytes.
fn manifest_id_of(name: &EntryName) -> Result<[u8; 16], OpenRefusal> {
    match name {
        EntryName::Manifest { version: _, id } => Ok(*id.as_bytes()),
        _ => Err(OpenRefusal::Corrupt(
            crate::open::CorruptReason::ChainLinkMismatch,
        )),
    }
}

/// A transaction name's identity bytes.
fn tx_id_of(name: &EntryName) -> Result<[u8; 16], OpenRefusal> {
    match name {
        EntryName::Transaction(id) => Ok(*id.as_bytes()),
        _ => Err(OpenRefusal::Corrupt(
            crate::open::CorruptReason::ChainLinkMismatch,
        )),
    }
}

/// Open the destination's four families under its already-held root lock.
fn open_destination_families<F: FileSystem>(
    fs: &F,
    root: &F::Dir,
    ledger: &mut dyn LeaseLedger,
) -> Result<Families<F>, BackupError> {
    let mut open = |family| {
        ledger
            .try_charge_entries(1)
            .map_err(|error| BackupError::Refused(Refusal::Budget(error)))?;
        ledger
            .checkpoint()
            .map_err(|stop| BackupError::Refused(Refusal::Stopped(stop)))?;
        fs.open_family(root, family, CreateDir::Never)
            .map_err(|error| filesystem_error(BackupStep::CreateDestination, error))
    };
    Ok(Families {
        staging: open(Family::Staging)?,
        blobs: open(Family::Blobs)?,
        transactions: open(Family::Transactions)?,
        manifests: open(Family::Manifests)?,
    })
}

/// Copy one planned row from the source into the destination, streaming through a stack chunk.
#[allow(clippy::too_many_arguments)]
fn copy_row<F: FileSystem>(
    src_fs: &F,
    src_root: &F::Dir,
    src_families: &Families<F>,
    dst_fs: &F,
    dst_root: &F::Dir,
    dst_families: &Families<F>,
    row: &Row,
    ledger: &mut dyn LeaseLedger,
) -> Result<(), BackupError> {
    let name = row
        .name()
        .ok_or(BackupError::InventoryCorrupt(InventoryDefect::FieldShape))?;
    let (src_dir, dst_dir) = match row.family() {
        None => (src_root, dst_root),
        Some(Family::Manifests) => (&src_families.manifests, &dst_families.manifests),
        Some(Family::Transactions) => (&src_families.transactions, &dst_families.transactions),
        Some(Family::Blobs) => (&src_families.blobs, &dst_families.blobs),
        Some(_) => {
            return Err(BackupError::Invariant {
                detail: "row names an unsupported family",
            });
        }
    };

    // The source content is hashed as it streams; the destination receives the same bytes.
    let mut hasher = StreamingSha256::with_prepaid(ledger, row.file_len).map_err(hash_error)?;
    let mut reader = src_fs
        .open_read(src_dir, name)
        .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
    let mut writer = dst_fs
        .create_new(dst_dir, name)
        .map_err(|error| filesystem_error(BackupStep::CreateDestination, error))?;
    let mut chunk = [0u8; CHUNK];
    let mut written = 0u64;
    // NOTE: `with_prepaid` admitted `row.file_len` bytes up front and holds the ledger for the
    // life of the hasher, so this loop must not also borrow it. Control at chunk boundaries is
    // therefore taken through the prepaid `update`, which is the canonical streaming gate.
    loop {
        let read = src_fs
            .read(&mut reader, &mut chunk)
            .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
        if read == 0 {
            break;
        }
        let slice = &chunk[..read];
        hasher.update(slice).map_err(hash_error)?;
        // A positive short write must be finished, bounded by the chunk.
        let mut offset = 0usize;
        while offset < read {
            match dst_fs
                .write(&mut writer, &slice[offset..])
                .map_err(|error| filesystem_error(BackupStep::WriteDestination, error))?
            {
                WriteOutcome { written: 0 } => {
                    return Err(BackupError::Invariant {
                        detail: "zero-length short write",
                    });
                }
                WriteOutcome { written: n } => offset += n,
            }
        }
        written += read as u64;
        if written > row.file_len {
            return Err(BackupError::FileMismatch { tag: row.tag as u8 });
        }
    }
    let actual = hasher.finish().map_err(hash_error)?;
    dst_fs
        .sync_file(&writer)
        .map_err(|error| filesystem_error(BackupStep::SyncDestination, error))?;
    dst_fs
        .sync_entry(dst_dir, name)
        .map_err(|error| filesystem_error(BackupStep::SyncDestination, error))?;

    // Exact length and hash must agree with the plan.
    if written != row.file_len {
        return Err(BackupError::FileMismatch { tag: row.tag as u8 });
    }
    if actual != row.sha256 {
        return Err(BackupError::FileMismatch { tag: row.tag as u8 });
    }
    Ok(())
}

/// Encode the selected version's CURRENT before planning its size and hash.
fn encode_selected_current(
    kb_uuid: KbUuid,
    version: u64,
    manifest_name: EntryName,
    manifest_sha256: [u8; 32],
    ledger: &mut dyn LeaseLedger,
) -> Result<Vec<u8>, BackupError> {
    let name = crate::names::render_name_bounded(manifest_name, ledger)?;
    let current = match sage_db_format::current::CurrentToEncode::new(
        kb_uuid,
        version,
        &name,
        manifest_sha256,
    ) {
        sage_db_types::framing::FormatOutcome::Ok(current) => current,
        sage_db_types::framing::FormatOutcome::Format(_) => {
            return Err(BackupError::Invariant {
                detail: "the selected manifest name is not a canonical CURRENT name",
            });
        }
        sage_db_types::framing::FormatOutcome::Refused(refusal) => {
            return Err(BackupError::Refused(refusal));
        }
    };
    let mut bytes: Vec<u8> = Vec::new();
    match sage_db_format::current::encode_current(&current, &mut bytes, &Limits::V1, ledger) {
        sage_db_types::framing::FormatOutcome::Ok(()) => {}
        sage_db_types::framing::FormatOutcome::Format(_) => {
            return Err(BackupError::Invariant {
                detail: "the canonical CURRENT encoder refused its own input",
            });
        }
        sage_db_types::framing::FormatOutcome::Refused(refusal) => {
            return Err(BackupError::Refused(refusal));
        }
    }
    Ok(bytes)
}

/// Write `bytes` to `name` in `dir`, sync the file and the entry.
fn write_whole<F: FileSystem>(
    fs: &F,
    dir: &F::Dir,
    name: EntryName,
    bytes: &[u8],
    step: BackupStep,
    ledger: &mut dyn LeaseLedger,
) -> Result<(), BackupError> {
    let mut writer = fs
        .create_new(dir, name)
        .map_err(|error| filesystem_error(step, error))?;
    let mut offset = 0usize;
    while offset < bytes.len() {
        ledger
            .checkpoint()
            .map_err(|stop| BackupError::Refused(Refusal::Stopped(stop)))?;
        let end = offset + CHUNK.min(bytes.len() - offset);
        match fs
            .write(&mut writer, &bytes[offset..end])
            .map_err(|error| filesystem_error(step, error))?
        {
            WriteOutcome { written: 0 } => {
                return Err(BackupError::Invariant {
                    detail: "zero-length short write",
                });
            }
            WriteOutcome { written: n } => offset += n,
        }
    }
    fs.sync_file(&writer)
        .map_err(|error| filesystem_error(step, error))?;
    fs.sync_entry(dir, name)
        .map_err(|error| filesystem_error(step, error))?;
    Ok(())
}

/// Encode the inventory rows into their exact byte form.
fn encode_inventory(
    plan: &ExportPlan,
    ledger: &mut dyn LeaseLedger,
) -> Result<Vec<u8>, BackupError> {
    let rows = plan.rows.len();
    let total = HEADER_BYTES
        .checked_add(
            rows.checked_mul(ROW_BYTES)
                .ok_or(BackupError::ExtentOverflow)?,
        )
        .ok_or(BackupError::ExtentOverflow)?;
    let mut out: Vec<u8> = Vec::new();
    ledger
        .try_charge(total as u64)
        .map_err(|error| BackupError::Refused(Refusal::Budget(error)))?;
    out.try_reserve_exact(total)
        .map_err(|_| BackupError::Allocation { requested: total })?;
    if out.capacity() != total {
        return Err(BackupError::Invariant {
            detail: "inventory byte capacity differs from admitted size",
        });
    }
    out.extend_from_slice(&INVENTORY_MAGIC);
    out.extend_from_slice(plan.kb_uuid.as_bytes());
    out.extend_from_slice(&plan.version.to_le_bytes());
    out.extend_from_slice(&plan.manifest_sha256);
    out.extend_from_slice(&(rows as u64).to_le_bytes());
    for row in &plan.rows {
        ledger
            .checkpoint()
            .map_err(|stop| BackupError::Refused(Refusal::Stopped(stop)))?;
        out.push(row.tag as u8);
        out.extend_from_slice(&row.version.to_le_bytes());
        out.extend_from_slice(&row.id);
        out.extend_from_slice(&row.sha256);
        out.extend_from_slice(&row.file_len.to_le_bytes());
    }
    if out.len() != total {
        return Err(BackupError::Invariant {
            detail: "inventory length disagreed with its own arithmetic",
        });
    }
    Ok(out)
}

/// Encode the completion marker: magic, inventory digest, inventory length.
fn encode_complete(
    inventory_sha: [u8; 32],
    inventory_len: u64,
) -> Result<[u8; COMPLETE_BYTES], BackupError> {
    let mut out = [0u8; COMPLETE_BYTES];
    out[0..8].copy_from_slice(&COMPLETE_MAGIC);
    out[8..40].copy_from_slice(&inventory_sha);
    out[40..48].copy_from_slice(&inventory_len.to_le_bytes());
    Ok(out)
}

/// The canonical raw SHA-256 of `bytes`, through the format's own bounded helper.
fn raw_sha(bytes: &[u8], ledger: &mut dyn LeaseLedger) -> Result<[u8; 32], BackupError> {
    sage_db_format::digest::raw_sha256_bounded(bytes, ledger).map_err(|error| match error {
        sage_db_format::digest::BoundedHashError::Budget(budget) => {
            BackupError::Refused(Refusal::Budget(budget))
        }
        sage_db_format::digest::BoundedHashError::Stopped(stop) => {
            BackupError::Refused(Refusal::Stopped(stop))
        }
    })
}

fn filesystem_error(step: BackupStep, error: FsError) -> BackupError {
    BackupError::Filesystem { step, error }
}

/// Preserve a destination-preparation refusal instead of flattening it.
///
/// A non-empty or already-initialized root is a FRESHNESS verdict; a filesystem refusal keeps its
/// `Effect` and kind; a budget/stop keeps its canonical payload.
fn map_init(error: crate::init::InitRefusal) -> BackupError {
    use crate::init::InitRefusal;
    match error {
        InitRefusal::NotEmpty | InitRefusal::AlreadyInitialized => BackupError::DestinationExists,
        InitRefusal::Filesystem { step: _, error } => BackupError::Filesystem {
            step: BackupStep::CreateDestination,
            error,
        },
        InitRefusal::Budget => BackupError::InitializerBudget,
        InitRefusal::Stopped(stop) => BackupError::Refused(Refusal::Stopped(stop)),
    }
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// Decode an inventory's rows from its exact bytes, validating every field and the ordering.
fn decode_inventory(
    bytes: &[u8],
    max_files: u64,
    ledger: &mut dyn LeaseLedger,
) -> Result<(KbUuid, u64, [u8; 32], Vec<Row>), BackupError> {
    if bytes.len() < HEADER_BYTES {
        return Err(BackupError::InventoryCorrupt(InventoryDefect::Length));
    }
    if bytes[0..8] != INVENTORY_MAGIC {
        return Err(BackupError::InventoryCorrupt(InventoryDefect::Magic));
    }
    let mut kb = [0u8; 16];
    kb.copy_from_slice(&bytes[8..24]);
    let mut version_bytes = [0u8; 8];
    version_bytes.copy_from_slice(&bytes[24..32]);
    let version = u64::from_le_bytes(version_bytes);
    if version == 0 {
        return Err(BackupError::InventoryCorrupt(InventoryDefect::FieldShape));
    }
    let mut manifest_sha = [0u8; 32];
    manifest_sha.copy_from_slice(&bytes[32..64]);
    let mut count_bytes = [0u8; 8];
    count_bytes.copy_from_slice(&bytes[64..72]);
    let count = u64::from_le_bytes(count_bytes);

    // The count must agree with the actual bytes BEFORE any rows or slots are reserved.
    let slots = usize::try_from(count).map_err(|_| BackupError::ExtentOverflow)?;
    let expected = slots
        .checked_mul(ROW_BYTES)
        .and_then(|n| n.checked_add(HEADER_BYTES))
        .ok_or(BackupError::ExtentOverflow)?;
    if expected != bytes.len() {
        return Err(BackupError::InventoryCorrupt(InventoryDefect::Length));
    }
    if count > max_files {
        return Err(BackupError::LimitExceeded(BackupLimit::Files));
    }
    let mut rows: Vec<Row> = Vec::new();
    let extent = slots
        .checked_mul(core::mem::size_of::<Row>())
        .ok_or(BackupError::ExtentOverflow)?;
    ledger
        .try_charge_entries(count)
        .map_err(|error| BackupError::Refused(Refusal::Budget(error)))?;
    ledger
        .try_charge(extent as u64)
        .map_err(|error| BackupError::Refused(Refusal::Budget(error)))?;
    rows.try_reserve_exact(slots)
        .map_err(|_| BackupError::Allocation { requested: extent })?;
    if rows.capacity() != slots {
        return Err(BackupError::Invariant {
            detail: "inventory row capacity differs from admitted count",
        });
    }
    let mut current_rows = 0usize;
    for index in 0..slots {
        let at = HEADER_BYTES + index * ROW_BYTES;
        let tag = RowTag::from_u8(bytes[at])
            .ok_or(BackupError::InventoryCorrupt(InventoryDefect::Tag))?;
        let mut vb = [0u8; 8];
        vb.copy_from_slice(&bytes[at + 1..at + 9]);
        let row_version = u64::from_le_bytes(vb);
        let mut id = [0u8; 16];
        id.copy_from_slice(&bytes[at + 9..at + 25]);
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&bytes[at + 25..at + 57]);
        let mut lb = [0u8; 8];
        lb.copy_from_slice(&bytes[at + 57..at + 65]);
        let file_len = u64::from_le_bytes(lb);

        // Scalar fields must be legal for the tag: no unused field is silently ignored.
        match tag {
            RowTag::Current => {
                if row_version != 0 || id != [0u8; 16] {
                    return Err(BackupError::InventoryCorrupt(InventoryDefect::FieldShape));
                }
                current_rows += 1;
            }
            RowTag::Manifest => {
                if row_version == 0 {
                    return Err(BackupError::InventoryCorrupt(InventoryDefect::FieldShape));
                }
            }
            RowTag::Transaction => {
                // A transaction row carries its own 16-byte identity.
                if row_version != 0 || id == [0u8; 16] {
                    return Err(BackupError::InventoryCorrupt(InventoryDefect::FieldShape));
                }
            }
            RowTag::Blob => {
                // A blob is named by its content hash and carries no separate identity. An
                // EMPTY blob is legal (an empty body has a real SHA), so `file_len == 0` is not
                // rejected for this tag below.
                if row_version != 0 || id != [0u8; 16] || sha == [0u8; 32] {
                    return Err(BackupError::InventoryCorrupt(InventoryDefect::FieldShape));
                }
            }
        }
        // Only the structural files must be non-empty. An empty BLOB is a real, valid object
        // (an empty body), and its hash is the SHA-256 of zero bytes.
        if file_len == 0 && tag != RowTag::Blob {
            return Err(BackupError::InventoryCorrupt(InventoryDefect::EmptyFile));
        }
        ledger
            .checkpoint()
            .map_err(|stop| BackupError::Refused(Refusal::Stopped(stop)))?;
        rows.push(Row {
            tag,
            version: row_version,
            id,
            sha256: sha,
            file_len,
        });
    }
    if current_rows != 1 {
        return Err(BackupError::InventoryCorrupt(InventoryDefect::Current));
    }
    // Canonical order and no duplicate typed name.
    for window in rows.windows(2) {
        ledger
            .try_charge_entries(1)
            .map_err(|error| BackupError::Refused(Refusal::Budget(error)))?;
        ledger
            .checkpoint()
            .map_err(|stop| BackupError::Refused(Refusal::Stopped(stop)))?;
        if window[0].sort_key() >= window[1].sort_key() {
            return Err(BackupError::InventoryCorrupt(InventoryDefect::Order));
        }
    }
    Ok((KbUuid::from_bytes(kb), version, manifest_sha, rows))
}

/// Read `name` from `dir` whole, bounded by `limit`.
fn read_all<F: FileSystem>(
    fs: &F,
    dir: &F::Dir,
    name: EntryName,
    limit: u64,
    ledger: &mut dyn LeaseLedger,
) -> Result<Vec<u8>, BackupError> {
    let size = fs
        .size(dir, name)
        .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
    if size > limit {
        return Err(BackupError::LimitExceeded(BackupLimit::InventoryBytes));
    }
    // `size` is the observed length; the buffer is admitted for EXACTLY that and may not grow.
    let admitted = usize::try_from(size).map_err(|_| BackupError::ExtentOverflow)?;
    let mut out: Vec<u8> = Vec::new();
    ledger
        .try_charge(size)
        .map_err(|error| BackupError::Refused(Refusal::Budget(error)))?;
    out.try_reserve_exact(admitted)
        .map_err(|_| BackupError::Allocation {
            requested: admitted,
        })?;
    if out.capacity() != admitted {
        return Err(BackupError::Invariant {
            detail: "metadata capacity differs from the admitted extent",
        });
    }
    let mut reader = fs
        .open_read(dir, name)
        .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
    let mut chunk = [0u8; CHUNK];
    loop {
        ledger
            .checkpoint()
            .map_err(|stop| BackupError::Refused(Refusal::Stopped(stop)))?;
        let read = fs
            .read(&mut reader, &mut chunk)
            .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
        if read == 0 {
            break;
        }
        // Check the length the buffer WOULD reach BEFORE copying into it, so a file that grew
        // since `size` was observed cannot make this `Vec` grow on unadmitted capacity.
        let next = out
            .len()
            .checked_add(read)
            .ok_or(BackupError::ExtentOverflow)?;
        if next as u64 > size || next > admitted {
            return Err(BackupError::FileMismatch { tag: 0xff });
        }
        out.extend_from_slice(&chunk[..read]);
    }
    if out.len() as u64 != size {
        return Err(BackupError::FileMismatch { tag: 0xff });
    }
    Ok(out)
}

/// Restore a backup into a **fresh** root and return a genuine `Ready` store.
///
/// The completion marker and the inventory are validated, and every listed source file is hashed
/// **before** any destination is created. Routing and authorization are the caller's; this takes
/// an explicit expected KB identity rather than picking one.
///
/// # Errors
///
/// [`BackupError`]: a missing or mismatched completion marker, a corrupt inventory, a file whose
/// length or hash disagrees, an existing destination, a KB identity mismatch, a real
/// ledger/allocator/filesystem refusal, or a destination the canonical replay will not serve.
pub fn restore<F: FileSystem>(
    artifact_fs: &F,
    artifact: &std::path::Path,
    destination_fs: F,
    destination: &std::path::Path,
    expected_kb: KbUuid,
    limits: &BackupLimits,
    ledger: &mut dyn LeaseLedger,
) -> Result<(Store<F>, RestoreReport), BackupError> {
    // 1. Inspect the ARTIFACT (read-only) and validate everything before creating anything.
    let artifact_root = artifact_fs
        .open_root(artifact)
        .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
    // Backends may create LOCK when acquiring it. Confirm the completed
    // artifact has its lock entry before using that operation.
    let lock_name = EntryName::Root(RootName::Lock);
    if artifact_fs
        .size(&artifact_root, lock_name)
        .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?
        != 0
    {
        return Err(BackupError::InventoryCorrupt(InventoryDefect::FieldShape));
    }
    let _source_lock = artifact_fs
        .try_lock_exclusive(&artifact_root)
        .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
    let artifact_families = open_artifact_families(artifact_fs, &artifact_root, ledger)?;

    let complete = match read_all(
        artifact_fs,
        &artifact_root,
        EntryName::Root(RootName::BackupComplete),
        COMPLETE_BYTES as u64 + 1,
        ledger,
    ) {
        Ok(bytes) => bytes,
        // Only a genuine absence means "no completion marker". A budget refusal, a stop or a
        // real filesystem failure keeps its own class.
        Err(BackupError::Filesystem { error, .. }) if error.kind() == FsErrorKind::NotFound => {
            return Err(BackupError::MissingComplete);
        }
        Err(other) => return Err(other),
    };
    if complete.len() != COMPLETE_BYTES || complete[0..8] != COMPLETE_MAGIC {
        return Err(BackupError::CompleteMismatch);
    }
    let mut declared_sha = [0u8; 32];
    declared_sha.copy_from_slice(&complete[8..40]);
    let mut len_bytes = [0u8; 8];
    len_bytes.copy_from_slice(&complete[40..48]);
    let declared_len = u64::from_le_bytes(len_bytes);

    let inventory = read_all(
        artifact_fs,
        &artifact_root,
        EntryName::Root(RootName::BackupInventory),
        limits.max_inventory_bytes,
        ledger,
    )?;
    if inventory.len() as u64 != declared_len {
        return Err(BackupError::CompleteMismatch);
    }
    if raw_sha(&inventory, ledger)? != declared_sha {
        return Err(BackupError::CompleteMismatch);
    }
    let (kb_uuid, version, manifest_sha, rows) =
        decode_inventory(&inventory, limits.max_files, ledger)?;
    if kb_uuid != expected_kb {
        return Err(BackupError::KbMismatch {
            found: kb_uuid,
            expected: expected_kb,
        });
    }

    // 2. Hash every listed source file BEFORE creating the destination.
    let mut total = 0u64;
    for row in &rows {
        let name = row
            .name()
            .ok_or(BackupError::InventoryCorrupt(InventoryDefect::FieldShape))?;
        let dir = artifact_dir(&artifact_families, &artifact_root, row.family());
        let size = match artifact_fs.size(dir, name) {
            Ok(size) => size,
            Err(error) if error.kind() == FsErrorKind::NotFound => {
                return Err(BackupError::FileMismatch { tag: row.tag as u8 });
            }
            Err(error) => return Err(filesystem_error(BackupStep::ReadSource, error)),
        };
        if size != row.file_len {
            return Err(BackupError::FileMismatch { tag: row.tag as u8 });
        }
        if size > limits.max_file_bytes {
            return Err(BackupError::LimitExceeded(BackupLimit::FileBytes));
        }
        total = total.checked_add(size).ok_or(BackupError::ExtentOverflow)?;
        if total > limits.max_total_bytes {
            return Err(BackupError::LimitExceeded(BackupLimit::TotalBytes));
        }
        {
            let mut hasher = StreamingSha256::with_prepaid(ledger, size).map_err(hash_error)?;
            let mut reader = artifact_fs
                .open_read(dir, name)
                .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
            let mut chunk = [0u8; CHUNK];
            let mut hashed = 0u64;
            loop {
                let read = artifact_fs
                    .read(&mut reader, &mut chunk)
                    .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
                if read == 0 {
                    break;
                }
                hasher.update(&chunk[..read]).map_err(hash_error)?;
                hashed += read as u64;
            }
            let actual = hasher.finish().map_err(hash_error)?;
            if hashed != row.file_len {
                return Err(BackupError::FileMismatch { tag: row.tag as u8 });
            }
            // Only a Blob is content-addressed by its own digest; manifest/tx bytes are hashed
            // against the row's recorded file hash.
            if actual != row.sha256 {
                return Err(BackupError::FileMismatch { tag: row.tag as u8 });
            }
        }
    }

    let artifact_observed = crate::open::observe_root(
        artifact_fs,
        &artifact_root,
        &artifact_families,
        &Limits::V1,
        ledger,
    )
    .map_err(BackupError::Replay)?;
    if artifact_observed.version() != version
        || artifact_observed.current.kb_uuid != kb_uuid
        || artifact_observed.current.manifest_sha256 != manifest_sha
    {
        return Err(BackupError::InventoryCorrupt(InventoryDefect::FieldShape));
    }

    // 3. Fresh destination only, under one continuous held lock.
    let fresh = crate::init::prepare_new(&destination_fs, destination, ledger).map_err(map_init)?;
    let (root_dir, lock) = fresh.into_parts();
    let families = open_destination_families(&destination_fs, &root_dir, ledger)?;

    for row in &rows {
        copy_row(
            artifact_fs,
            &artifact_root,
            &artifact_families,
            &destination_fs,
            &root_dir,
            &families,
            row,
            ledger,
        )?;
    }
    sync_layout(&destination_fs, &root_dir, &families, ledger)?;

    // 4. Open through the CANONICAL replay path under the SAME held lock. `resolve_observed` is
    //    the one implementation that replays, publishes a tagged view, syncs the chain and
    //    promotes to Durable; a restored root is exactly an observed root here.
    let observed =
        crate::open::observe_root(&destination_fs, &root_dir, &families, &Limits::V1, ledger)
            .map_err(BackupError::Replay)?;
    if observed.basis() != artifact_observed.basis() {
        return Err(BackupError::Invariant {
            detail: "restored CURRENT does not describe the backup",
        });
    }
    match crate::handle::resolve_observed(
        destination_fs,
        root_dir,
        families,
        lock,
        observed,
        &Limits::V1,
        ledger,
    )
    .map_err(BackupError::Replay)?
    {
        crate::open::Opened::Ready(store) => {
            let basis = store
                .view()
                .ok_or(BackupError::Invariant {
                    detail: "a Ready store must publish a view",
                })?
                .basis();
            Ok((
                store,
                RestoreReport {
                    kb_uuid,
                    version,
                    files: rows.len() as u64,
                    basis,
                },
            ))
        }
        // Verified but not durable: NOT a usable restore, and the handle is dropped rather than
        // handed out as if it were Ready.
        crate::open::Opened::ReadOnly { .. } => Err(BackupError::Invariant {
            detail: "restored root did not come back Durable",
        }),
    }
}

fn sync_layout<F: FileSystem>(
    fs: &F,
    root: &F::Dir,
    families: &Families<F>,
    ledger: &mut dyn LeaseLedger,
) -> Result<(), BackupError> {
    for dir in [
        &families.manifests,
        &families.transactions,
        &families.blobs,
        root,
    ] {
        ledger
            .checkpoint()
            .map_err(|stop| BackupError::Refused(Refusal::Stopped(stop)))?;
        fs.sync_dir(dir)
            .map_err(|error| filesystem_error(BackupStep::SyncDestination, error))?;
    }
    Ok(())
}

/// Open the artifact's families WITHOUT creating anything.
///
/// The artifact is read-only here: this function passes `CreateDir::Never` for every family, so a
/// malformed artifact cannot be initialized by opening a family. The caller holds its lock.
fn open_artifact_families<F: FileSystem>(
    fs: &F,
    root: &F::Dir,
    ledger: &mut dyn LeaseLedger,
) -> Result<Families<F>, BackupError> {
    let staging = fs
        .open_family(root, Family::Staging, CreateDir::Never)
        .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
    let blobs = fs
        .open_family(root, Family::Blobs, CreateDir::Never)
        .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
    let transactions = fs
        .open_family(root, Family::Transactions, CreateDir::Never)
        .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
    let manifests = fs
        .open_family(root, Family::Manifests, CreateDir::Never)
        .map_err(|error| filesystem_error(BackupStep::ReadSource, error))?;
    ledger
        .checkpoint()
        .map_err(|stop| BackupError::Refused(Refusal::Stopped(stop)))?;
    Ok(Families {
        staging,
        blobs,
        transactions,
        manifests,
    })
}

/// The directory a row's file lives in, for the artifact side.
fn artifact_dir<'a, F: FileSystem>(
    families: &'a Families<F>,
    root: &'a F::Dir,
    family: Option<Family>,
) -> &'a F::Dir {
    match family {
        None => root,
        Some(Family::Manifests) => &families.manifests,
        Some(Family::Transactions) => &families.transactions,
        _ => &families.blobs,
    }
}

/// Map a bounded-hash refusal to its canonical class, verbatim.
fn hash_error(error: sage_db_format::digest::BoundedHashError) -> BackupError {
    match error {
        sage_db_format::digest::BoundedHashError::Budget(budget) => {
            BackupError::Refused(Refusal::Budget(budget))
        }
        sage_db_format::digest::BoundedHashError::Stopped(stop) => {
            BackupError::Refused(Refusal::Stopped(stop))
        }
    }
}

/// Order rows canonically, charging one entry and one control check per comparison.
///
/// Insertion sort: `O(n²)` comparisons, which for one KB's committed file count is acceptable and
/// is the price of making the ordering chargeable and interruptible. Not a performance claim.
fn attended_sort_rows(rows: &mut [Row], ledger: &mut dyn LeaseLedger) -> Result<(), OpenRefusal> {
    for index in 1..rows.len() {
        let mut position = index;
        while position > 0 {
            ledger
                .try_charge_entries(1)
                .map_err(|error| OpenRefusal::Refused(Refusal::Budget(error)))?;
            ledger
                .checkpoint()
                .map_err(|stop| OpenRefusal::Refused(Refusal::Stopped(stop)))?;
            if rows[position - 1].sort_key() <= rows[position].sort_key() {
                break;
            }
            rows.swap(position - 1, position);
            position -= 1;
        }
    }
    Ok(())
}
