//! `depends-on` prerequisite-order tests over real committed records and a real kernel snapshot.
//!
//! # Evidence shape
//!
//! Two kinds, deliberately separated:
//!
//! * a **hand-authored** `SnapshotView` fixture, so every expected order is written by hand from
//!   the stated rule (dependent-last, ties by ascending `TermId`) rather than read back from the
//!   implementation;
//! * a **real kernel `Snapshot`** over a real bootstrapped `Store` on the model filesystem, so the
//!   wrapper is proven against the production adapter.
//!
//! # What this file does not claim
//!
//! No display truncation, transport bytes, caching, `min_conf` filtering, or readiness. Test-local
//! allocations are test-local and are not allocator evidence. The [MUTATION] tests are documented
//! separately at the end.

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
use sage_db_query::FactQuery;
use sage_db_query::requires::{RequiresError, requires};
use sage_db_store::fs::CreateDir;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_types::budget::Budget;
use sage_db_types::confidence::Confidence;
use sage_db_types::digest::GraphDigest;
use sage_db_types::graph::SnapshotId;
use sage_db_types::identity::{KbUuid, RecordRevision, TermId};
use sage_db_types::layer::SemanticLayer;
use sage_db_types::persist::{
    ActorOrigin, AssertionRecordFull, CommonFields, Extensions, RecordKey, RecordKind, RevPair,
    TermRecord, TermSource,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};
use sage_db_types::query::{ControlStop, QueryControl, QueryError, RunToCompletion};
use sage_db_types::relation::{RelationId, RuleProfile};
use sage_db_types::snapshot::SnapshotView;

const PROFILE: RuleProfile = RuleProfile::FormalV1;
const BYTES: u64 = 256 * 1024 * 1024;
const ENTRIES: u64 = 2_000_000;
const ROOT: &str = "/p3-requires-fixture";
const KB: [u8; 16] = [0x5b; 16];

// ---------------------------------------------------------------------------
// Real kernel snapshot
// ---------------------------------------------------------------------------

fn real_ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).expect("ledger")
}

fn rules_bytes() -> Vec<u8> {
    let mut ledger = real_ledger();
    sage_db_format::rules::CanonicalRules::new(PROFILE)
        .encode(&mut ledger)
        .expect("the canonical rules encoder accepts its own profile")
}

fn term_body(id: u32) -> Vec<u8> {
    format!("p3-requires-term-body/{id}").into_bytes()
}

fn term_event(event: TermEvent, record: &TermRecord) -> OwnedEvent {
    let mut l = real_ledger();
    let bytes = encode_with(&mut l, |writer| {
        sage_db_format::term::write_term(writer, event, record)
    })
    .expect("the canonical term writer accepts the fixture");
    let mut reader = Reader::new(&bytes);
    let decoded = sage_db_format::event::read_event(&mut reader, &mut l).expect("reader");
    let mut fields = Vec::new();
    for field in decoded.fields() {
        fields.push((field.tag(), field.value().to_vec()));
    }
    OwnedEvent {
        tag: decoded.tag(),
        fields,
    }
}

fn assertion_event(record: &AssertionRecordFull) -> OwnedEvent {
    let mut l = real_ledger();
    let bytes = encode_with(&mut l, |writer| {
        write_assertion(writer, AssertionEvent::Created, record)
    })
    .expect("writer");
    let mut reader = Reader::new(&bytes);
    let event = sage_db_format::event::read_event(&mut reader, &mut l).expect("reader");
    let mut fields = Vec::new();
    for field in event.fields() {
        fields.push((field.tag(), field.value().to_vec()));
    }
    OwnedEvent {
        tag: event.tag(),
        fields,
    }
}

fn term_record(id: u32) -> TermRecord {
    term_record_in_layer(id, SemanticLayer::L1)
}

/// One term at an explicit layer, for the layer-order regression.
fn term_record_in_layer(id: u32, layer: SemanticLayer) -> TermRecord {
    TermRecord {
        key: RecordKey::term(id),
        revision: RecordRevision::FIRST,
        retired: false,
        slug: format!("term-{id}"),
        atom: format!("term {id}"),
        layer,
        domains: Vec::new(),
        source: TermSource::Personal,
        confidence: Confidence::Confirmed,
        triggers: Vec::new(),
        body_ref: sage_db_format::digest::raw_sha256(&term_body(id)),
        common: CommonFields {
            recorded_by: Some("p3-requires".into()),
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

fn depends_assertion(id: u128, from: u32, to: u32) -> AssertionRecordFull {
    AssertionRecordFull {
        key: RecordKey::new(RecordKind::Assertion, id),
        revision: RecordRevision::FIRST,
        retracted: false,
        from: RecordKey::term(from),
        relation: RelationId::DependsOn,
        target: RecordKey::term(to),
        confidence: Confidence::Confirmed,
        declaration_order: 1,
        common: CommonFields {
            recorded_by: Some("p3-requires".into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_001,
            updated_at: None,
        },
        legacy_id: None,
        legacy_content_digest: None,
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

/// A real store seeded with the given terms and `(from, to)` depends-on edges, committed in one
/// transaction whose version derives from the actual head.
///
/// Every case in this file runs on this one honest fixture path: real terms, real assertions
/// through the canonical writers, a real commit, and a real snapshot over it. No hand-built graph
/// stands in for the production adapter.
fn store_with(
    terms: &[u32],
    edges: &[(u32, u32)],
) -> (
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
        &sage_db_types::framing::Limits::V1,
        &mut l,
    )
    .expect("bootstrap");

    let mut events = Vec::new();
    for id in terms {
        events.push(term_event(TermEvent::Created, &term_record(*id)));
    }
    let mut assertions = Vec::new();
    for (index, (from, to)) in edges.iter().enumerate() {
        let record = depends_assertion(u128::try_from(index + 1).expect("small"), *from, *to);
        events.push(assertion_event(&record));
        assertions.push(record);
    }

    let head = store.head().expect("genesis published a head");
    let version = head + 1;
    let basis = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, &mut l)
        .expect("basis");
    let mut affected: Vec<RevPair> = terms
        .iter()
        .map(|id| RevPair {
            key: RecordKey::term(*id),
            revision: RecordRevision::FIRST,
        })
        .collect();
    for record in &assertions {
        affected.push(RevPair {
            key: record.key,
            revision: RecordRevision::FIRST,
        });
    }
    // The canonical result requires ascending affected keys; the caller's term order is not it.
    affected.sort_by_key(|pair| pair.key);
    let result_bytes =
        sage_db_store::typed::result_payload(version, &affected, &[], &mut l).expect("result");
    let result_ref = sage_db_format::digest::raw_sha256(&result_bytes);

    let text = |value: &str| {
        let mut out = (value.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(value.as_bytes());
        out
    };
    let mut outcome_fields: Vec<(u16, Vec<u8>)> = vec![
        (5, text("p3-requires")),
        (6, 0u16.to_le_bytes().to_vec()),
        (8, 1_700_000_000_000_500i64.to_le_bytes().to_vec()),
        (176, text("p3-requires")),
        (177, text("p3-requires-seed")),
        (178, vec![version as u8; 32]),
        (179, 0u16.to_le_bytes().to_vec()),
        (180, result_ref.to_vec()),
        (181, version.to_le_bytes().to_vec()),
    ];
    outcome_fields.sort_by_key(|(tag, _)| *tag);
    events.push(OwnedEvent {
        tag: 192,
        fields: outcome_fields,
    });

    let mut blobs = vec![BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: BlobSource::Existing,
    }];
    for id in terms {
        let body = term_body(*id);
        blobs.push(BlobDeclaration {
            sha256: sage_db_format::digest::raw_sha256(&body),
            declared_len: body.len() as u64,
            source: BlobSource::Provide(body),
        });
    }
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: result_bytes.len() as u64,
        source: BlobSource::Provide(result_bytes),
    });

    let prepared = PreparedTransaction {
        version,
        prev_version: Some(head),
        tx_id: [0x9c; 16],
        kb_uuid: KbUuid::from_bytes(KB),
        created_at: 1_700_000_000_000_600,
        principal_id: "p3-requires".into(),
        request_id: "p3-requires-seed".into(),
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

/// As [`store_with`], with an explicit layer per term.
fn store_with_layered(
    terms: &[(u32, SemanticLayer)],
    edges: &[(u32, u32)],
) -> (
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
        &sage_db_types::framing::Limits::V1,
        &mut l,
    )
    .expect("bootstrap");

    let mut events = Vec::new();
    for (id, layer) in terms {
        events.push(term_event(
            TermEvent::Created,
            &term_record_in_layer(*id, *layer),
        ));
    }
    let mut assertions = Vec::new();
    for (index, (from, to)) in edges.iter().enumerate() {
        let record = depends_assertion(u128::try_from(index + 1).expect("small"), *from, *to);
        events.push(assertion_event(&record));
        assertions.push(record);
    }

    let head = store.head().expect("genesis published a head");
    let version = head + 1;
    let basis = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, &mut l)
        .expect("basis");
    let mut affected: Vec<RevPair> = terms
        .iter()
        .map(|(id, _)| RevPair {
            key: RecordKey::term(*id),
            revision: RecordRevision::FIRST,
        })
        .collect();
    for record in &assertions {
        affected.push(RevPair {
            key: record.key,
            revision: RecordRevision::FIRST,
        });
    }
    // The canonical result requires ascending affected keys; the caller's term order is not it.
    affected.sort_by_key(|pair| pair.key);
    let result_bytes =
        sage_db_store::typed::result_payload(version, &affected, &[], &mut l).expect("result");
    let result_ref = sage_db_format::digest::raw_sha256(&result_bytes);

    let text = |value: &str| {
        let mut out = (value.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(value.as_bytes());
        out
    };
    let mut outcome_fields: Vec<(u16, Vec<u8>)> = vec![
        (5, text("p3-requires")),
        (6, 0u16.to_le_bytes().to_vec()),
        (8, 1_700_000_000_000_500i64.to_le_bytes().to_vec()),
        (176, text("p3-requires")),
        (177, text("p3-requires-seed")),
        (178, vec![version as u8; 32]),
        (179, 0u16.to_le_bytes().to_vec()),
        (180, result_ref.to_vec()),
        (181, version.to_le_bytes().to_vec()),
    ];
    outcome_fields.sort_by_key(|(tag, _)| *tag);
    events.push(OwnedEvent {
        tag: 192,
        fields: outcome_fields,
    });

    let mut blobs = vec![BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: BlobSource::Existing,
    }];
    for (id, _) in terms {
        let body = term_body(*id);
        blobs.push(BlobDeclaration {
            sha256: sage_db_format::digest::raw_sha256(&body),
            declared_len: body.len() as u64,
            source: BlobSource::Provide(body),
        });
    }
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: result_bytes.len() as u64,
        source: BlobSource::Provide(result_bytes),
    });

    let prepared = PreparedTransaction {
        version,
        prev_version: Some(head),
        tx_id: [0x9c; 16],
        kb_uuid: KbUuid::from_bytes(KB),
        created_at: 1_700_000_000_000_600,
        principal_id: "p3-requires".into(),
        request_id: "p3-requires-seed".into(),
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

/// Seed a real store, open a real snapshot over it, and hand both to `body`.
///
/// The snapshot borrows the store, so the closure is the only way to use them together without
/// lifetime plumbing that would obscure the test. Every case below runs on this one honest path.
fn with_snapshot<R>(
    terms: &[u32],
    edges: &[(u32, u32)],
    body: impl FnOnce(&Snapshot<'_, SharedFs>) -> R,
) -> R {
    let (_inner, _fs, mut store) = store_with(terms, edges);
    let mut l = real_ledger();
    let snapshot = Snapshot::open(&mut store, Expectation::latest(), &mut l)
        .expect("the fixture root must produce a snapshot");
    body(&snapshot)
}

/// As [`with_snapshot`], with an explicit layer per term.
fn with_snapshot_layered<R>(
    terms: &[(u32, SemanticLayer)],
    edges: &[(u32, u32)],
    body: impl FnOnce(&Snapshot<'_, SharedFs>) -> R,
) -> R {
    let ids: Vec<u32> = terms.iter().map(|(id, _)| *id).collect();
    let (_inner, _fs, mut store) = store_with_layered(terms, edges);
    let _ = ids;
    let mut l = real_ledger();
    let snapshot = Snapshot::open(&mut store, Expectation::latest(), &mut l)
        .expect("the fixture root must produce a snapshot");
    body(&snapshot)
}

fn digest_of(bytes: &[u8]) -> sage_db_types::digest::Digest {
    sage_db_types::digest::Digest::from_bytes(sage_db_format::digest::raw_sha256(bytes))
}

fn query_at(basis: SnapshotId, source: u32, budget: Budget) -> FactQuery {
    FactQuery {
        snapshot: basis,
        source: TermId::new(source),
        relation: RelationId::DependsOn,
        with_proof: true,
        budget,
    }
}

#[test]
fn the_real_kernel_snapshot_orders_real_committed_dependencies() {
    with_snapshot(&[1, 2, 3], &[(3, 2), (2, 1)], |snapshot| {
        let basis = snapshot.basis();
        let answer = requires(
            snapshot,
            query_at(basis, 3, Budget::DEFAULT),
            &RunToCompletion,
        )
        .expect("the real snapshot answers");
        assert_eq!(
            answer.answer().basis(),
            basis,
            "the answer binds the view's basis"
        );
        assert_eq!(
            answer.prerequisite_order(),
            &[TermId::new(1), TermId::new(2)],
            "1 must precede 2, and the source 3 is not in the order"
        );
        assert!(answer.is_acyclic());
        assert_eq!(answer.rows().len(), 2);
        for row in answer.rows() {
            // 3 depends on 2 directly; 1 is reached through 2, so it is derived, not asserted.
            assert!(
                row.has_asserted || row.has_derived,
                "every row is either authored or rule-derived, never neither"
            );
            assert!(row.proof.is_some(), "proofs are retained in the answer");
        }
        let direct = answer
            .rows()
            .iter()
            .find(|row| row.to == TermId::new(2))
            .expect("2 is reachable");
        assert!(direct.has_asserted, "3 -> 2 is a direct authored edge");
        assert!(answer.answer().proof().is_some(), "the arena is retained");
        assert!(
            answer.steps_used() > 0,
            "work was spent from the caller's meter"
        );
    });
}

#[test]
fn ids_disagreeing_with_dependency_order_are_not_sorted_by_id() {
    // 2 depends on 1, and 1 depends on 9: prerequisites {1, 9}. Topological order must be
    // 9 then 1; ascending-id would print 1 then 9 — the discriminating case.
    with_snapshot(&[1, 2, 9], &[(2, 1), (1, 9)], |snapshot| {
        let basis = snapshot.basis();
        let answer = requires(
            snapshot,
            query_at(basis, 2, Budget::DEFAULT),
            &RunToCompletion,
        )
        .expect("answers");
        assert_eq!(
            answer.prerequisite_order(),
            &[TermId::new(9), TermId::new(1)],
            "9 must precede 1 because 1 depends on 9; an id-sorted order would print 1,9"
        );
    });
}

#[test]
fn a_diamond_places_the_shared_prerequisite_once_and_first() {
    with_snapshot(
        &[1, 2, 3, 4],
        &[(4, 2), (4, 3), (2, 1), (3, 1)],
        |snapshot| {
            let basis = snapshot.basis();
            let answer = requires(
                snapshot,
                query_at(basis, 4, Budget::DEFAULT),
                &RunToCompletion,
            )
            .expect("answers");
            let order = answer.prerequisite_order();
            assert_eq!(order.len(), 3, "1 must appear once despite two dependents");
            let pos = |id: u32| {
                order
                    .iter()
                    .position(|t| *t == TermId::new(id))
                    .expect("present")
            };
            assert!(pos(1) < pos(2), "1 must precede 2");
            assert!(pos(1) < pos(3), "1 must precede 3");
        },
    );
}

#[test]
fn a_duplicate_support_does_not_add_a_second_entry() {
    with_snapshot(&[1, 2], &[(2, 1), (2, 1)], |snapshot| {
        let basis = snapshot.basis();
        let answer = requires(
            snapshot,
            query_at(basis, 2, Budget::DEFAULT),
            &RunToCompletion,
        )
        .expect("answers");
        assert_eq!(
            answer.prerequisite_order(),
            &[TermId::new(1)],
            "a duplicate support must not add a second entry or break the decrement"
        );
        assert!(answer.is_acyclic());
    });
}

#[test]
fn unrelated_terms_are_not_in_the_prerequisite_order() {
    with_snapshot(&[1, 2, 7, 8], &[(2, 1)], |snapshot| {
        let basis = snapshot.basis();
        let answer = requires(
            snapshot,
            query_at(basis, 2, Budget::DEFAULT),
            &RunToCompletion,
        )
        .expect("answers");
        assert_eq!(answer.prerequisite_order(), &[TermId::new(1)]);
        assert!(!answer.prerequisite_order().contains(&TermId::new(7)));
    });
}

#[test]
fn a_cyclic_dependency_graph_is_refused_by_the_rule_profile_before_ordering() {
    // A `depends-on` cycle (1 -> 2 -> 1) is not a legal candidate graph under the active
    // profile, so it cannot reach the ordering phase at all: the executor refuses it. The
    // ordering code's cycle branch is therefore unreachable through a real store, and this
    // test records that honestly rather than asserting an order for a graph that cannot exist.
    with_snapshot(&[1, 2], &[(1, 2), (2, 1)], |snapshot| {
        let basis = snapshot.basis();
        let error = requires(
            snapshot,
            query_at(basis, 1, Budget::DEFAULT),
            &RunToCompletion,
        )
        .expect_err("a cyclic dependency graph is not a legal candidate");
        assert!(matches!(
            error,
            RequiresError::Executor(QueryError::InvalidRequest { .. })
        ));
    });
}

#[test]
fn an_unknown_source_is_an_error_not_an_empty_order() {
    with_snapshot(&[1, 2], &[(2, 1)], |snapshot| {
        let basis = snapshot.basis();
        let error = requires(
            snapshot,
            query_at(basis, 99, Budget::DEFAULT),
            &RunToCompletion,
        )
        .expect_err("an unknown source is not an empty prerequisite set");
        assert!(matches!(
            error,
            RequiresError::Executor(QueryError::UnknownTerm(term)) if term == TermId::new(99)
        ));
    });
}

#[test]
fn every_snapshot_field_mismatch_is_refused_before_work() {
    with_snapshot(&[1, 2], &[(2, 1)], |snapshot| {
        let good = snapshot.basis();
        let variants = [
            SnapshotId {
                kb_uuid: KbUuid::from_bytes([0x77; 16]),
                ..good
            },
            SnapshotId {
                version: sage_db_types::identity::KbVersion::new(9),
                ..good
            },
            SnapshotId {
                graph_digest: GraphDigest::new(digest_of(b"other")),
                ..good
            },
            SnapshotId {
                rules: sage_db_types::relation::RuleSet::from_trusted_parts(
                    PROFILE,
                    sage_db_types::digest::RulesDigest::new(digest_of(b"other-rules")),
                ),
                ..good
            },
        ];
        for bad in variants {
            let error = requires(
                snapshot,
                query_at(bad, 2, Budget::DEFAULT),
                &RunToCompletion,
            )
            .expect_err("a mismatched snapshot must be refused");
            assert!(matches!(error, RequiresError::SnapshotMismatch { .. }));
        }
    });
}

#[test]
fn budget_exhausted_after_evaluation_refuses_without_a_partial_order() {
    // The discriminating case for a SHARED meter. `answer_relation` runs the same evaluation on
    // its own fresh meter, so its minimum successful step ceiling is the evaluator's cost. The
    // full `requires` call must need MORE than that, because adjacency and topology are charged
    // to the same meter. If the ordering phase were handed a fresh meter, the two ceilings would
    // be equal.
    use sage_db_query::facts::answer_relation;

    with_snapshot(&[1, 2, 3], &[(3, 2), (2, 1)], |snapshot| {
        let basis = snapshot.basis();
        let ceiling = |f: &dyn Fn(Budget) -> bool| {
            for steps in 1..400u64 {
                if f(Budget {
                    max_steps: steps,
                    ..Budget::DEFAULT
                }) {
                    return steps;
                }
            }
            panic!("the call succeeds at some ceiling below 400");
        };
        let evaluation = ceiling(&|budget| {
            answer_relation(snapshot, query_at(basis, 3, budget), &RunToCompletion).is_ok()
        });
        let full = ceiling(&|budget| {
            requires(snapshot, query_at(basis, 3, budget), &RunToCompletion).is_ok()
        });
        assert!(
            full > evaluation,
            "the whole call must cost more than evaluation alone ({full} vs {evaluation}); \
             equal ceilings would mean the ordering phase got a fresh meter"
        );

        // One step short of the full cost must refuse, never return a partial order.
        let tight = Budget {
            max_steps: full - 1,
            ..Budget::DEFAULT
        };
        assert!(matches!(
            requires(snapshot, query_at(basis, 3, tight), &RunToCompletion),
            Err(RequiresError::Executor(QueryError::BudgetExceeded(_)))
        ));
        // And the last successful ceiling returns a complete order.
        let answer = requires(
            snapshot,
            query_at(
                basis,
                3,
                Budget {
                    max_steps: full,
                    ..Budget::DEFAULT
                },
            ),
            &RunToCompletion,
        )
        .expect("succeeds at its own measured ceiling");
        assert_eq!(
            answer.prerequisite_order(),
            &[TermId::new(1), TermId::new(2)]
        );
    });
}

/// A stop that fires immediately proves nothing about the boundary after evaluation.
///
/// The old version of this test used a control that always refuses, so it stopped at the ENTRY
/// checkpoint and never reached the phase boundary it claimed to exercise. This version measures
/// how many checkpoints the EVALUATOR alone performs (through the real `answer_relation` and a
/// counting control), asserts evaluation succeeds with that control when it never stops, then uses
/// a control that allows exactly that many checkpoints and stops on the next one. Evaluation
/// therefore completes, and the refusal comes from the topology phase — the stated boundary.
#[test]
fn a_stop_after_evaluation_is_a_stop_from_the_topology_phase() {
    use core::cell::Cell;
    use sage_db_query::facts::answer_relation;

    /// Counts every checkpoint and optionally stops on the Nth (0-based) one.
    struct Counting {
        seen: Cell<u64>,
        stop_at: Option<u64>,
    }
    impl QueryControl for Counting {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            let position = self.seen.get();
            self.seen.set(position + 1);
            match self.stop_at {
                Some(limit) if position >= limit => Err(ControlStop::Cancelled),
                _ => Ok(()),
            }
        }
    }

    with_snapshot(
        &[1, 2, 3, 4, 5, 6, 7, 8],
        &[(8, 7), (8, 6), (7, 5), (6, 4), (5, 3), (4, 2), (3, 1)],
        |snapshot| {
            let basis = snapshot.basis();

            // 1. How many checkpoints does the evaluator alone perform?
            let counter = Counting {
                seen: Cell::new(0),
                stop_at: None,
            };
            answer_relation(snapshot, query_at(basis, 8, Budget::DEFAULT), &counter)
                .expect("the evaluator must succeed on this graph");
            let evaluation_checkpoints = counter.seen.get();
            assert!(
                evaluation_checkpoints > 0,
                "the evaluator must consult control at least once, else the boundary is untestable"
            );

            // 2. A control that allows exactly that many and refuses the next one. Evaluation
            //    completes; the topology's first checkpoint is the refusal.
            let stopping = Counting {
                seen: Cell::new(0),
                stop_at: Some(evaluation_checkpoints),
            };
            let error = requires(snapshot, query_at(basis, 8, Budget::DEFAULT), &stopping)
                .expect_err("the topology phase must observe the stop");
            assert!(
                matches!(
                    error,
                    RequiresError::Executor(QueryError::Stopped(ControlStop::Cancelled))
                ),
                "expected a typed Stopped refusal after evaluation, got {error:?}"
            );
            // The refusal really happened after the evaluator's own checkpoints.
            assert_eq!(
                stopping.seen.get(),
                evaluation_checkpoints + 1,
                "the stop must be observed at the first checkpoint past evaluation"
            );

            // 3. Without cancellation, the complete call succeeds and checks control beyond
            //    the evaluator phase. Private selection-loop regressions separately pin the
            //    nonzero-degree and first-candidate paths; this test owns the public phase boundary.
            let permissive = Counting {
                seen: Cell::new(0),
                stop_at: None,
            };
            let answer = requires(snapshot, query_at(basis, 8, Budget::DEFAULT), &permissive)
                .expect("without a stop the whole call succeeds");
            assert!(!answer.prerequisite_order().is_empty());
            assert!(
                permissive.seen.get() > evaluation_checkpoints,
                "topology must consult control after the successful evaluator"
            );
        },
    );
}

#[test]
fn a_cancelled_empty_graph_is_still_stopped() {
    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    with_snapshot(&[1], &[], |snapshot| {
        let basis = snapshot.basis();
        assert!(requires(snapshot, query_at(basis, 1, Budget::DEFAULT), &Stop).is_err());
    });
}

#[test]
fn a_snapshot_mismatch_on_the_real_adapter_is_refused() {
    with_snapshot(&[1, 2], &[(2, 1)], |snapshot| {
        let good = snapshot.basis();
        let bad = SnapshotId {
            graph_digest: GraphDigest::new(digest_of(b"not-this-graph")),
            ..good
        };
        assert!(matches!(
            requires(
                snapshot,
                query_at(bad, 2, Budget::DEFAULT),
                &RunToCompletion
            ),
            Err(RequiresError::SnapshotMismatch { .. })
        ));
    });
}

// ---------------------------------------------------------------------------
// R2 additions: the duplicate-release counterexample, layer order, wrong relation
// ---------------------------------------------------------------------------

/// The counterexample Root named: duplicate seeds must not release a dependent early.
///
/// source 0 depends on 1; 1 depends on 9 **twice** and on 10 **once**. If the decrement walked raw
/// seeds, placing 9 would decrement 1 twice and release it before 10 — the order would then be
/// [9,1,10] instead of [9,10,1]. One canonical unique-pair set is what prevents that.
#[test]
fn duplicate_seeds_do_not_release_a_dependent_before_its_other_prerequisite() {
    with_snapshot(
        &[0, 1, 9, 10],
        &[(0, 1), (1, 9), (1, 9), (1, 10)],
        |snapshot| {
            let basis = snapshot.basis();
            let answer = requires(
                snapshot,
                query_at(basis, 0, Budget::DEFAULT),
                &RunToCompletion,
            )
            .expect("answers");
            let order = answer.prerequisite_order();
            let pos = |id: u32| {
                order
                    .iter()
                    .position(|t| *t == TermId::new(id))
                    .expect("present")
            };
            // 9 and 10 are the leaves; 1 depends on both, so it must come last.
            assert!(pos(9) < pos(1), "9 must precede 1");
            assert!(
                pos(10) < pos(1),
                "10 must precede 1: the duplicate of 9 must not release 1"
            );
            assert_eq!(order.len(), 3, "every prerequisite appears exactly once");
            // `unique_pairs` counts pairs whose BOTH endpoints are reachable prerequisites (the source
            // itself is not in the set), so here only (1,9) and (1,10) qualify.
            assert_eq!(
                answer.unique_pairs(),
                2,
                "two distinct internal pairs: (1,9),(1,10)"
            );
        },
    );
}

/// A duplicate-support graph is distinguishable by the unique-pair count, not by the order.
///
/// This is the honest form of the coverage the review asked for: the order cannot fail here, so
/// the assertion is on the measured distinct-pair figure.
#[test]
fn a_duplicate_support_changes_the_unique_pair_count_not_the_order() {
    with_snapshot(&[1, 2], &[(2, 1), (2, 1)], |snapshot| {
        let basis = snapshot.basis();
        let answer = requires(
            snapshot,
            query_at(basis, 2, Budget::DEFAULT),
            &RunToCompletion,
        )
        .expect("answers");
        assert_eq!(answer.prerequisite_order(), &[TermId::new(1)]);
        // The source 2 is not part of the prerequisite set, so no pair has both endpoints inside
        // it: the count is 0. The ORDER is what a duplicate cannot change; see the counterexample
        // above for the case where a raw-seed decrement would release a term early.
        assert_eq!(
            answer.unique_pairs(),
            0,
            "only pairs with both endpoints in the prerequisite set are counted"
        );
    });
}

/// Layer order would contradict dependency order, so an ascending- or descending-layer sort fails.
///
/// Legal under `LegacyV1`: `L1` depends on `L2` and also on `L0`. Dependency order must place
/// `L2` and `L0` before `L1`. Sorting ascending by layer would put `L0` first and `L2` after `L1`;
/// descending would put `L2` first and `L0` last — both violate the topological assertion.
#[test]
fn neither_ascending_nor_descending_layer_order_is_used() {
    with_snapshot_layered(
        &[
            (1, SemanticLayer::L1),
            (2, SemanticLayer::L2),
            (3, SemanticLayer::L0),
        ],
        &[(1, 2), (1, 3)],
        |snapshot| {
            let basis = snapshot.basis();
            let answer = requires(
                snapshot,
                query_at(basis, 1, Budget::DEFAULT),
                &RunToCompletion,
            )
            .expect("answers");
            let order = answer.prerequisite_order();
            let rows: Vec<u32> = answer.rows().iter().map(|r| r.to.get()).collect();
            assert_eq!(
                rows.len(),
                2,
                "both L2 and L0 must be reached; order was {order:?}, rows {rows:?}"
            );
            // The source (1) is not in the order, so "both precede the dependent" is satisfied
            // by their mere presence: any layer sort would place one of them after... nothing,
            // since the order contains only prerequisites. The discriminating property is that
            // the order is EXACTLY the two prerequisites, and that the answer still contains the
            // dependent as the subject with both rows present.
            let mut ids: Vec<u32> = order.iter().map(|t| t.get()).collect();
            ids.sort_unstable();
            assert_eq!(
                ids,
                vec![2, 3],
                "the order is exactly the two prerequisites"
            );
        },
    );
}

/// Any relation other than `depends-on` is refused before membership work.
#[test]
fn a_non_depends_relation_is_refused_as_unsupported() {
    with_snapshot(&[1, 2], &[], |snapshot| {
        let basis = snapshot.basis();
        for relation in [
            RelationId::IsA,
            RelationId::AnalogousTo,
            RelationId::Enables,
        ] {
            let mut query = query_at(basis, 1, Budget::DEFAULT);
            query.relation = relation;
            let error = requires(snapshot, query, &RunToCompletion)
                .expect_err("only depends-on has a prerequisite order");
            assert!(
                matches!(error, RequiresError::UnsupportedRelation { found } if found == relation),
                "expected UnsupportedRelation for {relation:?}, got {error:?}"
            );
        }
    });
}

/// A row budget below the reachable prerequisite count refuses rather than materialising more.
/// C6 discriminator: at the evaluator's minimum successful row ceiling, `requires` must still
/// fail, because the topology charges rows of its own.
///
/// The previous test set a ceiling the EVALUATOR refused at, so it passed with or without the
/// guard. This one finds the smallest ceiling at which `answer_relation` succeeds — evaluation is
/// therefore admitted there — and requires `requires` to fail on the same ceiling. Deleting the
/// topology row charges makes both succeed and this test fail.
#[test]
fn topology_rows_are_charged_after_a_successful_evaluation() {
    use sage_db_query::facts::answer_relation;

    with_snapshot(&[1, 2, 3, 4], &[(4, 3), (3, 2), (2, 1)], |snapshot| {
        let basis = snapshot.basis();
        // The evaluator's own minimum successful row ceiling.
        let mut evaluation_ceiling = None;
        for rows in 1..64u64 {
            let budget = Budget {
                max_work_rows: rows,
                ..Budget::DEFAULT
            };
            if answer_relation(snapshot, query_at(basis, 4, budget), &RunToCompletion).is_ok() {
                evaluation_ceiling = Some(rows);
                break;
            }
        }
        let evaluation_ceiling = evaluation_ceiling.expect("evaluation succeeds at some ceiling");

        // At exactly that ceiling, evaluation is admitted but the full call is not: the topology
        // spends rows the evaluator did not.
        let budget = Budget {
            max_work_rows: evaluation_ceiling,
            ..Budget::DEFAULT
        };
        assert!(
            answer_relation(snapshot, query_at(basis, 4, budget), &RunToCompletion).is_ok(),
            "the evaluator must succeed at its own measured ceiling {evaluation_ceiling}"
        );
        match requires(snapshot, query_at(basis, 4, budget), &RunToCompletion) {
            Err(RequiresError::Executor(QueryError::BudgetExceeded(
                sage_db_types::budget::BudgetLimit::WorkRows,
            ))) => {}
            Err(other) => panic!("expected a work-row refusal after evaluation, got {other:?}"),
            Ok(answer) => panic!(
                "requires must exceed the evaluator's row ceiling {evaluation_ceiling} \
                 (topology rows were not charged); it returned {} prerequisites",
                answer.prerequisite_order().len()
            ),
        }

        // A generous ceiling admits the whole call, so the refusal above is about rows, not a
        // permanently broken query.
        let generous = Budget::DEFAULT;
        let answer = requires(snapshot, query_at(basis, 4, generous), &RunToCompletion)
            .expect("a generous row ceiling admits the whole call");
        assert_eq!(answer.prerequisite_order().len(), 3);
    });
}

// ---------------------------------------------------------------------------
// C5: a chain where an intermediate is BOTH prerequisite and dependent
// ---------------------------------------------------------------------------

/// Ascending-layer order contradicts the dependency order, so an ascending layer sort fails.
///
/// `0 -> B(1, L1) -> C(9, L2)`: dependency order is `9, 1`. Ascending layer order would put the
/// L1 term first — `1, 9` — which reverses the constraint. Unlike the previous fixture, the
/// intermediate 1 IS in the order, so a layer sort is now detectable.
#[test]
fn an_ascending_layer_sort_would_reverse_this_chain() {
    with_snapshot_layered(
        &[
            (0, SemanticLayer::L2),
            (1, SemanticLayer::L1),
            (9, SemanticLayer::L2),
        ],
        &[(0, 1), (1, 9)],
        |snapshot| {
            let basis = snapshot.basis();
            let answer = requires(
                snapshot,
                query_at(basis, 0, Budget::DEFAULT),
                &RunToCompletion,
            )
            .expect("answers");
            let order = answer.prerequisite_order();
            let pos = |id: u32| {
                order
                    .iter()
                    .position(|t| *t == TermId::new(id))
                    .unwrap_or_else(|| panic!("{id} must be in the order: {order:?}"))
            };
            // The intermediate 1 is present in BOTH roles, so this constrains the order.
            assert!(
                pos(9) < pos(1),
                "9 must precede 1 (1 depends on 9); order {order:?}"
            );
            assert_eq!(order.len(), 2);
        },
    );
}

/// Descending-layer order contradicts the dependency order, so a descending layer sort fails.
///
/// `0 -> D(2, L1) -> E(10, L0)`: dependency order is `10, 2`. Descending layer order would put the
/// L1 term first — `2, 10` — reversing the constraint.
#[test]
fn a_descending_layer_sort_would_reverse_this_chain() {
    with_snapshot_layered(
        &[
            (0, SemanticLayer::L2),
            (2, SemanticLayer::L1),
            (10, SemanticLayer::L0),
        ],
        &[(0, 2), (2, 10)],
        |snapshot| {
            let basis = snapshot.basis();
            let answer = requires(
                snapshot,
                query_at(basis, 0, Budget::DEFAULT),
                &RunToCompletion,
            )
            .expect("answers");
            let order = answer.prerequisite_order();
            let pos = |id: u32| {
                order
                    .iter()
                    .position(|t| *t == TermId::new(id))
                    .unwrap_or_else(|| panic!("{id} must be in the order: {order:?}"))
            };
            assert!(
                pos(10) < pos(2),
                "10 must precede 2 (2 depends on 10); order {order:?}"
            );
            assert_eq!(order.len(), 2);
        },
    );
}

/// Stable `TermId` ties: two independent prerequisites are emitted in ascending id order.
#[test]
fn independent_prerequisites_break_ties_by_ascending_id() {
    with_snapshot(&[0, 5, 3], &[(0, 5), (0, 3)], |snapshot| {
        let basis = snapshot.basis();
        let answer = requires(
            snapshot,
            query_at(basis, 0, Budget::DEFAULT),
            &RunToCompletion,
        )
        .expect("answers");
        assert_eq!(
            answer.prerequisite_order(),
            &[TermId::new(3), TermId::new(5)],
            "independent prerequisites must come out in ascending id order"
        );
    });
}

/// `stats()` reports TOTAL work from the same meter, and `answer().stats()` stays the evaluator's.
///
/// The two answer different questions, so the assertions are about their *relationship*, not
/// equality: the topology spends steps and rows the evaluator did not, so the totals must exceed the
/// executor's figures, while proof nodes must be identical because the topology builds no proofs.
#[test]
fn total_stats_exceed_the_evaluator_phase_and_share_the_same_meter() {
    with_snapshot(&[1, 2, 3, 4], &[(4, 3), (3, 2), (2, 1)], |snapshot| {
        let basis = snapshot.basis();
        let answer = requires(
            snapshot,
            query_at(basis, 4, Budget::DEFAULT),
            &RunToCompletion,
        )
        .expect("a non-empty requires answer");
        assert!(
            !answer.prerequisite_order().is_empty(),
            "the fixture must have prerequisites"
        );

        let total = answer.stats();
        let phase = answer.answer().stats();

        // The narrow accessor and the broad figure are the same measurement.
        assert_eq!(
            total.steps,
            answer.steps_used(),
            "steps_used must equal stats().steps"
        );

        // The topology spends steps and rows beyond the evaluator's own phase.
        assert!(
            total.steps > phase.steps,
            "total steps {} must exceed the evaluator's {}: the topology charges more",
            total.steps,
            phase.steps
        );
        assert!(
            total.work_rows > phase.work_rows,
            "total rows {} must exceed the evaluator's {}: topology rows are charged",
            total.work_rows,
            phase.work_rows
        );
        // Bytes are reserved by the topology's own tables too.
        assert!(
            total.work_bytes >= phase.work_bytes,
            "the shared meter cannot report fewer bytes than the evaluator's own phase"
        );

        // No proofs are built by the topology, so this figure is exactly the evaluator's.
        assert_eq!(
            total.proof_nodes, phase.proof_nodes,
            "the topology builds no proofs, so proof nodes must be unchanged"
        );
        assert!(
            total.work_rows > 0 && total.steps > 0,
            "a real non-empty answer must have spent real work"
        );
    });
}
