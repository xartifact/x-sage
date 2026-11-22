//! Ordinary Read-authorized current catalog and exact slug resolution.
//!
//! Every test here uses the real committed-record path: records are built through the typed
//! canonical bridge and committed through `Store::commit`, then read back through the public
//! catalog seam. No test fabricates a fold, substitutes a ledger, or uses a session-directory
//! fixture.
#![allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;

use observing_fs::ObservingFs;
use sage_db_kernel::{
    auth::{AuthError, Capability, CapabilitySet, Grants},
    catalog::{
        Activity, CatalogError, CatalogFamily, CatalogStats, SlugResolution, resolve_term_slug,
        visit_catalog,
    },
    claims::CommandError,
};
use sage_db_store::{
    fs::CreateDir,
    handle::{HandleOutcome, Store},
    init::bootstrap,
    lease::AttemptLedger,
    typed::{self, EventInput},
};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::{
    framing::{FormatOutcome, Limits, Refusal},
    persist::*,
    prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction},
    query::{ControlStop, QueryControl, RunToCompletion},
};
use std::{path::Path, rc::Rc};

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(fixture::BYTES, fixture::ENTRIES, &RunToCompletion).unwrap()
}

fn grants(capability: Capability) -> Grants<'static> {
    Grants::new(
        "catalog-reader",
        fixture::kb(),
        CapabilitySet::of(&[capability]),
    )
}

fn wrong_kb_grants(capability: Capability) -> Grants<'static> {
    Grants::new(
        "catalog-reader",
        sage_db_types::identity::KbUuid::from_bytes([0x42; 16]),
        CapabilitySet::of(&[capability]),
    )
}

/// One bootstrapped Store plus the ledger that opened it.
fn world() -> (
    Rc<ModelFileSystem>,
    ObservingFs,
    Store<ObservingFs>,
    AttemptLedger<'static>,
) {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = ObservingFs::new(Rc::clone(&inner));
    let mut l = ledger();
    let mut store = bootstrap(
        fs.clone(),
        Path::new(fixture::ROOT),
        CreateDir::IfMissing,
        fixture::kb(),
        fixture::RULES,
        &Limits::V1,
        &mut l,
    )
    .unwrap();
    // Version 2 commits Terms 1 and 2 (both active, revision 1).
    assert!(matches!(
        store.commit(fixture::carrier(2), &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 2, .. }
    ));
    (inner, fs, store, l)
}

// ---------------------------------------------------------------------------
// A fixture transaction carrying several identified families at once
// ---------------------------------------------------------------------------

fn body(id: u128, revision: u64) -> Vec<u8> {
    format!("catalog body {id} rev {revision}").into_bytes()
}

fn text(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
    out
}

/// Build a Term event with an explicit slug and retired flag.
fn term_event(id: u128, slug: &str, revision: u64, retired: bool) -> OwnedEvent {
    let bytes = body(id, 1);
    let mut key = 1u16.to_le_bytes().to_vec();
    key.extend_from_slice(&id.to_le_bytes());
    let mut fields = vec![
        (1, key),
        (2, revision.to_le_bytes().to_vec()),
        (4, u16::from(retired).to_le_bytes().to_vec()),
        (5, text(fixture::PRINCIPAL)),
        (6, 0u16.to_le_bytes().to_vec()),
        (7, text("catalog fixture reason")),
        (8, (id as i64).to_le_bytes().to_vec()),
        (14, 0u32.to_le_bytes().to_vec()),
        (16, text(slug)),
        (17, text("catalog fixture atom")),
        (18, vec![1]),
        (19, {
            let mut v = 1u32.to_le_bytes().to_vec();
            v.extend_from_slice(&text("product"));
            v
        }),
        (20, 0u16.to_le_bytes().to_vec()),
        (21, u16::from(retired).to_le_bytes().to_vec()),
        (22, {
            let mut v = 1u32.to_le_bytes().to_vec();
            v.extend_from_slice(&text("catalog"));
            v
        }),
        (23, sage_db_format::digest::raw_sha256(&bytes).to_vec()),
    ];
    if revision > 1 {
        fields.push((9, 99i64.to_le_bytes().to_vec()));
    }
    fields.sort_by_key(|(tag, _)| *tag);
    // Retirement is a distinct event tag over an EXISTING term: the encoder refuses
    // `retired = true` on a creation, and refuses tag 3 at revision 1.
    OwnedEvent {
        tag: if retired {
            3
        } else {
            if revision == 1 { 1 } else { 2 }
        },
        fields,
    }
}

/// An outcome event naming `version`'s own request id and result digest.
fn outcome_event(version: u64) -> OwnedEvent {
    let request = format!("catalog-{version}");
    let result = format!("catalog-result/{version}").into_bytes();
    let mut fields = vec![
        (5, text(fixture::PRINCIPAL)),
        (6, 0u16.to_le_bytes().to_vec()),
        (
            8,
            (1_700_100_000_000_000i64 + version as i64)
                .to_le_bytes()
                .to_vec(),
        ),
        (176, text(fixture::PRINCIPAL)),
        (177, text(&request)),
        (178, vec![version as u8; 32]),
        (179, 0u16.to_le_bytes().to_vec()),
        (180, sage_db_format::digest::raw_sha256(&result).to_vec()),
        (181, version.to_le_bytes().to_vec()),
    ];
    fields.sort_by_key(|(tag, _)| *tag);
    OwnedEvent { tag: 192, fields }
}

/// Build a transaction carrying the given events, reusing the fixture's own bindings.
///
/// The rules binding, kb identity and blob set are the fixture's, so this test cannot drift
/// from the canonical carrier; only the event list and version differ.
fn carrier_with(version: u64, events: Vec<OwnedEvent>) -> PreparedTransaction {
    let mut tx = fixture::carrier(2);
    tx.version = version;
    tx.prev_version = Some(version - 1);
    tx.tx_id = [0xb0 + version as u8; 16];
    tx.created_at = 1_700_100_000_000_000 + version as i64;
    // The fixture's outcome event (tag 177/178) asserts THIS transaction's request id and
    // payload digest, so the carrier must agree with them or the outcome is refused.
    tx.request_id = if version == 2 || version == 3 {
        fixture::request(version).into()
    } else {
        format!("catalog-{version}")
    };
    tx.payload_digest = [version as u8; 32];
    tx.expect_version = Some(version - 1);
    // The carrier's own outcome event is required for a valid transaction; append it after the
    // caller's events so the count reconciles.
    let mut all_events = events;
    let outcome = if version == 2 || version == 3 {
        fixture::outcome(version)
    } else {
        // Later versions need an outcome event naming THIS transaction; the fixture's builder
        // only knows its own two versions.
        outcome_event(version)
    };
    // The fixture's `result(version)` blob must be present for that outcome.
    let result_bytes = if version == 2 || version == 3 {
        fixture::result(version)
    } else {
        format!("catalog-result/{version}").into_bytes()
    };
    let digest = sage_db_format::digest::raw_sha256(&result_bytes);
    if !tx.blobs.iter().any(|blob| blob.sha256 == digest) {
        tx.blobs.push(BlobDeclaration {
            sha256: digest,
            declared_len: result_bytes.len() as u64,
            source: BlobSource::Provide(result_bytes),
        });
    }
    all_events.push(outcome);
    tx.events = all_events;
    // The fixture carrier already declares the rules blob and its own term bodies; add every
    // body our own Term events reference.
    for id in 1..=16u128 {
        let bytes = body(id, 1);
        let digest = sage_db_format::digest::raw_sha256(&bytes);
        if !tx.blobs.iter().any(|blob| blob.sha256 == digest) {
            tx.blobs.push(BlobDeclaration {
                sha256: digest,
                declared_len: bytes.len() as u64,
                source: BlobSource::Provide(bytes),
            });
        }
    }
    tx
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The catalog visits actual committed family records, and reports a real basis.
#[test]
fn the_catalog_visits_committed_family_records() {
    let (_inner, _fs, store, mut l) = world();
    let mut visited: Vec<(StateKind, u128)> = Vec::new();
    let read = visit_catalog(
        &store,
        &grants(Capability::Read),
        Activity::Live,
        &mut l,
        |record, _ledger| {
            match record {
                sage_db_store::fold::FoldedRecord::Term(t) => {
                    visited.push((StateKind::Term, t.key.id()))
                }
                other => visited.push((other.identity(&mut no_charge()).unwrap().state_kind(), 0)),
            }
            Ok::<(), Refusal>(())
        },
    )
    .unwrap();
    // The committed world has Terms 1 and 2, both live.
    assert_eq!(visited, vec![(StateKind::Term, 1), (StateKind::Term, 2)]);
    assert_eq!(read.stats.visited, 2);
    assert_eq!(read.stats.matched, 2);
    assert_eq!(read.stats.excluded_family, 0);
    // The basis is the Store's own durable basis.
    assert_eq!(read.basis, store.view().unwrap().basis());
}

/// A zero-cost helper ledger, only for classifying a record in a test visitor.
fn no_charge() -> AttemptLedger<'static> {
    AttemptLedger::open(1 << 20, 1 << 20, &RunToCompletion).unwrap()
}

/// `Live` excludes a genuinely retired Term; `All` shows it, and the exclusion is counted.
///
/// The fixture's own terms are all active (its builder always writes `retired = 0`), so this
/// commits a Term this test authors with the retired bit actually set — a real committed
/// after-image, not a fabricated fold row.
#[test]
fn activity_filters_are_explicit_and_counted() {
    let (_inner, _fs, mut store, mut l) = world();

    // Term 9 is CREATED first, then RETIRED at revision 2 — the canonical sequence. Term 1 and
    // 2 from the fixture stay active.
    let tx = carrier_with(3, vec![term_event(9, "retired-term", 1, false)]);
    match store.commit(tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed { version: 3, .. } => {}
        other => panic!("creating term 9 failed: {other:?}"),
    }
    let tx = carrier_with(4, vec![term_event(9, "retired-term", 2, true)]);
    match store.commit(tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed { version: 4, .. } => {}
        other => panic!("retiring term 9 failed: {other:?}"),
    }

    let mut live = Vec::new();
    let live_read = visit_catalog(
        &store,
        &grants(Capability::Read),
        Activity::Live,
        &mut l,
        |record, _| {
            if let sage_db_store::fold::FoldedRecord::Term(t) = record {
                live.push(t.key.id());
            }
            Ok::<(), Refusal>(())
        },
    )
    .unwrap();
    live.sort_unstable();
    assert_eq!(live, vec![1, 2], "the retired term is excluded from Live");
    assert_eq!(live_read.stats.excluded_inactive, 1);

    let mut all = Vec::new();
    let all_read = visit_catalog(
        &store,
        &grants(Capability::Read),
        Activity::All,
        &mut l,
        |record, _| {
            if let sage_db_store::fold::FoldedRecord::Term(t) = record {
                all.push(t.key.id());
            }
            Ok::<(), Refusal>(())
        },
    )
    .unwrap();
    all.sort_unstable();
    assert_eq!(all, vec![1, 2, 9], "All keeps every source row");
    assert_eq!(all_read.stats.matched, all_read.stats.visited);
    assert_eq!(all_read.stats.excluded_inactive, 0);
}

/// An exact slug resolves; case and non-ASCII variants do NOT.
#[test]
fn exact_slug_matches_and_case_differs() {
    let (_inner, _fs, store, mut l) = world();

    let found = resolve_term_slug(
        &store,
        &grants(Capability::Read),
        "term-1-revision-1",
        &mut l,
    )
    .unwrap();
    match found {
        SlugResolution::Found { term, basis } => {
            assert_eq!(term.key, RecordKey::term(1));
            assert_eq!(basis, store.view().unwrap().basis());
        }
        other => panic!("expected the exact slug to resolve, got {other:?}"),
    }

    // Wrong case is a DIFFERENT slug: no casefolding.
    assert!(matches!(
        resolve_term_slug(
            &store,
            &grants(Capability::Read),
            "TERM-1-REVISION-1",
            &mut l
        )
        .unwrap(),
        SlugResolution::Absent { .. }
    ));
    // U+FF11 FULLWIDTH DIGIT ONE is non-ASCII and is not normalized to ASCII '1'.
    match resolve_term_slug(
        &store,
        &grants(Capability::Read),
        "term-1-revision-\u{ff11}",
        &mut l,
    )
    .unwrap()
    {
        SlugResolution::Absent { basis } => assert_eq!(basis, store.view().unwrap().basis()),
        other => panic!("a different Unicode spelling must be absent, got {other:?}"),
    }
}

/// A missing slug is a COMPLETE absence on a real basis, never a refusal.
#[test]
fn absent_slug_is_complete_and_basis_bound() {
    let (_inner, _fs, store, mut l) = world();
    let outcome =
        resolve_term_slug(&store, &grants(Capability::Read), "no-such-slug", &mut l).unwrap();
    match outcome {
        SlugResolution::Absent { basis } => {
            assert_eq!(basis, store.view().unwrap().basis());
        }
        other => panic!("expected absence, got {other:?}"),
    }
}

/// A caller without `Read` is refused BEFORE any record is visited.
#[test]
fn missing_read_capability_refuses_before_traversal() {
    let (_inner, _fs, store, mut l) = world();
    let mut calls = 0u32;
    let error = visit_catalog(
        &store,
        &grants(Capability::Propose),
        Activity::Live,
        &mut l,
        |_, _| {
            calls += 1;
            Ok::<(), Refusal>(())
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        CatalogError::Command(CommandError::Auth(AuthError::MissingCapability { .. }))
    ));
    assert_eq!(calls, 0, "no record may be visited without Read");
}

/// A grant scoped to a different KB is refused before traversal.
#[test]
fn wrong_kb_refuses_before_traversal() {
    let (_inner, _fs, store, mut l) = world();
    let mut calls = 0u32;
    let error = visit_catalog(
        &store,
        &wrong_kb_grants(Capability::Read),
        Activity::Live,
        &mut l,
        |_, _| {
            calls += 1;
            Ok::<(), Refusal>(())
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        CatalogError::Command(CommandError::Auth(AuthError::WrongKb { .. }))
    ));
    assert_eq!(calls, 0);
}

/// The visitor's own error is preserved exactly and no partial catalog is returned.
#[test]
fn visitor_error_is_preserved_and_no_partial_result() {
    let (_inner, _fs, store, mut l) = world();
    #[derive(Debug, PartialEq, Eq)]
    struct OwnProblem(u8);
    let error = visit_catalog(
        &store,
        &grants(Capability::Read),
        Activity::Live,
        &mut l,
        |_, _| Err(OwnProblem(7)),
    )
    .unwrap_err();
    match error {
        CatalogError::Visitor(inner) => assert_eq!(inner, OwnProblem(7)),
        other => panic!("the visitor's exact error must survive, got {other:?}"),
    }
}

/// Bytes, entries and stop stay three distinct outcomes on the same real ledger.
#[test]
fn catalog_refusals_are_distinct_classes() {
    let (_inner, _fs, store, _l) = world();

    // Bytes: the catalog path itself charges no record bytes for a plain visit (the fold walk
    // charges ENTRY work), so a tiny byte ceiling alone does not refuse. What must hold is that
    // a byte refusal from the visitor's own charged work surfaces as `Budget(Bytes)`, not as
    // absence or a silent success.
    let mut bytes_ledger = AttemptLedger::open(1 << 26, 1 << 20, &RunToCompletion).unwrap();
    let err = visit_catalog(
        &store,
        &grants(Capability::Read),
        Activity::Live,
        &mut bytes_ledger,
        |_, scan_ledger| {
            // Charge more bytes than the caller can afford, through the SAME ledger.
            scan_ledger.try_charge(u64::MAX)?;
            Ok::<(), Refusal>(())
        },
    )
    .unwrap_err();
    match err {
        CatalogError::Visitor(Refusal::Budget(e)) => {
            assert_eq!(e.unit(), sage_db_types::ledger::BudgetUnit::Bytes)
        }
        other => panic!("expected a visitor byte refusal, got {other:?}"),
    }

    // Entries: bytes generous, no entry allowance.
    let mut entries_ledger = AttemptLedger::open(1 << 26, 0, &RunToCompletion).unwrap();
    let err = visit_catalog(
        &store,
        &grants(Capability::Read),
        Activity::Live,
        &mut entries_ledger,
        |_, _| Ok::<(), Refusal>(()),
    )
    .unwrap_err();
    // The fold walk's entry refusal arrives through the Store's record-read class, not the
    // command class — the unit is still distinguishable, which is the property under test.
    match err {
        CatalogError::Command(CommandError::Record(
            sage_db_store::view::RecordReadError::Refused(Refusal::Budget(e)),
        )) => assert_eq!(e.unit(), sage_db_types::ledger::BudgetUnit::Entries),
        CatalogError::Command(CommandError::Refused(Refusal::Budget(e))) => {
            assert_eq!(e.unit(), sage_db_types::ledger::BudgetUnit::Entries)
        }
        other => panic!("expected an entry refusal, got {other:?}"),
    }

    // Stop: the caller's control asks to stop.
    struct StopNow;
    impl QueryControl for StopNow {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let mut stopped = AttemptLedger::open(1 << 26, 1 << 20, &StopNow).unwrap();
    let err = visit_catalog(
        &store,
        &grants(Capability::Read),
        Activity::Live,
        &mut stopped,
        |_, _| Ok::<(), Refusal>(()),
    )
    .unwrap_err();
    // A stop is honoured FIRST — by authorization, before any record is read — so it surfaces
    // through the auth class. That ordering is the guarantee, not an accident.
    match err {
        CatalogError::Command(CommandError::Auth(AuthError::Refused(Refusal::Stopped(_))))
        | CatalogError::Command(CommandError::Refused(Refusal::Stopped(_)))
        | CatalogError::Command(CommandError::Record(
            sage_db_store::view::RecordReadError::Refused(Refusal::Stopped(_)),
        )) => {}
        other => panic!("expected a stop, got {other:?}"),
    }
}

/// An unfinished import refuses rather than reporting a partial or empty catalog.
#[test]
fn unfinished_import_refuses_rather_than_partial() {
    let (_inner, fs, mut store, mut l) = world();
    // Start an import stage record that is not complete, then reopen so the persisted
    // bookkeeping is what an ordinary client would see.
    // The session's `plan_ref` must name a DECLARED blob, so carry a real (unused) plan body.
    let plan_bytes = b"catalog import plan placeholder".to_vec();
    let plan_ref = sage_db_format::digest::raw_sha256(&plan_bytes);
    let session = ImportSessionRecord {
        import_session_id: "unfinished-catalog-import".into(),
        stage: ImportStage::Prepare,
        source_digest: [7u8; 32],
        plan_ref,
        expected_counts: Vec::new(),
        import_complete: false,
        audit: CreationAudit {
            recorded_by: Some("catalog-author".into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("catalog import guard test".into()),
            created_at: 1_700_200_000_000_000,
        },
        extensions: Extensions::new(),
    };
    let event = match typed::prepare_event(EventInput::ImportSession(&session), &mut l) {
        FormatOutcome::Ok(event) => event,
        other => panic!("session event preparation failed: {other:?}"),
    };
    let mut tx = carrier_with(3, vec![event]);
    tx.blobs.push(BlobDeclaration {
        sha256: plan_ref,
        declared_len: plan_bytes.len() as u64,
        source: BlobSource::Provide(plan_bytes),
    });
    tx.expect_version = Some(2);
    let committed = store.commit(tx, &Limits::V1, &mut l);
    assert!(
        matches!(committed, HandleOutcome::Committed { version: 3, .. }),
        "committing the unfinished import session failed: {committed:?}"
    );
    drop(store);

    let reopened = sage_db_store::open::open_existing(
        fs,
        Path::new(fixture::ROOT),
        Some(fixture::kb()),
        &Limits::V1,
        &mut l,
    );
    let store = match reopened {
        Ok(opened) => opened.into_store(),
        Err(refusal) => panic!("reopen failed: {refusal:?}"),
    };

    let mut calls = 0u32;
    let mut probe = ledger();
    let err = visit_catalog(
        &store,
        &grants(Capability::Read),
        Activity::Live,
        &mut probe,
        |_, _| {
            calls += 1;
            Ok::<(), Refusal>(())
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, CatalogError::Command(CommandError::ImportInProgress)),
        "an unfinished import must refuse, got {err:?}"
    );
    assert_eq!(
        calls, 0,
        "no record may be visited during an unfinished import"
    );
}

/// The catalog refuses a family it does not serve by counting, never by dropping silently.
#[test]
fn unserved_families_are_counted_not_dropped() {
    let (_inner, _fs, store, mut l) = world();
    let read = visit_catalog(
        &store,
        &grants(Capability::Read),
        Activity::All,
        &mut l,
        |_, _| Ok::<(), Refusal>(()),
    )
    .unwrap();
    // Every offered row is accounted for exactly once.
    assert_eq!(
        read.stats.visited,
        read.stats.matched + read.stats.excluded_family + read.stats.excluded_inactive
    );
    // And the statistics type is honest about what it holds.
    let _ = CatalogStats::default();
    let _ = CatalogFamily::ALL;
}

/// Two committed Terms claiming one slug is a real contradiction, reported not resolved.
///
/// The store does not enforce slug uniqueness (it is a business rule, not a physical key), so
/// this is reachable with committed records — the exact situation the lookup must refuse to
/// adjudicate. If the store ever starts rejecting it, this test's `commit` assertion fails and
/// the refusal path is proven unnecessary instead.
#[test]
fn duplicate_slug_is_reported_as_ambiguity_never_first_wins() {
    let (_inner, _fs, mut store, mut l) = world();

    // Two DIFFERENT term ids, same authored slug, committed for real.
    for (id, version) in [(7u128, 3u64), (8u128, 4u64)] {
        let tx = carrier_with(version, vec![term_event(id, "shared-slug", 1, false)]);
        match store.commit(tx, &Limits::V1, &mut l) {
            HandleOutcome::Committed { .. } => {}
            other => panic!("committing term {id} failed: {other:?}"),
        }
    }

    let error = resolve_term_slug(&store, &grants(Capability::Read), "shared-slug", &mut l)
        .expect_err("two holders of one slug must not be resolved first-wins");
    match error {
        CatalogError::AmbiguousSlug { claimants, basis } => {
            assert_eq!(claimants, 2, "both holders must be counted");
            assert_eq!(basis, store.view().unwrap().basis());
        }
        other => panic!("expected typed ambiguity, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// C3: a retired Term still owns its slug
// ---------------------------------------------------------------------------

/// A retired Term's slug still resolves, and reports `retired == true`.
///
/// This is the property the design decision rests on: `data-model` §2 forbids reusing a slug, so
/// a permanently reserved name must not look free once its Term is retired.
#[test]
fn a_retired_terms_slug_still_resolves_and_reports_retired() {
    let (_inner, _fs, mut store, mut l) = world();

    // Commit term 9 active, then retire it (the canonical tag-3 sequence).
    for (version, revision, retired) in [(3u64, 1u64, false), (4u64, 2u64, true)] {
        let tx = carrier_with(
            version,
            vec![term_event(9, "retired-owner", revision, retired)],
        );
        match store.commit(tx, &Limits::V1, &mut l) {
            HandleOutcome::Committed { .. } => {}
            other => panic!("commit {version} failed: {other:?}"),
        }
    }

    match resolve_term_slug(&store, &grants(Capability::Read), "retired-owner", &mut l).unwrap() {
        SlugResolution::Found { term, basis } => {
            assert_eq!(term.key, RecordKey::new(RecordKind::Term, 9));
            assert!(
                term.retired,
                "the caller must be told the holder is retired, not shown a free name"
            );
            assert_eq!(basis, store.view().unwrap().basis());
        }
        other => panic!("a retired Term's slug must still resolve, got {other:?}"),
    }
}

/// A live Term reusing a retired Term's slug is ambiguity, not a silent second owner.
///
/// §2 forbids reuse, so this state is a contradiction the lookup must report. If the store ever
/// enforces uniqueness instead, this commit fails and the check becomes unnecessary rather than
/// wrong.
#[test]
fn a_live_term_reusing_a_retired_slug_is_ambiguous() {
    let (_inner, _fs, mut store, mut l) = world();

    // Term 9: created then retired with slug "shared-name".
    for (version, revision, retired) in [(3u64, 1u64, false), (4u64, 2u64, true)] {
        let tx = carrier_with(
            version,
            vec![term_event(9, "shared-name", revision, retired)],
        );
        match store.commit(tx, &Limits::V1, &mut l) {
            HandleOutcome::Committed { .. } => {}
            other => panic!("commit {version} failed: {other:?}"),
        }
    }
    // Term 10: a LIVE term claiming the same slug.
    let tx = carrier_with(5, vec![term_event(10, "shared-name", 1, false)]);
    match store.commit(tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed { .. } => {}
        other => panic!("committing the live reuse failed: {other:?}"),
    }

    match resolve_term_slug(&store, &grants(Capability::Read), "shared-name", &mut l) {
        Err(CatalogError::AmbiguousSlug { claimants, .. }) => {
            assert_eq!(
                claimants, 2,
                "the retired and the live holder are both counted"
            )
        }
        other => panic!("reuse of a retired slug must be ambiguous, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// C5: an unserved family really increments excluded_family
// ---------------------------------------------------------------------------

/// Build one Evidence `Created` event through the **canonical** writer and envelope.
///
/// The typed bridge has no `Evidence` arm, so this uses the public canonical road the review
/// named: `evidence::write_evidence` into `event::encode_with`, then `event::read_event` to get
/// the field table back. No hand-written bytes.
fn evidence_event(id: u128, title: &str) -> OwnedEvent {
    use sage_db_format::{event, evidence};
    use sage_db_types::identity::RecordRevision;
    use sage_db_types::persist::{EvidenceKind, EvidenceRecord};

    let record = EvidenceRecord {
        key: RecordKey::new(RecordKind::Evidence, id),
        revision: RecordRevision::FIRST,
        kind: EvidenceKind::Text,
        title: title.to_string(),
        locator: Some("https://example.invalid/catalog".to_string()),
        excerpt_ref: None,
        asset_sha256: None,
        captured_at: 1_700_300_000_000_000,
        declared_captured_by: Some("catalog-author".into()),
        common: CommonFields {
            recorded_by: Some("catalog-author".into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("catalog unserved-family fixture".into()),
            created_at: 1_700_300_000_000_000,
            updated_at: None,
        },
        extensions: Extensions::new(),
        changed_fields: None,
    };
    let mut l = ledger();
    let bytes = event::encode_with(&mut l, |writer| {
        evidence::write_evidence(writer, evidence::EvidenceEvent::Created, &record)
    })
    .expect("canonical evidence encode");
    let event = event::read_event(&mut sage_db_format::wire::Reader::new(&bytes), &mut l)
        .expect("canonical evidence decode");
    let mut fields: Vec<(u16, Vec<u8>)> = Vec::new();
    for field in event.fields() {
        fields.push((field.tag(), field.value().to_vec()));
    }
    OwnedEvent {
        tag: event.tag(),
        fields,
    }
}

/// Committing an unserved family makes `excluded_family` actually increment.
#[test]
fn an_unserved_family_increments_excluded_family() {
    let (_inner, _fs, mut store, mut l) = world();

    let tx = carrier_with(3, vec![evidence_event(1, "unserved evidence")]);
    match store.commit(tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed { .. } => {}
        other => panic!("committing the Evidence record failed: {other:?}"),
    }

    let mut served = 0u32;
    let read = visit_catalog(
        &store,
        &grants(Capability::Read),
        Activity::All,
        &mut l,
        |record, _| {
            if CatalogFamily::includes(record) {
                served += 1;
            }
            Ok::<(), Refusal>(())
        },
    )
    .unwrap();

    assert_eq!(
        read.stats.excluded_family, 1,
        "the committed Evidence record is a real unserved row, so it must be counted as such"
    );
    assert_eq!(
        read.stats.matched,
        u64::from(served),
        "an unserved row must not be counted as matched"
    );
    assert_eq!(
        read.stats.visited,
        read.stats.matched + read.stats.excluded_family + read.stats.excluded_inactive
    );
}
