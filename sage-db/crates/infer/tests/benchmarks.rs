//! Bounded benchmarks: scan counts, duration and memory evidence.
//!
//! These are correctness-with-cost tests, not published performance claims. Each
//! prints measured numbers and asserts only the properties the design promises:
//!
//! - work stops inside the budget (no OOM-then-check);
//! - a deliberate budget refusal is a typed error;
//! - scan counts scale with the *reachable* subgraph, not with the all-pairs closure.

mod support;

use std::time::Instant;

use sage_db_infer::OptimizedExecutor;
use sage_db_types::{
    Budget, BudgetLimit, Confidence, RelationId, RuleProfile, RunToCompletion, SemanticLayer,
    SourceExecutor, SourceQuery, TermId,
};
use support::Builder;

struct Measurement {
    rows: usize,
    steps: u64,
    work_rows: u64,
    work_bytes: u64,
    millis: u128,
}

fn measure(graph: &sage_db_types::BaseGraph, source: TermId, with_proof: bool) -> Measurement {
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source,
        relation: RelationId::DependsOn,
        with_proof,
        budget: Budget::DEFAULT,
    };
    let started = Instant::now();
    let result = OptimizedExecutor::new()
        .solve(graph, &query, &RunToCompletion)
        .expect("benchmark query must complete");
    let millis = started.elapsed().as_millis();
    Measurement {
        rows: result.rows.len(),
        steps: result.stats.steps,
        work_rows: result.stats.work_rows,
        work_bytes: result.stats.work_bytes,
        millis,
    }
}

#[test]
fn sparse_graph_stays_cheap() {
    // 500 nodes, each depending on exactly one successor: a sparse DAG whose closure
    // from the root is a single chain.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, 500);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();
    let m = measure(&graph, terms[0], false);
    eprintln!(
        "sparse 500-chain: rows={} steps={} work_rows={} bytes={} ms={}",
        m.rows, m.steps, m.work_rows, m.work_bytes, m.millis
    );
    assert_eq!(m.rows, 499, "every reachable node becomes a row");
    // Linear work: scanning is O(V+E), nowhere near the 499*500/2 closure size.
    assert!(
        m.steps < 20_000,
        "a 500-node chain must not cost closure-sized work, got {} steps",
        m.steps
    );
}

#[test]
fn a_long_chain_does_not_materialise_the_square_closure() {
    // The design's headline case: a 1,000-node depends-on chain has 999 direct edges
    // but 499,500 pairs in its closure. Work must track edges, not pairs.
    const NODES: u32 = 1_000;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();
    let m = measure(&graph, terms[0], false);
    let closure_pairs = (NODES as u64 - 1) * (NODES as u64 - 2) / 2;
    eprintln!(
        "chain {NODES}: rows={} steps={} closure_pairs={closure_pairs} bytes={} ms={}",
        m.rows, m.steps, m.work_bytes, m.millis
    );
    assert_eq!(m.rows as u64, NODES as u64 - 1);
    assert!(
        m.steps < closure_pairs / 10,
        "steps {} should be far below the {} closure pairs",
        m.steps,
        closure_pairs
    );
}

#[test]
fn dense_dag_scans_proportional_to_edges() {
    // 120 nodes, every node pointing at the next 5: a dense DAG with ~600 edges.
    const NODES: u32 = 120;
    const WIDTH: u32 = 5;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for (position, term) in terms.iter().enumerate() {
        for step in 1..=WIDTH {
            if let Some(target) = terms.get(position + step as usize) {
                builder.edge(*term, RelationId::DependsOn, *target, Confidence::Confirmed);
            }
        }
    }
    let graph = builder.build();
    let edges = graph.assertion_count() as u64;
    let m = measure(&graph, terms[0], false);
    eprintln!(
        "dense {NODES}x{WIDTH}: edges={edges} rows={} steps={} ms={}",
        m.rows, m.steps, m.millis
    );
    assert_eq!(m.rows, (NODES - 1) as usize);
    assert!(
        m.steps < edges * 20,
        "steps {} should stay near edge count {edges}",
        m.steps
    );
}

#[test]
fn diamond_paths_pick_the_wider_route() {
    // Two routes to the same node: one confirmed, one contested. The annotation must
    // be the max, and the dispute flag must still be set.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let root = builder.term(SemanticLayer::L2);
    let left = builder.term(SemanticLayer::L1);
    let right = builder.term(SemanticLayer::L1);
    let sink = builder.term(SemanticLayer::L0);
    builder.edge(root, RelationId::DependsOn, left, Confidence::Confirmed);
    builder.edge(root, RelationId::DependsOn, right, Confidence::Contested);
    builder.edge(left, RelationId::DependsOn, sink, Confidence::Confirmed);
    builder.edge(right, RelationId::DependsOn, sink, Confidence::Hypothetical);
    let graph = builder.build();

    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: root,
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget::DEFAULT,
    };
    let result = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap();
    let sink_row = result.contains(sink).expect("sink reached");
    // Route A: confirmed ∘ confirmed = confirmed. Route B: contested ∘ hypothetical
    // = contested. Max across routes = confirmed.
    assert_eq!(
        sink_row.annotation,
        sage_db_types::Annotation::present(Confidence::Confirmed),
        "min along each route, then max across routes"
    );
    assert!(
        sink_row.disputed,
        "the contested route is still recorded even though it lost"
    );
}

#[test]
fn a_deliberate_budget_rejection_is_typed_and_fast() {
    // The cost of refusing must be small: the stop happens during expansion, not
    // after computing everything.
    const NODES: u32 = 400;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();

    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: terms[0],
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget {
            max_steps: 50,
            ..Budget::DEFAULT
        },
    };
    let started = Instant::now();
    let error = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap_err();
    let millis = started.elapsed().as_millis();
    eprintln!("budget rejection: {error} after {millis} ms");
    assert_eq!(
        error,
        sage_db_types::QueryError::BudgetExceeded(BudgetLimit::Steps)
    );
    assert!(millis < 100, "refusal must be immediate, took {millis} ms");
}

#[test]
fn proof_nodes_stay_linear_on_a_chain_under_the_default_budget() {
    // T5 builds one proof DAG per query and reuses composed prefixes across rows, so a
    // chain costs O(nodes) arena entries rather than O(rows x depth). This measures
    // that using the DEFAULT budget: no raised allowance is needed for 200 rows, and
    // the node bound is the accepted linear contract (<= 4 * N), not a rows*depth
    // estimate. No wall-clock gate is asserted here; timing is printed for context only.
    const NODES: u32 = 200;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: terms[0],
        relation: RelationId::DependsOn,
        with_proof: true,
        budget: Budget::DEFAULT,
    };
    let started = Instant::now();
    let result = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .expect("200 rows must fit the default proof budget with prefix reuse");
    let millis = started.elapsed().as_millis();
    eprintln!(
        "proofs over {NODES}-chain: rows={} proof_nodes={} bytes={} ms={millis}",
        result.rows.len(),
        result.stats.proof_nodes,
        result.stats.work_bytes
    );
    assert_eq!(result.rows.len() as u32, NODES - 1);
    assert!(result.proofs_complete());
    // Linear in chain length, matching the accepted contract in provenance tests.
    let linear = 4 * u64::from(NODES);
    assert!(
        u64::from(result.stats.proof_nodes) <= linear,
        "proof nodes {} exceeded the linear bound {linear}",
        result.stats.proof_nodes
    );
}
