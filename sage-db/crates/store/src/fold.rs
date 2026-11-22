//! The physical after-image fold.
//!
//! Answers "what state does a committed chain describe", **physically**: it folds the records the format
//! already decoded, keyed exactly as the format keys them. No graph inference, no lifecycle legality (P2),
//! no authorization. Pure: no filesystem, clock or environment.
//!
//! ## Two key spaces, because the format has two
//!
//! `RecordKind` stops at 9 by design so a relation endpoint can never name a model or rule config. The
//! natural-key families are therefore addressed by their **actual natural key**, not by kind alone:
//!
//! * asset → `sha256`;
//! * model → `model_key`;
//! * KB metadata → singleton (`kb_uuid` is immutable, so there is exactly one);
//! * rule config → singleton.
//!
//! Keying them by kind alone was a real defect in an earlier draft: two models at revision 1 would collide
//! in one bucket and the second would silently overwrite the first. [`NaturalKey`] carries the value.
//!
//! ## Resource discipline: two requirements, both met
//!
//! There are two separate requirements, and satisfying one does not satisfy the other:
//!
//! 1. **Admission** — the ledger must refuse before work is done on bytes the caller cannot afford.
//! 2. **Allocator fallibility** — an allocation failure must be observable by the caller, not fatal.
//!
//! A `BTreeMap::insert` fails requirement 2: on allocation failure it aborts the process, and no amount of
//! pre-charging changes that. So this module holds no map and boxes no record. Every table is a sorted `Vec`
//! grown with `try_reserve_exact`, whose failure is mapped to `Refusal::Allocation`; high water marks are a fixed
//! nine-element array that needs no allocation at all.
//!
//! On top of that:
//!
//! * new table-slot bytes and inserted entries are admitted separately;
//! * incoming full records have already been decoded/admitted on the same ledger and move without copying;
//! * key-string bytes are a third admitted quantity, because a model key or an idempotency pair is copied
//!   into a sort key;
//! * `apply` takes events **by value** so records move in rather than being cloned;
//! * `outcome()` binary-searches a sorted table with a borrowed probe, so a lookup allocates nothing and is
//!   O(log n) rather than a linear scan over committed history.
//!
//! ## Deliberately absent
//!
//! No duplicate wire encoder: canonical bodies come from the format projection helper.
//! Digest assembly and persistence belong to the consumer, not this physical keyed fold.

use sage_db_types::framing::Refusal;
use sage_db_types::identity::{KbUuid, RecordRevision};
use sage_db_types::ledger::SharedLedger;
use sage_db_types::persist::{
    AssertionRecordFull, AssetRecord, ClaimRecord, EvidenceLinkRecord, EvidenceRecord,
    FrontierEntryRecord, IdAllocationEvent, IdSpace, ImportSessionRecord, KbMetadataRecord,
    ModelRecord, ProposalRecord, RecordKey, RecordKind, RequestOutcomeEvent, RuleConfigRecord,
    SessionCheckpointRecord, StateKind, TermRecord, VectorRecord,
};

/// A fixed-key retained-index read failure, without copying an identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FixedReadError {
    Refused(Refusal),
    MissingOwner { revision: u64 },
}

/// The natural key of a record that has no numeric identity.
///
/// Carries the **value**, not just the family. Two models at revision 1 are different records; keying by
/// kind alone merged them.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum NaturalKey {
    /// `AssetDeclared`: the blob digest is the key (§7.2).
    Asset([u8; 32]),
    /// `ModelRegistered`/`Updated`/`Retired`: the configured stable name is the key.
    Model(String),
    /// `KbMetadataUpdated`: exactly one per KB.
    KbMetadata,
    /// `RuleProfileChanged`: exactly one per KB.
    RuleConfig,
}

impl NaturalKey {
    /// The state kind this key belongs to.
    #[must_use]
    pub const fn state_kind(&self) -> StateKind {
        match self {
            Self::Asset(_) => StateKind::Asset,
            Self::Model(_) => StateKind::Model,
            Self::KbMetadata => StateKind::KbMetadata,
            Self::RuleConfig => StateKind::RuleConfig,
        }
    }
}

/// Which record a folded revision belongs to.
///
/// Two variants because the format has two key spaces: identified records carry a [`RecordKey`], natural
/// records carry their [`NaturalKey`]. Collapsing them would need a `RecordKey` for a model, which
/// `RecordKind` deliberately forbids.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum RecordIdentity {
    /// An identified record.
    Identified {
        /// Record kind and numeric id.
        key: RecordKey,
    },
    /// A natural-key or singleton record.
    Natural {
        /// The natural key itself.
        key: NaturalKey,
    },
}

impl RecordIdentity {
    /// The state kind this record contributes to.
    #[must_use]
    pub fn state_kind(&self) -> StateKind {
        match self {
            Self::Identified { key } => match key.kind() {
                RecordKind::Term => StateKind::Term,
                RecordKind::Assertion => StateKind::Assertion,
                RecordKind::Claim => StateKind::Claim,
                RecordKind::Evidence => StateKind::Evidence,
                RecordKind::EvidenceLink => StateKind::EvidenceLink,
                RecordKind::Vector => StateKind::Vector,
                RecordKind::Proposal => StateKind::Proposal,
                RecordKind::FrontierEntry => StateKind::FrontierEntry,
                RecordKind::SessionCheckpoint => StateKind::SessionCheckpoint,
            },
            Self::Natural { key } => key.state_kind(),
        }
    }
}

/// One folded record, as the chain left it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FoldedRecord {
    /// Tag 64.
    Asset(AssetRecord),
    /// Tag 1–3.
    Term(TermRecord),
    /// Tag 16–18.
    Assertion(AssertionRecordFull),
    /// Tag 32–35.
    Claim(ClaimRecord),
    /// Tag 48–49.
    Evidence(EvidenceRecord),
    /// Tag 50–51.
    EvidenceLink(EvidenceLinkRecord),
    /// Tag 80–81.
    Vector(VectorRecord),
    /// Tag 96–97.
    Proposal(ProposalRecord),
    /// Tag 112.
    FrontierEntry(FrontierEntryRecord),
    /// Tag 128.
    SessionCheckpoint(SessionCheckpointRecord),
    /// Tag 144; singleton.
    KbMetadata(KbMetadataRecord),
    /// Tag 145–147; keyed by `model_key`.
    Model(ModelRecord),
    /// Tag 160; singleton.
    RuleConfig(RuleConfigRecord),
}

impl FoldedRecord {
    /// Which identity this record is filed under.
    ///
    /// # Errors
    ///
    /// Refused when copying a model's owned natural key exceeds the ledger or reservation.
    /// Other key kinds are fixed-size copies and allocate nothing.
    pub fn identity(&self, ledger: &mut dyn SharedLedger) -> Result<RecordIdentity, Refusal> {
        Ok(match self {
            Self::Asset(r) => RecordIdentity::Natural {
                key: NaturalKey::Asset(r.sha256),
            },
            Self::Term(r) => RecordIdentity::Identified { key: r.key },
            Self::Assertion(r) => RecordIdentity::Identified { key: r.key },
            Self::Claim(r) => RecordIdentity::Identified { key: r.key },
            Self::Evidence(r) => RecordIdentity::Identified { key: r.key },
            Self::EvidenceLink(r) => RecordIdentity::Identified { key: r.key },
            Self::Vector(r) => RecordIdentity::Identified { key: r.key },
            Self::Proposal(r) => RecordIdentity::Identified { key: r.key },
            Self::FrontierEntry(r) => RecordIdentity::Identified { key: r.key },
            Self::SessionCheckpoint(r) => RecordIdentity::Identified { key: r.key },
            Self::KbMetadata(_) => RecordIdentity::Natural {
                key: NaturalKey::KbMetadata,
            },
            Self::Model(r) => RecordIdentity::Natural {
                key: NaturalKey::Model(copy_key_bounded(&r.model_key, ledger)?),
            },
            Self::RuleConfig(_) => RecordIdentity::Natural {
                key: NaturalKey::RuleConfig,
            },
        })
    }

    /// The revision this record carries.
    #[must_use]
    pub const fn revision(&self) -> RecordRevision {
        match self {
            // An asset has no `revision` field: §4.4.3 fixes it at 1, and it is not revisable.
            Self::Asset(_) => RecordRevision::FIRST,
            Self::Term(r) => r.revision,
            Self::Assertion(r) => r.revision,
            Self::Claim(r) => r.revision,
            Self::Evidence(r) => r.revision,
            Self::EvidenceLink(r) => r.revision,
            Self::Vector(r) => r.revision,
            Self::Proposal(r) => r.revision,
            Self::FrontierEntry(r) => r.revision,
            Self::SessionCheckpoint(r) => r.revision,
            Self::KbMetadata(r) => r.revision,
            Self::Model(r) => r.revision,
            Self::RuleConfig(r) => r.revision,
        }
    }

    /// Whether this record contributes to the active graph.
    ///
    /// Only terms and assertions. Selecting typed fields is a physical projection, not a duplicate wire
    /// encoding (`P1-4-BINDING-MAP.md` §2.1, gap B2), so it needs no format helper. §4.5 requires the field
    /// set *including* revision, and active-only.
    #[must_use]
    pub fn is_active_for_graph(&self) -> bool {
        match self {
            Self::Term(r) => !r.retired,
            Self::Assertion(r) => !r.retracted,
            _ => false,
        }
    }
}

/// One folded transaction, in commit order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FoldedVersion {
    /// Commit sequence number; 1 for genesis.
    pub version: u64,
    /// Transaction identity.
    pub tx_id: [u8; 16],
    /// Events this transaction carried.
    pub event_count: u32,
    /// Rules profile the transaction was evaluated with.
    pub rules_profile: u16,
    /// Digest of the rules bytes.
    pub rules_digest: [u8; 32],
}

/// One committed request outcome.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FoldedOutcome {
    /// Digest of the caller's opaque request frame; an exact retry carries the same one.
    pub payload_digest: [u8; 32],
    /// Version at which the outcome was recorded.
    pub commit_version: u64,
    /// The full event behind a SHARED immutable owner, retained because a retry answer must be the *original* result.
    ///
    /// Shared, not owned: an exact retry returns this same payload by refcount bump. Keeping it owned here is what forced
    /// the reply to clone it.
    pub event: crate::commit::SharedOutcome,
}

/// One committed import session, keyed by `import_session_id` (§4.6).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FoldedImportSession {
    /// Full record, retained for the bookkeeping projection.
    pub record: ImportSessionRecord,
}

/// The physical after-image of a committed chain.
///
/// ## Why sorted `Vec`s and not `BTreeMap`
///
/// `BTreeMap::insert` allocates node storage **infallibly**: on allocation failure it aborts the process.
/// Ledger admission cannot change that — admission controls the budget, allocation failure controls the
/// allocator, and they are separate requirements. So the fold keeps its tables in sorted `Vec`s, grows them
/// with `try_reserve_exact` (fallible) and keeps them ordered by construction. A `Box::new` has the same problem,
/// which is why records are stored inline rather than boxed: the fold takes them by value and moves them in.
///
/// High water marks are a fixed array because there are exactly nine id spaces, so that table needs no
/// allocation at all.
#[derive(Debug, Default)]
pub struct StateFold {
    /// `(identity, revision)` ascending, each record behind a shared immutable owner.
    ///
    /// The owner is why a fork costs a refcount bump instead of a deep copy of every owned `String`/`Vec`: payloads are
    /// never duplicated, so there is no infallible-copy gap to waive. `triomphe::Arc` in this crate provides the real
    /// fallible constructor the owner is built with.
    revisions: Vec<((RecordIdentity, u64), SharedRecord)>,
    /// `(identity, revision)` ascending, the current revision per identity.
    current: Vec<(RecordIdentity, u64)>,
    /// `(digest, declared_len)` ascending by digest.
    blobs: Vec<([u8; 32], u64)>,
    /// `(principal, request_id)` ascending.
    outcomes: Vec<((String, String), FoldedOutcome)>,
    /// `import_session_id` ascending.
    imports: Vec<(String, FoldedImportSession)>,
    /// Commit order, append-only.
    versions: Vec<FoldedVersion>,
    /// Indexed by `IdSpace` discriminant; `None` = that space never advanced.
    high_water: [Option<u128>; 9],
    /// Commit order, append-only.
    rules: Vec<(u64, u16, [u8; 32])>,
    kb_uuid: Option<KbUuid>,
    /// The reservation covering this container's own table arrays and its owned index/outcome/import buffers.
    ///
    /// Per-record `SharedRecord` tokens cover record bodies and their owner blocks; they do NOT cover the seven `Vec`
    /// headers, the natural/index keys, or the outcome/import rows. Without this, a committed fold keeps serving that data
    /// after the attempt facade drops while the credit that admitted it is gone.
    container: Option<crate::lease::OwnedReservation>,
}

/// One folded record behind a shared immutable owner.
///
/// This exists so forking a fold does not deep-copy record payloads. A clone is a refcount bump: infallible, and it
/// allocates no payload bytes. Construction is the ONLY allocation, and it goes through the provider's fallible
/// constructor so a refusal is a refusal rather than an abort.
#[derive(Clone, Debug)]
pub(crate) struct SharedRecord(triomphe::Arc<RecordOwner>);

impl PartialEq for SharedRecord {
    fn eq(&self, other: &Self) -> bool {
        // Payload equality, NOT pointer identity: two owners holding equal records are equal even when they are distinct
        // allocations. Pointer identity here would make a forked fold compare unequal to its source.
        self.get() == other.get()
    }
}

impl Eq for SharedRecord {}

/// The shared body of one retained record: the payload AND the reservation that keeps it accounted for.
///
/// The lease travels inside the shared owner, so the extent is released when the last `Arc` drops — not when the attempt
/// facade does. Without it, a facade drop would release credit while the Store still served the data, which is exactly the
/// premature-release the finite-ledger probe demonstrated.
struct RecordOwner {
    /// The record payload.
    record: FoldedRecord,
    /// The reservation covering this owner's allocation. One token per owner, released on last-owner drop.
    ///
    /// Held for its `Drop`, never read: the token's whole purpose is the release that happens when the last `Arc` over this
    /// owner goes away, so "unused" is the correct shape rather than a gap.
    #[allow(dead_code)]
    lease: crate::lease::OwnedReservation,
    /// The transferred claim over the record's nested key/body allocations, held for the same reason.
    #[allow(dead_code)]
    payload_lease: Option<crate::lease::OwnedReservation>,
}

impl core::fmt::Debug for RecordOwner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The payload is what identity and ordering are about; the lease is accounting, not content.
        f.debug_struct("RecordOwner")
            .field("record", &self.record)
            .finish_non_exhaustive()
    }
}

impl SharedRecord {
    /// Real reservations held inside the shared owner, not physical/RSS bytes.
    pub(crate) fn reserved_bytes(&self) -> u64 {
        self.0.lease.bytes().saturating_add(
            self.0
                .payload_lease
                .as_ref()
                .map_or(0, crate::lease::OwnedReservation::bytes),
        )
    }

    /// Build an owner over `record`, admitting what is genuinely new and transferring what was already admitted.
    ///
    /// # Layout
    ///
    /// The predicted extent is the layout of the REAL owner type — payload plus its inline lease — not the payload type
    /// alone. This is a **prediction**: the provider's control-block shape is not a stable public ABI and the concrete
    /// allocation has not been measured with an allocating instrument.
    ///
    /// # Errors
    ///
    /// `Refused(Budget)` when fresh admission is short; `Refused(Allocation)` only when the provider's fallible
    /// constructor actually fails. A layout that cannot be computed is a [`FoldDefect`], because no allocation and no
    /// reservation was attempted — reporting `Allocation` there would claim a failure that did not happen.
    fn new(record: FoldedRecord, ledger: &mut dyn crate::lease::LeaseLedger) -> FoldOutcome<Self> {
        let extent = match core::alloc::Layout::new::<core::sync::atomic::AtomicUsize>()
            .extend(core::alloc::Layout::new::<RecordOwner>())
            .ok()
            .and_then(|(layout, _)| u64::try_from(layout.pad_to_align().size()).ok())
        {
            Some(extent) => extent,
            None => return FoldOutcome::Defect(FoldDefect::OwnerLayout),
        };

        // The owner block is this module's OWN allocation: admitted fresh, token placed INSIDE the owner so the
        // reservation lives exactly as long as the retained data rather than as long as the attempt facade.
        let lease = match ledger.reserve_owned(extent) {
            Ok(lease) => lease,
            Err(error) => return FoldOutcome::Refused(Refusal::Budget(error)),
        };

        // The record's nested String/Vec buffers were already admitted by whoever decoded them, so their credit is
        // TRANSFERRED rather than charged again: without this the owner's claim covers only its wrapper while the
        // payload it keeps alive is unaccounted for.
        let payload_bytes = match record_owned_bytes(&record) {
            Ok(bytes) => bytes,
            // A figure this sum cannot represent is a local arithmetic defect, not a layout or allocation one.
            Err(defect) => return FoldOutcome::Defect(defect),
        };
        let payload_lease = if payload_bytes == 0 {
            None
        } else {
            // A transfer shortfall means the caller never admitted these buffers. `Refusal` (a types-owned enum) has no
            // transfer variant, so it is reported as a DEFECT rather than disguised as budget or allocation: nothing about
            // an allowance or an allocator would change it, it is a broken admission precondition in the caller.
            match ledger.adopt_retained(payload_bytes) {
                Ok(lease) => Some(lease),
                Err(crate::lease::TransferError::Untransferred {
                    requested_bytes,
                    available_bytes,
                }) => {
                    return FoldOutcome::Defect(FoldDefect::PayloadNotAdmitted {
                        requested: requested_bytes,
                        available: available_bytes,
                    });
                }
            }
        };

        match triomphe::Arc::try_new(RecordOwner {
            record,
            lease,
            payload_lease,
        }) {
            Ok(arc) => FoldOutcome::Ok(Self(arc)),
            // `try_new` dropped the input, which dropped both leases, which released: nothing to unwind.
            Err(_) => FoldOutcome::Refused(Refusal::Allocation {
                requested: extent as usize,
            }),
        }
    }

    /// Borrow the record.
    pub(crate) fn get(&self) -> &FoldedRecord {
        &self.0.record
    }
}

/// A defect in the state being folded — **not** a verdict about stored bytes.
///
/// The separation `format::state::DigestAssemblyError::Structure` makes, and the reason the fold does not
/// return `FormatOutcome`: a fold over an unsubmitted candidate is describing a *caller's* batch, while a fold
/// over a committed chain is describing stored bytes. Same physical fact, different remedies. The consumer
/// names it: a commit path rejects the candidate, a replay path reports `CORRUPT`. Mapping it to
/// `FormatError::Corrupt` here would tell an operator to restore a backup because a commit candidate was built
/// wrongly.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FoldDefect {
    /// A later event reuses an already committed request key.
    DuplicateOutcome {
        /// Version of the first committed result.
        original_version: u64,
        /// Version attempting to reuse that request key.
        duplicate_version: u64,
    },
    /// The retained recorded-reply owner could not be constructed.
    Outcome(crate::commit::SharedOutcomeError),
    /// The real owner's layout could not be computed, so nothing was allocated and nothing was reserved.
    OwnerLayout,
    /// The caller's control asked to stop during a retention walk.
    ///
    /// A real caller cancellation, distinct from a defect in the data.
    Stopped(sage_db_types::query::ControlStop),
    /// A retained extent could not be represented in `u64`.
    ///
    /// The sum genuinely overflowed: no allocator was consulted and no allowance refused, so this is neither `Allocation`
    /// nor `Budget`, and saturating would hand a wrapped figure to `adopt_retained`.
    ExtentOverflow,
    /// A revision did not continue its identity's chain.
    ///
    /// A creation must be revision 1 and an update must be `previous + 1`. This is a property of the candidate or the
    /// replayed chain, so it is a fold defect — not a new wire/format corruption class.
    RevisionDiscontinuity {
        /// The record whose revision broke continuity.
        identity: RecordIdentity,
        /// The revision the chain requires (`None` when the predecessor would overflow).
        expected: Option<u64>,
        /// The revision actually presented.
        found: u64,
    },
    /// A rebuilt canonical container was rejected by its own canonicaliser.
    ///
    /// The SOURCE was canonical, so this can only be a defect in the copy — never budget, allocation or an input verdict.
    CopyNotCanonical,
    /// A record's nested buffers were not admitted before the record was handed to the fold.
    ///
    /// Not budget and not allocation: no allowance or allocator would produce credit that was never admitted. It reports
    /// the real quantities so the caller can see what it failed to admit.
    PayloadNotAdmitted {
        /// Bytes the record's nested buffers actually occupy.
        requested: u64,
        /// Untransferred credit the ledger actually held.
        available: u64,
    },
    /// Two different images share one `(identity, revision)`.
    ///
    /// MODEL §3 rule 1: a revision advances by exactly one and every revision is retained, so a collision makes
    /// the retained-history locator ambiguous. Never silently overwritten, never "repaired" by taking the
    /// maximum revision.
    HistoryCollision {
        /// Which record collided.
        identity: RecordIdentity,
        /// The revision that appeared twice.
        revision: u64,
    },
}

/// The fold's result: completed, ledger-refused, or structurally defective.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FoldOutcome<T> {
    /// The fold completed.
    Ok(T),
    /// The ledger or the allocator refused.
    Refused(Refusal),
    /// The records handed over cannot describe a legal chain.
    Defect(FoldDefect),
}

impl<T> FoldOutcome<T> {
    /// The value, when the fold completed.
    ///
    /// # Errors
    ///
    /// The refusal or defect otherwise, unchanged.
    pub fn into_result(self) -> Result<T, Self> {
        match self {
            Self::Ok(value) => Ok(value),
            other => Err(other),
        }
    }

    /// The refusal, when the ledger or allocator refused.
    #[must_use]
    pub const fn as_refusal(&self) -> Option<Refusal> {
        match self {
            Self::Refused(refusal) => Some(*refusal),
            _ => None,
        }
    }
}

impl StateFold {
    /// An empty fold: no committed history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The KB identity the chain belongs to, once a genesis has been folded.
    #[must_use]
    pub const fn kb_uuid(&self) -> Option<KbUuid> {
        self.kb_uuid
    }

    /// The highest folded version, or `None` before genesis.
    #[must_use]
    pub fn head_version(&self) -> Option<u64> {
        self.versions.last().map(|v| v.version)
    }

    /// Every folded version, in commit order.
    #[must_use]
    pub fn versions(&self) -> &[FoldedVersion] {
        &self.versions
    }

    /// Every revision folded for `identity`, ascending.
    /// The shared owner of one specific `(identity, revision)`, if this fold holds it.
    ///
    /// This is the history bridge: it hands out the EXISTING owner (a refcount clone), so a retained history image shares
    /// already-admitted payload instead of copying it. Crate-visible because it exposes the internal owner type.
    pub(crate) fn revision_owner(
        &self,
        identity: &RecordIdentity,
        revision: u64,
    ) -> Option<SharedRecord> {
        self.revisions
            .binary_search_by(|((id, rev), _)| (id, rev).cmp(&(identity, &revision)))
            .ok()
            .map(|at| self.revisions[at].1.clone())
    }

    /// Every revision folded for `identity`, ascending.
    pub fn revisions_of(&self, identity: &RecordIdentity) -> impl Iterator<Item = &FoldedRecord> {
        // Borrowed comparison: no owned identity is constructed, so a read allocates nothing. An earlier
        // revision cloned here and in four sibling accessors with no ledger parameter, making the allocation
        // unobservable by construction.
        self.revisions
            .iter()
            .filter(move |((id, _), _)| id == identity)
            .map(|(_, record)| record.get())
    }

    /// How many revisions are retained for `identity`.
    #[must_use]
    pub fn revision_count(&self, identity: &RecordIdentity) -> usize {
        self.revisions
            .iter()
            .filter(|((id, _), _)| id == identity)
            .count()
    }

    /// The current revision of `identity`, if any.
    #[must_use]
    pub fn current_revision(&self, identity: &RecordIdentity) -> Option<u64> {
        self.current
            .iter()
            .find(|(id, _)| id == identity)
            .map(|(_, rev)| *rev)
    }

    /// All current records without creating owned keys; graph assembly checkpoints each returned row.
    pub(crate) fn current_records(&self) -> impl Iterator<Item = &FoldedRecord> {
        self.current.iter().filter_map(|(identity, revision)| {
            self.revisions
                .binary_search_by(|((id, rev), _)| (id, rev).cmp(&(identity, revision)))
                .ok()
                .map(|at| self.revisions[at].1.get())
        })
    }

    /// The current record for `identity`, if any.
    #[must_use]
    pub fn current_record(&self, identity: &RecordIdentity) -> Option<&FoldedRecord> {
        let revision = self.current_revision(identity)?;
        self.revisions
            .iter()
            .find(|((id, rev), _)| id == identity && *rev == revision)
            .map(|(_, record)| record.get())
    }

    /// Read an identified current after-image, charging every searched table row.
    ///
    /// Numeric identities make each comparison fixed size. This avoids hiding
    /// an unbounded linear search behind a nominally budgeted Store accessor.
    /// Neither the key nor the returned record is cloned.
    pub(crate) fn identified_current_bounded(
        &self,
        key: RecordKey,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<&FoldedRecord>, crate::view::RecordReadError> {
        self.fixed_current_bounded(&RecordIdentity::Identified { key }, ledger)
            .map_err(|error| match error {
                FixedReadError::Refused(r) => crate::view::RecordReadError::Refused(r),
                FixedReadError::MissingOwner { revision } => {
                    crate::view::RecordReadError::MissingRetainedOwner { key, revision }
                }
            })
    }

    /// Visit every identified current record, accounting for the actual index
    /// traversal and each bounded retained-owner lookup. Natural-key metadata is
    /// deliberately outside this numeric-key API, not a silently dropped match.
    pub(crate) fn visit_identified_current<'a, E>(
        &'a self,
        ledger: &mut dyn SharedLedger,
        visitor: impl FnMut(&'a FoldedRecord, &mut dyn SharedLedger) -> Result<(), E>,
    ) -> Result<(), crate::view::RecordScanError<E>> {
        self.visit_current(CurrentSelection::Identified, ledger, visitor)
    }

    /// Visit every current record of every family on one immutable fold.
    ///
    /// The physical administrative surface: all thirteen record families the fold
    /// can hold, including retired and retracted ones. Non-record bookkeeping
    /// (outcomes, high-water marks, import stage rows, rule history) is deliberately
    /// excluded — those are not records and have no current after-image to serve.
    ///
    /// # One spine, two selections
    ///
    /// [`Self::visit_current`] owns the index traversal, its per-row control and
    /// entry charge, the retained-owner lookup and the missing-owner verdict exactly
    /// once; `selection` only decides which identities it serves. The two public
    /// algorithms therefore cannot drift.
    ///
    /// # Variable-length keys
    ///
    /// A model's natural key is a `String`. Comparing it is real work over
    /// caller-controlled bytes, so the comparison is charged and interruptible:
    /// [`Self::current_owner_bounded_with`] walks both keys in bounded steps,
    /// consulting the caller's ledger at each step and producing exactly `Ord`'s
    /// answer. No key is cloned and no [`RecordIdentity`] is constructed for a
    /// probe.
    ///
    /// # Errors
    ///
    /// The caller's real refusal/stop and the visitor's own error, via
    /// [`crate::view::RecordScanError`]. A current row whose owner is missing is
    /// named (`MissingRetainedOwner` for numeric keys,
    /// `MissingNaturalRetainedOwner` for natural ones), never skipped: silently
    /// dropping it would turn an index defect into a plausible complete scan.
    pub(crate) fn visit_all_current<'a, E>(
        &'a self,
        ledger: &mut dyn SharedLedger,
        visitor: impl FnMut(&'a FoldedRecord, &mut dyn SharedLedger) -> Result<(), E>,
    ) -> Result<(), crate::view::RecordScanError<E>> {
        self.visit_current(CurrentSelection::All, ledger, visitor)
    }

    /// The shared traversal spine behind both public scans.
    ///
    /// `selection` is the only difference between them; everything that could hide
    /// a row lives here once. An empty fold still takes an entry and an exit control
    /// step, so a caller that already stopped is never told it completed a scan of
    /// nothing.
    fn visit_current<'a, E>(
        &'a self,
        selection: CurrentSelection,
        ledger: &mut dyn SharedLedger,
        mut visitor: impl FnMut(&'a FoldedRecord, &mut dyn SharedLedger) -> Result<(), E>,
    ) -> Result<(), crate::view::RecordScanError<E>> {
        use crate::view::RecordScanError;
        index_step(ledger)?;
        for (identity, revision) in &self.current {
            index_step(ledger)?;
            if !selection.includes(identity) {
                continue;
            }
            let record = self
                .current_owner_bounded_with(identity, ledger)
                .map_err(RecordScanError::Read)?
                .map(SharedRecord::get)
                .ok_or_else(|| missing_owner(identity, *revision))
                .map_err(RecordScanError::Read)?;
            visitor(record, ledger).map_err(RecordScanError::Visitor)?;
        }
        index_step(ledger)?;
        Ok(())
    }

    /// The retained owner of exactly `identity`, charged per search iteration.
    ///
    /// This is the scan-facing wrapper: it maps the shared lookup's compact
    /// [`FixedReadError`] into the error the scan reports, naming the missing row by
    /// its identity shape without copying a variable-length key.
    ///
    /// One search implementation serves the scans *and* the pre-existing fixed-key
    /// readers ([`Self::fixed_current_owner_bounded`]), so the two cannot disagree
    /// about which row an identity names. What differs is only how a caller is told
    /// about a missing owner.
    fn current_owner_bounded_with(
        &self,
        identity: &RecordIdentity,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<&SharedRecord>, crate::view::RecordReadError> {
        self.owner_lookup_bounded(identity, ledger)
            .map_err(|error| match error {
                FixedReadError::Refused(refusal) => crate::view::RecordReadError::Refused(refusal),
                FixedReadError::MissingOwner { revision } => missing_owner(identity, revision),
            })
    }

    /// Only numeric identities and the fixed RuleConfig singleton use this helper.
    /// A future variable string key must additionally account for compared bytes.
    pub(crate) fn fixed_current_bounded(
        &self,
        identity: &RecordIdentity,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<&FoldedRecord>, FixedReadError> {
        self.fixed_current_owner_bounded(identity, ledger)
            .map(|owner| owner.map(SharedRecord::get))
    }

    /// Share the SAME already-admitted current owner through the same physical
    /// bounded lookup used by borrowed reads. A clone is a refcount bump only.
    pub(crate) fn identified_image_bounded(
        &self,
        key: RecordKey,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<crate::history::HistoryImage>, crate::view::RecordReadError> {
        self.fixed_current_owner_bounded(&RecordIdentity::Identified { key }, ledger)
            .map(|owner| owner.map(|owner| crate::history::HistoryImage::new(owner.clone())))
            .map_err(|error| match error {
                FixedReadError::Refused(r) => crate::view::RecordReadError::Refused(r),
                FixedReadError::MissingOwner { revision } => {
                    crate::view::RecordReadError::MissingRetainedOwner { key, revision }
                }
            })
    }

    pub(crate) fn fixed_current_owner_bounded(
        &self,
        identity: &RecordIdentity,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<&SharedRecord>, FixedReadError> {
        self.owner_lookup_bounded(identity, ledger)
    }

    /// The one bounded search over `current` and `revisions`.
    ///
    /// Fixed-shape identities (a numeric key, an asset digest, either empty-key
    /// singleton) compare in the same step that advances the search: **one**
    /// iteration charge covers one comparison, so the identified scan's per-row cost
    /// is unchanged from before this feature. A model key is variable-length and is
    /// compared through [`bounded_key_cmp`], which charges its own steps in addition
    /// to the iteration charge — the bytes it walks are work the fixed shapes do not
    /// do.
    fn owner_lookup_bounded(
        &self,
        identity: &RecordIdentity,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<&SharedRecord>, FixedReadError> {
        fn step(ledger: &mut dyn SharedLedger) -> Result<(), FixedReadError> {
            ledger
                .checkpoint()
                .map_err(|e| FixedReadError::Refused(Refusal::Stopped(e)))?;
            ledger
                .try_charge_entries(1)
                .map_err(|e| FixedReadError::Refused(Refusal::Budget(e)))
        }
        let mut lo = 0;
        let mut hi = self.current.len();
        let mut revision = None;
        while lo < hi {
            step(ledger)?;
            let mid = lo + (hi - lo) / 2;
            match identity_cmp_charged(&self.current[mid].0, identity, ledger)? {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => {
                    revision = Some(self.current[mid].1);
                    break;
                }
            }
        }
        let Some(revision) = revision else {
            return Ok(None);
        };
        let mut lo = 0;
        let mut hi = self.revisions.len();
        while lo < hi {
            step(ledger)?;
            let mid = lo + (hi - lo) / 2;
            let ((id, rev), owner) = &self.revisions[mid];
            let order = match identity_cmp_charged(id, identity, ledger)? {
                core::cmp::Ordering::Equal => rev.cmp(&revision),
                other => other,
            };
            match order {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Ok(Some(owner)),
            }
        }
        // A current entry without a retained owner is an internal index defect.
        // Returning `Ok(None)` here would let the caller report absence for a row
        // the index says exists.
        Err(FixedReadError::MissingOwner { revision })
    }

    /// Every currently-held record of one state kind, ascending by identity.
    pub fn current_of_kind(&self, kind: StateKind) -> impl Iterator<Item = &FoldedRecord> {
        self.current
            .iter()
            .filter(move |(id, _)| id.state_kind() == kind)
            .filter_map(move |(id, rev)| {
                self.revisions
                    .iter()
                    .find(|((rid, rrev), _)| rid == id && rrev == rev)
                    .map(|(_, record)| record.get())
            })
    }

    /// Probe the sorted committed-reference union with a real per-comparison gate.
    pub(crate) fn committed_blob_len_bounded(
        &self,
        digest: &[u8; 32],
        ledger: &mut dyn SharedLedger,
    ) -> Result<Option<u64>, Refusal> {
        let mut lo = 0;
        let mut hi = self.blobs.len();
        while lo < hi {
            ledger.checkpoint().map_err(Refusal::Stopped)?;
            ledger.try_charge_entries(1).map_err(Refusal::Budget)?;
            let mid = lo + (hi - lo) / 2;
            match self.blobs[mid].0.cmp(digest) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Ok(Some(self.blobs[mid].1)),
            }
        }
        Ok(None)
    }

    /// The **cumulative committed blob-reference union**: every blob reference the format enumerated across
    /// committed history, with its verified length,
    /// ascending by digest.
    ///
    /// Role, stated so it cannot drift into a second meaning (t67 item 2, root's lead):
    ///
    /// * It records only refs that a committed transaction actually required, added from
    ///   [`FoldEvent::CommittedBlobRef`] with the length of bytes the caller verified. Declared-but-unreferenced
    ///   blobs stay out: they may be durable orphans, and a replay could never derive them.
    /// * It is **not** a reference enumerator. Which `bytes32` fields are content addresses is a format
    ///   question with exactly one answer (`dispatch::is_blob_ref_tag`: tags 23, 50, 67, 68, 101, 115, 129,
    ///   133, 153, 162, 166, 180, 185), and Store must never re-derive it — guessing would make a reader open a
    ///   path that never exists.
    /// * `184 source_digest` is a digest *of* a value, is deliberately excluded from that set, and must never
    ///   appear here. An earlier revision inserted it with a fabricated length of 0; both were wrong.
    ///
    /// So the two readings t67 named are both used, for different jobs: this registry is (a), and
    /// enumeration is (b) via the format-owned predicate.
    #[must_use]
    pub fn blobs(&self) -> &[([u8; 32], u64)] {
        &self.blobs
    }

    /// The declared length of one blob, if it was declared.
    #[must_use]
    pub fn blob_len(&self, digest: &[u8; 32]) -> Option<u64> {
        self.blobs
            .iter()
            .find(|(d, _)| d == digest)
            .map(|(_, len)| *len)
    }

    /// The recorded outcome for an idempotency pair.
    ///
    /// Borrows the key pair, so a lookup allocates nothing.
    #[must_use]
    pub fn outcome(&self, principal_id: &str, request_id: &str) -> Option<&FoldedOutcome> {
        // Binary search over the sorted table: O(log n) and no allocation. This is the retry path, and the
        // table grows with committed history, so a linear scan would make every duplicate request O(n).
        // Comparing `(&str, &str)` against the stored `(String, String)` keys needs no copy.
        let probe = (principal_id, request_id);
        self.outcomes
            .binary_search_by(|((p, r), _)| (p.as_str(), r.as_str()).cmp(&probe))
            .ok()
            .map(|at| &self.outcomes[at].1)
    }

    /// Every recorded outcome, ascending by key.
    #[must_use]
    pub fn outcomes(&self) -> &[((String, String), FoldedOutcome)] {
        &self.outcomes
    }

    /// Every committed import session, keyed by id (§4.6 — not by creation time).
    #[must_use]
    pub fn imports(&self) -> &[(String, FoldedImportSession)] {
        &self.imports
    }

    /// Take ownership of the surviving fold buffers, called at the publication boundary.
    ///
    /// `try_fork` creates new table arrays and new keys/rows on the attempt's counter; those allocations outlive the facade
    /// exactly as long as the committed fold does. This adopts THAT retained extent onto the container so the claim lives
    /// with the data — it transfers credit for buffers already admitted here, and never touches unrelated scratch credit.
    ///
    /// # Errors
    ///
    /// A transfer shortfall is a `PayloadNotAdmitted` DEFECT carrying the real requested/available quantities: adoption
    /// never admits, so it can never return `Budget`, and no allocator was consulted so it is never `Allocation`. An
    /// arithmetic failure in the extent is `ExtentOverflow`, not `OwnerLayout`.
    pub fn adopt_container(
        &mut self,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> FoldOutcome<()> {
        let extent = match self.container_extent(ledger) {
            Ok(extent) => extent,
            Err(defect) => return FoldOutcome::Defect(defect),
        };
        if extent == 0 {
            return FoldOutcome::Ok(());
        }
        let adopted = match ledger.adopt_retained(extent) {
            Ok(lease) => lease,
            Err(crate::lease::TransferError::Untransferred {
                requested_bytes,
                available_bytes,
            }) => {
                // This attempt never admitted these buffers, so no allowance or allocator would help: a precondition
                // defect with the real quantities, preserved to the outer caller.
                return FoldOutcome::Defect(FoldDefect::PayloadNotAdmitted {
                    requested: requested_bytes,
                    available: available_bytes,
                });
            }
        };
        self.container = Some(adopted);
        FoldOutcome::Ok(())
    }
    /// Bytes this container retains beyond what per-record owners already cover.
    ///
    /// CHECKED arithmetic: an extent that cannot be represented is `ExtentOverflow`, because the sum genuinely failed and
    /// nothing about an owner's layout was wrong. Saturating here would pass a wrapped figure straight to `adopt_retained`.
    ///
    /// # Errors
    ///
    /// [`FoldDefect::ExtentOverflow`] when any product or sum does not fit `u64`.
    fn container_extent(
        &self,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> Result<u64, FoldDefect> {
        let mut total = ExtentSum::default();

        // The seven table arrays, at their REAL element types.
        total.add_slots::<((RecordIdentity, u64), SharedRecord)>(self.revisions.capacity())?;
        total.add_slots::<(RecordIdentity, u64)>(self.current.capacity())?;
        total.add_slots::<([u8; 32], u64)>(self.blobs.capacity())?;
        total.add_slots::<((String, String), FoldedOutcome)>(self.outcomes.capacity())?;
        total.add_slots::<(String, FoldedImportSession)>(self.imports.capacity())?;
        total.add_slots::<FoldedVersion>(self.versions.capacity())?;
        total.add_slots::<(u64, u16, [u8; 32])>(self.rules.capacity())?;

        // Owned keys inside those arrays. A slot covers its HEADER, never the storage it points at. Each loop consults the
        // caller's control, so a large retained set cannot be walked unbounded right before publication.
        for (identity, _) in &self.current {
            if let Err(stop) = SharedLedger::checkpoint(ledger) {
                return Err(FoldDefect::Stopped(stop));
            }
            if let RecordIdentity::Natural {
                key: NaturalKey::Model(name),
            } = identity
            {
                total.add(name.capacity() as u64)?;
            }
        }
        // EVERY revisions tuple key owns its own Model String, not only the `current` index row.
        for ((identity, _), _) in &self.revisions {
            if let Err(stop) = SharedLedger::checkpoint(ledger) {
                return Err(FoldDefect::Stopped(stop));
            }
            if let RecordIdentity::Natural {
                key: NaturalKey::Model(name),
            } = identity
            {
                total.add(name.capacity() as u64)?;
            }
        }
        // Outcome index keys are owned here; the event's immutable owner keeps its own block/payload claims.
        // Walked with the caller's control so a large index cannot hide cancellation immediately before publication.
        for ((principal, request), _) in &self.outcomes {
            if let Err(stop) = SharedLedger::checkpoint(ledger) {
                return Err(FoldDefect::Stopped(stop));
            }
            total.add(principal.capacity() as u64)?;
            total.add(request.capacity() as u64)?;
            // The shared event owns its own payload claim, including extension slots; do not adopt it twice.
        }
        // Import rows: index id plus the owned session record.
        for (session, import) in &self.imports {
            if let Err(stop) = SharedLedger::checkpoint(ledger) {
                return Err(FoldDefect::Stopped(stop));
            }
            total.add(session.capacity() as u64)?;
            total.add(import_owned_bytes(import)?)?;
        }
        Ok(total.0)
    }

    /// Fork this fold for a candidate attempt: tables reserved fallibly, their extent charged first.
    ///
    /// # What is fallible, and what is not
    ///
    /// Table growth is fallible: each table is charged for its exact extent and reserved with `try_reserve_exact` before
    /// any copy.
    ///
    /// Record payloads are NOT copied at all. Every revision is held behind a shared immutable owner
    /// (a private shared-owner wrapper), so forking clones owners — a refcount bump — rather than the strings and vectors inside them.
    /// That is what removes the earlier gap: there is no infallible payload copy left to waive, and the only allocation
    /// on the insertion path goes through the provider's fallible constructor.
    ///
    /// # Errors
    ///
    /// `Refused(Budget)` / `Refused(Stopped)` from the ledger; `Refused(Allocation)` when a table cannot be reserved.
    pub fn try_fork(&self, ledger: &mut dyn SharedLedger) -> FoldOutcome<Self> {
        // The ledger budgets elements and bytes as separate units, and the insert path charges both.
        let rows = self.revisions.len()
            + self.current.len()
            + self.blobs.len()
            + self.outcomes.len()
            + self.imports.len()
            + self.versions.len()
            + self.rules.len();
        if let Err(error) = ledger.try_charge_entries(rows as u64) {
            return FoldOutcome::Refused(Refusal::Budget(error));
        }

        // Byte extents: count x element size per table, charged and reserved before any copy.
        let mut fork = Self::default();
        macro_rules! fork_table {
            ($field:ident, $element:ty) => {{
                let count = self.$field.len();
                let bytes = (count as u64).saturating_mul(core::mem::size_of::<$element>() as u64);
                if let Err(error) = ledger.try_charge(bytes) {
                    return FoldOutcome::Refused(Refusal::Budget(error));
                }
                if let Err(stop) = ledger.checkpoint() {
                    return FoldOutcome::Refused(Refusal::Stopped(stop));
                }
                if fork.$field.try_reserve_exact(count).is_err() {
                    return FoldOutcome::Refused(Refusal::Allocation {
                        requested: bytes as usize,
                    });
                }
            }};
        }
        fork_table!(revisions, ((RecordIdentity, u64), SharedRecord));
        fork_table!(current, (RecordIdentity, u64));
        fork_table!(blobs, ([u8; 32], u64));
        fork_table!(outcomes, ((String, String), FoldedOutcome));
        fork_table!(imports, (String, FoldedImportSession));
        fork_table!(versions, FoldedVersion);
        fork_table!(rules, (u64, u16, [u8; 32]));

        // Records: the BODY is a refcount bump, but the tuple KEY is `(RecordIdentity, u64)` and `RecordIdentity` owns a
        // String for natural keys. So the rows are rebuilt: the body is cloned as an Arc (free) and the key is copied
        // fallibly, which is what the old `clone_from` got wrong.
        for ((identity, revision), body) in &self.revisions {
            let copied = match copy_identity(identity, ledger) {
                Ok(copied) => copied,
                Err(refusal) => return FoldOutcome::Refused(refusal),
            };
            fork.revisions.push(((copied, *revision), body.clone()));
        }

        // Records' IDENTITIES also carry String-bearing keys, and so do outcomes and imports. Those are copied
        // explicitly: admitted first, then moved into fallibly reserved slots. A `clone_from` here would deep-copy every
        // Model identity, outcome principal/request key and import session id infallibly and unadmitted.
        match fork_string_tables(&mut fork, self, ledger) {
            FoldOutcome::Ok(()) => {}
            FoldOutcome::Refused(refusal) => return FoldOutcome::Refused(refusal),
            FoldOutcome::Defect(defect) => return FoldOutcome::Defect(defect),
        }

        // Scalar rows only (a digest, a length, a version triple): copying these allocates nothing.
        fork.blobs.clone_from(&self.blobs);
        fork.versions.clone_from(&self.versions);
        fork.rules.clone_from(&self.rules);
        fork.high_water = self.high_water;
        fork.kb_uuid = self.kb_uuid;
        FoldOutcome::Ok(fork)
    }

    /// The high water mark of one id space, or `None` if that space never advanced.
    #[must_use]
    pub fn high_water(&self, space: IdSpace) -> Option<u128> {
        self.high_water[space as usize]
    }

    /// Every advanced id space.
    pub fn high_water_all(&self) -> impl Iterator<Item = (IdSpace, u128)> + '_ {
        IdSpace::ALL
            .into_iter()
            .filter_map(|space| self.high_water[space as usize].map(|hwm| (space, hwm)))
    }

    /// The `(version, profile, digest)` triples recorded per version, in commit order.
    #[must_use]
    pub fn rules(&self) -> &[(u64, u16, [u8; 32])] {
        &self.rules
    }

    /// Fold one committed transaction's events into this state.
    ///
    /// Events are taken **by value** so records move in rather than being cloned: the caller already paid to
    /// decode them, and a clone would be a second allocation for one logical event.
    ///
    /// # Contract: scratch and discard
    ///
    /// Table reservations happen before the first row mutation. Key-copy, entry or control refusal
    /// during the consuming pass may leave earlier rows applied. Capacity can grow even before a refusal.
    /// Incoming owned payloads must already have been admitted on this same ledger by their decoder;
    /// this function transfers their ownership and charges only new fold storage and index keys.
    ///
    /// The caller's obligation follows from that: fold into **scratch** state and discard it on failure. There is
    /// no rollback machinery here on purpose — the commit path builds a new snapshot rather than mutating a
    /// published one, so a discarded scratch fold is the correct recovery.
    ///
    /// # Errors
    ///
    /// `Refused` when the ledger or an allocator refuses; `Defect` for a structural problem in the input.
    pub fn apply_owning(
        &mut self,
        version: FoldedVersion,
        kb_uuid: KbUuid,
        events: Vec<FoldEvent>,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> FoldOutcome<()> {
        // Count each action kind first, so every table is reserved for its OWN element size rather than a
        // single `n` applied to seven vectors with wildly different elements (an IdAllocation is small; a
        // record-table slot plus its retained bytes is not).
        let mut record_actions = 0usize;
        let mut outcome_actions = 0usize;
        let mut import_actions = 0usize;
        let mut blob_actions = 0usize;
        for event in &events {
            // Per-action cancellation: an IdAllocation-only batch would otherwise traverse this whole pass with
            // no inner control check, so a caller's stop would be honoured only at the gate.
            if let Err(stop) = ledger.checkpoint() {
                return FoldOutcome::Refused(Refusal::Stopped(stop));
            }
            match event {
                FoldEvent::Record(_) => record_actions += 1,
                FoldEvent::Outcome(_) => outcome_actions += 1,
                FoldEvent::ImportSession(_) => import_actions += 1,
                FoldEvent::CommittedBlobRef { .. } => blob_actions += 1,
                FoldEvent::IdAllocation(_) => {}
            }
        }

        // The two append-only metadata rows are planned once per successful apply.
        // Other tables admit entries only when an insertion actually adds a row.
        if let Err(error) = ledger.try_charge_entries(2) {
            return FoldOutcome::Refused(Refusal::Budget(error));
        }
        if let Err(stop) = ledger.checkpoint() {
            return FoldOutcome::Refused(Refusal::Stopped(stop));
        }

        // Slot storage is separate from moved payload memory. Reserve only the tables
        // these actions can grow, with exact fallible growth and their own element size.
        macro_rules! grow {
            ($table:expr, $rows:expr) => {
                if let Err(refusal) = reserve_table(&mut $table, $rows, ledger) {
                    return FoldOutcome::Refused(refusal);
                }
            };
        }
        grow!(self.revisions, record_actions);
        grow!(self.current, record_actions);
        grow!(self.outcomes, outcome_actions);
        grow!(self.imports, import_actions);
        grow!(self.blobs, blob_actions);
        grow!(self.versions, 1);
        grow!(self.rules, 1);

        macro_rules! insert {
            ($table:expr, $key:expr, $value:expr $(,)?) => {
                if let Err(refusal) = insert_sorted($table, $key, $value, ledger) {
                    return FoldOutcome::Refused(refusal);
                }
            };
        }

        // 4. Past this point every insertion only moves an existing value or copies an admitted key.
        self.kb_uuid = Some(kb_uuid);
        for event in events {
            // Same bound in the mutating pass. A late stop is safe because the caller discards scratch state on
            // failure (see the contract above); what is not acceptable is an unbounded traversal.
            if let Err(stop) = ledger.checkpoint() {
                return FoldOutcome::Refused(Refusal::Stopped(stop));
            }
            match event {
                FoldEvent::Record(record) => match self.insert_record(record, ledger) {
                    FoldOutcome::Ok(()) => {}
                    other => return other,
                },
                FoldEvent::IdAllocation(allocation) => self.apply_allocation(&allocation),
                FoldEvent::Outcome(outcome) => {
                    if let Some(previous) = self.outcome(&outcome.principal_id, &outcome.request_id)
                    {
                        return FoldOutcome::Defect(FoldDefect::DuplicateOutcome {
                            original_version: previous.commit_version,
                            duplicate_version: outcome.commit_version,
                        });
                    }
                    let principal = match copy_key_bounded(&outcome.principal_id, ledger) {
                        Ok(key) => key,
                        Err(refusal) => return FoldOutcome::Refused(refusal),
                    };
                    let request = match copy_key_bounded(&outcome.request_id, ledger) {
                        Ok(key) => key,
                        Err(refusal) => return FoldOutcome::Refused(refusal),
                    };
                    let key = (principal, request);
                    insert!(
                        &mut self.outcomes,
                        key,
                        FoldedOutcome {
                            payload_digest: outcome.payload_digest,
                            commit_version: outcome.commit_version,
                            event: match crate::commit::SharedOutcome::new(outcome, ledger) {
                                Ok(shared) => shared,
                                Err(crate::commit::SharedOutcomeError::Refused(refusal)) =>
                                    return FoldOutcome::Refused(refusal),
                                Err(error) =>
                                    return FoldOutcome::Defect(FoldDefect::Outcome(error)),
                            },
                        },
                    );
                }
                FoldEvent::ImportSession(session) => {
                    let key = match copy_key_bounded(&session.import_session_id, ledger) {
                        Ok(key) => key,
                        Err(refusal) => return FoldOutcome::Refused(refusal),
                    };
                    insert!(
                        &mut self.imports,
                        key,
                        FoldedImportSession { record: session },
                    );
                }
                FoldEvent::CommittedBlobRef {
                    sha256,
                    declared_len,
                } => {
                    insert!(&mut self.blobs, sha256, declared_len);
                }
            }
        }

        // `event_count` is the WIRE event count and is NOT derived from this slice. The slice holds physical
        // actions: a Genesis contributes two records, and a `CommittedBlobRef` is a publication input rather
        // than an event at all, so counting actions would record a number the transaction never had.
        //
        // It therefore stays exactly as the caller supplied it, having been validated against the real wire
        // count by the format reader (S2 §4.2 step 0e). The fold states that as a precondition in its
        // documentation rather than re-deriving or re-checking it.
        self.rules
            .push((version.version, version.rules_profile, version.rules_digest));
        self.versions.push(version);
        FoldOutcome::Ok(())
    }

    /// Insert one record revision and make it current.
    ///
    /// # Errors
    ///
    /// `Defect(HistoryCollision)` when this `(identity, revision)` already exists. Overwriting would be the
    /// silent repair MODEL §3 rule 1 forbids: two different images under one identity make the retained
    /// history locator ambiguous, and a replay could not then say which bytes the chain contains.
    fn insert_record(
        &mut self,
        record: FoldedRecord,
        ledger: &mut dyn crate::lease::LeaseLedger,
    ) -> FoldOutcome<()> {
        let identity = match record.identity(ledger) {
            Ok(identity) => identity,
            Err(refusal) => return FoldOutcome::Refused(refusal),
        };
        let revision = record.revision().get();
        if self
            .revisions
            .binary_search_by(|((id, rev), _)| (id, rev).cmp(&(&identity, &revision)))
            .is_ok()
        {
            // Same identity and revision already folded with different content: this is a history collision,
            // not an update. An idempotent re-fold of identical bytes is also refused here rather than
            // compared, because a fold of a committed chain sees each revision once by construction.
            return FoldOutcome::Defect(FoldDefect::HistoryCollision { identity, revision });
        }
        let current_slot = self.current.binary_search_by(|(id, _)| id.cmp(&identity));

        // REVISION CONTINUITY — the one physical checker both commit and replay reach, because both go through this
        // apply path. A creation is revision 1; an update must advance the identity's current revision by exactly one.
        // Expressed as a fold defect (a property of the candidate/chain), never a new `Format::Corrupt` class.
        // MODEL §3 rule 1, enforced in the ONE physical insertion path that commit and any future replay both reach:
        // a CREATION is revision 1, and an update is exactly `previous + 1`. The missing-identity branch previously
        // accepted any nonzero revision, which let a never-seen record commit at revision 2. Held P1-5 partial replay gives
        // no authority to relax the complete commit path.
        let expected = match current_slot {
            Ok(at) => self.current[at].1.checked_add(1),
            Err(_) => Some(1),
        };
        if expected != Some(revision) {
            return FoldOutcome::Defect(FoldDefect::RevisionDiscontinuity {
                identity,
                expected,
                found: revision,
            });
        }
        // Revisions and current own separate keys. Copy the second key only when
        // current gains a row, admitting and reserving that String before insertion.
        let new_current_key = if current_slot.is_err() {
            match record.identity(ledger) {
                Ok(key) => Some(key),
                Err(refusal) => return FoldOutcome::Refused(refusal),
            }
        } else {
            None
        };
        let shared = match SharedRecord::new(record, ledger) {
            FoldOutcome::Ok(shared) => shared,
            FoldOutcome::Refused(refusal) => return FoldOutcome::Refused(refusal),
            FoldOutcome::Defect(defect) => return FoldOutcome::Defect(defect),
        };
        if let Err(refusal) =
            insert_sorted(&mut self.revisions, (identity, revision), shared, ledger)
        {
            return FoldOutcome::Refused(refusal);
        }
        match current_slot {
            Ok(at) => self.current[at].1 = revision,
            Err(_) => {
                if let Some(key) = new_current_key
                    && let Err(refusal) = insert_sorted(&mut self.current, key, revision, ledger)
                {
                    return FoldOutcome::Refused(refusal);
                }
            }
        }
        FoldOutcome::Ok(())
    }

    /// Apply an `IdAllocation`: the mark is a **maximum**, never a sum.
    ///
    /// A retried or out-of-order allocation therefore cannot inflate the mark past what was actually
    /// allocated, which is the property `bookkeeping_digest` depends on.
    fn apply_allocation(&mut self, allocation: &IdAllocationEvent) {
        let space = allocation.allocation.space;
        let observed = allocation.allocation.hwm;
        let slot = &mut self.high_water[space as usize];
        *slot = Some(slot.map_or(observed, |current| current.max(observed)));
    }
}

/// Admit only newly requested slot storage, then reserve it fallibly.
/// Existing spare capacity is credit; exact reservation avoids geometric overgrowth.
fn reserve_table<T>(
    table: &mut Vec<T>,
    additional: usize,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Refusal> {
    let needed = table.len().saturating_add(additional);
    let deficit = needed.saturating_sub(table.capacity());
    if deficit == 0 {
        return Ok(());
    }
    let bytes = deficit.saturating_mul(core::mem::size_of::<T>());
    ledger.try_charge(bytes as u64)?;
    ledger.checkpoint()?;
    table
        .try_reserve_exact(additional)
        .map_err(|_| Refusal::Allocation { requested: bytes })
}

/// Copy a key string with an admission and a fallible reservation before the copy.
///
/// `String::clone` on a bounded path is neither inherent nor waived: it allocates infallibly and uncharged.
/// This does the same work in the order the resource rule requires — admit the length, check control, reserve
/// exactly, then copy.
fn copy_key_bounded(value: &str, ledger: &mut dyn SharedLedger) -> Result<String, Refusal> {
    if let Err(error) = ledger.try_charge(value.len() as u64) {
        return Err(Refusal::Budget(error));
    }
    if let Err(stop) = ledger.checkpoint() {
        return Err(Refusal::Stopped(stop));
    }
    let mut out = String::new();
    if out.try_reserve_exact(value.len()).is_err() {
        return Err(Refusal::Allocation {
            requested: value.len(),
        });
    }
    out.push_str(value);
    Ok(out)
}

/// Insert into a sorted `Vec`, replacing an equal key.
///
/// Capacity was reserved fallibly before any insert in this call, so the `insert` below cannot need to
/// allocate; it moves a value that already exists.
fn insert_sorted<K: Ord, V>(
    vec: &mut Vec<(K, V)>,
    key: K,
    value: V,
    ledger: &mut dyn SharedLedger,
) -> Result<(), Refusal> {
    match vec.binary_search_by(|(k, _)| k.cmp(&key)) {
        Ok(at) => vec[at].1 = value,
        Err(at) => {
            ledger.try_charge_entries(1)?;
            // All capacity has been reserved before the consuming pass.
            debug_assert!(vec.len() < vec.capacity());
            vec.insert(at, (key, value));
        }
    }
    Ok(())
}

/// Which identities the shared traversal spine serves.
///
/// The two public scans differ **only** here, so a defect in the walk itself is a
/// defect in both and a family cannot be added to one arm alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CurrentSelection {
    /// Numeric identities only; natural-key metadata is outside that API by design.
    Identified,
    /// Every current record, identified or natural.
    All,
}

impl CurrentSelection {
    /// Whether the walk serves `identity`.
    const fn includes(self, identity: &RecordIdentity) -> bool {
        match self {
            Self::All => true,
            Self::Identified => matches!(identity, RecordIdentity::Identified { .. }),
        }
    }
}

/// The scalar missing-owner verdict for one index row.
///
/// Numeric keys are `Copy` and small, so the existing
/// [`crate::view::RecordReadError::MissingRetainedOwner`] names them exactly. A
/// natural key may own a `String`, so it is reported by its state kind and revision
/// instead of copying it: the error path still allocates nothing.
fn missing_owner(identity: &RecordIdentity, revision: u64) -> crate::view::RecordReadError {
    match identity {
        RecordIdentity::Identified { key } => crate::view::RecordReadError::MissingRetainedOwner {
            key: *key,
            revision,
        },
        RecordIdentity::Natural { .. } => {
            crate::view::RecordReadError::MissingNaturalRetainedOwner {
                kind: identity.state_kind(),
                revision,
            }
        }
    }
}

/// One control + entry charge, in the compact lookup vocabulary.
fn read_step(ledger: &mut dyn SharedLedger) -> Result<(), FixedReadError> {
    ledger
        .checkpoint()
        .map_err(|stop| FixedReadError::Refused(Refusal::Stopped(stop)))?;
    ledger
        .try_charge_entries(1)
        .map_err(|error| FixedReadError::Refused(Refusal::Budget(error)))
}

/// The same step, attributed to a scan.
///
/// Kept separate from [`read_step`] rather than converting its compact error: the
/// only refusal a step can produce is a refusal, and inventing a missing-owner key to
/// satisfy a conversion would put a fabricated identity on a path that can never
/// reach it.
fn index_step<E>(ledger: &mut dyn SharedLedger) -> Result<(), crate::view::RecordScanError<E>> {
    ledger.checkpoint().map_err(|stop| {
        crate::view::RecordScanError::Read(crate::view::RecordReadError::Refused(Refusal::Stopped(
            stop,
        )))
    })?;
    ledger.try_charge_entries(1).map_err(|error| {
        crate::view::RecordScanError::Read(crate::view::RecordReadError::Refused(Refusal::Budget(
            error,
        )))
    })
}

/// Bytes compared per charged step of a variable-length key.
const KEY_COMPARE_CHUNK: usize = 16;

/// Compare two model keys as `String`'s `Ord` would, charging only the bytes walked.
///
/// `str` orders byte-wise over UTF-8, so comparing chunk prefixes and then the
/// remaining length yields exactly the same answer while never touching more than
/// [`KEY_COMPARE_CHUNK`] bytes between two ledger checks.
///
/// One entry is charged **before** each chunk comparison and one more before the
/// final length comparison, so a stopped or exhausted caller never has bytes
/// examined on its behalf. The search iteration that calls this has already paid for
/// the row it is looking at; these charges are the extra work a fixed-shape key does
/// not do.
fn bounded_key_cmp(
    left: &str,
    right: &str,
    ledger: &mut dyn SharedLedger,
) -> Result<core::cmp::Ordering, FixedReadError> {
    use core::cmp::Ordering;
    let a = left.as_bytes();
    let b = right.as_bytes();
    let mut at = 0;
    while at < a.len() && at < b.len() {
        read_step(ledger)?;
        let end = (at + KEY_COMPARE_CHUNK).min(a.len()).min(b.len());
        match a[at..end].cmp(&b[at..end]) {
            Ordering::Equal => at = end,
            other => return Ok(other),
        }
    }
    read_step(ledger)?;
    Ok(a.len().cmp(&b.len()))
}

/// Position of a natural family in [`NaturalKey`]'s derived `Ord`.
fn natural_family_rank(key: &NaturalKey) -> u8 {
    match key {
        NaturalKey::Asset(_) => 0,
        NaturalKey::Model(_) => 1,
        NaturalKey::KbMetadata => 2,
        NaturalKey::RuleConfig => 3,
    }
}

/// Compare two natural keys of the same family whose payload is fixed-shape.
fn natural_scalar_cmp(left: &NaturalKey, right: &NaturalKey) -> core::cmp::Ordering {
    match (left, right) {
        (NaturalKey::Asset(a), NaturalKey::Asset(b)) => a.cmp(b),
        // Same family, no payload: equal. Models take `bounded_key_cmp` instead, and
        // a mixed pair is already decided by the rank.
        _ => core::cmp::Ordering::Equal,
    }
}

/// Compare two identities, charging only the work a fixed shape does not do.
///
/// The caller (one search iteration) has already paid one entry for the row being
/// examined, so a fixed-shape comparison must **not** charge again: doing so would
/// double the identified scan's per-row cost for no additional work. Only a model
/// key, whose comparison walks caller-controlled bytes, adds its own steps through
/// [`bounded_key_cmp`].
///
/// Every arm returns exactly the answer `RecordIdentity`'s derived `Ord` would give,
/// so the index order maintained by `insert_sorted` and the metered search cannot
/// disagree.
fn identity_cmp_charged(
    left: &RecordIdentity,
    right: &RecordIdentity,
    ledger: &mut dyn SharedLedger,
) -> Result<core::cmp::Ordering, FixedReadError> {
    use core::cmp::Ordering;
    match (left, right) {
        (RecordIdentity::Identified { key: a }, RecordIdentity::Identified { key: b }) => {
            Ok(a.cmp(b))
        }
        (
            RecordIdentity::Natural {
                key: NaturalKey::Model(a),
            },
            RecordIdentity::Natural {
                key: NaturalKey::Model(b),
            },
        ) => bounded_key_cmp(a, b, ledger),
        (RecordIdentity::Natural { key: a }, RecordIdentity::Natural { key: b }) => {
            Ok(natural_family_rank(a)
                .cmp(&natural_family_rank(b))
                .then_with(|| natural_scalar_cmp(a, b)))
        }
        // `RecordIdentity`'s derived `Ord` puts `Identified` before `Natural`.
        (RecordIdentity::Identified { .. }, RecordIdentity::Natural { .. }) => Ok(Ordering::Less),
        (RecordIdentity::Natural { .. }, RecordIdentity::Identified { .. }) => {
            Ok(Ordering::Greater)
        }
    }
}

/// One event as the fold consumes it, **owned**.
///
/// Owned rather than borrowed so [`StateFold::apply_owning`] can move each record in. A borrowed variant would
/// force a clone per event, which is an allocation for bytes the fold is supposed to take over.
// The variants differ in size because a record is large and an allocation is small. Boxing the record would
// remove the size difference and reintroduce an infallible allocation on the commit path, which is the
// property this module exists to avoid. The size is bounded by the largest record type, and events are
// carried one attempt at a time.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FoldEvent {
    /// A record-bearing event.
    Record(FoldedRecord),
    /// An `IdAllocation`.
    IdAllocation(IdAllocationEvent),
    /// A `RequestOutcomeRecorded`.
    Outcome(RequestOutcomeEvent),
    /// An `ImportBookkeeping`.
    ImportSession(ImportSessionRecord),
    /// A **required** blob reference that this transaction committed, with its verified length.
    ///
    /// The role is narrow on purpose. Three different sets exist and only one belongs here:
    ///
    /// * the **declared superset** in `PreparedTransaction.blobs` — what the caller offered to publish;
    /// * the **required set** — the refs the format actually enumerates for this transaction's events
    ///   (`dispatch::is_blob_ref_tag`, the tags of `storage-digests.md` §2);
    /// * this fold's cumulative state — the union of required refs over committed history.
    ///
    /// Only required refs reach the fold. A declared-but-unreferenced blob may be a durable orphan, and a
    /// replay could never derive it from the chain, so inserting it would make fold state disagree with
    /// recovery. `declared_len` is the length of bytes that were verified; a fabricated zero is never used,
    /// and a genuinely empty blob is legal only because its verification said so.
    CommittedBlobRef {
        /// Content address, as encoded in the referring field.
        sha256: [u8; 32],
        /// Verified length of those bytes.
        declared_len: u64,
    },
}

/// Copy the String-bearing fold tables into `fork`, admitting and reserving every byte first.
///
/// Natural Model identities, outcome index keys and import records require physical copies, admitted and reserved before
/// filling. Outcome payloads and record bodies share their existing immutable owners instead of allocating new copies.
///
/// # Errors
///
/// `Refused(Budget)` / `Refused(Stopped)` from the ledger; `Refused(Allocation)` when a reservation fails.
fn fork_string_tables(
    fork: &mut StateFold,
    source: &StateFold,
    ledger: &mut dyn SharedLedger,
) -> FoldOutcome<()> {
    // The `fork_table!` macro has ALREADY charged and reserved these three tables' slot extents. Charging them again here
    // would bill an allocation that does not happen, so this function charges only what the macro cannot see: the owned
    // payload inside each element (the identity keys, the outcome key pair plus its owned event, the import id and record).

    // Admit the payload once, from real capacities.
    // NO bulk payload charge here. Each copied buffer is admitted exactly once inside `copy_string` / `copy_extensions` /
    // the per-field builders below, which also reserve it. A bulk sum followed by per-buffer admissions billed the same
    // bytes twice, and a bulk sum alone could not make the individual allocations fallible.

    // Build each table by FALLIBLE construction: every owned String is reserved, then filled. No `clone_from` remains, so
    // no inner allocation is infallible. Values and ordering are preserved exactly — these are the same keys, the same
    // revisions and the same event payloads, only built explicitly.
    for (identity, revision) in &source.current {
        let copied = match copy_identity(identity, ledger) {
            Ok(copied) => copied,
            Err(refusal) => return FoldOutcome::Refused(refusal),
        };
        fork.current.push((copied, *revision));
    }
    for ((principal, request), outcome) in &source.outcomes {
        let copied_principal = match copy_string(principal, ledger) {
            Ok(copied) => copied,
            Err(refusal) => return FoldOutcome::Refused(refusal),
        };
        let copied_request = match copy_string(request, ledger) {
            Ok(copied) => copied,
            Err(refusal) => return FoldOutcome::Refused(refusal),
        };
        let key = (copied_principal, copied_request);
        let copied = match copy_outcome(outcome) {
            FoldOutcome::Ok(copied) => copied,
            FoldOutcome::Refused(refusal) => return FoldOutcome::Refused(refusal),
            FoldOutcome::Defect(defect) => return FoldOutcome::Defect(defect),
        };
        fork.outcomes.push((key, copied));
    }
    for (session, import) in &source.imports {
        let id = match copy_string(session, ledger) {
            Ok(copied) => copied,
            Err(refusal) => return FoldOutcome::Refused(refusal),
        };
        let copied = match copy_import(import, ledger) {
            FoldOutcome::Ok(copied) => copied,
            FoldOutcome::Refused(refusal) => return FoldOutcome::Refused(refusal),
            FoldOutcome::Defect(defect) => return FoldOutcome::Defect(defect),
        };
        fork.imports.push((id, copied));
    }
    FoldOutcome::Ok(())
}

/// Reserve and fill a `String` from `source`, so the allocation is fallible and admitted.
fn copy_string(source: &str, ledger: &mut dyn SharedLedger) -> Result<String, Refusal> {
    // ONE quantity: the new buffer reserves exactly `source.len()`, so that is what is admitted and what a refusal
    // reports. Billing `source.capacity()` charged spare capacity the new allocation never asks for.
    let needed = source.len() as u64;
    ledger.try_charge(needed)?;
    ledger.checkpoint()?;
    let mut out = String::new();
    if out.try_reserve_exact(source.len()).is_err() {
        return Err(Refusal::Allocation {
            requested: source.len(),
        });
    }
    out.push_str(source);
    Ok(out)
}

/// Copy a record identity, reserving its owned key when it has one.
fn copy_identity(
    identity: &RecordIdentity,
    ledger: &mut dyn SharedLedger,
) -> Result<RecordIdentity, Refusal> {
    Ok(match identity {
        RecordIdentity::Identified { key } => RecordIdentity::Identified { key: *key },
        RecordIdentity::Natural { key } => RecordIdentity::Natural {
            key: copy_natural_key(key, ledger)?,
        },
    })
}

/// Copy a natural key, reserving the String it owns (only `Model` has one).
fn copy_natural_key(
    key: &NaturalKey,
    ledger: &mut dyn SharedLedger,
) -> Result<NaturalKey, Refusal> {
    Ok(match key {
        NaturalKey::Model(name) => NaturalKey::Model(copy_string(name, ledger)?),
        NaturalKey::Asset(digest) => NaturalKey::Asset(*digest),
        NaturalKey::KbMetadata => NaturalKey::KbMetadata,
        NaturalKey::RuleConfig => NaturalKey::RuleConfig,
    })
}

/// A fork shares the already-admitted event owner; only the separate index keys are copied.
fn copy_outcome(outcome: &FoldedOutcome) -> FoldOutcome<FoldedOutcome> {
    FoldOutcome::Ok(outcome.clone())
}

/// `copy_string` in `FoldOutcome` form, so the caller can propagate either a refusal or a defect.
fn copy_string_out(source: &str, ledger: &mut dyn SharedLedger) -> FoldOutcome<String> {
    match copy_string(source, ledger) {
        Ok(copied) => FoldOutcome::Ok(copied),
        Err(refusal) => FoldOutcome::Refused(refusal),
    }
}

/// The same for an optional String.
fn copy_opt_string(
    source: &Option<String>,
    ledger: &mut dyn SharedLedger,
) -> FoldOutcome<Option<String>> {
    match source {
        None => FoldOutcome::Ok(None),
        Some(text) => match copy_string_out(text.as_str(), ledger) {
            FoldOutcome::Ok(copied) => FoldOutcome::Ok(Some(copied)),
            FoldOutcome::Refused(refusal) => FoldOutcome::Refused(refusal),
            FoldOutcome::Defect(defect) => FoldOutcome::Defect(defect),
        },
    }
}

/// Rebuild an `Extensions` table, admitting each entry's header slot, key block and value block once.
fn copy_extensions(
    source: &sage_db_types::persist::Extensions,
    ledger: &mut dyn SharedLedger,
) -> FoldOutcome<sage_db_types::persist::Extensions> {
    let entries = source.entries();
    let slots =
        (entries.len() as u64).saturating_mul(core::mem::size_of::<(String, Vec<u8>)>() as u64);
    if entries.is_empty() {
        return FoldOutcome::Ok(sage_db_types::persist::Extensions::new());
    }
    if let Err(error) = ledger.try_charge(slots) {
        return FoldOutcome::Refused(Refusal::Budget(error));
    }
    if let Err(stop) = ledger.checkpoint() {
        return FoldOutcome::Refused(Refusal::Stopped(stop));
    }
    let mut rebuilt: Vec<(String, Vec<u8>)> = Vec::new();
    if rebuilt.try_reserve_exact(entries.len()).is_err() {
        return FoldOutcome::Refused(Refusal::Allocation {
            requested: slots as usize,
        });
    }
    for (key, value) in entries {
        let copied_key = match copy_string(key, ledger) {
            Ok(key) => key,
            Err(refusal) => return FoldOutcome::Refused(refusal),
        };
        // Charge and report the NEW reserve, not the old buffer's spare slots: the copy allocates `value.len()`.
        let bytes = value.len() as u64;
        if let Err(error) = ledger.try_charge(bytes) {
            return FoldOutcome::Refused(Refusal::Budget(error));
        }
        if let Err(stop) = ledger.checkpoint() {
            return FoldOutcome::Refused(Refusal::Stopped(stop));
        }
        let mut copied_value: Vec<u8> = Vec::new();
        if copied_value.try_reserve_exact(value.len()).is_err() {
            return FoldOutcome::Refused(Refusal::Allocation {
                requested: value.len(),
            });
        }
        copied_value.extend_from_slice(value);
        rebuilt.push((copied_key, copied_value));
    }
    // The source was canonical, so a rebuilt copy with the same keys and values is canonical too. If it is not, that is a
    // real defect and must surface rather than be replaced by an empty table (which would silently drop data).
    match sage_db_types::persist::Extensions::from_canonical(rebuilt) {
        Ok(rebuilt) => FoldOutcome::Ok(rebuilt),
        // The source was canonical, so a rebuilt copy with the same keys is canonical: rejection here is a defect in the
        // copy, reported as one rather than disguised as a budget or allocator refusal.
        Err(_) => FoldOutcome::Defect(FoldDefect::CopyNotCanonical),
    }
}

/// Copy an import session record, reserving its owned buffers.
fn copy_import(
    import: &FoldedImportSession,
    ledger: &mut dyn SharedLedger,
) -> FoldOutcome<FoldedImportSession> {
    let source = &import.record;
    // No `import.record.clone()`: that first clone allocates every owned field, including expected_counts and
    // extensions, infallibly. Each buffer is built here with its own admission and reserve.
    let import_session_id = match copy_string_out(&source.import_session_id, ledger) {
        FoldOutcome::Ok(value) => value,
        FoldOutcome::Refused(refusal) => return FoldOutcome::Refused(refusal),
        FoldOutcome::Defect(defect) => return FoldOutcome::Defect(defect),
    };
    let mut expected_counts: Vec<sage_db_types::persist::CountPair> = Vec::new();
    let counts_bytes = (source.expected_counts.len() as u64)
        .saturating_mul(core::mem::size_of::<sage_db_types::persist::CountPair>() as u64);
    if counts_bytes > 0 {
        if let Err(error) = ledger.try_charge(counts_bytes) {
            return FoldOutcome::Refused(Refusal::Budget(error));
        }
        if let Err(stop) = ledger.checkpoint() {
            return FoldOutcome::Refused(Refusal::Stopped(stop));
        }
        if expected_counts
            .try_reserve_exact(source.expected_counts.len())
            .is_err()
        {
            return FoldOutcome::Refused(Refusal::Allocation {
                requested: counts_bytes as usize,
            });
        }
        expected_counts.extend(source.expected_counts.iter().copied());
    }
    let recorded_by = match copy_opt_string(&source.audit.recorded_by, ledger) {
        FoldOutcome::Ok(value) => value,
        FoldOutcome::Refused(refusal) => return FoldOutcome::Refused(refusal),
        FoldOutcome::Defect(defect) => return FoldOutcome::Defect(defect),
    };
    let reason = match copy_opt_string(&source.audit.reason, ledger) {
        FoldOutcome::Ok(value) => value,
        FoldOutcome::Refused(refusal) => return FoldOutcome::Refused(refusal),
        FoldOutcome::Defect(defect) => return FoldOutcome::Defect(defect),
    };
    let extensions = match copy_extensions(&source.extensions, ledger) {
        FoldOutcome::Ok(ext) => ext,
        FoldOutcome::Refused(refusal) => return FoldOutcome::Refused(refusal),
        FoldOutcome::Defect(defect) => return FoldOutcome::Defect(defect),
    };
    FoldOutcome::Ok(FoldedImportSession {
        record: sage_db_types::persist::ImportSessionRecord {
            import_session_id,
            stage: source.stage,
            source_digest: source.source_digest,
            plan_ref: source.plan_ref,
            expected_counts,
            import_complete: source.import_complete,
            audit: sage_db_types::persist::CreationAudit {
                recorded_by,
                actor_origin: source.audit.actor_origin,
                reason,
                created_at: source.audit.created_at,
            },
            extensions,
        },
    })
}

/// Bytes a record's own nested allocations retain.
///
/// Public because admitting them is the CALLER's obligation: the fold transfers already-admitted payload credit rather than
/// charging it (charging twice would be the double-charge defect), so whoever decodes or builds a record must reserve this
/// extent on the same ledger before handing it over.
///
/// # Errors
///
/// [`FoldDefect::ExtentOverflow`] when the record's retained extent does not fit `u64`.
pub fn record_retained_bytes(record: &FoldedRecord) -> Result<u64, FoldDefect> {
    record_owned_bytes(record)
}

/// Bytes the record's own nested allocations retain, from real capacities.
///
/// Every family is enumerated by its actual owned fields — `common` (recorded_by/reason), `extensions`, `changed_fields`,
/// legacy strings, and each family's own Strings/Vecs. Nothing returns `0` for a family that owns data: a zero arm in a
/// retained-extent function is a partial sum presented as an extent, not a conservative estimate.
///
/// Capacity (not length) is summed, because capacity is what the allocator handed out and therefore what is retained.
fn record_owned_bytes(record: &FoldedRecord) -> Result<u64, FoldDefect> {
    // CHECKED: this figure is passed directly to `adopt_retained` (see `SharedRecord::new`), so a saturated sum would
    // adopt the wrong extent. Overflow is a real local defect, never a fabricated allocator or budget refusal.
    fn s(text: &String) -> Result<u64, FoldDefect> {
        Ok(text.capacity() as u64)
    }
    fn opt(text: &Option<String>) -> Result<u64, FoldDefect> {
        match text {
            Some(text) => s(text),
            None => Ok(0),
        }
    }
    fn strs(items: &Vec<String>) -> Result<u64, FoldDefect> {
        let mut sum = ExtentSum::default();
        sum.add_slots::<String>(items.capacity())?;
        for item in items {
            sum.add(item.capacity() as u64)?;
        }
        Ok(sum.0)
    }
    fn extensions(ext: &sage_db_types::persist::Extensions) -> Result<u64, FoldDefect> {
        let mut sum = ExtentSum::default();
        sum.add_slots::<(String, Vec<u8>)>(ext.capacity())?;
        for (key, value) in ext.entries() {
            sum.add(key.capacity() as u64)?;
            sum.add(value.capacity() as u64)?;
        }
        Ok(sum.0)
    }
    fn plain<T>(items: &Vec<T>) -> Result<u64, FoldDefect> {
        let mut sum = ExtentSum::default();
        sum.add_slots::<T>(items.capacity())?;
        Ok(sum.0)
    }
    fn common(fields: &sage_db_types::persist::CommonFields) -> Result<u64, FoldDefect> {
        let mut sum = ExtentSum::default();
        sum.add(opt(&fields.recorded_by)?)?;
        sum.add(opt(&fields.reason)?)?;
        Ok(sum.0)
    }
    fn optional_audit(
        fields: &sage_db_types::persist::OptionalActorAudit,
    ) -> Result<u64, FoldDefect> {
        let mut sum = ExtentSum::default();
        sum.add(opt(&fields.recorded_by)?)?;
        sum.add(opt(&fields.reason)?)?;
        Ok(sum.0)
    }
    fn creation_audit(fields: &sage_db_types::persist::CreationAudit) -> Result<u64, FoldDefect> {
        let mut sum = ExtentSum::default();
        sum.add(opt(&fields.recorded_by)?)?;
        sum.add(opt(&fields.reason)?)?;
        Ok(sum.0)
    }

    let mut t = ExtentSum::default();
    match record {
        FoldedRecord::KbMetadata(r) => {
            t.add(s(&r.slug)?)?;
            t.add(s(&r.name)?)?;
            t.add(s(&r.description)?)?;
            t.add(strs(&r.domains)?)?;
            t.add(optional_audit(&r.audit)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::RuleConfig(r) => {
            t.add(optional_audit(&r.audit)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::Term(r) => {
            t.add(s(&r.slug)?)?;
            t.add(s(&r.atom)?)?;
            t.add(strs(&r.domains)?)?;
            t.add(strs(&r.triggers)?)?;
            t.add(opt(&r.legacy_id)?)?;
            t.add(opt(&r.legacy_source_path)?)?;
            t.add(common(&r.common)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::Asset(r) => {
            t.add(opt(&r.declared_by)?)?;
            t.add(creation_audit(&r.audit)?)?;
            t.add(extensions(&r.extensions)?)?;
        }
        FoldedRecord::Assertion(r) => {
            t.add(opt(&r.legacy_id)?)?;
            t.add(common(&r.common)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::Claim(r) => {
            t.add(opt(&r.legacy_id)?)?;
            t.add(common(&r.common)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::Evidence(r) => {
            t.add(s(&r.title)?)?;
            t.add(opt(&r.locator)?)?;
            t.add(opt(&r.declared_captured_by)?)?;
            t.add(common(&r.common)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::EvidenceLink(r) => {
            t.add(opt(&r.legacy_id)?)?;
            t.add(common(&r.common)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::Vector(r) => {
            t.add(s(&r.model_id)?)?;
            t.add(common(&r.common)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::Proposal(r) => {
            t.add(plain(&r.base_revisions)?)?;
            t.add(opt(&r.review_note)?)?;
            t.add(s(&r.declared_author)?)?;
            t.add(opt(&r.legacy_id)?)?;
            t.add(common(&r.common)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::FrontierEntry(r) => {
            t.add(plain(&r.concept_refs)?)?;
            t.add(common(&r.common)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::SessionCheckpoint(r) => {
            if let sage_db_types::persist::CheckpointLocation::Slug { slug, .. } = &r.location {
                t.add(s(slug)?)?;
            }
            t.add(common(&r.common)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
        FoldedRecord::Model(r) => {
            t.add(s(&r.model_key)?)?;
            t.add(s(&r.identifier)?)?;
            t.add(opt(&r.provider)?)?;
            t.add(common(&r.common)?)?;
            t.add(extensions(&r.extensions)?)?;
            if let Some(fields) = &r.changed_fields {
                t.add(plain(fields)?)?;
            }
        }
    }
    Ok(t.0)
}

/// A checked running total for retained extents.
///
/// Exists because these sums feed an ownership ADOPTION: a saturating or wrapped figure would adopt the wrong amount, so
/// every step is checked and overflow is a real local defect.
#[derive(Default)]
struct ExtentSum(u64);

impl ExtentSum {
    /// Add one table's slot array at its real element type.
    fn add_slots<T>(&mut self, count: usize) -> Result<(), FoldDefect> {
        let bytes = (count as u64)
            .checked_mul(core::mem::size_of::<T>() as u64)
            .ok_or(FoldDefect::ExtentOverflow)?;
        self.add(bytes)
    }

    /// Add an owned buffer's bytes.
    fn add(&mut self, bytes: u64) -> Result<(), FoldDefect> {
        self.0 = self
            .0
            .checked_add(bytes)
            .ok_or(FoldDefect::ExtentOverflow)?;
        Ok(())
    }
}

/// Bytes a folded import session's record owns, with checked arithmetic.
fn import_owned_bytes(import: &FoldedImportSession) -> Result<u64, FoldDefect> {
    let record = &import.record;
    let mut total = 0u64;
    let mut add = |bytes: u64| -> Result<(), FoldDefect> {
        total = total.checked_add(bytes).ok_or(FoldDefect::ExtentOverflow)?;
        Ok(())
    };
    add(record.import_session_id.capacity() as u64)?;
    add((record.expected_counts.capacity() as u64)
        .checked_mul(core::mem::size_of::<sage_db_types::persist::CountPair>() as u64)
        .ok_or(FoldDefect::ExtentOverflow)?)?;
    if let Some(text) = &record.audit.recorded_by {
        add(text.capacity() as u64)?;
    }
    if let Some(text) = &record.audit.reason {
        add(text.capacity() as u64)?;
    }
    add((record.extensions.capacity() as u64)
        .checked_mul(core::mem::size_of::<(String, Vec<u8>)>() as u64)
        .ok_or(FoldDefect::ExtentOverflow)?)?;
    for (key, value) in record.extensions.entries() {
        add(key.capacity() as u64)?;
        add(value.capacity() as u64)?;
    }
    Ok(total)
}

#[cfg(test)]
mod reservation_tests {
    use super::*;
    use sage_db_types::ledger::{BudgetError, BudgetUnit};
    use sage_db_types::query::ControlStop;

    struct Bytes {
        left: u64,
        charged: u64,
    }
    impl SharedLedger for Bytes {
        fn try_charge(&mut self, n: u64) -> Result<(), BudgetError> {
            if n > self.left {
                return Err(BudgetError::new(BudgetUnit::Bytes, n, self.left));
            }
            self.left -= n;
            self.charged += n;
            Ok(())
        }
        fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    #[test]
    fn exact_reservation_matches_the_admitted_capacity_growth() {
        let mut table: Vec<u8> = Vec::new();
        table.try_reserve_exact(1).expect("test setup");
        table.push(1);
        let before = table.capacity();
        let mut ledger = Bytes {
            left: 1,
            charged: 0,
        };
        reserve_table(&mut table, 1, &mut ledger).expect("one new slot");
        assert_eq!(ledger.charged, 1);
        assert_eq!(
            table.capacity() - before,
            1,
            "a geometric allocation cannot be paid as one extra slot"
        );
        table.push(2);
        assert_eq!(table, [1, 2]);
    }

    #[test]
    fn spare_capacity_is_not_charged_again() {
        let mut table: Vec<u64> = Vec::new();
        table.try_reserve_exact(4).expect("test setup");
        table.push(1);
        let before = table.capacity();
        let mut ledger = Bytes {
            left: 0,
            charged: 0,
        };
        reserve_table(&mut table, 3, &mut ledger).expect("existing credit");
        assert_eq!(ledger.charged, 0);
        assert_eq!(table.capacity(), before);
    }
}

#[cfg(test)]
mod retained_extent_tests {
    //! Discriminators over String-bearing rows: a zero-retained-payload mutant must fail these.
    use super::*;
    use crate::lease::AttemptLedger;

    fn capable() -> AttemptLedger<'static> {
        let control: &'static sage_db_types::query::RunToCompletion =
            Box::leak(Box::new(sage_db_types::query::RunToCompletion));
        AttemptLedger::open(u64::MAX, u64::MAX, control).expect("opens")
    }

    fn model(key: &str) -> FoldedRecord {
        FoldedRecord::Model(sage_db_types::persist::ModelRecord {
            revision: sage_db_types::identity::RecordRevision::FIRST,
            model_key: key.to_owned(),
            identifier: "identifier-value".to_owned(),
            provider: Some("provider-name".to_owned()),
            dimensions: 8,
            status: sage_db_types::persist::ModelStatus::Active,
            common: sage_db_types::persist::CommonFields {
                recorded_by: Some("recorder".to_owned()),
                actor_origin: sage_db_types::persist::ActorOrigin::Authenticated,
                reason: Some("because".to_owned()),
                created_at: 0,
                updated_at: None,
            },
            extensions: sage_db_types::persist::Extensions::new(),
            changed_fields: None,
        })
    }

    #[test]
    fn the_model_family_reports_nonzero_owned_bytes_and_extensions_add_to_it() {
        // SCOPE, stated so the name cannot overclaim: only `Model` is constructed here. The previous nine-arm zero version
        // passed no test at all, which is why it survived; this proves one family, not all thirteen. Other families are
        // covered by `record_owned_bytes`' field walk and need their own rows to be tested.
        assert!(
            record_owned_bytes(&model("a-model-key")).expect("extent fits") > 0,
            "Model owns Strings, so its extent must be positive"
        );
        // And a row with extensions must exceed the same row without them.
        let mut with_ext = model("a-model-key");
        if let FoldedRecord::Model(ref mut r) = with_ext {
            r.extensions = sage_db_types::persist::Extensions::from_canonical(vec![(
                "k".to_owned(),
                vec![0xAB; 32],
            )])
            .expect("one canonical extension");
        }
        assert!(
            record_owned_bytes(&with_ext).expect("fits")
                > record_owned_bytes(&model("a-model-key")).expect("fits"),
            "extensions must add to the retained extent"
        );
    }

    #[test]
    fn model_extent_covers_its_owned_strings_and_extensions() {
        let row = model("a-model-key");
        let bytes = record_owned_bytes(&row).expect("extent fits");
        // Lower bound from the fields alone: the key, the identifier and the provider name.
        let lower = "a-model-key".len() + "identifier-value".len() + "provider-name".len();
        assert!(
            bytes >= lower as u64,
            "extent {bytes} must cover at least the owned string payload {lower}"
        );
    }

    #[test]
    fn forking_a_model_row_is_fallible_and_preserves_the_key() {
        // A Model's identity is a String-bearing natural key, so the fork must rebuild it rather than clone_from.
        let mut fold = StateFold::new();
        let mut ledger = capable();
        let outcome = fold.apply_owning(
            FoldedVersion {
                version: 1,
                tx_id: [0x01; 16],
                event_count: 1,
                rules_profile: 1,
                rules_digest: [0x02; 32],
            },
            KbUuid::from_bytes([0x11; 16]),
            vec![FoldEvent::Record(model("a-model-key"))],
            &mut ledger,
        );
        assert!(outcome.into_result().is_ok(), "the model row must fold");

        let fork = match fold.try_fork(&mut ledger) {
            FoldOutcome::Ok(fork) => fork,
            other => panic!("fork must succeed: {other:?}"),
        };
        // Value identity preserved: same natural key, same revision, same record.
        let identity = RecordIdentity::Natural {
            key: NaturalKey::Model("a-model-key".to_owned()),
        };
        assert_eq!(fork.revision_count(&identity), 1);
        assert_eq!(fork.current_revision(&identity), Some(1));
        assert!(fork.current_record(&identity).is_some());
    }

    #[test]
    fn a_refused_payload_admission_fails_the_fork_rather_than_copying() {
        // A zero-allowance ledger must refuse BEFORE the key is copied, so the fork cannot silently deep-copy.
        struct NoBytes;
        impl SharedLedger for NoBytes {
            fn try_charge(&mut self, bytes: u64) -> Result<(), sage_db_types::ledger::BudgetError> {
                Err(sage_db_types::ledger::BudgetError::new(
                    sage_db_types::ledger::BudgetUnit::Bytes,
                    bytes,
                    0,
                ))
            }
            fn try_charge_entries(
                &mut self,
                _: u64,
            ) -> Result<(), sage_db_types::ledger::BudgetError> {
                Ok(())
            }
            fn checkpoint(&mut self) -> Result<(), sage_db_types::query::ControlStop> {
                Ok(())
            }
        }

        let mut fold = StateFold::new();
        let mut ledger = capable();
        fold.apply_owning(
            FoldedVersion {
                version: 1,
                tx_id: [0x01; 16],
                event_count: 1,
                rules_profile: 1,
                rules_digest: [0x02; 32],
            },
            KbUuid::from_bytes([0x11; 16]),
            vec![FoldEvent::Record(model("a-model-key"))],
            &mut ledger,
        )
        .into_result()
        .expect("folds");

        let mut no_bytes = NoBytes;
        match fold.try_fork(&mut no_bytes) {
            FoldOutcome::Refused(Refusal::Budget(_)) => {}
            other => panic!("a zero-allowance ledger must refuse the fork, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod key_compare_tests {
    //! The chunked model-key comparison, tested against `str`'s own ordering.
    //!
    //! These live in-crate because `bounded_key_cmp` is private, and a public call would
    //! only reach it through a search whose state (which rows exist, how many
    //! comparisons the binary search happens to make) would be guessed rather than
    //! known. A guessed shape cannot prove "the second chunk was charged".
    use super::*;
    use crate::lease::AttemptLedger;
    use crate::view::RecordReadError;
    use sage_db_types::ledger::BudgetUnit;
    use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};

    fn ledger(bytes: u64, entries: u64) -> AttemptLedger<'static> {
        AttemptLedger::open(bytes, entries, &RunToCompletion).expect("opens")
    }

    fn charged(ledger: &AttemptLedger<'_>) -> u128 {
        ledger.stats().admitted_entries.expect("representable")
    }

    /// Independent expectation: Rust's own `str` ordering, over the same pair.
    fn expect(a: &str, b: &str) -> core::cmp::Ordering {
        a.cmp(b)
    }

    #[test]
    fn shared_long_prefix_differs_at_a_later_chunk_and_matches_str_ord() {
        let prefix = "embed-model-with-a-shared-prefix-0000000000";
        assert!(prefix.len() >= 33, "the fixture prefix must span 3 chunks");
        let a = format!("{prefix}-a");
        let b = format!("{prefix}-b");

        let mut l = ledger(u64::MAX, u64::MAX);
        let got = bounded_key_cmp(&a, &b, &mut l).expect("compares");
        assert_eq!(got, expect(&a, &b));
        assert_eq!(got, core::cmp::Ordering::Less);

        // Chunks 0 and 1 are equal, chunk 2 differs, then the length step: the walk
        // must have charged strictly more than a single-chunk comparison would.
        let single_chunk_floor = 2; // one chunk + the length step
        assert!(
            charged(&l) > single_chunk_floor,
            "a >=3-chunk pair must charge more than {single_chunk_floor}, charged {}",
            charged(&l)
        );
    }

    #[test]
    fn a_second_chunk_charge_is_refused_by_a_real_entry_budget() {
        let prefix = "embed-model-with-a-shared-prefix-0000000000";
        let a = format!("{prefix}-a");
        let b = format!("{prefix}-b");

        // One entry: enough for the first chunk, not for the second.
        let mut short = ledger(u64::MAX, 1);
        match bounded_key_cmp(&a, &b, &mut short) {
            Err(FixedReadError::Refused(Refusal::Budget(error))) => {
                assert_eq!(error.unit(), BudgetUnit::Entries);
                assert_eq!(error.requested(), 1);
            }
            other => panic!("the second chunk must be refused, got {other:?}"),
        }
    }

    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }

    #[test]
    fn a_stop_is_observed_before_the_second_chunk() {
        let prefix = "embed-model-with-a-shared-prefix-0000000000";
        let a = format!("{prefix}-a");
        let b = format!("{prefix}-b");
        let mut stopped = AttemptLedger::open(u64::MAX, u64::MAX, &Stop).expect("opens");
        assert!(matches!(
            bounded_key_cmp(&a, &b, &mut stopped),
            Err(FixedReadError::Refused(Refusal::Stopped(
                ControlStop::Cancelled
            )))
        ));
    }

    #[test]
    fn fixed_shape_comparisons_charge_nothing_beyond_the_search_iteration() {
        // The iteration charge is what a fixed comparison is covered by; charging here
        // would double the identified scan's per-row cost.
        let mut l = ledger(u64::MAX, u64::MAX);
        let key = RecordKey::term(7);
        let a = RecordIdentity::Identified { key };
        let b = RecordIdentity::Identified {
            key: RecordKey::term(9),
        };
        assert_eq!(
            identity_cmp_charged(&a, &b, &mut l).expect("compares"),
            core::cmp::Ordering::Less
        );
        assert_eq!(charged(&l), 0, "a fixed-shape comparison is free");

        // ... while a model comparison does charge, because it walks bytes.
        let m1 = RecordIdentity::Natural {
            key: NaturalKey::Model("a".repeat(40)),
        };
        let m2 = RecordIdentity::Natural {
            key: NaturalKey::Model(format!("{}b", "a".repeat(39))),
        };
        let before = charged(&l);
        assert_eq!(
            identity_cmp_charged(&m1, &m2, &mut l).expect("compares"),
            expect(
                match &m1 {
                    RecordIdentity::Natural {
                        key: NaturalKey::Model(s),
                    } => s.as_str(),
                    _ => unreachable!(),
                },
                match &m2 {
                    RecordIdentity::Natural {
                        key: NaturalKey::Model(s),
                    } => s.as_str(),
                    _ => unreachable!(),
                }
            )
        );
        assert!(charged(&l) > before, "byte comparison work is charged");
    }

    #[test]
    fn natural_ranking_reproduces_derived_ord_across_families() {
        let asset = RecordIdentity::Natural {
            key: NaturalKey::Asset([0x11; 32]),
        };
        let model = RecordIdentity::Natural {
            key: NaturalKey::Model("m".into()),
        };
        let kb = RecordIdentity::Natural {
            key: NaturalKey::KbMetadata,
        };
        let rules = RecordIdentity::Natural {
            key: NaturalKey::RuleConfig,
        };
        let mut l = ledger(u64::MAX, u64::MAX);
        let pairs = [
            (&asset, &model),
            (&model, &kb),
            (&kb, &rules),
            // Two different empty-key singletons must never compare as equal payloads.
            (&kb, &rules),
        ];
        for (a, b) in pairs {
            assert_eq!(
                identity_cmp_charged(a, b, &mut l).expect("compares"),
                a.cmp(b),
                "must match derived Ord"
            );
        }
        // Mixed variant shape.
        let identified = RecordIdentity::Identified {
            key: RecordKey::term(1),
        };
        assert_eq!(
            identity_cmp_charged(&identified, &asset, &mut l).expect("compares"),
            identified.cmp(&asset)
        );
    }

    #[test]
    fn missing_owner_maps_to_the_scalar_natural_or_exact_numeric_variant() {
        let model = RecordIdentity::Natural {
            key: NaturalKey::Model("embed".into()),
        };
        assert!(matches!(
            missing_owner(&model, 3),
            RecordReadError::MissingNaturalRetainedOwner {
                kind: StateKind::Model,
                revision: 3
            }
        ));
        let key = RecordKey::term(5);
        let identified = RecordIdentity::Identified { key };
        assert!(matches!(
            missing_owner(&identified, 2),
            RecordReadError::MissingRetainedOwner { key: k, revision: 2 } if k == key
        ));
    }
}
