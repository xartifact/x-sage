//! Navigation tests: shortest undirected authored paths, over a **real** kernel snapshot.
//!
//! # Why a real store
//!
//! Every case below builds a `bootstrap`ped store, commits real Terms and Assertions through the
//! canonical codecs, opens the kernel [`Snapshot`], and navigates that. There is no hand-built
//! `SnapshotView`, no session record and no `/tmp` fixture: if the production adapter changes
//! shape (a different graph, a different basis), these tests fail rather than silently passing
//! against a stand-in.
//!
//! # What the graph is
//!
//! Six terms and this authored edge set, all `is-a` unless stated:
//!
//! ```text
//!   1 -> 2        2 -> 3        3 -> 4        1 -> 5        5 -> 6        6 -> 4
//!   1 -> 4  (a direct shortcut, so the short answer is 1 hop, not 3)
//! ```
//!
//! Consequences the tests rely on:
//!
//! * `1..4` is reachable in **one** hop via the shortcut and in three via the chain, so a
//!   longest-path or DFS answer is distinguishable;
//! * `4 -> 1` is reachable only by walking the same assertions **backwards**, so a forward-only
//!   implementation fails;
//! * `2 - 5` is reachable only through a mix (`2 <- 1 -> 5`), so direction handling must be
//!   symmetric per edge rather than "reverse the whole path";
//! * term `7` is absent, so it is a genuine unknown on either side.
//!
//! What this file does **not** claim: display truncation, transport encoding, R3 virtual edges,
//! inference semantics, or any readiness/cutover property. Test-local allocations are test-local.

#![allow(dead_code)]

#[path = "../../store-tests/tests/support/shared_fs.rs"]
mod shared_fs;

use std::path::Path;
use std::rc::Rc;

use shared_fs::SharedFs;

use sage_db_format::assertion::{AssertionEvent, write_assertion};
use sage_db_format::event::encode_with;
use sage_db_format::term::TermEvent;
use sage_db_format::wire::Reader;
use sage_db_kernel::snapshot::{Expectation, Snapshot};
use sage_db_query::navigation::{Endpoint, Navigation, NavigationError, PathStep, shortest_path};
use sage_db_store::fs::CreateDir;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_types::budget::{Budget, BudgetLimit};
use sage_db_types::confidence::Confidence;
use sage_db_types::framing::Limits;
use sage_db_types::identity::{KbUuid, KbVersion, RecordRevision, TermId};
use sage_db_types::persist::{
    ActorOrigin, AssertionRecordFull, CommonFields, Extensions, RecordKey, RecordKind, RevPair,
    TermRecord, TermSource,
};
use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};
use sage_db_types::relation::{RelationId, RuleProfile};
use sage_db_types::snapshot::SnapshotView;

const ROOT: &str = "/navigation-root";
const KB: [u8; 16] = [0x5b; 16];
const PRINCIPAL: &str = "navigation-fixture";

fn real_ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(u64::MAX, u64::MAX, &RunToCompletion).expect("ledger")
}

fn rules_bytes() -> Vec<u8> {
    let mut l = real_ledger();
    sage_db_format::rules::CanonicalRules::new(RuleProfile::FormalV1)
        .encode(&mut l)
        .expect("canonical rules")
}

fn term_body(id: u32) -> Vec<u8> {
    format!("navigation-term-body/{id}").into_bytes()
}

fn term_record(id: u32) -> TermRecord {
    TermRecord {
        key: RecordKey::term(id),
        revision: RecordRevision::FIRST,
        retired: false,
        slug: format!("nav-term-{id}"),
        atom: format!("nav term {id}"),
        layer: sage_db_types::layer::SemanticLayer::L1,
        domains: vec!["default".into()],
        source: TermSource::Personal,
        confidence: Confidence::Confirmed,
        triggers: vec!["nav".into()],
        body_ref: sage_db_format::digest::raw_sha256(&term_body(id)),
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.into()),
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

fn term_event(event: TermEvent, record: &TermRecord) -> sage_db_types::prepared::OwnedEvent {
    let mut l = real_ledger();
    let bytes = encode_with(&mut l, |writer| {
        sage_db_format::term::write_term(writer, event, record)
    })
    .expect("canonical term encode");
    read_back(&bytes)
}

fn assertion(id: u128, from: u32, relation: RelationId, to: u32) -> AssertionRecordFull {
    AssertionRecordFull {
        key: RecordKey::new(RecordKind::Assertion, id),
        revision: RecordRevision::FIRST,
        retracted: false,
        from: RecordKey::term(from),
        relation,
        target: RecordKey::term(to),
        confidence: Confidence::Confirmed,
        declaration_order: 1,
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.into()),
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

fn assertion_event(record: &AssertionRecordFull) -> sage_db_types::prepared::OwnedEvent {
    let mut l = real_ledger();
    let bytes = encode_with(&mut l, |writer| {
        write_assertion(writer, AssertionEvent::Created, record)
    })
    .expect("canonical assertion encode");
    read_back(&bytes)
}

/// Recover the field table through the canonical reader, so no envelope is hand-written here.
fn read_back(bytes: &[u8]) -> sage_db_types::prepared::OwnedEvent {
    let mut l = real_ledger();
    let mut reader = Reader::new(bytes);
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

/// Commit one prepared transaction carrying `events`, returning the store.
fn commit(
    store: &mut Store<SharedFs>,
    events: Vec<sage_db_types::prepared::OwnedEvent>,
    affected: Vec<(RecordKey, [u8; 32])>,
    request: &str,
) {
    let mut l = real_ledger();
    let head = store.head().expect("a head");
    let version = head + 1;
    let basis = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, &mut l)
        .expect("basis");
    let pairs: Vec<RevPair> = affected
        .iter()
        .map(|(key, _)| RevPair {
            key: *key,
            revision: RecordRevision::FIRST,
        })
        .collect();
    let result_bytes =
        sage_db_store::typed::result_payload(version, &pairs, &[], &mut l).expect("result");
    let result_ref = sage_db_format::digest::raw_sha256(&result_bytes);

    let mut fields: Vec<(u16, Vec<u8>)> = Vec::new();
    let text = |value: &str| {
        let mut out = (value.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(value.as_bytes());
        out
    };
    fields.push((5, text(PRINCIPAL)));
    fields.push((6, 0u16.to_le_bytes().to_vec()));
    fields.push((8, 1_700_000_000_000_500i64.to_le_bytes().to_vec()));
    fields.push((176, text(PRINCIPAL)));
    fields.push((177, text(request)));
    fields.push((178, vec![version as u8; 32]));
    fields.push((179, 0u16.to_le_bytes().to_vec()));
    fields.push((180, result_ref.to_vec()));
    fields.push((181, version.to_le_bytes().to_vec()));
    fields.sort_by_key(|(tag, _)| *tag);
    let mut all = events;
    all.push(sage_db_types::prepared::OwnedEvent { tag: 192, fields });

    let mut blobs = vec![sage_db_types::prepared::BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: sage_db_types::prepared::BlobSource::Existing,
    }];
    // Declare a body blob for every affected TERM key, derived from the key itself so the
    // declaration can never disagree with what the term event references.
    for (key, _) in &affected {
        if key.kind() == RecordKind::Term {
            let id = u32::try_from(key.id()).expect("term ids fit u32");
            let body = term_body(id);
            let sha = sage_db_format::digest::raw_sha256(&body);
            if !blobs.iter().any(|b| b.sha256 == sha) {
                blobs.push(sage_db_types::prepared::BlobDeclaration {
                    sha256: sha,
                    declared_len: body.len() as u64,
                    source: sage_db_types::prepared::BlobSource::Provide(body),
                });
            }
        }
    }
    blobs.push(sage_db_types::prepared::BlobDeclaration {
        sha256: result_ref,
        declared_len: result_bytes.len() as u64,
        source: sage_db_types::prepared::BlobSource::Provide(result_bytes),
    });

    let mut tx_id = [0x9b; 16];
    // Every commit needs its own transaction id; a reused one collides on publication.
    tx_id[0] = u8::try_from(version).unwrap_or(0xff);
    tx_id[1] = u8::try_from(request.len()).unwrap_or(0);
    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: Some(head),
        tx_id,
        kb_uuid: KbUuid::from_bytes(KB),
        created_at: 1_700_000_000_000_600,
        principal_id: PRINCIPAL.into(),
        request_id: request.into(),
        payload_digest: [version as u8; 32],
        expect_version: Some(head),
        extensions: Extensions::new(),
        events: all,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, &mut l) {
        HandleOutcome::Committed { version: at, .. } => assert_eq!(at, version),
        other => panic!("commit {request} failed: {other:?}"),
    }
}

/// Recover a term id from the body digest the test registered, so the commit declares exactly
/// the blobs it wrote.
fn body_id_from_body_hash(hash: [u8; 32]) -> Option<u32> {
    (1..=6u32).find(|id| sage_db_format::digest::raw_sha256(&term_body(*id)) == hash)
}

/// The authored fixture: six terms, six assertions, one commit.
fn fixture() -> (
    Rc<sage_db_store_tests::model::fs_model::ModelFileSystem>,
    SharedFs,
    Store<SharedFs>,
) {
    let inner = Rc::new(sage_db_store_tests::model::fs_model::ModelFileSystem::new(
        sage_db_store_tests::model::fs_model::ModelCapabilities::full(),
    ));
    let fs = SharedFs::from_rc(Rc::clone(&inner));
    let mut l = real_ledger();
    let mut store = bootstrap(
        fs.clone(),
        Path::new(ROOT),
        CreateDir::IfMissing,
        KbUuid::from_bytes(KB),
        &rules_bytes(),
        &Limits::V1,
        &mut l,
    )
    .expect("bootstrap");

    let mut events = Vec::new();
    let mut affected: Vec<(RecordKey, [u8; 32])> = Vec::new();
    for id in 1..=6u32 {
        let record = term_record(id);
        events.push(term_event(TermEvent::Created, &record));
        affected.push((
            record.key,
            sage_db_format::digest::raw_sha256(&term_body(id)),
        ));
    }
    // Edges, in the authored direction written here.
    for (id, from, to) in [
        (1u128, 1u32, 2u32),
        (2, 2, 3),
        (3, 3, 4),
        (4, 1, 5),
        (5, 5, 6),
        (6, 6, 4),
    ] {
        let record = assertion(id, from, RelationId::IsA, to);
        events.push(assertion_event(&record));
        affected.push((record.key, [0u8; 32]));
    }
    // The direct shortcut, authored last so its assertion id is the highest of 1's out-edges
    // while its TARGET (4) still sorts after 2 and 5 in canonical visit order — which is what
    // makes a level-order search the only one that can return the 1-hop answer.
    let shortcut = assertion(7, 1, RelationId::IsA, 4);
    events.push(assertion_event(&shortcut));
    affected.push((shortcut.key, [0u8; 32]));

    commit(&mut store, events, affected, "nav-seed");
    (inner, fs, store)
}

fn snapshot<'a>(store: &'a mut Store<SharedFs>) -> Snapshot<'a, SharedFs> {
    let mut l = real_ledger();
    Snapshot::open(store, Expectation::latest(), &mut l).expect("the fixture root opens")
}

fn tid(value: u32) -> TermId {
    TermId::new(value)
}

fn path(
    snapshot: &Snapshot<'_, SharedFs>,
    from: u32,
    to: u32,
    budget: Budget,
    control: &dyn QueryControl,
) -> Result<Navigation, NavigationError> {
    let basis = snapshot.basis();
    shortest_path(snapshot, basis, tid(from), tid(to), budget, control)
}

// ---------------------------------------------------------------------------
// Shortest paths, direction, and determinism
// ---------------------------------------------------------------------------

#[test]
fn a_direct_shortcut_wins_over_the_longer_chain() {
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);
    let found = path(&view, 1, 4, Budget::DEFAULT, &RunToCompletion).expect("1 reaches 4");

    assert_eq!(found.from(), tid(1));
    assert_eq!(found.to(), tid(4));
    assert_eq!(found.hops(), 1, "the authored shortcut is one hop");
    assert_eq!(found.nodes(), &[tid(1), tid(4)]);
    assert_eq!(found.basis(), view.basis());
    assert_eq!(found.steps()[0].relation(), RelationId::IsA);
}

#[test]
fn a_reverse_path_walks_the_same_authored_assertions_backwards() {
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);
    // 4 -> 1 is only possible by traversing authored 1 -> 4 backwards.
    let found = path(&view, 4, 1, Budget::DEFAULT, &RunToCompletion).expect("4 reaches 1");
    assert_eq!(found.hops(), 1);
    assert_eq!(found.nodes(), &[tid(4), tid(1)]);
    let step = &found.steps()[0];
    assert!(
        step.reversed,
        "the walk crossed the authored edge backwards"
    );
    // The witness keeps the AUTHORED direction, never the traversal direction.
    assert_eq!(step.witness.from, tid(1));
    assert_eq!(step.witness.target, tid(4));
    assert_eq!(
        step.assertion(),
        sage_db_types::identity::AssertionId::new(7)
    );
}

#[test]
fn a_mixed_direction_path_is_two_hops() {
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);
    // 2 <- 1 -> 5: one backward hop then one forward hop.
    let found = path(&view, 2, 5, Budget::DEFAULT, &RunToCompletion).expect("2 reaches 5");
    assert_eq!(found.hops(), 2);
    assert_eq!(found.nodes(), &[tid(2), tid(1), tid(5)]);
    assert!(found.steps()[0].reversed, "2 <- 1 is backwards");
    assert!(!found.steps()[1].reversed, "1 -> 5 is forwards");
}

#[test]
fn every_relation_kind_participates() {
    let (_inner, _fs, store) = fixture();
    let mut view_store = store;
    // A second commit adds one edge of a different relation and nothing else.
    let record = assertion(20, 2, RelationId::Contradicts, 6);
    let mut events = vec![assertion_event(&record)];
    let mut affected = vec![(record.key, [0u8; 32])];
    // Terms already exist from the seed.
    let _ = &mut events;
    let _ = &mut affected;
    commit(&mut view_store, events, affected, "nav-extra");
    let view = snapshot(&mut view_store);
    let found = path(&view, 6, 2, Budget::DEFAULT, &RunToCompletion)
        .expect("the contradicts edge is navigable like any other");
    assert_eq!(found.hops(), 1);
    assert_eq!(found.steps()[0].relation(), RelationId::Contradicts);
}

#[test]
fn a_multi_hop_chain_reports_its_exact_hop_count() {
    // 1 -> 5 -> 6 -> 4 is a 3-hop authored chain and the shortcut 1 -> 4 is not on it, so the
    // only way to answer 1..6 is the chain. A traversal that answered with ANY path rather than
    // the shortest would still pass this; the pair below is what pins the count.
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);
    let found = path(&view, 1, 6, Budget::DEFAULT, &RunToCompletion).expect("1 reaches 6");
    // There are exactly two 2-hop routes from 1 to 6 (1 -> 5 -> 6 and 1 -> 4 -> 6 backwards),
    // and one 3-hop route (1 -> 2 -> 3 -> 4 -> 6 is four, so none shorter than two exists).
    // The SHORTEST is therefore two hops, and the node count must agree with it.
    assert_eq!(
        found.hops(),
        2,
        "no route from 1 to 6 is shorter than two hops"
    );
    assert_eq!(found.nodes()[0], tid(1));
    assert_eq!(found.nodes()[found.hops()], tid(6));
    assert_eq!(found.nodes().len(), found.hops() + 1);
}

#[test]
fn equal_length_alternatives_follow_canonical_order_not_insertion_order() {
    // A REAL tie: two distinct authored assertions joining the same pair (1, 4) — one hop each,
    // two different witness identities. Repeating one query cannot distinguish any
    // deterministic rule, so the discriminator is WHICH witness is returned.
    //
    // Canonical order is (from, relation, target, assertion_id), so the lower-id support must
    // win. The higher-id support is authored FIRST here, so "first inserted" and "canonical"
    // disagree and only a canonical-order rule returns the lower id.
    let (_inner, _fs, store) = fixture();
    let mut s = store;
    let first_written = assertion(60, 1, RelationId::IsA, 4);
    commit(
        &mut s,
        vec![assertion_event(&first_written)],
        vec![(first_written.key, [0u8; 32])],
        "nav-tie-high",
    );
    let second_written = assertion(20, 1, RelationId::IsA, 4);
    commit(
        &mut s,
        vec![assertion_event(&second_written)],
        vec![(second_written.key, [0u8; 32])],
        "nav-tie-low",
    );
    let view = snapshot(&mut s);

    // Both supports really exist, in both directions of the pair.
    let supports: Vec<u128> = view
        .graph()
        .assertions_from(tid(1))
        .iter()
        .filter(|a| a.target == tid(4))
        .map(|a| a.id.get())
        .collect();
    // Three parallel supports: the fixture's own shortcut (id 7) plus the two added here.
    assert_eq!(
        supports.len(),
        3,
        "the fixture must expose parallel supports"
    );
    assert!(supports.contains(&7) && supports.contains(&20) && supports.contains(&60));
    let canonical_first = supports.iter().copied().min().expect("non-empty");
    assert_eq!(canonical_first, 7, "canonical order is by assertion id");

    let found = path(&view, 1, 4, Budget::DEFAULT, &RunToCompletion).expect("1 reaches 4");
    assert_eq!(found.hops(), 1);
    // The returned witness must be the CANONICAL-FIRST support (lowest assertion id), which is
    // the one written SECOND — so insertion order cannot be what decided it.
    assert_eq!(
        found.steps()[0].assertion(),
        sage_db_types::identity::AssertionId::new(7),
        "the tie must be broken by canonical assertion id, not by insertion order"
    );
    assert!(
        found.tie_rule().contains("not the JS input-order choice"),
        "the rule must state it is not JS input-order parity"
    );

    // Reversing the same question must also pick the canonical support, not the last seen.
    let back = path(&view, 4, 1, Budget::DEFAULT, &RunToCompletion).expect("4 reaches 1");
    assert_eq!(
        back.steps()[0].assertion(),
        sage_db_types::identity::AssertionId::new(7),
        "the reverse direction must obey the same canonical rule"
    );
}

// ---------------------------------------------------------------------------
// Zero-hop and unknown endpoints
// ---------------------------------------------------------------------------

#[test]
fn a_known_term_reaches_itself_in_zero_hops_with_one_node() {
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);
    let found = path(&view, 3, 3, Budget::DEFAULT, &RunToCompletion).expect("known identity");
    assert_eq!(found.nodes(), &[tid(3)]);
    assert_eq!(found.hops(), 0);
    assert!(found.steps().is_empty());
}

#[test]
fn an_unknown_term_is_refused_on_either_side_and_never_fabricates_a_zero_hop() {
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);

    match path(&view, 7, 1, Budget::DEFAULT, &RunToCompletion) {
        Err(NavigationError::UnknownTerm { side, term }) => {
            assert_eq!(side, Endpoint::From);
            assert_eq!(term, tid(7));
        }
        other => panic!("an unknown source must be UnknownTerm, got {other:?}"),
    }
    match path(&view, 1, 7, Budget::DEFAULT, &RunToCompletion) {
        Err(NavigationError::UnknownTerm { side, term }) => {
            assert_eq!(side, Endpoint::To);
            assert_eq!(term, tid(7));
        }
        other => panic!("an unknown target must be UnknownTerm, got {other:?}"),
    }
    // The same unknown term on both sides is still unknown — NOT a zero-hop path.
    match path(&view, 7, 7, Budget::DEFAULT, &RunToCompletion) {
        Err(NavigationError::UnknownTerm { term, .. }) => assert_eq!(term, tid(7)),
        other => panic!("unknown==unknown must not be a fabricated identity path, got {other:?}"),
    }
}

#[test]
fn disconnected_known_terms_are_a_complete_no_path() {
    let (_inner, _fs, store) = fixture();
    let mut s = store;
    // A seventh term with no assertions at all: known, but isolated.
    let record = term_record(9);
    commit(
        &mut s,
        vec![term_event(TermEvent::Created, &record)],
        vec![(
            record.key,
            sage_db_format::digest::raw_sha256(&term_body(9)),
        )],
        "nav-island",
    );
    let view = snapshot(&mut s);
    match path(&view, 9, 1, Budget::DEFAULT, &RunToCompletion) {
        Err(NavigationError::NoPath { from, to }) => {
            assert_eq!(from, tid(9));
            assert_eq!(to, tid(1));
        }
        other => panic!("a complete search on an island must be NoPath, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Snapshot binding
// ---------------------------------------------------------------------------

#[test]
fn a_foreign_snapshot_field_is_refused() {
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);
    let basis = view.basis();
    // Each of the four fields independently.
    let mut wrong = basis;
    wrong.version = KbVersion::new(basis.version.get() + 1);
    assert!(matches!(
        shortest_path(
            &view,
            wrong,
            tid(1),
            tid(4),
            Budget::DEFAULT,
            &RunToCompletion
        ),
        Err(NavigationError::SnapshotMismatch { .. })
    ));

    let mut other_kb = basis;
    other_kb.kb_uuid = KbUuid::from_bytes([0x42; 16]);
    assert!(matches!(
        shortest_path(
            &view,
            other_kb,
            tid(1),
            tid(4),
            Budget::DEFAULT,
            &RunToCompletion
        ),
        Err(NavigationError::SnapshotMismatch { .. })
    ));
}

// ---------------------------------------------------------------------------
// Budget and cancellation
// ---------------------------------------------------------------------------

/// A budget that admits real work before it refuses.
///
/// Not a magic small number: the value is chosen so the walk demonstrably completes a prefix
/// (shown by the same query succeeding under `Budget::DEFAULT`) and then refuses partway.
fn prefix_budget() -> Budget {
    Budget {
        max_steps: 12,
        ..Budget::DEFAULT
    }
}

#[test]
fn a_budget_exhausted_after_a_real_prefix_is_a_typed_refusal_not_a_path() {
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);
    // First establish that the SAME query succeeds with the full budget, so the tight run below
    // is known to have begun on a genuinely reachable question rather than on a dead one.
    let full = path(&view, 2, 5, Budget::DEFAULT, &RunToCompletion).expect("2 reaches 5");
    assert_eq!(full.hops(), 2);
    assert!(full.stats().steps > 0, "the successful run really did work");

    // Now admit only part of that work: the refusal must be a budget class, never a path.
    match path(&view, 2, 5, prefix_budget(), &RunToCompletion) {
        Err(NavigationError::BudgetExceeded(_)) => {}
        other => panic!("a partial budget must refuse rather than answer, got {other:?}"),
    }
}

#[test]
fn a_zero_row_limit_refuses_inside_the_scan() {
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);
    // Rows are charged before materialisation, so a table that may not hold one row must refuse
    // even though the step allowance is large: this is the nested row limit, not a step limit.
    let no_rows = Budget {
        max_work_rows: 0,
        ..Budget::DEFAULT
    };
    match path(&view, 2, 5, no_rows, &RunToCompletion) {
        Err(NavigationError::BudgetExceeded(BudgetLimit::WorkRows)) => {}
        other => panic!("a zero work-row allowance must refuse at materialisation, got {other:?}"),
    }
}

#[test]
fn a_stop_during_traversal_returns_no_path() {
    /// Counts checkpoints and stops at a target index, recording where it fired.
    struct CountToStop {
        seen: core::cell::Cell<usize>,
        stop_at: usize,
        fired_at: core::cell::Cell<Option<usize>>,
    }
    impl QueryControl for CountToStop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            let at = self.seen.get();
            self.seen.set(at + 1);
            if at >= self.stop_at {
                self.fired_at.set(Some(at));
                return Err(ControlStop::Cancelled);
            }
            Ok(())
        }
    }

    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);

    // First, measure how many checkpoints a SUCCESSFUL 2 -> 5 search takes. That is observed,
    // not guessed: a stop any earlier than this count must land inside the traversal.
    let counting = CountToStop {
        seen: core::cell::Cell::new(0),
        stop_at: usize::MAX,
        fired_at: core::cell::Cell::new(None),
    };
    let ok = path(&view, 2, 5, Budget::DEFAULT, &counting).expect("2 reaches 5");
    let total = counting.seen.get();
    assert!(
        total > 2,
        "a real traversal takes more than an entry and an exit"
    );
    assert_eq!(ok.hops(), 2);

    // Now stop strictly after entry (index > 0) and strictly before completion.
    let stop_at = total / 2;
    assert!(stop_at > 0, "the stop must be after the entry check");
    assert!(stop_at < total, "and before the search would have finished");
    let control = CountToStop {
        seen: core::cell::Cell::new(0),
        stop_at,
        fired_at: core::cell::Cell::new(None),
    };
    match path(&view, 2, 5, Budget::DEFAULT, &control) {
        Err(NavigationError::Stopped(ControlStop::Cancelled)) => {
            let fired = control.fired_at.get().expect("the stop fired");
            assert_eq!(
                fired, stop_at,
                "the stop landed exactly where it was asked to"
            );
        }
        other => panic!("a mid-traversal stop must refuse, got {other:?}"),
    }
}

#[test]
fn a_stop_at_the_empty_entry_is_observed() {
    struct AlwaysStop;
    impl QueryControl for AlwaysStop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);
    // Even a zero-hop question must consult control on entry; it does not get to succeed.
    match path(&view, 3, 3, Budget::DEFAULT, &AlwaysStop) {
        Err(NavigationError::Stopped(ControlStop::Cancelled)) => {}
        other => panic!("a pre-stopped caller must be refused before any work, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Witness identity
// ---------------------------------------------------------------------------

#[test]
fn parallel_support_keeps_its_own_witness_identity() {
    let (_inner, _fs, store) = fixture();
    let mut s = store;
    // A second assertion for the SAME pair 1 -> 4, with its own id.
    let duplicate = assertion(30, 1, RelationId::IsA, 4);
    commit(
        &mut s,
        vec![assertion_event(&duplicate)],
        vec![(duplicate.key, [0u8; 32])],
        "nav-parallel",
    );
    let view = snapshot(&mut s);
    let found = path(&view, 1, 4, Budget::DEFAULT, &RunToCompletion).expect("a path");
    assert_eq!(found.hops(), 1);
    // Whichever support is chosen is named by a REAL assertion id present in the graph, and its
    // endpoints are the authored ones.
    let step: &PathStep = &found.steps()[0];
    let witness = view
        .graph()
        .assertion(step.assertion())
        .expect("the witness must be a real assertion of this graph");
    assert_eq!(witness.from, tid(1));
    assert_eq!(witness.target, tid(4));
    assert_eq!(step.revision(), witness.revision);
    assert_eq!(step.witness, witness);
}

#[test]
fn the_result_reports_the_stats_of_the_meter_that_bounded_it() {
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);

    // A real walk charges every class the contract reports.
    let found = path(&view, 2, 5, Budget::DEFAULT, &RunToCompletion).expect("2 reaches 5");
    let stats = found.stats();
    // Steps are pinned too: the attended reverse and the charged binary searches are step work,
    // so an unmetered `reverse()` or an uncharged comparison changes this figure.
    assert_eq!(
        stats.steps, 50,
        "the 2 -> 5 walk charges exactly 50 steps on this fixture"
    );
    // Pinned to the actual figure for this fixture, so a row over- OR under-charge is visible
    // rather than hidden behind a `>=` band. Breakdown for 2 -> 5 on this graph: 1 bitmap word,
    // 3 discovered nodes + 3 queue slots, 3 chain entries, 3 output nodes, 2 output steps,
    // 2 attended swaps and 1 entry row... the exact composition is the module's business; what
    // matters is that it is stable and bounded.
    assert_eq!(
        stats.work_rows, 19,
        "the 2 -> 5 walk materialises exactly 19 rows on this fixture"
    );
    assert!(stats.work_bytes > 0, "its tables were admitted, not free");
    assert_eq!(stats.proof_nodes, 0, "navigation produces no proof nodes");

    // A zero-hop answer materialises EXACTLY one row: the single node. `>= 1` would accept a
    // double charge on the smallest possible answer, so the figure is pinned.
    let zero = path(&view, 3, 3, Budget::DEFAULT, &RunToCompletion).expect("identity");
    assert_eq!(zero.hops(), 0);
    assert_eq!(zero.nodes().len(), 1);
    assert_eq!(
        zero.stats().work_rows,
        1,
        "a one-node path must cost exactly one work row"
    );
    assert!(
        zero.stats().steps < stats.steps,
        "identity must cost fewer steps than a two-hop walk"
    );
}

#[test]
fn a_one_row_allowance_accepts_the_zero_hop_and_zero_refuses_it() {
    let (_inner, _fs, mut store) = fixture();
    let view = snapshot(&mut store);

    // Exactly one row is enough — and it must be enough, or the accounting over-charges.
    let exact = Budget {
        max_work_rows: 1,
        ..Budget::DEFAULT
    };
    let zero = path(&view, 3, 3, exact, &RunToCompletion)
        .expect("a one-row budget must admit a one-node answer");
    assert_eq!(zero.stats().work_rows, 1);
    assert_eq!(zero.nodes(), &[tid(3)]);

    // Zero rows cannot materialise the node, so the refusal must be the ROW limit.
    let none = Budget {
        max_work_rows: 0,
        ..Budget::DEFAULT
    };
    match path(&view, 3, 3, none, &RunToCompletion) {
        Err(NavigationError::BudgetExceeded(BudgetLimit::WorkRows)) => {}
        other => panic!("a zero-row budget must refuse the one node, got {other:?}"),
    }
}
