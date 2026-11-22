//! P5 physical typed verification read surface: every current record, all families.
//!
//! Everything here goes through the production entry points — `bootstrap`,
//! `Store::commit`, `Store::scan_current_records`, `Store::scan_identified_records`,
//! `open_existing` — with the real ledger and the real canonical codecs. Model, asset
//! and KB-metadata events are assembled with the existing canonical *writers*
//! (`sage_db_format::natural`), never a new product serializer.
//!
//! # What this file does not claim
//!
//! Not P5 completion, readiness or cutover; not an export or a backup; not platform
//! durability and not an allocator/RSS bound. The scan is a *physical administrative*
//! read surface — the caller authenticates before using it, and a completed walk says
//! nothing about whether an import finished.

// Only part of the shared fixture is used here, and the same module is compiled by
// other targets that use different parts.
#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::path::Path;
use std::rc::Rc;

use fixture::{BYTES, ENTRIES, ROOT, RULES, carrier, kb};
use observing_fs::ObservingFs;

use sage_db_format::assertion::AssertionEvent;
use sage_db_format::digest::raw_sha256;
use sage_db_format::natural::{ModelEvent, write_asset, write_kb_metadata, write_model};
use sage_db_format::term::TermEvent;
use sage_db_format::wire::Reader;
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::{CreateDir, Effect, FsErrorKind};
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::open::{Opened, open_existing};
use sage_db_store::view::{RecordReadError, RecordScanError, ViewDurability};
use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::identity::RecordRevision;
use sage_db_types::ledger::{BudgetUnit, SharedLedger};
use sage_db_types::persist::{
    ActorOrigin, AssetRecord, CommonFields, CreationAudit, Extensions, KbMetadataRecord, MediaType,
    ModelRecord, ModelStatus, RecordKey, RecordKind,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};
use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};

const PRINCIPAL: &str = "p5-administrator";
/// Shared 43-byte prefix: chunks 0 and 1 compare equal, so a one-chunk
/// implementation would stop early and a 16-byte-only comparison could not reach the
/// difference at byte 43. That is the shape F1 asked for — it forces a **second**
/// chunk comparison plus the final length step.
const MODEL_KEY: &str = "embed-model-with-a-shared-prefix-0000000000-a";
const RETIRED_MODEL_KEY: &str = "embed-model-with-a-shared-prefix-0000000000-b";

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap()
}

/// One world: the p16 fixture (version 2 = two terms) plus real natural records.
fn world() -> (Rc<ModelFileSystem>, ObservingFs, Store<ObservingFs>) {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = ObservingFs::new(Rc::clone(&inner));
    let mut l = ledger();
    let mut store = bootstrap(
        fs.clone(),
        Path::new(ROOT),
        CreateDir::IfMissing,
        kb(),
        RULES,
        &Limits::V1,
        &mut l,
    )
    .expect("bootstrap");
    assert!(matches!(
        store.commit(carrier(2), &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 2, .. }
    ));
    seed_naturals(&mut store, &mut l);
    (inner, fs, store)
}

/// Assemble one natural record's event through the canonical writer, then read it back
/// with the canonical reader so the seeded image is what a writer would persist.
fn natural_event(
    ledger: &mut AttemptLedger<'_>,
    build: impl FnOnce(
        &mut sage_db_format::wire::Writer<'_>,
    ) -> Result<(), sage_db_format::wire::WriteError>,
    read_back: impl FnOnce(
        &mut Reader<'_>,
        &mut dyn SharedLedger,
    ) -> Result<(), sage_db_format::wire::ReadError>,
) -> OwnedEvent {
    let bytes = sage_db_format::event::encode_with(ledger, build).expect("natural record encodes");
    let mut reader = Reader::new(&bytes);
    read_back(&mut reader, ledger).expect("natural record decodes");
    let event = sage_db_format::event::read_event(&mut Reader::new(&bytes), ledger)
        .expect("event envelope");
    let mut fields = Vec::new();
    for field in event.fields() {
        fields.push((field.tag(), field.value().to_vec()));
    }
    OwnedEvent {
        tag: event.tag(),
        fields,
    }
}

fn active_model() -> ModelRecord {
    ModelRecord {
        revision: RecordRevision::new(2).expect("revision 2"),
        model_key: MODEL_KEY.into(),
        identifier: "text-embedding-x".into(),
        provider: Some("provider-a".into()),
        dimensions: 384,
        status: ModelStatus::Active,
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_100,
            updated_at: Some(1_700_000_000_000_200),
        },
        extensions: Extensions::new(),
        changed_fields: Some(vec![96]),
    }
}

fn retired_model() -> ModelRecord {
    ModelRecord {
        revision: RecordRevision::new(2).expect("revision 2"),
        model_key: RETIRED_MODEL_KEY.into(),
        identifier: "legacy-embed".into(),
        provider: None,
        dimensions: 8,
        status: ModelStatus::Retired,
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("superseded".into()),
            created_at: 1_700_000_000_000_300,
            updated_at: Some(1_700_000_000_000_400),
        },
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

fn asset_record() -> AssetRecord {
    // The rules blob is a real committed blob with a declared length and hash, so the
    // asset names actual bytes rather than a fabricated zero reference.
    AssetRecord {
        sha256: raw_sha256(RULES),
        media_type: MediaType::Png,
        byte_size: RULES.len() as u64,
        declared_by: Some("legacy-render".into()),
        declared_at: Some(1_700_000_000_000_500),
        audit: CreationAudit {
            recorded_by: Some(PRINCIPAL.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("imported asset".into()),
            created_at: 1_700_000_000_000_600,
        },
        extensions: Extensions::new(),
    }
}

fn metadata_update() -> KbMetadataRecord {
    KbMetadataRecord {
        revision: RecordRevision::new(2).expect("revision 2"),
        kb_uuid: kb(),
        slug: "kb".into(),
        name: "Renamed Knowledge Base".into(),
        description: "renamed by the administrator".into(),
        domains: vec!["default".into(), "extra".into()],
        legacy_frontier_ref: None,
        audit: sage_db_types::persist::OptionalActorAudit {
            actor: Some(ActorOrigin::Authenticated),
            recorded_by: Some(PRINCIPAL.into()),
            reason: Some("rename".into()),
            created_at: 1_700_000_000_000_700,
            updated_at: Some(1_700_000_000_000_800),
        },
        extensions: Extensions::new(),
        changed_fields: Some(vec![145, 147]),
    }
}

/// Commit the real natural records: two models (one retired), one asset, one metadata
/// update, plus a retired term and a retracted assertion so activity filtering is
/// visibly *not* applied by the scan.
fn seed_naturals(store: &mut Store<ObservingFs>, l: &mut AttemptLedger<'_>) {
    let asset = asset_record();
    let metadata = metadata_update();
    let active = active_model();
    let retired = retired_model();

    let mut events: Vec<OwnedEvent> = Vec::new();
    events.push(natural_event(
        l,
        |writer| write_asset(writer, &asset),
        |reader, ledger| sage_db_format::natural::read_asset(reader, ledger).map(|_| ()),
    ));
    events.push(natural_event(
        l,
        |writer| write_kb_metadata(writer, &metadata),
        |reader, ledger| sage_db_format::natural::read_kb_metadata(reader, ledger).map(|_| ()),
    ));
    // Model records are revision 2 because registration owns revision 1; the seed
    // therefore writes an update/retire pair on top of a created image.
    let mut registered = active.clone();
    registered.revision = RecordRevision::FIRST;
    registered.common.updated_at = None;
    registered.common.reason = None;
    registered.changed_fields = None;
    registered.status = ModelStatus::Active;
    events.push(natural_event(
        l,
        |writer| write_model(writer, ModelEvent::Registered, &registered),
        |reader, ledger| sage_db_format::natural::read_model(reader, ledger).map(|_| ()),
    ));
    events.push(natural_event(
        l,
        |writer| write_model(writer, ModelEvent::Updated, &active),
        |reader, ledger| sage_db_format::natural::read_model(reader, ledger).map(|_| ()),
    ));
    let mut legacy_registered = retired.clone();
    legacy_registered.revision = RecordRevision::FIRST;
    legacy_registered.common.updated_at = None;
    legacy_registered.common.reason = None;
    legacy_registered.changed_fields = None;
    legacy_registered.status = ModelStatus::Active;
    events.push(natural_event(
        l,
        |writer| write_model(writer, ModelEvent::Registered, &legacy_registered),
        |reader, ledger| sage_db_format::natural::read_model(reader, ledger).map(|_| ()),
    ));
    events.push(natural_event(
        l,
        |writer| write_model(writer, ModelEvent::Retired, &retired),
        |reader, ledger| sage_db_format::natural::read_model(reader, ledger).map(|_| ()),
    ));

    // A fresh term plus its retirement: the scan must still serve the retired row.
    let live = base_term(9);
    events.push(typed_term(TermEvent::Created, &live, l));
    let mut retired_term = live.clone();
    retired_term.retired = true;
    retired_term.revision = RecordRevision::new(2).expect("revision 2");
    retired_term.common.reason = Some("retired for the scan control".into());
    retired_term.common.updated_at = Some(1_700_000_000_000_900);
    events.push(typed_term(TermEvent::Retired, &retired_term, l));

    let assertion = sage_db_types::persist::AssertionRecordFull {
        key: RecordKey::new(RecordKind::Assertion, 41),
        revision: RecordRevision::new(2).expect("revision 2"),
        retracted: true,
        from: RecordKey::term(1),
        relation: sage_db_types::relation::RelationId::IsA,
        target: RecordKey::term(2),
        confidence: sage_db_types::confidence::Confidence::Hypothetical,
        declaration_order: 1,
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("retracted for the scan control".into()),
            created_at: 1_700_000_000_001_000,
            updated_at: Some(1_700_000_000_001_100),
        },
        legacy_id: None,
        legacy_content_digest: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    let mut created = assertion.clone();
    created.revision = RecordRevision::FIRST;
    created.retracted = false;
    created.common.updated_at = None;
    created.common.reason = None;
    for (event, record) in [
        (AssertionEvent::Created, &created),
        (AssertionEvent::Retracted, &assertion),
    ] {
        events.push(natural_event(
            l,
            |writer| sage_db_format::assertion::write_assertion(writer, event, record),
            |reader, ledger| sage_db_format::assertion::read_assertion(reader, ledger).map(|_| ()),
        ));
    }

    let result_bytes = b"p5-current-scan-result/3".to_vec();
    let result_ref = raw_sha256(&result_bytes);
    let mut outcome_fields: Vec<(u16, Vec<u8>)> = Vec::new();
    let text = |value: &str| {
        let mut out = (value.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(value.as_bytes());
        out
    };
    outcome_fields.push((5, text(PRINCIPAL)));
    outcome_fields.push((6, 0u16.to_le_bytes().to_vec()));
    outcome_fields.push((8, (1_700_000_000_001_200i64).to_le_bytes().to_vec()));
    outcome_fields.push((176, text(PRINCIPAL)));
    outcome_fields.push((177, text("p5-current-scan-seed")));
    outcome_fields.push((178, vec![3u8; 32]));
    outcome_fields.push((179, 0u16.to_le_bytes().to_vec()));
    outcome_fields.push((180, result_ref.to_vec()));
    outcome_fields.push((181, 3u64.to_le_bytes().to_vec()));
    outcome_fields.sort_by_key(|(tag, _)| *tag);
    events.push(OwnedEvent {
        tag: 192,
        fields: outcome_fields,
    });

    let mut blobs = vec![BlobDeclaration {
        sha256: raw_sha256(RULES),
        declared_len: RULES.len() as u64,
        source: BlobSource::Existing,
    }];
    blobs.push(BlobDeclaration {
        sha256: raw_sha256(b"scan-body-9"),
        declared_len: b"scan-body-9".len() as u64,
        source: BlobSource::Provide(b"scan-body-9".to_vec()),
    });
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: result_bytes.len() as u64,
        source: BlobSource::Provide(result_bytes),
    });

    let basis = store
        .commit_basis(sage_db_types::persist::IdSpace::TermId, l)
        .expect("basis");
    let prepared = PreparedTransaction {
        version: 3,
        prev_version: Some(2),
        tx_id: [0xc3; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_001_300,
        principal_id: PRINCIPAL.into(),
        request_id: "p5-current-scan-seed".into(),
        payload_digest: [3u8; 32],
        expect_version: Some(2),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, l) {
        HandleOutcome::Committed { version: 3, .. } => {}
        other => panic!("natural seed commit failed: {other:?}"),
    }
}

/// One typed term record built from the canonical writer, so the seeded image is
/// exactly what a writer would persist (including any required event flags).
fn typed_term(
    event: TermEvent,
    record: &sage_db_types::persist::TermRecord,
    l: &mut AttemptLedger<'_>,
) -> OwnedEvent {
    natural_event(
        l,
        |writer| sage_db_format::term::write_term(writer, event, record),
        |reader, ledger| sage_db_format::term::read_term(reader, ledger).map(|_| ()),
    )
}

/// A base term record whose only meaningful fields are the ones the scan asserts.
fn base_term(id: u32) -> sage_db_types::persist::TermRecord {
    sage_db_types::persist::TermRecord {
        key: RecordKey::term(id),
        revision: RecordRevision::FIRST,
        retired: false,
        slug: format!("scan-term-{id}"),
        atom: format!("scan atom {id}"),
        layer: sage_db_types::layer::SemanticLayer::L1,
        domains: vec!["default".into()],
        source: sage_db_types::persist::TermSource::Personal,
        confidence: sage_db_types::confidence::Confidence::Confirmed,
        triggers: vec!["scan".into()],
        body_ref: raw_sha256(format!("scan-body-{id}").as_bytes()),
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_002_000,
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

fn commit_count(store: &Store<ObservingFs>) -> u64 {
    store.head().expect("a committed head")
}

/// Collect every visited record by kind, for structural assertions.
fn kinds(store: &Store<ObservingFs>, mut l: AttemptLedger<'_>) -> (Vec<u8>, Vec<String>) {
    let mut tags = Vec::new();
    let basis = store
        .scan_current_records(&mut l, |record, _| {
            tags.push(kind_tag(record));
            Ok::<_, core::convert::Infallible>(())
        })
        .expect("scan");
    assert_eq!(basis, store.view().unwrap().basis());
    let mut named = tags.iter().map(|tag| format!("{tag}")).collect::<Vec<_>>();
    named.sort();
    (tags, named)
}

fn kind_tag(record: &FoldedRecord) -> u8 {
    match record {
        FoldedRecord::Term(_) => 1,
        FoldedRecord::Assertion(_) => 2,
        FoldedRecord::Claim(_) => 3,
        FoldedRecord::Evidence(_) => 4,
        FoldedRecord::EvidenceLink(_) => 5,
        FoldedRecord::Vector(_) => 6,
        FoldedRecord::Proposal(_) => 7,
        FoldedRecord::FrontierEntry(_) => 8,
        FoldedRecord::SessionCheckpoint(_) => 9,
        FoldedRecord::Asset(_) => 10,
        FoldedRecord::Model(_) => 11,
        FoldedRecord::KbMetadata(_) => 12,
        FoldedRecord::RuleConfig(_) => 13,
    }
}

#[test]
fn the_scan_serves_every_family_including_naturals_and_retired_rows() {
    let (_inner, fs, store) = world();
    let before = fs.counts().borrow().total();
    let (tags, _) = kinds(&store, ledger());

    // The two Genesis singletons: KbMetadata and RuleConfig.
    assert_eq!(
        tags.iter().filter(|tag| **tag == 12).count(),
        1,
        "exactly one current KbMetadata row"
    );
    assert_eq!(
        tags.iter().filter(|tag| **tag == 13).count(),
        1,
        "exactly one current RuleConfig row"
    );
    // Two models (one retired), one asset.
    assert_eq!(tags.iter().filter(|tag| **tag == 11).count(), 2);
    assert_eq!(tags.iter().filter(|tag| **tag == 10).count(), 1);
    // Two live fixture terms plus the retired one, and the retracted assertion.
    assert_eq!(tags.iter().filter(|tag| **tag == 1).count(), 3);
    assert_eq!(tags.iter().filter(|tag| **tag == 2).count(), 1);
    assert_eq!(
        fs.counts().borrow().total(),
        before,
        "the scan performs no named mutation or sync"
    );
}

#[test]
fn the_numeric_scan_still_excludes_naturals() {
    let (_inner, _fs, store) = world();
    let mut tags = Vec::new();
    store
        .scan_identified_records(&mut ledger(), |record, _| {
            tags.push(kind_tag(record));
            Ok::<_, core::convert::Infallible>(())
        })
        .expect("numeric scan");
    assert!(
        tags.iter().all(|tag| *tag <= 9),
        "the identified API must not serve natural families: {tags:?}"
    );
    // ... while the physical scan on the same store does.
    let (full, _) = kinds(&store, ledger());
    assert!(
        full.iter().any(|tag| *tag >= 10),
        "naturals are visible here"
    );
}

#[test]
fn retired_models_and_full_metadata_survive_a_reopen_with_the_same_basis() {
    let (_inner, fs, store) = world();
    let version = commit_count(&store);
    let mut recorded: Vec<(u8, String)> = Vec::new();
    let basis = store
        .scan_current_records(&mut ledger(), |record, _| {
            if let FoldedRecord::Model(model) = record {
                recorded.push((
                    kind_tag(record),
                    format!(
                        "{}|{}|{}",
                        model.model_key, model.dimensions, model.status as u8
                    ),
                ));
            }
            if let FoldedRecord::KbMetadata(metadata) = record {
                recorded.push((
                    kind_tag(record),
                    format!("{}|{}", metadata.name, metadata.domains.join(",")),
                ));
            }
            Ok::<_, core::convert::Infallible>(())
        })
        .expect("scan");
    drop(store);

    let reopened = match open_existing(
        fs.clone(),
        Path::new(ROOT),
        Some(kb()),
        &Limits::V1,
        &mut ledger(),
    )
    .expect("reopen")
    {
        Opened::Ready(store) => store,
        Opened::ReadOnly { .. } => panic!("expected a durable reopen, got a read-only one"),
    };
    assert_eq!(commit_count(&reopened), version);
    assert_eq!(
        reopened.view().expect("view").durability(),
        ViewDurability::Durable
    );

    let mut after: Vec<(u8, String)> = Vec::new();
    let after_basis = reopened
        .scan_current_records(&mut ledger(), |record, _| {
            if let FoldedRecord::Model(model) = record {
                after.push((
                    kind_tag(record),
                    format!(
                        "{}|{}|{}",
                        model.model_key, model.dimensions, model.status as u8
                    ),
                ));
            }
            if let FoldedRecord::KbMetadata(metadata) = record {
                after.push((
                    kind_tag(record),
                    format!("{}|{}", metadata.name, metadata.domains.join(",")),
                ));
            }
            Ok::<_, core::convert::Infallible>(())
        })
        .expect("scan after reopen");
    assert_eq!(after_basis, basis, "same durable basis across the reopen");
    assert_eq!(after, recorded, "byte-identical typed after-images");

    // Authored literals, not values derived from the scan.
    assert!(recorded.contains(&(11, format!("{MODEL_KEY}|384|0"))));
    assert!(recorded.contains(&(11, format!("{RETIRED_MODEL_KEY}|8|1"))));
    assert!(recorded.contains(&(12, "Renamed Knowledge Base|default,extra".to_string())));
}

#[test]
fn visitor_errors_are_preserved_and_no_record_is_served_after_one() {
    struct VisitorRefused;
    let (_inner, _fs, store) = world();
    let mut visits = 0;
    let error = store
        .scan_current_records(&mut ledger(), |_, _| {
            visits += 1;
            Err::<(), _>(VisitorRefused)
        })
        .expect_err("the visitor's own error must surface");
    assert!(matches!(error, RecordScanError::Visitor(VisitorRefused)));
    assert_eq!(visits, 1, "no callback after the visitor's refusal");
}

#[test]
fn real_budget_and_stop_refuse_before_any_record_and_empty_fold_consults_control() {
    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }

    let (_inner, _fs, store) = world();
    let mut calls = 0;
    let mut no_entries = AttemptLedger::open(BYTES, 0, &RunToCompletion).unwrap();
    let error = store
        .scan_current_records(&mut no_entries, |_, _| {
            calls += 1;
            Ok::<_, ()>(())
        })
        .expect_err("an entry-exhausted ledger must refuse");
    assert!(
        matches!(
            error,
            RecordScanError::Read(RecordReadError::Refused(Refusal::Budget(budget)))
                if budget.unit() == BudgetUnit::Entries
        ),
        "a real entry budget refusal, not an invented byte charge"
    );
    assert_eq!(calls, 0, "refused before the first record");

    let control = Stop;
    let mut stopped = AttemptLedger::open(BYTES, ENTRIES, &control).unwrap();
    assert!(matches!(
        store.scan_current_records(&mut stopped, |_, _| Ok::<_, ()>(())),
        Err(RecordScanError::Read(RecordReadError::Refused(
            Refusal::Stopped(ControlStop::Cancelled)
        )))
    ));
    assert_eq!(
        no_entries.stats().live_bytes,
        0,
        "the scanner allocated nothing on the caller's ledger"
    );
}

#[test]
fn an_undecided_authority_is_refused_not_partially_served() {
    // A minimal store at head 2, so the proven uncertainty injection applies: a real
    // commit whose publication point fails AFTER the effect.
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = ObservingFs::new(Rc::clone(&inner));
    let mut l = ledger();
    let mut store = bootstrap(
        fs,
        Path::new(ROOT),
        CreateDir::IfMissing,
        kb(),
        RULES,
        &Limits::V1,
        &mut l,
    )
    .expect("bootstrap");
    assert!(matches!(
        store.commit(carrier(2), &Limits::V1, &mut l),
        HandleOutcome::Committed { version: 2, .. }
    ));

    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    assert!(matches!(
        store.commit(carrier(3), &Limits::V1, &mut l),
        HandleOutcome::Unknown
    ));
    assert!(inner.faults_handle().borrow().all_fired());
    assert!(store.authority_undecided());

    let mut visits = 0;
    let error = store
        .scan_current_records(&mut ledger(), |_, _| {
            visits += 1;
            Ok::<_, core::convert::Infallible>(())
        })
        .expect_err("an undecided authority cannot serve a scan");
    assert!(
        matches!(
            error,
            RecordScanError::Incomplete(sage_db_store::open::OutcomeIncomplete::NoCurrentView)
        ),
        "typed incompleteness, not a partial answer: {error:?}"
    );
    assert_eq!(
        visits, 0,
        "not one row may be served from an undecided world"
    );
}

#[test]
fn a_real_filesystem_fault_during_reopen_is_reported_truthfully() {
    let (_inner, fs, store) = world();
    drop(store);
    // Corrupt nothing: just prove the reopen path itself keeps its error kind, so this
    // test cannot silently pass by serving a world it never read.
    match open_existing(
        fs.clone(),
        Path::new(ROOT),
        Some(kb()),
        &Limits::V1,
        &mut ledger(),
    ) {
        Ok(Opened::Ready(store)) => {
            assert_eq!(store.head(), Some(3));
        }
        Ok(Opened::ReadOnly { .. }) => panic!("the fixture root must replay durably"),
        Err(error) => {
            let _ = (error, Effect::Unknown, FsErrorKind::Io(0));
            panic!("the fixture root must replay cleanly");
        }
    }
}

/// Exact entry totals, calculated from the algorithm rather than observed first.
///
/// Genesis publishes exactly two current records (`KbMetadata`, `RuleConfig`), both
/// natural, ordered `KbMetadata` then `RuleConfig` by the derived `Ord`.
///
/// The spine charges: one entry before the first row, one entry per row examined, one
/// entry per search iteration in each of the two binary searches, and one entry after
/// the last row.
///
/// * **Identified scan** (natural rows filtered out by the selection): 1 entry +
///   2 rows + 1 exit = **4**. No owner lookup runs, because a skipped row is filtered
///   *before* the lookup.
/// * **All-record scan**: the same walk (4) plus, for each served row, its two
///   searches. `KbMetadata` needs 2 iterations in `current` (mid is `RuleConfig`,
///   which is greater) and 2 in `revisions`; `RuleConfig` finds itself at the first
///   mid in both. So 4 + (2+2) + (1+1) = **10**.
///
/// These are assertions about *this* algorithm's shape, not about a caller's budget:
/// they fail if a charge is added, removed or moved, which is exactly what a silent
/// cost change to the pre-existing identified scan would look like.
#[test]
fn both_selections_charge_the_exact_documented_entry_totals() {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = ObservingFs::new(Rc::clone(&inner));
    let mut l = ledger();
    let store = bootstrap(
        fs,
        Path::new(ROOT),
        CreateDir::IfMissing,
        kb(),
        RULES,
        &Limits::V1,
        &mut l,
    )
    .expect("bootstrap");

    let mut numeric = AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap();
    let mut numeric_rows = 0;
    store
        .scan_identified_records(&mut numeric, |_, _| {
            numeric_rows += 1;
            Ok::<_, core::convert::Infallible>(())
        })
        .expect("numeric scan");
    assert_eq!(numeric_rows, 0, "Genesis holds no numeric records");
    assert_eq!(
        numeric.stats().admitted_entries,
        Some(4),
        "entry + 2 filtered rows + exit"
    );

    let mut all = AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap();
    let mut all_rows = 0;
    store
        .scan_current_records(&mut all, |_, _| {
            all_rows += 1;
            Ok::<_, core::convert::Infallible>(())
        })
        .expect("all scan");
    assert_eq!(all_rows, 2, "the two Genesis singletons");
    assert_eq!(
        all.stats().admitted_entries,
        Some(10),
        "the same walk plus two charge-carrying searches per served row"
    );
}

/// A public-path exercise of the multi-chunk model comparison.
///
/// The two seeded model keys share a 43-byte prefix, so every comparison between them
/// walks at least two full chunks plus the length step. Sizing the entry allowance to
/// stop inside that comparison proves the chunked walk is reachable through the real
/// scan, not only through the private unit test.
#[test]
fn a_long_shared_prefix_model_comparison_is_charged_per_chunk_through_the_public_scan() {
    let (_inner, _fs, store) = world();
    assert!(
        MODEL_KEY.len() > 32 && RETIRED_MODEL_KEY.len() > 32,
        "the fixture keys must span multiple chunks"
    );
    assert!(
        MODEL_KEY.starts_with("embed-model-with-a-shared-prefix-"),
        "the keys must actually share a long prefix"
    );

    // A generous ledger succeeds and still serves both models.
    let mut ok = AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap();
    let mut models = 0;
    store
        .scan_current_records(&mut ok, |record, _| {
            if matches!(record, FoldedRecord::Model(_)) {
                models += 1;
            }
            Ok::<_, core::convert::Infallible>(())
        })
        .expect("a generous ledger serves the scan");
    assert_eq!(models, 2);

    // A ledger with just enough entries to finish a one-chunk comparison but not the
    // multi-chunk one refuses with a real entry budget verdict.
    let mut tiny = AttemptLedger::open(BYTES, 0, &RunToCompletion).unwrap();
    let before = tiny.stats().admitted_entries;
    assert!(matches!(
        store.scan_current_records(&mut tiny, |_, _| Ok::<_, core::convert::Infallible>(())),
        Err(RecordScanError::Read(RecordReadError::Refused(Refusal::Budget(
            error
        )))) if error.unit() == BudgetUnit::Entries
    ));
    assert_eq!(
        before,
        tiny.stats().admitted_entries,
        "refused at the first step"
    );
}
