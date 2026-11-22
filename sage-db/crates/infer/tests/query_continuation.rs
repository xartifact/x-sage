//! Public continuation seams retain one query meter and canonical rule seeds.
mod support;

use sage_db_infer::{Adjacency, OptimizedExecutor, Work};
use sage_db_types::{
    Budget, BudgetLimit, ControlStop, QueryControl, QueryError, RelationId, RuleProfile,
    RunToCompletion, SemanticLayer, SourceExecutor, SourceQuery,
};
use support::Builder;

#[test]
fn ordinary_and_shared_meter_evaluations_have_identical_rows_proofs_and_stats() {
    let mut b = Builder::new(RuleProfile::FormalV1);
    let a = b.term(SemanticLayer::L2);
    let mid = b.term(SemanticLayer::L1);
    let leaf = b.term(SemanticLayer::L0);
    b.edge_c(a, RelationId::IsA, mid);
    b.edge_c(mid, RelationId::DependsOn, leaf);
    let graph = b.build();
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: a,
        relation: RelationId::DependsOn,
        with_proof: true,
        budget: Budget::DEFAULT,
    };
    let ordinary = OptimizedExecutor
        .solve(&graph, &query, &RunToCompletion)
        .unwrap();
    let mut work = Work::new(query.budget);
    let shared = OptimizedExecutor
        .solve_with_work(&graph, &query, &RunToCompletion, &mut work)
        .unwrap();
    assert_eq!(ordinary.rows, shared.rows);
    assert_eq!(ordinary.stats, shared.stats);
    assert!(shared.proofs_complete());
    sage_db_infer::verify_rows_with(&graph, &shared, &RunToCompletion, query.budget).unwrap();
}

#[test]
fn prior_work_is_not_reset_and_a_wider_meter_is_rejected() {
    let mut b = Builder::new(RuleProfile::LegacyV1);
    let a = b.term(SemanticLayer::L0);
    let graph = b.build();
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: a,
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget {
            max_steps: 1,
            ..Budget::DEFAULT
        },
    };
    let mut work = Work::new(query.budget);
    work.step(1).unwrap();
    assert_eq!(
        OptimizedExecutor
            .solve_with_work(&graph, &query, &RunToCompletion, &mut work)
            .unwrap_err(),
        QueryError::BudgetExceeded(BudgetLimit::Steps)
    );
    let mut wide = Work::new(Budget::DEFAULT);
    assert!(matches!(
        OptimizedExecutor.solve_with_work(&graph, &query, &RunToCompletion, &mut wide),
        Err(QueryError::InvalidRequest { .. })
    ));
}

#[test]
fn formal_dependency_walk_uses_real_virtual_seeds_while_legacy_keeps_authored_only() {
    for profile in [RuleProfile::LegacyV1, RuleProfile::FormalV1] {
        let mut b = Builder::new(profile);
        let a = b.term(SemanticLayer::L1);
        let parent = b.term(SemanticLayer::L0);
        b.edge_c(a, RelationId::IsA, parent);
        let graph = b.build();
        let mut work = Work::new(Budget::DEFAULT);
        let adjacency =
            Adjacency::build(&graph, RelationId::DependsOn, &RunToCompletion, &mut work).unwrap();
        let before = work.steps();
        let mut edges = Vec::new();
        adjacency
            .visit_seed_endpoints(&RunToCompletion, &mut work, |from, to, _| {
                edges.push((from, to));
                Ok(())
            })
            .unwrap();
        let expected = if profile == RuleProfile::FormalV1 {
            vec![(a, parent)]
        } else {
            vec![]
        };
        assert_eq!(edges, expected);
        assert_eq!(work.steps() - before, edges.len() as u64);
    }
}

struct Stop;
impl QueryControl for Stop {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        Err(ControlStop::Cancelled)
    }
}
#[test]
fn an_empty_seed_walk_still_observes_control() {
    let b = Builder::new(RuleProfile::LegacyV1);
    let graph = b.build();
    let mut work = Work::new(Budget::DEFAULT);
    let adjacency =
        Adjacency::build(&graph, RelationId::DependsOn, &RunToCompletion, &mut work).unwrap();
    assert_eq!(
        adjacency.visit_seed_endpoints(&Stop, &mut work, |_, _, _| Ok(())),
        Err(QueryError::Stopped(ControlStop::Cancelled))
    );
}
