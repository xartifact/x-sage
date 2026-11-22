//! Validation tests: V1–V6, profile gating, and the legacy cycle distinction.

mod support;

use sage_db_infer::{MetricStatus, Violation, validate};
use sage_db_types::{Confidence, RelationId, RuleProfile, SemanticLayer};
use support::Builder;

fn report(graph: &sage_db_types::BaseGraph) -> sage_db_infer::ValidationReport {
    validate::validate_complete(graph).expect("validation must complete under the default budget")
}

#[test]
fn a_legal_chain_validates_cleanly() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L0);
    builder.edge_c(a, RelationId::DependsOn, b);

    let report = report(&builder.build());
    assert!(report.is_valid(), "{:?}", report.violations);
    assert_eq!(report.metric, MetricStatus::Available);
}

#[test]
fn layer_direction_is_enforced_under_formal_v1() {
    // L1 depending on L2 violates "target <= source" (legacy) and "target < source"
    // (formal) alike for definition relations, but depends-on is unrestricted; use
    // is-a so the layer rule actually applies.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let shallow = builder.term(SemanticLayer::L1);
    let deep = builder.term(SemanticLayer::L2);
    builder.edge_c(shallow, RelationId::IsA, deep);

    let report = report(&builder.build());
    assert_eq!(report.violations.len(), 1);
    assert!(matches!(
        report.violations[0],
        Violation::LayerDirection {
            relation: RelationId::IsA,
            ..
        }
    ));
    assert_eq!(report.violations[0].code(), "V3");
}

#[test]
fn formal_requires_a_strictly_shallower_definition_target() {
    // Same layer is legal under legacy, illegal under formal.
    let build = |profile| {
        let mut builder = Builder::new(profile);
        let a = builder.term(SemanticLayer::L1);
        let b = builder.term(SemanticLayer::L1);
        builder.edge_c(a, RelationId::IsA, b);
        report(&builder.build())
    };

    assert!(
        build(RuleProfile::LegacyV1).is_valid(),
        "legacy allows equal layers"
    );
    let formal = build(RuleProfile::FormalV1);
    assert!(
        !formal.is_valid(),
        "formal requires a strictly shallower target"
    );
    assert!(matches!(
        formal.violations[0],
        Violation::LayerDirection { .. }
    ));
}

#[test]
fn multiple_instantiation_targets_count_unique_endpoints_not_supports() {
    // Two different targets: a violation.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let instance = builder.term(SemanticLayer::L3);
    let first = builder.term(SemanticLayer::L1);
    let second = builder.term(SemanticLayer::L1);
    builder.edge_c(instance, RelationId::Instantiates, first);
    builder.edge_c(instance, RelationId::Instantiates, second);
    let first_report = report(&builder.build());
    assert!(matches!(
        first_report.violations[0],
        Violation::MultipleInstantiationTargets { .. }
    ));

    // Two *supports* for the same target: legal. Counting assertion ids here is the
    // bug the review called out.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let instance = builder.term(SemanticLayer::L3);
    let target = builder.term(SemanticLayer::L1);
    builder.edge(
        instance,
        RelationId::Instantiates,
        target,
        Confidence::Confirmed,
    );
    builder.edge(
        instance,
        RelationId::Instantiates,
        target,
        Confidence::Hypothetical,
    );
    let second_report = report(&builder.build());
    assert!(
        second_report.is_valid(),
        "duplicate supports for one target are not multiple targets: {:?}",
        second_report.violations
    );
}

#[test]
fn l3_coverage_is_formal_only() {
    let build = |profile| {
        let mut builder = Builder::new(profile);
        let _lonely = builder.term(SemanticLayer::L3);
        report(&builder.build())
    };

    assert!(
        build(RuleProfile::LegacyV1).is_valid(),
        "legacy does not require instantiation coverage"
    );
    let formal = build(RuleProfile::FormalV1);
    assert!(!formal.is_valid());
    assert!(matches!(
        formal.violations[0],
        Violation::LayerThreeWithoutTarget { .. }
    ));
}

#[test]
fn acyclic_relations_reject_cycles() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    builder.edge_c(a, RelationId::DependsOn, b);
    builder.edge_c(b, RelationId::DependsOn, c);
    builder.edge_c(c, RelationId::DependsOn, a);
    // A legal first edge justified by the chain above: the cycle is still a cycle.
    builder.edge_c(a, RelationId::DependsOn, c);

    let report = report(&builder.build());
    assert!(report
        .violations
        .iter()
        .any(|violation| matches!(violation, Violation::RelationCycle { relation, .. } if *relation == RelationId::DependsOn)));
}

#[test]
fn cyclic_relations_are_allowed_where_the_profile_says_so() {
    // `enables` is not acyclic, so a cycle is legal there.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge_c(a, RelationId::Enables, b);
    builder.edge_c(b, RelationId::Enables, a);

    let report = report(&builder.build());
    assert!(report.is_valid(), "{:?}", report.violations);
}

#[test]
fn legacy_mixed_definition_cycle_makes_the_metric_unavailable_without_failing() {
    // is-a + depends-on forming a cycle: legacy data may legally contain this, and
    // the design says only the depth metric becomes unavailable.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge_c(a, RelationId::IsA, b);
    builder.edge_c(b, RelationId::DependsOn, a);

    let report = report(&builder.build());
    assert!(
        report.is_valid(),
        "legacy mixed cycles must not block otherwise legal data: {:?}",
        report.violations
    );
    assert!(matches!(report.metric, MetricStatus::Unavailable { .. }));
}

#[test]
fn formal_mixed_definition_cycle_is_a_violation() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge_c(a, RelationId::IsA, b);
    builder.edge_c(b, RelationId::DependsOn, a);

    let report = report(&builder.build());
    assert!(
        report
            .violations
            .iter()
            .any(|violation| matches!(violation, Violation::DefinitionUnionCycle { .. }))
    );
}

#[test]
fn the_report_states_what_it_does_not_check() {
    // Silence must never be readable as "V7 passed".
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L1);
    builder.edge_c(a, RelationId::Enables, a);
    // Self relations are rejected at construction, so use an isolated legal graph.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let _a = builder.term(SemanticLayer::L0);
    let report = report(&builder.build());

    assert!(report.enforced.iter().any(|check| check.starts_with("V1")));
    assert!(
        report.not_enforced.iter().any(|check| check.contains("V7")),
        "V7 must be declared unenforced, got {:?}",
        report.not_enforced
    );
}

#[test]
fn validation_stops_on_a_budget_it_cannot_finish_within() {
    use sage_db_infer::Work;
    use sage_db_types::Budget;

    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let terms = builder.terms_at(SemanticLayer::L1, 8);
    to_chain(&mut builder, &terms);
    let graph = builder.build();

    // Enough rows for admission, too few steps to finish the scan.
    let starved = Budget {
        max_steps: 2,
        ..Budget::DEFAULT
    };
    let control = sage_db_types::RunToCompletion;
    let mut work = Work::new(starved);
    let error = validate::validate(&graph, &control, &mut work).unwrap_err();
    assert!(matches!(
        error,
        sage_db_types::QueryError::BudgetExceeded(sage_db_types::BudgetLimit::Steps)
    ));
}

fn to_chain(builder: &mut Builder, terms: &[sage_db_types::TermId]) {
    for pair in terms.windows(2) {
        builder.edge_c(pair[0], RelationId::DependsOn, pair[1]);
    }
}

#[test]
fn every_diagnostic_is_charged_as_a_work_row() {
    // Review finding: 200 isolated L3 terms produced 200 diagnostics with
    // work_rows = 0. A diagnostic is a materialised row and must be charged.
    use sage_db_infer::Work;

    let mut builder = Builder::new(RuleProfile::FormalV1);
    for _ in 0..200 {
        builder.term(SemanticLayer::L3);
    }
    let graph = builder.build();

    let control = sage_db_types::RunToCompletion;
    let mut work = Work::new(sage_db_types::Budget::DEFAULT);
    let report = validate::validate(&graph, &control, &mut work).unwrap();
    assert_eq!(report.violations.len(), 200, "one V5 per uncovered L3 term");
    assert!(
        work.work_rows() >= 200,
        "each diagnostic needs a charged row, got {}",
        work.work_rows()
    );
}

#[test]
fn a_diagnostic_heavy_graph_can_exhaust_its_row_budget() {
    // With a tight row allowance the validator must refuse rather than accumulate
    // diagnostics for free.
    use sage_db_infer::Work;

    let mut builder = Builder::new(RuleProfile::FormalV1);
    for _ in 0..50 {
        builder.term(SemanticLayer::L3);
    }
    let graph = builder.build();

    let control = sage_db_types::RunToCompletion;
    let mut work = Work::new(sage_db_types::Budget {
        max_work_rows: 40,
        ..sage_db_types::Budget::DEFAULT
    });
    let error = validate::validate(&graph, &control, &mut work).unwrap_err();
    assert!(matches!(
        error,
        sage_db_types::QueryError::BudgetExceeded(sage_db_types::BudgetLimit::WorkRows)
    ));
}

#[test]
fn cancellation_reaches_the_early_validation_phases() {
    // Review finding: check_nodes / check_layer_direction / cardinality / coverage only
    // consulted control at the end of the whole phase, so a large graph could complete
    // an entire scan before noticing a cancel.
    use sage_db_infer::Work;

    struct StopAfter(std::cell::Cell<u32>);
    impl sage_db_types::QueryControl for StopAfter {
        fn checkpoint(&self) -> Result<(), sage_db_types::ControlStop> {
            let left = self.0.get();
            if left == 0 {
                return Err(sage_db_types::ControlStop::Cancelled);
            }
            self.0.set(left - 1);
            Ok(())
        }
    }

    // A large graph with both V1-ish work (many assertions) and V3 work (layer rules).
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let root = builder.term(SemanticLayer::L2);
    for _ in 0..300 {
        let leaf = builder.term(SemanticLayer::L1);
        builder.edge_c(root, RelationId::DependsOn, leaf);
    }
    let graph = builder.build();

    // A small allowance must stop inside a phase, not after it.
    let control = StopAfter(std::cell::Cell::new(0));
    let mut work = Work::new(sage_db_types::Budget::DEFAULT);
    assert!(
        matches!(
            validate::validate(&graph, &control, &mut work).unwrap_err(),
            sage_db_types::QueryError::Stopped(_)
        ),
        "cancellation must be observed before finishing the first phase"
    );

    // And a mid-phase allowance must stop too (not only at phase boundaries).
    let control = StopAfter(std::cell::Cell::new(2));
    let mut work = Work::new(sage_db_types::Budget::DEFAULT);
    let outcome = validate::validate(&graph, &control, &mut work);
    assert!(
        outcome.is_err(),
        "a 2-checkpoint allowance must not complete a 300-edge validation"
    );
}

#[test]
fn l3_coverage_charges_every_scanned_edge() {
    // `.any()` over a wide L3 node hid the scan. Each examined edge must be charged.
    use sage_db_infer::Work;

    let mut builder = Builder::new(RuleProfile::FormalV1);
    let instance = builder.term(SemanticLayer::L3);
    // Many non-instantiates edges first, so the scan is genuinely long.
    for _ in 0..200 {
        let other = builder.term(SemanticLayer::L1);
        builder.edge_c(instance, RelationId::DependsOn, other);
    }
    let target = builder.term(SemanticLayer::L1);
    builder.edge_c(instance, RelationId::Instantiates, target);
    let graph = builder.build();

    let control = sage_db_types::RunToCompletion;
    let mut work = Work::new(sage_db_types::Budget::DEFAULT);
    let before = work.steps();
    validate::validate(&graph, &control, &mut work).unwrap();
    assert!(
        work.steps() - before > 200,
        "the L3 scan must charge per examined edge, charged {}",
        work.steps() - before
    );
}
