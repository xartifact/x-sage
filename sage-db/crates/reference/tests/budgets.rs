//! Resource and boundary behaviour that the accepted contract fixes.
//!
//! Two things are asserted here and nowhere else:
//!
//! * a complete evaluation is never truncated for a *display* limit — `max_rows`
//!   and `max_output_bytes` are carried but not applied by this layer;
//! * every evaluation limit (`steps`, `work_rows`, `work_bytes`, `proof_nodes`) and
//!   every control signal is a typed error, never a partial row set.

mod support;

use sage_db_reference::Expected;
use sage_db_types::{
    Annotation, Budget, BudgetLimit, Confidence, ControlStop, QueryControl, QueryError, RelationId,
    RuleProfile, RunToCompletion, SemanticLayer,
};
use support::Builder;

struct StopAfter(std::cell::Cell<u32>);

impl QueryControl for StopAfter {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        let remaining = self.0.get();
        if remaining == 0 {
            Err(ControlStop::Cancelled)
        } else {
            self.0.set(remaining - 1);
            Ok(())
        }
    }
}

fn chain(length: u32) -> (Builder, Vec<sage_db_types::TermId>) {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let terms = builder.terms_at(SemanticLayer::L1, length);
    for pair in terms.windows(2) {
        builder.edge_c(pair[0], RelationId::DependsOn, pair[1]);
    }
    (builder, terms)
}

#[test]
fn a_low_display_row_limit_does_not_truncate_a_complete_result() {
    // Six terms in a chain from a: five facts are reported. `max_rows` is one — a
    // display hint for a later layer — so the complete result must still hold five
    // rows and no row may be missing.
    let (builder, terms) = chain(6);
    let graph = builder.build();

    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.budget = Budget {
        max_rows: 1,
        max_output_bytes: 1,
        ..Budget::DEFAULT
    };
    let result = support::solve_with(&graph, &request, &RunToCompletion)
        .expect("a display limit must not fail the evaluation");
    assert_eq!(
        result.rows.len(),
        5,
        "display limits do not truncate P0 results"
    );
    // And nothing was turned into absence.
    for term in &terms[1..] {
        assert!(sage_db_reference::row_for(&result, *term).is_some());
    }
}

#[test]
fn a_low_display_row_limit_still_charges_the_work_rows() {
    let (builder, terms) = chain(6);
    let graph = builder.build();
    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.budget = Budget {
        max_rows: 1,
        ..Budget::DEFAULT
    };
    let result = support::solve_with(&graph, &request, &RunToCompletion).expect("complete");
    assert!(
        result.stats.work_rows >= result.rows.len() as u64,
        "every returned row is charged: {} rows, {} charged",
        result.rows.len(),
        result.stats.work_rows
    );
}

#[test]
fn a_step_limit_is_a_typed_error_not_a_partial_result() {
    let (builder, terms) = chain(8);
    let graph = builder.build();
    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.budget = Budget {
        max_steps: 3,
        ..Budget::DEFAULT
    };
    assert_eq!(
        support::solve_with(&graph, &request, &RunToCompletion),
        Err(QueryError::BudgetExceeded(BudgetLimit::Steps))
    );
}

#[test]
fn a_work_row_limit_is_a_typed_error() {
    let (builder, terms) = chain(8);
    let graph = builder.build();
    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.budget = Budget {
        max_work_rows: 2,
        ..Budget::DEFAULT
    };
    assert_eq!(
        support::solve_with(&graph, &request, &RunToCompletion),
        Err(QueryError::BudgetExceeded(BudgetLimit::WorkRows))
    );
}

#[test]
fn a_work_byte_limit_is_a_typed_error() {
    let (builder, terms) = chain(8);
    let graph = builder.build();
    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.budget = Budget {
        max_work_bytes: 16,
        ..Budget::DEFAULT
    };
    assert_eq!(
        support::solve_with(&graph, &request, &RunToCompletion),
        Err(QueryError::BudgetExceeded(BudgetLimit::WorkBytes))
    );
}

#[test]
fn a_proof_node_limit_is_a_typed_error() {
    let (builder, terms) = chain(6);
    let graph = builder.build();
    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.with_proof = true;
    request.budget = Budget {
        max_proof_nodes: 2,
        ..Budget::DEFAULT
    };
    assert_eq!(
        support::solve_with(&graph, &request, &RunToCompletion),
        Err(QueryError::BudgetExceeded(BudgetLimit::ProofNodes))
    );
}

#[test]
fn cancelling_at_a_checkpoint_returns_stopped_and_no_rows() {
    let (builder, terms) = chain(8);
    let graph = builder.build();
    let request = support::query(&graph, terms[0], RelationId::DependsOn);

    // Zero remaining means the very first checkpoint stops the evaluation.
    let control = StopAfter(std::cell::Cell::new(0));
    assert_eq!(
        support::solve_with(&graph, &request, &control),
        Err(QueryError::Stopped(ControlStop::Cancelled))
    );
}

#[test]
fn an_unknown_source_term_is_refused() {
    let (builder, _terms) = chain(2);
    let graph = builder.build();
    let request = support::query(
        &graph,
        sage_db_types::TermId::new(99),
        RelationId::DependsOn,
    );
    assert_eq!(
        support::solve_with(&graph, &request, &RunToCompletion),
        Err(QueryError::UnknownTerm(sage_db_types::TermId::new(99)))
    );
}

#[test]
fn a_snapshot_mismatch_is_refused_rather_than_answered() {
    let (builder, terms) = chain(3);
    let graph = builder.build();
    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.snapshot = support::snapshot(RuleProfile::FormalV1, 2);
    assert!(matches!(
        support::solve_with(&graph, &request, &RunToCompletion),
        Err(QueryError::RuleMismatch { .. })
    ));

    let mut newer = support::query(&graph, terms[0], RelationId::DependsOn);
    newer.snapshot = support::snapshot(RuleProfile::LegacyV1, 7);
    assert!(matches!(
        support::solve_with(&graph, &newer, &RunToCompletion),
        Err(QueryError::RuleMismatch { .. })
    ));
}

#[test]
fn a_zero_step_budget_is_an_invalid_request() {
    let (builder, terms) = chain(2);
    let graph = builder.build();
    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.budget = Budget {
        max_steps: 0,
        ..Budget::DEFAULT
    };
    assert_eq!(
        support::solve_with(&graph, &request, &RunToCompletion),
        Err(QueryError::InvalidRequest {
            reason: "max_steps must be positive"
        })
    );
}

#[test]
fn the_result_reports_the_snapshot_and_engine_it_evaluated() {
    let (builder, terms) = chain(3);
    let graph = builder.build();
    let result = support::solve(&graph, terms[0], RelationId::DependsOn, false);
    assert_eq!(result.snapshot, graph.snapshot());
    assert_eq!(result.engine, sage_db_reference::ENGINE_VERSION);
}

#[test]
fn the_identity_of_the_source_is_never_reported_as_a_fact() {
    let (builder, terms) = chain(3);
    let graph = builder.build();
    let result = support::solve(&graph, terms[0], RelationId::DependsOn, false);
    assert!(
        sage_db_reference::row_for(&result, terms[0]).is_none(),
        "the zero-hop identity is not a fact"
    );
}

#[test]
fn a_proof_budget_error_names_only_evaluation_limits() {
    // `BudgetLimit` has no display variant, so a P0 failure can never report a
    // truncated display as the reason an evaluation failed.
    let limits = [
        BudgetLimit::Steps,
        BudgetLimit::WorkRows,
        BudgetLimit::WorkBytes,
        BudgetLimit::ProofNodes,
    ];
    for limit in limits {
        let text = limit.to_string();
        assert!(
            !text.contains("max_rows") && !text.contains("max_output_bytes"),
            "display limit leaked into an evaluation error: {text}"
        );
    }
}

#[test]
fn stats_expose_charged_work_rows() {
    let (builder, terms) = chain(4);
    let graph = builder.build();
    let result = support::solve(&graph, terms[0], RelationId::DependsOn, false);
    assert!(result.stats.steps > 0);
    assert!(result.stats.work_rows > 0);
    assert!(result.stats.work_bytes > 0);
    assert_eq!(result.stats.proof_nodes, 0, "no proofs were requested");
}

#[test]
fn a_proof_result_charges_proof_nodes_and_stays_replayable() {
    let (builder, terms) = chain(5);
    let graph = builder.build();
    let result = support::solve(&graph, terms[0], RelationId::DependsOn, true);
    assert!(result.stats.proof_nodes > 0);
    assert!(result.proofs_complete());
    let arena = result.proof.as_ref().expect("arena");
    for row in &result.rows {
        sage_db_reference::proof::replay_expecting(
            &graph,
            graph.rules(),
            arena,
            row.proof.expect("root"),
            Expected {
                from: row.from,
                relation: row.relation,
                to: row.to,
                annotation: row.annotation,
            },
        )
        .expect("every charged proof must replay");
    }
    // A chain's proofs share prefixes, so the arena is much smaller than
    // rows * hops would be if each row were rendered independently.
    let naive: usize = result
        .rows
        .iter()
        .enumerate()
        .map(|(index, _)| index + 1)
        .sum();
    assert!(
        arena.len() <= naive,
        "shared sub-proofs must not cost more than per-row copies: {} > {naive}",
        arena.len()
    );
}

#[test]
fn a_budget_that_admits_the_evaluation_still_reports_every_fact() {
    // Generous but explicit ceiling: the fixture is small, so a refusal here would be
    // a defect rather than a resource limit.
    let (builder, terms) = chain(10);
    let graph = builder.build();
    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.with_proof = true;
    request.budget = Budget {
        max_steps: 1_000_000,
        max_work_rows: 100_000,
        max_work_bytes: 8 * 1024 * 1024,
        max_rows: 1,
        max_output_bytes: 1,
        max_proof_nodes: 10_000,
    };
    let result = support::solve_with(&graph, &request, &RunToCompletion)
        .expect("small bounded fixtures must complete under a generous ceiling");
    assert_eq!(result.rows.len(), 9);
    for row in &result.rows {
        assert_eq!(row.annotation, Annotation::present(Confidence::Confirmed));
    }
}

// ---------------------------------------------------------------------------
// Proof-arena accounting: doubling capacity and the shared work area
// ---------------------------------------------------------------------------

#[test]
fn a_growing_arena_is_charged_the_doubled_capacity_not_one_slot() {
    // 40 terms: 114 proof nodes are needed, so the arena doubles several times.
    // Charging one slot per push would leave the ledger far below real memory.
    let (builder, terms) = chain(40);
    let graph = builder.build();
    let result = support::solve(&graph, terms[0], RelationId::DependsOn, true);
    let arena = result.proof.as_ref().expect("arena");
    let slot = sage_db_types::ProofNode::slot_bytes();

    assert!(arena.len() > 100, "fixture must be large enough to double");
    let logical = arena.len() as u64 * slot;
    let resident = arena.bytes();
    assert!(
        resident > logical,
        "a doubling arena holds more than len*slot: resident {resident}, logical {logical}"
    );

    // The ledger must cover the arena's real resident bytes, plus the other work
    // areas; it must never be below the arena alone.
    assert!(
        result.stats.work_bytes >= resident,
        "ledger {} must cover the arena's resident {}",
        result.stats.work_bytes,
        resident
    );
    // And it must not have charged a whole extra full budget either.
    let budget = Budget::DEFAULT.max_work_bytes;
    assert!(
        result.stats.work_bytes < budget,
        "the ledger must reflect real growth, not the whole budget"
    );
}

#[test]
fn the_arena_receives_the_remaining_share_not_the_whole_budget() {
    // A budget barely above one doubling step: the proof must be refused with a typed
    // error rather than the arena being handed `max_work_bytes` and overrunning.
    let (builder, terms) = chain(8);
    let graph = builder.build();
    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.with_proof = true;
    request.budget = Budget {
        max_work_bytes: 4 * 1024,
        max_proof_nodes: 10_000,
        ..Budget::DEFAULT
    };

    match support::solve_with(&graph, &request, &RunToCompletion) {
        Err(QueryError::BudgetExceeded(BudgetLimit::WorkBytes)) => {}
        Err(other) => panic!("expected a byte-budget refusal, got {other}"),
        Ok(result) => panic!(
            "a 4 KiB work area must not host this proof set: {} rows, {} bytes charged",
            result.rows.len(),
            result.stats.work_bytes
        ),
    }
}

#[test]
fn a_proof_whose_arena_fits_is_still_accepted() {
    // Control for the test above: the same graph under a generous ceiling succeeds,
    // so the refusal is the ceiling and not the fixture.
    let (builder, terms) = chain(8);
    let graph = builder.build();
    let mut request = support::query(&graph, terms[0], RelationId::DependsOn);
    request.with_proof = true;
    request.budget = Budget {
        max_work_bytes: 8 * 1024 * 1024,
        ..Budget::DEFAULT
    };
    let result = support::solve_with(&graph, &request, &RunToCompletion).expect("generous ceiling");
    let arena = result.proof.as_ref().expect("arena");
    assert!(!arena.is_empty());
    assert!(result.stats.work_bytes >= arena.bytes());
}

/// Counts how many times the executor actually consults the control hook.
struct Counting(std::cell::Cell<u32>);

impl QueryControl for Counting {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        self.0.set(self.0.get() + 1);
        Ok(())
    }
}

#[test]
fn every_step_is_within_one_control_interval_of_a_check() {
    // The documented bound is one `checkpoint` per `CONTROL_CHECK_INTERVAL_STEPS`
    // charged steps. This fixture makes the *inner* join loop the dominant run: one
    // midpoint with 3000 outbound edges. Without a check inside that loop the
    // pigeonhole bound is violated, which is what this asserts.
    //
    // Measured before the fix: steps=30007, checkpoints=25, bound=30 -> fails.
    // Measured after:         steps=30007, checkpoints=31, bound=30 -> passes.
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let mid = builder.term(SemanticLayer::L1);
    let leaves: Vec<_> = (0..3000).map(|_| builder.term(SemanticLayer::L1)).collect();
    builder.edge_c(a, RelationId::DependsOn, mid);
    for leaf in &leaves {
        builder.edge_c(mid, RelationId::DependsOn, *leaf);
    }
    let graph = builder.build();

    let request = support::query(&graph, a, RelationId::DependsOn);
    let control = Counting(std::cell::Cell::new(0));
    let result = support::solve_with(&graph, &request, &control).expect("complete evaluation");

    let interval = sage_db_types::CONTROL_CHECK_INTERVAL_STEPS;
    let required = result.stats.steps / interval;
    let observed = u64::from(control.0.get());
    assert!(
        observed >= required,
        "pigeonhole bound violated: {} steps in {} checkpoints, at least {required} required \
         (one check per {interval} steps)",
        result.stats.steps,
        observed
    );
}

#[test]
fn a_cancelled_evaluation_stops_at_the_first_consultation() {
    let mut builder = Builder::new(RuleProfile::LegacyV1);
    let a = builder.term(SemanticLayer::L1);
    let mid = builder.term(SemanticLayer::L1);
    let leaves: Vec<_> = (0..3000).map(|_| builder.term(SemanticLayer::L1)).collect();
    builder.edge_c(a, RelationId::DependsOn, mid);
    for leaf in &leaves {
        builder.edge_c(mid, RelationId::DependsOn, *leaf);
    }
    let graph = builder.build();
    let request = support::query(&graph, a, RelationId::DependsOn);

    let control = StopAfter(std::cell::Cell::new(0));
    assert_eq!(
        support::solve_with(&graph, &request, &control),
        Err(QueryError::Stopped(ControlStop::Cancelled))
    );
}
