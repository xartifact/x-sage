//! Identity, record revision and KB version counters.
//!
//! Four counters exist and must never be interchangeable: term IDs, assertion
//! IDs, per-record revisions and the KB commit version. All of them are checked:
//! reaching the maximum rejects the write, because v1 never reuses an ID or a
//! revision (see `docs/design/data-model.md` §2).

use core::fmt;
use std::num::NonZeroU64;

/// A monotonic counter whose exhaustion must reject a write instead of wrapping.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Counter {
    /// [`TermId`] allocation space.
    TermId,
    /// [`AssertionId`] allocation space.
    AssertionId,
    /// Per-record revision space ([`RecordRevision`]).
    RecordRevision,
    /// KB commit sequence ([`KbVersion`]).
    KbVersion,
}

impl Counter {
    /// Stable lowercase name, suitable for logs and typed error payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TermId => "term_id",
            Self::AssertionId => "assertion_id",
            Self::RecordRevision => "record_revision",
            Self::KbVersion => "kb_version",
        }
    }
}

impl fmt::Display for Counter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Returned when a counter cannot advance without wrapping.
///
/// Callers must turn this into a rejected write; clamping or wrapping would
/// reuse an identity that historical records and evidence still reference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exhausted {
    counter: Counter,
}

impl Exhausted {
    /// Construct the error for the exhausted counter.
    #[must_use]
    pub const fn new(counter: Counter) -> Self {
        Self { counter }
    }

    /// The counter that ran out.
    #[must_use]
    pub const fn counter(self) -> Counter {
        self.counter
    }
}

impl fmt::Display for Exhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} space exhausted", self.counter)
    }
}

impl std::error::Error for Exhausted {}

/// Immutable per-KB identity.
///
/// References that cross a KB boundary carry this value so two KBs can never be
/// confused by a bare numeric ID.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KbUuid([u8; 16]);

impl KbUuid {
    /// Wrap 16 raw bytes. The engine never derives this from a name or path.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Raw bytes, for canonical encoding and digest domains.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for KbUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for KbUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// KB-local term identity.
///
/// Distinct from [`AssertionId`] even though both address records: a relation
/// endpoint is never an assertion, and the two allocation spaces are separate.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TermId(u32);

impl TermId {
    /// Wrap an already-allocated numeric ID.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Numeric value for indexing and canonical encoding.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Next ID, or [`Exhausted`] at the top of the space.
    ///
    /// # Errors
    ///
    /// Fails when `self` is `u32::MAX`; no ID is ever reused.
    pub const fn checked_next(self) -> Result<Self, Exhausted> {
        match self.0.checked_add(1) {
            Some(next) => Ok(Self(next)),
            None => Err(Exhausted::new(Counter::TermId)),
        }
    }
}

/// 128-bit assertion identity.
///
/// Every authored support keeps its own identity, revision and direction;
/// aggregation into a fact never merges two assertions (see
/// `docs/design/axiomatics.md` §4).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct AssertionId(u128);

impl AssertionId {
    /// Wrap an already-allocated numeric ID.
    #[must_use]
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    /// Numeric value for indexing and canonical encoding.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0
    }

    /// Next ID, or [`Exhausted`] at the top of the space.
    ///
    /// # Errors
    ///
    /// Fails when `self` is `u128::MAX`; no ID is ever reused.
    pub const fn checked_next(self) -> Result<Self, Exhausted> {
        match self.0.checked_add(1) {
            Some(next) => Ok(Self(next)),
            None => Err(Exhausted::new(Counter::AssertionId)),
        }
    }
}

/// Monotonic revision of one record.
///
/// Starts at 1 and increments on every state or content change, so reverting
/// content to an earlier byte sequence still produces a new revision (no ABA).
/// Not interchangeable with [`KbVersion`] or with a content digest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RecordRevision(NonZeroU64);

impl RecordRevision {
    /// Revision of a freshly created record.
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    /// Wrap a non-zero revision number.
    ///
    /// Returns `None` for 0: "no revision" is absence of a record, not revision
    /// zero, and must stay distinguishable in results.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Numeric value for encoding and comparison against persisted baselines.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Next revision, or [`Exhausted`] at the top of the space.
    ///
    /// # Errors
    ///
    /// Fails when `self` is `u64::MAX`; revisions never wrap.
    pub const fn checked_next(self) -> Result<Self, Exhausted> {
        match self.0.get().checked_add(1) {
            Some(next) => match NonZeroU64::new(next) {
                Some(next) => Ok(Self(next)),
                None => Err(Exhausted::new(Counter::RecordRevision)),
            },
            None => Err(Exhausted::new(Counter::RecordRevision)),
        }
    }
}

/// KB-wide commit sequence number.
///
/// Advances for every committed transaction, including proposal-only commits
/// that do not change active facts. A reader pins one version and must see a
/// single consistent snapshot for the whole request.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct KbVersion(u64);

impl KbVersion {
    /// No committed history yet: a directory without a durable genesis commit.
    pub const PRE_GENESIS: Self = Self(0);
    /// The genesis commit, which persists KB metadata.
    pub const GENESIS: Self = Self(1);

    /// Wrap a persisted commit sequence number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Numeric value for encoding and for `expect_version` checks.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Whether the database has a committed genesis.
    #[must_use]
    pub const fn is_genesis_or_later(self) -> bool {
        self.0 >= Self::GENESIS.0
    }

    /// Next version, or [`Exhausted`] at the top of the space.
    ///
    /// # Errors
    ///
    /// Fails when `self` is `u64::MAX`; versions never wrap.
    pub const fn checked_next(self) -> Result<Self, Exhausted> {
        match self.0.checked_add(1) {
            Some(next) => Ok(Self(next)),
            None => Err(Exhausted::new(Counter::KbVersion)),
        }
    }
}
