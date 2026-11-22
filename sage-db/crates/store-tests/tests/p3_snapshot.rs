//! P3 snapshot: the read-only view and the actual typed base graph, over a real Store.
//!
//! Everything here goes through public entry points: `bootstrap`, `Store::commit`,
//! `open_existing`, and the kernel snapshot adapter. Records are seeded with the
//! canonical codecs (term events through the shared process fixture, assertions and
//! claims through the `typed` bridge), so a seeded row is byte-identical to what a
//! writer would persist.
//!
//! # What this file does not claim
//!
//! Not P3 phase acceptance: there is no query crate, no closure, no proof, no cache
//! and no network. The base graph is validated by `BaseGraph`'s own construction
//! rules (duplicate identity, dangling endpoint, self reference), which is **not**
//! the V1–V7 candidate-validation set — that belongs to `infer`, explicitly later.

// The shared process fixture is a support module: only part of it is used here, and the
// same file is compiled by other targets that use different parts.
#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
// The support backend is shared with other targets that use the parts this file does not.
#[allow(dead_code)]
#[path = "support/shared_fs.rs"]
mod shared_fs;

use std::rc::Rc;

use fixture::{BYTES, ENTRIES, ROOT, RULES, kb};
use shared_fs::SharedFs;

use sage_db_format::assertion::{AssertionEvent, write_assertion};
use sage_db_format::digest::raw_sha256;
use sage_db_format::event::encode_with;
use sage_db_kernel::snapshot::{Expectation, Snapshot, SnapshotBuildError};
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::open::{Opened, open_existing};
use sage_db_store::view::ViewDurability;
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::graph::GraphBuildRefusal;
use sage_db_types::identity::{KbUuid, KbVersion, RecordRevision};
use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::persist::{
    ActorOrigin, AssertionRecordFull, ClaimKind, ClaimRecord, ClaimStatus, CommonFields,
    Extensions, IdSpace, RecordKey, RecordKind, RevPair, SessionCheckpointRecord, TermRecord,
    TermSource,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};
use sage_db_types::query::ControlStop;
use sage_db_types::relation::{RelationId, RuleProfile};
use sage_db_types::snapshot::{
    ReadRefusal, RecordRef, RecordView, ScanError, SnapshotFamily, SnapshotOpenError, SnapshotView,
};

const PRINCIPAL: &str = "p3-reader";

fn control() -> &'static sage_db_types::query::RunToCompletion {
    &sage_db_types::query::RunToCompletion
}

/// A ledger with finite allowances, so admission is observable.
struct FixedLedger {
    bytes: u64,
    entries: u64,
}

impl FixedLedger {
    const fn new(bytes: u64, entries: u64) -> Self {
        Self { bytes, entries }
    }
}

impl SharedLedger for FixedLedger {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        if bytes > self.bytes {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.bytes));
        }
        self.bytes -= bytes;
        Ok(())
    }
    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        if count > self.entries {
            return Err(BudgetError::new(BudgetUnit::Entries, count, self.entries));
        }
        self.entries -= count;
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

/// A ledger that refuses on the nth checkpoint, so a stop is observable mid-walk.
struct StopsAfter {
    remaining: u32,
}

impl StopsAfter {
    const fn new(checkpoints: u32) -> Self {
        Self {
            remaining: checkpoints,
        }
    }
}

impl SharedLedger for Stopping {
    fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        if self.remaining == 0 {
            return Err(ControlStop::Cancelled);
        }
        self.remaining -= 1;
        Ok(())
    }
}

/// A ledger that refuses the first entry charge, so a budget refusal is distinct from
/// a stop.
struct NoEntries;

impl SharedLedger for NoEntries {
    fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        Err(BudgetError::new(BudgetUnit::Entries, count, 0))
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

// The `StopsAfter` name above is only used through this alias, so the type keeps a
// descriptive name at the definition and a short one at the call sites.
type Stopping = StopsAfter;

fn rules_formal() -> sage_db_types::prepared::RulesBinding {
    use sage_db_format::digest::{Domain, digest};
    sage_db_types::prepared::RulesBinding {
        profile: RuleProfile::FormalV1,
        rules_digest: digest(Domain::Rules, RULES),
        rules_ref: raw_sha256(RULES),
    }
}

fn str_value(text: &str) -> Vec<u8> {
    let mut value = (text.len() as u32).to_le_bytes().to_vec();
    value.extend_from_slice(text.as_bytes());
    value
}

fn outcome_event(version: u64, request_id: &str, result_ref: [u8; 32]) -> OwnedEvent {
    let mut fields = vec![
        (5, str_value(PRINCIPAL)),
        (6, 0u16.to_le_bytes().to_vec()),
        (
            8,
            (1_700_000_000_000_000i64 + version as i64)
                .to_le_bytes()
                .to_vec(),
        ),
        (176, str_value(PRINCIPAL)),
        (177, str_value(request_id)),
        (178, vec![version as u8; 32]),
        (179, 0u16.to_le_bytes().to_vec()),
        (180, result_ref.to_vec()),
        (181, version.to_le_bytes().to_vec()),
    ];
    fields.sort_by_key(|(tag, _)| *tag);
    OwnedEvent { tag: 192, fields }
}

/// An assertion event, encoded by the canonical writer (no hand-built field bytes).
fn assertion_event(event: AssertionEvent, record: &AssertionRecordFull) -> OwnedEvent {
    let mut ledger = FixedLedger::new(u64::MAX, u64::MAX);
    let bytes = encode_with(&mut ledger, |writer| write_assertion(writer, event, record))
        .expect("the canonical writer accepts the fixture");
    let mut reader = sage_db_format::wire::Reader::new(&bytes);
    let decoded = sage_db_format::event::read_event(&mut reader, &mut ledger)
        .expect("the canonical reader accepts its own writer's bytes");
    let mut fields: Vec<(u16, Vec<u8>)> = decoded
        .fields()
        .iter()
        .map(|field| (field.tag(), field.value().to_vec()))
        .collect();
    fields.sort_by_key(|(tag, _)| *tag);
    OwnedEvent {
        tag: decoded.tag(),
        fields,
    }
}

/// The text blob a seeded claim points at. The digest is of these actual bytes, because
/// a declared ref the transaction cannot provide is a dangling blob, not a fixture.
fn claim_text(id: u128) -> Vec<u8> {
    format!("p3 claim text {id}").into_bytes()
}

/// The content blob a seeded checkpoint points at.
fn checkpoint_content(id: u128) -> Vec<u8> {
    format!("p3 checkpoint content {id}").into_bytes()
}

/// A term event, encoded by the canonical writer and read back through the canonical reader.
fn term_event(event: sage_db_format::term::TermEvent, record: &TermRecord) -> OwnedEvent {
    let mut ledger = FixedLedger::new(u64::MAX, u64::MAX);
    let bytes = encode_with(&mut ledger, |writer| {
        sage_db_format::term::write_term(writer, event, record)
    })
    .expect("the canonical writer accepts the term");
    let mut reader = sage_db_format::wire::Reader::new(&bytes);
    let decoded = sage_db_format::event::read_event(&mut reader, &mut ledger)
        .expect("the canonical reader accepts its own writer's bytes");
    let mut fields: Vec<(u16, Vec<u8>)> = decoded
        .fields()
        .iter()
        .map(|field| (field.tag(), field.value().to_vec()))
        .collect();
    fields.sort_by_key(|(tag, _)| *tag);
    OwnedEvent {
        tag: decoded.tag(),
        fields,
    }
}

fn claim_record(id: u128, concept: u32) -> ClaimRecord {
    ClaimRecord {
        key: RecordKey::new(RecordKind::Claim, id),
        revision: RecordRevision::FIRST,
        status: ClaimStatus::Accepted,
        concept: RecordKey::term(concept),
        kind: ClaimKind::Observation,
        text_ref: raw_sha256(&claim_text(id)),
        confidence: sage_db_types::confidence::Confidence::Confirmed,
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.to_owned()),
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

fn checkpoint_record(id: u128, concept: u32) -> SessionCheckpointRecord {
    SessionCheckpointRecord {
        key: RecordKey::new(RecordKind::SessionCheckpoint, id),
        revision: RecordRevision::FIRST,
        location: sage_db_types::persist::CheckpointLocation::Term {
            concept: RecordKey::term(concept),
            session_no: 1,
        },
        content_ref: raw_sha256(&checkpoint_content(id)),
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

/// The term fixture used for both creation and retirement, so the retirement is a
/// revision of the same record rather than a different one.
fn term_record(id: u32) -> TermRecord {
    TermRecord {
        key: RecordKey::term(id),
        revision: RecordRevision::FIRST,
        retired: false,
        slug: format!("term-{id}"),
        atom: format!("atom {id}"),
        layer: sage_db_types::layer::SemanticLayer::L1,
        domains: Vec::new(),
        source: TermSource::Personal,
        confidence: sage_db_types::confidence::Confidence::Confirmed,
        triggers: Vec::new(),
        // The body is a real content-addressed ref whose bytes the transaction declares.
        body_ref: raw_sha256(&fixture::body(id as u128, 1)),
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.to_owned()),
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

/// The declared blobs a seeded transaction provides: its rules bytes plus every
/// content-addressed reference its events name.
fn seed_blobs(events: &[OwnedEvent], extra: Vec<Vec<u8>>) -> Vec<BlobDeclaration> {
    let mut blobs = vec![BlobDeclaration {
        sha256: raw_sha256(RULES),
        declared_len: RULES.len() as u64,
        source: BlobSource::Provide(RULES.to_vec()),
    }];
    // Every content-addressed reference this transaction names must have real bytes
    // declared: the store refuses an undeclared ref rather than writing a dangling blob.
    for bytes in extra {
        blobs.push(BlobDeclaration {
            sha256: raw_sha256(&bytes),
            declared_len: bytes.len() as u64,
            source: BlobSource::Provide(bytes),
        });
    }
    let _ = events;
    blobs
}

fn seed(
    store: &mut Store<SharedFs>,
    ledger: &mut AttemptLedger<'_>,
    version: u64,
    mut events: Vec<OwnedEvent>,
    affected: Vec<RevPair>,
    extra_blobs: Vec<Vec<u8>>,
) {
    let basis = store.commit_basis(IdSpace::TermId, ledger).expect("basis");
    let result_bytes = sage_db_store::typed::result_payload(version, &affected, &[], ledger)
        .expect("result payload");
    let result_ref = raw_sha256(&result_bytes);
    events.push(outcome_event(
        version,
        &format!("p3-seed-{version}"),
        result_ref,
    ));
    let mut blobs = seed_blobs(&events, extra_blobs);
    blobs.push(BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: BlobSource::Existing,
    });
    blobs.push(BlobDeclaration {
        sha256: result_ref,
        declared_len: result_bytes.len() as u64,
        source: BlobSource::Provide(result_bytes),
    });
    let prepared = PreparedTransaction {
        version,
        prev_version: Some(version - 1),
        tx_id: [0x40 + version as u8; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000 + version as i64,
        principal_id: PRINCIPAL.to_owned(),
        request_id: format!("p3-seed-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: Some(version - 1),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: rules_formal(),
    };
    match store.commit(prepared, &Limits::V1, ledger) {
        HandleOutcome::Committed { version: at, .. } => assert_eq!(at, version),
        other => panic!("seed commit at {version} failed: {other:?}"),
    }
}

/// Bootstrap, then commit one transaction holding the records every test reads.
///
/// Two active terms, one retired term, one active assertion, one retracted
/// assertion, one claim and one session checkpoint. The mix is deliberate: each test
/// asserts against a state where the *wrong* filtering rule would be visible.
fn world() -> (Rc<ModelFileSystem>, SharedFs, Store<SharedFs>) {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = SharedFs::from_rc(Rc::clone(&inner));
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let mut store = bootstrap(
        fs.clone(),
        std::path::Path::new(ROOT),
        sage_db_store::fs::CreateDir::IfMissing,
        kb(),
        RULES,
        &Limits::V1,
        &mut ledger,
    )
    .expect("bootstrap");

    let mut events = vec![
        fixture::term(1, 1),
        fixture::term(2, 1),
        fixture::term(3, 1),
    ];

    // One active assertion and one retracted one, both through the canonical codec.
    let active = AssertionRecordFull {
        key: RecordKey::new(RecordKind::Assertion, 11),
        revision: RecordRevision::FIRST,
        retracted: false,
        from: RecordKey::term(1),
        relation: RelationId::IsA,
        target: RecordKey::term(2),
        confidence: sage_db_types::confidence::Confidence::Hypothetical,
        declaration_order: 1,
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        legacy_id: None,
        legacy_content_digest: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    events.push(assertion_event(AssertionEvent::Created, &active));

    let claim = claim_record(21, 1);
    events.push(
        match sage_db_store::typed::prepare_event(
            sage_db_store::typed::EventInput::Claim(
                sage_db_store::typed::ClaimEvent::Created,
                &claim,
            ),
            &mut ledger,
        ) {
            sage_db_types::framing::FormatOutcome::Ok(event) => event,
            other => panic!("claim event: {other:?}"),
        },
    );

    let checkpoint = checkpoint_record(31, 1);
    events.push(
        match sage_db_store::typed::prepare_event(
            sage_db_store::typed::EventInput::SessionCheckpoint(&checkpoint),
            &mut ledger,
        ) {
            sage_db_types::framing::FormatOutcome::Ok(event) => event,
            other => panic!("checkpoint event: {other:?}"),
        },
    );

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
            key: RecordKey::term(3),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: active.key,
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: claim.key,
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: checkpoint.key,
            revision: RecordRevision::FIRST,
        },
    ];
    // Term bodies live behind `body_ref`; claim text and checkpoint content behind their
    // own refs. All of them are declared here, with the bytes their digests name.
    let extras = vec![
        fixture::body(1, 1),
        fixture::body(2, 1),
        fixture::body(3, 1),
        claim_text(21),
        checkpoint_content(31),
    ];
    seed(&mut store, &mut ledger, 2, events, affected, extras);

    // A second transaction retires term 3 and retracts assertion 11. Both lifecycle
    // changes are ordinary revisions carrying the complete after-image, so the current
    // rows the snapshot reads are the revised ones.
    let retired_term = TermRecord {
        retired: true,
        revision: RecordRevision::new(2).expect("revision 2"),
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("retired by the p3 fixture".to_owned()),
            created_at: 1_700_000_000_000_000,
            updated_at: Some(1_700_000_300_000_000),
        },
        ..term_record(3)
    };
    let mut retracted_assertion = active.clone();
    retracted_assertion.retracted = true;
    retracted_assertion.revision = RecordRevision::new(2).expect("revision 2");
    // A non-creation revision requires the audit fields the retirement event carries.
    retracted_assertion.common.updated_at = Some(1_700_000_300_000_000);
    retracted_assertion.common.reason = Some("retracted by the p3 fixture".to_owned());
    let mut second = vec![
        term_event(sage_db_format::term::TermEvent::Retired, &retired_term),
        assertion_event(AssertionEvent::Retracted, &retracted_assertion),
    ];
    let second_affected = vec![
        RevPair {
            key: RecordKey::term(3),
            revision: RecordRevision::new(2).expect("revision 2"),
        },
        RevPair {
            key: active.key,
            revision: RecordRevision::new(2).expect("revision 2"),
        },
    ];
    // The retired term's body bytes are referenced by its after-image too.
    seed(
        &mut store,
        &mut ledger,
        3,
        std::mem::take(&mut second),
        second_affected,
        vec![fixture::body(3, 1)],
    );
    (inner, fs, store)
}

fn open_fresh<'a>(fs: SharedFs, ledger: &mut AttemptLedger<'a>) -> Opened<SharedFs> {
    open_existing(
        fs,
        std::path::Path::new(ROOT),
        Some(kb()),
        &Limits::V1,
        ledger,
    )
    .expect("the committed root reopens")
}

#[test]
fn a_snapshot_serves_the_records_and_the_graph_from_one_durable_view() {
    let (_inner, _fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let snapshot =
        Snapshot::open(&mut store, Expectation::latest(), &mut ledger).expect("snapshot");

    // One basis, durable, and the version the commit produced.
    // Construction only succeeds on a durable publication, so reaching here is the assertion.
    assert_eq!(
        snapshot.store().view().expect("view").durability(),
        ViewDurability::Durable
    );
    assert_eq!(
        snapshot.basis().version,
        KbVersion::new(3),
        "the fixture retires and retracts in a second transaction"
    );
    assert_eq!(snapshot.basis().kb_uuid, kb());
    // The graph identity comes from the committed pin. Read through the view's own borrowed
    // store, which is the same publication (the mutable borrow used for construction is over).
    assert_eq!(
        snapshot.basis(),
        *snapshot.store().view().expect("view").snapshot().id()
    );

    // Rules come from the committed configuration.
    assert_eq!(snapshot.committed_profile(), RuleProfile::FormalV1);
    assert_eq!(snapshot.rules().profile, RuleProfile::FormalV1);

    // Counts by family: three terms, one assertion, one claim, one checkpoint.
    let counts = snapshot.counts();
    assert_eq!(counts.terms, 3);
    assert_eq!(counts.assertions, 1);
    assert_eq!(counts.claims, 1);
    assert_eq!(counts.checkpoints, 1);

    // The graph is built from active authored rows only.
    let graph = snapshot.graph();
    assert_eq!(
        graph.term_count(),
        2,
        "three current terms, of which one is retired and therefore excluded"
    );
    assert_eq!(
        graph.assertion_count(),
        0,
        "the assertion is retracted, so no active authored edge remains"
    );
    assert_eq!(graph.snapshot(), snapshot.basis());

    // Point reads: present, and full-record equality against the commit's own value.
    let mut read = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    match snapshot
        .record(RecordKey::term(1), &mut read)
        .expect("read")
    {
        RecordView::Found(RecordRef::Term(term)) => {
            assert_eq!(term.key, RecordKey::term(1));
            assert!(!term.retired);
            assert_eq!(term.revision, RecordRevision::FIRST);
        }
        other => panic!("expected the active term, got {other:?}"),
    }

    // Point reads: absent for a key that is not current, without pretending otherwise.
    match snapshot
        .record(RecordKey::term(99), &mut read)
        .expect("read")
    {
        RecordView::Absent { key } => assert_eq!(key, RecordKey::term(99)),
        other => panic!("expected absence, got {other:?}"),
    }

    // Wrong kind for an existing id: the kind is part of identity, so this is absent,
    // not a different record silently returned.
    match snapshot
        .record(RecordKey::new(RecordKind::Claim, 1), &mut read)
        .expect("read")
    {
        RecordView::Absent { .. } => {}
        other => panic!("a term id read as a claim must be absent, got {other:?}"),
    }
    drop(store);
}

#[test]
fn scans_visit_every_record_of_a_family_and_charge_the_caller() {
    let (_inner, _fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let snapshot =
        Snapshot::open(&mut store, Expectation::latest(), &mut ledger).expect("snapshot");

    let mut charge = FixedLedger::new(u64::MAX, 100);
    let before = charge.entries;
    let mut terms = 0usize;
    snapshot
        .scan_family::<()>(SnapshotFamily::Term, &mut charge, &mut |record| {
            assert_eq!(record.family(), SnapshotFamily::Term);
            terms += 1;
            Ok(())
        })
        .expect("scan");
    assert_eq!(
        terms, 3,
        "the scan sees retired terms too: activity is a caller filter"
    );
    assert_eq!(
        charge.entries,
        before - 3,
        "each visited record charges exactly one entry"
    );

    // A stop during the walk is a stop, not a completed scan.
    let mut stopping = Stopping::new(1);
    let result = snapshot.scan_family::<()>(SnapshotFamily::Term, &mut stopping, &mut |_| Ok(()));
    match result {
        Err(ScanError::Refused(ReadRefusal::Refused(Refusal::Stopped(ControlStop::Cancelled)))) => {
        }
        other => panic!("a cancelled walk must report a stop, got {other:?}"),
    }

    // A budget refusal is a different answer from a stop.
    let mut no_entries = NoEntries;
    match snapshot.scan_family::<()>(SnapshotFamily::Term, &mut no_entries, &mut |_| Ok(())) {
        Err(ScanError::Refused(ReadRefusal::Refused(Refusal::Budget(error)))) => {
            assert_eq!(error.unit(), BudgetUnit::Entries);
        }
        other => panic!("an entry refusal must be a budget refusal, got {other:?}"),
    }

    // An empty family on a stopped caller is still a stop: not "zero results".
    let mut stops_immediately = Stopping::new(0);
    assert!(matches!(
        snapshot.scan_family::<()>(
            SnapshotFamily::SessionCheckpoint,
            &mut stops_immediately,
            &mut |_| Ok(())
        ),
        Err(ScanError::Refused(_))
    ));
}

#[test]
fn an_unverified_or_wrong_version_publication_is_refused_not_answered() {
    let (_inner, _fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");

    // Naming a version the publication is not: refused, and specifically as a version
    // mismatch so a caller cannot mistake it for "not durable".
    match Snapshot::open(
        &mut store,
        Expectation {
            kb: Some(kb()),
            version: Some(KbVersion::new(7)),
        },
        &mut ledger,
    ) {
        Err(SnapshotBuildError::View(SnapshotOpenError::VersionMismatch { expected, found })) => {
            assert_eq!(expected, KbVersion::new(7));
            assert_eq!(found, KbVersion::new(3));
        }
        other => panic!("expected a version mismatch, got {other:?}"),
    }

    // Naming another KB: refused as a KB mismatch.
    match Snapshot::open(
        &mut store,
        Expectation {
            kb: Some(KbUuid::from_bytes([0x77; 16])),
            version: None,
        },
        &mut ledger,
    ) {
        Err(SnapshotBuildError::View(SnapshotOpenError::KbMismatch { .. })) => {}
        other => panic!("expected a KB mismatch, got {other:?}"),
    }

    // A healthy expectation still builds: the refusals above are about *which*
    // publication, not about refusing everything.
    Snapshot::open(
        &mut store,
        Expectation::at(kb(), KbVersion::new(3)),
        &mut ledger,
    )
    .expect("the named publication builds");
}

#[test]
fn the_graph_retains_its_admission_and_returns_it_when_dropped() {
    let (_inner, _fs, mut store) = world();
    // One ledger for the whole test, so the same counter is observed before, during and
    // after the view's life. A view whose lease died with the construction attempt would
    // show `held_bytes` fall to zero while the graph still existed.
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let baseline = ledger.stats().held_bytes;
    assert_eq!(baseline, 0, "nothing is held before the view exists");

    let snapshot =
        Snapshot::open(&mut store, Expectation::latest(), &mut ledger).expect("snapshot");
    let retained = snapshot.retained_bytes();
    assert!(
        retained > 0,
        "the graph must own a real reservation, not a zero placeholder"
    );

    let during = ledger.stats();
    assert!(
        during.held_bytes >= retained,
        "the ledger must show the graph's reservation while the view lives: \
         held={} retained={}",
        during.held_bytes,
        retained
    );
    // The graph is genuinely populated, so "holds bytes" is not an empty structure.
    assert!(snapshot.graph().term_count() > 0);

    // The view's retained credit is TRANSFERRED credit (`held_bytes`), and the scan charges
    // remain untransferred attempt credit — so `live_bytes` is deliberately not the figure that
    // must fall. Only the lease belongs to the view, and dropping the view releases it.
    let held_before_drop = during.held_bytes;
    assert!(
        held_before_drop >= retained,
        "the held figure must cover the view's whole extent"
    );
    drop(snapshot);
    let after = ledger.stats();
    assert_eq!(
        after.held_bytes, 0,
        "every held byte must come back when the view is dropped"
    );
    assert!(
        retained > after.held_bytes,
        "the view must have held a real, released extent: retained={retained} after={}",
        after.held_bytes
    );
}

#[test]
fn a_dangling_active_assertion_is_reported_rather_than_dropped() {
    // Seed an assertion whose target term is retired: the honest answer is a semantic
    // fault, because the graph would otherwise omit a live authored edge.
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = SharedFs::from_rc(Rc::clone(&inner));
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let mut store = bootstrap(
        fs,
        std::path::Path::new(ROOT),
        sage_db_store::fs::CreateDir::IfMissing,
        kb(),
        RULES,
        &Limits::V1,
        &mut ledger,
    )
    .expect("bootstrap");

    let mut events = vec![fixture::term(1, 1)];
    let dangling = AssertionRecordFull {
        key: RecordKey::new(RecordKind::Assertion, 12),
        revision: RecordRevision::FIRST,
        retracted: false,
        from: RecordKey::term(1),
        relation: RelationId::IsA,
        target: RecordKey::term(2),
        confidence: sage_db_types::confidence::Confidence::Hypothetical,
        declaration_order: 1,
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        legacy_id: None,
        legacy_content_digest: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    events.push(assertion_event(AssertionEvent::Created, &dangling));
    let affected = vec![
        RevPair {
            key: RecordKey::term(1),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: dangling.key,
            revision: RecordRevision::FIRST,
        },
    ];
    seed(
        &mut store,
        &mut ledger,
        2,
        events,
        affected,
        vec![fixture::body(1, 1)],
    );

    match Snapshot::open(&mut store, Expectation::latest(), &mut ledger) {
        Err(SnapshotBuildError::View(SnapshotOpenError::Graph(GraphBuildRefusal::Invalid(
            error,
        )))) => {
            // The graph's own verdict, naming the dangling endpoint.
            assert!(
                matches!(
                    error,
                    sage_db_types::graph::GraphBuildError::UnknownTerm { .. }
                ),
                "expected UnknownTerm, got {error:?}"
            );
        }
        Ok(_) => panic!("an active assertion to an absent term must not produce a view"),
        Err(other) => panic!("expected a graph refusal, got {other:?}"),
    }
}

#[test]
fn a_reopened_store_serves_the_same_records_and_basis() {
    let (_inner, fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let committed_basis = {
        let snapshot =
            Snapshot::open(&mut store, Expectation::latest(), &mut ledger).expect("snapshot");
        snapshot.basis()
    };
    let counts_before = {
        let snapshot =
            Snapshot::open(&mut store, Expectation::latest(), &mut ledger).expect("snapshot");
        snapshot.counts()
    };
    drop(store);

    let opened = open_fresh(fs, &mut ledger);
    assert_eq!(opened.durability(), ViewDurability::Durable);
    let mut reopened = opened.into_store();
    let snapshot = Snapshot::open(
        &mut reopened,
        Expectation::at(kb(), KbVersion::new(3)),
        &mut ledger,
    )
    .expect("reopened snapshot");
    assert_eq!(snapshot.basis(), committed_basis, "one basis across reopen");
    let counts_after = snapshot.counts();
    assert_eq!(counts_before, counts_after, "the same records, by family");

    let mut read = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    match snapshot.record(RecordKey::new(RecordKind::Assertion, 11), &mut read) {
        Ok(RecordView::Found(RecordRef::Assertion(assertion))) => {
            assert_eq!(assertion.key, RecordKey::new(RecordKind::Assertion, 11));
            // The current after-image is the retracted revision: the snapshot serves the
            // current state, and the caller decides what an inactive record means.
            assert!(assertion.retracted);
            assert_eq!(
                assertion.revision,
                RecordRevision::new(2).expect("revision 2")
            );
        }
        other => panic!("the assertion must survive reopen, got {other:?}"),
    }
}

#[test]
fn point_reads_charge_the_ledger_before_looking() {
    let (_inner, _fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let snapshot =
        Snapshot::open(&mut store, Expectation::latest(), &mut ledger).expect("snapshot");

    // Zero entry allowance: the first lookup refuses rather than scanning for free.
    let mut no_entries = NoEntries;
    match snapshot.record(RecordKey::term(1), &mut no_entries) {
        Err(ReadRefusal::Refused(Refusal::Budget(error))) => {
            assert_eq!(error.unit(), BudgetUnit::Entries);
        }
        other => panic!("a zero-entry ledger must refuse the lookup, got {other:?}"),
    }

    // A stop is distinguishable from a budget refusal at the same call.
    let mut stopping = Stopping::new(0);
    match snapshot.record(RecordKey::term(1), &mut stopping) {
        Err(ReadRefusal::Refused(Refusal::Stopped(ControlStop::Cancelled))) => {}
        other => panic!("a stopped caller must see a stop, got {other:?}"),
    }
}

#[test]
fn a_family_outside_scope_is_unsupported_not_absent_and_bad_keys_are_invalid() {
    let (_inner, _fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let snapshot =
        Snapshot::open(&mut store, Expectation::latest(), &mut ledger).expect("snapshot");
    let mut read = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");

    // Families outside the four in scope answer `UnsupportedFamily`, never `Absent`: the
    // view does not interpret them, so it cannot say a record of that kind is gone.
    for kind in [
        RecordKind::Evidence,
        RecordKind::EvidenceLink,
        RecordKind::Vector,
        RecordKind::Proposal,
        RecordKind::FrontierEntry,
    ] {
        match snapshot.record(RecordKey::new(kind, 1), &mut read) {
            Err(ReadRefusal::UnsupportedFamily(observed)) => assert_eq!(observed, kind),
            other => panic!("{kind:?} must be Unsupported, got {other:?}"),
        }
    }

    // A key whose id exceeds its kind's bound is malformed: a distinct caller refusal, not
    // absence and not an unsupported family.
    let out_of_range = RecordKey::new(RecordKind::Term, u32::MAX as u128 + 1);
    assert!(!out_of_range.id_is_in_range());
    match snapshot.record(out_of_range, &mut read) {
        Err(ReadRefusal::InvalidKey(key)) => assert_eq!(key, out_of_range),
        other => panic!("an out-of-range key must be InvalidKey, got {other:?}"),
    }
}

#[test]
fn a_mid_build_stop_is_observed_inside_the_indexing_loops() {
    use sage_db_types::snapshot::SnapshotView as _;

    // Build a view on a healthy ledger, then drive a *stopping* ledger through accessors that
    // iterate: the per-record control must fire during the walk, not only before it.
    let (_inner, _fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let snapshot =
        Snapshot::open(&mut store, Expectation::latest(), &mut ledger).expect("snapshot");

    let mut stopping = Stopping::new(2);
    let mut visited = 0usize;
    let result = snapshot.scan_family::<()>(SnapshotFamily::Term, &mut stopping, &mut |_| {
        visited += 1;
        Ok(())
    });
    assert!(
        matches!(result, Err(ScanError::Refused(_))),
        "the walk must stop mid-way, got {result:?}"
    );
    assert!(
        visited < snapshot.counts().terms,
        "the stop must arrive before every record is visited: visited={visited} of {}",
        snapshot.counts().terms
    );
}

/// The import plan bytes the session's `plan_ref` addresses.
const PLAN_BYTES: &[u8] = b"p3 import plan placeholder";

#[test]
fn an_unfinished_import_session_blocks_the_view() {
    use sage_db_types::persist::{CountPair, CreationAudit, ImportSessionRecord, ImportStage};

    let (_inner, _fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");

    // Persist a Prepare-stage session that has NOT set its completion marker.
    let session = ImportSessionRecord {
        import_session_id: "p3-import-session".to_owned(),
        stage: ImportStage::Prepare,
        source_digest: [0x31u8; 32],
        // A content address: its bytes must be declared by the same transaction.
        plan_ref: raw_sha256(PLAN_BYTES),
        expected_counts: vec![CountPair {
            kind: sage_db_types::persist::StateKind::Term,
            count: 1,
        }],
        import_complete: false,
        audit: CreationAudit {
            recorded_by: Some(PRINCIPAL.to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            // The import event's required field set includes a reason.
            reason: Some("p3 import session".to_owned()),
            created_at: 1_700_000_000_000_000,
        },
        extensions: Extensions::new(),
    };
    let event = match sage_db_store::typed::prepare_event(
        sage_db_store::typed::EventInput::ImportSession(&session),
        &mut ledger,
    ) {
        sage_db_types::framing::FormatOutcome::Ok(event) => event,
        other => panic!("import session event: {other:?}"),
    };
    seed(
        &mut store,
        &mut ledger,
        4,
        vec![event],
        Vec::new(),
        vec![PLAN_BYTES.to_vec()],
    );

    // The view must refuse: a stage without its verified completion marker is not a serving
    // basis, even though every record read above would succeed.
    match Snapshot::open(&mut store, Expectation::latest(), &mut ledger) {
        Err(SnapshotBuildError::View(SnapshotOpenError::ImportIncomplete)) => {}
        other => panic!("an unfinished import must block the view, got {other:?}"),
    }
}

#[test]
fn the_view_copies_no_record_payload_and_its_extent_covers_the_slot_tables() {
    let (_inner, _fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let snapshot =
        Snapshot::open(&mut store, Expectation::latest(), &mut ledger).expect("snapshot");

    // The extent is a real reservation, and it must be larger than the graph vectors alone:
    // the four borrowed-slot tables are part of what the view keeps alive.
    let retained = snapshot.retained_bytes();
    let counts = snapshot.counts();
    let slots = (counts.terms + counts.assertions + counts.claims + counts.checkpoints) as u64;
    assert!(slots > 0, "the fixture holds records");
    assert!(
        retained > 0,
        "the view must reserve a non-zero extent, not a placeholder"
    );

    // A record borrowed from the view is the SAME allocation the store retained: comparing
    // the address proves no deep copy was made for the payload.
    let mut read = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let borrowed = match snapshot.record(RecordKey::term(1), &mut read) {
        Ok(RecordView::Found(RecordRef::Term(term))) => term as *const TermRecord,
        other => panic!("expected the term, got {other:?}"),
    };
    // The store's own current record for the same key, reached through its public API.
    let mut store_read = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    let original = match snapshot
        .store()
        .record(RecordKey::term(1), &mut store_read)
        .expect("store record")
    {
        sage_db_store::view::RecordLookup::Found {
            record: sage_db_store::fold::FoldedRecord::Term(term),
            ..
        } => term as *const TermRecord,
        other => panic!("expected the retained term, got {other:?}"),
    };
    assert_eq!(
        borrowed, original,
        "the view must borrow the retained record, not copy it"
    );
}

#[test]
fn committed_rules_are_read_and_verified_from_their_real_blob() {
    use sage_db_format::digest::raw_sha256;

    let (_inner, fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).expect("ledger");
    seed(
        &mut store,
        &mut ledger,
        4,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    drop(store);

    // This is the healthy rules-blob control: the profile is decoded from actual bytes.
    // It makes no missing/corrupt-blob claim.
    let mut reopened = open_fresh(fs, &mut ledger).into_store();
    let snapshot = Snapshot::open(&mut reopened, Expectation::latest(), &mut ledger)
        .expect("a valid committed rules blob must verify");
    assert_eq!(
        snapshot.rules().rules_ref,
        sage_db_types::digest::Digest::from_bytes(raw_sha256(RULES)),
        "the view records the committed blob address, verified from the bytes"
    );
    assert_eq!(
        snapshot.committed_profile(),
        RuleProfile::FormalV1,
        "the profile comes from the verified bytes"
    );
}

#[test]
fn real_rules_read_failure_keeps_the_filesystem_operation_kind_and_effect() {
    use sage_db_store::fs::{Effect, FsErrorKind};
    use sage_db_store::view::{BlobReadError, BlobReadStep};
    use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
    let (inner, _fs, mut store) = world();
    *inner.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::ReadExact, Injection::FailUnknown, 1);
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).unwrap();
    match Snapshot::open(&mut store, Expectation::latest(), &mut ledger) {
        Err(SnapshotBuildError::Blob(BlobReadError::Filesystem { step, error })) => {
            assert_eq!(step, BlobReadStep::Read);
            assert_eq!(error.kind(), FsErrorKind::Io(0));
            assert_eq!(error.effect(), Effect::Unknown);
        }
        other => panic!("must preserve the actual rules read error: {other:?}"),
    }
    inner.assert_all_faults_fired();
    assert_eq!(
        inner.faults_handle().borrow().fired(),
        &[(FaultPoint::ReadExact, Injection::FailUnknown)]
    );
    // Transport failure did not invent a corrupt verdict. A fresh adequate attempt can retry.
    Snapshot::open(&mut store, Expectation::latest(), &mut ledger).unwrap();
}

#[test]
fn snapshot_keeps_its_whole_claim_after_the_construction_facade_drops() {
    use sage_db_store::lease::LeaseLedger;
    let (_inner, _fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).unwrap();
    let observer = ledger.reserve_owned(0).unwrap();
    let snapshot = Snapshot::open(&mut store, Expectation::latest(), &mut ledger).unwrap();
    let retained = snapshot.retained_bytes();
    assert_eq!(observer.stats().held_bytes, retained);
    drop(ledger);
    assert_eq!(observer.stats().live_bytes, retained);
    assert_eq!(observer.stats().held_bytes, retained);
    let mut read = AttemptLedger::open(BYTES, ENTRIES, control()).unwrap();
    assert!(matches!(
        snapshot.record(RecordKey::term(1), &mut read),
        Ok(RecordView::Found(_))
    ));
    drop(snapshot);
    assert_eq!(observer.stats().live_bytes, 0);
}

#[test]
fn snapshot_graph_digest_is_the_canonical_active_projection() {
    use sage_db_types::identity::TermId;
    use sage_db_types::record::TermMeta;
    let (_inner, _fs, mut store) = world();
    let mut ledger = AttemptLedger::open(BYTES, ENTRIES, control()).unwrap();
    let snapshot = Snapshot::open(&mut store, Expectation::latest(), &mut ledger).unwrap();
    // Authored expectation: term 3 retired and assertion 11 retracted in the fixture.
    let expected = [
        TermMeta {
            id: TermId::new(1),
            layer: term_record(1).layer,
        },
        TermMeta {
            id: TermId::new(2),
            layer: term_record(2).layer,
        },
    ];
    let actual = sage_db_format::graph::graph_digest(&expected, &[], &mut ledger).unwrap();
    assert_eq!(
        *snapshot.basis().graph_digest.as_digest().as_bytes(),
        actual
    );
    assert_eq!(snapshot.graph().terms(), &expected);
    assert_eq!(snapshot.graph().assertion_count(), 0);
}
