//! The executor boundary.
//!
//! Two independent executors implement this trait: the optimized one in
//! `sage-db-infer` and the naive reference one in `sage-db-reference`. Neither
//! may call the other in production, and the reference crate must not depend on
//! `sage-db-infer` at all — only differential tests in `infer/tests/` may use it.
//!
//! The trait is deliberately one method wide. Adding a second entry point here
//! would force both executors and their manifests to change together, which is
//! exactly what P0 wants to avoid while T4/T5 are still unbuilt.

use crate::graph::BaseGraph;
use crate::query::{QueryControl, QueryError, SourceQuery, SourceResult};

/// A source query evaluator over one immutable base graph.
///
/// Implementations are pure with respect to the outside world: the only inputs are
/// the graph, the request and the caller's [`QueryControl`], and the only outputs
/// are rows, an optional proof arena and execution statistics. Nothing may read a
/// clock, the filesystem or the environment; time and cancellation arrive through
/// the control hook.
///
/// # Scope of this layer (P0)
///
/// This is an **internal complete-fact evaluation** interface, not a display or
/// serialization interface. It is bounded by
/// [`crate::budget::Budget::max_steps`], `max_work_rows`, `max_work_bytes` and
/// `max_proof_nodes` plus [`QueryControl`]; the result it returns is the *whole*
/// set of facts for the question.
///
/// `max_rows` and `max_output_bytes` are deliberately **not** applied here. Paginating
/// or truncating for a client is the job of the later query/display layer, which
/// may only cut the already-computed, already-ordered result. An implementation of
/// this trait must therefore never:
///
/// - drop rows to satisfy a display limit,
/// - return a row set that depends on `max_rows`/`max_output_bytes`, or
/// - turn a display limit into `absent`.
///
/// Display truncation cannot manufacture absence because absence is decided by the
/// complete evaluation that happens here, before any output limit applies.
pub trait SourceExecutor {
    /// Short stable name, used in diagnostics and differential test output.
    fn name(&self) -> &'static str;

    /// Evaluate `query` against `graph`.
    ///
    /// The returned result is a complete evaluation. Any stop — budget, proof
    /// allowance, cancellation, deadline — is returned as [`QueryError`]; an
    /// executor must never return a partially computed row set.
    ///
    /// # Errors
    ///
    /// Returns the typed failure from [`QueryError`].
    fn solve(
        &self,
        graph: &BaseGraph,
        query: &SourceQuery,
        control: &dyn QueryControl,
    ) -> Result<SourceResult, QueryError>;
}
