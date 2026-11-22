//! P3 exact fact-query tests: complete answers, point facts and explanations.
//!
//! Two kinds of evidence, deliberately separated:
//!
//! * a **hand-authored** `SnapshotView` fixture with an explicit chain and diamond, so
//!   every expected row is written by hand from the rules rather than read back from
//!   any executor;
//! * a **real kernel `Snapshot`** over a real `bootstrap`ped store for one integration
//!   case, so the wrapper is proven against the production adapter and not only
//!   against a stand-in.
//!
//! What this file does not claim: display truncation, transport encoding, `Requires`
//! ordering, search, caching, or any whole-product readiness. Test-local allocations
//! are test-local and are not allocator evidence.

#![allow(dead_code)]

#[path = "../../store-tests/tests/support/shared_fs.rs"]
mod shared_fs;

use std::path::Path;
use std::rc::Rc;

use shared_fs::SharedFs;

/// Rules bytes produced by the canonical encoder for the profile under test.
///
/// Reusing `store-tests`' checked-in fixture would need a fifth file in this crate,
/// and the fixture's `include_bytes!` is relative to its own crate, so the canonical
/// encoder is both the smaller and the more honest source: these are the bytes the
/// store will actually commit.
fn rules_bytes() -> Vec<u8> {
    let mut ledger = sage_db_store::lease::AttemptLedger::open(
        u64::MAX,
        u64::MAX,
        &sage_db_types::query::RunToCompletion,
    )
    .expect("ledger");
    sage_db_format::rules::CanonicalRules::new(PROFILE)
        .encode(&mut ledger)
        .expect("the canonical rules encoder accepts its own profile")
}

use sage_db_format::assertion::{AssertionEvent, write_assertion};
use sage_db_format::event::encode_with;
use sage_db_format::term::TermEvent;
use sage_db_format::wire::Reader;
use sage_db_kernel::snapshot::{Expectation, Snapshot};
use sage_db_query::facts::{
    FactQuery, FactQueryError, SnapshotField, answer_relation, explain_fact, point_fact,
};
use sage_db_store::fs::CreateDir;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_types::budget::Budget;
use sage_db_types::confidence::Confidence;
use sage_db_types::digest::{Digest, GraphDigest, RulesDigest};
use sage_db_types::graph::{BaseGraph, SnapshotId};
use sage_db_types::identity::{KbUuid, KbVersion, RecordRevision, TermId};
use sage_db_types::persist::{
    ActorOrigin, AssertionRecordFull, CommonFields, Extensions, RecordKey, RecordKind, RevPair,
};
use sage_db_types::proof::ProofNode;
use sage_db_types::query::{ControlStop, QueryControl, QueryError};
use sage_db_types::record::TermMeta;
use sage_db_types::relation::{RelationId, RuleProfile, RuleSet};
use sage_db_types::snapshot::{
    CommittedRules, ReadRefusal, RecordRef, RecordView, ScanError, SnapshotFamily, SnapshotView,
};

/// The canonical rules the fixture store commits.
const PROFILE: RuleProfile = RuleProfile::FormalV1;

fn digest_of(profile: RuleProfile) -> RulesDigest {
    let _ = profile;
    RulesDigest::new(Digest::from_bytes(sage_db_format::digest::raw_sha256(
        &rules_bytes(),
    )))
}

fn fixture_rules() -> CommittedRules {
    CommittedRules {
        profile: PROFILE,
        rules_digest: digest_of(PROFILE),
        rules_ref: Digest::from_bytes(sage_db_format::digest::raw_sha256(&rules_bytes())),
    }
}

fn snapshot_id(version: u64) -> SnapshotId {
    SnapshotId {
        kb_uuid: KbUuid::from_bytes([0x5b; 16]),
        version: KbVersion::new(version),
        rules: RuleSet::from_trusted_parts(PROFILE, digest_of(PROFILE)),
        graph_digest: GraphDigest::new(Digest::from_bytes([0x5a; 32])),
    }
}

// ---------------------------------------------------------------------------
// A hand-authored view fixture
// ---------------------------------------------------------------------------

/// A `SnapshotView` over a hand-built graph.
///
/// The graph is constructed through `BaseGraph::new`, i.e. the real validator and
/// indexer, so the fixture cannot contain a shape the production graph would reject.
/// Record reads are unsupported: this crate never reads records, and answering
/// `Absent` there would be a false claim about stored data.
struct HandView {
    id: SnapshotId,
    graph: BaseGraph,
}

impl HandView {
    /// Layers are explicit because `is-a` is a definition relation: its target must
    /// sit on a strictly shallower layer, and a fixture that put every term on L1
    /// would be rejected by the profile before any query ran.
    fn build(
        id: SnapshotId,
        terms: &[(u32, sage_db_types::layer::SemanticLayer)],
        edges: &[(u32, RelationId, u32, Confidence)],
    ) -> Self {
        let term_metas: Vec<TermMeta> = terms
            .iter()
            .map(|(id, layer)| TermMeta {
                id: TermId::new(*id),
                layer: *layer,
            })
            .collect();
        let assertions: Vec<sage_db_types::record::AssertionRecord> = edges
            .iter()
            .enumerate()
            .map(|(index, (from, relation, to, confidence))| {
                sage_db_types::record::AssertionRecord {
                    id: sage_db_types::identity::AssertionId::new(index as u128 + 1),
                    revision: RecordRevision::FIRST,
                    from: TermId::new(*from),
                    relation: *relation,
                    target: TermId::new(*to),
                    confidence: *confidence,
                }
            })
            .collect();
        let graph = BaseGraph::new(id, term_metas, assertions, Budget::DEFAULT)
            .expect("the hand-authored graph must be valid");
        Self { id, graph }
    }
}

impl SnapshotView for HandView {
    fn basis(&self) -> SnapshotId {
        self.id
    }
    fn rules(&self) -> CommittedRules {
        fixture_rules()
    }
    fn graph(&self) -> &BaseGraph {
        &self.graph
    }
    fn record(
        &self,
        _key: RecordKey,
        _ledger: &mut dyn sage_db_types::ledger::SharedLedger,
    ) -> Result<RecordView<'_>, ReadRefusal> {
        Err(ReadRefusal::UnsupportedFamily(RecordKind::Term))
    }
    fn scan_family<'a, E>(
        &'a self,
        _family: SnapshotFamily,
        _ledger: &mut dyn sage_db_types::ledger::SharedLedger,
        _visitor: &mut dyn FnMut(RecordRef<'a>) -> Result<(), E>,
    ) -> Result<(), ScanError<E>> {
        Err(ScanError::Refused(ReadRefusal::UnsupportedFamily(
            RecordKind::Term,
        )))
    }
}

const L0: sage_db_types::layer::SemanticLayer = sage_db_types::layer::SemanticLayer::L0;
const L1: sage_db_types::layer::SemanticLayer = sage_db_types::layer::SemanticLayer::L1;
const L2: sage_db_types::layer::SemanticLayer = sage_db_types::layer::SemanticLayer::L2;
const L3: sage_db_types::layer::SemanticLayer = sage_db_types::layer::SemanticLayer::L3;

fn query_for(id: SnapshotId, source: u32, relation: RelationId, with_proof: bool) -> FactQuery {
    FactQuery {
        snapshot: id,
        source: TermId::new(source),
        relation,
        with_proof,
        budget: Budget::DEFAULT,
    }
}

// ---------------------------------------------------------------------------
// Complete answers over an authored chain and diamond
// ---------------------------------------------------------------------------

#[test]
fn a_chain_reports_the_complete_reachable_set_with_authored_rows() {
    // 1 -is-a-> 2 -is-a-> 3 -is-a-> 4, all confirmed. `is-a` is a *definition*
    // relation under this profile: each target must sit on a strictly shallower layer,
    // so the terms descend L3→L0 and every edge is legal. From 1 the reachable set is
    // {2,3,4}.
    let id = snapshot_id(7);
    let view = HandView::build(
        id,
        &[(1, L3), (2, L2), (3, L1), (4, L0)],
        &[
            (1, RelationId::Instantiates, 2, Confidence::Confirmed),
            (1, RelationId::IsA, 2, Confidence::Confirmed),
            (2, RelationId::IsA, 3, Confidence::Confirmed),
            (3, RelationId::IsA, 4, Confidence::Confirmed),
        ],
    );
    let answer = answer_relation(
        &view,
        query_for(id, 1, RelationId::IsA, false),
        &sage_db_types::query::RunToCompletion,
    )
    .expect("the chain must solve");

    // Bound to the view's own basis, not to anything the caller passed separately.
    assert_eq!(answer.basis(), view.basis());
    assert_eq!(answer.len(), 3, "1 reaches 2, 3 and 4");

    let row2 = answer.row(TermId::new(2)).expect("2 is directly asserted");
    assert_eq!(row2.from, TermId::new(1));
    assert_eq!(row2.relation, RelationId::IsA);
    assert!(row2.has_asserted, "1->2 is authored in this direction");
    assert_eq!(row2.annotation.confidence(), Some(Confidence::Confirmed));

    let row4 = answer.row(TermId::new(4)).expect("4 is reachable");
    assert!(
        !row4.has_asserted,
        "1->4 is not authored; it is only reached transitively"
    );
    assert!(row4.has_derived, "a two-hop reach is derived");
    assert!(!row4.disputed);
}

#[test]
fn a_diamond_reports_both_paths_without_duplicating_the_object() {
    // 1 -> 2, 1 -> 3 (confirmed), 2 -> 4 (confirmed), 3 -> 4 (contested).
    // 4 is reached by two paths and must appear once, with the conflicting support
    // recorded as disputed even though the effective annotation is stronger.
    let id = snapshot_id(9);
    let view = HandView::build(
        id,
        &[(1, L3), (2, L2), (3, L2), (4, L1)],
        &[
            (1, RelationId::Instantiates, 2, Confidence::Confirmed),
            (1, RelationId::IsA, 2, Confidence::Confirmed),
            (1, RelationId::IsA, 3, Confidence::Confirmed),
            (2, RelationId::IsA, 4, Confidence::Confirmed),
            (3, RelationId::IsA, 4, Confidence::Contested),
        ],
    );
    let answer = answer_relation(
        &view,
        query_for(id, 1, RelationId::IsA, true),
        &sage_db_types::query::RunToCompletion,
    )
    .expect("the diamond must solve");

    assert_eq!(answer.len(), 3, "2, 3 and 4 — 4 exactly once");
    let occurrences = answer
        .rows()
        .iter()
        .filter(|row| row.to == TermId::new(4))
        .count();
    assert_eq!(occurrences, 1, "a reachable object appears once");

    let row4 = answer.row(TermId::new(4)).expect("4 is reachable");
    assert!(row4.disputed, "a contested support disputes the fact");
    assert_eq!(
        row4.annotation.confidence(),
        Some(Confidence::Confirmed),
        "max over supports keeps the stronger annotation"
    );
    // Every row of a proof request has a root inside the answer's own arena.
    let arena = answer.proof().expect("proofs were requested");
    assert!(
        answer
            .rows()
            .iter()
            .all(|row| row.proof.is_some_and(|root| arena.get(root).is_some()))
    );
}

#[test]
fn proof_roots_replay_through_the_real_replay_checker() {
    let id = snapshot_id(11);
    let view = HandView::build(
        id,
        &[(1, L3), (2, L2), (3, L1)],
        &[
            (1, RelationId::Instantiates, 2, Confidence::Confirmed),
            (1, RelationId::IsA, 2, Confidence::Confirmed),
            (2, RelationId::IsA, 3, Confidence::Confirmed),
        ],
    );
    let answer = answer_relation(
        &view,
        query_for(id, 1, RelationId::IsA, true),
        &sage_db_types::query::RunToCompletion,
    )
    .expect("solves");
    let arena = answer.proof().expect("proofs");

    let replayed = sage_db_infer::proof::replay_rows(
        view.graph(),
        arena,
        answer.rows(),
        &sage_db_types::query::RunToCompletion,
        Budget::DEFAULT,
    )
    .expect("every returned root must replay");
    assert_eq!(replayed.len(), answer.len());
    for (replayed, row) in replayed.iter().zip(answer.rows()) {
        assert_eq!(replayed.from, row.from);
        assert_eq!(replayed.to, row.to);
        assert_eq!(
            replayed.annotation.confidence(),
            row.annotation.confidence()
        );
    }
    // A leaf of the transitive proof is an authored assertion, not a bare claim.
    assert!(
        arena
            .nodes()
            .iter()
            .any(|node| matches!(node, ProofNode::Asserted { .. }))
    );
}

// ---------------------------------------------------------------------------
// Point facts
// ---------------------------------------------------------------------------

#[test]
fn a_point_fact_is_present_or_definitively_absent() {
    let id = snapshot_id(13);
    let view = HandView::build(
        id,
        &[(1, L2), (2, L1), (3, L0)],
        &[
            (1, RelationId::IsA, 2, Confidence::Confirmed),
            (1, RelationId::IsA, 3, Confidence::Confirmed),
        ],
    );
    let control = sage_db_types::query::RunToCompletion;

    let present = point_fact(
        &view,
        query_for(id, 1, RelationId::IsA, false),
        TermId::new(2),
        &control,
    )
    .expect("a point query must solve");
    assert!(present.is_present());
    assert_eq!(present.row().map(|row| row.to), Some(TermId::new(2)));
    // The owning answer is retained, and the row points into it.
    assert_eq!(present.answer().len(), 2);

    // A KNOWN but unreachable object is a definitive absence, carrying the complete
    // answer that decided it. (A term cannot reach itself.)
    let known_unreachable = point_fact(
        &view,
        query_for(id, 1, RelationId::IsA, false),
        TermId::new(1),
        &control,
    )
    .expect("a known object is a complete answer");
    assert!(!known_unreachable.is_present());
    assert_eq!(known_unreachable.answer().len(), 2);

    // An UNKNOWN object is the typed unknown, never absence: "does not exist" and
    // "exists but unreachable" are different operator answers (`query-dsl.md` §3).
    assert!(matches!(
        point_fact(
            &view,
            query_for(id, 1, RelationId::IsA, false),
            TermId::new(3_000),
            &control
        ),
        Err(FactQueryError::Executor(QueryError::UnknownTerm(term)))
            if term == TermId::new(3_000)
    ));

    // A relation nobody authored is empty, not an error.
    let empty = answer_relation(
        &view,
        query_for(id, 1, RelationId::Contradicts, false),
        &control,
    )
    .expect("a valid query over an unauthored relation solves");
    assert!(empty.is_empty());
    let absent_2 = point_fact(
        &view,
        query_for(id, 1, RelationId::Contradicts, false),
        TermId::new(2),
        &control,
    )
    .expect("solves");
    assert!(!absent_2.is_present());
}

#[test]
fn an_unknown_source_is_an_error_not_an_absent_answer() {
    let id = snapshot_id(15);
    let view = HandView::build(
        id,
        &[(1, L2), (2, L1)],
        &[(1, RelationId::IsA, 2, Confidence::Confirmed)],
    );
    let control = sage_db_types::query::RunToCompletion;

    assert!(matches!(
        answer_relation(&view, query_for(id, 99, RelationId::IsA, false), &control),
        Err(FactQueryError::Executor(QueryError::UnknownTerm(term))) if term == TermId::new(99)
    ));
    // A point query over an unknown source must not be spellable as absence.
    assert!(matches!(
        point_fact(
            &view,
            query_for(id, 99, RelationId::IsA, false),
            TermId::new(2),
            &control
        ),
        Err(FactQueryError::Executor(QueryError::UnknownTerm(_)))
    ));
}

// ---------------------------------------------------------------------------
// Snapshot binding
// ---------------------------------------------------------------------------

#[test]
fn a_request_naming_another_snapshot_is_refused_field_by_field() {
    let id = snapshot_id(17);
    let view = HandView::build(
        id,
        &[(1, L2), (2, L1)],
        &[(1, RelationId::IsA, 2, Confidence::Confirmed)],
    );
    let control = sage_db_types::query::RunToCompletion;

    // Version: the caller pinned a later state than the view serves.
    let mut wrong = id;
    wrong.version = KbVersion::new(18);
    assert!(matches!(
        answer_relation(&view, query_for(wrong, 1, RelationId::IsA, false), &control),
        Err(FactQueryError::SnapshotMismatch {
            field: SnapshotField::Version,
            ..
        })
    ));

    // KB: a different database entirely.
    let mut other_kb = id;
    other_kb.kb_uuid = KbUuid::from_bytes([0x7e; 16]);
    assert!(matches!(
        answer_relation(
            &view,
            query_for(other_kb, 1, RelationId::IsA, false),
            &control
        ),
        Err(FactQueryError::SnapshotMismatch {
            field: SnapshotField::Kb,
            ..
        })
    ));

    // Graph: same version, different contents. This is the field a version-only
    // comparison would miss.
    let mut other_graph = id;
    other_graph.graph_digest = GraphDigest::new(Digest::from_bytes([0x11; 32]));
    assert!(matches!(
        answer_relation(
            &view,
            query_for(other_graph, 1, RelationId::IsA, false),
            &control
        ),
        Err(FactQueryError::SnapshotMismatch {
            field: SnapshotField::GraphDigest,
            ..
        })
    ));

    // Rules: same graph, different rule configuration.
    let mut other_rules = id;
    other_rules.rules = RuleSet::from_trusted_parts(RuleProfile::LegacyV1, digest_of(PROFILE));
    assert!(matches!(
        answer_relation(
            &view,
            query_for(other_rules, 1, RelationId::IsA, false),
            &control
        ),
        Err(FactQueryError::SnapshotMismatch {
            field: SnapshotField::Rules,
            ..
        })
    ));

    // The matching request still works: the refusals above are about the mismatch,
    // not about the view being unusable.
    assert!(answer_relation(&view, query_for(id, 1, RelationId::IsA, false), &control).is_ok());
}

// ---------------------------------------------------------------------------
// Witnesses: real budget, real stop, display limits
// ---------------------------------------------------------------------------

struct Stop;
impl QueryControl for Stop {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        Err(ControlStop::Cancelled)
    }
}

/// Stops on the *second* call, so entry succeeds and the exit (or a mid-walk) check
/// fails: an empty result must still surface the stop rather than "no facts".
struct StopAfterEntry {
    calls: core::cell::Cell<u32>,
}
impl QueryControl for StopAfterEntry {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        let n = self.calls.get() + 1;
        self.calls.set(n);
        if n > 1 {
            Err(ControlStop::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[test]
fn a_stop_is_an_error_even_when_the_result_would_be_empty() {
    let id = snapshot_id(19);
    // No Contradicts edges at all: the honest answer is an empty result, so this test
    // cannot pass by accidentally finding nothing after a truncation.
    let view = HandView::build(
        id,
        &[(1, L2), (2, L1)],
        &[(1, RelationId::IsA, 2, Confidence::Confirmed)],
    );

    assert!(matches!(
        answer_relation(
            &view,
            query_for(id, 1, RelationId::Contradicts, false),
            &Stop
        ),
        Err(FactQueryError::Executor(QueryError::Stopped(
            ControlStop::Cancelled
        )))
    ));

    let control = StopAfterEntry {
        calls: core::cell::Cell::new(0),
    };
    assert!(matches!(
        point_fact(
            &view,
            query_for(id, 1, RelationId::Contradicts, false),
            TermId::new(2),
            &control
        ),
        Err(FactQueryError::Executor(QueryError::Stopped(_)))
    ));
}

#[test]
fn a_real_work_budget_is_an_error_and_never_an_absent_fact() {
    let id = snapshot_id(21);
    let view = HandView::build(
        id,
        &[(1, L2), (2, L1), (3, L0)],
        &[
            (1, RelationId::IsA, 2, Confidence::Confirmed),
            (2, RelationId::IsA, 3, Confidence::Confirmed),
        ],
    );
    let control = sage_db_types::query::RunToCompletion;

    // A zero work-row allowance cannot complete any evaluation.
    let mut starved = query_for(id, 1, RelationId::IsA, false);
    starved.budget.max_work_rows = 0;
    match point_fact(&view, starved, TermId::new(2), &control) {
        Err(FactQueryError::Executor(QueryError::BudgetExceeded(_))) => {}
        other => panic!("a starved budget must refuse, not answer: {other:?}"),
    }

    // A zero step allowance likewise.
    let mut no_steps = query_for(id, 1, RelationId::IsA, false);
    no_steps.budget.max_steps = 0;
    assert!(matches!(
        answer_relation(&view, no_steps, &control),
        Err(FactQueryError::Executor(QueryError::InvalidRequest { .. }))
    ));
}

#[test]
fn display_limits_never_change_the_answer_or_point_truth() {
    let id = snapshot_id(23);
    let view = HandView::build(
        id,
        &[(1, L2), (2, L1), (3, L0), (4, L0)],
        &[
            (1, RelationId::IsA, 2, Confidence::Confirmed),
            (1, RelationId::IsA, 3, Confidence::Confirmed),
            (1, RelationId::IsA, 4, Confidence::Confirmed),
        ],
    );
    let control = sage_db_types::query::RunToCompletion;

    let full = answer_relation(&view, query_for(id, 1, RelationId::IsA, false), &control)
        .expect("complete");
    assert_eq!(full.len(), 3);

    // `max_rows = 0` is rejected on **shape** by the accepted P0 interface
    // (`SourceQuery::validate`), not consulted as a display limit. This layer preserves
    // that exactly: it must not become a truncation and must not be silently accepted.
    let mut no_display = query_for(id, 1, RelationId::IsA, false);
    no_display.budget.max_rows = 0;
    assert!(
        matches!(
            answer_relation(&view, no_display, &control),
            Err(FactQueryError::Executor(QueryError::InvalidRequest { .. }))
        ),
        "max_rows = 0 keeps the P0 shape rejection; it never truncates and never answers"
    );

    // max_output_bytes is not consulted by the executor at all, so the rows are equal.
    let mut tiny_output = query_for(id, 1, RelationId::IsA, false);
    tiny_output.budget.max_output_bytes = 0;
    let answer = answer_relation(&view, tiny_output, &control).expect("complete");
    assert_eq!(answer.len(), full.len());
    assert_eq!(answer.rows(), full.rows(), "rows are identical");
}

// ---------------------------------------------------------------------------
// Explanation
// ---------------------------------------------------------------------------

#[test]
fn explain_requires_a_proof_and_returns_the_rows_own_root() {
    let id = snapshot_id(25);
    let view = HandView::build(
        id,
        &[(1, L2), (2, L1), (3, L0)],
        &[
            (1, RelationId::IsA, 2, Confidence::Confirmed),
            (2, RelationId::IsA, 3, Confidence::Confirmed),
        ],
    );
    let control = sage_db_types::query::RunToCompletion;

    // Without a proof request the layer refuses rather than answering without evidence.
    assert!(matches!(
        explain_fact(
            &view,
            query_for(id, 1, RelationId::IsA, false),
            TermId::new(3),
            &control
        ),
        Err(FactQueryError::Executor(QueryError::InvalidRequest { .. }))
    ));

    let explained = explain_fact(
        &view,
        query_for(id, 1, RelationId::IsA, true),
        TermId::new(3),
        &control,
    )
    .expect("explain with proofs solves");
    let row = explained.row().expect("3 is reachable");
    let root = row.proof.expect("the row carries its proof root");
    let arena = explained.answer().proof().expect("arena");
    assert!(
        arena.get(root).is_some(),
        "the root must resolve inside the retained arena"
    );

    // The proof is real: replaying it re-derives the row's annotation.
    let replayed = sage_db_infer::proof::replay_rows(
        view.graph(),
        arena,
        explained.answer().rows(),
        &control,
        Budget::DEFAULT,
    )
    .expect("replays");
    assert!(
        replayed
            .iter()
            .any(|fact| fact.to == TermId::new(3) && fact.annotation.confidence().is_some())
    );
}

// ---------------------------------------------------------------------------
// Real kernel snapshot integration
// ---------------------------------------------------------------------------

/// Test-local budget for attempt ledgers. Large enough that these tests are not
/// about ledger exhaustion; the real work budget is exercised separately.
const BYTES: u64 = 256 * 1024 * 1024;
const ENTRIES: u64 = 2_000_000;
const ROOT: &str = "/p3-query-fixture";

fn real_ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &sage_db_types::query::RunToCompletion).expect("ledger")
}

/// Assemble one assertion event through the canonical writer, then read it back with
/// the canonical reader, so the seeded image is what a writer would persist.
fn assertion_event(record: &AssertionRecordFull) -> sage_db_types::prepared::OwnedEvent {
    let mut l = real_ledger();
    let bytes = encode_with(&mut l, |writer| {
        write_assertion(writer, AssertionEvent::Created, record)
    })
    .expect("the canonical writer accepts the assertion");
    let mut reader = Reader::new(&bytes);
    let event = sage_db_format::event::read_event(&mut reader, &mut l)
        .expect("the canonical reader accepts its own bytes");
    let mut fields = Vec::new();
    for field in event.fields() {
        fields.push((field.tag(), field.value().to_vec()));
    }
    sage_db_types::prepared::OwnedEvent {
        tag: event.tag(),
        fields,
    }
}

fn assertion(id: u128, from: u32, to: u32) -> AssertionRecordFull {
    AssertionRecordFull {
        key: RecordKey::new(RecordKind::Assertion, id),
        revision: RecordRevision::FIRST,
        retracted: false,
        from: RecordKey::term(from),
        relation: RelationId::IsA,
        target: RecordKey::term(to),
        confidence: Confidence::Confirmed,
        declaration_order: 1,
        common: CommonFields {
            recorded_by: Some("p3-fixture".into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        legacy_id: None,
        legacy_content_digest: None,
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

/// A real store with two terms and one authored `is-a` assertion.
///
/// Terms and the assertion are committed in ONE transaction whose versions are derived
/// from the store's actual head, so this helper works whether genesis is version 1 or
/// the fixture has already advanced.
fn real_store() -> (
    Rc<sage_db_store_tests::model::fs_model::ModelFileSystem>,
    SharedFs,
    Store<SharedFs>,
) {
    let (inner, fs, mut store) = fixture_store();
    let mut l = real_ledger();

    // Two real terms through the canonical term writer: L2 -> L1, which `is-a`
    // accepts as a definition direction under the active profile.
    let term_one = term_record(1, L2);
    let term_two = term_record(2, L1);
    let mut events = vec![
        term_event(TermEvent::Created, &term_one),
        term_event(TermEvent::Created, &term_two),
    ];
    let record = assertion(1, 1, 2);
    events.push(assertion_event(&record));

    let head = store.head().expect("genesis published a head");
    let version = head + 1;
    let basis = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, &mut l)
        .expect("basis");
    let affected = vec![
        RevPair {
            key: RecordKey::term(1),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: RecordKey::term(2),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: record.key,
            revision: RecordRevision::FIRST,
        },
    ];
    let result_bytes =
        sage_db_store::typed::result_payload(version, &affected, &[], &mut l).expect("result");
    let result_ref = sage_db_format::digest::raw_sha256(&result_bytes);
    let mut outcome_fields: Vec<(u16, Vec<u8>)> = Vec::new();
    let text = |value: &str| {
        let mut out = (value.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(value.as_bytes());
        out
    };
    outcome_fields.push((5, text("p3-fixture")));
    outcome_fields.push((6, 0u16.to_le_bytes().to_vec()));
    outcome_fields.push((8, 1_700_000_000_000_500i64.to_le_bytes().to_vec()));
    outcome_fields.push((176, text("p3-fixture")));
    outcome_fields.push((177, text("p3-seed")));
    outcome_fields.push((178, vec![version as u8; 32]));
    outcome_fields.push((179, 0u16.to_le_bytes().to_vec()));
    outcome_fields.push((180, result_ref.to_vec()));
    outcome_fields.push((181, version.to_le_bytes().to_vec()));
    outcome_fields.sort_by_key(|(tag, _)| *tag);
    events.push(sage_db_types::prepared::OwnedEvent {
        tag: 192,
        fields: outcome_fields,
    });

    let mut blobs = vec![sage_db_types::prepared::BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: sage_db_types::prepared::BlobSource::Existing,
    }];
    for (id, body) in [(1u32, term_body(1)), (2u32, term_body(2))] {
        let _ = id;
        blobs.push(sage_db_types::prepared::BlobDeclaration {
            sha256: sage_db_format::digest::raw_sha256(&body),
            declared_len: body.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(body),
        });
    }
    blobs.push(sage_db_types::prepared::BlobDeclaration {
        sha256: result_ref,
        declared_len: result_bytes.len() as u64,
        source: sage_db_types::prepared::BlobSource::Provide(result_bytes),
    });

    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: Some(head),
        tx_id: [0x9b; 16],
        kb_uuid: KbUuid::from_bytes([0x5b; 16]),
        created_at: 1_700_000_000_000_600,
        principal_id: "p3-fixture".into(),
        request_id: "p3-seed".into(),
        payload_digest: [version as u8; 32],
        expect_version: Some(head),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &sage_db_types::framing::Limits::V1, &mut l) {
        HandleOutcome::Committed {
            version: committed, ..
        } => assert_eq!(committed, version),
        other => panic!("seed commit failed: {other:?}"),
    }
    (inner, fs, store)
}

fn term_body(id: u32) -> Vec<u8> {
    format!("p3-query-term-body/{id}").into_bytes()
}

fn term_record(
    id: u32,
    layer: sage_db_types::layer::SemanticLayer,
) -> sage_db_types::persist::TermRecord {
    sage_db_types::persist::TermRecord {
        key: RecordKey::term(id),
        revision: RecordRevision::FIRST,
        retired: false,
        slug: format!("p3-term-{id}"),
        atom: format!("p3 term {id}"),
        layer,
        domains: vec!["default".into()],
        source: sage_db_types::persist::TermSource::Personal,
        confidence: Confidence::Confirmed,
        triggers: vec!["p3".into()],
        body_ref: sage_db_format::digest::raw_sha256(&term_body(id)),
        common: CommonFields {
            recorded_by: Some("p3-fixture".into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        legacy_id: None,
        legacy_source_path: None,
        legacy_content_digest: None,
        legacy_source_file_digest: None,
        legacy_source_revision: None,
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

/// One term event, encoded and read back through the canonical codecs.
fn term_event(
    event: sage_db_format::term::TermEvent,
    record: &sage_db_types::persist::TermRecord,
) -> sage_db_types::prepared::OwnedEvent {
    let mut l = real_ledger();
    let bytes = encode_with(&mut l, |writer| {
        sage_db_format::term::write_term(writer, event, record)
    })
    .expect("the canonical term writer accepts the fixture");
    let mut reader = Reader::new(&bytes);
    let decoded = sage_db_format::event::read_event(&mut reader, &mut l)
        .expect("the canonical reader accepts its own bytes");
    let mut fields = Vec::new();
    for field in decoded.fields() {
        fields.push((field.tag(), field.value().to_vec()));
    }
    sage_db_types::prepared::OwnedEvent {
        tag: decoded.tag(),
        fields,
    }
}

fn fixture_store() -> (
    Rc<sage_db_store_tests::model::fs_model::ModelFileSystem>,
    SharedFs,
    Store<SharedFs>,
) {
    let inner = Rc::new(sage_db_store_tests::model::fs_model::ModelFileSystem::new(
        sage_db_store_tests::model::fs_model::ModelCapabilities::full(),
    ));
    let fs = SharedFs::from_rc(Rc::clone(&inner));
    let mut l = real_ledger();
    let store = bootstrap(
        fs.clone(),
        Path::new(ROOT),
        CreateDir::IfMissing,
        KbUuid::from_bytes([0x5b; 16]),
        &rules_bytes(),
        &sage_db_types::framing::Limits::V1,
        &mut l,
    )
    .expect("bootstrap");
    (inner, fs, store)
}

#[test]
fn the_real_kernel_snapshot_answers_through_this_layer() {
    let (_inner, _fs, mut store) = real_store();
    let mut l = real_ledger();
    let snapshot = Snapshot::open(&mut store, Expectation::latest(), &mut l)
        .expect("the fixture root must produce a snapshot");

    // The real adapter's basis is what every answer binds to.
    let basis = snapshot.basis();
    let query = FactQuery {
        snapshot: basis,
        source: TermId::new(1),
        relation: RelationId::IsA,
        with_proof: true,
        budget: Budget::DEFAULT,
    };

    let answer = answer_relation(&snapshot, query, &sage_db_types::query::RunToCompletion)
        .expect("the real snapshot must answer");
    assert_eq!(answer.basis(), basis);
    assert_eq!(answer.len(), 1, "1 is-a 2 is the one authored edge");
    let row = answer.row(TermId::new(2)).expect("2 is reachable");
    assert!(row.has_asserted);
    assert_eq!(row.annotation.confidence(), Some(Confidence::Confirmed));

    // Point and explain run over the same adapter.
    let present = point_fact(
        &snapshot,
        query,
        TermId::new(2),
        &sage_db_types::query::RunToCompletion,
    )
    .expect("solves");
    assert!(present.is_present());

    let explained = explain_fact(
        &snapshot,
        query,
        TermId::new(2),
        &sage_db_types::query::RunToCompletion,
    )
    .expect("explains");
    let root = explained.row().expect("present").proof.expect("root");
    assert!(
        explained
            .answer()
            .proof()
            .expect("arena")
            .get(root)
            .is_some()
    );

    // A snapshot id the view is not must be refused, even though the store is the
    // same one that produced it.
    let mut wrong = basis;
    wrong.version = KbVersion::new(basis.version.get() + 1);
    assert!(matches!(
        answer_relation(
            &snapshot,
            FactQuery {
                snapshot: wrong,
                ..query
            },
            &sage_db_types::query::RunToCompletion
        ),
        Err(FactQueryError::SnapshotMismatch {
            field: SnapshotField::Version,
            ..
        })
    ));
}

/// A control that allows exactly `budget` checkpoints, then stops.
///
/// Used to prove the wrapper's **post-solve** lookup is observable: the executor's own
/// entry/exit checks consume the first calls, and the stop lands inside the wrapper's
/// lookup rather than inside evaluation. Without that discrimination the test would
/// pass even if the lookup consulted nothing.
struct StopAfter {
    remaining: core::cell::Cell<u32>,
    calls: core::cell::Cell<u32>,
}

impl StopAfter {
    fn allowing(checkpoints: u32) -> Self {
        Self {
            remaining: core::cell::Cell::new(checkpoints),
            calls: core::cell::Cell::new(0),
        }
    }
    fn observed(&self) -> u32 {
        self.calls.get()
    }
}

impl QueryControl for StopAfter {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        self.calls.set(self.calls.get() + 1);
        let left = self.remaining.get();
        if left == 0 {
            return Err(ControlStop::Cancelled);
        }
        self.remaining.set(left - 1);
        Ok(())
    }
}

/// The wrapper's lookup is control-observable, and it cannot manufacture an answer.
///
/// The baseline is [`answer_relation`], which runs the **same** `solve` and then stops:
/// every checkpoint it makes is executor work. `point_fact` runs that same solve and
/// then does its own lookup, so with a control that permits exactly the relation's
/// allowance, the lookup's first check must be the one that trips.
///
/// That is the discriminator: if the wrapper consulted no control, the identical
/// allowance would produce a complete answer instead of `Stopped`.
#[test]
fn the_post_solve_lookup_is_control_observable_and_never_fabricates_an_answer() {
    let id = snapshot_id(41);
    let view = HandView::build(
        id,
        &[(1, L3), (2, L2), (3, L1)],
        &[
            (1, RelationId::Instantiates, 2, Confidence::Confirmed),
            (1, RelationId::IsA, 2, Confidence::Confirmed),
            (2, RelationId::IsA, 3, Confidence::Confirmed),
        ],
    );
    let query = query_for(id, 1, RelationId::IsA, false);

    // Executor-only baseline: a relation answer makes exactly this many checks.
    let baseline_control = StopAfter::allowing(u32::MAX);
    answer_relation(&view, query, &baseline_control).expect("the relation must solve");
    let executor_checks = baseline_control.observed();
    assert!(
        executor_checks >= 2,
        "the executor checks entry and exit at least: {executor_checks}"
    );

    // That exact allowance is enough for the relation, and therefore for the same
    // solve inside `point_fact` — but not for the wrapper's own lookup afterwards.
    let relation_tight = StopAfter::allowing(executor_checks);
    answer_relation(&view, query, &relation_tight)
        .expect("the executor-only allowance must suffice for the relation");

    let tight = StopAfter::allowing(executor_checks);
    assert!(
        matches!(
            point_fact(&view, query, TermId::new(2), &tight),
            Err(FactQueryError::Executor(QueryError::Stopped(
                ControlStop::Cancelled
            )))
        ),
        "the wrapper's post-solve check must be the one that trips; removing it would\
         let the same allowance return an answer"
    );

    // And the stop cannot be spelled as absence: same allowance, object that would be
    // `Absent`, still a typed stop.
    let absent_tight = StopAfter::allowing(executor_checks);
    assert!(
        matches!(
            point_fact(&view, query, TermId::new(1), &absent_tight),
            Err(FactQueryError::Executor(QueryError::Stopped(
                ControlStop::Cancelled
            )))
        ),
        "a stop must never be spellable as absence"
    );

    // A generous control still answers, so the refusals above are about the stop.
    let generous = StopAfter::allowing(u32::MAX);
    assert!(
        point_fact(&view, query, TermId::new(2), &generous)
            .expect("generous control")
            .is_present()
    );
}

/// Wrapper work is not reported as executor work.
///
/// `ExecStats` describes the **evaluation**: steps, work rows, work bytes and proof
/// nodes as the executor metered them. The wrapper's bounded binary-search lookup and
/// its two control checks run after that evaluation finished, charge no inference
/// budget, and therefore must not appear in these numbers. The assertion pins the
/// separation rather than an exact magnitude: the stats of a point query must equal
/// those of the identical relation query, because the only difference between them is
/// wrapper work.
#[test]
fn executor_stats_exclude_the_bounded_wrapper_work() {
    let id = snapshot_id(43);
    let view = HandView::build(
        id,
        &[(1, L3), (2, L2), (3, L1)],
        &[
            (1, RelationId::Instantiates, 2, Confidence::Confirmed),
            (1, RelationId::IsA, 2, Confidence::Confirmed),
            (2, RelationId::IsA, 3, Confidence::Confirmed),
        ],
    );
    let control = sage_db_types::query::RunToCompletion;
    let query = query_for(id, 1, RelationId::IsA, false);

    let relation = answer_relation(&view, query, &control).expect("relation answer");
    let point = point_fact(&view, query, TermId::new(3), &control).expect("point answer");

    assert_eq!(
        point.answer().stats(),
        relation.stats(),
        "a point lookup must not add executor work; it is wrapper work"
    );
}

#[test]
fn an_unknown_target_is_a_typed_error_for_point_and_explain() {
    let id = snapshot_id(71);
    let view = HandView::build(
        id,
        &[(1, L2), (2, L1)],
        &[(1, RelationId::IsA, 2, Confidence::Confirmed)],
    );
    let control = sage_db_types::query::RunToCompletion;
    for proof in [false, true] {
        let query = query_for(id, 1, RelationId::IsA, proof);
        let answer = if proof {
            explain_fact(&view, query, TermId::new(3000), &control)
        } else {
            point_fact(&view, query, TermId::new(3000), &control)
        };
        assert!(
            matches!(answer, Err(FactQueryError::Executor(QueryError::UnknownTerm(id))) if id == TermId::new(3000))
        );
    }
}
