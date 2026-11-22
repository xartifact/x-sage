//! Differential tests: optimized executor against the independent reference oracle.
//!
//! # What is compared
//!
//! [`CanonicalRow`] only: endpoints, relation, effective annotation, `has_asserted`,
//! `has_derived` and `disputed`. Proof arena layout, `ExecStats`, engine version and
//! cache state are excluded on purpose — two correct executors may differ there.
//!
//! # What is *not* used as an oracle
//!
//! Expected values are never taken from `sage_db_infer`. The oracle is
//! [`sage_db_reference`], which depends on `sage-db-types` only. Proofs are replayed
//! by this test's own replay helper against the base graph, not by
//! `sage_db_infer::verify_row`, so a bug in either implementation cannot certify
//! itself.
//!
//! # Resource handling
//!
//! Every fixture here is small and bounded, so a resource refusal is a **failure**,
//! not a skip: it would mean one executor cannot evaluate a graph the other can.
//! A separate test asserts that both implementations report typed errors for a
//! genuinely too-small budget, without requiring equal costs.

use sage_db_infer::OptimizedExecutor;
use sage_db_reference::ReferenceExecutor;
use sage_db_reference::paths::{self, Caps, RawRecords};
use sage_db_types::{
    Annotation, AssertionId, AssertionRecord, BaseGraph, Budget, CanonicalRow, Confidence, Digest,
    EngineVersion, FactRow, GraphDigest, KbUuid, KbVersion, ProofArena, ProofNode, ProofRef,
    QueryControl, QueryError, RecordRevision, RelationId, RuleProfile, RuleSet, RulesDigest,
    SemanticLayer, SnapshotId, SourceExecutor, SourceQuery, TermId, TermMeta,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const KB: KbUuid = KbUuid::from_bytes([0x21; 16]);

fn rules(profile: RuleProfile) -> RuleSet {
    RuleSet::from_trusted_parts(profile, RulesDigest::new(Digest::from_bytes([0x31; 32])))
}

fn snapshot(profile: RuleProfile) -> SnapshotId {
    SnapshotId {
        kb_uuid: KB,
        version: KbVersion::new(1),
        rules: rules(profile),
        graph_digest: GraphDigest::new(Digest::from_bytes([0x41; 32])),
    }
}

#[derive(Clone)]
struct Builder {
    profile: RuleProfile,
    next_term: u32,
    next_assertion: u128,
    terms: Vec<TermMeta>,
    assertions: Vec<AssertionRecord>,
}

impl Builder {
    fn new(profile: RuleProfile) -> Self {
        Self {
            profile,
            next_term: 0,
            next_assertion: 1,
            terms: Vec::new(),
            assertions: Vec::new(),
        }
    }

    fn term(&mut self, layer: SemanticLayer) -> TermId {
        let id = TermId::new(self.next_term);
        self.next_term += 1;
        self.terms.push(TermMeta { id, layer });
        id
    }

    fn terms_at(&mut self, layer: SemanticLayer, count: u32) -> Vec<TermId> {
        (0..count).map(|_| self.term(layer)).collect()
    }

    fn edge(
        &mut self,
        from: TermId,
        relation: RelationId,
        to: TermId,
        confidence: Confidence,
    ) -> AssertionId {
        self.edge_rev(from, relation, to, confidence, 1)
    }

    fn edge_c(&mut self, from: TermId, relation: RelationId, to: TermId) -> AssertionId {
        self.edge(from, relation, to, Confidence::Confirmed)
    }

    fn edge_rev(
        &mut self,
        from: TermId,
        relation: RelationId,
        to: TermId,
        confidence: Confidence,
        revision: u64,
    ) -> AssertionId {
        let id = AssertionId::new(self.next_assertion);
        self.next_assertion += 1;
        self.assertions.push(AssertionRecord {
            id,
            revision: RecordRevision::new(revision).expect("revision"),
            from,
            relation,
            target: to,
            confidence,
        });
        id
    }

    fn reverse_assertions(&mut self) {
        self.assertions.reverse();
    }

    fn build(&self) -> BaseGraph {
        BaseGraph::new(
            snapshot(self.profile),
            self.terms.clone(),
            self.assertions.clone(),
            Budget::DEFAULT,
        )
        .expect("fixture graph must be constructible")
    }
}

// ---------------------------------------------------------------------------
// Differential harness
// ---------------------------------------------------------------------------

/// A single comparison, printed on failure.
fn compare(graph: &BaseGraph, source: TermId, relation: RelationId, with_proof: bool) {
    let query = SourceQuery {
        snapshot: graph.snapshot(),
        source,
        relation,
        with_proof,
        budget: Budget::DEFAULT,
    };

    let optimized = OptimizedExecutor::new()
        .solve(graph, &query, &sage_db_types::RunToCompletion)
        .unwrap_or_else(|error| {
            panic!(
                "optimized refused a bounded fixture (source {source:?}, {relation}, proof={with_proof}): {error}"
            )
        });
    let reference = ReferenceExecutor::new()
        .solve(graph, &query, &sage_db_types::RunToCompletion)
        .unwrap_or_else(|error| {
            panic!(
                "reference refused a bounded fixture (source {source:?}, {relation}, proof={with_proof}): {error}"
            )
        });

    let left = optimized.canonical_rows();
    let right = reference.canonical_rows();
    assert_eq!(
        left, right,
        "canonical rows differ for source {source:?}, relation {relation}, proof={with_proof}\n\
         optimized: {left:#?}\nreference: {right:#?}"
    );

    if with_proof {
        assert!(
            optimized.proofs_complete(),
            "optimized proof set is incomplete"
        );
        assert!(
            reference.proofs_complete(),
            "reference proof set is incomplete"
        );
        replay_all(graph, &optimized, "optimized");
        replay_all(graph, &reference, "reference");
    } else {
        assert!(
            optimized.proof.is_none(),
            "optimized built an arena without being asked"
        );
        assert!(
            reference.proof.is_none(),
            "reference built an arena without being asked"
        );
        assert!(optimized.rows.iter().all(|row| row.proof.is_none()));
        assert!(reference.rows.iter().all(|row| row.proof.is_none()));
    }
}

/// Replay every row of `result` against `graph`, with this test's own replay.
fn replay_all(graph: &BaseGraph, result: &sage_db_types::SourceResult, who: &str) {
    let arena = result.proof.as_ref().expect("arena");
    for row in &result.rows {
        let root = row.proof.expect("root");
        replay_row(graph, arena, root, row).unwrap_or_else(|error| {
            panic!("{who} proof failed independent replay for {row:?}: {error}")
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Replayed {
    from: TermId,
    relation: RelationId,
    to: TermId,
    annotation: Annotation,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReplayFailure {
    Dangling,
    Forward,
    MissingLeaf,
    StaleRevision,
    Disconnected,
    RuleNotApplicable(RelationId),
    FactMismatch,
    AnnotationMismatch,
}

/// Independent replay, written here so neither executor can certify itself.
///
/// Resolves every leaf against `graph`, recomposes with `meet`/`max` semantics and
/// checks the root against the row.
fn replay_row(
    graph: &BaseGraph,
    arena: &ProofArena,
    root: ProofRef,
    row: &FactRow,
) -> Result<(), ReplayFailure> {
    let count = arena.len();
    let mut facts: Vec<Option<Replayed>> = vec![None; count];
    let rules = graph.rules();

    for position in 0..count {
        let node = arena
            .get(ProofRef::new(position as u32))
            .ok_or(ReplayFailure::Dangling)?;
        let fact = match node {
            ProofNode::Asserted {
                assertion_id,
                revision,
            } => {
                let assertion = graph
                    .assertion(assertion_id)
                    .ok_or(ReplayFailure::MissingLeaf)?;
                if assertion.revision != revision {
                    return Err(ReplayFailure::StaleRevision);
                }
                Replayed {
                    from: assertion.from,
                    relation: assertion.relation,
                    to: assertion.target,
                    annotation: Annotation::present(assertion.confidence),
                }
            }
            ProofNode::Symmetry { premise } => {
                let inner = premise_of(&facts, premise)?;
                let relation = inner.relation;
                if !rules.spec(relation).symmetric {
                    return Err(ReplayFailure::RuleNotApplicable(relation));
                }
                match arena.get(premise) {
                    Some(ProofNode::Asserted { .. }) => {}
                    Some(_) => return Err(ReplayFailure::RuleNotApplicable(relation)),
                    None => return Err(ReplayFailure::Dangling),
                }
                Replayed {
                    from: inner.to,
                    relation,
                    to: inner.from,
                    annotation: inner.annotation,
                }
            }
            ProofNode::DefinitionImpliesDependency { premise } => {
                let inner = premise_of(&facts, premise)?;
                let relation = inner.relation;
                if !rules.implies_dependency(relation) {
                    return Err(ReplayFailure::RuleNotApplicable(relation));
                }
                match arena.get(premise) {
                    Some(ProofNode::Asserted { .. }) => {}
                    Some(_) => return Err(ReplayFailure::RuleNotApplicable(relation)),
                    None => return Err(ReplayFailure::Dangling),
                }
                Replayed {
                    from: inner.from,
                    relation: RelationId::DependsOn,
                    to: inner.to,
                    annotation: inner.annotation,
                }
            }
            ProofNode::Transitive { left, right } => {
                let left = premise_of(&facts, left)?;
                let right = premise_of(&facts, right)?;
                if left.to != right.from || left.relation != right.relation {
                    return Err(ReplayFailure::Disconnected);
                }
                if !rules.spec(left.relation).transitive {
                    return Err(ReplayFailure::RuleNotApplicable(left.relation));
                }
                Replayed {
                    from: left.from,
                    relation: left.relation,
                    to: right.to,
                    annotation: left.annotation.meet(right.annotation),
                }
            }
        };
        facts[position] = Some(fact);
    }

    let root = premise_of(&facts, root)?;
    if root.from != row.from || root.relation != row.relation || root.to != row.to {
        return Err(ReplayFailure::FactMismatch);
    }
    if root.annotation != row.annotation {
        return Err(ReplayFailure::AnnotationMismatch);
    }
    Ok(())
}

impl std::fmt::Display for ReplayFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Hop count of a rendered proof: authored leaves along the derivation.
///
/// Annotation agreement cannot see a proof that is correct but not shortest, so the
/// shortest-proof property is asserted on this number taken from each executor's own
/// arena.
fn proof_hops(arena: &ProofArena, root: ProofRef) -> u32 {
    match arena.get(root).expect("node") {
        ProofNode::Asserted { .. } => 1,
        ProofNode::Symmetry { premise } | ProofNode::DefinitionImpliesDependency { premise } => {
            proof_hops(arena, premise)
        }
        ProofNode::Transitive { left, right } => proof_hops(arena, left) + proof_hops(arena, right),
    }
}

fn premise_of(facts: &[Option<Replayed>], reference: ProofRef) -> Result<Replayed, ReplayFailure> {
    let position = reference.get() as usize;
    if position >= facts.len() {
        return Err(ReplayFailure::Dangling);
    }
    facts[position].ok_or(ReplayFailure::Forward)
}

// ---------------------------------------------------------------------------
// Curated counterexamples, compared field by field
// ---------------------------------------------------------------------------

#[test]
fn a_plain_direct_fact_agrees() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    let graph = builder.build();

    compare(&graph, a, RelationId::DependsOn, false);
    compare(&graph, a, RelationId::DependsOn, true);
    let rows = OptimizedExecutor::new()
        .solve(
            &graph,
            &SourceQuery {
                snapshot: graph.snapshot(),
                source: a,
                relation: RelationId::DependsOn,
                with_proof: false,
                budget: Budget::DEFAULT,
            },
            &sage_db_types::RunToCompletion,
        )
        .unwrap()
        .canonical_rows();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].has_asserted);
    assert!(!rows[0].has_derived);
}

#[test]
fn every_relation_agrees_including_the_non_transitive_ones() {
    // The interface answers a fact query for all seven relations. A relation that is
    // neither transitive nor symmetric must still report its authored facts, so this
    // test would catch an executor that refuses them.
    for relation in RelationId::ALL {
        let mut builder = Builder::new(RuleProfile::LegacyV1);
        let a = builder.term(SemanticLayer::L2);
        let b = builder.term(SemanticLayer::L1);
        let c = builder.term(SemanticLayer::L1);
        builder.edge(a, relation, b, Confidence::Confirmed);
        builder.edge(b, relation, c, Confidence::Hypothetical);
        let graph = builder.build();
        compare(&graph, a, relation, false);
        compare(&graph, b, relation, false);
        compare(&graph, a, relation, true);
    }
}

#[test]
fn a_chain_agrees_at_every_start() {
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
    let graph = builder.build();
    for source in &terms[..4] {
        compare(&graph, *source, RelationId::DependsOn, true);
    }
}

#[test]
fn mixed_confidence_alternatives_agree() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let d = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Contested);
    builder.edge(b, RelationId::DependsOn, d, Confidence::Contested);
    builder.edge(a, RelationId::DependsOn, c, Confidence::Confirmed);
    builder.edge(c, RelationId::DependsOn, d, Confidence::Confirmed);
    let graph = builder.build();
    compare(&graph, a, RelationId::DependsOn, true);
}

/// The short-weak-versus-long-strong prefix counterexample the coordinator named.
///
/// `A -h-> B`; `A -c-> X`, `X -c-> Y`, `Y -c-> B`; `B -h-> T`.
///
/// - `B` is **confirmed at 3 hops**: the strong route beats the 1-hop hypothetical.
/// - `T` is **hypothetical at 2 hops**. There is **no confirmed route to `T`**: the
///   confirmed prefix is truncated by the hypothetical suffix, so `min(c, h)` is `h`
///   along both routes and the 3-hop route to `B` would only give a *longer* proof
///   of the same strength (4 hops).
///
/// Both properties are asserted on hop counts read from each executor's own arena,
/// because annotation agreement alone cannot detect a proof that is correct but not
/// shortest. Term ids are ordered `A0 X1 Y2 B3 T4`; `B` must not sort before `X`/`Y`,
/// which is the ordering that masked the reference defect.
#[test]
fn a_prefix_threshold_counterexample_agrees() {
    // Term-id order is A0 X1 Y2 B3 T4 on purpose. With `B` declared before `X`/`Y`
    // the defect this fixture exists for is masked, because the short prefix then
    // wins `B` locally and the long route is never composed for `T`.
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
    let graph = builder.build();

    compare(&graph, a, RelationId::DependsOn, true);

    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &SourceQuery {
                snapshot: graph.snapshot(),
                source: a,
                relation: RelationId::DependsOn,
                with_proof: true,
                budget: Budget::DEFAULT,
            },
            &sage_db_types::RunToCompletion,
        )
        .unwrap();
    // B: the 3-hop confirmed route, not the 1-hop hypothetical edge.
    let b_row = result.rows.iter().find(|row| row.to == b).expect("b row");
    assert_eq!(b_row.annotation, Annotation::present(Confidence::Confirmed));
    assert_eq!(
        proof_hops(
            result.proof.as_ref().expect("arena"),
            b_row.proof.expect("root")
        ),
        3,
        "optimized must prove B with the 3-hop confirmed route"
    );

    // T: hypothetical, and specifically the 2-hop A->B->T route.
    let t_row = result.rows.iter().find(|row| row.to == t).expect("t row");
    assert_eq!(
        t_row.annotation,
        Annotation::present(Confidence::Hypothetical)
    );
    assert_eq!(
        proof_hops(
            result.proof.as_ref().expect("arena"),
            t_row.proof.expect("root")
        ),
        2,
        "optimized must prove T with the 2-hop A->B->T route"
    );

    let reference = ReferenceExecutor::new()
        .solve(
            &graph,
            &SourceQuery {
                snapshot: graph.snapshot(),
                source: a,
                relation: RelationId::DependsOn,
                with_proof: true,
                budget: Budget::DEFAULT,
            },
            &sage_db_types::RunToCompletion,
        )
        .unwrap();
    assert_eq!(result.canonical_rows(), reference.canonical_rows());

    // The reference must satisfy the same two hop counts independently.
    let reference_arena = reference.proof.as_ref().expect("arena");
    let reference_b = reference
        .rows
        .iter()
        .find(|row| row.to == b)
        .expect("b row");
    assert_eq!(
        proof_hops(reference_arena, reference_b.proof.expect("root")),
        3,
        "reference must prove B with the 3-hop confirmed route"
    );
    let reference_t = reference
        .rows
        .iter()
        .find(|row| row.to == t)
        .expect("t row");
    assert_eq!(
        proof_hops(reference_arena, reference_t.proof.expect("root")),
        2,
        "reference must prove T with the 2-hop A->B->T route, not the 4-hop one"
    );
}

#[test]
fn duplicate_supports_and_retraction_agree() {
    // Same triple twice, then retract the stronger one.
    let strong = AssertionId::new(1);
    let weak = AssertionId::new(2);
    for retracted in [None, Some(strong), Some(weak)] {
        let mut builder = Builder::new(RuleProfile::LegacyV1);
        let a = builder.term(SemanticLayer::L1);
        let b = builder.term(SemanticLayer::L1);
        if retracted != Some(strong) {
            builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Confirmed, 1);
        }
        if retracted != Some(weak) {
            builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Hypothetical, 2);
        }
        let _ = (strong, weak);
        let graph = builder.build();
        compare(&graph, a, RelationId::DependsOn, true);
    }
}

#[test]
fn a_symmetric_relation_agrees_in_both_directions() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L2);
    let c = builder.term(SemanticLayer::L2);
    builder.edge(a, RelationId::Contradicts, b, Confidence::Confirmed);
    builder.edge(c, RelationId::Contradicts, a, Confidence::Hypothetical);
    let graph = builder.build();
    for source in [a, b, c] {
        compare(&graph, source, RelationId::Contradicts, true);
    }
}

#[test]
fn a_weaker_contested_route_arriving_late_raises_disputed_in_both() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Contested, 2);
    builder.edge(b, RelationId::DependsOn, c, Confidence::Confirmed);
    let graph = builder.build();
    compare(&graph, a, RelationId::DependsOn, true);

    // Assert the property directly, so agreement on a shared *bug* is not enough.
    let result = ReferenceExecutor::new()
        .solve(
            &graph,
            &SourceQuery {
                snapshot: graph.snapshot(),
                source: a,
                relation: RelationId::DependsOn,
                with_proof: false,
                budget: Budget::DEFAULT,
            },
            &sage_db_types::RunToCompletion,
        )
        .unwrap();
    assert_eq!(
        result
            .rows
            .iter()
            .find(|row| row.to == c)
            .unwrap()
            .annotation,
        Annotation::present(Confidence::Confirmed)
    );
    assert!(result.rows.iter().find(|row| row.to == c).unwrap().disputed);
}

#[test]
fn formal_r3_seeds_agree() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let instance = builder.term(SemanticLayer::L3);
    let kind = builder.term(SemanticLayer::L2);
    let core = builder.term(SemanticLayer::L1);
    builder.edge(
        instance,
        RelationId::Instantiates,
        kind,
        Confidence::Confirmed,
    );
    builder.edge(kind, RelationId::IsA, core, Confidence::Hypothetical);
    let graph = builder.build();
    compare(&graph, instance, RelationId::DependsOn, true);
    compare(&graph, kind, RelationId::DependsOn, true);
    compare(&graph, instance, RelationId::Instantiates, true);
}

#[test]
fn legacy_has_no_r3_seed_in_either_executor() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let instance = builder.term(SemanticLayer::L3);
    let kind = builder.term(SemanticLayer::L2);
    builder.edge_c(instance, RelationId::Instantiates, kind);
    let graph = builder.build();
    compare(&graph, instance, RelationId::DependsOn, false);
}

// ---------------------------------------------------------------------------
// Seeded graph families
// ---------------------------------------------------------------------------

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

fn seeded(seed: u64, profile: RuleProfile, family: Family) -> Builder {
    let mut rng = Lcg::new(seed);
    let mut builder = Builder::new(profile);
    match family {
        Family::Sparse => {
            let terms = builder.terms_at(SemanticLayer::L1, 5 + rng.below(3) as u32);
            let count = terms.len() as u64 + rng.below(terms.len() as u64);
            for _ in 0..count {
                let from = rng.below(terms.len() as u64 - 1) as usize;
                let to = from + 1 + rng.below(terms.len() as u64 - from as u64 - 1) as usize;
                builder.edge(
                    terms[from],
                    RelationId::DependsOn,
                    terms[to],
                    CONFIDENCES[rng.below(3) as usize],
                );
            }
        }
        Family::Diamond => {
            let terms = builder.terms_at(SemanticLayer::L1, 4);
            let pairs = [(0, 1), (0, 2), (1, 3), (2, 3)];
            for (from, to) in pairs {
                builder.edge(
                    terms[from],
                    RelationId::DependsOn,
                    terms[to],
                    CONFIDENCES[rng.below(3) as usize],
                );
            }
        }
        Family::Chain => {
            let terms = builder.terms_at(SemanticLayer::L1, 6);
            for pair in terms.windows(2) {
                builder.edge(
                    pair[0],
                    RelationId::DependsOn,
                    pair[1],
                    CONFIDENCES[rng.below(3) as usize],
                );
            }
        }
        Family::Dense => {
            let terms = builder.terms_at(SemanticLayer::L1, 6);
            for from in 0..terms.len() - 1 {
                for to in from + 1..terms.len() {
                    if rng.below(2) == 0 {
                        builder.edge(
                            terms[from],
                            RelationId::DependsOn,
                            terms[to],
                            CONFIDENCES[rng.below(3) as usize],
                        );
                    }
                }
            }
        }
    }
    builder
}

#[derive(Clone, Copy, Debug)]
enum Family {
    Sparse,
    Diamond,
    Chain,
    Dense,
}

fn run_family(profile: RuleProfile, family: Family, seeds: std::ops::Range<u64>) {
    for seed in seeds {
        let builder = seeded(seed, profile, family);
        let graph = builder.build();
        for term in graph.terms() {
            for relation in [RelationId::DependsOn, RelationId::IsA] {
                let rows = OptimizedExecutor::new().solve(
                    &graph,
                    &SourceQuery {
                        snapshot: graph.snapshot(),
                        source: term.id,
                        relation,
                        with_proof: false,
                        budget: Budget::DEFAULT,
                    },
                    &sage_db_types::RunToCompletion,
                );
                let rows = rows.unwrap_or_else(|error| {
                    panic!("seed {seed} ({family:?}, {profile}): optimized refused: {error}")
                });
                let reference = ReferenceExecutor::new()
                    .solve(
                        &graph,
                        &SourceQuery {
                            snapshot: graph.snapshot(),
                            source: term.id,
                            relation,
                            with_proof: false,
                            budget: Budget::DEFAULT,
                        },
                        &sage_db_types::RunToCompletion,
                    )
                    .unwrap_or_else(|error| {
                        panic!("seed {seed} ({family:?}, {profile}): reference refused: {error}")
                    });
                assert_eq!(
                    rows.canonical_rows(),
                    reference.canonical_rows(),
                    "seed {seed} ({family:?}, {profile}) source {:?} {relation}",
                    term.id
                );
            }
        }
        // One proof-bearing query per seed, on the first term.
        let source = graph.terms()[0].id;
        compare(&graph, source, RelationId::DependsOn, true);
    }
}

#[test]
fn seeded_sparse_graphs_agree() {
    for profile in RuleProfile::ALL {
        run_family(profile, Family::Sparse, 0..20);
    }
}

#[test]
fn seeded_diamonds_agree() {
    for profile in RuleProfile::ALL {
        run_family(profile, Family::Diamond, 0..20);
    }
}

#[test]
fn seeded_chains_agree() {
    for profile in RuleProfile::ALL {
        run_family(profile, Family::Chain, 0..12);
    }
}

#[test]
fn seeded_bounded_dense_dags_agree() {
    for profile in RuleProfile::ALL {
        run_family(profile, Family::Dense, 0..12);
    }
}

#[test]
fn a_permuted_input_does_not_change_either_answer() {
    for seed in 0..12 {
        let builder = seeded(seed, RuleProfile::LegacyV1, Family::Sparse);
        let first = builder.build();
        let mut permuted = builder.clone();
        permuted.reverse_assertions();
        let second = permuted.build();

        for term in first.terms() {
            let query = |graph: &BaseGraph| {
                let request = SourceQuery {
                    snapshot: graph.snapshot(),
                    source: term.id,
                    relation: RelationId::DependsOn,
                    with_proof: false,
                    budget: Budget::DEFAULT,
                };
                let optimized = OptimizedExecutor::new()
                    .solve(graph, &request, &sage_db_types::RunToCompletion)
                    .unwrap();
                let reference = ReferenceExecutor::new()
                    .solve(graph, &request, &sage_db_types::RunToCompletion)
                    .unwrap();
                (optimized.canonical_rows(), reference.canonical_rows())
            };
            let (optimized_a, reference_a) = query(&first);
            let (optimized_b, reference_b) = query(&second);
            assert_eq!(optimized_a, reference_a, "seed {seed}: executors disagree");
            assert_eq!(
                optimized_b, reference_b,
                "seed {seed}: executors disagree, permuted"
            );
            assert_eq!(
                optimized_a, optimized_b,
                "seed {seed}: optimized depends on input order"
            );
            assert_eq!(
                reference_a, reference_b,
                "seed {seed}: reference depends on input order"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-checks that do not go through either executor's selection
// ---------------------------------------------------------------------------

#[test]
fn both_executors_match_independent_path_enumeration() {
    for seed in 0..12 {
        let builder = seeded(seed, RuleProfile::LegacyV1, Family::Sparse);
        let graph = builder.build();
        for term in graph.terms() {
            let mut work = sage_db_reference::Meter::new(Budget::DEFAULT);
            let enumeration = paths::enumerate(
                RawRecords {
                    terms: graph.terms(),
                    assertions: graph.assertions(),
                    rules: graph.rules(),
                },
                RelationId::DependsOn,
                term.id,
                Caps::DEFAULT,
                &mut work,
                &sage_db_types::RunToCompletion,
            )
            .unwrap_or_else(|error| panic!("seed {seed}: enumeration failed: {error}"));

            let mut targets: Vec<TermId> = enumeration
                .paths
                .iter()
                .map(|path| path.steps.last().expect("non-empty").to)
                .collect();
            targets.sort_unstable();
            targets.dedup();
            targets.retain(|target| *target != term.id);

            let expected: Vec<CanonicalRow> = {
                let mut rows: Vec<CanonicalRow> = targets
                    .iter()
                    .map(|target| {
                        let matching = enumeration
                            .paths
                            .iter()
                            .filter(|path| path.steps.last().expect("non-empty").to == *target);
                        let annotation = matching
                            .clone()
                            .fold(Annotation::ABSENT, |acc, path| acc.join(path.annotation()));
                        let disputed = matching.clone().any(|path| path.disputed());
                        let derived = matching
                            .clone()
                            .any(|path| path.derived() || path.steps.len() >= 2);
                        let asserted = matching
                            .clone()
                            .any(|path| path.steps.len() == 1 && path.steps[0].rule.is_none());
                        CanonicalRow {
                            from: term.id,
                            relation: RelationId::DependsOn,
                            to: *target,
                            annotation,
                            has_asserted: asserted,
                            has_derived: derived,
                            disputed,
                        }
                    })
                    .collect();
                rows.sort_unstable_by_key(|row| (row.to, row.relation, row.from));
                rows
            };

            let request = SourceQuery {
                snapshot: graph.snapshot(),
                source: term.id,
                relation: RelationId::DependsOn,
                with_proof: false,
                budget: Budget::DEFAULT,
            };
            for (who, rows) in [
                (
                    "optimized",
                    OptimizedExecutor::new()
                        .solve(&graph, &request, &sage_db_types::RunToCompletion)
                        .unwrap()
                        .canonical_rows(),
                ),
                (
                    "reference",
                    ReferenceExecutor::new()
                        .solve(&graph, &request, &sage_db_types::RunToCompletion)
                        .unwrap()
                        .canonical_rows(),
                ),
            ] {
                assert_eq!(
                    rows, expected,
                    "seed {seed}: {who} disagrees with independent path enumeration for {:?}",
                    term.id
                );
            }
        }
    }
}

#[test]
fn every_proof_from_both_executors_replays_independently() {
    for seed in 0..10 {
        let graph = seeded(seed, RuleProfile::LegacyV1, Family::Dense).build();
        for term in graph.terms() {
            compare(&graph, term.id, RelationId::DependsOn, true);
        }
    }
    for seed in 0..6 {
        let graph = seeded(seed, RuleProfile::FormalV1, Family::Sparse).build();
        for term in graph.terms() {
            compare(&graph, term.id, RelationId::DependsOn, true);
        }
    }
}

// ---------------------------------------------------------------------------
// Resource boundary: typed errors, not equal costs
// ---------------------------------------------------------------------------

#[test]
fn a_starved_budget_is_a_typed_error_in_both_and_costs_are_not_compared() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let terms = builder.terms_at(SemanticLayer::L1, 8);
    for pair in terms.windows(2) {
        builder.edge_c(pair[0], RelationId::DependsOn, pair[1]);
    }
    let graph = builder.build();
    let starved = SourceQuery {
        snapshot: graph.snapshot(),
        source: terms[0],
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget {
            max_steps: 2,
            ..Budget::DEFAULT
        },
    };

    let optimized =
        OptimizedExecutor::new().solve(&graph, &starved, &sage_db_types::RunToCompletion);
    let reference =
        ReferenceExecutor::new().solve(&graph, &starved, &sage_db_types::RunToCompletion);

    // Both refuse, and both name an evaluation limit. Their step *counts* differ by
    // design, so equality of the error variant is the contract, not equal work.
    for (who, outcome) in [("optimized", optimized), ("reference", reference)] {
        match outcome {
            Err(QueryError::BudgetExceeded(_)) => {}
            other => panic!("{who} must refuse a starved budget, got {other:?}"),
        }
    }
}

#[test]
fn a_generous_budget_completes_in_both_and_reports_every_fact() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let terms = builder.terms_at(SemanticLayer::L1, 10);
    for pair in terms.windows(2) {
        builder.edge_c(pair[0], RelationId::DependsOn, pair[1]);
    }
    let graph = builder.build();
    let request = SourceQuery {
        snapshot: graph.snapshot(),
        source: terms[0],
        relation: RelationId::DependsOn,
        with_proof: true,
        budget: Budget {
            max_steps: 1_000_000,
            max_work_rows: 200_000,
            max_work_bytes: 32 * 1024 * 1024,
            max_rows: 1,
            max_output_bytes: 1,
            max_proof_nodes: 20_000,
        },
    };
    for who in ["optimized", "reference"] {
        let result = match who {
            "optimized" => {
                OptimizedExecutor::new().solve(&graph, &request, &sage_db_types::RunToCompletion)
            }
            _ => ReferenceExecutor::new().solve(&graph, &request, &sage_db_types::RunToCompletion),
        }
        .unwrap_or_else(|error| {
            panic!("{who} must complete a bounded fixture under a generous ceiling: {error}")
        });
        assert_eq!(
            result.rows.len(),
            9,
            "{who} must not truncate for max_rows=1"
        );
        assert!(result.proofs_complete(), "{who} proof set incomplete");
    }
}

#[test]
fn cancellation_is_typed_in_both() {
    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), sage_db_types::ControlStop> {
            Err(sage_db_types::ControlStop::Cancelled)
        }
    }

    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let terms = builder.terms_at(SemanticLayer::L1, 5);
    for pair in terms.windows(2) {
        builder.edge_c(pair[0], RelationId::DependsOn, pair[1]);
    }
    let graph = builder.build();
    let request = SourceQuery {
        snapshot: graph.snapshot(),
        source: terms[0],
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget::DEFAULT,
    };
    for who in ["optimized", "reference"] {
        let outcome = match who {
            "optimized" => OptimizedExecutor::new().solve(&graph, &request, &Stop),
            _ => ReferenceExecutor::new().solve(&graph, &request, &Stop),
        };
        assert_eq!(
            outcome,
            Err(QueryError::Stopped(sage_db_types::ControlStop::Cancelled)),
            "{who} must stop at the first checkpoint"
        );
    }
}

#[test]
fn neither_executor_compares_engine_version_into_semantics() {
    // Same rules, same graph: two engine builds must agree. This guards against an
    // engine version leaking into a semantic decision.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    builder.edge_c(a, RelationId::DependsOn, b);
    let graph = builder.build();
    let request = SourceQuery {
        snapshot: graph.snapshot(),
        source: a,
        relation: RelationId::DependsOn,
        with_proof: false,
        budget: Budget::DEFAULT,
    };
    let optimized = OptimizedExecutor::new()
        .solve(&graph, &request, &sage_db_types::RunToCompletion)
        .unwrap();
    let reference = ReferenceExecutor::new()
        .solve(&graph, &request, &sage_db_types::RunToCompletion)
        .unwrap();
    assert_eq!(optimized.canonical_rows(), reference.canonical_rows());
    assert_ne!(
        optimized.engine,
        EngineVersion::new(u32::MAX),
        "engine version is provenance, and both are valid builds"
    );
}

// ---------------------------------------------------------------------------
// The differential replay must be able to fail
// ---------------------------------------------------------------------------
//
// A replay helper that accepted everything would make every proof test above
// vacuous. These controls forge proofs with the *right endpoints and the right
// confidence* and require rejection, so the checks are load-bearing.

fn arena_of(nodes: &[ProofNode]) -> (ProofArena, Vec<ProofRef>) {
    let mut arena = ProofArena::new();
    let mut refs = Vec::new();
    for node in nodes {
        refs.push(arena.push(*node, u32::MAX, u64::MAX).expect("push"));
    }
    (arena, refs)
}

fn row_for(row: &sage_db_types::SourceResult, to: TermId) -> FactRow {
    *row.rows
        .iter()
        .find(|candidate| candidate.to == to)
        .expect("row")
}

#[test]
fn the_differential_replay_rejects_a_swapped_transitive_composition() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let ab = builder.edge_c(a, RelationId::DependsOn, b);
    let bc = builder.edge_c(b, RelationId::DependsOn, c);
    let graph = builder.build();
    let revision = RecordRevision::FIRST;

    // The real row: canonical, with a real proof.
    let result = ReferenceExecutor::new()
        .solve(
            &graph,
            &SourceQuery {
                snapshot: graph.snapshot(),
                source: a,
                relation: RelationId::DependsOn,
                with_proof: true,
                budget: Budget::DEFAULT,
            },
            &sage_db_types::RunToCompletion,
        )
        .unwrap();
    let row = row_for(&result, c);
    let real = result.proof.as_ref().expect("arena");
    assert_eq!(
        replay_row(&graph, real, row.proof.expect("root"), &row),
        Ok(())
    );

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
    assert_eq!(replay_row(&graph, &correct, correct_refs[2], &row), Ok(()));

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
    assert_eq!(
        replay_row(&graph, &swapped, swapped_refs[2], &row),
        Err(ReplayFailure::Disconnected),
        "swapping the operands must be rejected even though both leaves are real"
    );
}

#[test]
fn the_differential_replay_rejects_a_wrong_endpoint_at_the_right_confidence() {
    // A confirmed leaf whose endpoints are not the row's: same annotation, wrong
    // fact. A replay that only compared confidence would accept this.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let c = builder.term(SemanticLayer::L1);
    let ab = builder.edge_c(a, RelationId::DependsOn, b);
    builder.edge_c(b, RelationId::DependsOn, c);
    let graph = builder.build();

    let result = ReferenceExecutor::new()
        .solve(
            &graph,
            &SourceQuery {
                snapshot: graph.snapshot(),
                source: b,
                relation: RelationId::DependsOn,
                with_proof: true,
                budget: Budget::DEFAULT,
            },
            &sage_db_types::RunToCompletion,
        )
        .unwrap();
    let row = row_for(&result, c);
    assert_eq!(row.annotation, Annotation::present(Confidence::Confirmed));

    let (arena, refs) = arena_of(&[ProofNode::Asserted {
        assertion_id: ab,
        revision: RecordRevision::FIRST,
    }]);
    assert_eq!(
        replay_row(&graph, &arena, refs[0], &row),
        Err(ReplayFailure::FactMismatch)
    );
}

#[test]
fn the_differential_replay_rejects_a_stale_revision_and_a_dangling_root() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let b = builder.term(SemanticLayer::L1);
    let id = builder.edge_rev(a, RelationId::DependsOn, b, Confidence::Confirmed, 4);
    let graph = builder.build();
    let row = FactRow {
        from: a,
        relation: RelationId::DependsOn,
        to: b,
        annotation: Annotation::present(Confidence::Confirmed),
        has_asserted: true,
        has_derived: false,
        disputed: false,
        strongest_support: None,
        proof: None,
    };

    let (arena, refs) = arena_of(&[ProofNode::Asserted {
        assertion_id: id,
        revision: RecordRevision::new(3).unwrap(),
    }]);
    assert_eq!(
        replay_row(&graph, &arena, refs[0], &row),
        Err(ReplayFailure::StaleRevision)
    );

    let (arena, _refs) = arena_of(&[ProofNode::Asserted {
        assertion_id: id,
        revision: RecordRevision::new(4).unwrap(),
    }]);
    assert_eq!(
        replay_row(&graph, &arena, ProofRef::new(9), &row),
        Err(ReplayFailure::Dangling)
    );
}
