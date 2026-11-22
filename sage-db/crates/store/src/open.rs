//! Opening an existing root: acquisition, observation, verification, replay, resolution.
//!
//! Everything here derives from **one** observation of CURRENT per attempt. A later attempt may observe again, but no attempt
//! mixes two observations, and nothing latches to a pre-write hash, a highest manifest, orphan files or a previous `Arc`.
//!
//! # Ordering (the contract's, not an implementation preference)
//!
//! observe → validate chain and files → full integrity replay from Genesis → publish tagged `Unverified` → whole-chain sync
//! → `Ready`. Replay comes **before** sync because replay is what decides whether the observed bytes are even coherent; syncing
//! a chain whose old link is false would make durability evidence for something that was never valid.

use crate::fs::{FileSystem, FsError, FsErrorKind};
use crate::lease::LeaseLedger;
use crate::names::EntryName;
use crate::view::{TaggedView, ViewBasis, ViewDurability};
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};
use sage_db_types::identity::{KbUuid, KbVersion};

/// Why an open could not produce a handle.
///
/// Each variant keeps its own remedy: this is the whole point of not collapsing them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OpenRefusal {
    /// The root does not exist, or exists but holds nothing but the lock file.
    ///
    /// Opening never creates a root — `init` is the only creator.
    NotInitialized,
    /// The root is non-empty but CURRENT is absent.
    ///
    /// This is corruption, not a fresh root: a torn or damaged directory must not be adopted as new.
    MissingCurrentOnNonEmpty,
    /// Chain or byte verification failed.
    Corrupt(CorruptReason),
    /// The canonical format layer rejected the bytes, with ITS OWN four-class verdict preserved.
    Format(sage_db_types::framing::FormatError),
    /// A real format or ceiling the Store does not support.
    Unsupported(UnsupportedReason),
    /// Replay or verification ran out of budget, or the caller stopped.
    Refused(Refusal),
    /// A seam operation failed. `FsError` carries `Effect` intact.
    Filesystem {
        /// Which acquisition/verification step.
        step: OpenStep,
        /// The seam's own error.
        error: FsError,
    },
    /// The backend cannot honour the durability contract.
    NotEligible(EligibilityReason),
    /// `expected_kb` did not match the observed root.
    IdentityMismatch {
        /// What the caller expected.
        expected: KbUuid,
        /// What the root holds.
        found: KbUuid,
    },
    /// A derivation helper refused, retaining its local verdict.
    Derivation(crate::commit::CommitRefusal),
    /// A digest assembly helper found a local structural defect.
    Assembly(sage_db_format::state::DigestAssemblyError),
    /// Canonical record-set construction failed with its original ordering verdict.
    RecordSet(sage_db_types::persist::RecordSetRejection),
    /// A retained extent could not be represented.
    ExtentOverflow,
    /// The root and one of its families live on different devices.
    ///
    /// Atomic rename and directory-sync ordering cannot be honoured across devices, so this is refused as its own reason
    /// rather than surfacing later as an unexplained rename failure.
    CrossDevice {
        /// Family whose actual device differs from the root.
        family: crate::fs::Family,
        /// Observed root device identity.
        root_device: u64,
        /// Observed family device identity.
        family_device: u64,
    },
    /// An existing immutable file violated the seam's exact-read contract.
    ReadLengthMismatch {
        /// Size reported by the immutable-file stat.
        expected: usize,
        /// Bytes actually returned by the exact-read seam.
        actual: usize,
    },
    /// Pin layout or retained-credit ownership failed; no allocation failure is invented.
    PinLayout,
    /// Retained pin payload was not admitted by its caller.
    PinTransfer(crate::lease::TransferError),
    /// The framing-metadata owner's layout could not be represented.
    HistoryLayout,
    /// Framing-buffer admission was not available to transfer.
    HistoryTransfer(crate::lease::TransferError),
    /// The derived framing/record cache disagreed with its validated publication basis.
    HistoryFrameMismatch,
    /// Readiness was requested for a different publication basis.
    PublicationBasisMismatch,
    /// A canonical failure mapper unexpectedly produced success: a local invariant defect.
    CanonicalMapperInvariant,
    /// A requested slot array has no representable Rust layout.
    BufferLayout,
    /// A supposedly validated observation violated its required transaction-name invariant.
    InvalidObservation,
    /// Complete physical replay did not materialize the required rule configuration.
    MissingRulesState,
    /// A local fold/ownership defect, with its original quantities.
    Fold(crate::fold::FoldDefect),
}

/// Why this backend cannot provide the required product semantics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EligibilityReason {
    /// An actual required capability is absent.
    MissingCapability(crate::fs::PlatformCapability),
    /// The backend does not promise durable synchronization.
    DurableSyncUnavailable,
}

/// Which step of acquisition or verification failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenStep {
    /// Synchronizing this exact named file on the observed root.
    SyncFile {
        /// Fixed family, or None for the root.
        family: Option<crate::fs::Family>,
        /// Exact canonical file name.
        name: EntryName,
    },
    /// Synchronizing this directory's namespace.
    SyncDirectory(Option<crate::fs::Family>),
    /// Opening the root directory itself.
    OpenRoot,
    /// Taking the exclusive lock.
    Lock,
    /// Reading CURRENT.
    ReadCurrent,
    /// Decoding CURRENT.
    DecodeCurrent,
    /// Reading a manifest.
    ReadManifest,
    /// Reading a transaction.
    ReadTransaction,
    /// Hashing a blob or file.
    Hash,
    /// Syncing during resolution.
    Sync,
    /// Opening one of the fixed families on an existing root.
    OpenManifestFamily,
    /// Comparing device identity of the root and its families.
    DeviceProbe,
}

/// Why the observed content is corrupt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CorruptReason {
    /// A repeated request key contradicts committed idempotency history.
    DuplicateOutcome {
        /// Version of the first committed result.
        original_version: u64,
        /// Version attempting to reuse that request key.
        duplicate_version: u64,
    },
    /// Two persisted images reuse a revision.
    HistoryCollision,
    /// Transaction header facts contradict their named manifest.
    HeaderManifestMismatch,
    /// The selected rule configuration disagrees with this version's rules binding.
    RulesBindingMismatch,
    /// Recomputed canonical state differs from the manifest's claim.
    StateDigestMismatch,
    /// Recomputed canonical graph differs from the manifest's claim.
    GraphDigestMismatch,
    /// Recomputed cumulative bookkeeping differs from the manifest's claim.
    BookkeepingDigestMismatch,
    /// A prior corrupt diagnosis remains terminal.
    TerminalCorrupt,
    /// CURRENT names a manifest whose bytes do not hash to the recorded digest.
    ManifestHashMismatch,
    /// A manifest's predecessor link does not match its predecessor's actual digest.
    ChainLinkMismatch,
    /// A manifest's recorded transaction file hash does not match the file.
    TxFileHashMismatch,
    /// A named committed transaction is absent.
    MissingTransaction,
    /// A named committed manifest is absent.
    MissingManifest,
    /// A named committed blob is absent.
    MissingBlob,
    /// A referenced blob is missing or does not hash as recorded.
    BlobMismatch,
    /// A recomputed digest disagrees with the manifest.
    DigestMismatch,
    /// A replayed version's revision continuity was broken.
    RevisionDiscontinuity,
    /// A manifest's version is not the predecessor's successor.
    VersionDiscontinuity,
    /// A prior link is false even though the tail is self-consistent.
    HistoricalLinkFalse,
}

/// Why the observed content is unsupported rather than corrupt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnsupportedReason {
    /// Unsupported actual rules profile.
    RulesProfile(u16),
    /// The dispatcher did not provide an owned payload for this registered family.
    UnmaterializedEventFamily(sage_db_format::dispatch::CodecFamily),
    /// A format version this Store does not implement.
    FormatVersion,
    /// A file kind that does not belong where it was found.
    FileKind,
    /// The file exceeds the ceiling this Store will read at all.
    ///
    /// A ceiling is NOT an unsupported format version: the format may be perfectly well understood, and the same bytes would
    /// open if the configured limit were larger. Collapsing the two would tell an operator to upgrade the format when the real
    /// remedy is a bigger limit.
    FileCeiling {
        /// Bytes the file actually occupies.
        size: u64,
        /// The ceiling this Store enforces.
        ceiling: u64,
    },
}

/// One manifest row, retaining every fact a later comparison needs.
///
/// Owned scalars and hashes only: no reference into the reusable read buffer survives this struct, so a later stage can compare
/// without re-reading and without borrowing freed bytes.
#[derive(Clone, Copy, Debug)]
pub struct ChainRow {
    /// Version this manifest describes.
    pub version: u64,
    /// SHA-256 of this manifest FILE (its identity on disk).
    pub manifest_sha256: [u8; 32],
    /// This manifest's own canonical typed name, retained so the sync plan and any re-read address the exact file.
    pub manifest_name: EntryName,
    /// `manifest.prev_manifest_sha256`.
    pub prev_manifest_sha256: Option<[u8; 32]>,
    /// `manifest.tx_file_sha256` — expected transaction FILE identity.
    pub tx_file_sha256: [u8; 32],
    /// The exact transaction this manifest names (`ManifestHeader.tx_file_name`), parsed to its canonical typed name.
    ///
    /// Retained as a typed name so nothing later enumerates the family to rediscover which committed file this version owns.
    pub tx_name: EntryName,
    /// KB this version belongs to.
    pub kb_uuid: KbUuid,
    /// `manifest.created_at`.
    pub created_at: i64,
    /// `manifest.rules_profile`.
    pub rules_profile: u16,
    /// `manifest.rules_digest`.
    pub rules_digest: [u8; 32],
    /// `manifest.state_digest`.
    pub state_digest: [u8; 32],
    /// `manifest.graph_digest`.
    pub graph_digest: [u8; 32],
    /// `manifest.chain_digest`.
    pub chain_digest: [u8; 32],
    /// `manifest.bookkeeping_digest`.
    pub bookkeeping_digest: [u8; 32],
}

/// The result of resolving an already-open handle whose authority is undecided.
///
/// Resolution decides about the ROOT that CURRENT names. It does not decide whether any particular unknown REQUEST committed —
/// that needs G0 plus outcome membership in this same validated, replayed and synced root.
#[derive(Debug)]
pub enum Resolution {
    /// CURRENT's root validated, replayed and synced: the handle is Ready and its view is Durable.
    Ready,
    /// Replay completed and verified, but the sync did not succeed. Readable, fenced, tagged `Unverified`.
    VerifiedUnsynced {
        /// Which sync step failed.
        step: OpenStep,
        /// The real cause: a seam failure or a canonical refusal.
        cause: SyncFailure,
    },
    /// Replay could not finish within the caller's budget or was stopped. Fenced, and latest stays absent.
    Incomplete(Refusal),
    /// CURRENT's root failed verification. Terminal for normal serving.
    Corrupt(CorruptReason),
    /// The bytes were a format this Store does not support.
    Unsupported(UnsupportedReason),
    /// The canonical format verdict, including unsupported/limit/internal classes.
    Format(sage_db_types::framing::FormatError),
    /// Another exact local/acquisition failure; never renamed corruption.
    Failed(OpenRefusal),
    /// A seam operation failed; `FsError`/`Effect` intact.
    Filesystem {
        /// Which step failed.
        step: OpenStep,
        /// The seam's error.
        error: FsError,
    },
}

/// A public outcome lookup, gated on a durable basis.
#[derive(Debug)]
pub enum OutcomeLookup {
    /// The request is known on this durable basis, with its ORIGINAL version and payload.
    Found {
        /// The original outcome payload, shared rather than copied.
        outcome: crate::commit::SharedOutcome,
        /// The version at which it was first committed.
        original_version: u64,
        /// The durable basis that answers it.
        basis: ViewBasis,
    },
    /// Complete coverage of this durable basis, and no such request.
    Absent {
        /// The durable basis that answers it.
        basis: ViewBasis,
    },
    /// Cannot answer definitively.
    Incomplete(OutcomeIncomplete),
}

/// Why an outcome lookup could not answer definitively.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutcomeIncomplete {
    /// No current view at all (attempted-authority unknown, or nothing materialized).
    NoCurrentView,
    /// A view exists but is not durable: membership was never made durable.
    NotDurable,
    /// The outcome index does not cover all committed history.
    CoverageIncomplete,
}

/// The single observation of a root for one attempt.
pub struct ObservedRoot {
    /// CURRENT's decoded fields.
    pub current: sage_db_format::current::DecodedCurrent,
    /// SHA-256 of the CURRENT file itself.
    pub current_sha256: [u8; 32],
    /// Verified chain rows, tail → genesis.
    pub chain: Vec<ChainRow>,
    /// The complete historical blob reference union this validation/replay observed.
    ///
    /// Built during full validation, not from a tail-only or current-only view: a blob referenced by any committed version
    /// belongs in the durability set.
    pub referenced_blobs: Vec<[u8; 32]>,
}

impl ObservedRoot {
    /// The version CURRENT names.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.current.version
    }

    /// The basis a view of this observation carries.
    #[must_use]
    pub fn basis(&self) -> ViewBasis {
        ViewBasis {
            kb_uuid: self.current.kb_uuid,
            version: KbVersion::new(self.current.version),
            manifest_sha256: self.current.manifest_sha256,
        }
    }
}

// ---------------------------------------------------------------------------
// Acquisition
// ---------------------------------------------------------------------------

/// An opened existing root, ready or readable-but-fenced.
///
/// Two shapes only, because those are the two real outcomes after a completed replay: a durable root (`Ready`) or a root whose
/// sync has not succeeded (`ReadOnly`). The wrapper does **not** duplicate mutable head/durability state — that lives in the
/// `Store`, and `Opened` delegates.
pub enum Opened<F: FileSystem> {
    /// Replay completed and the whole chain synced: the root is durable.
    Ready(super::handle::Store<F>),
    /// Replay completed but sync did not succeed. Readable, writes fenced, view tagged `Unverified`.
    ///
    /// The cause is carried as itself: a seam failure keeps its `FsError`/`Effect`, and a bounded sync loop that ran out of
    /// budget or was stopped keeps its canonical `Refusal`. Neither is fabricated into the other, and completed replay is not
    /// reported as an incomplete one.
    ReadOnly {
        /// The fenced handle.
        store: super::handle::Store<F>,
        /// Which sync step failed.
        step: OpenStep,
        /// Why the sync did not succeed.
        cause: SyncFailure,
    },
}

/// Why a completed-replay sync did not succeed.
#[derive(Debug)]
pub enum SyncFailure {
    /// A seam operation failed; `FsError` carries `Effect` intact.
    Filesystem(FsError),
    /// A bounded sync loop was refused by the same ledger/control the caller passed.
    Refused(Refusal),
}

impl<F: FileSystem> Opened<F> {
    /// The durability of the root this open produced.
    #[must_use]
    pub fn durability(&self) -> ViewDurability {
        match self {
            Self::Ready(_) => ViewDurability::Durable,
            Self::ReadOnly { .. } => ViewDurability::Unverified,
        }
    }

    /// The current view, delegating to the `Store`'s single accessor.
    #[must_use]
    pub fn view(&self) -> Option<TaggedView> {
        match self {
            Self::Ready(store) | Self::ReadOnly { store, .. } => store.view(),
        }
    }

    /// Borrow the handle.
    #[must_use]
    pub fn store(&self) -> &super::handle::Store<F> {
        match self {
            Self::Ready(store) | Self::ReadOnly { store, .. } => store,
        }
    }

    /// Consume into the handle.
    #[must_use]
    pub fn into_store(self) -> super::handle::Store<F> {
        match self {
            Self::Ready(store) | Self::ReadOnly { store, .. } => store,
        }
    }
}

/// Capability and durability eligibility, with a real reason on refusal.
fn check_eligible<F: FileSystem>(fs: &F) -> Result<(), OpenRefusal> {
    let caps = fs.capabilities();
    if let Some(missing) = caps.missing_required() {
        return Err(OpenRefusal::NotEligible(
            EligibilityReason::MissingCapability(missing),
        ));
    }
    if !caps.durable_sync {
        return Err(OpenRefusal::NotEligible(
            EligibilityReason::DurableSyncUnavailable,
        ));
    }
    Ok(())
}

/// Admit a table's actual element slots before an exact fallible reserve.
pub(crate) fn reserve_slots<T>(
    out: &mut Vec<T>,
    needed: usize,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> Result<(), OpenRefusal> {
    ledger
        .checkpoint()
        .map_err(|s| OpenRefusal::Refused(Refusal::Stopped(s)))?;
    if needed <= out.capacity() {
        return Ok(());
    }
    core::alloc::Layout::array::<T>(needed).map_err(|_| OpenRefusal::BufferLayout)?;
    let growth = needed
        .checked_sub(out.capacity())
        .ok_or(OpenRefusal::ExtentOverflow)?;
    let bytes = growth
        .checked_mul(core::mem::size_of::<T>())
        .ok_or(OpenRefusal::ExtentOverflow)?;
    let bytes64 = u64::try_from(bytes).map_err(|_| OpenRefusal::ExtentOverflow)?;
    ledger
        .try_charge(bytes64)
        .map_err(|e| OpenRefusal::Refused(Refusal::Budget(e)))?;
    out.try_reserve_exact(needed - out.len())
        .map_err(|_| OpenRefusal::Refused(Refusal::Allocation { requested: bytes }))?;
    Ok(())
}

/// Read exact bytes using a bounded admitted reusable frame.
pub(crate) fn read_whole<F: FileSystem>(
    fs: &F,
    dir: &F::Dir,
    name: EntryName,
    buf: &mut Vec<u8>,
    step: OpenStep,
    max_len: u64,
    ledger: &mut dyn LeaseLedger,
) -> Result<(), OpenRefusal> {
    ledger
        .try_charge_entries(1)
        .map_err(|e| OpenRefusal::Refused(Refusal::Budget(e)))?;
    ledger
        .checkpoint()
        .map_err(|s| OpenRefusal::Refused(Refusal::Stopped(s)))?;
    let size = fs.size(dir, name).map_err(|error| {
        if error.kind() == FsErrorKind::NotFound {
            match step {
                OpenStep::ReadTransaction => {
                    return OpenRefusal::Corrupt(CorruptReason::MissingTransaction);
                }
                OpenStep::ReadManifest => {
                    return OpenRefusal::Corrupt(CorruptReason::MissingManifest);
                }
                _ => {}
            }
        }
        fs_failure(step, error)
    })?;
    if size > max_len {
        return Err(OpenRefusal::Unsupported(UnsupportedReason::FileCeiling {
            size,
            ceiling: max_len,
        }));
    }
    let size = usize::try_from(size).map_err(|_| OpenRefusal::ExtentOverflow)?;
    buf.clear();
    reserve_slots(buf, size, ledger)?;
    buf.resize(size, 0);
    let actual = fs
        .read_exact_into(dir, name, buf)
        .map_err(|error| fs_failure(step, error))?;
    if actual != size {
        return Err(OpenRefusal::ReadLengthMismatch {
            expected: size,
            actual,
        });
    }
    Ok(())
}

/// Preserve the canonical mapping for transport errors.
pub(crate) fn from_read_error(error: sage_db_format::wire::ReadError) -> OpenRefusal {
    match sage_db_format::dispatch::from_read_error::<()>(error) {
        FormatOutcome::Format(error) => OpenRefusal::Format(error),
        FormatOutcome::Refused(refusal) => OpenRefusal::Refused(refusal),
        FormatOutcome::Ok(()) => OpenRefusal::CanonicalMapperInvariant,
    }
}

impl OpenRefusal {
    /// Only a real corruption verdict terminally invalidates normal serving.
    pub(crate) fn is_corrupt(&self) -> bool {
        matches!(
            self,
            Self::Corrupt(_)
                | Self::MissingCurrentOnNonEmpty
                | Self::Format(sage_db_types::framing::FormatError::Corrupt(_))
        )
    }
}

// ---------------------------------------------------------------------------
// Name parsing
// ---------------------------------------------------------------------------

/// Parse a validated contract manifest/transaction name into its canonical `EntryName`.
///
/// The base only ever *renders* these names; replay has to go the other way, so this is the one Store-local inverse. It runs
/// only on names the format layer already validated (`is_valid_manifest_name` / the transaction grammar), and re-checks the
/// shape so a slice never indexes past its input.
fn manifest_entry_name(name: &str) -> Option<EntryName> {
    let rest = name.strip_prefix('v')?.strip_suffix(".sgm")?;
    let dash = rest.find('-')?;
    let (version, id) = (&rest[..dash], &rest[dash + 1..]);
    let version: u64 = version.parse().ok()?;
    if version == 0 || id.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    let hex = id.as_bytes();
    for index in 0..16 {
        let hi = (hex[index * 2] as char).to_digit(16)?;
        let lo = (hex[index * 2 + 1] as char).to_digit(16)?;
        bytes[index] = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(EntryName::Manifest {
        version,
        id: crate::names::Id128::from_bytes(bytes),
    })
}

/// The exact transaction name a manifest records, parsed to its canonical typed form.
///
/// Public within the crate because `ChainRow` retains it and replay reads by it.
pub(crate) fn transaction_entry_name(name: &str) -> Option<EntryName> {
    let rest = name.strip_prefix("tx-")?.strip_suffix(".sgl")?;
    if rest.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    let hex = rest.as_bytes();
    for index in 0..16 {
        let hi = (hex[index * 2] as char).to_digit(16)?;
        let lo = (hex[index * 2 + 1] as char).to_digit(16)?;
        bytes[index] = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(EntryName::Transaction(crate::names::Id128::from_bytes(
        bytes,
    )))
}

// ---------------------------------------------------------------------------
// Observation and chain verification
// ---------------------------------------------------------------------------

/// Observe CURRENT and verify the whole manifest chain, tail → genesis.
///
/// This is the ONE observation for an attempt: it reads CURRENT once, hashes it, then walks the chain by following each
/// manifest's recorded predecessor. Orphan files are never consulted, and no highest-version scan happens anywhere.
///
/// # Errors
///
/// [`OpenRefusal`] with the class that matches the actual failure.
pub fn observe_root<F: FileSystem>(
    fs: &F,
    root: &F::Dir,
    families: &crate::commit::Families<F>,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<ObservedRoot, OpenRefusal> {
    let mut current_bytes = Vec::new();
    match read_whole(
        fs,
        root,
        EntryName::Root(crate::names::RootName::Current),
        &mut current_bytes,
        OpenStep::ReadCurrent,
        limits.max_current_file_bytes,
        ledger,
    ) {
        Err(OpenRefusal::Filesystem { error, .. }) if error.kind() == FsErrorKind::NotFound => {
            return Err(OpenRefusal::MissingCurrentOnNonEmpty);
        }
        other => other?,
    }
    let current_sha256 = sage_db_format::digest::raw_sha256_bounded(&current_bytes, ledger)
        .map_err(refusal_from_hash)?;
    let current = match sage_db_format::current::decode_current(&current_bytes, limits, ledger) {
        FormatOutcome::Ok(c) => c,
        FormatOutcome::Format(e) => return Err(OpenRefusal::Format(e)),
        FormatOutcome::Refused(r) => return Err(OpenRefusal::Refused(r)),
    };
    let mut next = manifest_entry_name(current.manifest_name())
        .ok_or(OpenRefusal::Corrupt(CorruptReason::ChainLinkMismatch))?;
    let mut expected_sha = current.manifest_sha256;
    let mut expected_version = current.version;
    let mut chain = Vec::new();
    let mut scratch = Vec::new();
    loop {
        ledger
            .try_charge_entries(1)
            .map_err(|e| OpenRefusal::Refused(Refusal::Budget(e)))?;
        ledger
            .checkpoint()
            .map_err(|s| OpenRefusal::Refused(Refusal::Stopped(s)))?;
        read_whole(
            fs,
            &families.manifests,
            next,
            &mut scratch,
            OpenStep::ReadManifest,
            limits.max_manifest_file_bytes,
            ledger,
        )?;
        let actual = sage_db_format::digest::raw_sha256_bounded(&scratch, ledger)
            .map_err(refusal_from_hash)?;
        if actual != expected_sha {
            return Err(OpenRefusal::Corrupt(CorruptReason::ManifestHashMismatch));
        }
        let summary = match sage_db_format::manifest::read_manifest(&scratch, limits, ledger) {
            FormatOutcome::Ok(m) => m,
            FormatOutcome::Format(e) => return Err(OpenRefusal::Format(e)),
            FormatOutcome::Refused(r) => return Err(OpenRefusal::Refused(r)),
        };
        let header = summary.header;
        if header.kb_uuid != current.kb_uuid {
            return Err(OpenRefusal::IdentityMismatch {
                expected: current.kb_uuid,
                found: header.kb_uuid,
            });
        }
        if header.version != expected_version {
            return Err(OpenRefusal::Corrupt(CorruptReason::VersionDiscontinuity));
        }
        if !matches!(next, EntryName::Manifest { version, .. } if version == header.version) {
            return Err(OpenRefusal::Corrupt(CorruptReason::VersionDiscontinuity));
        }
        let tx_name = transaction_entry_name(header.tx_file_name)
            .ok_or(OpenRefusal::Corrupt(CorruptReason::TxFileHashMismatch))?;
        let previous = match (header.prev_manifest_name, header.prev_manifest_sha256) {
            (None, None) if header.version == 1 => None,
            (Some(name), Some(sha)) if header.version > 1 => Some((
                manifest_entry_name(name)
                    .ok_or(OpenRefusal::Corrupt(CorruptReason::ChainLinkMismatch))?,
                sha,
            )),
            _ => return Err(OpenRefusal::Corrupt(CorruptReason::ChainLinkMismatch)),
        };
        let row = ChainRow {
            version: header.version,
            manifest_sha256: actual,
            manifest_name: next,
            prev_manifest_sha256: header.prev_manifest_sha256,
            tx_file_sha256: header.tx_file_sha256,
            tx_name,
            kb_uuid: header.kb_uuid,
            created_at: header.created_at,
            rules_profile: header.rules_profile,
            rules_digest: header.rules_digest,
            state_digest: header.state_digest,
            graph_digest: header.graph_digest,
            chain_digest: header.chain_digest,
            bookkeeping_digest: header.bookkeeping_digest,
        };
        let needed = chain
            .len()
            .checked_add(1)
            .ok_or(OpenRefusal::ExtentOverflow)?;
        reserve_slots(&mut chain, needed, ledger)?;
        chain.push(row);
        match previous {
            Some((name, sha)) => {
                next = name;
                expected_sha = sha;
                expected_version = expected_version
                    .checked_sub(1)
                    .ok_or(OpenRefusal::ExtentOverflow)?;
            }
            None => break,
        }
    }
    Ok(ObservedRoot {
        current,
        current_sha256,
        chain,
        referenced_blobs: Vec::new(),
    })
}

/// Map a bounded-hash refusal onto an open refusal, keeping the canonical payload.
pub(crate) fn refusal_from_hash(error: sage_db_format::digest::BoundedHashError) -> OpenRefusal {
    match error {
        sage_db_format::digest::BoundedHashError::Budget(error) => {
            OpenRefusal::Refused(Refusal::Budget(error))
        }
        sage_db_format::digest::BoundedHashError::Stopped(stop) => {
            OpenRefusal::Refused(Refusal::Stopped(stop))
        }
    }
}

// ---------------------------------------------------------------------------
// Public open
// ---------------------------------------------------------------------------

/// Open an **existing** root: acquire, observe, verify, replay, resolve.
///
/// This never creates anything. It checks eligibility, opens the absolute root, takes the exclusive lock **before** reading any
/// content, opens the four families with `Never`, captures CURRENT once, verifies the chain, replays from Genesis, and then
/// decides durability by syncing the observed chain.
///
/// # Errors
///
/// [`OpenRefusal`], with the class and payload that match the actual failure.
pub fn open_existing<F: FileSystem>(
    fs: F,
    root_path: &std::path::Path,
    expected_kb: Option<KbUuid>,
    limits: &Limits,
    ledger: &mut dyn LeaseLedger,
) -> Result<Opened<F>, OpenRefusal> {
    // 1. Eligibility first: a backend that cannot honour durability must not be read as if it could.
    check_eligible(&fs)?;

    // 2. The root must already exist. `NotFound` here is "never initialized", not corruption.
    let root = match fs.open_root(root_path) {
        Ok(root) => root,
        Err(error) if error.kind() == FsErrorKind::NotFound => {
            return Err(OpenRefusal::NotInitialized);
        }
        Err(error) => {
            return Err(fs_failure(OpenStep::OpenRoot, error));
        }
    };

    // 3. The lock is taken BEFORE any content read, and held for the returned handle's lifetime.
    let lock = fs
        .try_lock_exclusive(&root)
        .map_err(|error| fs_failure(OpenStep::Lock, error))?;

    // 4. Families must already exist; opening never creates one.
    // Decide "never initialized" BEFORE blaming a missing family: a root holding nothing but LOCK has simply never had a
    // KB created in it, and reporting a layout failure there would send an operator looking for damage that does not exist.
    let mut saw_any_entry = false;
    let walk = fs.for_each_entry(&root, ledger, &mut |name| {
        if name == crate::names::RootName::Lock.as_str() {
            return crate::fs::Each::Continue;
        }
        saw_any_entry = true;
        crate::fs::Each::Stop
    });
    match walk {
        Ok(()) => {
            if !saw_any_entry {
                return Err(OpenRefusal::NotInitialized);
            }
        }
        // A traversal that could not complete tells us nothing about emptiness; report the real error rather than guessing.
        Err(error) => {
            return Err(fs_failure(OpenStep::OpenRoot, error));
        }
    }

    let families = match crate::commit::open_families(&fs, &root) {
        Ok(families) => families,
        Err(error) => {
            // A real family is missing on a NON-empty root: that is a layout failure, reported with its own step and error.
            return Err(fs_failure(OpenStep::OpenManifestFamily, error));
        }
    };

    // DEVICE IDENTITY: compare the root against every real family handle. A cross-device layout cannot honour rename
    // atomicity or directory sync ordering, so it must be refused as its own reason rather than discovered later.
    let root_device = fs
        .device_id(&root)
        .map_err(|error| fs_failure(OpenStep::DeviceProbe, error))?;
    for (family, dir) in [
        (crate::fs::Family::Staging, &families.staging),
        (crate::fs::Family::Blobs, &families.blobs),
        (crate::fs::Family::Transactions, &families.transactions),
        (crate::fs::Family::Manifests, &families.manifests),
    ] {
        let device = fs
            .device_id(dir)
            .map_err(|error| fs_failure(OpenStep::DeviceProbe, error))?;
        if device != root_device {
            return Err(OpenRefusal::CrossDevice {
                family,
                root_device,
                family_device: device,
            });
        }
    }

    // 5. ONE observation for this attempt.
    let observed = match observe_root(&fs, &root, &families, limits, ledger) {
        Ok(observed) => observed,
        Err(OpenRefusal::NotInitialized) => {
            // CURRENT absent. On a non-empty root that is corruption, not a fresh root.
            return Err(OpenRefusal::MissingCurrentOnNonEmpty);
        }
        Err(other) => return Err(other),
    };

    // 6. Optional identity check against what the root actually holds.
    if let Some(expected) = expected_kb
        && expected != observed.current.kb_uuid
    {
        return Err(OpenRefusal::IdentityMismatch {
            expected,
            found: observed.current.kb_uuid,
        });
    }

    // 7. Replay + publish + sync.
    crate::handle::resolve_observed(fs, root, families, lock, observed, limits, ledger)
}

/// Read a whole family file through the seam, charging the read to the caller's ledger.
///
/// Same shape as [`read_whole`] but admits the bytes first, so replay does not read large transaction files uncharged.
pub(crate) fn read_whole_strict<F: FileSystem>(
    fs: &F,
    dir: &F::Dir,
    name: EntryName,
    buf: &mut Vec<u8>,
    max_len: u64,
    ledger: &mut dyn LeaseLedger,
) -> Result<(), OpenRefusal> {
    read_whole(
        fs,
        dir,
        name,
        buf,
        OpenStep::ReadTransaction,
        max_len,
        ledger,
    )
}

/// Retain the actual pin refusal class.
pub(crate) fn pin_failure(error: crate::pin::PinError) -> OpenRefusal {
    match error {
        crate::pin::PinError::Budget(e) => OpenRefusal::Refused(Refusal::Budget(e)),
        crate::pin::PinError::Stopped(s) => OpenRefusal::Refused(Refusal::Stopped(s)),
        crate::pin::PinError::Allocate { requested } => match usize::try_from(requested) {
            Ok(requested) => OpenRefusal::Refused(Refusal::Allocation { requested }),
            Err(_) => OpenRefusal::ExtentOverflow,
        },
        crate::pin::PinError::Transfer(error) => OpenRefusal::PinTransfer(error),
        crate::pin::PinError::Layout => OpenRefusal::PinLayout,
        crate::pin::PinError::ExtentOverflow => OpenRefusal::ExtentOverflow,
    }
}

/// Distinguish stored structural defects from local ownership or resource failures.
pub(crate) fn fold_failure(error: crate::fold::FoldDefect) -> OpenRefusal {
    use crate::fold::FoldDefect;
    match error {
        FoldDefect::Stopped(s) => OpenRefusal::Refused(Refusal::Stopped(s)),
        FoldDefect::Outcome(crate::commit::SharedOutcomeError::Refused(r)) => {
            OpenRefusal::Refused(r)
        }
        FoldDefect::RevisionDiscontinuity { .. } => {
            OpenRefusal::Corrupt(CorruptReason::RevisionDiscontinuity)
        }
        FoldDefect::HistoryCollision { .. } => {
            OpenRefusal::Corrupt(CorruptReason::HistoryCollision)
        }
        FoldDefect::DuplicateOutcome {
            original_version,
            duplicate_version,
        } => OpenRefusal::Corrupt(CorruptReason::DuplicateOutcome {
            original_version,
            duplicate_version,
        }),
        other => OpenRefusal::Fold(other),
    }
}

/// Forward real filesystem-carried canonical budget/control payloads.
pub(crate) fn fs_failure(step: OpenStep, error: FsError) -> OpenRefusal {
    if let Some(budget) = error.budget() {
        return OpenRefusal::Refused(Refusal::Budget(budget));
    }
    if let Some(stop) = error.control() {
        return OpenRefusal::Refused(Refusal::Stopped(stop));
    }
    OpenRefusal::Filesystem { step, error }
}

/// Preserve the shared derivation helper's refusal or defect.
pub(crate) fn derivation_failure(error: crate::commit::CommitRefusal) -> OpenRefusal {
    match error {
        crate::commit::CommitRefusal::Refused(r) => OpenRefusal::Refused(r),
        crate::commit::CommitRefusal::Format(e) => OpenRefusal::Format(e),
        crate::commit::CommitRefusal::ExtentOverflow => OpenRefusal::ExtentOverflow,
        other => OpenRefusal::Derivation(other),
    }
}

pub(crate) fn assembly_failure(error: sage_db_format::state::DigestAssemblyError) -> OpenRefusal {
    match error.as_refusal() {
        Some(r) => OpenRefusal::Refused(r),
        None => OpenRefusal::Assembly(error),
    }
}
