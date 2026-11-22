//! Annotation carrier, layer/depth separation and the fixed relation table.
//!
//! The relation table is duplicated from `docs/design/data-model.md` §4 on
//! purpose: a silent change to a rule template is a semantic change, and this test
//! is where it becomes visible.

use sage_db_types::{
    Annotation, Confidence, DefinitionDepth, DepthStatus, DepthUnavailable, LayerRule, RelationId,
    RuleProfile, SemanticLayer,
};

#[test]
fn contested_is_present_and_distinct_from_absent() {
    // The whole point of the four-element carrier: a disputed fact exists.
    let contested = Annotation::present(Confidence::Contested);
    assert!(contested.is_present());
    assert_ne!(contested, Annotation::ABSENT);
    assert_eq!(contested.confidence(), Some(Confidence::Contested));
    assert_eq!(Annotation::ABSENT.confidence(), None);
}

#[test]
fn join_is_max_with_bottom_as_identity() {
    for level in Confidence::ALL {
        let present = Annotation::present(level);
        assert_eq!(present.join(Annotation::ABSENT), present);
        assert_eq!(Annotation::ABSENT.join(present), present);
        assert_eq!(present.join(present), present);
    }
    assert_eq!(
        Annotation::ABSENT.join(Annotation::ABSENT),
        Annotation::ABSENT
    );

    let confirmed = Annotation::present(Confidence::Confirmed);
    let contested = Annotation::present(Confidence::Contested);
    assert_eq!(contested.join(confirmed), confirmed);
    assert_eq!(confirmed.join(contested), confirmed);
}

#[test]
fn meet_is_min_with_bottom_as_absorber() {
    let confirmed = Annotation::present(Confidence::Confirmed);
    let hypothetical = Annotation::present(Confidence::Hypothetical);
    assert_eq!(confirmed.meet(hypothetical), hypothetical);
    assert_eq!(hypothetical.meet(confirmed), hypothetical);
    assert_eq!(confirmed.meet(Annotation::ABSENT), Annotation::ABSENT);
    assert_eq!(
        Annotation::ABSENT.meet(Annotation::ABSENT),
        Annotation::ABSENT
    );

    // A path through any unsupported step has no support at all.
    let contested = Annotation::present(Confidence::Contested);
    assert_eq!(contested.meet(hypothetical), contested);
}

#[test]
fn annotation_algebra_is_distributive_and_idempotent() {
    let elements = [
        Annotation::ABSENT,
        Annotation::present(Confidence::Contested),
        Annotation::present(Confidence::Hypothetical),
        Annotation::present(Confidence::Confirmed),
    ];

    for left in elements {
        for middle in elements {
            for right in elements {
                assert_eq!(
                    left.meet(middle.join(right)),
                    left.meet(middle).join(left.meet(right))
                );
                assert_eq!(
                    left.join(middle.meet(right)),
                    left.join(middle).meet(left.join(right))
                );
            }
            assert_eq!(left.join(middle), middle.join(left));
            assert_eq!(left.meet(middle), middle.meet(left));
            assert_eq!(left.join(left), left);
        }
    }
}

#[test]
fn confidence_levels_are_ordered_by_strength() {
    assert!(Confidence::Contested < Confidence::Hypothetical);
    assert!(Confidence::Hypothetical < Confidence::Confirmed);
    assert_eq!(Confidence::ALL.len(), 3);
    assert_eq!(Confidence::Confirmed.as_str(), "confirmed");
}

#[test]
fn layer_covers_zero_to_three_and_nothing_else() {
    let decoded: Vec<SemanticLayer> = (0..=3).filter_map(SemanticLayer::from_u8).collect();
    assert_eq!(decoded, SemanticLayer::ALL);
    assert!(SemanticLayer::from_u8(4).is_none());
    assert!(SemanticLayer::L0.is_axiom());
    assert!(!SemanticLayer::L1.is_axiom());
}

#[test]
fn definition_depth_is_not_a_layer() {
    // An L3 instance may be shallower than an L1 concept: depth is topological.
    let shallow_instance = DepthStatus::Available(DefinitionDepth::new(1));
    let deep_core = DepthStatus::Available(DefinitionDepth::new(9));
    assert!(shallow_instance.depth().unwrap() < deep_core.depth().unwrap());

    // Depth is unbounded, unlike the 0..=3 layer space.
    assert_eq!(DefinitionDepth::new(u32::MAX).get(), u32::MAX);

    // Unavailable is not absent: the term exists, the metric does not.
    let unavailable = DepthStatus::Unavailable(DepthUnavailable::CyclicDefinitionUnion);
    assert!(unavailable.depth().is_none());
    assert_ne!(unavailable, DepthStatus::Available(DefinitionDepth::new(0)));
}

#[test]
fn relation_ids_are_stable_and_closed() {
    assert_eq!(RelationId::ALL.len(), 7);
    for relation in RelationId::ALL {
        assert_eq!(
            RelationId::from_u8(relation.get()),
            Some(relation),
            "{relation} must round-trip through its persisted ID"
        );
    }
    assert!(
        RelationId::from_u8(7).is_none(),
        "unknown IDs are rejected, not clamped"
    );
    assert_eq!(RelationId::IsA.as_str(), "is-a");
    assert_eq!(RelationId::AnalogousTo.as_str(), "analogous-to");
}

#[test]
fn relation_table_matches_the_design_for_legacy_v1() {
    struct Expected {
        relation: RelationId,
        definition: bool,
        transitive: bool,
        symmetric: bool,
        acyclic: bool,
        layer_rule: LayerRule,
    }

    let expected = [
        Expected {
            relation: RelationId::IsA,
            definition: true,
            transitive: true,
            symmetric: false,
            acyclic: true,
            layer_rule: LayerRule::TargetAtMostSource,
        },
        Expected {
            relation: RelationId::PartOf,
            definition: true,
            transitive: true,
            symmetric: false,
            acyclic: true,
            layer_rule: LayerRule::TargetAtMostSource,
        },
        Expected {
            relation: RelationId::Instantiates,
            definition: true,
            transitive: false,
            symmetric: false,
            acyclic: true,
            layer_rule: LayerRule::TargetAtMostSource,
        },
        Expected {
            relation: RelationId::DependsOn,
            definition: false,
            transitive: true,
            symmetric: false,
            acyclic: true,
            layer_rule: LayerRule::Unrestricted,
        },
        Expected {
            relation: RelationId::Enables,
            definition: false,
            transitive: false,
            symmetric: false,
            acyclic: false,
            layer_rule: LayerRule::Unrestricted,
        },
        Expected {
            relation: RelationId::Contradicts,
            definition: false,
            transitive: false,
            symmetric: true,
            acyclic: false,
            layer_rule: LayerRule::Unrestricted,
        },
        Expected {
            relation: RelationId::AnalogousTo,
            definition: false,
            transitive: false,
            symmetric: true,
            acyclic: false,
            layer_rule: LayerRule::Unrestricted,
        },
    ];

    assert_eq!(
        expected.iter().map(|e| e.relation).collect::<Vec<_>>(),
        RelationId::ALL.to_vec(),
        "the table must cover every relation exactly once"
    );

    for item in expected {
        let spec = RuleProfile::LegacyV1.spec(item.relation);
        assert_eq!(
            spec.is_definition, item.definition,
            "{}: is_definition",
            item.relation
        );
        assert_eq!(
            spec.transitive, item.transitive,
            "{}: transitive",
            item.relation
        );
        assert_eq!(
            spec.symmetric, item.symmetric,
            "{}: symmetric",
            item.relation
        );
        assert_eq!(spec.acyclic, item.acyclic, "{}: acyclic", item.relation);
        assert_eq!(
            spec.layer_rule, item.layer_rule,
            "{}: layer rule",
            item.relation
        );
        assert!(
            !spec.implies_dependency,
            "{}: legacy-v1 must not add definition implications",
            item.relation
        );
    }
}

#[test]
fn formal_v1_only_tightens_definition_rules() {
    for relation in RelationId::ALL {
        let legacy = RuleProfile::LegacyV1.spec(relation);
        let formal = RuleProfile::FormalV1.spec(relation);
        assert_eq!(
            legacy.transitive, formal.transitive,
            "{relation}: transitivity is fixed"
        );
        assert_eq!(
            legacy.symmetric, formal.symmetric,
            "{relation}: symmetry is fixed"
        );
        assert_eq!(
            legacy.acyclic, formal.acyclic,
            "{relation}: acyclicity is fixed"
        );
        assert_eq!(formal.implies_dependency, legacy.is_definition);

        if legacy.is_definition {
            assert_eq!(formal.layer_rule, LayerRule::TargetBelowSource);
        } else {
            assert_eq!(formal.layer_rule, legacy.layer_rule);
            assert!(!formal.implies_dependency);
        }
    }
}

#[test]
fn profiles_default_differently_for_new_and_migrated_kbs() {
    assert_eq!(
        RuleProfile::default_for_new_kb(),
        RuleProfile::FormalV1,
        "a new KB starts on the stricter profile"
    );
    assert_eq!(
        RuleProfile::default_for_migration(),
        RuleProfile::LegacyV1,
        "migration must not upgrade semantics as a side effect"
    );
    assert_eq!(RuleProfile::ALL.len(), 2);
    assert_eq!(RuleProfile::FormalV1.as_str(), "formal-v1");
}

#[test]
fn layer_rules_are_strict_only_where_the_profile_says_so() {
    use SemanticLayer::{L0, L1, L2, L3};

    let at_most = LayerRule::TargetAtMostSource;
    assert!(at_most.accepts(L2, L2), "legacy allows equal layers");
    assert!(at_most.accepts(L2, L1));
    assert!(!at_most.accepts(L1, L2));

    let below = LayerRule::TargetBelowSource;
    assert!(
        !below.accepts(L2, L2),
        "formal requires a strictly shallower target"
    );
    assert!(below.accepts(L2, L1));
    assert!(!below.accepts(L0, L1));

    // L3 instances pointing at L1 are legal: layer is not a depth ordering.
    assert!(at_most.accepts(L3, L1));
    assert!(below.accepts(L3, L1));

    let unrestricted = LayerRule::Unrestricted;
    assert!(unrestricted.accepts(L0, L3));
}
