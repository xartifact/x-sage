//! Deterministic seeded graphs and add/update/retract sequences.
//!
//! A failing case must be reproducible from what it prints, so every test names its
//! seed and prints the graph and sequence on failure. The generator is a fixed
//! 64-bit LCG written here — no external RNG, no time, no environment.

mod support;

use sage_db_reference::paths::{self, Caps, RawRecords};
use sage_db_types::{
    Annotation, AssertionId, Confidence, RelationId, RuleProfile, SemanticLayer, TermId,
};
use support::Builder;

/// Small deterministic generator. Published so a printed seed is reproducible.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1))
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

const CONFIDENCES: [Confidence; 3] = [
    Confidence::Contested,
    Confidence::Hypothetical,
    Confidence::Confirmed,
];

fn confidence(index: u64) -> Confidence {
    CONFIDENCES[(index % 3) as usize]
}

/// One generated graph, kept as data so it can be printed on failure.
#[derive(Clone, Debug)]
struct Spec {
    seed: u64,
    profile: RuleProfile,
    terms: u32,
    edges: Vec<(u32, u32, RelationId, Confidence)>,
}

impl Spec {
    fn builder(&self) -> Builder {
        let mut builder = Builder::new(self.profile);
        let ids: Vec<TermId> = (0..self.terms)
            .map(|_| builder.term(SemanticLayer::L1))
            .collect();
        for (from, to, relation, confidence) in &self.edges {
            builder.edge(
                ids[*from as usize],
                *relation,
                ids[*to as usize],
                *confidence,
            );
        }
        builder
    }
}

/// Sparse random DAG over `terms` terms in the `depends-on` relation.
fn sparse(seed: u64) -> Spec {
    let mut rng = Lcg::new(seed);
    let terms = 5 + rng.below(3) as u32;
    let mut edges = Vec::new();
    let count = terms + rng.below(terms as u64) as u32;
    for _ in 0..count {
        let from = rng.below(terms as u64 - 1) as u32;
        let to = from + 1 + rng.below(terms as u64 - from as u64 - 1) as u32;
        edges.push((from, to, RelationId::DependsOn, confidence(rng.next())));
    }
    Spec {
        seed,
        profile: RuleProfile::LegacyV1,
        terms,
        edges,
    }
}

/// A diamond plus a shortcut: two same-length routes and one direct edge.
fn diamond(seed: u64) -> Spec {
    let mut rng = Lcg::new(seed);
    let terms = 4;
    let edges = vec![
        (0, 1, RelationId::DependsOn, confidence(rng.next())),
        (0, 2, RelationId::DependsOn, confidence(rng.next())),
        (1, 3, RelationId::DependsOn, confidence(rng.next())),
        (2, 3, RelationId::DependsOn, confidence(rng.next())),
    ];
    Spec {
        seed,
        profile: RuleProfile::LegacyV1,
        terms,
        edges,
    }
}

/// A single chain of `length` terms.
fn chain(seed: u64, length: u32) -> Spec {
    let mut rng = Lcg::new(seed);
    let edges = (0..length - 1)
        .map(|from| {
            (
                from,
                from + 1,
                RelationId::DependsOn,
                confidence(rng.next()),
            )
        })
        .collect();
    Spec {
        seed,
        profile: RuleProfile::LegacyV1,
        terms: length,
        edges,
    }
}

/// A bounded dense DAG: every node may point at any later node.
fn dense(seed: u64) -> Spec {
    let mut rng = Lcg::new(seed);
    let terms = 6;
    let mut edges = Vec::new();
    for from in 0..terms - 1 {
        for to in from + 1..terms {
            if rng.below(2) == 0 {
                edges.push((from, to, RelationId::DependsOn, confidence(rng.next())));
            }
        }
    }
    Spec {
        seed,
        profile: RuleProfile::LegacyV1,
        terms,
        edges,
    }
}

/// Compare saturation with enumeration for every term and every relation present.
fn assert_internal_agreement(spec: &Spec) {
    let builder = spec.builder();
    let graph = builder.build();
    let relations: Vec<RelationId> = {
        let mut relations: Vec<RelationId> = spec.edges.iter().map(|edge| edge.2).collect();
        relations.sort_unstable();
        relations.dedup();
        relations
    };
    for relation in relations {
        for source in graph.terms() {
            let result = support::solve(&graph, source.id, relation, false);
            let mut work = support::work();
            let enumeration = paths::enumerate(
                RawRecords {
                    terms: graph.terms(),
                    assertions: graph.assertions(),
                    rules: graph.rules(),
                },
                relation,
                source.id,
                Caps::DEFAULT,
                &mut work,
                &sage_db_types::RunToCompletion,
            )
            .unwrap_or_else(|error| panic!("seed {} enumeration failed: {error}", spec.seed));

            let mut targets: Vec<TermId> = enumeration
                .paths
                .iter()
                .map(|path| path.steps.last().expect("non-empty").to)
                .collect();
            targets.sort_unstable();
            targets.dedup();
            targets.retain(|target| *target != source.id);

            for target in &targets {
                let matching: Vec<&paths::Path> = enumeration
                    .paths
                    .iter()
                    .filter(|path| path.steps.last().expect("non-empty").to == *target)
                    .collect();
                let expected = matching
                    .iter()
                    .fold(Annotation::ABSENT, |acc, path| acc.join(path.annotation()));
                let settled = sage_db_reference::annotation_of(&result, *target);
                assert_eq!(
                    settled, expected,
                    "seed {}: annotation mismatch for {target:?} in {relation} from {:?}\n{spec:#?}",
                    spec.seed, source.id
                );
                let row = sage_db_reference::row_for(&result, *target).expect("row");
                assert_eq!(
                    row.disputed,
                    matching.iter().any(|path| path.disputed()),
                    "seed {}: disputed mismatch for {target:?}\n{spec:#?}",
                    spec.seed
                );
            }
            for row in &result.rows {
                assert!(
                    targets.contains(&row.to),
                    "seed {}: saturation invented {row:?}\n{spec:#?}",
                    spec.seed
                );
            }
        }
    }
}

#[test]
fn seeded_sparse_graphs_agree_with_enumeration() {
    for seed in 0..24 {
        let spec = sparse(seed);
        assert_internal_agreement(&spec);
    }
}

#[test]
fn seeded_diamonds_agree_with_enumeration() {
    for seed in 0..24 {
        let spec = diamond(seed);
        assert_internal_agreement(&spec);
    }
}

#[test]
fn seeded_chains_agree_with_enumeration() {
    for seed in 0..12 {
        for length in 2..7 {
            let spec = chain(seed, length);
            assert_internal_agreement(&spec);
        }
    }
}

#[test]
fn seeded_bounded_dense_dags_agree_with_enumeration() {
    for seed in 0..12 {
        let spec = dense(seed);
        assert_internal_agreement(&spec);
    }
}

#[test]
fn seeded_graphs_are_stable_under_assertion_permutation() {
    for seed in 0..16 {
        let spec = sparse(seed);
        let mut builder = spec.builder();
        let first = builder.build();
        let baseline: Vec<_> = first
            .terms()
            .iter()
            .map(|term| {
                support::solve(&first, term.id, RelationId::DependsOn, false).canonical_rows()
            })
            .collect();

        builder.reverse_assertions();
        let second = builder.build();
        let after: Vec<_> = second
            .terms()
            .iter()
            .map(|term| {
                support::solve(&second, term.id, RelationId::DependsOn, false).canonical_rows()
            })
            .collect();

        assert_eq!(
            baseline, after,
            "seed {seed}: assertion order changed the answer\n{spec:#?}"
        );
    }
}

#[test]
fn seeded_graphs_replay_every_proof() {
    for seed in 0..12 {
        let spec = dense(seed);
        let graph = spec.builder().build();
        for term in graph.terms() {
            let result = support::solve(&graph, term.id, RelationId::DependsOn, true);
            let arena = match result.proof.as_ref() {
                Some(arena) => arena,
                None => continue,
            };
            for row in &result.rows {
                sage_db_reference::proof::replay_expecting(
                    &graph,
                    graph.rules(),
                    arena,
                    row.proof.expect("root"),
                    sage_db_reference::Expected {
                        from: row.from,
                        relation: row.relation,
                        to: row.to,
                        annotation: row.annotation,
                    },
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "seed {}: replay failed for {row:?}: {error}\n{spec:#?}",
                        spec.seed
                    )
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Add / update / retract sequences
// ---------------------------------------------------------------------------

/// One step of a mutation sequence.
#[derive(Clone, Copy, Debug)]
enum Step {
    Add(AssertionId),
    Retract(AssertionId),
}

/// Run a sequence and return the canonical rows of each intermediate snapshot.
fn run_sequence(
    base: &Builder,
    added: &[(AssertionId, TermId, RelationId, TermId, Confidence)],
    steps: &[Step],
    source: TermId,
    relation: RelationId,
) -> Vec<Vec<sage_db_types::CanonicalRow>> {
    let mut active: Vec<AssertionId> = Vec::new();
    let mut seen: Vec<Vec<sage_db_types::CanonicalRow>> = Vec::new();

    for step in steps {
        match step {
            Step::Add(id) => active.push(*id),
            Step::Retract(id) => active.retain(|candidate| candidate != id),
        }
        let mut builder = base.clone();
        // Rebuild the endpoint set from scratch: the fixture must not depend on the
        // history of a previous snapshot.
        for (id, from, relation, to, confidence) in added {
            if active.contains(id) {
                builder.edge_id(*id, *from, *relation, *to, *confidence);
            }
        }
        let graph = builder.build();
        seen.push(support::solve(&graph, source, relation, false).canonical_rows());
    }
    seen
}

#[test]
fn a_mutation_sequence_keeps_the_expected_annotations() {
    // The base is terms only: every support arrives through the sequence, so the
    // sequence alone determines each snapshot.
    let mut base = Builder::new(RuleProfile::LegacyV1);
    let a = base.term(SemanticLayer::L1);
    let b = base.term(SemanticLayer::L1);
    let strong_id = AssertionId::new(1);
    let weak_id = AssertionId::new(2);

    let added: Vec<(AssertionId, TermId, RelationId, TermId, Confidence)> = vec![
        (
            strong_id,
            a,
            RelationId::DependsOn,
            b,
            Confidence::Confirmed,
        ),
        (
            weak_id,
            a,
            RelationId::DependsOn,
            b,
            Confidence::Hypothetical,
        ),
    ];
    let steps = [
        Step::Add(strong_id),
        Step::Add(weak_id),
        Step::Retract(strong_id),
        Step::Retract(weak_id),
    ];
    let snapshots = run_sequence(&base, &added, &steps, a, RelationId::DependsOn);

    // The sequence is printed because a failure here must be reproducible.
    let describe = |rows: &[sage_db_types::CanonicalRow]| {
        rows.iter()
            .map(|row| (row.to, row.annotation.confidence()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        describe(&snapshots[0]),
        vec![(b, Some(Confidence::Confirmed))],
        "add strong"
    );
    assert_eq!(
        describe(&snapshots[1]),
        vec![(b, Some(Confidence::Confirmed))],
        "add weak: max keeps confirmed"
    );
    assert_eq!(
        describe(&snapshots[2]),
        vec![(b, Some(Confidence::Hypothetical))],
        "retract strong: downgrade, not absent"
    );
    assert_eq!(snapshots[3], Vec::new(), "retract last: absent");
}

#[test]
fn a_mutation_sequence_over_a_chain_keeps_the_downstream_facts_consistent() {
    let mut base = Builder::new(RuleProfile::LegacyV1);
    let terms = base.terms_at(SemanticLayer::L1, 4);
    let first = AssertionId::new(1);
    let second = AssertionId::new(2);
    let third = AssertionId::new(3);

    let added = vec![
        (
            first,
            terms[0],
            RelationId::DependsOn,
            terms[1],
            Confidence::Confirmed,
        ),
        (
            second,
            terms[1],
            RelationId::DependsOn,
            terms[2],
            Confidence::Confirmed,
        ),
        (
            third,
            terms[2],
            RelationId::DependsOn,
            terms[3],
            Confidence::Confirmed,
        ),
    ];
    let steps = [
        Step::Add(first),
        Step::Add(second),
        Step::Add(third),
        Step::Retract(second),
        Step::Retract(third),
    ];
    let snapshots = run_sequence(&base, &added, &steps, terms[0], RelationId::DependsOn);

    let reachable = |rows: &[sage_db_types::CanonicalRow]| -> Vec<TermId> {
        rows.iter().map(|row| row.to).collect()
    };
    assert_eq!(reachable(&snapshots[0]), vec![terms[1]]);
    assert_eq!(reachable(&snapshots[1]), vec![terms[1], terms[2]]);
    assert_eq!(reachable(&snapshots[2]), vec![terms[1], terms[2], terms[3]]);
    assert_eq!(
        reachable(&snapshots[3]),
        vec![terms[1]],
        "dropping the middle link removes everything downstream of it"
    );
    assert_eq!(reachable(&snapshots[4]), vec![terms[1]]);
}

#[test]
fn an_update_that_merely_reraises_a_revision_does_not_change_the_answer() {
    // A revision bump with the same semantics is invisible to evaluation.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Confirmed, 1);
    let first = support::solve(&builder.build(), a, RelationId::DependsOn, false).rows;

    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Confirmed, 9);
    let second = support::solve(&builder.build(), a, RelationId::DependsOn, false).rows;

    assert_eq!(
        first.iter().map(|row| row.canonical()).collect::<Vec<_>>(),
        second.iter().map(|row| row.canonical()).collect::<Vec<_>>()
    );
    // ... but the reported revision follows the record.
    assert_eq!(second[0].strongest_support.unwrap().revision.get(), 9);
}
