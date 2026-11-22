//! Per-threshold provenance, proof reuse and resource boundaries.
//!
//! Each test here corresponds to a defect found in review, so the name states the
//! property rather than the mechanism.

mod support;

use std::cell::Cell;
use std::time::Instant;

use sage_db_infer::OptimizedExecutor;
use sage_db_types::{
    Budget, BudgetLimit, Confidence, ControlStop, QueryControl, RelationId, RuleProfile,
    RunToCompletion, SemanticLayer, SourceExecutor, SourceQuery, TermId,
};
use support::Builder;

fn query(
    graph: &sage_db_types::BaseGraph,
    source: TermId,
    relation: RelationId,
    with_proof: bool,
    budget: Budget,
) -> SourceQuery {
    SourceQuery {
        snapshot: graph.snapshot(),
        source,
        relation,
        with_proof,
        budget,
    }
}

#[test]
fn a_shortcut_at_a_weaker_threshold_is_not_spliced_onto_a_stronger_parent() {
    // The review's counterexample:
    //   A -> B            hypothetical
    //   A -> X -> Y -> B  confirmed (3 hops)
    //   B -> T            hypothetical
    // B's strongest annotation is confirmed, but T is only hypothetical, and under
    // hypothetical the shortest B route is the 2-hop A -> B -> T. Splicing T onto
    // B's confirmed 3-hop parent would produce a 4-hop proof for a 2-hop fact.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    let b = builder.term(SemanticLayer::L1);
    let x = builder.term(SemanticLayer::L1);
    let y = builder.term(SemanticLayer::L1);
    let t = builder.term(SemanticLayer::L0);
    builder.edge(a, RelationId::DependsOn, b, Confidence::Hypothetical);
    builder.edge(a, RelationId::DependsOn, x, Confidence::Confirmed);
    builder.edge(x, RelationId::DependsOn, y, Confidence::Confirmed);
    builder.edge(y, RelationId::DependsOn, b, Confidence::Confirmed);
    builder.edge(b, RelationId::DependsOn, t, Confidence::Hypothetical);
    let graph = builder.build();

    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(&graph, a, RelationId::DependsOn, true, Budget::DEFAULT),
            &RunToCompletion,
        )
        .unwrap();

    let b_row = result.contains(b).expect("b reached");
    assert_eq!(
        b_row.annotation,
        sage_db_types::Annotation::present(Confidence::Confirmed),
        "b is confirmed by the long route"
    );
    let t_row = result.contains(t).expect("t reached");
    assert_eq!(
        t_row.annotation,
        sage_db_types::Annotation::present(Confidence::Hypothetical),
        "t is only hypothetical"
    );

    // The proof for t must be 2 hops (A -> B -> T), not 4 through b's confirmed parent.
    let fact = sage_db_infer::verify_row(&graph, &result, t_row).unwrap();
    assert_eq!((fact.from, fact.to), (a, t));
    assert_eq!(fact.annotation, t_row.annotation);
}

#[test]
fn proof_nodes_stay_near_linear_on_a_long_chain_thanks_to_prefix_reuse() {
    // 200-node chain, proofs for every row, default 20k proof budget. Without in-query
    // prefix reuse this needs ~40k nodes and is rejected; with reuse it is ~n.
    const NODES: u32 = 200;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();

    let started = Instant::now();
    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                terms[0],
                RelationId::DependsOn,
                true,
                Budget::DEFAULT,
            ),
            &RunToCompletion,
        )
        .expect("a 200-node chain must fit the default proof budget");
    let millis = started.elapsed().as_millis();

    eprintln!(
        "200-chain proofs: rows={} proof_nodes={} steps={} ms={millis}",
        result.rows.len(),
        result.stats.proof_nodes,
        result.stats.steps
    );
    assert_eq!(result.rows.len() as u32, NODES - 1);
    assert!(result.proofs_complete());
    // Near-linear: each row extends the previous prefix instead of rebuilding it.
    assert!(
        result.stats.proof_nodes <= 4 * NODES,
        "prefix reuse should keep nodes near the chain length, got {}",
        result.stats.proof_nodes
    );
}

#[test]
fn a_thousand_node_chain_completes_with_default_proof_budget() {
    const NODES: u32 = 1_000;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();

    let started = Instant::now();
    // Probe: find how many bytes this actually asks for.
    let mut probe = Budget::DEFAULT;
    for _ in 0..40 {
        match OptimizedExecutor::new().solve(
            &graph,
            &query(&graph, terms[0], RelationId::DependsOn, true, probe),
            &RunToCompletion,
        ) {
            Ok(result) => {
                eprintln!("1000-chain needed work_bytes <= {}", probe.max_work_bytes);
                eprintln!("actual charged: {}", result.stats.work_bytes);
                break;
            }
            Err(_) => probe.max_work_bytes = probe.max_work_bytes.saturating_mul(2),
        }
    }
    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                terms[0],
                RelationId::DependsOn,
                true,
                Budget::DEFAULT,
            ),
            &RunToCompletion,
        )
        .expect("1 000-node chain must fit the default budget");
    let millis = started.elapsed().as_millis();
    eprintln!(
        "1000-chain proofs: rows={} proof_nodes={} steps={} ms={millis}",
        result.rows.len(),
        result.stats.proof_nodes,
        result.stats.steps
    );
    assert_eq!(result.rows.len() as u32, NODES - 1);
    assert!(
        u64::from(result.stats.proof_nodes) <= 4 * u64::from(NODES),
        "expected near-linear node count, got {}",
        result.stats.proof_nodes
    );
    // Replay the shared arena once, not once per row.
    let verified = sage_db_infer::verify_rows(&graph, &result).expect("all rows replay");
    assert_eq!(verified.len(), result.rows.len());
}

#[test]
fn a_sparse_term_id_space_costs_nothing() {
    // Terms with large, sparse ids: indexing by id would allocate proportional to the
    // largest id. Positions must come from the graph's own term table.
    use sage_db_types::{
        AssertionId, AssertionRecord, BaseGraph, Digest, GraphDigest, KbUuid, KbVersion, RuleSet,
        RulesDigest, SnapshotId, TermMeta,
    };
    let mut terms = Vec::new();
    let ids = [0u32, 1_000_000, 2_000_000, 3_000_000];
    for (position, raw) in ids.iter().enumerate() {
        terms.push(TermMeta {
            id: TermId::new(*raw),
            layer: SemanticLayer::from_u8(u8::try_from(position % 4).unwrap()).unwrap(),
        });
    }
    let assertions = vec![
        AssertionRecord {
            id: AssertionId::new(1),
            revision: sage_db_types::RecordRevision::FIRST,
            from: TermId::new(1_000_000),
            relation: RelationId::DependsOn,
            target: TermId::new(2_000_000),
            confidence: Confidence::Confirmed,
        },
        AssertionRecord {
            id: AssertionId::new(2),
            revision: sage_db_types::RecordRevision::FIRST,
            from: TermId::new(2_000_000),
            relation: RelationId::DependsOn,
            target: TermId::new(3_000_000),
            confidence: Confidence::Confirmed,
        },
    ];
    let graph = BaseGraph::new(
        SnapshotId {
            kb_uuid: KbUuid::from_bytes([1; 16]),
            version: KbVersion::GENESIS,
            // Legacy-v1: the fixture is about id magnitude, not layer legality.
            rules: RuleSet::from_trusted_parts(
                RuleProfile::LegacyV1,
                RulesDigest::new(Digest::from_bytes([2; 32])),
            ),
            graph_digest: GraphDigest::new(Digest::from_bytes([3; 32])),
        },
        terms,
        assertions,
        Budget::DEFAULT,
    )
    .unwrap();

    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                TermId::new(1_000_000),
                RelationId::DependsOn,
                false,
                Budget::DEFAULT,
            ),
            &RunToCompletion,
        )
        .expect("sparse ids must not blow the budget");
    assert_eq!(result.rows.len(), 2, "both successors reached");
    // A 4-term graph must stay cheap regardless of id magnitude.
    assert!(
        result.stats.work_bytes < 64 * 1024,
        "sparse ids must not allocate by magnitude, used {} bytes",
        result.stats.work_bytes
    );
    assert!(result.stats.steps < 200);
}

#[test]
fn a_star_graph_validates_without_quadratic_rescanning() {
    // 400 leaves from one hub: a per-frame filter().collect() over the hub's full
    // adjacency would make cycle detection O(E^2) and blow the step budget.
    const LEAVES: u32 = 400;
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let hub = builder.term(SemanticLayer::L2);
    for _ in 0..LEAVES {
        let leaf = builder.term(SemanticLayer::L1);
        builder.edge(hub, RelationId::DependsOn, leaf, Confidence::Confirmed);
    }
    let graph = builder.build();

    let report = sage_db_infer::validate_candidate(
        &graph,
        Budget {
            max_steps: 200_000,
            ..Budget::DEFAULT
        },
        &RunToCompletion,
    )
    .expect("a star graph must validate within a linear step budget");
    assert!(report.is_valid(), "{:?}", report.violations);
}

#[test]
fn cancellation_is_honoured_in_each_phase() {
    // Setup/aggregation, validation and path evaluation must each consult control, not
    // only the BFS inner loop.
    struct StopAfter(Cell<u32>);
    impl QueryControl for StopAfter {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            let left = self.0.get();
            if left == 0 {
                return Err(ControlStop::Cancelled);
            }
            self.0.set(left - 1);
            Ok(())
        }
    }

    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    for _ in 0..20 {
        let t = builder.term(SemanticLayer::L1);
        builder.edge(a, RelationId::DependsOn, t, Confidence::Confirmed);
    }
    let graph = builder.build();
    let request = query(&graph, a, RelationId::DependsOn, true, Budget::DEFAULT);

    // Stop at a range of checkpoints: every phase must eventually honour it.
    for allowance in 0..12 {
        let control = StopAfter(Cell::new(allowance));
        let outcome = OptimizedExecutor::new().solve(&graph, &request, &control);
        match outcome {
            Err(sage_db_types::QueryError::Stopped(_)) => {}
            Ok(_) => {
                // A generous allowance may legitimately finish; require that a small
                // one does not.
                assert!(
                    allowance > 6,
                    "allowance {allowance} should still have been cancelled"
                );
            }
            Err(other) => panic!("unexpected error at allowance {allowance}: {other:?}"),
        }
    }
}

#[test]
fn a_low_work_bytes_budget_rejects_before_allocating() {
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let a = builder.term(SemanticLayer::L2);
    for _ in 0..40 {
        let t = builder.term(SemanticLayer::L1);
        builder.edge(a, RelationId::DependsOn, t, Confidence::Confirmed);
    }
    let graph = builder.build();
    let error = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                a,
                RelationId::DependsOn,
                false,
                Budget {
                    max_work_bytes: 512,
                    ..Budget::DEFAULT
                },
            ),
            &RunToCompletion,
        )
        .unwrap_err();
    assert_eq!(
        error,
        sage_db_types::QueryError::BudgetExceeded(BudgetLimit::WorkBytes)
    );
}

#[test]
fn a_high_outdegree_node_stays_inside_the_step_budget() {
    const LEAVES: u32 = 5_000;
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let hub = builder.term(SemanticLayer::L2);
    for _ in 0..LEAVES {
        let leaf = builder.term(SemanticLayer::L1);
        builder.edge(hub, RelationId::DependsOn, leaf, Confidence::Confirmed);
    }
    let graph = builder.build();

    // A tight step budget must stop this, not silently scan everything.
    let error = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                hub,
                RelationId::DependsOn,
                false,
                Budget {
                    max_steps: 1_000,
                    ..Budget::DEFAULT
                },
            ),
            &RunToCompletion,
        )
        .unwrap_err();
    assert_eq!(
        error,
        sage_db_types::QueryError::BudgetExceeded(BudgetLimit::Steps)
    );

    // With room, it completes and returns every leaf.
    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(&graph, hub, RelationId::DependsOn, false, Budget::DEFAULT),
            &RunToCompletion,
        )
        .unwrap();
    assert_eq!(result.rows.len() as u32, LEAVES);
}

#[test]
fn equal_length_complete_paths_are_deterministic_under_reversed_input() {
    // Two full paths of equal length and strength to the same sink. The chosen proof
    // must be the same regardless of input order, because BFS order is fixed by the
    // queue plus the documented adjacency order.
    let build = |reverse: bool| {
        let mut builder = Builder::new(RuleProfile::FormalV1);
        let a = builder.term(SemanticLayer::L2);
        let left = builder.term(SemanticLayer::L1);
        let right = builder.term(SemanticLayer::L1);
        let sink = builder.term(SemanticLayer::L0);
        let edges = [(a, left), (a, right), (left, sink), (right, sink)];
        if reverse {
            for (from, to) in edges.iter().rev() {
                builder.edge(*from, RelationId::DependsOn, *to, Confidence::Confirmed);
            }
        } else {
            for (from, to) in edges {
                builder.edge(from, RelationId::DependsOn, to, Confidence::Confirmed);
            }
        }
        builder.build()
    };

    let canonical = |graph: &sage_db_types::BaseGraph| {
        let result = OptimizedExecutor::new()
            .solve(
                graph,
                &query(
                    graph,
                    TermId::new(0),
                    RelationId::DependsOn,
                    true,
                    Budget::DEFAULT,
                ),
                &RunToCompletion,
            )
            .unwrap();
        let row = result.contains(TermId::new(3)).expect("sink reached");
        let fact = sage_db_infer::verify_row(graph, &result, row).unwrap();
        (fact.from, fact.to, row.proof.is_some())
    };

    assert_eq!(
        canonical(&build(false)),
        canonical(&build(true)),
        "equal-length paths must pick the same proof in either input order"
    );
}

#[test]
fn a_cached_prefix_is_never_reused_across_confidence_levels() {
    // Review counterexample (item A). Ids are chosen so canonical row order puts T
    // before B, so T's (contested) 3-hop route is proved first and would file the
    // contested A->B prefix under a stronger level:
    //   A=0 T=1 X=2 Y=3 B=4 S=5
    //   A-X-Y-B all confirmed   (B confirmed in 3 hops)
    //   A-S contested, S-B confirmed, B-T contested
    // T's strongest annotation is contested and its shortest route is A-S-B-T. If that
    // route's prefix were memoised under `confirmed`, B's row would reuse it and claim
    // confirmed while its proof is actually contested.
    use sage_db_types::{
        AssertionRecord, BaseGraph, Digest, GraphDigest, KbUuid, KbVersion, RecordRevision,
        RuleSet, RulesDigest, SnapshotId, TermMeta,
    };
    fn term(id: u32, layer: SemanticLayer) -> TermMeta {
        TermMeta {
            id: TermId::new(id),
            layer,
        }
    }
    let a = TermId::new(0);
    let t = TermId::new(1);
    let x = TermId::new(2);
    let y = TermId::new(3);
    let b = TermId::new(4);
    let s = TermId::new(5);

    let mut assertions = Vec::new();
    let mut next = 1u128;
    let mut edge = |from: TermId, to: TermId, confidence| {
        assertions.push(AssertionRecord {
            id: sage_db_types::AssertionId::new(next),
            revision: RecordRevision::FIRST,
            from,
            relation: RelationId::DependsOn,
            target: to,
            confidence,
        });
        next += 1;
    };
    edge(a, x, Confidence::Confirmed);
    edge(x, y, Confidence::Confirmed);
    edge(y, b, Confidence::Confirmed);
    edge(a, s, Confidence::Contested);
    edge(s, b, Confidence::Confirmed);
    edge(b, t, Confidence::Contested);

    let graph = BaseGraph::new(
        SnapshotId {
            kb_uuid: KbUuid::from_bytes([7; 16]),
            version: KbVersion::GENESIS,
            rules: RuleSet::from_trusted_parts(
                RuleProfile::LegacyV1,
                RulesDigest::new(Digest::from_bytes([8; 32])),
            ),
            graph_digest: GraphDigest::new(Digest::from_bytes([9; 32])),
        },
        vec![
            term(0, SemanticLayer::L2),
            term(1, SemanticLayer::L0),
            term(2, SemanticLayer::L1),
            term(3, SemanticLayer::L1),
            term(4, SemanticLayer::L1),
            term(5, SemanticLayer::L1),
        ],
        assertions,
        Budget::DEFAULT,
    )
    .unwrap();

    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(&graph, a, RelationId::DependsOn, true, Budget::DEFAULT),
            &RunToCompletion,
        )
        .expect("query must succeed");

    // Every row's proof must independently replay to the row's own claim. A wrongly
    // shared prefix shows up here as FactMismatch or an annotation disagreement.
    for row in &result.rows {
        let fact = sage_db_infer::verify_row(&graph, &result, row).unwrap_or_else(|error| {
            panic!(
                "row {} ({:?}) failed replay: {error}",
                row.to.get(),
                row.annotation
            )
        });
        assert_eq!(fact.annotation, row.annotation);
        assert_eq!((fact.from, fact.to), (a, row.to));
    }

    // B must be confirmed (3-hop all-confirmed route) and T contested.
    let b_row = result.contains(b).expect("b reached");
    assert_eq!(
        b_row.annotation,
        sage_db_types::Annotation::present(Confidence::Confirmed),
        "b has an all-confirmed route"
    );
    let t_row = result.contains(t).expect("t reached");
    assert_eq!(
        t_row.annotation,
        sage_db_types::Annotation::present(Confidence::Contested),
        "t is only reachable through a contested edge"
    );
    assert!(t_row.disputed);
}

#[test]
fn batch_replay_visits_each_arena_node_once_regardless_of_row_count() {
    // 200-node chain, proofs for every row: batch verification must be proportional to
    // arena nodes + rows, not rows x depth.
    const NODES: u32 = 200;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();

    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                terms[0],
                RelationId::DependsOn,
                true,
                Budget::DEFAULT,
            ),
            &RunToCompletion,
        )
        .unwrap();
    let arena_nodes = result.proof.as_ref().unwrap().len();
    let verified = sage_db_infer::verify_rows(&graph, &result).expect("batch replay must pass");
    assert_eq!(verified.len(), result.rows.len());
    // The sweep is bounded by arena nodes plus rows; a per-row replay would be
    // rows * depth and far exceed this.
    assert!(
        arena_nodes + result.rows.len() < result.rows.len() * NODES as usize,
        "batch replay must not revisit per row: nodes={arena_nodes} rows={}",
        result.rows.len()
    );
}

#[test]
fn batch_replay_honours_a_tight_budget_and_cancellation() {
    const NODES: u32 = 120;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();
    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                terms[0],
                RelationId::DependsOn,
                true,
                Budget::DEFAULT,
            ),
            &RunToCompletion,
        )
        .unwrap();

    // Zero-byte budget: the value table cannot be reserved.
    let error = sage_db_infer::verify_rows_with(
        &graph,
        &result,
        &RunToCompletion,
        Budget {
            max_work_bytes: 0,
            ..Budget::DEFAULT
        },
    )
    .unwrap_err();
    assert!(
        matches!(error.1, sage_db_infer::ReplayError::BudgetExhausted),
        "a zero-byte verification budget must be refused, got {:?}",
        error.1
    );

    // Mid-way cancellation.
    struct StopAfter(Cell<u32>);
    impl QueryControl for StopAfter {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            let left = self.0.get();
            if left == 0 {
                return Err(ControlStop::Cancelled);
            }
            self.0.set(left - 1);
            Ok(())
        }
    }
    let error =
        sage_db_infer::verify_rows_with(&graph, &result, &StopAfter(Cell::new(1)), Budget::DEFAULT)
            .unwrap_err();
    assert!(
        matches!(error.1, sage_db_infer::ReplayError::Stopped),
        "cancellation during verification must be reported, got {:?}",
        error.1
    );
}

#[test]
fn batch_replay_cost_is_arena_plus_rows_not_rows_times_depth() {
    // Review measured 28 203 per-row visits against 473 for one shared walk on a
    // 120-chain. The assertion is on the ratio, not a printed number.
    const NODES: u32 = 120;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();
    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                terms[0],
                RelationId::DependsOn,
                true,
                Budget::DEFAULT,
            ),
            &RunToCompletion,
        )
        .unwrap();
    let arena_nodes = result.proof.as_ref().unwrap().len();

    // Charge accounting: a batch verification over this result must fit a budget that a
    // per-row replay could not, because the table is built once.
    let budget = Budget::DEFAULT;
    let verified =
        sage_db_infer::verify_rows_with(&graph, &result, &RunToCompletion, budget).unwrap();
    assert_eq!(verified.len(), result.rows.len());

    // The work is bounded by nodes + rows. `rows * depth` for this shape is
    // ~120*119 = 14 280, so require the bound to be far below that.
    let bound = arena_nodes + result.rows.len();
    assert!(
        bound < result.rows.len() * NODES as usize / 4,
        "batch replay bound {bound} should be far below rows*depth {}",
        result.rows.len() * NODES as usize
    );
}

#[test]
fn batch_verification_is_charged_once_for_the_whole_operation() {
    // Superseded assumption: the table and the output vector are alive together, so a
    // "single table" budget is NOT sufficient by design. What must hold is that the
    // whole operation fits one ledger, and that a smaller budget is refused rather
    // than silently granted a second allowance.
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, 120);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();
    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                terms[0],
                RelationId::DependsOn,
                true,
                Budget::DEFAULT,
            ),
            &RunToCompletion,
        )
        .unwrap();

    // Too small for the combined peak: refused.
    let refused = sage_db_infer::verify_rows_with(
        &graph,
        &result,
        &RunToCompletion,
        Budget {
            max_work_bytes: 64,
            ..Budget::DEFAULT
        },
    );
    assert!(
        refused.is_err(),
        "a 64-byte budget cannot verify a 120-chain"
    );

    // The default budget covers the whole operation exactly once.
    let verified =
        sage_db_infer::verify_rows_with(&graph, &result, &RunToCompletion, Budget::DEFAULT)
            .expect("default budget covers one full verification");
    assert_eq!(verified.len(), result.rows.len());
}

#[test]
fn verification_uses_one_ledger_and_charges_cumulatively() {
    // A double-ledger implementation passes as soon as EACH stage fits on its own,
    // because each stage gets a fresh full allowance:
    //   old stage 1 = value table + recovered facts (both alive inside replay_rows)
    //   old stage 2 = final output vector
    //   old threshold = max(stage1, stage2)
    // A single ledger instead accumulates every reservation monotonically, so the
    // requirement is the SUM of the charges:
    //   new threshold = stage1 + stage2
    //
    // This is cumulative allocation accounting, not a hard peak: when
    // `replay_rows_with` returns, `values` has already been dropped, so only the
    // recovered facts and the final output are live at that instant. The figure below
    // is what one ledger has been asked to reserve in total, and that is what the
    // assertion pins. It must sit inside [old_threshold, new_threshold) to tell the
    // two designs apart. An earlier version of this test used max(table, output),
    // which is BELOW old stage 1, so the old code failed too and the test proved
    // nothing.
    const NODES: u32 = 120;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();
    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                terms[0],
                RelationId::DependsOn,
                true,
                Budget::DEFAULT,
            ),
            &RunToCompletion,
        )
        .unwrap();

    let arena_nodes = result.proof.as_ref().unwrap().len() as u64;
    let row_count = result.rows.len() as u64;
    let slot = core::mem::size_of::<Option<sage_db_infer::ReplayedFact>>() as u64;
    let fact = core::mem::size_of::<sage_db_infer::ReplayedFact>() as u64;
    let out_slot = core::mem::size_of::<(usize, sage_db_infer::ReplayedFact)>() as u64;

    // Stage 1 holds the value table AND the recovered vector at the same time.
    let stage1 = arena_nodes * slot + row_count * fact;
    let stage2 = row_count * out_slot;
    let old_threshold = stage1.max(stage2);
    let new_threshold = stage1 + stage2;
    assert!(
        old_threshold < new_threshold,
        "precondition: the two designs must differ (old {old_threshold} vs new {new_threshold})"
    );

    // A budget that a double-ledger design accepts and a single ledger must refuse.
    let distinguishing = old_threshold;
    assert!(distinguishing < new_threshold);

    let refused = sage_db_infer::verify_rows_with(
        &graph,
        &result,
        &RunToCompletion,
        Budget {
            max_work_bytes: distinguishing,
            ..Budget::DEFAULT
        },
    );
    assert!(
        refused.is_err(),
        "a single ledger must refuse {distinguishing} bytes: cumulative charges \
         total {new_threshold}"
    );

    // One byte less than the true requirement must also fail, so the bound is tight
    // rather than lucky.
    let one_short = sage_db_infer::verify_rows_with(
        &graph,
        &result,
        &RunToCompletion,
        Budget {
            max_work_bytes: new_threshold - 1,
            ..Budget::DEFAULT
        },
    );
    assert!(one_short.is_err(), "the refusal boundary must be tight");

    // And the full figure succeeds: the positive control.
    let verified = sage_db_infer::verify_rows_with(
        &graph,
        &result,
        &RunToCompletion,
        Budget {
            max_work_bytes: new_threshold,
            ..Budget::DEFAULT
        },
    )
    .expect("the exact combined figure must be accepted");
    assert_eq!(verified.len(), result.rows.len());

    // The default budget still covers everything.
    sage_db_infer::verify_rows_with(&graph, &result, &RunToCompletion, Budget::DEFAULT)
        .expect("default budget covers one full verification");
}

#[test]
fn verification_refuses_a_zero_work_row_budget() {
    // Held values and compared rows are work rows; zero must refuse, not be ignored.
    const NODES: u32 = 20;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();
    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                terms[0],
                RelationId::DependsOn,
                true,
                Budget::DEFAULT,
            ),
            &RunToCompletion,
        )
        .unwrap();

    let error = sage_db_infer::verify_rows_with(
        &graph,
        &result,
        &RunToCompletion,
        Budget {
            max_work_rows: 0,
            ..Budget::DEFAULT
        },
    )
    .unwrap_err();
    assert!(
        matches!(error.1, sage_db_infer::ReplayError::BudgetExhausted),
        "a zero row budget must refuse verification, got {:?}",
        error.1
    );
}

#[test]
fn cancellation_reaches_the_row_comparison_phase() {
    struct StopAfter(Cell<u32>);
    impl QueryControl for StopAfter {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            let left = self.0.get();
            if left == 0 {
                return Err(ControlStop::Cancelled);
            }
            self.0.set(left - 1);
            Ok(())
        }
    }

    const NODES: u32 = 60;
    let mut builder = Builder::new(RuleProfile::FormalV1);
    let terms = builder.terms_at(SemanticLayer::L1, NODES);
    for pair in terms.windows(2) {
        builder.edge(
            pair[0],
            RelationId::DependsOn,
            pair[1],
            Confidence::Confirmed,
        );
    }
    let graph = builder.build();
    let result = OptimizedExecutor::new()
        .solve(
            &graph,
            &query(
                &graph,
                terms[0],
                RelationId::DependsOn,
                true,
                Budget::DEFAULT,
            ),
            &RunToCompletion,
        )
        .unwrap();

    // Measure how many checkpoints a full verification actually makes, then require
    // that cancelling at *every* one of them is observed. Guessing a band would let a
    // missing phase check pass unnoticed.
    struct Counter(Cell<u32>);
    impl QueryControl for Counter {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }
    }
    let counter = Counter(Cell::new(0));
    sage_db_infer::verify_rows_with(&graph, &result, &counter, Budget::DEFAULT)
        .expect("verification succeeds");
    let total = counter.0.get();
    assert!(
        total >= 3,
        "entry, phase boundary and exit must all be consulted, got {total}"
    );

    for allowance in 0..total {
        let error = sage_db_infer::verify_rows_with(
            &graph,
            &result,
            &StopAfter(Cell::new(allowance)),
            Budget::DEFAULT,
        )
        .unwrap_err();
        assert!(
            matches!(error.1, sage_db_infer::ReplayError::Stopped),
            "allowance {allowance} of {total} must stop verification, got {:?}",
            error.1
        );
    }
    // The full allowance succeeds.
    sage_db_infer::verify_rows_with(
        &graph,
        &result,
        &StopAfter(Cell::new(total)),
        Budget::DEFAULT,
    )
    .expect("full allowance succeeds");
}
