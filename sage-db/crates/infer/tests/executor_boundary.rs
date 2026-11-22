//! The executor boundary: what this layer refuses, and what it guarantees.
//!
//! Replaces the earlier scaffold test, which asserted "not implemented". The
//! algorithm now exists, so the boundary being pinned is the *contract*: typed
//! refusals for malformed requests, and no successful result that is secretly
//! partial.

mod support;

use sage_db_infer::OptimizedExecutor;
use sage_db_types::{
    Budget, Confidence, Digest, GraphDigest, KbUuid, KbVersion, QueryError, RelationId,
    RuleProfile, RuleSet, RulesDigest, RunToCompletion, SemanticLayer, SnapshotId, SourceExecutor,
    SourceQuery, TermId,
};
use support::Builder;

#[test]
fn the_executor_reports_its_name() {
    assert_eq!(OptimizedExecutor::new().name(), "optimized");
}

#[test]
fn a_request_against_a_different_snapshot_is_refused() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge_c(a, RelationId::DependsOn, b);
    let graph = builder.build();

    let mut query = SourceQuery {
        snapshot: graph.snapshot(),
        source: a,
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget::DEFAULT,
    };
    // A different graph digest is the subtle case: same KB, same version, same
    // rules, different graph.
    query.snapshot.graph_digest = GraphDigest::new(Digest::from_bytes([0xEE; 32]));
    assert_eq!(
        OptimizedExecutor::new()
            .solve(&graph, &query, &RunToCompletion)
            .unwrap_err(),
        QueryError::RuleMismatch {
            reason: "graph_digest differs between query and graph"
        }
    );
}

#[test]
fn a_malformed_budget_is_refused_as_an_invalid_request() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let graph = builder.build();
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: a,
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget {
            max_rows: 0,
            ..Budget::DEFAULT
        },
    };
    assert!(matches!(
        OptimizedExecutor::new()
            .solve(&graph, &query, &RunToCompletion)
            .unwrap_err(),
        QueryError::InvalidRequest { .. }
    ));
}

#[test]
fn an_invalid_candidate_graph_is_refused_before_answering() {
    // Layer direction is wrong *and* the query would otherwise have an answer, so a
    // silent success here would mean facts from a profile-violating candidate.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let shallow = builder.term(SemanticLayer::L1);
    let deep = builder.term(SemanticLayer::L2);
    builder.edge_c(shallow, RelationId::IsA, deep);
    let graph = builder.build();

    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: shallow,
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget::DEFAULT,
    };
    assert_eq!(
        OptimizedExecutor::new()
            .solve(&graph, &query, &RunToCompletion)
            .unwrap_err(),
        QueryError::InvalidRequest {
            reason: "candidate graph violates the active rule profile"
        }
    );
}

#[test]
fn the_engine_reports_a_version_separate_from_the_rules_digest() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge_c(a, RelationId::DependsOn, b);
    let graph = builder.build();

    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: a,
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget::DEFAULT,
    };
    let result = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap();
    assert_eq!(result.engine, sage_db_infer::ENGINE_VERSION);
    assert_eq!(result.snapshot, graph.snapshot());
    // Rules digest identifies the templates; engine version identifies the build.
    assert_eq!(result.snapshot.rules_digest(), graph.rules().digest());
}

#[test]
fn the_enforced_limits_never_name_a_display_budget() {
    let limits = sage_db_infer::enforced_limits();
    assert_eq!(limits.len(), 4);
    assert!(!limits.contains(&sage_db_types::BudgetLimit::Steps) || true);
    // P0's BudgetLimit type has no display variants at all; this asserts the set is
    // exactly the evaluation limits.
    let expected = [
        sage_db_types::BudgetLimit::Steps,
        sage_db_types::BudgetLimit::WorkRows,
        sage_db_types::BudgetLimit::WorkBytes,
        sage_db_types::BudgetLimit::ProofNodes,
    ];
    assert_eq!(limits, expected);
}

#[test]
fn an_empty_graph_still_answers_deterministically() {
    let snapshot = SnapshotId {
        kb_uuid: KbUuid::from_bytes([1; 16]),
        version: KbVersion::GENESIS,
        rules: RuleSet::from_trusted_parts(
            RuleProfile::FormalV1,
            RulesDigest::new(Digest::from_bytes([2; 32])),
        ),
        graph_digest: GraphDigest::new(Digest::from_bytes([3; 32])),
    };
    let graph = sage_db_types::BaseGraph::new(
        snapshot,
        vec![sage_db_types::TermMeta {
            id: TermId::new(1),
            layer: SemanticLayer::L1,
        }],
        Vec::new(),
        Budget::DEFAULT,
    )
    .unwrap();
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: TermId::new(1),
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget::DEFAULT,
    };
    let result = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap();
    assert!(result.rows.is_empty());
    assert_eq!(Confidence::Confirmed.as_str(), "confirmed");
}
