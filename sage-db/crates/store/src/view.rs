//! Durability-tagged view of an observed root.
//!
//! A `Store` may serve a root that has been fully replayed and verified but whose whole-chain sync has not yet succeeded.
//! Such a root is **Unverified**: it is real data, but it is not evidence of a durable commit. This module carries that
//! distinction as a value, so no caller has to remember it.
//!
//! Two rules make the tag hard to lose:
//!
//! 1. the legacy accessors (`published_pin`, `retained_entries`) filter the SAME tagged publication for Durable;
//!    they do not expose a separate slot or silently strip an Unverified tag;
//! 2. `TaggedView` is immutable and carries its observed root identity, so a result can never be promoted by accident.

use crate::pin::Snapshot;
use sage_db_types::identity::{KbUuid, KbVersion};

/// Scalar metadata from the one fully replayed durable publication.
/// This is a read result, not permission to bypass the Store's write fence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitBasis {
    /// Exact current root identity.
    pub basis: ViewBasis,
    /// The actual retained rule configuration, not caller configuration.
    pub rules: sage_db_types::prepared::RulesBinding,
    /// Verified length of the referenced rule blob.
    pub rules_len: u64,
    /// Current allocation mark in the space requested by the caller; zero when
    /// that space has never been allocated. The writer still checks collisions.
    pub allocated_high_water: u128,
}

/// Whether ordinary kernel operations may use a root being imported.
/// This is a bounded read of persisted import bookkeeping, not an activation permit.
#[derive(Debug)]
pub enum ImportReadiness<'a> {
    /// There are no unfinished sessions on this definitive durable basis.
    Ready {
        /// The publication inspected.
        basis: ViewBasis,
    },
    /// A persisted session has not set its verified completion marker.
    Incomplete {
        /// The publication inspected.
        basis: ViewBasis,
        /// The actual retained stage record; no identifier or payload is copied.
        session: &'a sage_db_types::persist::ImportSessionRecord,
    },
}

/// Refusal to read the KB's canonical metadata from a durable publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KbMetadataReadError {
    /// The publication is incomplete or unverified.
    Incomplete(crate::open::OutcomeIncomplete),
    /// The caller's ledger refused work or stopped it.
    Refused(sage_db_types::framing::Refusal),
    /// The current index points to an owner that is absent.
    IndexInvariant {
        /// Indexed current revision.
        revision: u64,
    },
    /// The metadata singleton is absent from the published fold.
    Missing,
    /// The record at the metadata identity has a different type or KB identity.
    WrongRecord,
}

/// A metadata read refusal, distinct from a commit result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitReadError {
    /// No definitive durable publication.
    Incomplete(crate::open::OutcomeIncomplete),
    /// The same caller ledger refused work or stopped it.
    Refused(sage_db_types::framing::Refusal),
    /// The published fold has no rule configuration: internal invariant defect.
    MissingRules,
    /// The retained owner contradicts the rule singleton's identity.
    WrongRulesRecord,
    /// A current index names no retained revision owner.
    IndexInvariant {
        /// Current singleton revision whose retained owner is missing.
        revision: u64,
    },
    /// The retained rule configuration has no committed blob-length fact.
    MissingRulesBlob,
}

/// The one retained after-image owner used by current and historical reads.
/// This is an alias, not a second owner or record definition.
pub use crate::history::HistoryImage as RecordImage;

/// An owned current lookup whose payload stays admitted beyond Store/attempt drop.
#[derive(Debug)]
pub enum RecordImageLookup {
    /// The exact current after-image behind its existing shared owner.
    Found {
        /// The retained full typed record.
        image: RecordImage,
        /// Actual durable publication which chose this revision.
        basis: ViewBasis,
    },
    /// Complete coverage establishes absence on this basis.
    Absent {
        /// Actual durable publication.
        basis: ViewBasis,
    },
    /// No definitive durable current answer.
    Incomplete(crate::open::OutcomeIncomplete),
}

/// A bounded identified-record traversal failed; callers must discard partial
/// working results. No successful partial scan is returned.
#[derive(Debug)]
pub enum RecordScanError<E> {
    /// No definitive durable publication is available.
    Incomplete(crate::open::OutcomeIncomplete),
    /// The caller's ledger or a retained index lookup refused the scan.
    Read(RecordReadError),
    /// The visitor refused its own work; preserved without relabelling it.
    Visitor(E),
}

/// Which real operation failed while reading a committed blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobReadStep {
    /// The immutable file's size probe.
    Size,
    /// Its exact byte read.
    Read,
}

/// Confirmed damage to a blob reachable from the committed reference union.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobCorruption {
    /// A referenced file no longer exists.
    Missing,
    /// Its size differs from the verified committed length.
    Length {
        /// Retained committed length.
        expected: u64,
        /// Actual file or returned buffer length.
        actual: u64,
    },
    /// Actual returned bytes do not hash to their content-addressed name.
    HashMismatch,
}

/// A committed blob read error; no successful partial buffer is returned.
#[derive(Debug)]
pub enum BlobReadError {
    /// No definitive durable publication.
    Incomplete(crate::open::OutcomeIncomplete),
    /// The requested digest is not in this publication's committed union.
    NotReferenced([u8; 32]),
    /// A caller-configured ceiling, not corruption or a guessed budget counter.
    Ceiling {
        /// Actual file length.
        size: u64,
        /// Caller ceiling.
        ceiling: u64,
    },
    /// Real file transport failure with FsError/Effect intact.
    Filesystem {
        /// Actual failing operation.
        step: BlobReadStep,
        /// The seam's exact error.
        error: crate::fs::FsError,
    },
    /// The same caller ledger refused work or stopped it.
    Refused(sage_db_types::framing::Refusal),
    /// A buffer length/layout is not representable on this host.
    ExtentOverflow,
    /// The vector reported a capacity different from the admitted exact extent.
    /// No buffer is published under an insufficient retained claim.
    UnexpectedCapacity {
        /// Admitted backing byte extent.
        requested: usize,
        /// Capacity actually reported by the vector.
        actual: usize,
    },
    /// A successful exact-read callback violated its reported size contract.
    ReadLengthMismatch {
        /// Bytes requested from the exact-read seam.
        expected: usize,
        /// Bytes it reported.
        actual: usize,
    },
    /// Confirmed committed damage; normal Store serving has been invalidated.
    Corrupt(BlobCorruption),
}

/// Actual committed blob bytes and the reservation keeping them admitted.
/// Dropping the read attempt does not release bytes still held by this owner.
pub struct CommittedBlob {
    pub(crate) bytes: Vec<u8>,
    pub(crate) retained: crate::lease::OwnedReservation,
    pub(crate) basis: ViewBasis,
}

impl CommittedBlob {
    /// Borrow the verified bytes without copying or shedding their reservation.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Byte length, including a genuinely empty blob.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }
    /// Whether the committed content is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
    /// The durable publication on which this read was admitted.
    #[must_use]
    pub const fn basis(&self) -> ViewBasis {
        self.basis
    }
    /// Bytes claimed by this read's retained reservation (not a physical/RSS measure).
    #[must_use]
    pub fn reserved_bytes(&self) -> u64 {
        self.retained.bytes()
    }
}

impl core::fmt::Debug for CommittedBlob {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CommittedBlob")
            .field("len", &self.len())
            .field("basis", &self.basis)
            .finish()
    }
}

/// A typed current-record lookup from the Store's one publication basis.
///
/// A record is borrowed from the retained physical fold. No body is copied or
/// re-encoded, and the borrow prevents committing or resolving on the handle
/// while the caller uses the record. This is a current read, not a historical
/// disk search or an authorization decision.
#[derive(Debug)]
pub enum RecordLookup<'a> {
    /// The record exists on this fully replayed, durable basis.
    Found {
        /// The complete typed after-image.
        record: &'a crate::fold::FoldedRecord,
        /// The same root identity used by every current accessor.
        basis: ViewBasis,
    },
    /// Complete coverage establishes that this identified record is absent.
    Absent {
        /// The root on which absence was established.
        basis: ViewBasis,
    },
    /// Authority, durability or coverage does not permit a definitive answer.
    Incomplete(crate::open::OutcomeIncomplete),
}

/// A current-record read refusal, not a verdict about persisted bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordReadError {
    /// A caller supplied a term identity outside the term id range.
    InvalidKey(sage_db_types::persist::RecordKey),
    /// The in-memory current index names no retained owner: an internal defect,
    /// never absence, corruption of disk bytes, or an allocation refusal.
    MissingRetainedOwner {
        /// The indexed record.
        key: sage_db_types::persist::RecordKey,
        /// Its indexed revision.
        revision: u64,
    },
    /// The in-memory current index names no retained owner for a **natural-key**
    /// record: the same internal defect as [`Self::MissingRetainedOwner`], reported
    /// without copying the variable-length key.
    ///
    /// A model's key is an owned `String`, so naming it here would force an
    /// allocation on an error path. The state kind plus the indexed revision
    /// identify the row for an operator without one, and neither is a payload.
    MissingNaturalRetainedOwner {
        /// Which natural-key family the index row belongs to.
        kind: sage_db_types::persist::StateKind,
        /// Its indexed revision.
        revision: u64,
    },
    /// The same caller ledger refused work or stopped the read.
    Refused(sage_db_types::framing::Refusal),
}

/// Whether the served root is durable.
///
/// This is a Store-local type, not a canonical one: nothing in `types` changes for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewDurability {
    /// The observed root verified AND its whole chain synced successfully. This is evidence of a durable commit.
    Durable,
    /// The observed root verified and replayed, but its sync did not succeed. Readable, never a durability claim.
    Unverified,
}

impl ViewDurability {
    /// Whether this view may be treated as durable evidence.
    #[must_use]
    pub const fn is_durable(self) -> bool {
        matches!(self, Self::Durable)
    }
}

/// The identity of the root a view was built from.
///
/// `SnapshotId` alone is not enough: it describes a *graph* (kb, version, rules, graph digest) and carries no manifest file
/// digest, so two different on-disk roots can share one `SnapshotId`. The basis therefore keeps the CURRENT-named manifest
/// FILE digest, which is what actually distinguishes alternate roots.
///
/// Everything here is scalar or a small owned value — no name `String` is cloned, and no reference into a reused read buffer
/// survives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ViewBasis {
    /// The KB this root belongs to.
    pub kb_uuid: KbUuid,
    /// The version CURRENT names.
    pub version: KbVersion,
    /// SHA-256 of the manifest FILE that CURRENT names (not `chain_digest`).
    pub manifest_sha256: [u8; 32],
}

/// A `Snapshot` plus the durability tag and observed-root basis it was read from.
///
/// Immutable by design: a view cannot be promoted in place. Promotion happens by the `Store` publishing a *new* view after a
/// successful sync, so a holder of this value never sees its own tag change underneath it.
pub struct TaggedView {
    inner: Snapshot,
    durability: ViewDurability,
    basis: ViewBasis,
}

impl TaggedView {
    /// Build a view. Crate-internal: only the `Store` publishes views.
    pub(crate) fn new(inner: Snapshot, durability: ViewDurability, basis: ViewBasis) -> Self {
        Self {
            inner,
            durability,
            basis,
        }
    }

    /// The underlying immutable pin. Cloning is a refcount bump.
    #[must_use]
    pub fn snapshot(&self) -> &Snapshot {
        &self.inner
    }

    /// Take an owned handle to the pin. Refcount bump, no payload copy.
    #[must_use]
    pub fn snapshot_owned(&self) -> Snapshot {
        self.inner.clone()
    }

    /// This view's durability.
    #[must_use]
    pub const fn durability(&self) -> ViewDurability {
        self.durability
    }

    /// The root identity this view was built from.
    #[must_use]
    pub const fn basis(&self) -> ViewBasis {
        self.basis
    }

    /// The version this view serves.
    #[must_use]
    pub fn head(&self) -> KbVersion {
        self.inner.head()
    }

    /// The canonical current entry set of the viewed version.
    #[must_use]
    pub fn entries(&self) -> &[sage_db_types::persist::StateEntry] {
        self.inner.entries()
    }
}

impl Clone for TaggedView {
    fn clone(&self) -> Self {
        // A refcount bump on the pin: no payload copy, no new admission, no second claim.
        Self {
            inner: self.inner.clone(),
            durability: self.durability,
            basis: self.basis,
        }
    }
}

impl core::fmt::Debug for TaggedView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TaggedView")
            .field("durability", &self.durability)
            .field("basis", &self.basis)
            .field("head", &self.inner.head())
            .finish_non_exhaustive()
    }
}
