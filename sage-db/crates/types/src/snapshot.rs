//! The read-only snapshot contract: what a query layer may ask of one committed
//! version, without owning storage, inference or a cache.
//!
//! # Why this lives in `types`
//!
//! [`SnapshotView`] is the seam between "some immutable committed state" and "the
//! query verb layer". It is declared here, next to the records it returns, so that
//! neither side has to depend on the other:
//!
//! * `types` must not import `store`, `kernel` or `format` (this crate is pure and
//!   the dependency arrow points inward), so the trait cannot be defined in the
//!   store;
//! * a query crate would otherwise have to depend on the store's concrete handle,
//!   which would make the store the query layer's owner and put `infer` behind a
//!   storage dependency.
//!
//! A concrete adapter lives elsewhere (`sage-db-kernel::snapshot`) and implements
//! this trait over one immutable publication. This module therefore contains
//! **no** I/O, no ledger implementation, no ownership primitive and no closure,
//! proof or cache accessor.
//!
//! # What a view may and may not do
//!
//! At this stage a view offers exactly:
//!
//! * typed **point** access to the current after-image of a record;
//! * typed **scan** access over the current records of the four families in
//!   scope (Term, Assertion, Claim, SessionCheckpoint);
//! * the one **base graph** built from those records;
//! * the rule configuration actually committed for that version.
//!
//! Deliberately absent: `fact()`, `explain()`, closure, widest path, search,
//! ranking and any result cache. Those are computation over a graph, they are
//! bounded by the caller's inference budget, and putting an unbudgeted accessor
//! here would move inference cost outside the executor that meters it
//! (`docs/design/system.md` §3).
//!
//! # Refusal vocabulary
//!
//! Three outcomes must never be conflated:
//!
//! | outcome | value | meaning |
//! |---|---|---|
//! | absent | [`RecordView::Absent`] | complete coverage says this key is not current |
//! | refused | [`ReadRefusal::Refused`] | this call's ledger refused, or the caller stopped |
//! | unusable view | [`SnapshotOpenError`] | there is no single durable publication to read |
//!
//! `Absent` is only ever returned on a **complete, durable** basis. A view that is
//! not durable is not constructed at all, so a query layer never has to ask
//! "is this absence real?".

use crate::digest::{Digest, RulesDigest};
use crate::framing::Refusal;
use crate::graph::{BaseGraph, GraphBuildError, GraphBuildRefusal, SnapshotId};
use crate::identity::{KbUuid, KbVersion};
use crate::ledger::SharedLedger;
use crate::persist::{
    AssertionRecordFull, ClaimRecord, RecordKey, RecordKind, SessionCheckpointRecord, TermRecord,
};
use crate::relation::{RuleProfile, RuleSet};

/// The record families a snapshot view exposes at this stage.
///
/// The set is closed and matches the release scope: Terms, authored Assertions,
/// Claims and session checkpoints. Evidence, links, vectors, proposals, frontier
/// entries and assets are deliberately not reachable through a
/// [`SnapshotView`] yet — adding one is a contract change, not a local
/// convenience.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SnapshotFamily {
    /// A term (concept) record.
    Term,
    /// An authored assertion with its own identity.
    Assertion,
    /// A claim about a term.
    Claim,
    /// One user session checkpoint.
    SessionCheckpoint,
}

impl SnapshotFamily {
    /// Every family in scope, in the order a scan visits them.
    pub const ALL: [Self; 4] = [
        Self::Term,
        Self::Assertion,
        Self::Claim,
        Self::SessionCheckpoint,
    ];

    /// Stable name for diagnostics and typed payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Term => "term",
            Self::Assertion => "assertion",
            Self::Claim => "claim",
            Self::SessionCheckpoint => "session-checkpoint",
        }
    }

    /// The record kind this family holds, when it has one closed numeric kind.
    #[must_use]
    pub const fn is_family_of(self, key: RecordKey) -> bool {
        matches!(
            (self, key.kind()),
            (Self::Term, crate::persist::RecordKind::Term)
                | (Self::Assertion, crate::persist::RecordKind::Assertion)
                | (Self::Claim, crate::persist::RecordKind::Claim)
                | (
                    Self::SessionCheckpoint,
                    crate::persist::RecordKind::SessionCheckpoint
                )
        )
    }
}

/// A borrowed current after-image of one record family.
///
/// The borrow is the point: a caller reads the record the store already owns, and
/// the borrow keeps the publication from changing underneath it. No deep clone is
/// made, so no reservation is silently transferred or shed.
#[derive(Clone, Copy, Debug)]
pub enum RecordRef<'a> {
    /// A current term record.
    Term(&'a TermRecord),
    /// A current authored assertion record.
    Assertion(&'a AssertionRecordFull),
    /// A current claim record.
    Claim(&'a ClaimRecord),
    /// A current session checkpoint record.
    SessionCheckpoint(&'a SessionCheckpointRecord),
}

impl<'a> RecordRef<'a> {
    /// The record's identity, whatever family it is.
    #[must_use]
    pub const fn key(self) -> RecordKey {
        match self {
            Self::Term(record) => record.key,
            Self::Assertion(record) => record.key,
            Self::Claim(record) => record.key,
            Self::SessionCheckpoint(record) => record.key,
        }
    }

    /// The record's current revision.
    #[must_use]
    pub const fn revision(self) -> crate::identity::RecordRevision {
        match self {
            Self::Term(record) => record.revision,
            Self::Assertion(record) => record.revision,
            Self::Claim(record) => record.revision,
            Self::SessionCheckpoint(record) => record.revision,
        }
    }

    /// Which family this record belongs to.
    #[must_use]
    pub const fn family(self) -> SnapshotFamily {
        match self {
            Self::Term(_) => SnapshotFamily::Term,
            Self::Assertion(_) => SnapshotFamily::Assertion,
            Self::Claim(_) => SnapshotFamily::Claim,
            Self::SessionCheckpoint(_) => SnapshotFamily::SessionCheckpoint,
        }
    }
}

/// The answer to one point read on a complete durable basis.
#[derive(Clone, Copy, Debug)]
pub enum RecordView<'a> {
    /// The record is current on this view's basis.
    Found(RecordRef<'a>),
    /// Complete coverage on this basis establishes that the key is not current.
    ///
    /// A retired or retracted record still has a current after-image and is returned
    /// as `Found` with its lifecycle flag. Absence means no current after-image,
    /// independent of active-graph filtering.
    Absent {
        /// The key that was asked for, echoed so a batched caller can pair answers
        /// without re-deriving which request produced which result.
        key: RecordKey,
    },
}

impl<'a> RecordView<'a> {
    /// The record, when present.
    #[must_use]
    pub const fn record(self) -> Option<RecordRef<'a>> {
        match self {
            Self::Found(record) => Some(record),
            Self::Absent { .. } => None,
        }
    }

    /// Whether this answer is `Absent`.
    #[must_use]
    pub const fn is_absent(self) -> bool {
        matches!(self, Self::Absent { .. })
    }
}

/// Why a read did not produce an answer.
///
/// Not a data verdict and not absence: the caller either ran out of its own
/// allowance or asked to stop, and the same call can succeed unchanged with more
/// allowance or without the stop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadRefusal {
    /// The caller's ledger refused, or the caller stopped.
    Refused(Refusal),
    /// The key names a record kind this view does not expose.
    ///
    /// Distinct from `RecordView::Absent`: "I do not interpret that family" is not
    /// "that record does not exist here". Reporting the second when the first is
    /// true would let a caller conclude a committed record is gone.
    UnsupportedFamily(RecordKind),
    /// The key is not representable: its id does not fit the bound its kind declares.
    ///
    /// A malformed key is a caller error, never a statement about stored data.
    InvalidKey(RecordKey),
}

impl From<Refusal> for ReadRefusal {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

/// Why a view could not be constructed.
///
/// Every variant means "there is no single durable publication to read", so no
/// query should be attempted against a different basis.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SnapshotOpenError {
    /// The backend published no durable view (unverified root, incomplete
    /// coverage, or nothing observed at all).
    NotDurable,
    /// The caller asked for a specific version and the publication is a different
    /// one.
    ///
    /// This is never resolved by reading the latest version instead: a caller that
    /// named a version is asking about *that* state, and silently answering from
    /// another one would make the answer unfalsifiable.
    VersionMismatch {
        /// The version the caller required.
        expected: KbVersion,
        /// The version actually published.
        found: KbVersion,
    },
    /// The view belongs to a different KB.
    KbMismatch {
        /// The KB the caller required.
        expected: KbUuid,
        /// The KB actually published.
        found: KbUuid,
    },
    /// The caller's ledger refused or stopped while the view was being built.
    Refused(Refusal),
    /// The store reports an unfinished import session. The retained session can be
    /// inspected through the administrative Store API; this refusal copies no payload.
    ImportIncomplete,
    /// The committed rule bytes did not verify.
    ///
    /// Distinct from `NotDurable`: the view exists, the record set is readable, and the
    /// *rules* blob failed its own canonical verification — a different fault with a
    /// different remedy. The format verdict is preserved, not flattened.
    Rules(crate::framing::FormatError),
    /// The view's retained credit could not be transferred to its owner.
    ///
    /// An accounting shortfall, not a quota: the bytes were charged on this attempt and the
    /// transfer found fewer untransferred bytes than the view must retain. Reported as itself
    /// so an operator does not retry with a bigger budget.
    TransferShortfall {
        /// Bytes the view must retain.
        requested: u64,
        /// Untransferred bytes the attempt actually held.
        available: u64,
    },
    /// The base graph could not be built from the records in scope.
    ///
    /// Carried as the graph's own verdict rather than flattened: duplicate
    /// identities, dangling endpoints and a caller refusal have different
    /// remedies.
    Graph(GraphBuildRefusal),
}

impl From<GraphBuildRefusal> for SnapshotOpenError {
    fn from(refusal: GraphBuildRefusal) -> Self {
        Self::Graph(refusal)
    }
}

impl From<GraphBuildError> for SnapshotOpenError {
    fn from(error: GraphBuildError) -> Self {
        Self::Graph(GraphBuildRefusal::Invalid(error))
    }
}

/// The rule configuration in force for a view, as committed.
///
/// `profile` and `digest` are the committed facts; `rule_set` is the pair the
/// graph identity needs. Nothing here is re-derived from the compiled-in profile:
/// a build that silently used `formal-v1` where the KB committed `legacy-v1`
/// would answer under rules nobody chose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CommittedRules {
    /// The profile the committed rule blob declares.
    pub profile: RuleProfile,
    /// Its domain-separated digest.
    pub rules_digest: RulesDigest,
    /// The content address of the committed rule bytes.
    pub rules_ref: Digest,
}

impl CommittedRules {
    /// The pair a [`SnapshotId`] carries, so graph identity keeps the profile and
    /// the digest together.
    #[must_use]
    pub const fn as_rule_set(self) -> RuleSet {
        RuleSet::from_trusted_parts(self.profile, self.rules_digest)
    }
}

/// Read-only access to one immutable committed version.
///
/// Implementations must satisfy four properties, each of which a query layer is
/// entitled to rely on:
///
/// 1. **One basis.** Every method on one view answers from the same publication.
///    `basis()` never changes for the life of the value.
/// 2. **Absence needs completeness.** `record` returns [`RecordView::Absent`] only
///    on a complete durable basis; a view that cannot establish that is not
///    constructed.
/// 3. **Real ledger.** Every read charges the caller's ledger before it looks
///    anything up or visits anything, and a refusal or stop is reported as
///    [`ReadRefusal`], never as absence.
/// 4. **Borrowed data.** Records are borrowed from the publication; the
///    implementation does not deep-copy them, so no reservation is silently
///    created or dropped on the caller's behalf.
///
/// The lifetime is the publication's: while a view or a record borrowed from it
/// is live, the underlying immutable version cannot be dropped.
pub trait SnapshotView {
    /// The snapshot identity every answer in this view was computed from.
    #[must_use]
    fn basis(&self) -> SnapshotId;

    /// The rule configuration committed for this version.
    #[must_use]
    fn rules(&self) -> CommittedRules;

    /// The base graph of active authored terms and assertions at this version.
    ///
    /// Built once, from the records in this view; it carries its own reservation
    /// for as long as the view lives.
    #[must_use]
    fn graph(&self) -> &BaseGraph;

    /// The current after-image of one record.
    ///
    /// Three answers, never conflated:
    ///
    /// * [`RecordView::Found`] — the record is current on this basis;
    /// * [`RecordView::Absent`] — complete coverage on this basis says it is not current;
    /// * [`ReadRefusal::UnsupportedFamily`] — the key names a family this view does not
    ///   expose, so nothing can be said about presence. Answering `Absent` here would
    ///   claim a committed record does not exist when the truth is "not interpreted".
    ///
    /// # Errors
    ///
    /// [`ReadRefusal`] for a caller budget/stop refusal, an unsupported family, or an
    /// unrepresentable key. None of them is `Absent`.
    fn record(
        &self,
        key: RecordKey,
        ledger: &mut dyn SharedLedger,
    ) -> Result<RecordView<'_>, ReadRefusal>;

    /// Visit every current record of one family.
    ///
    /// The visitor borrows each record from the publication and sees it once. The
    /// walk consults the caller's ledger per record, and consults control even for
    /// an empty family, so a stopped or exhausted caller is never reported as a
    /// completed scan of nothing.
    ///
    /// # Errors
    ///
    /// `E` exactly as the visitor returned it, or [`ReadRefusal`] when the ledger
    /// refuses or stops before the walk completes. A scan that does not complete
    /// returns no success: the caller must discard any partial output it built.
    fn scan_family<'a, E>(
        &'a self,
        family: SnapshotFamily,
        ledger: &mut dyn SharedLedger,
        visitor: &mut dyn FnMut(RecordRef<'a>) -> Result<(), E>,
    ) -> Result<(), ScanError<E>>;
}

/// Why a scan did not complete.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScanError<E> {
    /// The caller's ledger refused or stopped during the walk.
    Refused(ReadRefusal),
    /// The visitor refused its own work; preserved without relabelling it.
    Visitor(E),
}

impl<E> ScanError<E> {
    /// Whether this failure is the visitor's own error.
    #[must_use]
    pub const fn is_visitor(&self) -> bool {
        matches!(self, Self::Visitor(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::RecordRevision;

    #[test]
    fn families_are_distinct_and_cover_the_scope() {
        // A family table that accidentally aliased two kinds would make a scan
        // visit the wrong records; this pins the set and its size.
        assert_eq!(SnapshotFamily::ALL.len(), 4);
        let names: Vec<_> = SnapshotFamily::ALL.iter().map(|f| f.as_str()).collect();
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), 4, "family names are distinct");
        assert_eq!(
            RecordKey::term(7).kind(),
            crate::persist::RecordKind::Term,
            "the term key constructor names the term kind"
        );
        assert!(SnapshotFamily::Term.is_family_of(RecordKey::term(7)));
        assert!(!SnapshotFamily::Assertion.is_family_of(RecordKey::term(7)));
    }

    #[test]
    fn absent_echoes_the_key_and_carries_no_record() {
        let view: RecordView<'_> = RecordView::Absent {
            key: RecordKey::term(3),
        };
        assert!(view.is_absent());
        assert!(view.record().is_none());
    }

    #[test]
    fn a_version_mismatch_is_its_own_refusal() {
        // Never remapped to NotDurable: "you asked for v3, this is v2" has a
        // different remedy from "this publication is not durable".
        let error = SnapshotOpenError::VersionMismatch {
            expected: KbVersion::new(3),
            found: KbVersion::new(2),
        };
        assert_ne!(error, SnapshotOpenError::NotDurable);
    }

    #[test]
    fn committed_rules_keep_profile_and_digest_together() {
        let rules = CommittedRules {
            profile: RuleProfile::LegacyV1,
            rules_digest: RulesDigest::new(Digest::from_bytes([0x11; 32])),
            rules_ref: Digest::from_bytes([0x22; 32]),
        };
        let set = rules.as_rule_set();
        assert_eq!(set.profile(), RuleProfile::LegacyV1);
        assert_eq!(set.digest(), rules.rules_digest);
    }

    #[test]
    fn a_record_ref_reports_its_own_family_and_key() {
        let term = TermRecord {
            key: RecordKey::term(9),
            revision: RecordRevision::FIRST,
            retired: false,
            slug: "term-9".to_owned(),
            atom: "atom".to_owned(),
            layer: crate::layer::SemanticLayer::L1,
            domains: Vec::new(),
            source: crate::persist::TermSource::Personal,
            confidence: crate::confidence::Confidence::Confirmed,
            triggers: Vec::new(),
            body_ref: [0u8; 32],
            common: crate::persist::CommonFields {
                recorded_by: None,
                actor_origin: crate::persist::ActorOrigin::Authenticated,
                reason: None,
                created_at: 0,
                updated_at: None,
            },
            legacy_id: None,
            legacy_source_path: None,
            legacy_content_digest: None,
            legacy_source_file_digest: None,
            legacy_source_revision: None,
            extensions: crate::persist::Extensions::new(),
            changed_fields: None,
        };
        let record = RecordRef::Term(&term);
        assert_eq!(record.family(), SnapshotFamily::Term);
        assert_eq!(record.key(), RecordKey::term(9));
        assert_eq!(record.revision(), RecordRevision::FIRST);
    }
}
