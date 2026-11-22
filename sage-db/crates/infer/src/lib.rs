//! `sage-db-infer` — the optimized inference executor.
//!
//! Implements the accepted P0 contract: whole-candidate validation (V1–V6), active
//! support aggregation, bounded three-threshold widest-path evaluation, and bounded
//! proof construction with independent replay.
//!
//! # Scope
//!
//! Pure: no filesystem, network, clock or environment access. Time and cancellation
//! arrive through [`sage_db_types::QueryControl`]. Storage, HTTP, migration and
//! caching are out of scope for P0 — a query always recomputes from the immutable
//! snapshot it is given, which is also why withdrawal needs no incremental
//! deletion machinery: a new snapshot simply has fewer active supports.
//!
//! # Ownership
//!
//! This crate owns `src/**` and its unit tests. `sage-db-reference` and
//! `infer/tests/differential.rs` belong to the reference executor; this crate
//! depends on the reference crate only as a dev-dependency for differential tests.
//!
//! # Budgets
//!
//! Every phase charges the same [`meter::Work`]: validation, aggregation, adjacency
//! building, BFS, proof construction and result assembly. A per-container budget
//! would let each spend a full allowance while all are live at once.
//! `max_rows`/`max_output_bytes` are **not** applied here — they belong to the later
//! presentation layer, and this layer never truncates a result or reports a
//! display-limited row as absent.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod aggregate;
pub mod meter;
pub mod path;
pub mod proof;
pub mod validate;

use sage_db_types::{
    Annotation, BaseGraph, Budget, BudgetLimit, EngineVersion, ExecStats, FactRow, ProofArena,
    QueryControl, QueryError, RelationId, SourceExecutor, SourceQuery, SourceResult,
};

pub use aggregate::{Aggregates, FactKey, FactState, SupportWitness};
pub use meter::Work;
pub use path::{Adjacency, Reach, Witness};
pub use proof::{ReplayError, ReplayedFact};
pub use validate::{MetricStatus, ValidationReport, Violation};

/// Engine build identifier reported in results.
///
/// Bumped when a change could alter results; two builds must agree semantically for
/// the same rules, so this is provenance for cache keys, not a rule input.
pub const ENGINE_VERSION: EngineVersion = EngineVersion::new(1);

/// The optimized executor.
#[derive(Clone, Copy, Debug, Default)]
pub struct OptimizedExecutor;

impl OptimizedExecutor {
    /// Construct the executor. All input arrives per call.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
    /// Evaluate with the caller's existing query work meter.
    ///
    /// This is the same algorithm as `SourceExecutor::solve`; it does not reset counters.
    /// A compound query can continue with canonical seed traversal and ordering on this
    /// SAME meter, so each phase cannot spend a fresh copy of the request allowance.
    /// Returned statistics describe cumulative work through this evaluation.
    ///
    /// # Errors
    /// The ordinary query errors, or `InvalidRequest` if the meter's ceilings differ from
    /// the query. A caller may arrive with already-spent capacity, never a wider budget.
    pub fn solve_with_work(
        &self,
        graph: &BaseGraph,
        query: &SourceQuery,
        control: &dyn QueryControl,
        work: &mut Work,
    ) -> Result<SourceResult, QueryError> {
        if work.budget() != query.budget {
            return Err(QueryError::InvalidRequest {
                reason: "query and shared work meter have different ceilings",
            });
        }
        query.validate()?;
        query.check_against(graph)?;

        work.check_entry(control)?;

        if graph.term(query.source).is_none() {
            return Err(QueryError::UnknownTerm(query.source));
        }
        // Transitive relations get a closure, symmetric ones exactly one hop, and the
        // rest a single authored hop. All seven predicates therefore answer basic
        // fact queries; only an explicit transitive `+` request is a future DSL
        // concern, not a property of this interface.

        // Whole-candidate validation runs as part of every solve: a query against an
        // invalid candidate must not return facts that the profile forbids.
        let report = validate::validate(graph, control, work)?;
        if !report.is_valid() {
            work.check_exit(control)?;
            return Err(QueryError::InvalidRequest {
                reason: "candidate graph violates the active rule profile",
            });
        }

        let reaches = evaluate_relation(graph, query, control, work, &report)?;
        let mut result = assemble(graph, query, control, work, &reaches)?;
        work.check_exit(control)?;

        result.stats = ExecStats {
            steps: work.steps(),
            work_rows: work.work_rows(),
            work_bytes: work.work_bytes(),
            proof_nodes: work.proof_nodes(),
        };
        Ok(result)
    }
}

impl SourceExecutor for OptimizedExecutor {
    fn name(&self) -> &'static str {
        "optimized"
    }

    fn solve(
        &self,
        graph: &BaseGraph,
        query: &SourceQuery,
        control: &dyn QueryControl,
    ) -> Result<SourceResult, QueryError> {
        let mut work = Work::new(query.budget);
        self.solve_with_work(graph, query, control, &mut work)
    }
}

/// Everything one relation evaluation produced.
struct RelationEvaluation {
    reach: Reach,
    aggregates: Aggregates,
}

/// Aggregate supports and run the widest-path closure for the requested relation.
fn evaluate_relation(
    graph: &BaseGraph,
    query: &SourceQuery,
    control: &dyn QueryControl,
    work: &mut Work,
    _report: &ValidationReport,
) -> Result<RelationEvaluation, QueryError> {
    let aggregates = aggregate::aggregate(graph, control, work)?;
    let adjacency = path::Adjacency::build(graph, query.relation, control, work)?;
    let reach = path::evaluate(
        graph,
        &adjacency,
        query.relation,
        query.source,
        control,
        work,
    )?;
    Ok(RelationEvaluation { reach, aggregates })
}

/// Turn the evaluation into canonical rows and, if requested, proofs.
fn assemble(
    graph: &BaseGraph,
    query: &SourceQuery,
    control: &dyn QueryControl,
    work: &mut Work,
    evaluation: &RelationEvaluation,
) -> Result<SourceResult, QueryError> {
    let reaches = &evaluation.reach;
    let aggregates = &evaluation.aggregates;

    let mut rows: Vec<FactRow> = Vec::new();
    // Reserved once at the reached-term bound, then filled by the loop below.
    crate::meter::grow(&mut rows, reaches.terms().len(), work)?;
    let mut arena = query.with_proof.then(ProofArena::new);
    let mut prover = query
        .with_proof
        .then(|| proof::Prover::new(graph, work))
        .transpose()?;

    for term in reaches.terms() {
        work.row()?;
        work.step(1)?;
        work.periodic_check(control)?;
        if *term == query.source {
            continue;
        }
        let annotation = reaches.annotation(*term).unwrap_or(Annotation::ABSENT);
        if !annotation.is_present() {
            continue;
        }
        let key = FactKey {
            from: query.source,
            relation: query.relation,
            to: *term,
        };
        let state = aggregates.get(&key).copied().unwrap_or(FactState::EMPTY);

        // `has_asserted` is about the *reported direction*, so it comes from the
        // aggregate, not from whether the winning path happened to be direct.
        let has_asserted = state.has_asserted;
        let multi_hop = reaches.hops(*term).unwrap_or(0) >= 2;
        let has_derived = reaches.derived(*term) || multi_hop || aggregates.is_reachable(&key);
        // Dispute is over all supports and paths, never only the winner.
        let disputed = state.disputed || reaches.disputed(*term);

        rows.push(FactRow {
            from: query.source,
            relation: query.relation,
            to: *term,
            annotation,
            has_asserted,
            has_derived,
            disputed,
            strongest_support: state
                .witness
                .map(|witness| sage_db_types::AssertionSupport {
                    id: witness.id,
                    revision: witness.revision,
                    confidence: witness.confidence,
                }),
            proof: None,
        });
    }
    rows.sort_unstable_by_key(FactRow::sort_key);

    // Proofs are built after ordering so `rows[i].proof` refers to that row's fact,
    // and each row is proved at the level that produced its annotation.
    if let (Some(arena), Some(prover)) = (arena.as_mut(), prover.as_mut()) {
        for row in &mut rows {
            work.row()?;
            let level = row
                .annotation
                .confidence()
                .ok_or(QueryError::InvalidRequest {
                    reason: "a returned row must have a confidence level",
                })?;
            let root = prover.prove(
                graph,
                query.source,
                row.to,
                level,
                reaches,
                arena,
                control,
                work,
            )?;
            row.proof = Some(root);
        }
    }

    // A proof request must yield a root for every row, or nothing at all.
    if let Some(arena) = arena.as_ref() {
        let complete = rows
            .iter()
            .all(|row| row.proof.is_some_and(|root| arena.get(root).is_some()));
        if !complete {
            return Err(QueryError::InvalidRequest {
                reason: "proof requested but not produced for every returned fact",
            });
        }
    }

    Ok(SourceResult {
        snapshot: query.snapshot,
        engine: ENGINE_VERSION,
        rows,
        proof: arena,
        stats: ExecStats::default(),
    })
}

/// Validate a candidate graph without running a query.
///
/// # Errors
///
/// Propagates budget and control failures.
pub fn validate_candidate(
    graph: &BaseGraph,
    budget: Budget,
    control: &dyn QueryControl,
) -> Result<ValidationReport, QueryError> {
    let mut work = Work::new(budget);
    work.check_entry(control)?;
    let report = validate::validate(graph, control, &mut work)?;
    work.check_exit(control)?;
    Ok(report)
}

/// Verify one row's proof against `graph`.
///
/// Compares the **whole** recovered fact (endpoints, relation, annotation) and the
/// snapshot identity. For more than one row use [`verify_rows`], which walks the arena
/// once instead of once per row.
///
/// # Errors
///
/// Returns [`ReplayError`] on any mismatch.
pub fn verify_row(
    graph: &BaseGraph,
    result: &SourceResult,
    row: &FactRow,
) -> Result<proof::ReplayedFact, ReplayError> {
    if result.snapshot != graph.snapshot() {
        return Err(ReplayError::SnapshotMismatch);
    }
    let facts = proof::replay_rows(
        graph,
        result
            .proof
            .as_ref()
            .ok_or(ReplayError::DanglingReference)?,
        std::slice::from_ref(row),
        &sage_db_types::RunToCompletion,
        result_budget(result),
    )?;
    let recovered = *facts.first().ok_or(ReplayError::DanglingReference)?;
    let claimed = proof::ReplayedFact {
        from: row.from,
        relation: row.relation,
        to: row.to,
        annotation: row.annotation,
    };
    if recovered != claimed {
        return Err(ReplayError::FactMismatch { recovered, claimed });
    }
    Ok(recovered)
}

/// Verify every row in **one** arena pass and check each against its claim.
///
/// Cost is `O(arena_nodes + rows)`: the value table is filled once in topological
/// order, so this is not `rows x depth`.
///
/// # Errors
///
/// Returns the row index and [`ReplayError`] of the first failure.
pub fn verify_rows(
    graph: &BaseGraph,
    result: &SourceResult,
) -> Result<Vec<(usize, proof::ReplayedFact)>, (usize, ReplayError)> {
    verify_rows_with(
        graph,
        result,
        &sage_db_types::RunToCompletion,
        result_budget(result),
    )
}

/// [`verify_rows`] with an explicit control hook and budget.
///
/// The budget covers the **whole** verification: the value table, the node sweep and
/// every row comparison are charged against it, so a large result cannot verify for
/// free.
///
/// # Errors
///
/// Returns the row index and [`ReplayError`] of the first failure, including a stop or
/// an exhausted budget.
pub fn verify_rows_with(
    graph: &BaseGraph,
    result: &SourceResult,
    control: &dyn QueryControl,
    budget: Budget,
) -> Result<Vec<(usize, proof::ReplayedFact)>, (usize, ReplayError)> {
    if result.snapshot != graph.snapshot() {
        return Err((0, ReplayError::SnapshotMismatch));
    }
    let arena = result
        .proof
        .as_ref()
        .ok_or((0, ReplayError::DanglingReference))?;
    // ONE ledger for the whole verification: arena sweep, recovered facts, output
    // vector and the row-comparison loop. A second `Work::new` here would hand the
    // same allowance out twice while `recovered` is still alive.
    let mut work = Work::new(budget);
    work.check_entry(control)
        .map_err(|_| (0, ReplayError::Stopped))?;
    let recovered = proof::replay_rows_with(graph, arena, &result.rows, control, &mut work)
        .map_err(|error| (0, error))?;

    // Phase boundary: the sweep is done and the comparison is about to begin. Without
    // an explicit check here a cancel during a short comparison would only be noticed
    // at the very end, because the 1 024-step interval never elapses on small results.
    work.check(control).map_err(|_| (0, ReplayError::Stopped))?;

    let mut out: Vec<(usize, proof::ReplayedFact)> = Vec::new();
    crate::meter::grow(&mut out, recovered.len(), &mut work)
        .map_err(|_| (0, ReplayError::BudgetExhausted))?;

    // The comparison loop is real work: charged, cancellable, and its rows counted.
    for (index, (row, fact)) in result.rows.iter().zip(recovered).enumerate() {
        work.step(1)
            .map_err(|_| (index, ReplayError::BudgetExhausted))?;
        work.row()
            .map_err(|_| (index, ReplayError::BudgetExhausted))?;
        work.periodic_check(control)
            .map_err(|_| (index, ReplayError::Stopped))?;
        let claimed = proof::ReplayedFact {
            from: row.from,
            relation: row.relation,
            to: row.to,
            annotation: row.annotation,
        };
        if fact != claimed {
            return Err((
                index,
                ReplayError::FactMismatch {
                    recovered: fact,
                    claimed,
                },
            ));
        }
        out.push((index, fact));
    }
    work.check_exit(control)
        .map_err(|_| (0, ReplayError::Stopped))?;
    Ok(out)
}

/// Budget for verifying an existing result.
///
/// Verification re-derives every node, so it needs its own allowance rather than the
/// query's already-spent one.
fn result_budget(_result: &SourceResult) -> Budget {
    Budget::DEFAULT
}

/// Describe the budget limits this executor enforces at this layer.
///
/// Display limits are absent on purpose: [`BudgetLimit`] has no `Rows` or
/// `OutputBytes` variant, so a P0 failure can never name a presentation limit.
#[must_use]
pub const fn enforced_limits() -> [BudgetLimit; 4] {
    [
        BudgetLimit::Steps,
        BudgetLimit::WorkRows,
        BudgetLimit::WorkBytes,
        BudgetLimit::ProofNodes,
    ]
}

/// The relation this executor is willing to evaluate transitively.
///
/// Kept explicit so the refusal path is testable without guessing.
#[must_use]
pub fn supports_transitive(relation: RelationId, rules: sage_db_types::RuleSet) -> bool {
    rules.spec(relation).transitive
}
