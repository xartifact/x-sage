//! Query-shaped contracts, cancellation, proof roots and the proof vocabulary.
//!
//! What a caller may rely on before either algorithm exists: request validation,
//! canonical row order, the exclusion list for differential comparison, the
//! per-fact proof-root rule, and the direction carried by
//! `ProofNode::Transitive`.

use sage_db_types::{
    Annotation, AssertionId, AssertionRecord, AssertionSupport, BaseGraph, Budget, Confidence,
    ControlStop, Digest, EngineVersion, FactRow, GraphBuildError, GraphDigest, KbUuid, KbVersion,
    ProofArena, ProofArenaError, ProofNode, ProofRef, QueryControl, QueryError, RecordRevision,
    RelationId, RuleProfile, RuleSet, RulesDigest, RunToCompletion, SemanticLayer, SnapshotId,
    SourceQuery, SourceResult, TermId, TermMeta,
};

fn rules(profile: RuleProfile, seed: u8) -> RuleSet {
    RuleSet::from_trusted_parts(profile, RulesDigest::new(Digest::from_bytes([seed; 32])))
}

fn snapshot(version: u64) -> SnapshotId {
    SnapshotId {
        kb_uuid: KbUuid::from_bytes([3; 16]),
        version: KbVersion::new(version),
        rules: rules(RuleProfile::FormalV1, 4),
        graph_digest: GraphDigest::new(Digest::from_bytes([5; 32])),
    }
}

fn query(budget: Budget) -> SourceQuery {
    SourceQuery {
        snapshot: snapshot(1),
        source: TermId::new(1),
        relation: RelationId::DependsOn,
        with_proof: false,
        budget,
    }
}

fn row(to: u32, confidence: Confidence) -> FactRow {
    FactRow {
        from: TermId::new(1),
        relation: RelationId::DependsOn,
        to: TermId::new(to),
        annotation: Annotation::present(confidence),
        has_asserted: true,
        has_derived: false,
        disputed: false,
        strongest_support: None,
        proof: None,
    }
}

fn empty_graph(snapshot: SnapshotId) -> BaseGraph {
    BaseGraph::new(snapshot, Vec::new(), Vec::new(), Budget::DEFAULT).unwrap()
}

#[test]
fn a_zero_row_budget_is_rejected_before_any_work() {
    let mut budget = Budget::DEFAULT;
    budget.max_rows = 0;
    assert_eq!(
        query(budget).validate().unwrap_err(),
        QueryError::InvalidRequest {
            reason: "max_rows must be positive"
        }
    );
}

#[test]
fn a_zero_step_budget_is_rejected() {
    let mut budget = Budget::DEFAULT;
    budget.max_steps = 0;
    assert_eq!(
        query(budget).validate().unwrap_err(),
        QueryError::InvalidRequest {
            reason: "max_steps must be positive"
        }
    );
}

#[test]
fn a_rule_profile_mismatch_is_refused_rather_than_answered() {
    let graph = empty_graph(snapshot(1));

    // Same version, different profile: answering would mix two semantics.
    let mut mismatched = query(Budget::DEFAULT);
    mismatched.snapshot.rules = rules(RuleProfile::LegacyV1, 4);
    assert_eq!(
        mismatched.check_against(&graph).unwrap_err(),
        QueryError::RuleMismatch {
            reason: "rule set differs between query and graph"
        }
    );

    // Same profile, different digest: the rules text changed, so the digest must
    // not be ignored either.
    let mut digest_only = query(Budget::DEFAULT);
    digest_only.snapshot.rules = rules(RuleProfile::FormalV1, 9);
    assert_eq!(
        digest_only.check_against(&graph).unwrap_err(),
        QueryError::RuleMismatch {
            reason: "rule set differs between query and graph"
        }
    );

    // A different version is also refused, never silently re-pinned.
    let mut other_version = query(Budget::DEFAULT);
    other_version.snapshot.version = KbVersion::new(2);
    assert_eq!(
        other_version.check_against(&graph).unwrap_err(),
        QueryError::RuleMismatch {
            reason: "version differs between query and graph"
        }
    );

    // Matching everything is accepted.
    query(Budget::DEFAULT).check_against(&graph).unwrap();
}

#[test]
fn a_different_graph_digest_is_refused_even_at_the_same_version() {
    // The regression this guards: same KB, same commit number, same rules, but a
    // different graph — e.g. an index rebuilt from a different assertion set.
    // Comparing only version/rules would answer against the wrong graph.
    let mut graph_snapshot = snapshot(1);
    graph_snapshot.graph_digest = GraphDigest::new(Digest::from_bytes([5; 32]));
    let graph = empty_graph(graph_snapshot);

    let mut request = query(Budget::DEFAULT);
    request.snapshot.kb_uuid = graph_snapshot.kb_uuid;
    request.snapshot.version = graph_snapshot.version;
    request.snapshot.rules = graph_snapshot.rules;
    request.snapshot.graph_digest = GraphDigest::new(Digest::from_bytes([6; 32]));

    assert_eq!(
        request.check_against(&graph).unwrap_err(),
        QueryError::RuleMismatch {
            reason: "graph_digest differs between query and graph"
        },
        "graph_digest alone must be enough to reject the pairing"
    );

    // Matching digests are accepted.
    request.snapshot.graph_digest = graph_snapshot.graph_digest;
    request.check_against(&graph).unwrap();
}

#[test]
fn the_request_exposes_the_profile_it_was_read_under() {
    let request = query(Budget::DEFAULT);
    assert_eq!(request.rules().profile(), RuleProfile::FormalV1);
    assert_ne!(request.rules(), rules(RuleProfile::LegacyV1, 4));
}

#[test]
fn incomplete_results_are_errors_not_row_sets() {
    // There is no "limited" success variant: interruption is a typed failure, so
    // an empty row set can only mean a complete `absent`.
    let stop = QueryError::Stopped(ControlStop::DeadlineExceeded);
    assert_eq!(stop.to_string(), "evaluation stopped: deadline exceeded");
    assert_ne!(stop, QueryError::Stopped(ControlStop::Cancelled));
    assert_eq!(
        QueryError::BudgetExceeded(sage_db_types::BudgetLimit::Steps).to_string(),
        "budget exceeded: max_steps"
    );
}

#[test]
fn canonical_rows_exclude_proof_execution_and_engine_state() {
    let base = SourceResult {
        snapshot: snapshot(1),
        engine: EngineVersion::new(1),
        rows: vec![row(2, Confidence::Confirmed), row(3, Confidence::Contested)],
        proof: None,
        stats: sage_db_types::ExecStats {
            steps: 10,
            work_rows: 2,
            work_bytes: 64,
            proof_nodes: 0,
        },
    };

    let mut with_extra = base.clone();
    with_extra.stats.steps = 999;
    with_extra.engine = EngineVersion::new(2);
    assert_eq!(
        base.canonical_rows(),
        with_extra.canonical_rows(),
        "statistics and engine build are not semantic differences"
    );

    let mut reordered = base.clone();
    reordered.rows.reverse();
    assert_ne!(
        base.canonical_rows(),
        reordered.canonical_rows(),
        "row order is part of the canonical contract"
    );

    let mut disputed = base.clone();
    disputed.rows[0].disputed = true;
    assert_ne!(
        base.canonical_rows(),
        disputed.canonical_rows(),
        "a dispute marker is semantic, not cosmetic"
    );
}

#[test]
fn canonical_row_ignores_support_handle_and_proof_root() {
    let mut left = row(2, Confidence::Confirmed);
    left.strongest_support = Some(AssertionSupport {
        id: AssertionId::new(1),
        revision: RecordRevision::FIRST,
        confidence: Confidence::Confirmed,
    });
    let mut right = left;
    right.strongest_support = Some(AssertionSupport {
        id: AssertionId::new(2),
        revision: RecordRevision::new(4).unwrap(),
        confidence: Confidence::Confirmed,
    });
    assert_eq!(left.canonical(), right.canonical());

    // Proof handles are arena-local; the same fact proved differently is the same
    // fact.
    let mut proved = left;
    proved.proof = Some(ProofRef::new(7));
    assert_eq!(proved.canonical(), left.canonical());
}

#[test]
fn an_empty_complete_result_means_absent_with_no_proof_roots() {
    let mut arena = ProofArena::new();
    arena
        .push(
            ProofNode::Asserted {
                assertion_id: AssertionId::new(1),
                revision: RecordRevision::FIRST,
            },
            8,
            u64::MAX,
        )
        .unwrap();

    let no_rows_with_arena = SourceResult {
        snapshot: snapshot(1),
        engine: EngineVersion::new(1),
        rows: Vec::new(),
        proof: Some(arena),
        stats: sage_db_types::ExecStats::default(),
    };
    assert!(no_rows_with_arena.contains(TermId::new(2)).is_none());
    assert!(
        !no_rows_with_arena.proofs_complete(),
        "an empty result must not carry proof roots"
    );

    let empty = SourceResult {
        snapshot: snapshot(1),
        engine: EngineVersion::new(1),
        rows: Vec::new(),
        proof: Some(ProofArena::new()),
        stats: sage_db_types::ExecStats::default(),
    };
    assert!(empty.proofs_complete());
}

#[test]
fn every_row_must_have_a_resolvable_proof_root_when_proofs_are_requested() {
    let mut arena = ProofArena::new();
    let root = arena
        .push(
            ProofNode::Asserted {
                assertion_id: AssertionId::new(1),
                revision: RecordRevision::FIRST,
            },
            8,
            u64::MAX,
        )
        .unwrap();

    let mut first = row(2, Confidence::Confirmed);
    first.proof = Some(root);
    let mut second = row(3, Confidence::Hypothetical);
    second.proof = Some(root);

    let mut result = SourceResult {
        snapshot: snapshot(1),
        engine: EngineVersion::new(1),
        rows: vec![first, second],
        proof: Some(arena),
        stats: sage_db_types::ExecStats::default(),
    };
    assert!(result.proofs_complete());

    // One row missing its root is a contract violation, not a partial success.
    result.rows[1].proof = None;
    assert!(!result.proofs_complete());

    // A root that the arena cannot resolve is equally wrong.
    let mut dangling = row(4, Confidence::Confirmed);
    dangling.proof = Some(ProofRef::new(99));
    let mut second_result = SourceResult {
        snapshot: snapshot(1),
        engine: EngineVersion::new(1),
        rows: vec![dangling],
        proof: Some(ProofArena::new()),
        stats: sage_db_types::ExecStats::default(),
    };
    assert!(!second_result.proofs_complete());
    second_result.proof = None;
    assert!(
        !second_result.proofs_complete(),
        "no arena at all cannot satisfy a proof request"
    );
}

#[test]
fn transitive_proof_keeps_from_mid_to_order() {
    // left is from->mid, right is mid->to. Swapping them for "normalisation"
    // would describe a different composition.
    let mut arena = ProofArena::new();
    let left = arena
        .push(
            ProofNode::Asserted {
                assertion_id: AssertionId::new(10),
                revision: RecordRevision::FIRST,
            },
            8,
            u64::MAX,
        )
        .unwrap();
    let right = arena
        .push(
            ProofNode::Asserted {
                assertion_id: AssertionId::new(11),
                revision: RecordRevision::FIRST,
            },
            8,
            u64::MAX,
        )
        .unwrap();
    let composed = arena
        .push(ProofNode::Transitive { left, right }, 8, u64::MAX)
        .unwrap();
    assert_eq!(
        arena.get(composed),
        Some(ProofNode::Transitive { left, right }),
        "the composed node must not be reordered"
    );

    // The opposite composition is a distinct derivation, not the same node
    // normalised: a vocabulary that sorted its premises would hide a wrong
    // composition.
    let mirrored = arena
        .push(
            ProofNode::Transitive {
                left: right,
                right: left,
            },
            8,
            u64::MAX,
        )
        .unwrap();
    assert_ne!(arena.get(mirrored), arena.get(composed));
    assert_eq!(
        arena.get(mirrored),
        Some(ProofNode::Transitive {
            left: right,
            right: left
        })
    );
}

#[test]
fn proof_arena_rejects_forward_premises() {
    let mut arena = ProofArena::new();
    let error = arena
        .push(
            ProofNode::Symmetry {
                premise: ProofRef::new(5),
            },
            16,
            u64::MAX,
        )
        .unwrap_err();
    assert_eq!(
        error,
        ProofArenaError::ForwardReference {
            node: 0,
            premise: ProofRef::new(5),
        }
    );
    assert!(arena.is_empty(), "a rejected push must not consume a slot");
}

#[test]
fn proof_arena_rejects_a_self_referential_premise() {
    let mut arena = ProofArena::new();
    arena
        .push(
            ProofNode::Asserted {
                assertion_id: AssertionId::new(1),
                revision: RecordRevision::FIRST,
            },
            16,
            u64::MAX,
        )
        .unwrap();
    let error = arena
        .push(
            ProofNode::Symmetry {
                premise: ProofRef::new(1),
            },
            16,
            u64::MAX,
        )
        .unwrap_err();
    assert_eq!(
        error,
        ProofArenaError::ForwardReference {
            node: 1,
            premise: ProofRef::new(1),
        }
    );
}

#[test]
fn proof_arena_enforces_node_and_byte_budgets_without_partial_charge() {
    let mut arena = ProofArena::new();
    let node = ProofNode::Asserted {
        assertion_id: AssertionId::new(1),
        revision: RecordRevision::FIRST,
    };
    arena.push(node, 1, u64::MAX).unwrap();
    assert_eq!(
        arena.push(node, 1, u64::MAX).unwrap_err(),
        ProofArenaError::NodeBudgetExceeded { attempted: 2 }
    );
    assert_eq!(arena.len(), 1);
}

#[test]
fn proof_arena_charges_real_slots_and_capacity_not_a_guess() {
    // The charge must follow the actual in-memory layout and the actual capacity.
    let slot = ProofNode::slot_bytes();
    assert_eq!(slot, core::mem::size_of::<ProofNode>() as u64);
    assert!(
        slot >= 24,
        "a u128 payload plus discriminant cannot be smaller"
    );

    let leaf = |id: u128| ProofNode::Asserted {
        assertion_id: AssertionId::new(id),
        revision: RecordRevision::FIRST,
    };

    // Exactly one slot's worth of bytes admits exactly the first push.
    let mut arena = ProofArena::new();
    arena.push(leaf(1), 100, slot).unwrap();
    assert_eq!(arena.capacity(), 1);
    assert_eq!(arena.bytes(), slot);
    assert_eq!(arena.len(), 1);

    // The second push needs a second slot; with no room it must be refused.
    let error = arena.push(leaf(2), 100, slot).unwrap_err();
    assert!(
        matches!(error, ProofArenaError::ByteBudgetExceeded { .. }),
        "growth beyond the byte budget must be refused, got {error:?}"
    );
    // Refusal changed nothing: same content, same capacity, same charge.
    assert_eq!(arena.len(), 1);
    assert_eq!(arena.capacity(), 1);
    assert_eq!(arena.bytes(), slot);
    assert_eq!(arena.get(ProofRef::new(0)), Some(leaf(1)));
}

#[test]
fn proof_arena_reports_capacity_based_bytes() {
    // A doubling vector holds more slots than nodes. Charging `len * slot` would
    // understate what the process keeps alive, so the figure must track capacity.
    let leaf = |id: u128| ProofNode::Asserted {
        assertion_id: AssertionId::new(id),
        revision: RecordRevision::FIRST,
    };
    let mut arena = ProofArena::new();
    for id in 0..5 {
        arena.push(leaf(id), 100, u64::MAX).unwrap();
    }
    assert!(arena.capacity() >= arena.len());
    assert_eq!(
        arena.bytes(),
        (arena.capacity() as u64) * ProofNode::slot_bytes()
    );
    assert!(
        arena.bytes() >= (arena.len() as u64) * ProofNode::slot_bytes(),
        "capacity charge can never be below the logical length"
    );
}

#[test]
fn premises_are_reported_in_semantic_order() {
    let node = ProofNode::Transitive {
        left: ProofRef::new(1),
        right: ProofRef::new(2),
    };
    assert_eq!(
        node.premises().collect::<Vec<_>>(),
        vec![ProofRef::new(1), ProofRef::new(2)]
    );
    assert_eq!(node.premises().count(), 2);

    let leaf = ProofNode::Asserted {
        assertion_id: AssertionId::new(1),
        revision: RecordRevision::FIRST,
    };
    assert_eq!(leaf.premises().count(), 0);
}

#[test]
fn query_errors_are_displayable_and_distinct() {
    let messages = [
        QueryError::UnknownTerm(TermId::new(7)).to_string(),
        QueryError::RelationNotTransitive(RelationId::Enables).to_string(),
        QueryError::BudgetExceeded(sage_db_types::BudgetLimit::WorkRows).to_string(),
        QueryError::NotImplemented {
            executor: "reference",
        }
        .to_string(),
        QueryError::RuleMismatch {
            reason: "rule set differs between query and graph",
        }
        .to_string(),
    ];
    assert_eq!(messages[0], "unknown term 7");
    assert_eq!(messages[1], "relation enables is not transitive");
    assert_eq!(messages[2], "budget exceeded: max_work_rows");
    assert_eq!(messages[3], "reference executor is not implemented yet");
    assert_eq!(
        messages[4],
        "rule mismatch: rule set differs between query and graph"
    );
    assert_ne!(messages[0], messages[1]);
}

/// A control hook that refuses at a fixed checkpoint, standing in for a caller
/// cancelling the request or observing an expired deadline.
struct StopAfter {
    allowed: core::cell::Cell<u32>,
    stop: ControlStop,
}

impl QueryControl for StopAfter {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        let remaining = self.allowed.get();
        if remaining == 0 {
            return Err(self.stop);
        }
        self.allowed.set(remaining - 1);
        Ok(())
    }
}

#[test]
fn control_hook_is_caller_supplied_and_typed() {
    // The pure layer has no clock; stopping is a policy the caller injects.
    assert!(RunToCompletion.checkpoint().is_ok());

    let cancelled = StopAfter {
        allowed: core::cell::Cell::new(0),
        stop: ControlStop::Cancelled,
    };
    assert_eq!(cancelled.checkpoint(), Err(ControlStop::Cancelled));

    let expiring = StopAfter {
        allowed: core::cell::Cell::new(1),
        stop: ControlStop::DeadlineExceeded,
    };
    assert_eq!(expiring.checkpoint(), Ok(()));
    assert_eq!(expiring.checkpoint(), Err(ControlStop::DeadlineExceeded));
    assert_eq!(
        QueryError::from(ControlStop::Cancelled),
        QueryError::Stopped(ControlStop::Cancelled)
    );
}

#[test]
fn graph_construction_rejects_inputs_it_cannot_index() {
    // Two terms but a row allowance of one: refused at admission, not by panic.
    let terms = vec![
        TermMeta {
            id: TermId::new(1),
            layer: SemanticLayer::L1,
        },
        TermMeta {
            id: TermId::new(2),
            layer: SemanticLayer::L1,
        },
    ];
    let error = BaseGraph::new(
        snapshot(1),
        terms,
        vec![AssertionRecord {
            id: AssertionId::new(1),
            revision: RecordRevision::FIRST,
            from: TermId::new(1),
            relation: RelationId::IsA,
            target: TermId::new(2),
            confidence: Confidence::Confirmed,
        }],
        Budget {
            max_work_rows: 2,
            ..Budget::DEFAULT
        },
    )
    .unwrap_err();
    assert_eq!(
        error,
        GraphBuildError::BudgetExceeded {
            limit: sage_db_types::BuildLimit::Rows
        }
    );

    // The same input is fine with a realistic budget.
    BaseGraph::new(snapshot(1), Vec::new(), Vec::new(), Budget::DEFAULT).unwrap();
}

#[test]
fn control_interval_is_bounded() {
    // The bound is part of the contract: a caller must be able to rely on being
    // consulted at least this often, so it is a compile-time constant check.
    const { assert!(sage_db_types::CONTROL_CHECK_INTERVAL_STEPS > 0) };
    const { assert!(sage_db_types::CONTROL_CHECK_INTERVAL_STEPS <= 4_096) };
}

#[test]
fn source_flags_are_independent_of_each_other() {
    // The three flags answer three different questions and must not be collapsed:
    // asserted = authored in this direction, derived = non-circular rule proof,
    // disputed = any contested support anywhere. A confirmed fact can still be
    // disputed, and a reverse R2 reading is derived without being asserted.
    let mut reverse_reading = row(2, Confidence::Confirmed);
    reverse_reading.has_asserted = false;
    reverse_reading.has_derived = true;
    assert!(!reverse_reading.canonical().has_asserted);
    assert!(reverse_reading.canonical().has_derived);

    let mut both = row(3, Confidence::Hypothetical);
    both.has_asserted = true;
    both.has_derived = true;
    let mut disputed_confirmed = row(4, Confidence::Confirmed);
    disputed_confirmed.disputed = true;

    // Disagreement survives a stronger annotation; it is not a fourth level.
    assert_eq!(
        disputed_confirmed.annotation.confidence(),
        Some(Confidence::Confirmed)
    );
    assert!(disputed_confirmed.canonical().disputed);

    // And the flags remain part of the comparable contract, so an executor that
    // forgets one is caught by differential comparison rather than passing.
    let mut forgotten = both;
    forgotten.has_derived = false;
    assert_ne!(both.canonical(), forgotten.canonical());
}

#[test]
fn same_digest_but_different_profile_is_refused() {
    // The reason RuleSet carries the profile at all: an equal digest must not be
    // able to stand in for an equal rule set. This pairing is exactly what a
    // digest-only contract would accept.
    let digest = RulesDigest::new(Digest::from_bytes([4; 32]));
    let graph_rules = RuleSet::from_trusted_parts(RuleProfile::FormalV1, digest);
    let query_rules = RuleSet::from_trusted_parts(RuleProfile::LegacyV1, digest);
    assert_eq!(
        graph_rules.digest(),
        query_rules.digest(),
        "the digests are equal, so a digest-only check would pass"
    );
    assert_ne!(
        graph_rules, query_rules,
        "the pairs must still differ because the profiles differ"
    );

    let mut graph_snapshot = snapshot(1);
    graph_snapshot.rules = graph_rules;
    let graph = empty_graph(graph_snapshot);

    let mut request = query(Budget::DEFAULT);
    request.snapshot.rules = query_rules;
    assert_eq!(
        request.check_against(&graph).unwrap_err(),
        QueryError::RuleMismatch {
            reason: "rule set differs between query and graph"
        }
    );
}

#[test]
fn tightening_bytes_below_resident_rejects_the_next_push() {
    // The shared-work-budget case: another work area grew, so the caller lowers
    // this arena's allowance below what it already holds. Spare capacity must not
    // let one more push slip through.
    let leaf = |id: u128| ProofNode::Asserted {
        assertion_id: AssertionId::new(id),
        revision: RecordRevision::FIRST,
    };

    let mut arena = ProofArena::new();
    for id in 0..3 {
        arena.push(leaf(id), 100, u64::MAX).unwrap();
    }
    // Four slots allocated, three used: there is spare capacity to fill.
    assert_eq!(arena.capacity(), 4);
    assert_eq!(arena.len(), 3);
    let resident = arena.bytes();
    assert_eq!(resident, 4 * ProofNode::slot_bytes());

    let tightened = resident - 1;
    let before_capacity = arena.capacity();
    let error = arena.push(leaf(99), 100, tightened).unwrap_err();
    assert!(
        matches!(error, ProofArenaError::ByteBudgetExceeded { .. }),
        "a push into spare capacity must still respect the lowered limit, got {error:?}"
    );

    // Nothing changed: no node added, no capacity growth, no re-charge.
    assert_eq!(arena.len(), 3);
    assert_eq!(arena.capacity(), before_capacity);
    assert_eq!(arena.bytes(), resident);
    assert_eq!(arena.get(ProofRef::new(0)), Some(leaf(0)));
    assert_eq!(
        arena.get(ProofRef::new(3)),
        None,
        "the refusal wrote nothing"
    );
}
