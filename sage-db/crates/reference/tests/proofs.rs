//! Proof selection, independent replay, and the negative controls that make replay
//! meaningful.
//!
//! Replay is checked here against the *graph*, never against the reference's own
//! table, so these tests fail if selection and replay ever share a mistake. The
//! negative controls forge a proof and require replay to reject it; a replay that
//! accepted every proof would pass the positive tests and fail these.

mod support;

use sage_db_reference::{Expected, ReplayError, proof};
use sage_db_types::{
    Annotation, Confidence, ProofArena, ProofNode, ProofRef, RelationId, RuleProfile,
    SemanticLayer, TermId,
};
use support::Builder;

fn stacked_assertion(node: Option<ProofNode>) -> Option<sage_db_types::AssertionId> {
    match node {
        Some(ProofNode::Asserted { assertion_id, .. }) => Some(assertion_id),
        _ => None,
    }
}

fn verify(
    graph: &sage_db_types::BaseGraph,
    result: &sage_db_types::SourceResult,
    to: TermId,
    annotation: Annotation,
) {
    let arena = result.proof.as_ref().expect("arena");
    let row = sage_db_reference::row_for(result, to).expect("row");
    let root = row.proof.expect("root");
    proof::replay_expecting(
        graph,
        graph.rules(),
        arena,
        root,
        Expected {
            from: row.from,
            relation: row.relation,
            to: row.to,
            annotation,
        },
    )
    .unwrap_or_else(|error| panic!("replay failed for {to:?}: {error}"));
}

#[test]
fn every_row_carries_a_replayable_strongest_proof() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let d = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Hypothetical);
    builder.edge(a, RelationId::DependsOn, d, Confidence::Confirmed);
    builder.edge(d, RelationId::DependsOn, c, Confidence::Confirmed);
    builder.edge_c(a, RelationId::DependsOn, c);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, true);
    assert!(
        result.proofs_complete(),
        "every row needs a resolvable root"
    );

    for row in &result.rows {
        verify(&graph, &result, row.to, row.annotation);
    }

    // c has a direct authored fact and a 2-hop route; the proof must be for the
    // reported (confirmed) annotation either way.
    assert_eq!(
        sage_db_reference::annotation_of(&result, c),
        Annotation::present(Confidence::Confirmed)
    );
}

#[test]
fn a_proof_request_with_no_facts_yields_no_arena_and_no_roots() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let _b = builder.term(SemanticLayer::L1);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, true);
    assert!(result.rows.is_empty());
    let arena = result.proof.as_ref().expect("an arena is still returned");
    assert!(arena.is_empty(), "an empty result must carry no roots");
    assert!(result.proofs_complete());
}

#[test]
fn without_proof_neither_roots_nor_an_arena_are_produced() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge_c(a, RelationId::DependsOn, b);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);
    assert!(!result.rows.is_empty());
    assert!(result.proof.is_none(), "no arena was requested");
    assert!(result.rows.iter().all(|row| row.proof.is_none()));
    assert!(!result.proofs_complete());
}

#[test]
fn a_symmetry_proof_replays_through_the_swap() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L2);
    builder.edge(a, RelationId::Contradicts, b, Confidence::Hypothetical);
    let graph = builder.build();

    let result = support::solve(&graph, b, RelationId::Contradicts, true);
    let arena = result.proof.as_ref().expect("arena");
    let row = sage_db_reference::row_for(&result, a).expect("mirror row");
    let root = row.proof.expect("root");

    // The root is a Symmetry node over the authored direction.
    assert!(matches!(arena.get(root), Some(ProofNode::Symmetry { .. })));
    verify(
        &graph,
        &result,
        a,
        Annotation::present(Confidence::Hypothetical),
    );

    // The root proves `a -contradicts-> b`, not the authored `b -> a`. Requiring the
    // authored direction of a mirror proof must be rejected: a symmetry proof may
    // never be presented as authorship.
    let error = proof::replay(
        &graph,
        graph.rules(),
        arena,
        root,
        a,
        RelationId::Contradicts,
        b,
    )
    .unwrap_err();
    assert_eq!(error, ReplayError::FactMismatch);
}

#[test]
fn an_r3_proof_replays_and_names_the_definition_assertion() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let definition = builder.edge_c(a, RelationId::IsA, b);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, true);
    let arena = result.proof.as_ref().expect("arena");
    let row = sage_db_reference::row_for(&result, b).expect("row");
    let root = row.proof.expect("root");

    assert!(matches!(
        arena.get(root),
        Some(ProofNode::DefinitionImpliesDependency { .. })
    ));
    // The arena names the concrete definition assertion, not an anonymous edge.
    assert!(arena.nodes().iter().any(|node| matches!(
        node,
        ProofNode::Asserted { assertion_id, .. } if *assertion_id == definition
    )));
    verify(
        &graph,
        &result,
        b,
        Annotation::present(Confidence::Confirmed),
    );
}

#[test]
fn a_transitive_proof_keeps_from_mid_to_order() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let ab = builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    let bc = builder.edge(b, RelationId::DependsOn, c, Confidence::Hypothetical);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, true);
    let arena = result.proof.as_ref().expect("arena");
    let row = sage_db_reference::row_for(&result, c).expect("row");
    let root = row.proof.expect("root");

    let ProofNode::Transitive { left, right } = arena.get(root).expect("root node") else {
        panic!("expected a transitive root, got {:?}", arena.get(root));
    };
    // left proves a -> b, right proves b -> c.
    assert_eq!(stacked_assertion(arena.get(left)), Some(ab));
    assert_eq!(stacked_assertion(arena.get(right)), Some(bc));
    verify(
        &graph,
        &result,
        c,
        Annotation::present(Confidence::Hypothetical),
    );
}

#[test]
fn a_three_hop_proof_nests_with_the_far_half_on_the_right() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let d = builder.term(SemanticLayer::L1);
    builder.edge_c(a, RelationId::DependsOn, b);
    builder.edge_c(b, RelationId::DependsOn, c);
    builder.edge_c(c, RelationId::DependsOn, d);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, true);
    let arena = result.proof.as_ref().expect("arena");
    let root = sage_db_reference::row_for(&result, d)
        .expect("row")
        .proof
        .expect("root");
    // Root is (a->b, b->d); the right half is itself a composition.
    let ProofNode::Transitive { right, .. } = arena.get(root).expect("node") else {
        panic!("expected a transitive root");
    };
    assert!(matches!(
        arena.get(right),
        Some(ProofNode::Transitive { .. })
    ));
    verify(
        &graph,
        &result,
        d,
        Annotation::present(Confidence::Confirmed),
    );
}

// ---------------------------------------------------------------------------
// Negative controls: a replay that accepted these would be worthless
// ---------------------------------------------------------------------------

fn arena_of(nodes: &[ProofNode]) -> (ProofArena, Vec<ProofRef>) {
    let mut arena = ProofArena::new();
    let mut refs = Vec::new();
    for node in nodes {
        refs.push(arena.push(*node, u32::MAX, u64::MAX).expect("push"));
    }
    (arena, refs)
}

#[test]
fn replay_rejects_a_wrong_endpoint_even_at_the_right_confidence() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let ab = builder.edge_c(a, RelationId::DependsOn, b);
    // A separate confirmed fact b -> c exists, but no a -> c.
    builder.edge_c(b, RelationId::DependsOn, c);
    let graph = builder.build();

    // A single confirmed leaf is enough to reach "confirmed"; claiming it proves
    // a -> c must still fail, because the endpoints come from the assertion.
    let (arena, refs) = arena_of(&[ProofNode::Asserted {
        assertion_id: ab,
        revision: sage_db_types::RecordRevision::FIRST,
    }]);
    let error = proof::replay(
        &graph,
        graph.rules(),
        &arena,
        refs[0],
        a,
        RelationId::DependsOn,
        c,
    )
    .unwrap_err();
    assert_eq!(error, ReplayError::FactMismatch);
}

#[test]
fn replay_rejects_wrong_relation_even_at_the_right_endpoints() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let id = builder.edge_c(a, RelationId::IsA, b);
    let graph = builder.build();

    let (arena, refs) = arena_of(&[ProofNode::Asserted {
        assertion_id: id,
        revision: sage_db_types::RecordRevision::FIRST,
    }]);
    let error = proof::replay(
        &graph,
        graph.rules(),
        &arena,
        refs[0],
        a,
        RelationId::DependsOn,
        b,
    )
    .unwrap_err();
    assert_eq!(error, ReplayError::FactMismatch);
}

#[test]
fn replay_rejects_swapped_transitive_operands() {
    // a -> b -> c. The correct composition is (a->b, b->c). Swapping gives
    // (b->c, a->b), whose join does not hold, so replay must refuse.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let ab = builder.edge_c(a, RelationId::DependsOn, b);
    let bc = builder.edge_c(b, RelationId::DependsOn, c);
    let graph = builder.build();

    let revision = sage_db_types::RecordRevision::FIRST;
    let (correct, correct_refs) = arena_of(&[
        ProofNode::Asserted {
            assertion_id: ab,
            revision,
        },
        ProofNode::Asserted {
            assertion_id: bc,
            revision,
        },
        ProofNode::Transitive {
            left: ProofRef::new(0),
            right: ProofRef::new(1),
        },
    ]);
    proof::replay(
        &graph,
        graph.rules(),
        &correct,
        correct_refs[2],
        a,
        RelationId::DependsOn,
        c,
    )
    .expect("the correctly ordered composition must replay");

    let (swapped, swapped_refs) = arena_of(&[
        ProofNode::Asserted {
            assertion_id: ab,
            revision,
        },
        ProofNode::Asserted {
            assertion_id: bc,
            revision,
        },
        ProofNode::Transitive {
            left: ProofRef::new(1),
            right: ProofRef::new(0),
        },
    ]);
    let error = proof::replay(
        &graph,
        graph.rules(),
        &swapped,
        swapped_refs[2],
        a,
        RelationId::DependsOn,
        c,
    )
    .unwrap_err();
    assert_eq!(error, ReplayError::Disconnected);
}

#[test]
fn replay_rejects_a_stale_revision_leaf() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let id = builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Confirmed, 3);
    let graph = builder.build();

    let stale = sage_db_types::RecordRevision::new(2).unwrap();
    let (arena, refs) = arena_of(&[ProofNode::Asserted {
        assertion_id: id,
        revision: stale,
    }]);
    let error = proof::replay(
        &graph,
        graph.rules(),
        &arena,
        refs[0],
        a,
        RelationId::DependsOn,
        b,
    )
    .unwrap_err();
    assert_eq!(
        error,
        ReplayError::RevisionMismatch {
            assertion: id,
            claimed: stale,
            actual: sage_db_types::RecordRevision::new(3).unwrap(),
        }
    );
}

#[test]
fn replay_rejects_an_assertion_that_is_not_active() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let id = builder.edge_c(a, RelationId::DependsOn, b);
    builder.retract(id);
    let graph = builder.build();

    let (arena, refs) = arena_of(&[ProofNode::Asserted {
        assertion_id: id,
        revision: sage_db_types::RecordRevision::FIRST,
    }]);
    let error = proof::replay(
        &graph,
        graph.rules(),
        &arena,
        refs[0],
        a,
        RelationId::DependsOn,
        b,
    )
    .unwrap_err();
    assert_eq!(error, ReplayError::MissingAssertion(id));
}

#[test]
fn replay_rejects_symmetry_under_a_profile_where_the_relation_is_not_symmetric() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let id = builder.edge_c(a, RelationId::DependsOn, b);
    let graph = builder.build();

    let revision = sage_db_types::RecordRevision::FIRST;
    let (arena, refs) = arena_of(&[
        ProofNode::Asserted {
            assertion_id: id,
            revision,
        },
        ProofNode::Symmetry {
            premise: ProofRef::new(0),
        },
    ]);
    let error = proof::replay(
        &graph,
        graph.rules(),
        &arena,
        refs[1],
        b,
        RelationId::DependsOn,
        a,
    )
    .unwrap_err();
    assert_eq!(
        error,
        ReplayError::RuleNotApplicable {
            relation: RelationId::DependsOn
        }
    );
}

#[test]
fn replay_rejects_a_double_symmetry_round_trip() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L2);
    let id = builder.edge_c(a, RelationId::Contradicts, b);
    let graph = builder.build();

    let revision = sage_db_types::RecordRevision::FIRST;
    let (arena, refs) = arena_of(&[
        ProofNode::Asserted {
            assertion_id: id,
            revision,
        },
        ProofNode::Symmetry {
            premise: ProofRef::new(0),
        },
        ProofNode::Symmetry {
            premise: ProofRef::new(1),
        },
    ]);
    let error = proof::replay(
        &graph,
        graph.rules(),
        &arena,
        refs[2],
        a,
        RelationId::Contradicts,
        b,
    )
    .unwrap_err();
    assert_eq!(error, ReplayError::CircularRuleApplication);
}

#[test]
fn replay_rejects_r3_under_legacy_where_no_dependency_is_implied() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let id = builder.edge_c(a, RelationId::IsA, b);
    let graph = builder.build();

    let revision = sage_db_types::RecordRevision::FIRST;
    let (arena, refs) = arena_of(&[
        ProofNode::Asserted {
            assertion_id: id,
            revision,
        },
        ProofNode::DefinitionImpliesDependency {
            premise: ProofRef::new(0),
        },
    ]);
    let error = proof::replay(
        &graph,
        graph.rules(),
        &arena,
        refs[1],
        a,
        RelationId::DependsOn,
        b,
    )
    .unwrap_err();
    assert_eq!(
        error,
        ReplayError::RuleNotApplicable {
            relation: RelationId::IsA
        }
    );
}

#[test]
fn replay_rejects_an_annotation_that_does_not_match_the_row() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let id = builder.edge(a, RelationId::DependsOn, b, Confidence::Hypothetical);
    let graph = builder.build();

    let (arena, refs) = arena_of(&[ProofNode::Asserted {
        assertion_id: id,
        revision: sage_db_types::RecordRevision::FIRST,
    }]);
    assert_eq!(
        proof::replay_expecting(
            &graph,
            graph.rules(),
            &arena,
            refs[0],
            Expected {
                from: a,
                relation: RelationId::DependsOn,
                to: b,
                annotation: Annotation::present(Confidence::Hypothetical),
            },
        ),
        Ok(())
    );
    assert_eq!(
        proof::replay_expecting(
            &graph,
            graph.rules(),
            &arena,
            refs[0],
            Expected {
                from: a,
                relation: RelationId::DependsOn,
                to: b,
                annotation: Annotation::present(Confidence::Confirmed),
            },
        ),
        Err(ReplayError::AnnotationMismatch)
    );
}

#[test]
fn proofs_are_identical_across_repeated_evaluation_and_input_permutation() {
    let build = |reverse: bool| {
        let mut builder = Builder::new(RuleProfile::LegacyV1);
        let a = builder.term(SemanticLayer::L1);
        let b = builder.term(SemanticLayer::L1);
        let c = builder.term(SemanticLayer::L1);
        builder.edge_c(a, RelationId::DependsOn, b);
        builder.edge_c(b, RelationId::DependsOn, c);
        builder.edge_c(a, RelationId::DependsOn, c);
        if reverse {
            builder.reverse_assertions();
        }
        builder.build()
    };

    let first = build(false);
    let second = build(false);
    let reversed = build(true);

    let canonical = |graph: &sage_db_types::BaseGraph| {
        let result = support::solve(graph, TermId::new(0), RelationId::DependsOn, true);
        let arena = result.proof.as_ref().expect("arena");
        let nodes: Vec<ProofNode> = arena.nodes().to_vec();
        (result.canonical_rows(), nodes)
    };

    assert_eq!(canonical(&first), canonical(&second));
    assert_eq!(
        canonical(&first),
        canonical(&reversed),
        "assertion order must not change the selected proofs"
    );
}

// ---------------------------------------------------------------------------
// Shortest-proof selection: a weak short prefix must not be lost to a strong
// long route to the same midpoint.
// ---------------------------------------------------------------------------

/// Hop count of a proof: authored leaves along the derivation.
fn proof_hops(arena: &ProofArena, root: ProofRef) -> u32 {
    match arena.get(root).expect("node") {
        ProofNode::Asserted { .. } => 1,
        ProofNode::Symmetry { premise } | ProofNode::DefinitionImpliesDependency { premise } => {
            proof_hops(arena, premise)
        }
        ProofNode::Transitive { left, right } => proof_hops(arena, left) + proof_hops(arena, right),
    }
}

fn hops_of(
    builder: &Builder,
    source: TermId,
    to: TermId,
    relation: RelationId,
) -> (Annotation, u32, usize) {
    let graph = builder.build();
    let result = support::solve(&graph, source, relation, true);
    let arena = result.proof.as_ref().expect("arena");
    let row = sage_db_reference::row_for(&result, to).expect("row");
    (
        row.annotation,
        proof_hops(arena, row.proof.expect("root")),
        arena.len(),
    )
}

/// The reported counterexample, in the coordinator's term order `A0 X1 Y2 B3 T4`.
///
/// `A -h-> B`; `A -c-> X -c-> Y -c-> B`; `B -h-> T`. `B` is confirmed by the 3-hop
/// route, but `T` must be the **2-hop** `A -h-> B -h-> T`: composing `T` from the
/// strong 3-hop route to `B` would give `min(c, h) = h` at 4 hops, which is the same
/// strength and strictly longer. A selection that keeps one winner per fact picks
/// the 4-hop proof, and which one it picks depends on term-id order.
#[test]
fn a_weak_short_prefix_is_not_lost_to_a_strong_long_route() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L2); // 0
    let x = builder.term(SemanticLayer::L1); // 1
    let y = builder.term(SemanticLayer::L1); // 2
    let b = builder.term(SemanticLayer::L1); // 3
    let t = builder.term(SemanticLayer::L0); // 4
    builder.edge(a, RelationId::DependsOn, b, Confidence::Hypothetical);
    builder.edge(a, RelationId::DependsOn, x, Confidence::Confirmed);
    builder.edge(x, RelationId::DependsOn, y, Confidence::Confirmed);
    builder.edge(y, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, t, Confidence::Hypothetical);

    let (b_annotation, b_hops, _) = hops_of(&builder, a, b, RelationId::DependsOn);
    assert_eq!(b_annotation, Annotation::present(Confidence::Confirmed));
    assert_eq!(b_hops, 3, "B must be proven by the 3-hop confirmed route");

    let (t_annotation, t_hops, _) = hops_of(&builder, a, t, RelationId::DependsOn);
    assert_eq!(
        t_annotation,
        Annotation::present(Confidence::Hypothetical),
        "T is hypothetical: the confirmed prefix does not survive the hypothetical suffix"
    );
    assert_eq!(
        t_hops, 2,
        "T must be the 2-hop A->B->T route, not the 4-hop A->X->Y->B->T"
    );
}

/// The same graph with `B` before `X`/`Y` in term-id order.
///
/// This ordering masked the defect in the first delivery: with the weak short prefix
/// sorting first, the strong route to `B` never had to be composed for `T`.
#[test]
fn the_shortest_proof_does_not_depend_on_term_id_order() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L2); // 0
    let b = builder.term(SemanticLayer::L1); // 1  <- before X and Y
    let x = builder.term(SemanticLayer::L1); // 2
    let y = builder.term(SemanticLayer::L1); // 3
    let t = builder.term(SemanticLayer::L0); // 4
    builder.edge(a, RelationId::DependsOn, b, Confidence::Hypothetical);
    builder.edge(a, RelationId::DependsOn, x, Confidence::Confirmed);
    builder.edge(x, RelationId::DependsOn, y, Confidence::Confirmed);
    builder.edge(y, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, t, Confidence::Hypothetical);

    let (_, b_hops, _) = hops_of(&builder, a, b, RelationId::DependsOn);
    assert_eq!(b_hops, 3);
    let (t_annotation, t_hops, _) = hops_of(&builder, a, t, RelationId::DependsOn);
    assert_eq!(t_annotation, Annotation::present(Confidence::Hypothetical));
    assert_eq!(
        t_hops, 2,
        "the answer must not depend on which prefix sorts first"
    );
}

/// Assertion order is not term order: permuting the records must not change the
/// selected hop count either.
#[test]
fn the_shortest_proof_survives_assertion_permutation() {
    let build = |reverse: bool| {
        let mut builder = Builder::new(RuleProfile::LegacyV1);
        let a = builder.term(SemanticLayer::L2);
        let x = builder.term(SemanticLayer::L1);
        let y = builder.term(SemanticLayer::L1);
        let b = builder.term(SemanticLayer::L1);
        let t = builder.term(SemanticLayer::L0);
        builder.edge(a, RelationId::DependsOn, b, Confidence::Hypothetical);
        builder.edge(a, RelationId::DependsOn, x, Confidence::Confirmed);
        builder.edge(x, RelationId::DependsOn, y, Confidence::Confirmed);
        builder.edge(y, RelationId::DependsOn, b, Confidence::Confirmed);
        builder.edge(b, RelationId::DependsOn, t, Confidence::Hypothetical);
        if reverse {
            builder.reverse_assertions();
        }
        builder
    };

    for reverse in [false, true] {
        let builder = build(reverse);
        let (t_annotation, t_hops, _) = hops_of(
            &builder,
            TermId::new(0),
            TermId::new(4),
            RelationId::DependsOn,
        );
        assert_eq!(
            t_annotation,
            Annotation::present(Confidence::Hypothetical),
            "reverse={reverse}"
        );
        assert_eq!(
            t_hops, 2,
            "reverse={reverse}: hop count must be order-independent"
        );
    }
}

/// A dominated candidate must not be stored: the kept set is one shortest route per
/// confidence level, so a strictly longer route at the same level must lose.
#[test]
fn only_the_shortest_route_per_level_is_kept() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let d = builder.term(SemanticLayer::L1);
    // Direct 1-hop confirmed, plus a 3-hop confirmed route to the same target.
    builder.edge(a, RelationId::DependsOn, d, Confidence::Confirmed);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Confirmed);
    builder.edge(c, RelationId::DependsOn, d, Confidence::Confirmed);

    let (annotation, hops, arena_nodes) = hops_of(&builder, a, d, RelationId::DependsOn);
    assert_eq!(annotation, Annotation::present(Confidence::Confirmed));
    assert_eq!(
        hops, 1,
        "the direct route must be selected over the longer one"
    );
    assert!(
        arena_nodes <= 4,
        "a dominated longer route must not be materialised: {arena_nodes} nodes"
    );
}
