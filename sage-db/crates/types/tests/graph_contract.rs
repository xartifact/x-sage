//! Base-graph construction and access contracts.
//!
//! Focus: a built graph is total (no dangling endpoints, no duplicate identities,
//! deterministic order), reverse lookups do not invent symmetry, and admission is
//! checked *before* index allocation.

use sage_db_types::{
    AssertionId, AssertionRecord, BaseGraph, Budget, BuildLimit, Confidence, Digest, EngineVersion,
    GraphBuildError, GraphDigest, KbUuid, KbVersion, RecordRevision, RelationId, RuleProfile,
    RuleSet, RulesDigest, SemanticLayer, SnapshotId, TermId, TermMeta,
};

fn rules() -> RuleSet {
    RuleSet::from_trusted_parts(
        RuleProfile::FormalV1,
        RulesDigest::new(Digest::from_bytes([1; 32])),
    )
}

fn snapshot() -> SnapshotId {
    SnapshotId {
        kb_uuid: KbUuid::from_bytes([7; 16]),
        version: KbVersion::new(3),
        rules: rules(),
        graph_digest: GraphDigest::new(Digest::from_bytes([2; 32])),
    }
}

fn term(id: u32, layer: SemanticLayer) -> TermMeta {
    TermMeta {
        id: TermId::new(id),
        layer,
    }
}

fn assertion(
    id: u128,
    from: u32,
    relation: RelationId,
    target: u32,
    confidence: Confidence,
) -> AssertionRecord {
    AssertionRecord {
        id: AssertionId::new(id),
        revision: RecordRevision::FIRST,
        from: TermId::new(from),
        relation,
        target: TermId::new(target),
        confidence,
    }
}

fn build(
    terms: Vec<TermMeta>,
    assertions: Vec<AssertionRecord>,
) -> Result<BaseGraph, GraphBuildError> {
    BaseGraph::new(snapshot(), terms, assertions, Budget::DEFAULT)
}

#[test]
fn construction_rejects_duplicate_terms() {
    let error = build(
        vec![term(1, SemanticLayer::L1), term(1, SemanticLayer::L2)],
        vec![],
    )
    .unwrap_err();
    assert_eq!(error, GraphBuildError::DuplicateTerm(TermId::new(1)));
}

#[test]
fn construction_rejects_duplicate_assertions_even_with_identical_triples() {
    // Two authors may support the same triple; they must still be distinct
    // records. Reusing an ID would merge two histories.
    let error = build(
        vec![term(1, SemanticLayer::L1), term(2, SemanticLayer::L1)],
        vec![
            assertion(9, 1, RelationId::IsA, 2, Confidence::Confirmed),
            assertion(9, 1, RelationId::IsA, 2, Confidence::Hypothetical),
        ],
    )
    .unwrap_err();
    assert_eq!(
        error,
        GraphBuildError::DuplicateAssertion(AssertionId::new(9))
    );
}

#[test]
fn duplicate_assertion_is_detected_across_distinct_endpoints() {
    // Regression: comparing only adjacent records after sorting by
    // (from, relation, target, id) misses a duplicate whose two records sit at
    // different endpoints with a third record between them in visit order.
    let error = build(
        vec![
            term(1, SemanticLayer::L1),
            term(2, SemanticLayer::L1),
            term(3, SemanticLayer::L1),
        ],
        vec![
            assertion(77, 1, RelationId::IsA, 2, Confidence::Confirmed),
            // Distinct endpoints, same identity: sorts to a different position.
            assertion(77, 3, RelationId::IsA, 1, Confidence::Confirmed),
            assertion(5, 2, RelationId::IsA, 1, Confidence::Confirmed),
        ],
    )
    .unwrap_err();
    assert_eq!(
        error,
        GraphBuildError::DuplicateAssertion(AssertionId::new(77)),
        "identity uniqueness must be checked in ID order, not visit order"
    );
}

#[test]
fn duplicate_detection_is_permutation_invariant() {
    // Same records in several input orders must all be rejected; a check that
    // depends on input order is not a check.
    let records = [
        assertion(77, 1, RelationId::IsA, 2, Confidence::Confirmed),
        assertion(77, 3, RelationId::IsA, 1, Confidence::Confirmed),
        assertion(5, 2, RelationId::IsA, 1, Confidence::Confirmed),
    ];
    let terms = [
        term(1, SemanticLayer::L1),
        term(2, SemanticLayer::L1),
        term(3, SemanticLayer::L1),
    ];

    let permutations = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    for order in permutations {
        let supplied: Vec<AssertionRecord> = order.iter().map(|i| records[*i]).collect();
        let error = build(terms.to_vec(), supplied).unwrap_err();
        assert_eq!(
            error,
            GraphBuildError::DuplicateAssertion(AssertionId::new(77)),
            "permutation {order:?} escaped duplicate detection"
        );
    }
}

#[test]
fn construction_rejects_dangling_endpoints() {
    let error = build(
        vec![term(1, SemanticLayer::L1)],
        vec![assertion(1, 1, RelationId::IsA, 99, Confidence::Confirmed)],
    )
    .unwrap_err();
    assert_eq!(
        error,
        GraphBuildError::UnknownTerm {
            term: TermId::new(99),
            assertion: AssertionId::new(1),
        }
    );
}

#[test]
fn construction_rejects_self_reference() {
    let error = build(
        vec![term(1, SemanticLayer::L1)],
        vec![assertion(
            1,
            1,
            RelationId::Enables,
            1,
            Confidence::Confirmed,
        )],
    )
    .unwrap_err();
    assert_eq!(error, GraphBuildError::SelfReference(AssertionId::new(1)));
}

#[test]
fn construction_admits_within_budget_and_rejects_beyond_it() {
    let terms = vec![term(1, SemanticLayer::L1), term(2, SemanticLayer::L1)];
    let assertions = vec![assertion(1, 1, RelationId::IsA, 2, Confidence::Confirmed)];

    // A generous budget accepts.
    BaseGraph::new(
        snapshot(),
        terms.clone(),
        assertions.clone(),
        Budget::DEFAULT,
    )
    .unwrap();

    // A starved row allowance refuses before allocating index vectors.
    let starved_rows = Budget {
        max_work_rows: 1,
        ..Budget::DEFAULT
    };
    assert_eq!(
        BaseGraph::new(snapshot(), terms.clone(), assertions.clone(), starved_rows).unwrap_err(),
        GraphBuildError::BudgetExceeded {
            limit: BuildLimit::Rows
        }
    );

    // A starved byte allowance refuses too.
    let starved_bytes = Budget {
        max_work_bytes: 1,
        ..Budget::DEFAULT
    };
    assert_eq!(
        BaseGraph::new(snapshot(), terms, assertions, starved_bytes).unwrap_err(),
        GraphBuildError::BudgetExceeded {
            limit: BuildLimit::Bytes
        }
    );
}

#[test]
fn input_order_does_not_change_access_order() {
    let forward = build(
        vec![
            term(1, SemanticLayer::L2),
            term(2, SemanticLayer::L1),
            term(3, SemanticLayer::L1),
        ],
        vec![
            assertion(11, 1, RelationId::IsA, 2, Confidence::Confirmed),
            assertion(12, 1, RelationId::IsA, 3, Confidence::Hypothetical),
            assertion(13, 1, RelationId::PartOf, 2, Confidence::Confirmed),
        ],
    )
    .unwrap();
    let reversed = build(
        vec![
            term(3, SemanticLayer::L1),
            term(2, SemanticLayer::L1),
            term(1, SemanticLayer::L2),
        ],
        vec![
            assertion(13, 1, RelationId::PartOf, 2, Confidence::Confirmed),
            assertion(12, 1, RelationId::IsA, 3, Confidence::Hypothetical),
            assertion(11, 1, RelationId::IsA, 2, Confidence::Confirmed),
        ],
    )
    .unwrap();

    let ids = |graph: &BaseGraph| -> Vec<AssertionId> {
        graph
            .assertions_from(TermId::new(1))
            .iter()
            .map(|a| a.id)
            .collect()
    };
    assert_eq!(ids(&forward), ids(&reversed));
    assert_eq!(
        ids(&forward),
        vec![
            AssertionId::new(11),
            AssertionId::new(12),
            AssertionId::new(13)
        ],
        "visit order is (relation_id, target, assertion_id)"
    );
    assert_eq!(
        forward.terms().iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![TermId::new(1), TermId::new(2), TermId::new(3)]
    );
}

#[test]
fn reverse_lookup_preserves_authored_direction() {
    let graph = build(
        vec![
            term(1, SemanticLayer::L1),
            term(2, SemanticLayer::L2),
            term(3, SemanticLayer::L0),
        ],
        vec![
            assertion(1, 1, RelationId::Contradicts, 2, Confidence::Contested),
            assertion(2, 3, RelationId::Enables, 2, Confidence::Confirmed),
        ],
    )
    .unwrap();

    let incoming: Vec<AssertionRecord> = graph
        .assertions_to(TermId::new(2))
        .iter()
        .copied()
        .collect();
    assert_eq!(incoming.len(), 2);
    assert_eq!(graph.in_degree(TermId::new(2)), 2);
    assert_eq!(graph.out_degree(TermId::new(2)), 0);

    for found in &incoming {
        assert_eq!(
            found.target,
            TermId::new(2),
            "a symmetric relation must not be rewritten into a reverse assertion"
        );
    }
    assert_eq!(
        incoming.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![AssertionId::new(1), AssertionId::new(2)]
    );

    assert!(graph.assertions_from(TermId::new(2)).is_empty());
    assert!(graph.assertions_to(TermId::new(3)).is_empty());
}

#[test]
fn assertion_lookup_is_by_identity_not_position() {
    let graph = build(
        vec![term(1, SemanticLayer::L1), term(2, SemanticLayer::L1)],
        vec![
            assertion(50, 2, RelationId::IsA, 1, Confidence::Confirmed),
            assertion(10, 1, RelationId::IsA, 2, Confidence::Confirmed),
        ],
    )
    .unwrap();

    assert_eq!(
        graph.assertion(AssertionId::new(10)).unwrap().from,
        TermId::new(1)
    );
    assert_eq!(
        graph.assertion(AssertionId::new(50)).unwrap().from,
        TermId::new(2)
    );
    assert!(graph.assertion(AssertionId::new(11)).is_none());
}

#[test]
fn degree_accessors_count_only_real_edges() {
    let graph = build(
        vec![term(1, SemanticLayer::L1), term(2, SemanticLayer::L1)],
        vec![
            assertion(1, 1, RelationId::DependsOn, 2, Confidence::Confirmed),
            assertion(2, 1, RelationId::PartOf, 2, Confidence::Confirmed),
        ],
    )
    .unwrap();

    assert_eq!(graph.out_degree(TermId::new(1)), 2);
    assert_eq!(graph.out_degree(TermId::new(2)), 0);
    assert_eq!(graph.in_degree(TermId::new(2)), 2);
    // An unknown term behaves like an isolated one, never like an error at this
    // layer: existence was validated at construction.
    assert_eq!(graph.out_degree(TermId::new(42)), 0);
    assert!(graph.term(TermId::new(42)).is_none());
    assert_eq!(graph.term_count(), 2);
    assert_eq!(graph.assertion_count(), 2);
}

#[test]
fn snapshot_carries_the_rule_profile_not_only_a_digest() {
    let graph = build(vec![term(1, SemanticLayer::L1)], vec![]).unwrap();
    assert_eq!(graph.snapshot().version, KbVersion::new(3));
    assert_eq!(graph.snapshot(), snapshot());
    assert_eq!(graph.rules().profile(), RuleProfile::FormalV1);
    assert_eq!(
        graph.rules().digest(),
        RulesDigest::new(Digest::from_bytes([1; 32]))
    );

    // A digest alone cannot distinguish the profiles; the RuleSet can.
    let legacy = RuleSet::from_trusted_parts(
        RuleProfile::LegacyV1,
        RulesDigest::new(Digest::from_bytes([1; 32])),
    );
    assert_ne!(legacy.profile(), graph.rules().profile());
    assert_eq!(
        legacy.digest(),
        graph.rules().digest(),
        "equal digests with different profiles are exactly why RuleSet carries both"
    );
    assert!(!legacy.implies_dependency(RelationId::IsA));
    assert!(graph.rules().implies_dependency(RelationId::IsA));
}

#[test]
fn snapshot_and_result_identities_stay_distinct_types() {
    // Version, revision, rules digest and engine version must not be
    // interchangeable: same numeric shape, different meaning.
    let snapshot = snapshot();
    assert_eq!(snapshot.version.get(), 3);
    assert_eq!(snapshot.rules.digest().as_digest().as_bytes()[0], 1);
    assert_eq!(EngineVersion::new(3).get(), 3);
}

#[test]
fn owned_bytes_are_measured_and_within_the_admitted_bound() {
    // Owned bytes must come from real capacities. The admitted figure is a
    // conservative upper bound (groupings charged at term capacity), so for a
    // sparse graph it is strictly larger.
    use core::mem::size_of;

    let terms: Vec<TermMeta> = (0..8u32).map(|id| term(id, SemanticLayer::L1)).collect();
    let assertions: Vec<AssertionRecord> = (0..6u128)
        .map(|id| {
            let from = (id as u32) % 3;
            let target = 4 + (id as u32) % 4;
            assertion(id + 1, from, RelationId::IsA, target, Confidence::Confirmed)
        })
        .collect();
    let record_count = assertions.len();

    let graph = BaseGraph::new(snapshot(), terms, assertions, Budget::DEFAULT).unwrap();

    assert!(graph.owned_slots() > 0, "records and indexes own slots");
    assert!(graph.owned_bytes() > 0);

    // Measured <= admitted: the bound may over-charge, never under-charge.
    assert!(
        graph.owned_bytes() <= graph.admitted_bound_bytes(),
        "measured {} must not exceed the admitted bound {}",
        graph.owned_bytes(),
        graph.admitted_bound_bytes()
    );
    // For this sparse input the bound is strictly larger, which is the whole
    // reason the two figures are reported separately.
    assert!(
        graph.admitted_bound_bytes() > graph.owned_bytes(),
        "a graph with fewer distinct endpoints than terms must show a strict gap"
    );
    // Inputs were moved in, so they are part of the owned figure.
    assert!(
        graph.owned_bytes() >= (record_count as u64) * (size_of::<AssertionRecord>() as u64),
        "input records stay resident, so they must be counted"
    );
}

#[test]
fn an_isolated_term_graph_reports_less_than_its_admission_bound() {
    // No assertions at all: every grouping is empty, so owned bytes are just the
    // term vector, while the bound still reserves two grouping entries per term.
    let terms: Vec<TermMeta> = (0..4u32).map(|id| term(id, SemanticLayer::L1)).collect();
    let graph = BaseGraph::new(snapshot(), terms, Vec::new(), Budget::DEFAULT).unwrap();

    assert_eq!(graph.assertion_count(), 0);
    assert!(
        graph.owned_bytes() >= 4 * (core::mem::size_of::<TermMeta>() as u64),
        "four terms are resident"
    );
    assert!(
        graph.admitted_bound_bytes() > graph.owned_bytes(),
        "with no groupings the bound is strictly conservative"
    );
}

#[test]
fn a_budget_below_the_resident_figure_is_refused_before_building() {
    // Exactly one byte short of the real requirement must fail; the equal figure
    // must pass. `clone()` is avoided because it can produce a different capacity
    // than the original, and capacity is what the charge is computed from.
    use core::mem::size_of;

    let grouping_entry = (size_of::<TermId>() + size_of::<core::ops::Range<u32>>()) as u64;
    let inputs = || {
        (
            vec![term(1, SemanticLayer::L1), term(2, SemanticLayer::L1)],
            vec![assertion(1, 1, RelationId::IsA, 2, Confidence::Confirmed)],
        )
    };
    let charge = |term_capacity: usize, record_capacity: usize| {
        (term_capacity as u64) * (size_of::<TermMeta>() as u64)
            + (record_capacity as u64) * (size_of::<AssertionRecord>() as u64)
            + (record_capacity as u64) * 2 * 4
            + (term_capacity as u64) * 2 * grouping_entry
    };

    let (terms, assertions) = inputs();
    let (term_capacity, record_capacity) = (terms.capacity(), assertions.capacity());
    let exact = charge(term_capacity, record_capacity);

    let short = Budget {
        max_work_bytes: exact - 1,
        ..Budget::DEFAULT
    };
    assert_eq!(
        BaseGraph::new(snapshot(), terms, assertions, short).unwrap_err(),
        GraphBuildError::BudgetExceeded {
            limit: BuildLimit::Bytes
        }
    );

    let (terms, assertions) = inputs();
    assert_eq!(
        charge(terms.capacity(), assertions.capacity()),
        exact,
        "the fixture must build vectors of reproducible capacity"
    );
    let enough = Budget {
        max_work_bytes: exact,
        ..Budget::DEFAULT
    };
    let graph = BaseGraph::new(snapshot(), terms, assertions, enough).unwrap();
    assert_eq!(graph.admitted_bound_bytes(), exact);
}

#[test]
fn build_leaves_the_input_order_irrelevant_and_allocates_nothing_extra() {
    // In-place unstable sorts are only safe because every key is a total order
    // over distinct records. Mixed input must still produce identical access.
    let mut forward = Vec::new();
    let mut backward = Vec::new();
    for id in 0..12u128 {
        let from = (id as u32) % 4;
        let target = 4 + (id as u32) % 4;
        forward.push(assertion(
            id + 1,
            from,
            RelationId::IsA,
            target,
            Confidence::Confirmed,
        ));
    }
    backward.extend(forward.iter().rev().copied());

    let terms: Vec<TermMeta> = (0..8u32).map(|id| term(id, SemanticLayer::L1)).collect();
    let left = BaseGraph::new(snapshot(), terms.clone(), forward, Budget::DEFAULT).unwrap();
    let right = BaseGraph::new(snapshot(), terms, backward, Budget::DEFAULT).unwrap();

    assert_eq!(left.assertions(), right.assertions());
    for id in 0..8u32 {
        let term_id = TermId::new(id);
        assert_eq!(
            left.assertions_to(term_id)
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            right
                .assertions_to(term_id)
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>()
        );
    }
    // Capacities can differ between two independently built vectors, so compare
    // the semantic result, not the charge.
    assert_eq!(left.term_count(), right.term_count());
    assert_eq!(left.assertion_count(), right.assertion_count());
}

#[test]
fn owned_bytes_sums_each_grouping_vector_at_its_own_element_size() {
    // Regression: `slots()` counts *entries* (keys + ranges), so multiplying that
    // count by the per-pair size charged each grouping twice. The isolated-term
    // case could not catch it because both groupings were empty and 0 * x == 0.
    //
    // This input gives both adjacency directions non-empty groupings, and asserts
    // the exact byte total rather than merely `owned <= bound`.
    use core::mem::size_of;
    use core::ops::Range;

    let terms: Vec<TermMeta> = (0..4u32).map(|id| term(id, SemanticLayer::L1)).collect();
    // Term ids are 0..=3. Three distinct `from` values (0,1,2) and one shared
    // `target` (3) give both groupings a non-empty, asymmetric key count.
    let assertions = vec![
        assertion(1, 0, RelationId::IsA, 3, Confidence::Confirmed),
        assertion(2, 1, RelationId::IsA, 3, Confidence::Confirmed),
        assertion(3, 2, RelationId::IsA, 3, Confidence::Confirmed),
        assertion(4, 0, RelationId::PartOf, 3, Confidence::Confirmed),
    ];
    let term_capacity = terms.capacity();
    let record_capacity = assertions.capacity();
    let record_count = assertions.len();

    let graph = BaseGraph::new(snapshot(), terms, assertions, Budget::DEFAULT).unwrap();

    // Distinct endpoints, counted independently of the implementation.
    let outgoing_keys = 3; // authored `from` values {0, 1, 2}
    let incoming_keys = 1; // authored `target` value {3}

    // Every derived vector is reserved with `try_reserve_exact`, so its capacity is
    // the requested count. Spelling the sum out per vector — keys at TermId size,
    // ranges at Range<u32> size — is what the fix must produce; the previous
    // `(keys + ranges) * (TermId + Range)` double-counted each grouping.
    let grouping_bytes = ((outgoing_keys + incoming_keys) as u64)
        * ((size_of::<TermId>() + size_of::<Range<u32>>()) as u64);
    let expected = (term_capacity as u64) * (size_of::<TermMeta>() as u64)
        + (record_capacity as u64) * (size_of::<AssertionRecord>() as u64)
        + (record_count as u64) * 4 * 2 // incoming_positions + by_id, one u32 each
        + grouping_bytes;

    assert_eq!(
        graph.owned_bytes(),
        expected,
        "owned_bytes must charge each grouping vector at its own element size"
    );

    // The buggy formula would have produced exactly twice the grouping bytes, so
    // prove the two differ: this is the assertion the old code failed.
    let doubly_charged = expected + grouping_bytes;
    assert_ne!(
        graph.owned_bytes(),
        doubly_charged,
        "per-pair charging of a slot count is the defect; it must not reappear"
    );
    assert!(grouping_bytes > 0, "both groupings must be non-empty here");
    assert!(graph.owned_bytes() < doubly_charged);
}
