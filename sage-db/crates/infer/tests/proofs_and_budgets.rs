//! Proof construction, independent replay, and budget boundaries.

mod support;

use sage_db_infer::{OptimizedExecutor, verify_row};
use sage_db_types::{
    Annotation, Budget, BudgetLimit, Confidence, ProofNode, ProofRef, RelationId, RuleProfile,
    RunToCompletion, SemanticLayer, SourceExecutor, SourceQuery, TermId,
};
use support::Builder;

fn solve_with_proof(
    graph: &sage_db_types::BaseGraph,
    source: TermId,
    relation: RelationId,
) -> sage_db_types::SourceResult {
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source,
        relation,
        with_proof: true,
        budget: Budget::DEFAULT,
    };
    OptimizedExecutor::new()
        .solve(graph, &query, &RunToCompletion)
        .expect("query must succeed")
}

#[test]
fn every_row_proof_replays_to_the_reported_annotation() {
    // Independent replay: the proof is re-derived from the graph's assertions, not
    // trusted from construction.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Hypothetical);
    builder.edge(a, RelationId::DependsOn, c, Confidence::Contested);
    let graph = builder.build();

    let result = solve_with_proof(&graph, a, RelationId::DependsOn);
    assert!(
        result.proofs_complete(),
        "every row needs a resolvable root"
    );
    for row in &result.rows {
        verify_row(&graph, &result, row).unwrap_or_else(|error| {
            panic!("replay failed for {}: {error}", row.to.get());
        });
    }
}

#[test]
fn a_multi_hop_proof_composes_in_from_mid_to_order() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Confirmed);
    let graph = builder.build();

    let result = solve_with_proof(&graph, a, RelationId::DependsOn);
    let row = result.contains(c).expect("c reached");
    let arena = result.proof.as_ref().unwrap();
    let root = row.proof.unwrap();
    assert_eq!(
        arena.get(root).map(|node| node.kind()),
        Some("transitive"),
        "two hops compose transitively"
    );

    // Replay must recover the min of the two hops, and the right full fact.
    let fact = sage_db_infer::proof::replay(&graph, arena, root).unwrap();
    assert_eq!(fact.annotation, Annotation::present(Confidence::Confirmed));
    assert_eq!(
        (fact.from, fact.relation, fact.to),
        (a, RelationId::DependsOn, c)
    );
}

#[test]
fn an_r3_seed_proof_names_the_definition_assertion() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    let edge = builder.edge(a, RelationId::IsA, b, Confidence::Confirmed);
    let graph = builder.build();

    let result = solve_with_proof(&graph, a, RelationId::DependsOn);
    let row = result.contains(b).expect("dependency derived");
    let arena = result.proof.as_ref().unwrap();
    let root = row.proof.unwrap();

    let mut leaves = Vec::new();
    let mut stack = vec![root];
    while let Some(reference) = stack.pop() {
        match arena.get(reference).unwrap() {
            ProofNode::Asserted { assertion_id, .. } => leaves.push(assertion_id),
            ProofNode::Symmetry { premise }
            | ProofNode::DefinitionImpliesDependency { premise } => {
                stack.push(premise);
            }
            ProofNode::Transitive { left, right } => {
                stack.push(left);
                stack.push(right);
            }
        }
    }
    assert_eq!(
        leaves,
        vec![edge],
        "the leaf must be the definition assertion, not an anonymous implied edge"
    );
    verify_row(&graph, &result, row).unwrap();
}

#[test]
fn a_reversed_reading_is_proved_with_a_symmetry_node() {
    // A backwards read is a *reversal*, not a leaf in the wrong direction. The proof
    // must carry a Symmetry node whose premise names the authored assertion, so a
    // consumer replaying it sees the reversal explicitly.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let edge = builder.edge(a, RelationId::AnalogousTo, b, Confidence::Confirmed);
    let graph = builder.build();

    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: b,
        relation: RelationId::AnalogousTo,
        with_proof: true,
        budget: Budget::DEFAULT,
    };
    let result = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap();
    let row = result.contains(a).expect("reverse direction reached");
    let arena = result.proof.as_ref().unwrap();
    let root = row.proof.unwrap();

    // Root is a Symmetry node, not a bare leaf.
    let Some(ProofNode::Symmetry { premise }) = arena.get(root) else {
        panic!("root must be a Symmetry node, got {:?}", arena.get(root));
    };
    // The premise is the authored assertion, in its authored direction.
    assert_eq!(
        arena.get(premise),
        Some(ProofNode::Asserted {
            assertion_id: edge,
            revision: sage_db_types::RecordRevision::FIRST,
        }),
        "the premise must name the authored assertion"
    );

    // And replay recovers the reversed triple, not the authored one.
    let fact = sage_db_infer::proof::replay(&graph, arena, root).unwrap();
    assert_eq!(
        (fact.from, fact.to),
        (b, a),
        "replay yields the reversed reading"
    );
    assert_eq!(fact.relation, RelationId::AnalogousTo);
    verify_row(&graph, &result, row).unwrap();
}

#[test]
fn replay_rejects_a_revision_that_is_not_active() {
    // A proof naming a stale revision must fail replay: the leaf has to identify the
    // revision that is actually active at this snapshot.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge_with_revision(a, RelationId::DependsOn, b, Confidence::Confirmed, 7);
    let graph = builder.build();

    let mut arena = sage_db_types::ProofArena::new();
    let stale = arena
        .push(
            ProofNode::Asserted {
                assertion_id: sage_db_types::AssertionId::new(1),
                revision: sage_db_types::RecordRevision::new(6).unwrap(),
            },
            8,
            u64::MAX,
        )
        .unwrap();
    let error = sage_db_infer::proof::replay(&graph, &arena, stale).unwrap_err();
    assert!(matches!(
        error,
        sage_db_infer::ReplayError::RevisionMismatch { .. }
    ));
}

#[test]
fn replay_rejects_a_missing_assertion() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge_c(a, RelationId::DependsOn, b);
    let graph = builder.build();

    let mut arena = sage_db_types::ProofArena::new();
    let ghost = arena
        .push(
            ProofNode::Asserted {
                assertion_id: sage_db_types::AssertionId::new(999),
                revision: sage_db_types::RecordRevision::FIRST,
            },
            8,
            u64::MAX,
        )
        .unwrap();
    assert!(matches!(
        sage_db_infer::proof::replay(&graph, &arena, ghost).unwrap_err(),
        sage_db_infer::ReplayError::MissingAssertion(_)
    ));
}

#[test]
fn replay_rejects_actually_swapped_transitive_premises() {
    // Build a genuine two-hop proof, then construct a node that composes the same two
    // children in the *opposite* order. This is real tampering with left/right, not a
    // different-confidence substitute: the children do not join, so replay must refuse.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L0);
    let ab = builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    let bc = builder.edge(b, RelationId::DependsOn, c, Confidence::Confirmed);
    let graph = builder.build();

    let mut arena = sage_db_types::ProofArena::new();
    let left = arena
        .push(
            ProofNode::Asserted {
                assertion_id: ab,
                revision: sage_db_types::RecordRevision::FIRST,
            },
            64,
            u64::MAX,
        )
        .unwrap();
    let right = arena
        .push(
            ProofNode::Asserted {
                assertion_id: bc,
                revision: sage_db_types::RecordRevision::FIRST,
            },
            64,
            u64::MAX,
        )
        .unwrap();
    // Correct order joins: a -> b then b -> c.
    let correct = arena
        .push(ProofNode::Transitive { left, right }, 64, u64::MAX)
        .unwrap();
    let recovered = sage_db_infer::proof::replay(&graph, &arena, correct).unwrap();
    assert_eq!((recovered.from, recovered.to), (a, c));

    // Swapped order: b -> c then a -> b. `left.to (c) != right.from (a)`.
    let swapped = arena
        .push(
            ProofNode::Transitive {
                left: right,
                right: left,
            },
            64,
            u64::MAX,
        )
        .unwrap();
    assert_eq!(
        sage_db_infer::proof::replay(&graph, &arena, swapped).unwrap_err(),
        sage_db_infer::ReplayError::Disconnected,
        "a swapped composition must be rejected as disconnected"
    );
}

#[test]
fn replay_rejects_a_wrong_relation_in_a_composition() {
    // Two hops where the second is a different relation: the composition would join
    // across predicates, which this engine does not license.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L0);
    let ab = builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    let bc = builder.edge(b, RelationId::PartOf, c, Confidence::Confirmed);
    let graph = builder.build();

    let mut arena = sage_db_types::ProofArena::new();
    let left = arena
        .push(
            ProofNode::Asserted {
                assertion_id: ab,
                revision: sage_db_types::RecordRevision::FIRST,
            },
            64,
            u64::MAX,
        )
        .unwrap();
    let right = arena
        .push(
            ProofNode::Asserted {
                assertion_id: bc,
                revision: sage_db_types::RecordRevision::FIRST,
            },
            64,
            u64::MAX,
        )
        .unwrap();
    let composed = arena
        .push(ProofNode::Transitive { left, right }, 64, u64::MAX)
        .unwrap();
    assert!(matches!(
        sage_db_infer::proof::replay(&graph, &arena, composed).unwrap_err(),
        sage_db_infer::ReplayError::RelationMismatch { .. }
    ));
}

#[test]
fn verify_row_rejects_a_wrong_endpoint_at_equal_strength() {
    // Same annotation, wrong triple: the proof must not be accepted for another row.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(a, RelationId::DependsOn, c, Confidence::Confirmed);
    let graph = builder.build();

    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: a,
        relation: RelationId::DependsOn,
        with_proof: true,
        budget: Budget::DEFAULT,
    };
    let mut result = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap();

    // Swap the two rows' proof roots: both are Confirmed single hops, so an
    // annotation-only check would accept this.
    assert_eq!(result.rows.len(), 2);
    let first = result.rows[0].proof.expect("root");
    let second = result.rows[1].proof.expect("root");
    result.rows[0].proof = Some(second);
    result.rows[1].proof = Some(first);
    assert!(matches!(
        sage_db_infer::verify_row(&graph, &result, &result.rows[0]).unwrap_err(),
        sage_db_infer::ReplayError::FactMismatch { .. }
    ));
}

#[test]
fn verify_row_rejects_a_foreign_snapshot() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    let graph = builder.build();
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: a,
        relation: RelationId::DependsOn,
        with_proof: true,
        budget: Budget::DEFAULT,
    };
    let mut result = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap();
    result.snapshot.graph_digest =
        sage_db_types::GraphDigest::new(sage_db_types::Digest::from_bytes([0xAB; 32]));
    assert_eq!(
        sage_db_infer::verify_row(&graph, &result, &result.rows[0]).unwrap_err(),
        sage_db_infer::ReplayError::SnapshotMismatch
    );
}

#[test]
fn replay_rejects_symmetry_on_an_asymmetric_relation() {
    // A Symmetry node is only valid for a symmetric predicate under the active
    // profile; wrapping a depends-on leaf in one must be refused.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    let edge = builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    let graph = builder.build();

    let mut arena = sage_db_types::ProofArena::new();
    let leaf = arena
        .push(
            ProofNode::Asserted {
                assertion_id: edge,
                revision: sage_db_types::RecordRevision::FIRST,
            },
            16,
            u64::MAX,
        )
        .unwrap();
    let wrapped = arena
        .push(ProofNode::Symmetry { premise: leaf }, 16, u64::MAX)
        .unwrap();
    assert_eq!(
        sage_db_infer::proof::replay(&graph, &arena, wrapped).unwrap_err(),
        sage_db_infer::ReplayError::RelationsNotSymmetric(RelationId::DependsOn)
    );
}

#[test]
fn replay_rejects_definition_implication_under_legacy_v1() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    let edge = builder.edge(a, RelationId::IsA, b, Confidence::Confirmed);
    let graph = builder.build();

    let mut arena = sage_db_types::ProofArena::new();
    let leaf = arena
        .push(
            ProofNode::Asserted {
                assertion_id: edge,
                revision: sage_db_types::RecordRevision::FIRST,
            },
            16,
            u64::MAX,
        )
        .unwrap();
    let wrapped = arena
        .push(
            ProofNode::DefinitionImpliesDependency { premise: leaf },
            16,
            u64::MAX,
        )
        .unwrap();
    assert_eq!(
        sage_db_infer::proof::replay(&graph, &arena, wrapped).unwrap_err(),
        sage_db_infer::ReplayError::RuleNotActive
    );
}

#[test]
fn an_empty_complete_result_has_no_proof_roots() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let _isolated = builder.term(SemanticLayer::L0);
    let graph = builder.build();

    let result = solve_with_proof(&graph, a, RelationId::DependsOn);
    assert!(result.rows.is_empty());
    assert!(
        result.proofs_complete(),
        "an empty result with an empty arena satisfies the proof contract"
    );
    assert!(result.proof.as_ref().is_some_and(|arena| arena.is_empty()));
}

#[test]
fn proof_budget_exhaustion_is_typed_and_yields_no_result() {
    // Root at L2 so formal-v1's L3-coverage rule is not what fails here.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let d = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Confirmed);
    builder.edge(c, RelationId::DependsOn, d, Confidence::Confirmed);
    let graph = builder.build();

    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: a,
        relation: RelationId::DependsOn,
        with_proof: true,
        budget: Budget {
            max_proof_nodes: 1,
            ..Budget::DEFAULT
        },
    };
    let error = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap_err();
    assert!(
        matches!(
            error,
            sage_db_types::QueryError::BudgetExceeded(BudgetLimit::ProofNodes)
        ),
        "a proof that cannot be completed must error, not return partial proofs: {error:?}"
    );
}

#[test]
fn work_bytes_stop_the_query_before_it_can_exceed_the_budget() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    for _ in 0..4 {
        let t = builder.term(SemanticLayer::L1);
        builder.edge(a, RelationId::DependsOn, t, Confidence::Confirmed);
    }
    let graph = builder.build();
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source: a,
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget {
            max_work_bytes: 64,
            ..Budget::DEFAULT
        },
    };
    let error = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap_err();
    assert_eq!(
        error,
        sage_db_types::QueryError::BudgetExceeded(BudgetLimit::WorkBytes)
    );
}

#[test]
fn a_proof_root_is_never_dangling() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Confirmed);
    let graph = builder.build();

    let result = solve_with_proof(&graph, a, RelationId::DependsOn);
    let arena = result.proof.as_ref().unwrap();
    for row in &result.rows {
        let root: ProofRef = row.proof.expect("root");
        assert!(arena.get(root).is_some());
        assert!(root.get() < arena.len() as u32);
    }
}

#[test]
fn stats_report_work_that_was_actually_charged() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    let graph = builder.build();

    let result = solve_with_proof(&graph, a, RelationId::DependsOn);
    assert!(result.stats.steps > 0, "steps are charged");
    assert!(result.stats.work_rows > 0, "work rows are charged");
    assert!(result.stats.work_bytes > 0, "bytes are charged");
    assert!(result.stats.proof_nodes > 0, "proof nodes are charged");
}
