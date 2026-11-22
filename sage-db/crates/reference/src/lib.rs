//! `sage-db-reference` — the naive reference evaluator and differential oracle.
//!
//! This crate is the slow, obviously-correct implementation of the accepted P0
//! semantics. It exists to define expected answers, so it must not be able to
//! borrow the optimized executor's reasoning: it depends on `sage-db-types` only,
//! copies the raw records with [`BaseGraph::terms`]/[`BaseGraph::assertions`], and
//! builds its own ordering. The dependency direction is asserted by CI.
//!
//! # What it computes
//!
//! For one `(source, relation)` question it returns the **complete** set of facts
//! in that relation from that source, under the query's rule profile:
//!
//! - every active authored assertion in the reported direction (`asserted`);
//! - one R2 mirror for a symmetric relation (`derived`, never `asserted`);
//! - an R3 `depends-on` seed for each active definition assertion when the profile
//!   implies dependency (`derived`);
//! - every R1 composition of a transitive relation (`derived`), saturated to a
//!   fixed point.
//!
//! A relation that is neither transitive nor symmetric still answers: it returns
//! its authored facts and no closure. Refusing would have made a plain fact query
//! look impossible.
//!
//! # Semantics that are easy to get wrong
//!
//! - Absence is not `contested`. `Annotation::ABSENT` is the bottom element and
//!   `FactTable` never stores an absent entry.
//! - A path takes `min`, alternative paths take `max`; a later, stronger route
//!   replaces an earlier weak one, and a weaker route that arrives after a stronger
//!   fact still raises `disputed`/`derived` without moving the annotation.
//! - The authored direction is never rewritten. An R2 mirror is a different row.
//! - `disputed` is over all contributing supports, not only the winning one.
//! - The saturation loop re-applies R1 until **every field** of every entry stops
//!   changing. A key-set delta would miss confidence improvements.
//!
//! # Preconditions on the input graph
//!
//! The reference evaluates **V1–V6-valid snapshots**. It assumes the candidate has
//! already been admitted by commit-time validation: resolvable endpoints, unique
//! identity, no self reference, layer direction respected, cardinality respected and
//! acyclic relations acyclic.
//!
//! It performs **no** validation of its own and it is **not** a commit validator. In
//! particular it does not reject a cycle, a layer violation or a duplicate identity; it
//! simply saturates whatever active assertions it is given.
//!
//! Consequences a caller must know:
//!
//! - **Invalid-candidate parity is not promised.** The optimized executor runs its own
//!   defensive validation and may refuse a candidate this executor answers. A
//!   disagreement on an invalid candidate is therefore not a contract violation, and
//!   `infer/tests/differential.rs` compares only valid fixtures.
//! - The two implementations deliberately share no validator. Sharing one would make
//!   the differential oracle inherit the same acceptance bug as the code it checks.
//!
//! # Resource bounds
//!
//! Every row, byte and scan is charged to one [`Work`]; vector growth is fallible
//! and reserved before it is filled. Rows, bytes, steps and proof nodes are the P0
//! limits. `max_rows`/`max_output_bytes` are **not** applied: this layer returns a
//! complete evaluation, and display truncation belongs to the query layer above.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod closure;
pub mod facts;
pub mod index;
pub mod paths;
pub mod proof;
pub mod work;

use sage_db_types::{
    Annotation, BaseGraph, EngineVersion, ExecStats, FactRow, ProofArena, QueryControl, QueryError,
    SourceExecutor, SourceQuery, SourceResult, TermId,
};

use crate::facts::{FactKey, FactTable};
use crate::index::RawIndex;
use crate::work::Work;

pub use facts::{Entry, FactState, SupportWitness};
pub use proof::{Expected, PlanNode, PlanTable, Renderer, ReplayError, ReplayedFact};
pub use work::Work as Meter;

/// Engine build identifier reported in results.
///
/// Provenance only: the rules decide what may be derived. Bumping this must never
/// change a semantic answer for the same rules and graph.
pub const ENGINE_VERSION: EngineVersion = EngineVersion::new(1);

/// The naive reference executor.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReferenceExecutor;

impl ReferenceExecutor {
    /// Construct the executor. Carries no state: all input arrives per call.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl SourceExecutor for ReferenceExecutor {
    fn name(&self) -> &'static str {
        "reference"
    }

    fn solve(
        &self,
        graph: &BaseGraph,
        query: &SourceQuery,
        control: &dyn QueryControl,
    ) -> Result<SourceResult, QueryError> {
        query.validate()?;
        query.check_against(graph)?;

        let mut work = Work::new(query.budget);
        work.check_entry(control)?;

        if graph.term(query.source).is_none() {
            return Err(QueryError::UnknownTerm(query.source));
        }

        let index = RawIndex::build(graph, &mut work, control)?;
        let table = closure::saturate(&index, query.rules(), query.relation, &mut work, control)?;
        let mut result = assemble(query, &index, &table, &mut work, control)?;
        work.check_exit(control)?;

        result.stats = work.stats();
        Ok(result)
    }
}

/// Turn the saturated table into canonical rows over the queried relation.
pub fn assemble(
    query: &SourceQuery,
    index: &RawIndex,
    table: &FactTable,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<SourceResult, QueryError> {
    let mut rows: Vec<FactRow> = Vec::new();
    crate::work::reserve(&mut rows, table.len(), work)?;

    for entry in table.iter() {
        work.step(1)?;
        work.periodic_check(control)?;
        if entry.key.relation != query.relation || entry.key.from != query.source {
            continue;
        }
        // Identity is not a fact and is never reported.
        if entry.key.to == query.source {
            continue;
        }
        if !entry.state.annotation.is_present() {
            continue;
        }
        work.row()?;
        rows.push(FactRow {
            from: entry.key.from,
            relation: entry.key.relation,
            to: entry.key.to,
            annotation: entry.state.annotation,
            has_asserted: entry.state.asserted,
            has_derived: entry.state.derived,
            disputed: entry.state.disputed,
            strongest_support: entry
                .state
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

    let mut arena = query.with_proof.then(ProofArena::new);
    if let Some(arena) = arena.as_mut() {
        // One plan table and one renderer for the whole result, so a prefix shared
        // by several rows is materialised once instead of once per row.
        let plans = proof::select_all(index, table, query.rules(), query.relation, work, control)?;
        let mut renderer = proof::Renderer::new(&plans);
        for row in &mut rows {
            work.row()?;
            row.proof = Some(renderer.render(
                FactKey::new(row.from, row.relation, row.to),
                arena,
                work,
                control,
            )?);
        }
    }

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

/// Row for `to` in a result, for tests.
#[must_use]
pub fn row_for(result: &SourceResult, to: TermId) -> Option<&FactRow> {
    result.rows.iter().find(|row| row.to == to)
}

/// Annotation of `to` in a result, or absence.
#[must_use]
pub fn annotation_of(result: &SourceResult, to: TermId) -> Annotation {
    row_for(result, to).map_or(Annotation::ABSENT, |row| row.annotation)
}
