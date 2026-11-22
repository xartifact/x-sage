//! Exact fact queries: complete relation answers, point facts and explanations.
//!
//! # The three questions
//!
//! | function | question | answer |
//! |---|---|---|
//! | [`answer_relation`] | what does `source` reach by `relation`? | the complete [`SourceResult`] |
//! | [`point_fact`] | is `source relation to` true? | [`FactAnswer`] — present with the row, or definitively absent |
//! | [`explain_fact`] | why is it true? | the result plus the proof root for that fact |
//!
//! # Why the whole evaluation is the only way to answer
//!
//! A point query is answered by running the **same complete solve** a relation query
//! runs and then looking the fact up in it. That is deliberate and it is the only
//! honest construction available: [`SourceResult::contains`] is documented as a
//! definitive `None` precisely because a returned result is complete, and that
//! property is destroyed the moment a wrapper stops early. So:
//!
//! * absence is returned **only after** a successful complete solve, never from a
//!   partial scan, a display prefix or a work-budget failure;
//! * a budget refusal, a stop or an invalid request is an **error**, never an empty
//!   answer — "I could not finish" must not be spellable as "it is not there".
//!
//! # Who owns the rows
//!
//! [`Answer`] owns the [`SourceResult`] the executor produced. Rows are *not* copied
//! out, proofs are *not* deep-cloned, and the proof arena keeps its own roots, so a
//! row's [`sage_db_types::proof::ProofRef`] stays meaningful for as long as the answer
//! lives. Callers borrow
//! through accessors; a returned `&FactRow` points into the owner, not into a
//! temporary, so it survives the scope that produced it.
//!
//! # Snapshot and rule binding
//!
//! The request's [`SnapshotId`] must equal the view's basis field for field. Comparing
//! only the version would let a view built under different rules answer a request that
//! named them, so all four fields are compared and the first difference is named. A
//! mismatch is a refusal: there is no "latest" fallback, because a caller that named a
//! snapshot is asking about *that* state.

use sage_db_infer::OptimizedExecutor;
use sage_db_infer::meter::Work;
use sage_db_types::budget::Budget;
use sage_db_types::graph::SnapshotId;
use sage_db_types::identity::TermId;
use sage_db_types::query::{
    ExecStats, FactRow, QueryControl, QueryError, SourceQuery, SourceResult,
};
use sage_db_types::relation::RelationId;
use sage_db_types::snapshot::SnapshotView;

/// Which field of a [`SnapshotId`] disagreed with the view's.
///
/// Named individually because the remedies differ: a different KB is the wrong
/// database, a different version is a stale pin, and a different graph or rule set
/// means the two sides were built under different rules and answering across that
/// boundary would combine two graphs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SnapshotField {
    /// Different knowledge base.
    Kb,
    /// Different committed version.
    Version,
    /// Same version, different graph contents.
    GraphDigest,
    /// Same graph, different rule configuration.
    Rules,
}

/// Why a fact query produced no answer.
///
/// Exactly one variant wraps [`QueryError`]: this layer adds no error class of its own
/// beyond the snapshot binding check. A `QueryError` keeps whatever it already meant —
/// budget, stop, unknown term, invalid request — so a caller never has to translate one
/// refusal vocabulary into another.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FactQueryError {
    /// The request named a snapshot the view is not.
    ///
    /// Only the differing **field** is carried. Both `SnapshotId`s are already in the
    /// caller's hand (it built the request and it holds the view), so copying two
    /// four-field values into every refusal would make the error larger than the
    /// answer it stands for while adding no information.
    SnapshotMismatch {
        /// Which field differed.
        field: SnapshotField,
    },
    /// The real executor refused, or the caller stopped it.
    Executor(QueryError),
}

impl From<QueryError> for FactQueryError {
    fn from(error: QueryError) -> Self {
        Self::Executor(error)
    }
}

/// A complete relation answer: the executor's own result, plus the basis it is bound to.
///
/// The [`SourceResult`] is retained as the owner. Nothing is cloned, re-sorted or
/// re-annotated, so `source_binding` is a plain copy of the view's [`SnapshotId`] that
/// the rows were actually computed at.
#[derive(Debug)]
pub struct Answer {
    basis: SnapshotId,
    result: SourceResult,
}

impl Answer {
    /// The snapshot these rows were computed at.
    ///
    /// Always the **view's** basis, which the request was checked against; this is what
    /// a caller binds a cached result or a later diff to.
    #[must_use]
    pub const fn basis(&self) -> SnapshotId {
        self.basis
    }

    /// Every returned row, in the executor's canonical order.
    ///
    /// Complete: an answer exists only if the evaluation finished.
    #[must_use]
    pub fn rows(&self) -> &[FactRow] {
        &self.result.rows
    }

    /// How many facts the complete evaluation returned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.result.rows.len()
    }

    /// Whether the complete evaluation returned no facts.
    ///
    /// `true` here is a real answer ("nothing reaches"), not a truncated display.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.result.rows.is_empty()
    }

    /// Execution provenance, exactly as the executor reported it.
    #[must_use]
    pub fn stats(&self) -> ExecStats {
        self.result.stats
    }

    /// The proof arena, when the request asked for proofs.
    ///
    /// `None` means no proofs were requested. When it is `Some`, every row's
    /// [`sage_db_types::proof::ProofRef`] resolves inside it.
    #[must_use]
    pub fn proof(&self) -> Option<&sage_db_types::proof::ProofArena> {
        self.result.proof.as_ref()
    }

    /// The underlying result, borrowed rather than copied.
    #[must_use]
    pub const fn result(&self) -> &SourceResult {
        &self.result
    }

    /// Take the result, transferring ownership without copying.
    #[must_use]
    pub fn into_result(self) -> SourceResult {
        self.result
    }

    /// The row for one object, if the complete evaluation produced it.
    ///
    /// A bounded read accessor over this answer's **own** complete result: a binary
    /// search on `to`, which is the first component of the canonical row order
    /// (`FactRow::sort_key`), so it costs at most `log2(u32::MAX) = 32` comparisons
    /// regardless of result size, allocates nothing and sorts nothing.
    ///
    /// It is **not** a query: the answer already exists and was already paid for by the
    /// evaluation. Consequently it takes no control hook and charges no work
    /// accounting — there is no meter to charge, and holding a borrowed control inside
    /// `Answer` would tie a plain value to a caller's lifetime for a lookup that is
    /// constant-bounded. A caller that needs cancellable inspection should use
    /// [`point_fact`], which does consult control around its lookup.
    #[must_use]
    pub fn row(&self, to: TermId) -> Option<&FactRow> {
        let index = self
            .result
            .rows
            .binary_search_by_key(&to, |row| row.to)
            .ok()?;
        self.result.rows.get(index)
    }
}

/// The answer to "is this one fact true".
///
/// Three states, never conflated. `Absent` is only ever produced from a **complete**
/// solve, so it is a fact about the graph rather than about how far this call got.
#[derive(Debug)]
pub enum FactAnswer {
    /// The fact is present, with the row the executor produced.
    Present {
        /// The complete answer it came from, retained as the owner of the row.
        answer: Answer,
        /// Index of the fact's row inside [`Answer::rows`].
        ///
        /// Stored as an index rather than a reference so `Answer` stays movable and
        /// the row borrow is taken through an accessor.
        index: usize,
    },
    /// The complete evaluation finished and produced no such fact.
    Absent {
        /// The complete answer it was decided from.
        answer: Answer,
    },
}

impl FactAnswer {
    /// Whether the fact is present.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        matches!(self, Self::Present { .. })
    }

    /// The basis this answer is bound to.
    #[must_use]
    pub const fn basis(&self) -> SnapshotId {
        match self {
            Self::Present { answer, .. } | Self::Absent { answer } => answer.basis(),
        }
    }

    /// The complete answer this decision was made from, in both states.
    ///
    /// Present even for `Absent`, because the completeness of that answer is the
    /// evidence that absence is real.
    #[must_use]
    pub const fn answer(&self) -> &Answer {
        match self {
            Self::Present { answer, .. } | Self::Absent { answer } => answer,
        }
    }

    /// The fact's row, when present.
    #[must_use]
    pub fn row(&self) -> Option<&FactRow> {
        match self {
            Self::Present { answer, index } => answer.rows().get(*index),
            Self::Absent { .. } => None,
        }
    }

    /// Take the complete answer, dropping the point-index bookkeeping.
    #[must_use]
    pub fn into_answer(self) -> Answer {
        match self {
            Self::Present { answer, .. } | Self::Absent { answer } => answer,
        }
    }
}

/// Compare a request's snapshot against the view's, naming the first difference.
fn bind_snapshot(requested: SnapshotId, view: SnapshotId) -> Result<(), FactQueryError> {
    let field = if requested.kb_uuid != view.kb_uuid {
        SnapshotField::Kb
    } else if requested.version != view.version {
        SnapshotField::Version
    } else if requested.graph_digest != view.graph_digest {
        SnapshotField::GraphDigest
    } else if requested.rules != view.rules {
        SnapshotField::Rules
    } else {
        return Ok(());
    };
    Err(FactQueryError::SnapshotMismatch { field })
}

/// One fact query, as a caller states it.
///
/// Deliberately a small value: subject, relation, whether a proof is wanted, and the
/// budget. The object of a point query is not part of the request — it is the fact
/// being asked about — which is why [`point_fact`] takes it separately.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FactQuery {
    /// The snapshot the caller pins. Must equal the view's basis.
    pub snapshot: SnapshotId,
    /// Subject of the evaluation.
    pub source: TermId,
    /// Relation to evaluate from `source`.
    pub relation: RelationId,
    /// Ask for a proof root per row (and therefore for [`explain_fact`]).
    pub with_proof: bool,
    /// The real inference budget.
    ///
    /// Carried verbatim into the executor. The display limits inside it
    /// (`max_rows`, `max_output_bytes`) do not affect this crate's answers.
    pub budget: Budget,
}

impl FactQuery {
    /// Build the query body the executor consumes.
    fn to_source_query(self, snapshot: SnapshotId) -> SourceQuery {
        SourceQuery {
            snapshot,
            source: self.source,
            relation: self.relation,
            with_proof: self.with_proof,
            budget: self.budget,
        }
    }
}

/// Run one complete evaluation and bind it to the view's own basis.
///
/// This is the single place a [`SourceResult`] is produced, so the three public
/// answers cannot disagree about which snapshot they came from.
fn solve<V: SnapshotView + ?Sized>(
    view: &V,
    query: FactQuery,
    control: &dyn QueryControl,
) -> Result<Answer, FactQueryError> {
    // The ordinary path constructs exactly one meter and delegates, so there is a single
    // implementation of the binding/executor/rebinding sequence below.
    let mut work = Work::new(query.budget);
    solve_with_work(view, query, control, &mut work)
}

/// The one binding/execution/rebinding path, on a caller-supplied meter.
///
/// Crate-private: it exists so a second query in this crate can share one allowance across
/// phases without a second executor call path. It keeps the same snapshot binding before and
/// after execution, the same executor and the same result rebinding — nothing is duplicated.
///
/// The ordinary [`solve`] constructs exactly one meter and delegates here, so this is the single
/// implementation of that sequence.
pub(crate) fn solve_with_work<V: SnapshotView + ?Sized>(
    view: &V,
    query: FactQuery,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<Answer, FactQueryError> {
    if work.budget() != query.budget {
        // A meter built for a different allowance cannot pay this request honestly.
        return Err(FactQueryError::Executor(QueryError::InvalidRequest {
            reason: "query and shared work meter have different ceilings",
        }));
    }
    let basis = view.basis();
    // Request binding happens BEFORE any membership question, so a caller that named
    // another snapshot can never be answered from this view's rows.
    bind_snapshot(query.snapshot, basis)?;
    // Rule identity is NOT re-checked here: the executor's own
    // `SourceQuery::check_against` compares the request's rules against the graph's
    // snapshot, and `SnapshotView`'s contract keeps `view.rules()` equal to
    // `view.basis().rules`. Compare them here as well would be a second, weaker
    // authority for a fact the trait already guarantees.
    //
    // (Q3: the previous comment claimed this function verified that the view's rules
    // agreed with its basis. It does not, and should not — that obligation belongs to
    // `SourceQuery::check_against` and to the trait contract.)
    let source_query = query.to_source_query(basis);
    let result =
        OptimizedExecutor::new().solve_with_work(view.graph(), &source_query, control, work)?;
    // The executor echoes the snapshot it computed at. If it ever disagreed with the
    // request we handed it, the rows would belong to a different graph, so refuse
    // rather than return rows under a basis they were not computed at.
    bind_snapshot(result.snapshot, basis)?;
    Ok(Answer { basis, result })
}

/// Answer "what does `source` reach by `relation`" completely.
///
/// # Errors
///
/// [`FactQueryError::SnapshotMismatch`] when the request names another snapshot than
/// the view's; [`FactQueryError::Executor`] for a real budget refusal, a stop, an
/// unknown term or an invalid request. An empty [`Answer`] is a completed evaluation
/// that found nothing — never a truncation.
pub fn answer_relation<V: SnapshotView + ?Sized>(
    view: &V,
    query: FactQuery,
    control: &dyn QueryControl,
) -> Result<Answer, FactQueryError> {
    solve(view, query, control)
}

/// Answer "is `source relation to` true" from one complete evaluation.
///
/// The evaluation is the same one [`answer_relation`] runs. Absence is reported only
/// after it succeeded, so `Absent` is a fact about the graph rather than about this
/// call's progress.
///
/// # Errors
///
/// As [`answer_relation`]. In particular a budget refusal or a stop is an error, never
/// `Absent`.
pub fn point_fact<V: SnapshotView + ?Sized>(
    view: &V,
    query: FactQuery,
    to: TermId,
    control: &dyn QueryControl,
) -> Result<FactAnswer, FactQueryError> {
    let answer = solve(view, query, control)?;
    lookup_point(answer, to, view, control)
}

/// Decide one point fact from a **complete** answer, with a typed unknown target.
///
/// # Why the target is checked against the graph, not only against the rows
///
/// `query-dsl.md` §3 states that an unknown concept is `UNKNOWN_TERM`. "The object
/// does not exist" and "the object exists but nothing reaches it" are different
/// operator answers, and a point query that folded the first into the second would be
/// wrong even though the row set is correct. So the check is `view.graph().term(to)`
/// — the same graph the answer was computed from — and only an object that *is* known
/// can be reported `Absent`.
///
/// # Why a binary search
///
/// Rows are canonically ordered by `FactRow::sort_key`, whose first component is `to`
/// (`infer` sorts before returning). A `binary_search_by_key` on `to` is therefore
/// exact, allocates nothing, sorts nothing, and costs at most `log2(u32::MAX) = 32`
/// comparisons over the whole `TermId` domain — constant with respect to the result
/// size. A linear scan would be unnecessary work on the hot point path.
///
/// # Control
///
/// The caller's control is consulted **before** the lookup and **after** it, so a
/// cancellation that arrives while the wrapper is inspecting rows is observed rather
/// than reported as an answer. This is wrapper work, not evaluation work: it charges
/// no inference budget and never touches `ExecStats`.
fn lookup_point<V: SnapshotView + ?Sized>(
    answer: Answer,
    to: TermId,
    view: &V,
    control: &dyn QueryControl,
) -> Result<FactAnswer, FactQueryError> {
    // Control before the wrapper touches any row: a caller that already stopped must
    // not receive an answer this call produced by continuing. A stop becomes the same
    // typed `QueryError::Stopped` the executor would have produced.
    control.checkpoint().map_err(QueryError::Stopped)?;
    let index = answer.rows().binary_search_by_key(&to, |row| row.to).ok();
    control.checkpoint().map_err(QueryError::Stopped)?;
    match index {
        Some(index) => Ok(FactAnswer::Present { answer, index }),
        None => {
            // No row. That is only `Absent` if the object is a term this snapshot
            // knows; otherwise the honest answer is the typed unknown.
            if view.graph().term(to).is_none() {
                return Err(QueryError::UnknownTerm(to).into());
            }
            Ok(FactAnswer::Absent { answer })
        }
    }
}

/// Answer "why is `source relation to` true", with its proof.
///
/// The proof root is the executor's own, inside the answer's arena; nothing is
/// re-derived here. A request whose `with_proof` is `false` cannot explain, so it is
/// refused as an invalid request rather than answered without evidence — an
/// explanation without a proof would be a claim this layer cannot support.
///
/// # Errors
///
/// As [`answer_relation`], plus [`FactQueryError::Executor`] wrapping
/// [`QueryError::InvalidRequest`] when `with_proof` was not requested.
pub fn explain_fact<V: SnapshotView + ?Sized>(
    view: &V,
    query: FactQuery,
    to: TermId,
    control: &dyn QueryControl,
) -> Result<FactAnswer, FactQueryError> {
    if !query.with_proof {
        return Err(QueryError::InvalidRequest {
            reason: "explain requires with_proof",
        }
        .into());
    }
    point_fact(view, query, to, control)
}
