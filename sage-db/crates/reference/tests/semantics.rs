//! Expected reference semantics, stated as properties with explicit counterexamples.
//!
//! These tests assert what the accepted contract *requires*, not merely that the
//! implementation agrees with itself. Each one names the failure it is designed to
//! catch.

mod support;

use sage_db_types::{Annotation, Confidence, RelationId, RuleProfile, SemanticLayer, TermId};
use support::Builder;

fn annotation(result: &sage_db_types::SourceResult, to: TermId) -> Annotation {
    sage_db_reference::annotation_of(result, to)
}

fn row(result: &sage_db_types::SourceResult, to: TermId) -> &sage_db_types::FactRow {
    sage_db_reference::row_for(result, to).expect("row must exist")
}

// ---------------------------------------------------------------------------
// Absence, contested, and the lattice
// ---------------------------------------------------------------------------

#[test]
fn absence_is_a_different_element_from_contested() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Contested);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);

    // Present and disputed.
    assert_eq!(
        annotation(&result, b),
        Annotation::present(Confidence::Contested)
    );
    assert!(row(&result, b).disputed);
    // Genuinely absent, not "contested at zero".
    assert_eq!(annotation(&result, c), Annotation::ABSENT);
    assert!(sage_db_reference::row_for(&result, c).is_none());
    // The empty-capable result is still a complete answer.
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn a_chain_takes_the_minimum_and_alternatives_take_the_maximum() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let d = builder.term(SemanticLayer::L1);
    // a -> b -> c: min(confirmed, hypothetical) = hypothetical.
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Hypothetical);
    // a -> d -> c: min(hypothetical, confirmed) = hypothetical.
    builder.edge(a, RelationId::DependsOn, d, Confidence::Hypothetical);
    builder.edge(d, RelationId::DependsOn, c, Confidence::Confirmed);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);
    assert_eq!(
        annotation(&result, c),
        Annotation::present(Confidence::Hypothetical)
    );

    // Now a third route stronger than both: max picks it up.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Contested);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Contested);
    builder.edge(a, RelationId::DependsOn, c, Confidence::Confirmed);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);
    assert_eq!(
        annotation(&result, c),
        Annotation::present(Confidence::Confirmed)
    );
    // `max` must not erase the objection.
    assert!(
        row(&result, c).disputed,
        "a confirmed fact can still be disputed"
    );
}

#[test]
fn a_short_weak_route_found_first_loses_to_a_later_stronger_route() {
    // Fixpoint order is by key, so `a -> m -> t` (2 hops, contested) is derived
    // before `a -> b -> c -> t` (3 hops, confirmed) only if the keys happen to sort
    // that way. Either way the annotation must end at the stronger one.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let m = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let t = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, m, Confidence::Contested);
    builder.edge(m, RelationId::DependsOn, t, Confidence::Contested);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Confirmed);
    builder.edge(c, RelationId::DependsOn, t, Confidence::Confirmed);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);
    assert_eq!(
        annotation(&result, t),
        Annotation::present(Confidence::Confirmed),
        "the stronger 3-hop route must win over the short contested one"
    );
    assert!(
        row(&result, t).disputed,
        "the contested route still counts as an objection"
    );
}

#[test]
fn a_short_weak_route_found_first_loses_even_when_it_sorts_first() {
    // Same property, with ids chosen so the weak path is *earlier* in both the
    // table order and the hop order: t is reachable in 2 hops weakly and 3 hops
    // strongly, and the weak midpoint is term 1.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let weak = builder.term(SemanticLayer::L1);
    let strong1 = builder.term(SemanticLayer::L1);
    let strong2 = builder.term(SemanticLayer::L1);
    let t = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, weak, Confidence::Hypothetical);
    builder.edge(weak, RelationId::DependsOn, t, Confidence::Hypothetical);
    builder.edge(a, RelationId::DependsOn, strong1, Confidence::Confirmed);
    builder.edge(
        strong1,
        RelationId::DependsOn,
        strong2,
        Confidence::Confirmed,
    );
    builder.edge(strong2, RelationId::DependsOn, t, Confidence::Confirmed);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);
    assert_eq!(
        annotation(&result, t),
        Annotation::present(Confidence::Confirmed)
    );
}

// ---------------------------------------------------------------------------
// Direction and symmetry
// ---------------------------------------------------------------------------

#[test]
fn an_authored_direction_is_kept_and_the_mirror_is_derived_only() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L2);
    builder.edge(a, RelationId::Contradicts, b, Confidence::Confirmed);
    let graph = builder.build();

    let forward = support::solve(&graph, a, RelationId::Contradicts, false);
    let mirrored = support::solve(&graph, b, RelationId::Contradicts, false);

    let forward_row = row(&forward, b);
    assert_eq!(forward_row.from, a);
    assert!(
        forward_row.has_asserted,
        "the authored direction is asserted"
    );
    assert!(
        !forward_row.has_derived,
        "authoring alone is not a rule derivation"
    );

    let mirror_row = row(&mirrored, a);
    assert_eq!(mirror_row.from, b);
    assert!(!mirror_row.has_asserted, "R2 must not claim authorship");
    assert!(mirror_row.has_derived, "one R2 application is a derivation");
    assert!(
        mirror_row.strongest_support.is_none(),
        "no authored support in this direction"
    );
}

#[test]
fn two_opposite_authored_supports_aggregate_to_the_maximum_and_both_are_asserted() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L2);
    builder.edge(a, RelationId::AnalogousTo, b, Confidence::Hypothetical);
    builder.edge(b, RelationId::AnalogousTo, a, Confidence::Confirmed);
    let graph = builder.build();

    let forward = support::solve(&graph, a, RelationId::AnalogousTo, false);
    let backward = support::solve(&graph, b, RelationId::AnalogousTo, false);

    // Each direction is authored in its own right, so both are asserted. The
    // annotation is the max over *all* supports, and the confirmed opposite
    // direction mirrors in at full strength, so both directions report confirmed.
    let forward_row = row(&forward, b);
    assert_eq!(
        forward_row.annotation,
        Annotation::present(Confidence::Confirmed)
    );
    assert!(forward_row.has_asserted, "a authored a -> b itself");
    assert!(
        forward_row.has_derived,
        "the confirmed opposite direction also mirrors in"
    );

    let backward_row = row(&backward, a);
    assert_eq!(
        backward_row.annotation,
        Annotation::present(Confidence::Confirmed)
    );
    assert!(backward_row.has_asserted);
    assert!(backward_row.has_derived);

    // The distinct property: removing the weaker authored support must not move the
    // annotated strength, because the opposite confirmed support still mirrors in.
    // But the authored witness then disappears from the forward direction.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a2 = builder.term(SemanticLayer::L2);
    let b2 = builder.term(SemanticLayer::L2);
    builder.edge_c(b2, RelationId::AnalogousTo, a2);
    let only_opposite = support::solve(&builder.build(), a2, RelationId::AnalogousTo, false);
    let mirror_only = row(&only_opposite, b2);
    assert_eq!(
        mirror_only.annotation,
        Annotation::present(Confidence::Confirmed)
    );
    assert!(!mirror_only.has_asserted);
    assert!(mirror_only.has_derived);
}

#[test]
fn a_symmetry_round_trip_is_not_a_second_source() {
    // One authored contradiction, confirmed. The mirrored direction is derived, but
    // the *authored* direction must not become derived by mirroring the mirror.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L2);
    builder.edge(a, RelationId::Contradicts, b, Confidence::Confirmed);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::Contradicts, false);
    assert!(
        !row(&result, b).has_derived,
        "R2(R2(a)) returns to the authored direction and is not a new source"
    );
}

// ---------------------------------------------------------------------------
// Rule profiles
// ---------------------------------------------------------------------------

#[test]
fn legacy_does_not_imply_dependency_from_definition_relations() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let instance = builder.term(SemanticLayer::L2);
    let parent = builder.term(SemanticLayer::L1);
    builder.edge_c(instance, RelationId::IsA, parent);
    let graph = builder.build();

    let dependencies = support::solve(&graph, instance, RelationId::DependsOn, false);
    assert!(
        dependencies.rows.is_empty(),
        "legacy-v1 has no R3 seed: {:?}",
        dependencies.rows
    );

    // The definition relation itself still answers.
    let definitions = support::solve(&graph, instance, RelationId::IsA, false);
    assert_eq!(
        annotation(&definitions, parent),
        Annotation::present(Confidence::Confirmed)
    );
}

#[test]
fn formal_seeds_dependency_from_every_active_definition_assertion() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L0);
    // Two different definition relations, both seeding depends-on.
    builder.edge_c(a, RelationId::IsA, b);
    builder.edge_c(a, RelationId::PartOf, c);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);
    assert_eq!(
        annotation(&result, b),
        Annotation::present(Confidence::Confirmed)
    );
    assert_eq!(
        annotation(&result, c),
        Annotation::present(Confidence::Confirmed)
    );
    // Each seed is derived from a definition assertion, never asserted as a dependency.
    assert!(row(&result, b).has_derived);
    assert!(!row(&result, b).has_asserted);
}

#[test]
fn formal_dependency_seeds_close_transitively_with_authored_dependencies() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L0);
    builder.edge_c(a, RelationId::IsA, b);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Confirmed);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);
    // a -is-a-> b seeds a -depends-> b; b -depends-> c closes to a -depends-> c.
    assert_eq!(
        annotation(&result, b),
        Annotation::present(Confidence::Confirmed)
    );
    assert_eq!(
        annotation(&result, c),
        Annotation::present(Confidence::Confirmed)
    );
    assert!(row(&result, c).has_derived);
}

#[test]
fn instantiates_never_composes_with_itself() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let instance = builder.term(SemanticLayer::L3);
    let kind = builder.term(SemanticLayer::L2);
    let super_kind = builder.term(SemanticLayer::L1);
    builder.edge_c(instance, RelationId::Instantiates, kind);
    builder.edge_c(kind, RelationId::Instantiates, super_kind);
    let graph = builder.build();

    let result = support::solve(&graph, instance, RelationId::Instantiates, false);
    assert_eq!(
        annotation(&result, kind),
        Annotation::present(Confidence::Confirmed)
    );
    assert_eq!(
        annotation(&result, super_kind),
        Annotation::ABSENT,
        "instantiates is a definition edge but not transitive"
    );
}

#[test]
fn a_non_transitive_non_symmetric_relation_still_answers_its_authored_facts() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    builder.edge_c(a, RelationId::Enables, b);
    builder.edge_c(b, RelationId::Enables, c);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::Enables, false);
    assert_eq!(
        annotation(&result, b),
        Annotation::present(Confidence::Confirmed)
    );
    assert_eq!(
        annotation(&result, c),
        Annotation::ABSENT,
        "enables is navigation, not a transitive closure"
    );
    assert!(row(&result, b).has_asserted);
    assert!(!row(&result, b).has_derived);
}

// ---------------------------------------------------------------------------
// Supports, retraction, duplicates
// ---------------------------------------------------------------------------

#[test]
fn duplicate_endpoint_triples_keep_independent_supports() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let strong = builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    let weak = builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Hypothetical, 2);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);
    let only = row(&result, b);
    assert_eq!(only.annotation, Annotation::present(Confidence::Confirmed));
    assert_eq!(result.rows.len(), 1, "one fact, not one row per support");

    // The strongest witness is the confirmed support, not the later revision.
    let witness = only.strongest_support.expect("witness");
    assert_eq!(witness.id, strong);
    assert_eq!(witness.revision.get(), 1);

    // Retracting the strong support downgrades rather than deleting.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Hypothetical, 2);
    builder.retract(strong);
    let graph = builder.build();

    let downgraded = support::solve(&graph, a, RelationId::DependsOn, false);
    let downgraded_row = row(&downgraded, b);
    assert_eq!(
        downgraded_row.annotation,
        Annotation::present(Confidence::Hypothetical)
    );
    assert_eq!(downgraded_row.strongest_support.unwrap().id, weak);

    // Retracting the last support makes the fact absent.
    builder.retract(weak);
    let graph = builder.build();
    let absent = support::solve(&graph, a, RelationId::DependsOn, false);
    assert!(absent.rows.is_empty());
}

#[test]
fn a_direct_assertion_survives_retraction_of_the_indirect_route() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    // Direct shortcut a -> c, plus the indirect route a -> b -> c.
    builder.edge_c(a, RelationId::DependsOn, c);
    builder.edge_c(a, RelationId::DependsOn, b);
    builder.edge_c(b, RelationId::DependsOn, c);

    let with_route = support::solve(&builder.build(), a, RelationId::DependsOn, false);
    assert!(row(&with_route, c).has_asserted);
    assert!(
        row(&with_route, c).has_derived,
        "the indirect route also reaches c"
    );

    builder.retract_triple(b, RelationId::DependsOn, c);
    let after = support::solve(&builder.build(), a, RelationId::DependsOn, false);
    let surviving = row(&after, c);
    assert_eq!(
        surviving.annotation,
        Annotation::present(Confidence::Confirmed)
    );
    assert!(surviving.has_asserted);
    assert!(
        !surviving.has_derived,
        "the only remaining support is the authored one"
    );
}

#[test]
fn a_weaker_contested_route_arriving_late_still_sets_disputed() {
    // Regression: the strong fact exists first and is already the maximum, so the
    // later contested route improves no annotation. `disputed` must still rise, and
    // must propagate to facts derived through it.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let downstream = builder.term(SemanticLayer::L1);
    // `a -> b` is confirmed by one support and contested by a second, so the
    // contested route cannot move the annotation — it can only raise `disputed`.
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Contested, 2);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Confirmed);
    builder.edge(c, RelationId::DependsOn, downstream, Confidence::Confirmed);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);
    assert_eq!(
        annotation(&result, b),
        Annotation::present(Confidence::Confirmed)
    );
    assert!(
        row(&result, b).disputed,
        "the duplicate contested support still disputes b"
    );
    assert_eq!(
        annotation(&result, c),
        Annotation::present(Confidence::Confirmed)
    );
    assert!(
        row(&result, c).disputed,
        "dispute must propagate through a composition"
    );
    assert!(
        row(&result, downstream).disputed,
        "dispute must keep propagating, not stop after one hop"
    );
}

#[test]
fn an_alternate_derived_route_sets_derived_even_when_a_stronger_assertion_wins() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    // Confirmed direct assertion, plus a hypothetical 2-hop route.
    builder.edge(a, RelationId::DependsOn, c, Confidence::Confirmed);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Hypothetical);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Hypothetical);
    let graph = builder.build();

    let result = support::solve(&graph, a, RelationId::DependsOn, false);
    let entry = row(&result, c);
    assert_eq!(entry.annotation, Annotation::present(Confidence::Confirmed));
    assert!(entry.has_asserted);
    assert!(
        entry.has_derived,
        "a weaker route still proves a derivation exists"
    );
}

// ---------------------------------------------------------------------------
// Input permutation stability and layers
// ---------------------------------------------------------------------------

#[test]
fn results_are_stable_under_assertion_permutation() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let d = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Hypothetical);
    builder.edge(a, RelationId::DependsOn, d, Confidence::Contested);
    builder.edge(d, RelationId::DependsOn, c, Confidence::Confirmed);
    let graph = builder.build();

    let forward = support::solve(&graph, a, RelationId::DependsOn, false).canonical_rows();

    // Reverse the record order and rebuild: the graph sorts internally, but the
    // reference must not depend on that either.
    let mut reversed = builder.clone();
    reversed.reverse_assertions();
    let reversed_graph = reversed.build();
    let backward =
        support::solve(&reversed_graph, a, RelationId::DependsOn, false).canonical_rows();

    assert_eq!(forward, backward);
}

#[test]
fn a_target_layer_is_reported_as_authored_not_recomputed() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let deep = builder.term(SemanticLayer::L3);
    let shallow = builder.term(SemanticLayer::L0);
    builder.edge_c(deep, RelationId::IsA, shallow);
    let graph = builder.build();

    // The row reports endpoints and annotation only; the term table is the layer
    // authority and must be untouched by evaluation.
    let result = support::solve(&graph, deep, RelationId::IsA, false);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(graph.term(shallow).unwrap().layer, SemanticLayer::L0);
    assert_eq!(graph.term(deep).unwrap().layer, SemanticLayer::L3);
}
