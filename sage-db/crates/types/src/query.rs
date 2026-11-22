//! Query input, result, ordering and failure contracts.
//!
//! This module fixes the shape of a request, the canonical row order, how several
//! supports qualify for the reported confidence, and what a caller may conclude
//! from a result. It contains no rule evaluation.
//!
//! Three consequences are deliberate:
//!
//! - **No partial semantic results.** Exhausting work, proof or control budget is a
//!   typed error. A result that exists is a complete evaluation of the question.
//!   `absent` is therefore only meaningful for a returned (complete) result and is
//!   never inferred from an interrupted one (`docs/design/axiomatics.md` §5).
//! - **Progress is observable.** [`QueryControl`] is the only way an executor may
//!   notice cancellation or an expired deadline; it is supplied by the caller, so
//!   the pure layer reads no clock and no environment.
//! - **Differential comparison is defined on canonical fact rows**: endpoints,
//!   effective confidence, asserted/derived support and dispute markers. Proof
//!   handles, arena layout, execution statistics and cache state are excluded,
//!   because two correct executors may differ in those without disagreeing.

use core::fmt;

use crate::budget::{Budget, BudgetLimit};
use crate::confidence::{Annotation, Confidence};
use crate::digest::{Digest, EngineVersion, RulesDigest};
use crate::graph::SnapshotId;
use crate::identity::{AssertionId, KbUuid, KbVersion, RecordRevision, TermId};
use crate::proof::{ProofArena, ProofArenaError, ProofRef};
use crate::relation::{RelationId, RuleSet};

/// Why an executor must abandon the current evaluation.
///
/// Both variants are *caller* signals, never observations of the environment: the
/// types crate cannot read a clock, so the deadline arrives as a policy the caller
/// supplies with the request.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ControlStop {
    /// The caller cancelled the request.
    Cancelled,
    /// The request's deadline passed, as judged by the caller.
    DeadlineExceeded,
}

impl ControlStop {
    /// Stable name for error payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::DeadlineExceeded => "deadline exceeded",
        }
    }
}

impl fmt::Display for ControlStop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Caller-supplied progress and cancellation hook.
///
/// Executors call [`QueryControl::checkpoint`] at bounded intervals (every
/// `CONTROL_CHECK_INTERVAL_STEPS` charged steps, see
/// `docs/design/inference.md` §8) and must stop on `Err`. Implementations are the
/// only place a clock or an external cancellation flag may be consulted; the
/// inference layer itself stays pure.
pub trait QueryControl {
    /// Decide whether work may continue.
    ///
    /// # Errors
    ///
    /// The executor must stop and return the corresponding typed failure.
    fn checkpoint(&self) -> Result<(), ControlStop>;
}

/// Control hook that never stops.
///
/// Provided so callers that genuinely want to run to completion do not have to
/// write a no-op implementation, and so tests can differ only in the control.
#[derive(Clone, Copy, Debug, Default)]
pub struct RunToCompletion;

impl QueryControl for RunToCompletion {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        Ok(())
    }
}

/// Steps between mandatory [`QueryControl`] checks.
pub const CONTROL_CHECK_INTERVAL_STEPS: u64 = 1_024;

/// What a caller asks about one relation from one term.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SourceQuery {
    /// Version to evaluate, pinned by the caller before the request. Carries the
    /// rule configuration; it must match the graph's.
    pub snapshot: SnapshotId,
    /// Subject of the query.
    pub source: TermId,
    /// Relation to evaluate from `source`.
    pub relation: RelationId,
    /// Ask for a strongest proof for every returned fact.
    pub with_proof: bool,
    /// Limits for this request; already tightened by the server.
    pub budget: Budget,
}

impl SourceQuery {
    /// Validate the request shape before any work starts.
    ///
    /// # Errors
    ///
    /// Fails for a zero-row or zero-step budget (which cannot produce a complete
    /// answer) and for a request whose rule set differs from its snapshot's.
    pub fn validate(&self) -> Result<(), QueryError> {
        if self.budget.max_rows == 0 {
            return Err(QueryError::InvalidRequest {
                reason: "max_rows must be positive",
            });
        }
        if self.budget.max_steps == 0 {
            return Err(QueryError::InvalidRequest {
                reason: "max_steps must be positive",
            });
        }
        Ok(())
    }

    /// Check that this request and `graph` describe the same snapshot.
    ///
    /// Every field of [`SnapshotId`] is compared, not just the version: the same
    /// KB at the same commit number can carry a different `graph_digest` if the
    /// index was rebuilt from different rules, and answering across that boundary
    /// would silently combine two graphs. `graph_digest` is the field that pins
    /// the actual graph contents, so skipping it is the one omission that matters.
    ///
    /// # Errors
    ///
    /// Fails with [`QueryError::RuleMismatch`] naming the first differing field.
    pub fn check_against(&self, graph: &crate::graph::BaseGraph) -> Result<(), QueryError> {
        let graph_snapshot = graph.snapshot();
        if graph_snapshot.kb_uuid != self.snapshot.kb_uuid {
            return Err(QueryError::RuleMismatch {
                reason: "kb_uuid differs between query and graph",
            });
        }
        if graph_snapshot.version != self.snapshot.version {
            return Err(QueryError::RuleMismatch {
                reason: "version differs between query and graph",
            });
        }
        if graph_snapshot.rules != self.snapshot.rules {
            return Err(QueryError::RuleMismatch {
                reason: "rule set differs between query and graph",
            });
        }
        if graph_snapshot.graph_digest != self.snapshot.graph_digest {
            return Err(QueryError::RuleMismatch {
                reason: "graph_digest differs between query and graph",
            });
        }
        Ok(())
    }

    /// The rule set the request must be evaluated under.
    #[must_use]
    pub const fn rules(&self) -> RuleSet {
        self.snapshot.rules
    }
}

/// One canonical fact row.
///
/// Comparison ignores everything outside this struct, which is what makes it
/// usable as a differential oracle row.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FactRow {
    /// Subject, in the direction the row is reported for.
    pub from: TermId,
    /// Relation of the reported fact.
    pub relation: RelationId,
    /// Object, in the direction the row is reported for.
    pub to: TermId,
    /// Effective annotation: `max` over all qualifying supports.
    pub annotation: Annotation,
    /// A direct authored assertion exists **in this reported direction**.
    ///
    /// A reverse reading produced by R2 from an assertion authored the other way
    /// is *derived*, not asserted: the author did not state this direction.
    pub has_asserted: bool,
    /// A non-circular rule proof reaches the fact, additionally to any assertion.
    ///
    /// Counts as derived (see `docs/design/inference.md` §3):
    /// R1 of at least two hops, R3 from a direct definition assertion, or a single
    /// R2 applied to a reverse authored assertion. `R2(R2(a))` is **not** a second
    /// source: it returns to the authored direction and adds nothing, so it must
    /// not set this flag on its own.
    pub has_derived: bool,
    /// Some valid support or path is `contested`, even when the effective
    /// annotation is stronger.
    ///
    /// This is over **all** qualifying supports and paths, not just the strongest
    /// proof: `max` may raise the annotation, but it must never erase recorded
    /// disagreement. A fact proven confirmed can still be `disputed`.
    pub disputed: bool,
    /// Strongest contributing assertion, when an authored assertion supports it.
    pub strongest_support: Option<AssertionSupport>,
    /// Root of this fact's proof in [`SourceResult::proof`], when one was built.
    ///
    /// Excluded from canonical comparison: two executors may lay out their arenas
    /// differently while proving the same fact.
    pub proof: Option<ProofRef>,
}

/// An authored assertion that underpins a fact.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AssertionSupport {
    /// Assertion identity.
    pub id: AssertionId,
    /// Revision active at the query's snapshot.
    pub revision: RecordRevision,
    /// Confidence authored on that assertion.
    pub confidence: Confidence,
}

impl FactRow {
    /// Sort key: `(to, relation_id, from)`.
    ///
    /// Rows are reported in this order so both executors emit the same sequence
    /// for the same fact set, independent of traversal order. This is the
    /// *canonical* order used for comparison and digests; user-facing display
    /// ordering is a separate policy applied by the query layer
    /// (`docs/design/query-dsl.md` §5).
    #[must_use]
    pub const fn sort_key(&self) -> (TermId, RelationId, TermId) {
        (self.to, self.relation, self.from)
    }

    /// The semantically comparable part of this row.
    #[must_use]
    pub const fn canonical(&self) -> CanonicalRow {
        CanonicalRow {
            from: self.from,
            relation: self.relation,
            to: self.to,
            annotation: self.annotation,
            has_asserted: self.has_asserted,
            has_derived: self.has_derived,
            disputed: self.disputed,
        }
    }
}

/// The semantically comparable part of a [`FactRow`].
///
/// This is the differential oracle's unit of comparison. It omits proof handles,
/// statistics and cache state, so two independent executors can agree here while
/// their internal construction differs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CanonicalRow {
    /// Subject of the reported fact.
    pub from: TermId,
    /// Relation of the reported fact.
    pub relation: RelationId,
    /// Object of the reported fact.
    pub to: TermId,
    /// Effective annotation of the fact.
    pub annotation: Annotation,
    /// A direct authored assertion exists in this reported direction (see
    /// [`FactRow::has_asserted`]).
    pub has_asserted: bool,
    /// A non-circular rule proof reaches the fact (see [`FactRow::has_derived`]);
    /// `R2(R2(a))` does not qualify.
    pub has_derived: bool,
    /// Any valid support or path is disputed, not only the strongest proof (see
    /// [`FactRow::disputed`]).
    pub disputed: bool,
}

/// Why a query produced no semantic result.
///
/// Every variant means "no answer". None of them may be reported as an empty row
/// set, because an empty row set is a real, complete answer meaning `absent`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QueryError {
    /// The request could not be a valid question.
    InvalidRequest {
        /// Fixed, non-localised reason.
        reason: &'static str,
    },
    /// The request's rules or snapshot disagree with the graph's.
    RuleMismatch {
        /// Fixed, non-localised reason.
        reason: &'static str,
    },
    /// The requested source term is not in the graph.
    UnknownTerm(TermId),
    /// A transitive operation was requested for a non-transitive relation.
    RelationNotTransitive(RelationId),
    /// Work exceeded a budget limit; the answer is unknown, not absent.
    BudgetExceeded(BudgetLimit),
    /// The proof arena rejected a node or ran out of its allowance.
    Proof(ProofArenaError),
    /// The caller cancelled or the deadline expired.
    Stopped(ControlStop),
    /// The named executor is an interface only.
    ///
    /// Distinct from an empty result on purpose: a scaffold must never look like
    /// a completed evaluation that found nothing.
    NotImplemented {
        /// [`crate::executor::SourceExecutor::name`] of the refusing executor.
        executor: &'static str,
    },
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest { reason } => write!(f, "invalid request: {reason}"),
            Self::RuleMismatch { reason } => write!(f, "rule mismatch: {reason}"),
            Self::UnknownTerm(term) => write!(f, "unknown term {}", term.get()),
            Self::RelationNotTransitive(relation) => {
                write!(f, "relation {relation} is not transitive")
            }
            Self::BudgetExceeded(limit) => write!(f, "budget exceeded: {limit}"),
            Self::Proof(error) => fmt::Display::fmt(error, f),
            Self::Stopped(stop) => write!(f, "evaluation stopped: {stop}"),
            Self::NotImplemented { executor } => {
                write!(f, "{executor} executor is not implemented yet")
            }
        }
    }
}

impl std::error::Error for QueryError {}

impl From<ProofArenaError> for QueryError {
    fn from(error: ProofArenaError) -> Self {
        Self::Proof(error)
    }
}

impl From<BudgetLimit> for QueryError {
    fn from(limit: BudgetLimit) -> Self {
        Self::BudgetExceeded(limit)
    }
}

impl From<ControlStop> for QueryError {
    fn from(stop: ControlStop) -> Self {
        Self::Stopped(stop)
    }
}

/// Execution provenance, excluded from semantic comparison.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ExecStats {
    /// Node lookups and adjacency steps charged.
    pub steps: u64,
    /// Work rows materialised.
    pub work_rows: u64,
    /// Work-area bytes reserved.
    pub work_bytes: u64,
    /// Proof nodes allocated.
    pub proof_nodes: u32,
}

/// Result of one source query.
///
/// A returned result is a *complete* evaluation: every row is final, and the
/// absence of a row is a definitive `absent`. Interrupted evaluations return
/// [`QueryError`] instead of one of these.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SourceResult {
    /// Snapshot the result was computed at; never a different version.
    pub snapshot: SnapshotId,
    /// Engine build that produced the rows.
    pub engine: EngineVersion,
    /// Rows in canonical order.
    pub rows: Vec<FactRow>,
    /// Arena holding every proof referenced by `rows[..].proof`.
    ///
    /// `None` when the request did not ask for proofs. When it is `Some`, every
    /// row has a `Some(proof)` root; when the result is empty, the arena holds no
    /// roots at all.
    pub proof: Option<ProofArena>,
    /// Execution provenance.
    pub stats: ExecStats,
}

impl SourceResult {
    /// The row for `to`, if the evaluation produced one.
    ///
    /// `None` is a definitive `absent` because a returned result is complete.
    #[must_use]
    pub fn contains(&self, to: TermId) -> Option<&FactRow> {
        self.rows.iter().find(|row| row.to == to)
    }

    /// Canonical comparison view, for differential testing.
    ///
    /// Proof handles, arena layout, [`ExecStats`] and cache state are excluded on
    /// purpose: they are allowed to differ between correct executors.
    #[must_use]
    pub fn canonical_rows(&self) -> Vec<CanonicalRow> {
        self.rows.iter().map(FactRow::canonical).collect()
    }

    /// Whether every row carries a resolvable proof root.
    ///
    /// The contract for `with_proof=true`: this must hold, and an empty result must
    /// have no roots. A partial proof set is a bug, not a "best effort".
    #[must_use]
    pub fn proofs_complete(&self) -> bool {
        match (&self.proof, self.rows.is_empty()) {
            (None, _) => false,
            (Some(_), true) => self.proof.as_ref().is_some_and(ProofArena::is_empty),
            (Some(arena), false) => self
                .rows
                .iter()
                .all(|row| row.proof.is_some_and(|root| arena.get(root).is_some())),
        }
    }
}

/// Identity and digest a caller reports back to the client.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ResultIdentity {
    /// KB the result belongs to.
    pub kb_uuid: KbUuid,
    /// Version evaluated.
    pub version: KbVersion,
    /// Rules that were in force; not interchangeable with the engine version.
    pub rules_digest: RulesDigest,
    /// Engine build that produced the rows.
    pub engine: EngineVersion,
    /// Digest over canonical rows, excluding execution statistics and proofs.
    pub result_digest: Digest,
}
