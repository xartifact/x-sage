//! Cross-check: fixed-point saturation against independent path enumeration.
//!
//! The two evaluators in this file are built from the same raw records but from
//! completely different shapes — a repeated relaxation over an ordered table versus
//! explicit DFS over every simple path. Agreement is evidence; disagreement is a bug
//! in one of them, and neither can hide behind the other because neither reads the
//! other's state.
//!
//! These tests deliberately do **not** compare proofs. Path enumeration has no
//! selection policy, so a differing proof for an equally strong and equally short
//! route is not a defect.

mod support;

use sage_db_reference::paths::{self, Caps, RawRecords};
use sage_db_types::{
    Annotation, Confidence, RelationId, RuleProfile, RuleSet, SemanticLayer, TermId,
};
use support::Builder;

/// Compare enumeration against the executor for one query.
fn cross_check(builder: &Builder, source: TermId, relation: RelationId) {
    let graph = builder.build();
    let result = support::solve(&graph, source, relation, false);

    let mut work = support::work();
    let enumeration = paths::enumerate(
        RawRecords {
            terms: graph.terms(),
            assertions: graph.assertions(),
            rules: graph.rules(),
        },
        relation,
        source,
        Caps::DEFAULT,
        &mut work,
        &sage_db_types::RunToCompletion,
    )
    .expect("enumeration must complete under the caps");

    // Per-target comparison: same presence, same annotation, same flags.
    let mut targets: Vec<TermId> = enumeration
        .paths
        .iter()
        .map(|path| path.steps.last().expect("non-empty path").to)
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets.retain(|target| *target != source);

    for target in &targets {
        let direct: Vec<&paths::Path> = enumeration
            .paths
            .iter()
            .filter(|path| path.steps.last().expect("non-empty").to == *target)
            .collect();
        assert!(!direct.is_empty());

        let expected_annotation = direct
            .iter()
            .fold(Annotation::ABSENT, |acc, path| acc.join(path.annotation()));
        let expected_disputed = direct.iter().any(|path| path.disputed());
        // A one-hop path that used a rule already counts as derived; so does any
        // path of two or more hops.
        let expected_derived = direct
            .iter()
            .any(|path| path.derived() || path.steps.len() >= 2);
        let expected_asserted = direct
            .iter()
            .any(|path| path.steps.len() == 1 && path.steps[0].rule.is_none());

        let from_table = sage_db_reference::annotation_of(&result, *target);
        let row = sage_db_reference::row_for(&result, *target);

        assert_eq!(
            from_table, expected_annotation,
            "annotation disagrees for {target:?} in {relation} from {source:?}"
        );
        let row = row.unwrap_or_else(|| {
            panic!("saturation found no row for {target:?} but enumeration did")
        });
        assert_eq!(
            row.disputed, expected_disputed,
            "disputed disagrees for {target:?} in {relation}"
        );
        assert_eq!(
            row.has_asserted, expected_asserted,
            "asserted disagrees for {target:?} in {relation}"
        );
        assert_eq!(
            row.has_derived, expected_derived,
            "derived disagrees for {target:?} in {relation}"
        );
    }

    // Nothing extra: every saturated row must be reachable by some enumerated path.
    for row in &result.rows {
        assert!(
            targets.contains(&row.to),
            "saturation reported {row:?} but enumeration found no path"
        );
    }
}

fn caps_work() -> (Caps, RuleSet) {
    (Caps::DEFAULT, support::rules(RuleProfile::LegacyV1))
}

#[test]
fn a_chain_agrees() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let terms = builder.terms_at(SemanticLayer::L1, 5);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    for source in &terms[..4] {
        cross_check(&builder, *source, RelationId::DependsOn);
    }
    let _ = caps_work();
}

#[test]
fn a_diamond_agrees_on_every_route_length() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let left = builder.term(SemanticLayer::L1);
    let right = builder.term(SemanticLayer::L1);
    let sink = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, left, Confidence::Confirmed);
    builder.edge(a, RelationId::DependsOn, right, Confidence::Hypothetical);
    builder.edge(left, RelationId::DependsOn, sink, Confidence::Hypothetical);
    builder.edge(right, RelationId::DependsOn, sink, Confidence::Confirmed);
    cross_check(&builder, a, RelationId::DependsOn);
    cross_check(&builder, left, RelationId::DependsOn);
}

#[test]
fn mixed_confidence_agrees() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let d = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Contested);
    builder.edge(a, RelationId::DependsOn, c, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, d, Confidence::Confirmed);
    builder.edge(c, RelationId::DependsOn, d, Confidence::Hypothetical);
    cross_check(&builder, a, RelationId::DependsOn);
}

#[test]
fn a_symmetric_relation_agrees_including_the_round_trip_rule() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L2);
    let c = builder.term(SemanticLayer::L2);
    builder.edge(a, RelationId::Contradicts, b, Confidence::Confirmed);
    builder.edge(c, RelationId::Contradicts, a, Confidence::Hypothetical);
    for source in [a, b, c] {
        cross_check(&builder, source, RelationId::Contradicts);
    }
}

#[test]
fn formal_dependency_seeds_agree() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let instance = builder.term(SemanticLayer::L3);
    let kind = builder.term(SemanticLayer::L2);
    let core = builder.term(SemanticLayer::L1);
    let axiom = builder.term(SemanticLayer::L0);
    builder.edge(
        instance,
        RelationId::Instantiates,
        kind,
        Confidence::Confirmed,
    );
    builder.edge(kind, RelationId::IsA, core, Confidence::Hypothetical);
    builder.edge(core, RelationId::PartOf, axiom, Confidence::Confirmed);
    cross_check(&builder, instance, RelationId::DependsOn);
    cross_check(&builder, kind, RelationId::DependsOn);
    cross_check(&builder, instance, RelationId::Instantiates);
}

#[test]
fn duplicate_supports_agree() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Contested);
    builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Confirmed, 2);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Confirmed);
    cross_check(&builder, a, RelationId::DependsOn);
}

#[test]
fn a_non_transitive_relation_agrees_on_direct_facts_only() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::Enables, b, Confidence::Confirmed);
    builder.edge(b, RelationId::Enables, c, Confidence::Confirmed);
    cross_check(&builder, a, RelationId::Enables);
}

#[test]
fn enumeration_refuses_a_graph_beyond_its_cap_instead_of_truncating() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let terms = builder.terms_at(SemanticLayer::L1, 6);
    for pair in terms.windows(2) {
        builder.edge_c(pair[0], RelationId::DependsOn, pair[1]);
    }
    let graph = builder.build();

    let mut work = support::work();
    let error = paths::enumerate(
        RawRecords {
            terms: graph.terms(),
            assertions: graph.assertions(),
            rules: graph.rules(),
        },
        RelationId::DependsOn,
        terms[0],
        Caps {
            max_terms: 4,
            ..Caps::DEFAULT
        },
        &mut work,
        &sage_db_types::RunToCompletion,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        sage_db_types::QueryError::InvalidRequest { .. }
    ));

    // A path cap is reported the same way; neither is silently truncated.
    let mut work = support::work();
    let error = paths::enumerate(
        RawRecords {
            terms: graph.terms(),
            assertions: graph.assertions(),
            rules: graph.rules(),
        },
        RelationId::DependsOn,
        terms[0],
        Caps {
            max_paths: 1,
            ..Caps::DEFAULT
        },
        &mut work,
        &sage_db_types::RunToCompletion,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        sage_db_types::QueryError::InvalidRequest { .. }
    ));
}

#[test]
fn a_dense_dag_agrees_at_a_bounded_size() {
    // Layered DAG: every node on layer i points at two nodes on layer i+1. Bounded
    // so enumeration stays cheap while the route count is still combinatorial.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let mut layers: Vec<Vec<TermId>> = Vec::new();
    for _ in 0..4 {
        layers.push(builder.terms_at(SemanticLayer::L1, 3));
    }
    for pair in layers.windows(2) {
        for (index, from) in pair[0].iter().enumerate() {
            builder.edge_c(*from, RelationId::DependsOn, pair[1][index % 3]);
            builder.edge_c(*from, RelationId::DependsOn, pair[1][(index + 1) % 3]);
        }
    }
    cross_check(&builder, layers[0][0], RelationId::DependsOn);
}
