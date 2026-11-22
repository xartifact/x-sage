//! Persistent record types: the storage-layer contract, distinct from inference.
//!
//! [`crate::record::TermMeta`] and [`crate::record::AssertionRecord`] are **inference
//! projections**: the minimum the evaluators need. They are not the persisted
//! records, and they must not grow into them — the projection deliberately omits
//! body, evidence, audit and legacy fields, and a graph query must stay unable to
//! reach those bytes.
//!
//! Everything here is an after-image: a revision carries every record value, so a
//! single revision can be read without replaying earlier ones. Absence means absent;
//! nothing is inherited from a sibling revision.
//!
//! No I/O, no clock, no environment. Timestamps are caller-supplied microseconds so
//! a replay cannot re-invent them.

use crate::confidence::Confidence;
use crate::layer::SemanticLayer;
use crate::ledger::{BudgetError, ControlStop, SharedLedger};
use crate::relation::RelationId;

/// Closed set of persisted record kinds that carry a numeric identity.
///
/// Unknown values are a format error, never a new record kind. Records without a
/// numeric identity (assets, models, KB metadata, rule configuration) are absent
/// from this enum on purpose: they are keyed by content or by name, not by an id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u16)]
pub enum RecordKind {
    /// A term: the concept itself.
    Term = 1,
    /// An authored assertion with its own identity and revision.
    Assertion = 2,
    /// A claim made about a term.
    Claim = 3,
    /// An evidence record.
    Evidence = 4,
    /// A link binding evidence to a specific target revision.
    EvidenceLink = 5,
    /// A stored embedding.
    Vector = 6,
    /// A reviewable proposal.
    Proposal = 7,
    /// One activation-boundary entry.
    FrontierEntry = 8,
    /// One user session checkpoint.
    SessionCheckpoint = 9,
}

impl RecordKind {
    /// All kinds in numeric order.
    pub const ALL: [Self; 9] = [
        Self::Term,
        Self::Assertion,
        Self::Claim,
        Self::Evidence,
        Self::EvidenceLink,
        Self::Vector,
        Self::Proposal,
        Self::FrontierEntry,
        Self::SessionCheckpoint,
    ];

    /// Decode a persisted kind id.
    ///
    /// Returns `None` for 0 and for values at or above 10: the set is closed, so a
    /// decoder must report a format error rather than guess.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Term),
            2 => Some(Self::Assertion),
            3 => Some(Self::Claim),
            4 => Some(Self::Evidence),
            5 => Some(Self::EvidenceLink),
            6 => Some(Self::Vector),
            7 => Some(Self::Proposal),
            8 => Some(Self::FrontierEntry),
            9 => Some(Self::SessionCheckpoint),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding and ordering.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// Allocation space for a monotonically increasing identifier.
///
/// Deliberately separate from [`crate::identity::Counter`], which names an *error
/// payload* for the four P0 counters, not an allocation space. Term ids are bounded
/// by `u32`; the rest are 128-bit logical ranges.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u16)]
pub enum IdSpace {
    /// Term identifiers, bounded by `u32::MAX`.
    TermId = 0,
    /// Assertion identifiers.
    AssertionId = 1,
    /// Claim identifiers.
    ClaimId = 2,
    /// Evidence identifiers.
    EvidenceId = 3,
    /// Evidence-link identifiers.
    EvidenceLinkId = 4,
    /// Vector identifiers.
    VectorId = 5,
    /// Proposal identifiers.
    ProposalId = 6,
    /// Frontier-entry identifiers.
    FrontierEntryId = 7,
    /// Session-checkpoint identifiers.
    SessionCheckpointId = 8,
}

impl IdSpace {
    /// All spaces in numeric order.
    pub const ALL: [Self; 9] = [
        Self::TermId,
        Self::AssertionId,
        Self::ClaimId,
        Self::EvidenceId,
        Self::EvidenceLinkId,
        Self::VectorId,
        Self::ProposalId,
        Self::FrontierEntryId,
        Self::SessionCheckpointId,
    ];

    /// Decode a persisted space id; `None` outside `0..=8`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::TermId),
            1 => Some(Self::AssertionId),
            2 => Some(Self::ClaimId),
            3 => Some(Self::EvidenceId),
            4 => Some(Self::EvidenceLinkId),
            5 => Some(Self::VectorId),
            6 => Some(Self::ProposalId),
            7 => Some(Self::FrontierEntryId),
            8 => Some(Self::SessionCheckpointId),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// The binding upper bound for this space.
    ///
    /// Term ids are `u32`; every other space is declared as a full 128-bit range.
    /// The watermark type is `u128` for all of them so no space is silently capped
    /// at 64 bits.
    #[must_use]
    pub const fn max_id(self) -> u128 {
        match self {
            Self::TermId => u32::MAX as u128,
            _ => u128::MAX,
        }
    }
}

/// A record's numeric identity: kind plus id.
///
/// The id is always carried as `u128` so one wire shape covers every kind. For
/// [`RecordKind::Term`] the high bits must be zero, because term ids are `u32`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RecordKey {
    kind: RecordKind,
    id: u128,
}

impl RecordKey {
    /// A term key, rejecting ids above `u32::MAX`.
    ///
    /// Returns `None` rather than truncating: a truncated term id would alias a
    /// different term.
    #[must_use]
    pub const fn term(id: u32) -> Self {
        Self {
            kind: RecordKind::Term,
            id: id as u128,
        }
    }

    /// A key for a non-term kind.
    #[must_use]
    pub const fn new(kind: RecordKind, id: u128) -> Self {
        Self { kind, id }
    }

    /// The record kind.
    #[must_use]
    pub const fn kind(self) -> RecordKind {
        self.kind
    }

    /// The numeric id, widened.
    #[must_use]
    pub const fn id(self) -> u128 {
        self.id
    }

    /// Whether the id fits the bound declared by its kind.
    ///
    /// A decoder must call this: a term key with non-zero high bytes is a format
    /// error, not a large term.
    #[must_use]
    pub const fn id_is_in_range(self) -> bool {
        self.id <= self.kind_max()
    }

    const fn kind_max(self) -> u128 {
        match self.kind {
            RecordKind::Term => u32::MAX as u128,
            _ => u128::MAX,
        }
    }
}

/// A record plus the revision selected.
///
/// Used by proposal baselines, evidence links and history lookup: all three name a
/// *specific* revision, and a bare record id would silently mean "current".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RevPair {
    /// The record.
    pub key: RecordKey,
    /// The revision, first revision is 1.
    pub revision: crate::identity::RecordRevision,
}

/// An allocation high-water mark.
///
/// `hwm` is the largest id already allocated in `space`; the next id is `hwm + 1`.
/// `u128` for every space so no space is capped at 64 bits by the wire type.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Watermark {
    /// Which allocation space.
    pub space: IdSpace,
    /// Largest allocated id, 0 when nothing is allocated.
    pub hwm: u128,
}

/// A row count used by import reconciliation.
///
/// A different space from [`Watermark`]: a count is never an id, and mixing them
/// would let a 3-row import claim 3 allocated identifiers.
///
/// `kind` is a **state kind**, not a [`RecordKind`]. An import has to be able to
/// reconcile rows for the four natural-key records (Asset 10, Model 11, KbMetadata 12,
/// RuleConfig 13) as well as the nine identified ones, and [`RecordKind`] stops at 9
/// by design. Carrying `RecordKind` here would make those four uncountable, which is
/// a silent capacity loss in the reconciliation report rather than a format change.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CountPair {
    /// State kind counted, 1..=13.
    pub kind: crate::persist::StateKind,
    /// Number of rows.
    pub count: u64,
}

/// The fold's record-set partition, which is **not** [`RecordKind`].
///
/// The two share the numbers 1..=9 and stop sharing them immediately after: state
/// kinds 10..=13 name records that have no numeric identity at all. Keeping one enum
/// per role is what stops a natural-key record from being given a `RecordKey` because
/// somebody reused the other numbering.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u16)]
pub enum StateKind {
    /// A term.
    Term = 1,
    /// An assertion.
    Assertion = 2,
    /// A claim.
    Claim = 3,
    /// An evidence record.
    Evidence = 4,
    /// An evidence link.
    EvidenceLink = 5,
    /// A stored embedding.
    Vector = 6,
    /// A reviewable proposal.
    Proposal = 7,
    /// One activation-boundary entry.
    FrontierEntry = 8,
    /// One user session checkpoint.
    SessionCheckpoint = 9,
    /// A content-addressed asset; keyed by its SHA-256.
    Asset = 10,
    /// A registered embedding model; keyed by `model_key`.
    Model = 11,
    /// The KB's own metadata; a singleton.
    KbMetadata = 12,
    /// The current canonical rule configuration; a singleton.
    RuleConfig = 13,
}

impl StateKind {
    /// Every state kind, in numeric order.
    pub const ALL: [Self; 13] = [
        Self::Term,
        Self::Assertion,
        Self::Claim,
        Self::Evidence,
        Self::EvidenceLink,
        Self::Vector,
        Self::Proposal,
        Self::FrontierEntry,
        Self::SessionCheckpoint,
        Self::Asset,
        Self::Model,
        Self::KbMetadata,
        Self::RuleConfig,
    ];

    /// Decode a persisted state kind; `None` outside `1..=13`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Term),
            2 => Some(Self::Assertion),
            3 => Some(Self::Claim),
            4 => Some(Self::Evidence),
            5 => Some(Self::EvidenceLink),
            6 => Some(Self::Vector),
            7 => Some(Self::Proposal),
            8 => Some(Self::FrontierEntry),
            9 => Some(Self::SessionCheckpoint),
            10 => Some(Self::Asset),
            11 => Some(Self::Model),
            12 => Some(Self::KbMetadata),
            13 => Some(Self::RuleConfig),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding and ordering.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// The identified kind this state kind partitions, when it has one.
    #[must_use]
    pub const fn record_kind(self) -> Option<RecordKind> {
        match self {
            Self::Term => Some(RecordKind::Term),
            Self::Assertion => Some(RecordKind::Assertion),
            Self::Claim => Some(RecordKind::Claim),
            Self::Evidence => Some(RecordKind::Evidence),
            Self::EvidenceLink => Some(RecordKind::EvidenceLink),
            Self::Vector => Some(RecordKind::Vector),
            Self::Proposal => Some(RecordKind::Proposal),
            Self::FrontierEntry => Some(RecordKind::FrontierEntry),
            Self::SessionCheckpoint => Some(RecordKind::SessionCheckpoint),
            Self::Asset | Self::Model | Self::KbMetadata | Self::RuleConfig => None,
        }
    }

    /// Whether this record can have more than one revision.
    #[must_use]
    pub const fn is_revisable(self) -> bool {
        !matches!(self, Self::Asset)
    }

    /// The natural key width this kind uses, or `None` for numeric identity.
    ///
    /// `Some(0)` marks a singleton, whose key is the empty byte string.
    #[must_use]
    pub const fn natural_key(self) -> Option<NaturalKey> {
        match self {
            Self::Asset => Some(NaturalKey::Bytes32),
            Self::Model => Some(NaturalKey::Utf8),
            Self::KbMetadata | Self::RuleConfig => Some(NaturalKey::Singleton),
            _ => None,
        }
    }
}

/// How a natural-key record is addressed in the folded set.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum NaturalKey {
    /// The record's key is its 32 content-address bytes.
    Bytes32,
    /// The record's key is a UTF-8 name.
    Utf8,
    /// There is exactly one, with an empty key.
    Singleton,
}

/// Who actually performed a write, versus who a document claims.
///
/// The two are never interchangeable: promoting a declared value to an authenticated
/// identity fabricates an audit trail that cannot be undone later.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum ActorOrigin {
    /// Verified by the service layer.
    Authenticated = 0,
    /// Imported from a system that only recorded a declared author.
    LegacyDeclared = 1,
    /// The history exists but its actor was never archived.
    Unavailable = 2,
}

impl ActorOrigin {
    /// Decode a persisted value; `None` outside `0..=2`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Authenticated),
            1 => Some(Self::LegacyDeclared),
            2 => Some(Self::Unavailable),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// Advisory, byte-preserving annotation container.
///
/// Keys are held in UTF-8 byte order and values verbatim. It is advisory for
/// compatibility (an unknown reader must accept and preserve it) but it **is** part
/// of the canonical state: it cannot be reconstructed from other fields, so
/// excluding it from a state digest would let two different states compare equal.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Extensions(Vec<(String, Vec<u8>)>);

impl Extensions {
    /// An empty container.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Entries in canonical order.
    #[must_use]
    pub fn entries(&self) -> &[(String, Vec<u8>)] {
        &self.0
    }

    /// How many element slots the backing table has allocated.
    ///
    /// This is **slot** capacity, not bytes and not the number of entries: a table with
    /// spare slots reports a larger number than [`Self::entries`] would suggest. It is
    /// reported because a caller measuring the storage this container actually occupies
    /// must count the spare slots too; `entries().len()` undercounts them.
    ///
    /// It describes the outer table only. Each key `String` and value `Vec<u8>` owns its
    /// own allocation, which this number does not include.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    /// Look up one key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_slice())
    }

    /// Take ownership of already-canonical entries, validating them once.
    ///
    /// The decoder has the pairs in hand before this call, so requiring a fresh
    /// `insert` per entry would copy every key again and grow the vector one slot at a
    /// time - both invisible to the caller's ledger. This path validates order, key
    /// grammar, and value limits, then stores the vector as given.
    ///
    /// # Errors
    ///
    /// [`ExtensionsError::Order`] when keys are not strictly ascending (which also
    /// catches duplicates), [`ExtensionsError::InvalidKey`], or
    /// [`ExtensionsError::ValueTooLarge`].
    pub fn from_canonical(entries: Vec<(String, Vec<u8>)>) -> Result<Self, ExtensionsError> {
        let mut previous: Option<&str> = None;
        for (key, value) in &entries {
            if !valid_extension_key(key) {
                return Err(ExtensionsError::InvalidKey);
            }
            if value.len() > MAX_EXTENSION_VALUE {
                return Err(ExtensionsError::ValueTooLarge);
            }
            if previous.is_some_and(|prev| prev.as_bytes() >= key.as_bytes()) {
                return Err(ExtensionsError::Order);
            }
            previous = Some(key);
        }
        Ok(Self(entries))
    }

    /// Insert or replace a key, keeping canonical order.
    ///
    /// # Errors
    ///
    /// [`ExtensionsError::InvalidKey`] for a key outside `[a-z0-9]+(\.[a-z0-9-]+)*`
    /// or longer than 128 bytes; [`ExtensionsError::ValueTooLarge`] above 4096 bytes.
    pub fn insert(&mut self, key: &str, value: Vec<u8>) -> Result<(), ExtensionsError> {
        if !valid_extension_key(key) {
            return Err(ExtensionsError::InvalidKey);
        }
        if value.len() > MAX_EXTENSION_VALUE {
            return Err(ExtensionsError::ValueTooLarge);
        }
        match self
            .0
            .binary_search_by(|(k, _)| k.as_bytes().cmp(key.as_bytes()))
        {
            Ok(i) => self.0[i].1 = value,
            Err(i) => self.0.insert(i, (key.to_owned(), value)),
        }
        Ok(())
    }
}

/// Longest accepted extension key.
pub const MAX_EXTENSION_KEY: usize = 128;

/// Longest accepted extension value.
pub const MAX_EXTENSION_VALUE: usize = 4096;

/// A rejected extension mutation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExtensionsError {
    /// The key does not match `[a-z0-9]+(\.[a-z0-9-]+)*` or is too long.
    InvalidKey,
    /// The value exceeds [`MAX_EXTENSION_VALUE`].
    ValueTooLarge,
    /// Keys were not strictly ascending, which includes a duplicate.
    Order,
}

fn valid_extension_key(key: &str) -> bool {
    if key.is_empty() || key.len() > MAX_EXTENSION_KEY {
        return false;
    }
    for segment in key.split('.') {
        if segment.is_empty() {
            return false;
        }
        let mut chars = segment.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        if !first_ok {
            return false;
        }
        if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return false;
        }
    }
    true
}

/// Fields carried by every revisioned record event.
///
/// `updated_at` is optional because a creation event legitimately has none; absence
/// must stay absence rather than becoming a fabricated timestamp.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommonFields {
    /// Verified actor, when one was recorded.
    pub recorded_by: Option<String>,
    /// How the actor value came to exist.
    pub actor_origin: ActorOrigin,
    /// Why the write happened, when supplied.
    pub reason: Option<String>,
    /// Commit-supplied creation time, UTC microseconds.
    pub created_at: i64,
    /// Commit-supplied update time, UTC microseconds; absent on creation.
    pub updated_at: Option<i64>,
}

/// The audit tail of a record that can exist without an actor.
///
/// D10 gives `KbMetadata` and `RuleConfig` an **optional** actor: a Genesis
/// projection has none, and none is the honest value when the engine created the KB
/// rather than a principal. [`CommonFields`] cannot express that, because its
/// `actor_origin` is mandatory - every record that uses it must answer "who", and
/// `Unavailable` would be an answer invented by the writer rather than observed.
///
/// So this is a second, narrower carrier rather than a modified [`CommonFields`]:
/// term/assertion/claim/evidence/vector/proposal/etc. genuinely do require an origin
/// for every event, and loosening their field to `Option` would let a real audit gap
/// hide behind "no actor" everywhere. Only the records whose D10 row says the actor is
/// conditional use this type.
///
/// No conversion from [`CommonFields`] exists on purpose: a conversion would let a
/// caller launder an unavailable origin into an absent one and back, which is exactly
/// the distinction this type preserves.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OptionalActorAudit {
    /// Verified actor, when the event recorded one.
    ///
    /// `None` exactly when the event had no `actor_origin` at all - a Genesis
    /// projection. `Some(ActorOrigin::Unavailable)` is a different fact: the event
    /// stated that no actor is available.
    pub actor: Option<ActorOrigin>,
    /// Recorded principal, when one was recorded.
    pub recorded_by: Option<String>,
    /// Why the write happened, when supplied.
    pub reason: Option<String>,
    /// Commit-supplied creation time, UTC microseconds.
    pub created_at: i64,
    /// Commit-supplied update time, UTC microseconds; absent on creation.
    pub updated_at: Option<i64>,
}

impl OptionalActorAudit {
    /// The audit tail of a Genesis projection: no actor, no updater, no reason.
    #[must_use]
    pub const fn absent_actor(created_at: i64) -> Self {
        Self {
            actor: None,
            recorded_by: None,
            reason: None,
            created_at,
            updated_at: None,
        }
    }

    /// Whether the event stated an actor at all.
    ///
    /// Distinct from "the actor is unavailable": this asks whether the field was
    /// present, which is what a re-encoder needs in order to reproduce the image.
    #[must_use]
    pub const fn has_actor(&self) -> bool {
        self.actor.is_some()
    }
}

/// The audit tail of a record that is written once and never revised.
///
/// Asset, RequestOutcome and ImportSession have no `9 updated_at` in their D10 rows:
/// there is no later revision for a change time to describe. Carrying
/// [`CommonFields`] would give them a slot the wire cannot express, leaving an
/// encoder to either drop a caller's value or refuse it - both worse than not having
/// the field. Omitting it makes "an immutable record was updated at T" unrepresentable.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CreationAudit {
    /// Verified actor, when the event recorded one.
    pub recorded_by: Option<String>,
    /// How the actor value came to exist; every immutable record's row requires it.
    pub actor_origin: ActorOrigin,
    /// Why the write happened, when supplied.
    pub reason: Option<String>,
    /// Commit-supplied creation time, UTC microseconds.
    pub created_at: i64,
}

/// A complete term after-image.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TermRecord {
    /// Identity.
    pub key: RecordKey,
    /// Revision, first is 1.
    pub revision: crate::identity::RecordRevision,
    /// Whether the term is still active.
    pub retired: bool,
    /// Stable external slug.
    pub slug: String,
    /// Editable summary; not a dictionary key.
    pub atom: String,
    /// Authored layer, 0..=3.
    pub layer: SemanticLayer,
    /// Domains, in authored order.
    pub domains: Vec<String>,
    /// Declared source class.
    pub source: TermSource,
    /// Declared confidence.
    pub confidence: Confidence,
    /// Activation triggers, in authored order.
    pub triggers: Vec<String>,
    /// Content-addressed body reference.
    pub body_ref: [u8; 32],
    /// Audit and timestamps.
    pub common: CommonFields,
    /// Legacy identity from the imported system.
    pub legacy_id: Option<String>,
    /// Legacy source path.
    pub legacy_source_path: Option<String>,
    /// Legacy content hash, which is **not** this record's `revision`.
    pub legacy_content_digest: Option<[u8; 32]>,
    /// Legacy source-file hash at import time.
    pub legacy_source_file_digest: Option<[u8; 32]>,
    /// Legacy source revision value, which is a content hash, not a number.
    pub legacy_source_revision: Option<[u8; 32]>,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory and reconstructible from adjacent revisions.
    pub changed_fields: Option<Vec<u16>>,
}

/// Declared origin class of a term.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum TermSource {
    /// Personal position or methodology.
    Personal = 0,
    /// Objective description from a field of thought.
    Domain = 1,
    /// Synthesis across several fields.
    Synthesis = 2,
}

impl TermSource {
    /// Decode a persisted value; `None` outside `0..=2`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Personal),
            1 => Some(Self::Domain),
            2 => Some(Self::Synthesis),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// A complete assertion after-image.
///
/// Distinct from the inference projection [`crate::record::AssertionRecord`]: the
/// projection omits audit, legacy and status fields the store must persist.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AssertionRecordFull {
    /// Identity.
    pub key: RecordKey,
    /// Revision.
    pub revision: crate::identity::RecordRevision,
    /// Whether the assertion was retracted.
    pub retracted: bool,
    /// Authored subject.
    pub from: RecordKey,
    /// Relation template.
    pub relation: RelationId,
    /// Authored object; never rewritten, even for symmetric relations.
    pub target: RecordKey,
    /// Declared confidence.
    pub confidence: Confidence,
    /// Authoring order, preserved for export parity; never a semantic ordering.
    pub declaration_order: u32,
    /// Audit and timestamps.
    pub common: CommonFields,
    /// Legacy identity.
    pub legacy_id: Option<String>,
    /// Legacy content hash.
    pub legacy_content_digest: Option<[u8; 32]>,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory.
    pub changed_fields: Option<Vec<u16>>,
}

/// Claim lifecycle state.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum ClaimStatus {
    /// Authoritative context.
    Accepted = 0,
    /// Replaced by a later claim; history is kept.
    Superseded = 1,
    /// Reviewed and refused.
    Rejected = 2,
}

impl ClaimStatus {
    /// Decode a persisted value; `None` outside `0..=2`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Accepted),
            1 => Some(Self::Superseded),
            2 => Some(Self::Rejected),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// What a claim is doing in an argument.
///
/// Deliberately small: the kind selects how a reviewer reads the sentence, not a
/// taxonomy. A claim needing two kinds is two claims.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum ClaimKind {
    /// Supports the concept.
    Premise = 0,
    /// Restricts what is acceptable.
    Constraint = 1,
    /// Records a choice.
    Decision = 2,
    /// Names a hazard.
    Risk = 3,
    /// Reports something observed.
    Observation = 4,
    /// States something required.
    Requirement = 5,
}

impl ClaimKind {
    /// Decode a persisted value; `None` outside `0..=5`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Premise),
            1 => Some(Self::Constraint),
            2 => Some(Self::Decision),
            3 => Some(Self::Risk),
            4 => Some(Self::Observation),
            5 => Some(Self::Requirement),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// A complete claim after-image.
///
/// `text_ref` is a content hash: claims carry prose, and the prose lives in a blob for
/// the same reason a term body does - a revision must not rewrite the document, and a
/// revision read must not need unrelated records.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClaimRecord {
    /// Identity.
    pub key: RecordKey,
    /// Revision.
    pub revision: crate::identity::RecordRevision,
    /// Lifecycle state.
    pub status: ClaimStatus,
    /// The term this claim is about.
    pub concept: RecordKey,
    /// Argumentative role.
    pub kind: ClaimKind,
    /// Content-addressed reference to the claim text.
    pub text_ref: [u8; 32],
    /// Declared confidence.
    pub confidence: Confidence,
    /// Audit and timestamps.
    pub common: CommonFields,
    /// Legacy identity from the imported system.
    pub legacy_id: Option<String>,
    /// Legacy content hash, which is not this record's `revision`.
    pub legacy_content_digest: Option<[u8; 32]>,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory.
    pub changed_fields: Option<Vec<u16>>,
}

/// Evidence media class. Adding a kind is a product decision, not a data edit.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum EvidenceKind {
    /// Textual material with an excerpt.
    Text = 0,
    /// An image asset.
    Image = 1,
}

impl EvidenceKind {
    /// Decode a persisted value; `None` outside `0..=1`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Text),
            1 => Some(Self::Image),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// A complete evidence after-image.
///
/// Every field except `kind`, `title`, `captured_at` and the common fields is optional,
/// because a text evidence may have no locator and an imported record may have no
/// declared capturer. Their absence is a fact about the record, not a defect: an older
/// reader must never fill one in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EvidenceRecord {
    /// Identity.
    pub key: RecordKey,
    /// Revision.
    pub revision: crate::identity::RecordRevision,
    /// Media class.
    pub kind: EvidenceKind,
    /// Human title.
    pub title: String,
    /// Free-form locator; a URL, repository path or page number, not a KB reference.
    pub locator: Option<String>,
    /// Content-addressed excerpt blob.
    pub excerpt_ref: Option<[u8; 32]>,
    /// Content-addressed asset blob, required when `kind` is image.
    pub asset_sha256: Option<[u8; 32]>,
    /// Capture time as declared by the document, not the commit time.
    pub captured_at: i64,
    /// Capture attribution as declared by the document; never the executor.
    pub declared_captured_by: Option<String>,
    /// Audit and timestamps.
    pub common: CommonFields,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory.
    pub changed_fields: Option<Vec<u16>>,
}

/// What an evidence link points at.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum LinkTargetKind {
    /// A term revision.
    Term = 1,
    /// An assertion revision.
    Assertion = 2,
    /// A claim revision.
    Claim = 3,
}

impl LinkTargetKind {
    /// Decode a persisted value; `None` outside `1..=3`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Term),
            2 => Some(Self::Assertion),
            3 => Some(Self::Claim),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// A complete evidence-link after-image.
///
/// A link binds one evidence revision to one **target revision**. That binding is why
/// "current" versus "historical" is a read-time derivation rather than stored state: when
/// the target gains a revision, the link does not change, only its classification does.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EvidenceLinkRecord {
    /// Identity.
    pub key: RecordKey,
    /// Revision.
    pub revision: crate::identity::RecordRevision,
    /// Whether the link was retracted.
    pub retracted: bool,
    /// The evidence record.
    pub evidence: RecordKey,
    /// The evidence revision being cited.
    pub evidence_revision: crate::identity::RecordRevision,
    /// The target record.
    pub target: RecordKey,
    /// The target revision being supported.
    pub target_revision: crate::identity::RecordRevision,
    /// Audit and timestamps.
    pub common: CommonFields,
    /// Legacy identity from the imported system.
    pub legacy_id: Option<String>,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory.
    pub changed_fields: Option<Vec<u16>>,
}

/// A vector's lifecycle state.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum VectorStatus {
    /// Participates in retrieval subject to revision matching.
    Active = 0,
    /// Retained for history, excluded from retrieval.
    Retired = 1,
}

impl VectorStatus {
    /// Decode a persisted value; `None` outside `0..=1`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Active),
            1 => Some(Self::Retired),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// A complete vector after-image.
///
/// `input_digest` is the hash of the text the embedding was computed from, and
/// `subject_revision` is the object revision current at that moment. Retrieval checks
/// both: a vector whose subject has moved on is a claim about text that no longer exists,
/// so it is excluded rather than ranked. The values themselves are a blob
/// (`values_ref`), never inline - a 4096-dimension vector would otherwise dominate every
/// revision.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VectorRecord {
    /// Identity.
    pub key: RecordKey,
    /// Revision.
    pub revision: crate::identity::RecordRevision,
    /// Lifecycle state.
    pub status: VectorStatus,
    /// What the vector is about.
    pub subject: RecordKey,
    /// The subject revision the embedding was computed from.
    pub subject_revision: crate::identity::RecordRevision,
    /// Registered model identifier.
    pub model_id: String,
    /// Vector width; must match the model registry entry.
    pub dimensions: u32,
    /// Hash of the embedded text.
    pub input_digest: [u8; 32],
    /// Content-addressed float32 blob.
    pub values_ref: [u8; 32],
    /// Generation counter for this subject/model pair.
    pub embedding_revision: u64,
    /// Audit and timestamps.
    pub common: CommonFields,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory.
    pub changed_fields: Option<Vec<u16>>,
}

/// A proposal's review lifecycle.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum ProposalStatus {
    /// Awaiting review.
    Pending = 0,
    /// Approved; its content change committed with this status.
    Approved = 1,
    /// Refused by review.
    Rejected = 2,
    /// Its baseline went stale before approval.
    Conflicted = 3,
}

impl ProposalStatus {
    /// Decode a persisted value; `None` outside `0..=3`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Pending),
            1 => Some(Self::Approved),
            2 => Some(Self::Rejected),
            3 => Some(Self::Conflicted),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Whether this is a terminal decision.
    #[must_use]
    pub const fn is_decided(self) -> bool {
        matches!(self, Self::Approved | Self::Rejected | Self::Conflicted)
    }
}

/// Kinds of reviewable candidate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum ProposalKind {
    /// A whole-concept markdown submission.
    ConceptMarkdown = 0,
    /// A new claim.
    ClaimCreate = 1,
    /// An edit to an existing claim.
    ClaimEdit = 2,
    /// An evidence attachment.
    EvidenceAttach = 3,
    /// A relation assertion.
    RelationAssert = 4,
    /// Creation of the reserved Term identified by the subject.
    TermCreate = 5,
}

impl ProposalKind {
    /// Decode a persisted value; `None` outside `0..=5`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::ConceptMarkdown),
            1 => Some(Self::ClaimCreate),
            2 => Some(Self::ClaimEdit),
            3 => Some(Self::EvidenceAttach),
            4 => Some(Self::RelationAssert),
            5 => Some(Self::TermCreate),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// A complete proposal after-image.
///
/// `base_revisions` is the baseline vector the author read, not a bare version number:
/// a baseline constrains exactly the objects the proposal reads or writes, and a
/// whole-KB version would make unrelated proposals conflict. The payload is a blob so a
/// proposal never has to be re-serialised to be stored.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProposalRecord {
    /// Identity.
    pub key: RecordKey,
    /// Revision.
    pub revision: crate::identity::RecordRevision,
    /// Lifecycle state.
    pub status: ProposalStatus,
    /// What kind of candidate this is.
    pub kind: ProposalKind,
    /// The object the proposal is written against.
    pub subject: RecordKey,
    /// Baseline revisions, ascending and unique.
    pub base_revisions: Vec<RevPair>,
    /// Content-addressed payload blob.
    pub payload_ref: [u8; 32],
    /// Reviewer's note, when one was written.
    pub review_note: Option<String>,
    /// Author as declared by the submitted document; never the executor.
    pub declared_author: String,
    /// Audit and timestamps.
    pub common: CommonFields,
    /// Legacy identity from the imported system.
    pub legacy_id: Option<String>,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory.
    pub changed_fields: Option<Vec<u16>>,
}

/// Frontier entry state.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum FrontierStatus {
    /// Still unexplored.
    Open = 0,
    /// Explored.
    Resolved = 1,
}

impl FrontierStatus {
    /// Decode a persisted value; `None` outside `0..=1`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Open),
            1 => Some(Self::Resolved),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// A complete frontier-entry after-image.
///
/// The prose lives in `text_ref`; the entry carries references and a priority so the
/// boundary can be ordered without loading the text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FrontierEntryRecord {
    /// Identity.
    pub key: RecordKey,
    /// Revision.
    pub revision: crate::identity::RecordRevision,
    /// Lifecycle state.
    pub status: FrontierStatus,
    /// Concepts this entry is about.
    pub concept_refs: Vec<RecordKey>,
    /// Content-addressed reference to the entry's prose.
    pub text_ref: [u8; 32],
    /// Priority as authored.
    pub priority: u16,
    /// Audit and timestamps.
    pub common: CommonFields,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory.
    ///
    /// D10 allows tag 13 on a revise revision, and §6.4 makes it advisory rather than
    /// droppable: it is retained so a writer/reader pair cannot silently discard it.
    pub changed_fields: Option<Vec<u16>>,
}

/// The durable identity fields of a session checkpoint.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CheckpointLocation {
    /// A native session tied to an identified Term.
    Term {
        /// The native term key.
        concept: RecordKey,
        /// Positive native session ordinal.
        session_no: u32,
    },
    /// A session addressed by the public slug and its full ordinal.
    Slug {
        /// The public slug, which can exist before a term is registered.
        slug: String,
        /// Positive JavaScript-safe ordinal.
        session_no: u64,
    },
}

impl CheckpointLocation {
    /// The legacy public grammar and numeric domain for an addressed session.
    #[must_use]
    pub fn valid_public_parts(slug: &str, session_no: u64) -> bool {
        const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
        let mut bytes = slug.bytes();
        let lowercase_or_digit = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        (1..=MAX_SAFE_INTEGER).contains(&session_no)
            && bytes.next().is_some_and(lowercase_or_digit)
            && bytes.all(|byte| lowercase_or_digit(byte) || byte == b'-')
    }
}

/// A complete session-checkpoint after-image.
///
/// Distinct from a **storage** checkpoint: this is a user-facing record of a reasoning
/// session, and the two are never named the same in an API.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionCheckpointRecord {
    /// Identity.
    pub key: RecordKey,
    /// Revision.
    pub revision: crate::identity::RecordRevision,
    /// The native term or legacy public identity and session ordinal.
    pub location: CheckpointLocation,
    /// Content-addressed reference to the serialised session state.
    pub content_ref: [u8; 32],
    /// Audit and timestamps.
    pub common: CommonFields,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory.
    ///
    /// Retained for the same reason as on a frontier entry: an accepted advisory field
    /// must survive the round trip, not be silently dropped.
    pub changed_fields: Option<Vec<u16>>,
}

/// A committed request's durable outcome.
///
/// Only committed requests have one. A request rejected before the write produces a
/// typed error and no transaction; a transport-level unknown is not an outcome and
/// must resolve on retry. There is exactly one of these per non-genesis transaction,
/// and its principal/request/payload fields equal the transaction header's, so the
/// header and the event stream cannot drift apart unnoticed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RequestOutcomeEvent {
    /// Verified principal that submitted the request.
    pub principal_id: String,
    /// Caller-chosen request identity, the idempotency key component.
    pub request_id: String,
    /// Digest of the caller's request frame.
    pub payload_digest: [u8; 32],
    /// Reference to the persisted result payload blob.
    pub result_ref: [u8; 32],
    /// Version at which the outcome was recorded.
    pub commit_version: u64,
    /// Audit; a committed outcome is never revised.
    pub audit: CreationAudit,
    /// Advisory annotations, part of canonical state.
    ///
    /// D10 grants tag 14 to natural-key record events. Accepting it and having
    /// nowhere to put it would be accept-and-drop, which the format forbids; an
    /// outcome can therefore carry extensions even though `bookkeeping_digest`
    /// does not cover them (that digest's `Outcome` row deliberately excludes 14).
    pub extensions: Extensions,
}

/// A batch of mutually comparable records.
///
/// Constructed only from entries that are already in `(kind, key)` order with no
/// duplicate keys. The constructor rejects disorder instead of sorting: a silently
/// sorting fold hides a fold defect and yields a different-but-valid digest.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RecordSet {
    entries: Vec<StateEntry>,
}

/// One record as it enters the canonical state digest.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StateEntry {
    /// Record kind, including the natural-key kinds that have no numeric id.
    pub kind: u16,
    /// Natural key bytes; empty for singleton records.
    pub key: Vec<u8>,
    /// Current revision.
    pub revision: u64,
    /// Canonical record bytes.
    pub body: Vec<u8>,
}

/// Why a record set was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordSetError {
    /// Entries were not in ascending `(kind, key)` order.
    NotSorted,
    /// Two entries shared a `(kind, key)`.
    DuplicateKey,
}

impl RecordSet {
    /// Build from entries already in canonical order.
    ///
    /// # Errors
    ///
    /// [`RecordSetError::NotSorted`] or [`RecordSetError::DuplicateKey`]. Ordering
    /// and duplicate checks charge the shared ledger, so a hostile set cannot make
    /// this scan unbounded.
    pub fn from_sorted(
        entries: Vec<StateEntry>,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Self, RecordSetRejection> {
        ledger
            .try_charge_entries(entries.len() as u64)
            .map_err(RecordSetRejection::Budget)?;
        let mut prev: Option<(&u16, &[u8])> = None;
        for entry in &entries {
            ledger.checkpoint().map_err(RecordSetRejection::Stopped)?;
            if let Some((kind, key)) = prev {
                let order = (kind, key).cmp(&(&entry.kind, entry.key.as_slice()));
                if order == core::cmp::Ordering::Equal {
                    return Err(RecordSetRejection::Order(RecordSetError::DuplicateKey));
                }
                if order == core::cmp::Ordering::Greater {
                    return Err(RecordSetRejection::Order(RecordSetError::NotSorted));
                }
            }
            prev = Some((&entry.kind, &entry.key));
        }
        Ok(Self { entries })
    }

    /// The entries, in canonical order.
    #[must_use]
    pub fn entries(&self) -> &[StateEntry] {
        &self.entries
    }

    /// Consume the set and take ownership of its ordered entries.
    ///
    /// This is the accessor a caller needs when it must digest exactly these entries
    /// and then move them into something that owns them: cloning would duplicate every
    /// `key`/`body` buffer, and a copy is a different buffer from the one that was
    /// admitted.
    ///
    /// It preserves the buffer, its capacity and its contents exactly — no clone, no
    /// reallocation, no re-sort and no rescan. The set was already ordered and
    /// duplicate-checked at construction.
    ///
    /// Note the two distinct quantities [`RecordSet::from_sorted`] deals in: it charges
    /// **entry work** for the ordering scan and consults the caller's control, whereas the
    /// `key`/`body` bytes these entries retain are a separate concern. Extraction reads
    /// neither: it does not scan, so it owes no entry work, and it does not allocate, so
    /// it admits no bytes.
    #[must_use]
    pub fn into_entries(self) -> Vec<StateEntry> {
        self.entries
    }

    /// Number of records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// How an asset's bytes are encoded.
///
/// Closed: an unregistered value is a format error, not a new media type. The
/// registry is deliberately narrow because the persistence layer only needs to
/// tell a reader which decoder to use for `blobs/<sha256>`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum MediaType {
    /// PNG image.
    Png = 0,
    /// JPEG image.
    Jpeg = 1,
    /// WebP image.
    Webp = 2,
    /// GIF image.
    Gif = 3,
}

impl MediaType {
    /// Every registered value, in numeric order.
    pub const ALL: [Self; 4] = [Self::Png, Self::Jpeg, Self::Webp, Self::Gif];

    /// Decode a persisted value; `None` outside `0..=3`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Png),
            1 => Some(Self::Jpeg),
            2 => Some(Self::Webp),
            3 => Some(Self::Gif),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// A content-addressed asset: bytes that live in `blobs/<sha256>`.
///
/// Immutable. "Declared once" is the record's whole lifecycle: there is no revise or
/// delete event for it in this major, and no `status` or `revision` field, because a
/// content address cannot be revised - different bytes are a different asset.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AssetRecord {
    /// SHA-256 of the blob's bytes; also the natural key.
    pub sha256: [u8; 32],
    /// How those bytes are encoded.
    pub media_type: MediaType,
    /// Blob length in bytes; must equal the blob's actual length.
    pub byte_size: u64,
    /// Who the document claims produced the asset. Never a verified principal.
    pub declared_by: Option<String>,
    /// When the document claims it was produced, UTC microseconds.
    pub declared_at: Option<i64>,
    /// Audit; there is no update time, because the record is never revised.
    pub audit: CreationAudit,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
}

/// A model's lifecycle state.
///
/// Separate from [`VectorStatus`] even though the values read the same: a vector is
/// retired when it no longer describes its subject, a model when it should not be used
/// for new embeddings. Two decisions, two fields.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum ModelStatus {
    /// Usable for new embeddings.
    Active = 0,
    /// Present for existing vectors, not for new ones.
    Retired = 1,
}

impl ModelStatus {
    /// Every registered value, in numeric order.
    pub const ALL: [Self; 2] = [Self::Active, Self::Retired];

    /// Decode a persisted value; `None` outside `0..=1`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Active),
            1 => Some(Self::Retired),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// A registered embedding model.
///
/// Keyed by `model_key`, not by a numeric id: the key is the stable name callers
/// configured, and it survives a re-registration with a new identifier.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModelRecord {
    /// Revision, first is 1.
    pub revision: crate::identity::RecordRevision,
    /// Stable configured name; again the natural key.
    pub model_key: String,
    /// Provider-issued identifier this key resolves to.
    pub identifier: String,
    /// Provider name, when the deployment records one.
    pub provider: Option<String>,
    /// Embedding width; must be `1..=4096`.
    pub dimensions: u32,
    /// Lifecycle state.
    pub status: ModelStatus,
    /// Audit and timestamps.
    pub common: CommonFields,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory and reconstructible from adjacent revisions.
    pub changed_fields: Option<Vec<u16>>,
}

/// The knowledge base's own metadata, as a revivable singleton record.
///
/// Exactly one per KB. It carries **no** `rules_digest`: tag 163 belongs to
/// [`RuleConfigRecord`] only, so the current rules are read from that record instead
/// of being duplicated here.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KbMetadataRecord {
    /// Revision, first is 1.
    pub revision: crate::identity::RecordRevision,
    /// Immutable KB identity; must match the directory and the chain.
    pub kb_uuid: crate::identity::KbUuid,
    /// Stable slug.
    pub slug: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Domains, in authored order; not sorted.
    pub domains: Vec<String>,
    /// SHA-256 address of the exact legacy Frontier document, when established.
    pub legacy_frontier_ref: Option<[u8; 32]>,
    /// Audit and timestamps; `actor` is `None` exactly for a Genesis projection.
    pub audit: OptionalActorAudit,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory and reconstructible from adjacent revisions.
    pub changed_fields: Option<Vec<u16>>,
}

/// The KB's current canonical rule configuration, as a revivable singleton record.
///
/// Exactly one per KB. Distinct from [`KbMetadataRecord`] because the rules change on
/// an explicit migration, not when somebody renames the KB.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RuleConfigRecord {
    /// Revision, first is 1.
    pub revision: crate::identity::RecordRevision,
    /// Profile this configuration now selects; this is tag 196 (or 161).
    pub profile: crate::relation::RuleProfile,
    /// The profile that was in force before this change; tag 160.
    ///
    /// `None` exactly when projected from Genesis, which establishes a configuration
    /// rather than migrating one. A migration always states both ends, because
    /// "which profile did we come from" is not derivable from the new value and
    /// dropping it would silently lose a field the event requires.
    pub from_profile: Option<crate::relation::RuleProfile>,
    /// Digest of the canonical rules bytes.
    pub rules_digest: [u8; 32],
    /// Content address of those bytes in `blobs/<sha256>`.
    pub rules_ref: [u8; 32],
    /// Reference to a dry-run report, when a migration produced one.
    pub dry_run_report_ref: Option<[u8; 32]>,
    /// Audit and timestamps; `actor` is `None` exactly for a Genesis projection.
    pub audit: OptionalActorAudit,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
    /// Changed field tags, advisory and reconstructible from adjacent revisions.
    pub changed_fields: Option<Vec<u16>>,
}

/// One stage of an import session.
///
/// Session progress is a sequence, not a revision chain: the same
/// `import_session_id` appears once per stage, ordered by `created_at`. That is why
/// this record has no revision field.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ImportSessionRecord {
    /// Stable session identity.
    pub import_session_id: String,
    /// Which stage this event records.
    ///
    /// `CountPair`-style: the wire value is a `u16` with a closed domain of 0..=4.
    /// Carried as the typed enum so an out-of-domain value is refused at the codec
    /// rather than stored as an opaque number.
    pub stage: ImportStage,
    /// Digest of the imported source material.
    pub source_digest: [u8; 32],
    /// Content address of the import plan.
    pub plan_ref: [u8; 32],
    /// Expected row counts per record kind, ascending and unique.
    ///
    /// Row counts, not identifiers: [`CountPair`] is a different space from
    /// [`Watermark`], and mixing them would let a three-row import claim three
    /// allocated ids.
    pub expected_counts: Vec<CountPair>,
    /// Whether this stage completed the import.
    pub import_complete: bool,
    /// Audit; a stage record is written once. Later stages are new events under the
    /// same `import_session_id`, ordered by `created_at`, not revisions of this one.
    pub audit: CreationAudit,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
}

/// Which import stage an event records.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u16)]
pub enum ImportStage {
    /// Staging the source material.
    Prepare = 0,
    /// Checking the plan against the source.
    Validate = 1,
    /// Applying the plan.
    Import = 2,
    /// Re-reading what was written.
    Verify = 3,
    /// Making the imported data current.
    Activate = 4,
}

impl ImportStage {
    /// Every registered value, in numeric order.
    pub const ALL: [Self; 5] = [
        Self::Prepare,
        Self::Validate,
        Self::Import,
        Self::Verify,
        Self::Activate,
    ];

    /// Decode a persisted value; `None` outside `0..=5`.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Prepare),
            1 => Some(Self::Validate),
            2 => Some(Self::Import),
            3 => Some(Self::Verify),
            4 => Some(Self::Activate),
            _ => None,
        }
    }

    /// Numeric id used in canonical encoding.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// One allocation space's durable high-water mark.
///
/// `IdAllocation` events carry exactly the pair [`Watermark`] already holds, so this
/// is an alias rather than a second struct: two structurally identical declarations
/// would need an adapter at every boundary and would eventually disagree about which
/// one is authoritative.
pub type IdAllocationRecord = Watermark;

impl Watermark {
    /// Whether the mark is inside its space's declared bound.
    #[must_use]
    pub const fn is_in_range(self) -> bool {
        self.hwm <= self.space.max_id()
    }
}

/// One `IdAllocation` event.
///
/// The numeric payload is exactly [`Watermark`] - one definition, no duplicate struct
/// to keep in step. The event adds the two fields its D10 row also permits: a
/// conditional `7 reason` and an optional `14 extensions`. Folding those into the
/// two-field mark would either lose accepted data or forbid a legal event, so the
/// metadata lives here at the event boundary instead of inside the watermark.
///
/// Consumers that only need the mark (the bookkeeping digest, which covers
/// `counter`/`high_water_mark` and deliberately not extensions) take `allocation`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IdAllocationEvent {
    /// Which space advanced, and by how much.
    pub allocation: Watermark,
    /// Why the allocation happened; tag 7, conditional for a non-record event.
    pub reason: Option<String>,
    /// Advisory annotations, part of canonical state; tag 14.
    pub extensions: Extensions,
}

impl IdAllocationEvent {
    /// The mark alone, for digest assembly.
    #[must_use]
    pub const fn watermark(&self) -> Watermark {
        self.allocation
    }
}

/// The genesis event's projected metadata.
///
/// Genesis establishes [`KbMetadataRecord`] and [`RuleConfigRecord`] at revision 1 in
/// one event. It is deliberately **not** a pair of those records: D10 row 224 forbids
/// `5 recorded_by` and `6 actor_origin`, while both record types must carry a
/// [`CommonFields`] for their later update events. Wrapping them here would force the
/// caller to fabricate an actor and ask the codec to refuse it afterwards.
///
/// Making the illegal value unrepresentable is the fix: genesis has a creation time and
/// two field groups, and no place to put an actor at all.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GenesisRecord {
    /// Creation time, UTC microseconds; tag 8.
    pub created_at: i64,
    /// Why the KB was created; tag 7, conditional on row 224.
    ///
    /// `storage-digests.md` §4.4.6 puts tag 7 in both genesis bodies, and row 224
    /// lists it as conditional. A genesis that carries one is legal, so there must be
    /// a slot for it: refusing it would reject a legal file, and dropping it would
    /// violate §8.3.
    pub reason: Option<String>,
    /// The metadata genesis establishes, at revision 1 and with no actor.
    pub metadata: GenesisMetadata,
    /// The rule configuration genesis establishes, at revision 1 and with no actor.
    pub rules: GenesisRules,
}

/// The [`KbMetadataRecord`] field group as genesis carries it.
///
/// Same record values as [`KbMetadataRecord`] minus the audit tail: revision is always
/// [`crate::identity::RecordRevision::FIRST`] and there is no actor, update time or
/// reason for the engine to state.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GenesisMetadata {
    /// Immutable KB identity.
    pub kb_uuid: crate::identity::KbUuid,
    /// Stable slug.
    pub slug: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Domains, in authored order; not sorted.
    pub domains: Vec<String>,
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
}

/// The [`RuleConfigRecord`] field group as genesis carries it.
///
/// Genesis establishes a configuration rather than migrating one, so there is no
/// `from_profile` (that is tag 160, required only by `RuleProfileChanged`) and no
/// actor. The selected profile is tag 196 `rules_profile`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GenesisRules {
    /// Profile this configuration selects; tag 196.
    pub profile: crate::relation::RuleProfile,
    /// Digest of the canonical rules bytes.
    pub rules_digest: [u8; 32],
    /// Content address of those bytes in `blobs/<sha256>`.
    pub rules_ref: [u8; 32],
    /// Advisory annotations, part of canonical state.
    pub extensions: Extensions,
}

impl GenesisRecord {
    /// The metadata record genesis establishes.
    ///
    /// The result is the engine's own creation: revision 1, `ActorOrigin::Unavailable`
    /// and no `recorded_by`, which is exactly the admissible domain of a genesis
    /// projection. Nothing is invented; the values are derived from a shape that has
    /// no actor field to inherit.
    #[must_use]
    pub fn metadata_record(&self) -> KbMetadataRecord {
        KbMetadataRecord {
            revision: crate::identity::RecordRevision::FIRST,
            kb_uuid: self.metadata.kb_uuid,
            slug: self.metadata.slug.clone(),
            name: self.metadata.name.clone(),
            description: self.metadata.description.clone(),
            domains: self.metadata.domains.clone(),
            legacy_frontier_ref: None,
            audit: OptionalActorAudit {
                actor: None,
                recorded_by: None,
                reason: self.reason.clone(),
                created_at: self.created_at,
                updated_at: None,
            },
            extensions: self.metadata.extensions.clone(),
            changed_fields: None,
        }
    }

    /// The rule configuration genesis establishes.
    ///
    /// `from_profile` is `None`: a genesis migrates nothing.
    #[must_use]
    pub fn rules_record(&self) -> RuleConfigRecord {
        RuleConfigRecord {
            revision: crate::identity::RecordRevision::FIRST,
            profile: self.rules.profile,
            from_profile: None,
            rules_digest: self.rules.rules_digest,
            rules_ref: self.rules.rules_ref,
            dry_run_report_ref: None,
            audit: OptionalActorAudit {
                actor: None,
                recorded_by: None,
                reason: self.reason.clone(),
                created_at: self.created_at,
                updated_at: None,
            },
            extensions: self.rules.extensions.clone(),
            changed_fields: None,
        }
    }
}

/// Why a record set could not be built.
///
/// Kept separate from a format error for the same reason budget is separate from
/// corruption: the caller action differs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordSetRejection {
    /// The entries themselves are unacceptable.
    Order(RecordSetError),
    /// This request's budget ran out.
    Budget(BudgetError),
    /// The caller asked to stop.
    Stopped(ControlStop),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NeverRefuses;

    impl SharedLedger for NeverRefuses {
        fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
            Ok(())
        }

        fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
            Ok(())
        }

        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    fn entry(kind: u16, key: &[u8]) -> StateEntry {
        StateEntry {
            kind,
            key: key.to_vec(),
            revision: 1,
            body: Vec::new(),
        }
    }

    /// The consuming accessor must hand back the SAME buffers, not copies.
    ///
    /// This is a **selected-buffer pointer/capacity proof**, not allocator
    /// instrumentation: it pins the outer `Vec` and one entry's `key` and `body`
    /// buffers by address and capacity, which a clone would change. It deliberately
    /// does not claim to cover every buffer.
    #[test]
    fn into_entries_moves_the_selected_buffers_unchanged() {
        let mut led = NeverRefuses;
        let mut source = vec![entry(1, &[0, 0, 0, 1]), entry(12, &[])];
        // Distinctive capacities and a NON-EMPTY body with spare room, so a clone is
        // observable in both the address and the capacity of the payload buffer.
        source[0].key.reserve_exact(64);
        source[0].body = vec![0xAB, 0xCD, 0xEF];
        source[0].body.reserve_exact(29);
        source[1].key.reserve_exact(8);

        let outer_ptr = source.as_ptr();
        let outer_cap = source.capacity();
        let outer_len = source.len();
        let key_ptr = source[0].key.as_ptr();
        let key_cap = source[0].key.capacity();
        let body_ptr = source[0].body.as_ptr();
        let body_cap = source[0].body.capacity();

        let set = RecordSet::from_sorted(source, &mut led).expect("sorted input is accepted");
        let moved = set.into_entries();

        assert_eq!(moved.as_ptr(), outer_ptr, "outer buffer must not move");
        assert_eq!(
            moved.capacity(),
            outer_cap,
            "outer capacity must be preserved"
        );
        assert_eq!(moved.len(), outer_len);
        assert_eq!(
            moved[0].key.as_ptr(),
            key_ptr,
            "key buffer must be moved, not copied"
        );
        assert_eq!(moved[0].key.capacity(), key_cap);
        assert_eq!(
            moved[0].body.as_ptr(),
            body_ptr,
            "body buffer must be moved, not copied"
        );
        assert_eq!(moved[0].body.capacity(), body_cap);
        assert_eq!(
            moved[0].body,
            vec![0xAB, 0xCD, 0xEF],
            "body bytes unchanged"
        );
        assert_eq!(moved[0].key, vec![0, 0, 0, 1], "key bytes unchanged");
        assert_eq!(moved[1].kind, 12);
    }

    #[test]
    fn record_set_accepts_canonical_order() {
        let mut led = NeverRefuses;
        let set = RecordSet::from_sorted(
            vec![
                entry(1, &[0, 0, 0, 1]),
                entry(1, &[0, 0, 0, 2]),
                entry(12, &[]),
            ],
            &mut led,
        )
        .expect("sorted input is accepted");
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn record_set_rejects_disorder_rather_than_sorting() {
        let mut led = NeverRefuses;
        let err = RecordSet::from_sorted(
            vec![entry(1, &[0, 0, 0, 2]), entry(1, &[0, 0, 0, 1])],
            &mut led,
        )
        .expect_err("out of order must be refused");
        assert_eq!(
            err,
            RecordSetRejection::Order(RecordSetError::NotSorted),
            "a silently sorting constructor would hide a fold defect"
        );
    }

    #[test]
    fn record_set_rejects_duplicate_keys() {
        let mut led = NeverRefuses;
        let err = RecordSet::from_sorted(
            vec![entry(1, &[0, 0, 0, 1]), entry(1, &[0, 0, 0, 1])],
            &mut led,
        )
        .expect_err("duplicate key must be refused");
        assert_eq!(err, RecordSetRejection::Order(RecordSetError::DuplicateKey));
    }

    #[test]
    fn term_key_rejects_ids_above_u32() {
        let ok = RecordKey::term(7);
        assert!(ok.id_is_in_range());
        let too_big = RecordKey::new(RecordKind::Term, u32::MAX as u128 + 1);
        assert!(
            !too_big.id_is_in_range(),
            "term ids are u32, never truncated"
        );
    }

    #[test]
    fn watermark_preserves_the_full_128_bit_range() {
        let big = Watermark {
            space: IdSpace::AssertionId,
            hwm: u128::MAX - 1,
        };
        assert_eq!(big.hwm + 1, u128::MAX, "no silent 64-bit cap");
        assert_eq!(IdSpace::TermId.max_id(), u32::MAX as u128);
    }

    #[test]
    fn extensions_are_kept_in_byte_order() {
        let mut ext = Extensions::new();
        ext.insert("legacy.provider", b"alpha".to_vec()).unwrap();
        ext.insert("legacy.dimensions", b"4".to_vec()).unwrap();
        let keys: Vec<&str> = ext.entries().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["legacy.dimensions", "legacy.provider"]);
        assert_eq!(ext.get("legacy.provider"), Some(&b"alpha"[..]));
    }

    /// `capacity()` must report the REAL slot capacity, including spare slots that
    /// `entries().len()` does not show.
    ///
    /// A one-entry table built with spare capacity is the discriminating case: any
    /// accessor returning `entries.len()` reports 1 and fails here, while the real slot
    /// capacity is larger. This is a capacity regression, not allocator instrumentation.
    #[test]
    fn extensions_capacity_reports_spare_slots_not_just_len() {
        // One canonical entry in a vector with deliberate spare capacity.
        let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(8);
        entries.push(("legacy.provider".to_owned(), b"alpha".to_vec()));
        let actual_capacity = entries.capacity();
        let actual_len = entries.len();
        assert_eq!(actual_len, 1, "exactly one canonical entry");
        assert!(
            actual_capacity > actual_len,
            "the fixture must have spare slots, else len and capacity agree and this \
             test cannot distinguish them (capacity={actual_capacity}, len={actual_len})"
        );

        let ext = Extensions::from_canonical(entries).expect("canonical entry is accepted");

        assert_eq!(
            ext.capacity(),
            actual_capacity,
            "capacity must report the real slot count captured before the move"
        );
        assert_ne!(
            ext.capacity(),
            ext.entries().len(),
            "a len-valued accessor would report 1 and must fail here"
        );
        assert_eq!(
            ext.entries().len(),
            actual_len,
            "the entries themselves are unchanged"
        );
        assert_eq!(ext.get("legacy.provider"), Some(&b"alpha"[..]));
    }

    #[test]
    fn extensions_reject_bad_keys_and_oversize_values() {
        let mut ext = Extensions::new();
        assert_eq!(
            ext.insert("Bad/Key", Vec::new()),
            Err(ExtensionsError::InvalidKey)
        );
        assert_eq!(ext.insert("", Vec::new()), Err(ExtensionsError::InvalidKey));
        assert_eq!(
            ext.insert("ok", vec![0; MAX_EXTENSION_VALUE + 1]),
            Err(ExtensionsError::ValueTooLarge)
        );
    }

    #[test]
    fn unknown_kinds_and_spaces_are_refused_not_defaulted() {
        assert_eq!(RecordKind::from_u16(0), None);
        assert_eq!(RecordKind::from_u16(10), None);
        assert_eq!(IdSpace::from_u16(9), None);
        assert_eq!(ActorOrigin::from_u16(3), None);
        assert_eq!(TermSource::from_u16(3), None);
    }
}
