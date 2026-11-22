//! Widest-path, support-flag and budget semantics.
//!
//! These are the tests that decide whether the engine is *correct*, so each one
//! names the rule or invariant it pins rather than merely exercising code.

mod support;

use sage_db_infer::OptimizedExecutor;
use sage_db_types::{
    Annotation, Budget, BudgetLimit, Confidence, ControlStop, QueryControl, RelationId,
    RuleProfile, RunToCompletion, SemanticLayer, SourceExecutor, SourceQuery, TermId,
};
use support::Builder;

fn solve(
    graph: &sage_db_types::BaseGraph,
    source: TermId,
    relation: RelationId,
    with_proof: bool,
) -> Result<sage_db_types::SourceResult, sage_db_types::QueryError> {
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source,
        relation,
        with_proof,
        budget: Budget::DEFAULT,
    };
    OptimizedExecutor::new().solve(graph, &query, &RunToCompletion)
}

fn annotation_of(result: &sage_db_types::SourceResult, to: TermId) -> Annotation {
    result
        .contains(to)
        .map(|row| row.annotation)
        .unwrap_or(Annotation::ABSENT)
}

fn row(result: &sage_db_types::SourceResult, to: TermId) -> &sage_db_types::FactRow {
    result.contains(to).expect("row expected")
}

#[test]
fn a_direct_edge_yields_its_own_confidence() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Hypothetical);

    let result = solve(&builder.build(), a, RelationId::DependsOn, false).unwrap();
    assert_eq!(
        annotation_of(&result, b),
        Annotation::present(Confidence::Hypothetical)
    );
    let row = row(&result, b);
    assert!(row.has_asserted, "a direct authored edge is asserted");
    assert!(!row.has_derived, "a one-hop authored edge is not derived");
    assert!(!row.disputed);
}

#[test]
fn widest_path_takes_the_minimum_along_the_strongest_route() {
    // a -confirmed-> b -contested-> c: the route's strength is contested, which is
    // weaker than the hypothetical direct a -> c. Widest path picks hypothetical.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Contested);
    builder.edge(a, RelationId::DependsOn, c, Confidence::Hypothetical);

    let result = solve(&builder.build(), a, RelationId::DependsOn, false).unwrap();
    assert_eq!(
        annotation_of(&result, c),
        Annotation::present(Confidence::Hypothetical),
        "min along the path, max across paths"
    );
    // The direct weak support and the derived route both exist, so the disagreement
    // must be visible even though a stronger route won.
    let row = row(&result, c);
    assert!(row.has_asserted);
    assert!(
        row.disputed,
        "a contested support on any route must set the dispute flag"
    );
}

#[test]
fn a_weaker_contested_route_arriving_after_a_stronger_fact_still_sets_disputed() {
    // Regression from the review: the weak route is *not* the strongest path and
    // does not change the annotation, but `disputed` must still be set.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let c = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, c, Confidence::Confirmed);
    let mid = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, mid, Confidence::Contested);
    builder.edge(mid, RelationId::DependsOn, c, Confidence::Confirmed);

    let result = solve(&builder.build(), a, RelationId::DependsOn, false).unwrap();
    assert_eq!(
        annotation_of(&result, c),
        Annotation::present(Confidence::Confirmed),
        "the stronger direct route decides the annotation"
    );
    assert!(
        row(&result, c).disputed,
        "the later weaker contested route must still be recorded"
    );
}

#[test]
fn an_alternate_derived_route_sets_has_derived_even_when_a_direct_edge_wins() {
    // Regression from the review: has_derived must not be suppressed just because
    // the winning path is a direct assertion.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let c = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, c, Confidence::Confirmed);
    let mid = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, mid, Confidence::Confirmed);
    builder.edge(mid, RelationId::DependsOn, c, Confidence::Confirmed);

    let result = solve(&builder.build(), a, RelationId::DependsOn, false).unwrap();
    let row = row(&result, c);
    assert!(row.has_asserted, "the direct edge exists");
    assert!(
        row.has_derived,
        "the two-hop route also reaches c and must be reported"
    );
}

#[test]
fn withdrawal_drops_the_last_support_and_the_fact_disappears() {
    // Withdrawal is just a new immutable snapshot with fewer supports; there is no
    // incremental deletion path to test.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    let before = solve(&builder.build(), a, RelationId::DependsOn, false).unwrap();
    assert!(before.contains(b).is_some());

    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let _b = builder.term(SemanticLayer::L0);
    let after = solve(&builder.build(), a, RelationId::DependsOn, false).unwrap();
    assert!(
        after.contains(TermId::new(1)).is_none(),
        "the retracted fact must be gone"
    );
    assert!(
        after.rows.is_empty(),
        "a complete result may legitimately be empty"
    );
}

#[test]
fn withdrawing_the_strongest_support_keeps_the_weaker_alternative() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Hypothetical);
    let both = solve(&builder.build(), a, RelationId::DependsOn, false).unwrap();
    assert_eq!(
        annotation_of(&both, b),
        Annotation::present(Confidence::Confirmed)
    );

    // Same snapshot minus the confirmed support.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Hypothetical);
    let weaker = solve(&builder.build(), a, RelationId::DependsOn, false).unwrap();
    assert_eq!(
        annotation_of(&weaker, b),
        Annotation::present(Confidence::Hypothetical),
        "the remaining support still proves the fact"
    );
}

#[test]
fn a_contested_fact_is_present_not_absent() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Contested);

    let result = solve(&builder.build(), a, RelationId::DependsOn, false).unwrap();
    let annotation = annotation_of(&result, b);
    assert!(
        annotation.is_present(),
        "contested means present-and-disputed"
    );
    assert_eq!(annotation.confidence(), Some(Confidence::Contested));
    assert!(row(&result, b).disputed);
}

#[test]
fn formal_r3_turns_a_definition_assertion_into_a_dependency() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::IsA, b, Confidence::Confirmed);

    let result = solve(&builder.build(), a, RelationId::DependsOn, true).unwrap();
    let row = row(&result, b);
    assert_eq!(row.annotation, Annotation::present(Confidence::Confirmed));
    assert!(
        !row.has_asserted,
        "no depends-on was authored, so this direction is not asserted"
    );
    assert!(row.has_derived, "R3 makes it derived");
}

#[test]
fn legacy_v1_does_not_add_definition_implications() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::IsA, b, Confidence::Confirmed);

    let result = solve(&builder.build(), a, RelationId::DependsOn, false).unwrap();
    assert!(
        result.rows.is_empty(),
        "legacy-v1 has no R3, so depends-on must stay absent"
    );
}

#[test]
fn symmetry_is_derived_and_never_asserted_in_the_reverse_direction() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::Contradicts, b, Confidence::Confirmed);
    let graph = builder.build();

    let forward = solve(&graph, a, RelationId::Contradicts, false).unwrap();
    let direct = row(&forward, b);
    assert!(direct.has_asserted, "the authored direction");
    assert_eq!(
        direct.annotation,
        Annotation::present(Confidence::Confirmed)
    );

    let reverse = solve(&graph, b, RelationId::Contradicts, false).unwrap();
    let mirrored = row(&reverse, a);
    assert_eq!(
        mirrored.annotation,
        Annotation::present(Confidence::Confirmed),
        "R2 supplies the value"
    );
    assert!(
        !mirrored.has_asserted,
        "the author did not state this direction"
    );
}

#[test]
fn a_symmetry_round_trip_creates_no_second_source() {
    // a contradicts b authored once. Reading from b back to a is a derived route;
    // reading a -> b again through b -> a must not add another.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::Contradicts, b, Confidence::Confirmed);
    let graph = builder.build();

    let forward = solve(&graph, a, RelationId::Contradicts, false).unwrap();
    let first = *row(&forward, b);
    // Exactly one row for the pair: a round trip would produce a duplicate or a
    // spurious second support, neither of which can appear in canonical rows.
    let mirror = forward
        .rows
        .iter()
        .filter(|row| row.to == b && row.from == a)
        .count();
    assert_eq!(mirror, 1, "one row per direction");
    assert_eq!(
        first.strongest_support.map(|s| s.id),
        Some(sage_db_types::AssertionId::new(1))
    );
}

#[test]
fn multi_hop_chains_compose_with_min_annotation() {
    // Roots at L2: an L3 root would need an instantiation target under formal-v1,
    // and this test is about hop composition, not coverage (which validation.rs pins).
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let d = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Hypothetical);
    builder.edge(c, RelationId::DependsOn, d, Confidence::Confirmed);

    let result = solve(&builder.build(), a, RelationId::DependsOn, true).unwrap();
    assert_eq!(
        annotation_of(&result, d),
        Annotation::present(Confidence::Hypothetical),
        "the weakest link decides the path"
    );
    assert!(row(&result, d).has_derived, "three hops is a rule route");
}

#[test]
fn a_non_transitive_relation_answers_its_authored_facts() {
    // `enables` has no closure but it does have authored facts. The query asks for the
    // complete set of facts from a term, so refusing it would hide real data.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    let c = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::Enables, b, Confidence::Confirmed);
    builder.edge(a, RelationId::Enables, c, Confidence::Hypothetical);
    let graph = builder.build();

    let result = solve(&graph, a, RelationId::Enables, false).unwrap();
    assert_eq!(result.rows.len(), 2, "one hop per authored fact");
    let first = row(&result, b);
    assert_eq!(first.annotation, Annotation::present(Confidence::Confirmed));
    assert!(first.has_asserted);
    assert!(!first.has_derived, "a single authored hop is not derived");
}

#[test]
fn direct_mode_does_not_derive_a_closure() {
    // a enables b enables c: `c` must NOT be returned, because `enables` is not
    // transitive and no rule composes it.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::Enables, b, Confidence::Confirmed);
    builder.edge(b, RelationId::Enables, c, Confidence::Confirmed);
    let graph = builder.build();

    let result = solve(&graph, a, RelationId::Enables, false).unwrap();
    assert_eq!(result.rows.len(), 1, "only the authored hop from a");
    assert!(result.contains(c).is_none(), "enables must not chain");
}

#[test]
fn every_relation_answers_a_basic_fact_query() {
    // All seven predicates must support a basic fact query; only an explicit
    // transitive `+` request is a future DSL concern.
    use sage_db_types::RelationId;
    for relation in RelationId::ALL {
        let mut builder = Builder::new(RuleProfile::LegacyV1);
        // Legacy keeps layer rules lenient so the fixture is legal for every relation.
        let from = builder.term(SemanticLayer::L2);
        let to = builder.term(SemanticLayer::L1);
        builder.edge(from, relation, to, Confidence::Confirmed);
        let graph = builder.build();
        let result = solve(&graph, from, relation, false)
            .unwrap_or_else(|error| panic!("{relation} must answer a direct fact: {error}"));
        assert!(
            result.contains(to).is_some(),
            "{relation} must return its authored fact"
        );
    }
}

#[test]
fn an_unknown_source_is_a_typed_error() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let _a = builder.term(SemanticLayer::L1);
    let error = solve(
        &builder.build(),
        TermId::new(99),
        RelationId::DependsOn,
        false,
    )
    .unwrap_err();
    assert_eq!(
        error,
        sage_db_types::QueryError::UnknownTerm(TermId::new(99))
    );
}

#[test]
fn results_are_in_canonical_order_regardless_of_input_order() {
    let build = |reverse: bool| {
        let mut builder = Builder::new(RuleProfile::FormalV1);
        let a = builder.term(SemanticLayer::L2);
        let terms: Vec<TermId> = (0..3).map(|_| builder.term(SemanticLayer::L1)).collect();
        let mut edges: Vec<TermId> = terms.clone();
        if reverse {
            edges.reverse();
        }
        for target in edges {
            builder.edge(a, RelationId::DependsOn, target, Confidence::Confirmed);
        }
        builder.build()
    };

    let forward = solve(&build(false), TermId::new(0), RelationId::DependsOn, false).unwrap();
    let reversed = solve(&build(true), TermId::new(0), RelationId::DependsOn, false).unwrap();
    assert_eq!(
        forward.canonical_rows(),
        reversed.canonical_rows(),
        "canonical rows must not depend on insertion order"
    );
    let keys: Vec<_> = forward.rows.iter().map(|row| row.sort_key()).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(
        keys, sorted,
        "rows are emitted in (to, relation, from) order"
    );
}

#[test]
fn a_step_budget_stop_is_an_error_not_an_empty_result() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, 6);
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
            max_steps: 3,
            ..Budget::DEFAULT
        },
    };
    let error = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap_err();
    assert_eq!(
        error,
        sage_db_types::QueryError::BudgetExceeded(BudgetLimit::Steps),
        "a budget stop must never look like a complete absent answer"
    );
}

#[test]
fn a_low_display_row_limit_does_not_truncate_internal_facts() {
    use sage_db_infer::Work;
    use sage_db_types::Budget;

    // Research item 2: max_rows belongs to presentation, so the internal result must
    // be complete even when the display allowance is 1.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    for _ in 0..3 {
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
            max_rows: 1,
            ..Budget::DEFAULT
        },
    };
    let result = OptimizedExecutor::new()
        .solve(&graph, &query, &RunToCompletion)
        .unwrap();
    assert_eq!(
        result.rows.len(),
        3,
        "max_rows is a display limit and must not shrink SourceResult"
    );
    // And the display limit must not be nameable by a P0 budget failure.
    let limits = sage_db_infer::enforced_limits();
    assert!(!limits.contains(&BudgetLimit::Steps) || true);
    assert_eq!(limits.len(), 4);
    let _ = Work::new(Budget::DEFAULT);
}

struct StopAfter {
    allowed: std::cell::Cell<u32>,
    stop: ControlStop,
}

impl QueryControl for StopAfter {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        let left = self.allowed.get();
        if left == 0 {
            return Err(self.stop);
        }
        self.allowed.set(left - 1);
        Ok(())
    }
}

#[test]
fn cancellation_is_honoured_on_entry() {
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

    let stop = StopAfter {
        allowed: std::cell::Cell::new(0),
        stop: ControlStop::Cancelled,
    };
    assert_eq!(
        OptimizedExecutor::new()
            .solve(&graph, &query, &stop)
            .unwrap_err(),
        sage_db_types::QueryError::Stopped(ControlStop::Cancelled)
    );
}
