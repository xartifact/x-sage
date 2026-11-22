//! P2 auxiliary frontier-entry lifecycle on a real public Store and the model backend.
//!
//! Every command here goes through the production entry points — `bootstrap`,
//! `Store::commit`, `open_existing`, `create_frontier`, `revise_frontier`, `read_frontier` —
//! with the real ledger, the real canonical codecs and a real fault injector. The seed terms
//! are produced by the canonical `sage_db_format::term` writer and read back through the
//! canonical envelope reader, so a seeded record is byte-identical to what a writer would
//! persist.
//!
//! What this file does **not** claim: platform durability, a query/enumeration API, or that
//! anything here is allocator evidence. Test-local allocations are test-local.

#![allow(dead_code)]

#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::path::Path;
use std::rc::Rc;

use fixture::{BYTES, ENTRIES, ROOT, RULES, kb};
use observing_fs::ObservingFs;

use sage_db_format::event::read_event;
use sage_db_format::term::{TermEvent, write_term};
use sage_db_format::wire::Reader;
use sage_db_kernel::auth::{Capability, CapabilitySet, Grants};
use sage_db_kernel::claims::{CommandError, InputError, RequestMeta};
use sage_db_kernel::frontier::{
    FrontierError, FrontierInputError, FrontierRead, create_frontier, read_frontier,
    revise_frontier,
};
use sage_db_kernel::frontier_intent::{
    CREATE_FRONTIER, FrontierPayloadError, FrontierState, OPERATION_VERSION, REVISE_FRONTIER,
};
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::CreateDir;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::open::Opened;
use sage_db_store::typed::{self, EventInput};
use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::confidence::Confidence;
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};
use sage_db_types::identity::RecordRevision;
use sage_db_types::layer::SemanticLayer;
use sage_db_types::persist::{
    ActorOrigin, CommonFields, CreationAudit, Extensions, FrontierStatus, IdAllocationEvent,
    IdSpace, RecordKey, RecordKind, RequestOutcomeEvent, RevPair, TermRecord, TermSource,
    Watermark,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};
use sage_db_types::query::RunToCompletion;

const WRITER: &str = "real-frontier-writer";
const READER: &str = "real-frontier-reader";
const CONCEPT_A: u32 = 1;
const CONCEPT_B: u32 = 2;
const RETIRED_CONCEPT: u32 = 3;

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap()
}

fn concept_a() -> RecordKey {
    RecordKey::term(CONCEPT_A)
}

fn concept_b() -> RecordKey {
    RecordKey::term(CONCEPT_B)
}

fn retired_key() -> RecordKey {
    RecordKey::term(RETIRED_CONCEPT)
}

fn frontier_key(id: u128) -> RecordKey {
    RecordKey::new(RecordKind::FrontierEntry, id)
}

fn writer() -> Grants<'static> {
    Grants::new(
        WRITER,
        kb(),
        CapabilitySet::of(&[Capability::WriteAux, Capability::Read]),
    )
}

fn write_only() -> Grants<'static> {
    Grants::new(WRITER, kb(), CapabilitySet::of(&[Capability::WriteAux]))
}

fn read_only() -> Grants<'static> {
    Grants::new(READER, kb(), CapabilitySet::of(&[Capability::Read]))
}

fn no_capabilities() -> Grants<'static> {
    Grants::new(READER, kb(), CapabilitySet::of(&[]))
}

fn meta_at(request_id: &str, created_at: i64) -> RequestMeta<'_> {
    let digest = sage_db_format::digest::raw_sha256(request_id.as_bytes());
    let mut tx_id = [0u8; 16];
    tx_id.copy_from_slice(&digest[..16]);
    RequestMeta {
        request_id,
        tx_id,
        created_at,
    }
}

fn meta(request_id: &str) -> RequestMeta<'_> {
    meta_at(request_id, 1_700_000_100_000_000)
}

fn state<'a>(
    refs: &'a [RecordKey],
    text: &'a str,
    priority: u16,
    status: u16,
) -> FrontierState<'a> {
    FrontierState {
        concept_refs: refs,
        text,
        priority,
        status,
    }
}

fn prepared(outcome: FormatOutcome<OwnedEvent>) -> OwnedEvent {
    match outcome {
        FormatOutcome::Ok(event) => event,
        other => panic!("canonical preparation refused: {other:?}"),
    }
}

fn term_record(id: u32, retired: bool) -> TermRecord {
    TermRecord {
        key: RecordKey::term(id),
        revision: if retired {
            RecordRevision::new(2).unwrap()
        } else {
            RecordRevision::FIRST
        },
        retired,
        slug: format!("frontier-concept-{id}"),
        atom: format!("concept {id} for the frontier family"),
        layer: SemanticLayer::L1,
        domains: vec!["frontier".into()],
        source: TermSource::Personal,
        confidence: Confidence::Confirmed,
        triggers: vec!["frontier".into()],
        body_ref: sage_db_format::digest::raw_sha256(&term_body(id, retired)),
        common: CommonFields {
            recorded_by: Some(WRITER.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: if retired {
                Some("retired for the frontier control".into())
            } else {
                None
            },
            created_at: 1_700_000_000_000_000,
            updated_at: if retired {
                Some(1_700_000_000_000_500)
            } else {
                None
            },
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

fn term_body(id: u32, retired: bool) -> Vec<u8> {
    format!("frontier-term-body/{id}/retired={retired}").into_bytes()
}

fn term_event(
    id: u32,
    retired: bool,
    ledger: &mut dyn sage_db_types::ledger::SharedLedger,
) -> OwnedEvent {
    let record = term_record(id, retired);
    let event = if retired {
        TermEvent::Retired
    } else {
        TermEvent::Created
    };
    let bytes =
        sage_db_format::event::encode_with(ledger, |writer| write_term(writer, event, &record))
            .unwrap_or_else(|error| panic!("seed term encodes: {error:?}"));
    let mut reader = Reader::new(&bytes);
    let decoded = read_event(&mut reader, ledger)
        .unwrap_or_else(|error| panic!("seed term decodes: {error:?}"));
    let mut fields: Vec<(u16, Vec<u8>)> = Vec::new();
    for field in decoded.fields() {
        fields.push((field.tag(), field.value().to_vec()));
    }
    OwnedEvent {
        tag: decoded.tag(),
        fields,
    }
}

fn outcome_event(
    version: u64,
    request_id: &str,
    digest: [u8; 32],
    result_ref: [u8; 32],
) -> RequestOutcomeEvent {
    RequestOutcomeEvent {
        principal_id: WRITER.into(),
        request_id: request_id.into(),
        payload_digest: digest,
        result_ref,
        commit_version: version,
        audit: CreationAudit {
            recorded_by: Some(WRITER.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000 + version as i64,
        },
        extensions: Extensions::new(),
    }
}

fn blob(bytes: Vec<u8>) -> BlobDeclaration {
    BlobDeclaration {
        sha256: sage_db_format::digest::raw_sha256(&bytes),
        declared_len: bytes.len() as u64,
        source: BlobSource::Provide(bytes),
    }
}

struct Seed {
    version: u64,
    events: Vec<OwnedEvent>,
    blobs: Vec<BlobDeclaration>,
    affected: Vec<RevPair>,
}

fn commit_seed(
    store: &mut Store<ObservingFs>,
    seed: Seed,
    request_id: &str,
    ledger: &mut AttemptLedger<'_>,
) {
    let basis = store
        .commit_basis(IdSpace::FrontierEntryId, ledger)
        .unwrap();
    let result_bytes = typed::result_payload(seed.version, &seed.affected, &[], ledger).unwrap();
    let result_ref = sage_db_format::digest::raw_sha256(&result_bytes);
    let mut events = seed.events;
    events.push(prepared(typed::prepare_event(
        EventInput::Outcome(&outcome_event(
            seed.version,
            request_id,
            [seed.version as u8; 32],
            result_ref,
        )),
        ledger,
    )));
    let mut blobs = seed.blobs;
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
        version: seed.version,
        prev_version: Some(seed.version - 1),
        // Derive the transaction identity from the REQUEST, not the version alone: several
        // fixtures legitimately commit at the same version in different KBs, and a version-only
        // id makes their transactions indistinguishable in a failure report.
        tx_id: {
            let digest = sage_db_format::digest::raw_sha256(request_id.as_bytes());
            let mut id = [0u8; 16];
            id.copy_from_slice(&digest[..16]);
            id
        },
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000 + seed.version as i64,
        principal_id: WRITER.into(),
        request_id: request_id.into(),
        payload_digest: [seed.version as u8; 32],
        expect_version: Some(seed.version - 1),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, ledger) {
        HandleOutcome::Committed { version, .. } => assert_eq!(version, seed.version),
        other => panic!("seed commit at {} failed: {other:?}", seed.version),
    }
}

/// Bootstrap a KB holding two active concepts and one retired concept.
fn seed_world() -> (Rc<ModelFileSystem>, ObservingFs, Store<ObservingFs>) {
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
    .unwrap();

    commit_seed(
        &mut store,
        Seed {
            version: 2,
            events: vec![
                term_event(CONCEPT_A, false, &mut l),
                term_event(CONCEPT_B, false, &mut l),
                term_event(RETIRED_CONCEPT, false, &mut l),
            ],
            blobs: vec![
                blob(term_body(CONCEPT_A, false)),
                blob(term_body(CONCEPT_B, false)),
                blob(term_body(RETIRED_CONCEPT, false)),
            ],
            affected: vec![
                RevPair {
                    key: concept_a(),
                    revision: RecordRevision::FIRST,
                },
                RevPair {
                    key: concept_b(),
                    revision: RecordRevision::FIRST,
                },
                RevPair {
                    key: retired_key(),
                    revision: RecordRevision::FIRST,
                },
            ],
        },
        "seed-terms",
        &mut l,
    );
    commit_seed(
        &mut store,
        Seed {
            version: 3,
            events: vec![term_event(RETIRED_CONCEPT, true, &mut l)],
            blobs: vec![blob(term_body(RETIRED_CONCEPT, true))],
            affected: vec![RevPair {
                key: retired_key(),
                revision: RecordRevision::new(2).unwrap(),
            }],
        },
        "seed-retire",
        &mut l,
    );
    (inner, fs, store)
}

fn baseline_a() -> [RevPair; 1] {
    [RevPair {
        key: concept_a(),
        revision: RecordRevision::FIRST,
    }]
}

fn baseline_b() -> [RevPair; 1] {
    [RevPair {
        key: concept_b(),
        revision: RecordRevision::FIRST,
    }]
}

fn baseline_ab() -> [RevPair; 2] {
    [
        RevPair {
            key: concept_a(),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: concept_b(),
            revision: RecordRevision::FIRST,
        },
    ]
}

fn baseline_empty() -> [RevPair; 0] {
    []
}

#[allow(clippy::too_many_arguments)]
fn create(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    refs: &[RecordKey],
    text: &str,
    priority: u16,
    status: u16,
    read_set: &[RevPair],
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, FrontierError> {
    create_frontier(
        store,
        &writer(),
        meta(request_id),
        state(refs, text, priority, status),
        read_set,
        &Limits::V1,
        ledger,
    )
}

fn create_a(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    text: &str,
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, FrontierError> {
    let refs = [concept_a()];
    create(store, request_id, &refs, text, 0, 0, &baseline_a(), ledger)
}

#[allow(clippy::too_many_arguments)]
fn revise(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    key: RecordKey,
    refs: &[RecordKey],
    text: &str,
    priority: u16,
    status: u16,
    read_set: &[RevPair],
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, FrontierError> {
    revise_frontier(
        store,
        &writer(),
        meta_at(request_id, 1_700_000_200_000_000),
        key,
        state(refs, text, priority, status),
        read_set,
        &Limits::V1,
        ledger,
    )
}

fn read(
    store: &mut Store<ObservingFs>,
    key: RecordKey,
    max: u64,
    ledger: &mut AttemptLedger<'_>,
) -> Result<FrontierRead, FrontierError> {
    read_frontier(store, &read_only(), key, max, ledger)
}

fn record_of(read: &FrontierRead) -> &sage_db_types::persist::FrontierEntryRecord {
    match read {
        FrontierRead::Found { record, .. } => match record.record() {
            FoldedRecord::FrontierEntry(entry) => entry,
            other => panic!("expected a frontier entry, got {other:?}"),
        },
        FrontierRead::Absent { .. } => panic!("expected a frontier entry, found absence"),
    }
}

fn text_of(read: &FrontierRead) -> &[u8] {
    match read {
        FrontierRead::Found { text, .. } => text.as_bytes(),
        FrontierRead::Absent { .. } => panic!("expected text, found absence"),
    }
}

fn committed_version(outcome: HandleOutcome) -> u64 {
    match outcome {
        HandleOutcome::Committed { version, .. } => version,
        other => panic!("expected Committed, got {other:?}"),
    }
}

fn input_error<T: core::fmt::Debug>(result: Result<T, FrontierError>) -> FrontierInputError {
    match result {
        Err(FrontierError::Input(error)) => error,
        other => panic!("expected a frontier input refusal, got {other:?}"),
    }
}

fn command_error<T: core::fmt::Debug>(result: Result<T, FrontierError>) -> CommandError {
    match result {
        Err(FrontierError::Command(error)) => error,
        other => panic!("expected a command refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

#[test]
fn create_persists_the_complete_state_and_survives_reopen() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];
    let version = committed_version(
        create(
            &mut store,
            "create-1",
            &refs,
            "unexplored territory",
            1,
            0,
            &baseline_a(),
            &mut l,
        )
        .unwrap(),
    );
    let key = frontier_key(1);

    let read_back = read(&mut store, key, 1 << 20, &mut l).unwrap();
    let record = record_of(&read_back);
    assert_eq!(record.key, key);
    assert_eq!(record.revision, RecordRevision::FIRST);
    assert_eq!(record.status, FrontierStatus::Open);
    assert_eq!(record.priority, 1);
    assert_eq!(record.concept_refs, vec![concept_a()]);
    assert_eq!(text_of(&read_back), b"unexplored territory");
    assert_eq!(record.common.recorded_by.as_deref(), Some(WRITER));
    assert_eq!(record.common.actor_origin, ActorOrigin::Authenticated);
    assert_eq!(record.common.created_at, meta("create-1").created_at);

    // Drop and reopen: the same bytes come back on a fresh handle.
    drop(store);
    let fs = ObservingFs::new(Rc::clone(&_inner));
    let mut reopened =
        sage_db_store::open::open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut l)
            .unwrap()
            .into_store();
    assert_eq!(reopened.head(), Some(version));
    let again = read(&mut reopened, key, 1 << 20, &mut l).unwrap();
    assert_eq!(record_of(&again), record);
    assert_eq!(text_of(&again), b"unexplored territory");
}

#[test]
fn a_subjectless_entry_is_valid() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    committed_version(
        create(
            &mut store,
            "create-empty",
            &[],
            "",
            2,
            1,
            &baseline_empty(),
            &mut l,
        )
        .unwrap(),
    );
    let read_back = read(&mut store, frontier_key(1), 1 << 20, &mut l).unwrap();
    let record = record_of(&read_back);
    assert!(record.concept_refs.is_empty());
    assert_eq!(record.status, FrontierStatus::Resolved);
    assert_eq!(record.priority, 2);
    // An empty text is a genuine zero-length blob, not a missing one.
    assert!(text_of(&read_back).is_empty());
}

#[test]
fn text_bytes_are_preserved_exactly_including_unicode_and_nul() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let text = "frontier \u{4e2d}\u{6587} \u{1f3af} nul:\u{0} quote:\" slash:\\ end";
    let refs = [concept_a()];
    create(
        &mut store,
        "create-bytes",
        &refs,
        text,
        0,
        0,
        &baseline_a(),
        &mut l,
    )
    .unwrap();
    let read_back = read(&mut store, frontier_key(1), 1 << 20, &mut l).unwrap();
    assert_eq!(text_of(&read_back), text.as_bytes());
}

#[test]
fn a_missing_or_retired_concept_is_refused() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    let missing = [RecordKey::term(99)];
    assert!(matches!(
        input_error(create(
            &mut store,
            "create-missing",
            &missing,
            "t",
            0,
            0,
            &[RevPair {
                key: missing[0],
                revision: RecordRevision::FIRST
            }],
            &mut l
        )),
        FrontierInputError::MissingConcept(k) if k == missing[0]
    ));

    let retired = [retired_key()];
    assert!(matches!(
        input_error(create(
            &mut store,
            "create-retired",
            &retired,
            "t",
            0,
            0,
            &[RevPair {
                key: retired_key(),
                revision: RecordRevision::new(2).unwrap()
            }],
            &mut l
        )),
        FrontierInputError::ConceptRetired(k) if k == retired_key()
    ));
}

#[test]
fn a_stale_baseline_is_refused_without_writing() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];
    // Claiming revision 2 for a term still at 1 is a stale pin.
    let stale = [RevPair {
        key: concept_a(),
        revision: RecordRevision::new(2).unwrap(),
    }];
    assert!(matches!(
        command_error(create(
            &mut store,
            "create-stale",
            &refs,
            "t",
            0,
            0,
            &stale,
            &mut l
        )),
        CommandError::Input(InputError::StaleBaseline { .. })
    ));
    assert!(
        store.head() == Some(3),
        "a refusal must not advance the head"
    );
}

#[test]
fn the_read_set_must_be_exactly_the_listed_refs() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];
    // Missing the required term.
    assert!(matches!(
        command_error(create(
            &mut store,
            "create-noread",
            &refs,
            "t",
            0,
            0,
            &baseline_empty(),
            &mut l
        )),
        CommandError::Input(InputError::ReadSetLength { .. })
    ));
    // A superset is equally wrong: length is checked before membership, so a two-row read
    // set for a one-ref entry is refused as a length disagreement.
    assert!(matches!(
        command_error(create(
            &mut store,
            "create-super",
            &refs,
            "t",
            0,
            0,
            &[
                RevPair {
                    key: concept_b(),
                    revision: RecordRevision::FIRST
                },
                RevPair {
                    key: concept_a(),
                    revision: RecordRevision::FIRST
                },
            ],
            &mut l
        )),
        CommandError::Input(InputError::ReadSetLength { .. })
    ));

    // Same length, wrong key: this is the membership refusal.
    assert!(matches!(
        command_error(create(
            &mut store,
            "create-wrongkey",
            &refs,
            "t",
            0,
            0,
            &[RevPair {
                key: concept_b(),
                revision: RecordRevision::FIRST
            }],
            &mut l
        )),
        CommandError::Input(InputError::ReadSetKey { .. })
    ));
}

#[test]
fn create_requires_write_aux_and_read_requires_read() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];

    // Read-only grants cannot write.
    assert!(matches!(
        create_frontier(
            &mut store,
            &read_only(),
            meta("create-unauth"),
            state(&refs, "t", 0, 0),
            &baseline_a(),
            &Limits::V1,
            &mut l
        ),
        Err(FrontierError::Command(CommandError::Auth(_)))
    ));

    // write_aux does not imply read: a write-only grant cannot read.
    committed_version(
        create(
            &mut store,
            "create-for-read",
            &refs,
            "t",
            0,
            0,
            &baseline_a(),
            &mut l,
        )
        .unwrap(),
    );
    assert!(matches!(
        read_frontier(&mut store, &write_only(), frontier_key(1), 1 << 20, &mut l),
        Err(FrontierError::Command(CommandError::Auth(_)))
    ));
    // Nor can a grant with no capabilities do anything.
    assert!(matches!(
        read_frontier(
            &mut store,
            &no_capabilities(),
            frontier_key(1),
            1 << 20,
            &mut l
        ),
        Err(FrontierError::Command(CommandError::Auth(_)))
    ));
}

#[test]
fn a_wrong_kb_grant_is_refused() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let other_kb = sage_db_types::identity::KbUuid::from_bytes([0x11; 16]);
    let foreign = Grants::new(
        WRITER,
        other_kb,
        CapabilitySet::of(&[Capability::WriteAux, Capability::Read]),
    );
    let refs = [concept_a()];
    assert!(matches!(
        create_frontier(
            &mut store,
            &foreign,
            meta("create-foreign-kb"),
            state(&refs, "t", 0, 0),
            &baseline_a(),
            &Limits::V1,
            &mut l
        ),
        Err(FrontierError::Command(CommandError::Auth(_)))
    ));
}

#[test]
fn a_non_term_reference_is_refused_before_any_read() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let wrong = [frontier_key(1)];
    assert!(matches!(
        command_error(create(
            &mut store,
            "create-wrongkind",
            &wrong,
            "t",
            0,
            0,
            &[RevPair {
                key: wrong[0],
                revision: RecordRevision::FIRST
            }],
            &mut l
        )),
        CommandError::Input(InputError::WrongKeyKind { .. })
    ));
}

// ---------------------------------------------------------------------------
// Revise
// ---------------------------------------------------------------------------

#[test]
fn revise_replaces_state_and_preserves_creation_audit() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let refs_a = [concept_a()];
    create(
        &mut store,
        "create-1",
        &refs_a,
        "first text",
        1,
        0,
        &baseline_a(),
        &mut l,
    )
    .unwrap();
    let key = frontier_key(1);
    let first = read(&mut store, key, 1 << 20, &mut l).unwrap();
    let first_record = record_of(&first).clone();
    let first_text_ref = first_record.text_ref;

    // Resolve it, moving the reference set to B, and raise the priority.
    let refs_b = [concept_b()];
    revise(
        &mut store,
        "revise-1",
        key,
        &refs_b,
        "revised text",
        0,
        1,
        &[
            RevPair {
                key: concept_b(),
                revision: RecordRevision::FIRST,
            },
            RevPair {
                key,
                revision: RecordRevision::FIRST,
            },
        ],
        &mut l,
    )
    .unwrap();

    let second = read(&mut store, key, 1 << 20, &mut l).unwrap();
    let record = record_of(&second);
    assert_eq!(record.revision, RecordRevision::new(2).unwrap());
    assert_eq!(record.status, FrontierStatus::Resolved);
    assert_eq!(record.priority, 0);
    assert_eq!(record.concept_refs, vec![concept_b()]);
    assert_eq!(text_of(&second), b"revised text");
    // Creation audit and extensions are preserved verbatim.
    assert_eq!(record.common.created_at, first_record.common.created_at);
    assert_eq!(record.extensions, first_record.extensions);
    assert_eq!(record.changed_fields, first_record.changed_fields);
    // Only the update time and actor move.
    assert_eq!(record.common.updated_at, Some(1_700_000_200_000_000));
    assert_ne!(record.text_ref, first_text_ref);

    // The old text blob is still readable through its own committed digest.
    let old = store
        .read_committed_blob(first_text_ref, 1 << 20, &mut l)
        .unwrap();
    assert_eq!(old.as_bytes(), b"first text");
}

#[test]
fn reopening_a_resolved_entry_is_allowed() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];
    create(
        &mut store,
        "create-1",
        &refs,
        "t",
        1,
        0,
        &baseline_a(),
        &mut l,
    )
    .unwrap();
    let key = frontier_key(1);
    let base = [
        RevPair {
            key: concept_a(),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key,
            revision: RecordRevision::FIRST,
        },
    ];
    revise(
        &mut store,
        "revise-resolve",
        key,
        &refs,
        "t",
        1,
        1,
        &base,
        &mut l,
    )
    .unwrap();
    let base2 = [
        RevPair {
            key: concept_a(),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key,
            revision: RecordRevision::new(2).unwrap(),
        },
    ];
    revise(
        &mut store,
        "revise-reopen",
        key,
        &refs,
        "t",
        1,
        0,
        &base2,
        &mut l,
    )
    .unwrap();
    let read_back = read(&mut store, key, 1 << 20, &mut l).unwrap();
    assert_eq!(record_of(&read_back).status, FrontierStatus::Open);
    assert_eq!(
        record_of(&read_back).revision,
        RecordRevision::new(3).unwrap()
    );
}

#[test]
fn revise_cas_failure_leaves_zero_mutations() {
    let (_inner, fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];
    create(
        &mut store,
        "create-1",
        &refs,
        "t",
        0,
        0,
        &baseline_a(),
        &mut l,
    )
    .unwrap();
    let key = frontier_key(1);

    let before = {
        let counts = fs.counts();
        let borrowed = counts.borrow();
        borrowed.total()
    };
    // Claim a frontier revision that is not current.
    let wrong = [
        RevPair {
            key: concept_a(),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key,
            revision: RecordRevision::new(9).unwrap(),
        },
    ];
    assert!(matches!(
        command_error(revise(
            &mut store,
            "revise-stale",
            key,
            &refs,
            "x",
            0,
            0,
            &wrong,
            &mut l
        )),
        CommandError::Input(InputError::StaleBaseline { .. })
    ));
    let after = {
        let counts = fs.counts();
        let borrowed = counts.borrow();
        borrowed.total()
    };
    assert_eq!(
        before, after,
        "a CAS refusal must perform no mutating/sync call"
    );
}

#[test]
fn revising_a_missing_entry_is_refused() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let key = frontier_key(7);
    let refs = [concept_a()];
    assert!(matches!(
        input_error(revise(
            &mut store,
            "revise-missing",
            key,
            &refs,
            "t",
            0,
            0,
            &[RevPair { key: concept_a(), revision: RecordRevision::FIRST }, RevPair { key, revision: RecordRevision::FIRST }],
            &mut l
        )),
        FrontierInputError::MissingFrontier(k) if k == key
    ));
}

// ---------------------------------------------------------------------------
// Idempotency and ambiguity
// ---------------------------------------------------------------------------

#[test]
fn an_exact_retry_returns_the_original_outcome_without_mutating() {
    let (_inner, fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];
    let first = create(
        &mut store,
        "create-1",
        &refs,
        "t",
        1,
        0,
        &baseline_a(),
        &mut l,
    )
    .unwrap();
    let first_version = committed_version(first.clone());

    let before = {
        let c = fs.counts();
        let b = c.borrow();
        b.total()
    };
    let retry = create(
        &mut store,
        "create-1",
        &refs,
        "t",
        1,
        0,
        &baseline_a(),
        &mut l,
    )
    .unwrap();
    let after = {
        let c = fs.counts();
        let b = c.borrow();
        b.total()
    };

    assert!(
        matches!(retry, HandleOutcome::AlreadyCommitted { version, .. } if version == first_version)
    );
    assert_eq!(
        before, after,
        "an exact retry must not perform a mutating/sync call"
    );
    assert_eq!(store.head(), Some(first_version));
}

#[test]
fn the_same_request_id_with_different_content_is_refused() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];
    create(
        &mut store,
        "create-1",
        &refs,
        "first",
        1,
        0,
        &baseline_a(),
        &mut l,
    )
    .unwrap();
    assert!(matches!(
        command_error(create(
            &mut store,
            "create-1",
            &refs,
            "different",
            1,
            0,
            &baseline_a(),
            &mut l
        )),
        CommandError::IdempotencyMismatch
    ));
}

// ---------------------------------------------------------------------------
// Independently derived expectations for the Unknown-commit control
// ---------------------------------------------------------------------------

/// A request identity the test derives from the **actual submitted bytes**, never a
/// constant and never a copy of what the store happens to persist.
///
/// The payload comes from the production intent encoder, the frame from the public
/// canonical request encoder (with the caller's real read set), and the digest from
/// the public canonical digest helper. That is the same chain the command itself
/// walks, but entered from the outside: nothing here reads a persisted field.
struct DerivedRequest {
    principal: &'static str,
    request_id: String,
    payload_digest: [u8; 32],
}

impl DerivedRequest {
    /// Derive one request identity exactly as the canonical wire would.
    ///
    /// `kind` is `CREATE_FRONTIER` or `REVISE_FRONTIER`; `payload` is the encoder's
    /// output for the same state the caller submits; `read_set` is the actual set.
    fn derive(
        principal: &'static str,
        request_id: &str,
        kind: u16,
        payload: Vec<u8>,
        read_set: &[RevPair],
    ) -> Self {
        use sage_db_format::request::{RequestFrame, RequestOp, encode_request};
        let mut l = ledger();
        let ops = [RequestOp::new(kind, OPERATION_VERSION, &payload)];
        let frame = RequestFrame::new(&ops, read_set, None);
        let bytes = match encode_request(&frame, &mut l) {
            typed::RequestOutcome::Ok(bytes) => bytes,
            other => panic!("canonical frame encode refused: {other:?}"),
        };
        let digest = match sage_db_format::request::request_digest_of(&bytes, &mut l) {
            typed::RequestOutcome::Ok(digest) => digest,
            other => panic!("canonical request digest refused: {other:?}"),
        };
        Self {
            principal,
            request_id: request_id.to_owned(),
            payload_digest: digest,
        }
    }

    /// The oracle's logical request for this identity.
    fn logical(&self) -> sage_db_store_tests::oracle::attempts::LogicalRequest {
        sage_db_store_tests::oracle::attempts::LogicalRequest::new(
            self.principal,
            &self.request_id,
            self.payload_digest,
        )
    }

    /// The identity the store is expected to have persisted for a committed attempt.
    ///
    /// The persisted `(principal_id, request_id, payload_digest)` must equal these
    /// values. Comparing them is what makes "the store recorded the request I sent"
    /// an observation rather than an assumption.
    fn assert_persisted(&self, outcome: &RequestOutcomeEvent, where_: &str) {
        assert_eq!(outcome.principal_id, self.principal, "{where_}: principal");
        assert_eq!(outcome.request_id, self.request_id, "{where_}: request id");
        assert_eq!(
            outcome.payload_digest, self.payload_digest,
            "{where_}: payload digest"
        );
    }
}

/// Compare two outcome events **in full**, including the audit and advisory fields.
///
/// A field-by-field digest comparison would pass with a wrong recorded-by, a moved
/// timestamp or a dropped extension; this compares the whole value so a change to any
/// part of the committed outcome is a failure.
fn assert_same_outcome(expected: &RequestOutcomeEvent, actual: &RequestOutcomeEvent, where_: &str) {
    assert_eq!(
        actual.principal_id, expected.principal_id,
        "{where_}: principal"
    );
    assert_eq!(
        actual.request_id, expected.request_id,
        "{where_}: request id"
    );
    assert_eq!(
        actual.payload_digest, expected.payload_digest,
        "{where_}: payload digest"
    );
    assert_eq!(
        actual.result_ref, expected.result_ref,
        "{where_}: result ref"
    );
    assert_eq!(
        actual.commit_version, expected.commit_version,
        "{where_}: commit version"
    );
    assert_eq!(actual.audit, expected.audit, "{where_}: audit");
    assert_eq!(
        actual.extensions, expected.extensions,
        "{where_}: extensions"
    );
}

#[test]
fn a_real_unknown_commit_is_judged_by_every_restored_crash_state() {
    use sage_db_store::open::{Opened, OutcomeLookup, Resolution, open_existing};
    use sage_db_store_tests::oracle::attempts::{AttemptHistory, AttemptOutcome, Commit, World};

    let (inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];

    // --- The acknowledged base create, with its expectation derived independently ---
    let base_read_set = baseline_a();
    let base_payload = {
        // The canonical intent encoder, called from here rather than read off the store.
        let mut l = ledger();
        sage_db_kernel::frontier_intent::encode_create(&state(&refs, "old text", 0, 0), &mut l)
            .expect("canonical create payload")
    };
    let base_request = DerivedRequest::derive(
        WRITER,
        "ack-create",
        CREATE_FRONTIER,
        base_payload,
        &base_read_set,
    );

    let base_outcome = create(
        &mut store,
        "ack-create",
        &refs,
        "old text",
        0,
        0,
        &base_read_set,
        &mut l,
    )
    .unwrap();
    let base_version = committed_version(base_outcome.clone());
    // The four-field identity actually written must equal the derived one.
    let HandleOutcome::Committed { recorded, .. } = &base_outcome else {
        panic!("the base create must commit, got {base_outcome:?}");
    };
    let sage_db_store::commit::RecordedOutcome::Outcome(shared) = recorded else {
        panic!("a non-Genesis commit must record an outcome");
    };
    base_request.assert_persisted(shared.event(), "base create");
    // Save the acknowledged event so later worlds can be compared against it, not
    // against a reconstruction.
    let acknowledged_base_event = shared.event().clone();

    let key = frontier_key(1);
    let old = read(&mut store, key, BYTES, &mut l).unwrap();
    let old_record = record_of(&old).clone();
    let old_text = text_of(&old).to_vec();
    let old_base = store
        .view()
        .unwrap_or_else(|| panic!("the acknowledged create must publish a view"))
        .basis();

    // --- The candidate revise, same derivation discipline ---
    let revise_refs = [concept_a(), concept_b()];
    let revise_set = [
        RevPair {
            key: concept_a(),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: concept_b(),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key,
            revision: RecordRevision::FIRST,
        },
    ];
    let revise_payload = {
        let mut l = ledger();
        sage_db_kernel::frontier_intent::encode_revise(
            key,
            &state(&revise_refs, "new text", 0, 1),
            &mut l,
        )
        .expect("canonical revise payload")
    };
    let revise_request = DerivedRequest::derive(
        WRITER,
        "unknown-revise",
        REVISE_FRONTIER,
        revise_payload,
        &revise_set,
    );

    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    let outcome = revise(
        &mut store,
        "unknown-revise",
        key,
        &revise_refs,
        "new text",
        0,
        1,
        &revise_set,
        &mut l,
    )
    .expect("Unknown is an outcome, never an error");
    assert!(matches!(outcome, HandleOutcome::Unknown));
    assert!(inner.faults_handle().borrow().all_fired());
    assert!(store.authority_undecided());

    // Sample every crash state BEFORE resolve can synchronize the visible candidate root.
    let worlds = inner.model().crash_states();
    assert!(!worlds.states.is_empty());
    assert_eq!(worlds.sampled, worlds.states.len());

    // The oracle's history, built from the DERIVED identities.
    let world_base = base_version - 1;
    let mut history = AttemptHistory::default();
    history.record(
        "ack-create",
        base_request.logical(),
        AttemptOutcome::Durable {
            version: base_version,
        },
    );
    history.record(
        "unknown-revise",
        revise_request.logical(),
        AttemptOutcome::Unknown,
    );

    let mut examined = 0usize;
    let mut old_seen = false;
    let mut new_seen = false;
    for (index, state_bytes) in worlds.states.into_iter().enumerate() {
        let restored = ModelFileSystem::from_durable_state(
            ModelCapabilities::full(),
            Path::new(ROOT),
            state_bytes,
        )
        .unwrap_or_else(|e| panic!("world {index} restore failed: {e:?}"));
        let fs = ObservingFs::new(Rc::new(restored));
        let mut reopened =
            match open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger())
                .unwrap_or_else(|e| panic!("world {index} open failed: {e:?}"))
            {
                Opened::Ready(store) => store,
                other => panic!(
                    "world {index} did not become Ready: {}",
                    opened_name(&other)
                ),
            };
        let head = reopened
            .head()
            .unwrap_or_else(|| panic!("world {index} has no head"));
        assert!(
            head >= base_version,
            "world {index} lost the acknowledged base commit: head {head} < {base_version}"
        );

        // The observed base commit: identity and version come from what is ACTUALLY
        // persisted in this world, then must equal the independent derivation.
        let observed_base = match reopened.outcome(WRITER, "ack-create") {
            OutcomeLookup::Found {
                outcome,
                original_version,
                ..
            } => {
                assert_eq!(
                    original_version, base_version,
                    "world {index} moved the acknowledged version"
                );
                // The saved acknowledged event must still be exactly what is stored.
                assert_same_outcome(
                    &acknowledged_base_event,
                    outcome.event(),
                    &format!("world {index} acknowledged base"),
                );
                // ... and it must equal the independently prepared expectation.
                base_request
                    .assert_persisted(outcome.event(), &format!("world {index} acknowledged base"));
                Commit {
                    version: original_version,
                    request: sage_db_store_tests::oracle::attempts::LogicalRequest::new(
                        &outcome.event().principal_id,
                        &outcome.event().request_id,
                        outcome.event().payload_digest,
                    ),
                }
            }
            other => panic!("world {index} lost the acknowledged outcome: {other:?}"),
        };

        let mut commits = vec![observed_base];
        let observed = read(&mut reopened, key, BYTES, &mut l)
            .unwrap_or_else(|e| panic!("world {index} frontier read failed: {e:?}"));
        match observed {
            FrontierRead::Absent { .. } => {
                panic!("world {index} lost the acknowledged frontier entry")
            }
            FrontierRead::Found { record, text } => {
                let view_basis = reopened
                    .view()
                    .unwrap_or_else(|| panic!("world {index} published no view"))
                    .basis();
                if head == base_version {
                    assert_eq!(
                        view_basis, old_base,
                        "world {index} answered the old state on a different basis"
                    );
                } else {
                    assert_eq!(
                        view_basis.version.get(),
                        head,
                        "world {index} published a basis that does not name its own head"
                    );
                }
                let entry = match record.record() {
                    FoldedRecord::FrontierEntry(entry) => entry.clone(),
                    other => panic!("world {index} read back {other:?}"),
                };
                assert_eq!(
                    entry.text_ref,
                    sage_db_format::digest::raw_sha256(text.as_bytes())
                );

                if head == base_version {
                    old_seen = true;
                    assert_eq!(entry, old_record, "world {index} old after-image differs");
                    assert_eq!(
                        text.as_bytes(),
                        old_text.as_slice(),
                        "world {index} old text differs"
                    );
                    assert!(
                        matches!(
                            reopened.outcome(WRITER, "unknown-revise"),
                            OutcomeLookup::Absent { .. }
                        ),
                        "world {index} has candidate state without its outcome"
                    );
                } else {
                    new_seen = true;
                    let mut expected = old_record.clone();
                    expected.revision = RecordRevision::new(2).unwrap();
                    expected.status = FrontierStatus::Resolved;
                    expected.priority = 0;
                    expected.concept_refs = revise_refs.to_vec();
                    expected.text_ref = sage_db_format::digest::raw_sha256(b"new text");
                    expected.common.recorded_by = Some(WRITER.into());
                    expected.common.actor_origin = ActorOrigin::Authenticated;
                    expected.common.updated_at =
                        Some(meta_at("unknown-revise", 1_700_000_200_000_000).created_at);
                    assert_eq!(entry, expected, "world {index} new after-image differs");
                    assert_eq!(
                        text.as_bytes(),
                        b"new text",
                        "world {index} new text differs"
                    );

                    // The candidate commit is observed from the store, then checked
                    // against the derived request identity.
                    match reopened.outcome(WRITER, "unknown-revise") {
                        OutcomeLookup::Found {
                            outcome,
                            original_version,
                            ..
                        } => {
                            assert_eq!(original_version, head);
                            revise_request.assert_persisted(
                                outcome.event(),
                                &format!("world {index} candidate"),
                            );
                            assert_eq!(
                                outcome.event().commit_version,
                                head,
                                "world {index} candidate outcome names another version"
                            );
                            // Its result payload must describe THIS version and the
                            // affected record, so a stale or fabricated result blob is
                            // rejected rather than ignored.
                            let result = reopened
                                .read_committed_blob(
                                    outcome.event().result_ref,
                                    BYTES,
                                    &mut ledger(),
                                )
                                .unwrap_or_else(|e| {
                                    panic!("world {index} result blob unreadable: {e:?}")
                                });
                            assert_eq!(
                                result.as_bytes(),
                                typed::result_payload(
                                    head,
                                    &[RevPair {
                                        key,
                                        revision: RecordRevision::new(2).unwrap(),
                                    }],
                                    &[],
                                    &mut ledger(),
                                )
                                .expect("result payload"),
                                "world {index} candidate result payload differs"
                            );
                            let expected_result = typed::result_payload(
                                base_version + 1,
                                &[RevPair {
                                    key,
                                    revision: RecordRevision::new(2).unwrap(),
                                }],
                                &[],
                                &mut ledger(),
                            )
                            .expect("independent candidate result");
                            let expected_outcome = RequestOutcomeEvent {
                                principal_id: WRITER.into(),
                                request_id: "unknown-revise".into(),
                                payload_digest: revise_request.payload_digest,
                                result_ref: sage_db_format::digest::raw_sha256(&expected_result),
                                commit_version: base_version + 1,
                                audit: CreationAudit {
                                    recorded_by: Some(WRITER.into()),
                                    actor_origin: ActorOrigin::Authenticated,
                                    reason: None,
                                    created_at: meta_at("unknown-revise", 1_700_000_200_000_000)
                                        .created_at,
                                },
                                extensions: Extensions::new(),
                            };
                            assert_same_outcome(
                                &expected_outcome,
                                outcome.event(),
                                "candidate full outcome",
                            );
                            commits.push(Commit {
                                version: original_version,
                                request: sage_db_store_tests::oracle::attempts::LogicalRequest::new(
                                    &outcome.event().principal_id,
                                    &outcome.event().request_id,
                                    outcome.event().payload_digest,
                                ),
                            });
                        }
                        other => panic!(
                            "world {index} has candidate after-image without its outcome: {other:?}"
                        ),
                    }
                }
            }
        }

        assert!(
            history
                .accepts(
                    world_base,
                    &World {
                        base_version: world_base,
                        commits
                    },
                )
                .unwrap(),
            "sequence oracle rejected world {index}"
        );
        examined += 1;
    }
    assert_eq!(
        examined, worlds.sampled,
        "every sampled world must be examined"
    );
    assert!(
        old_seen && new_seen,
        "this control must actually exercise BOTH whole alternatives (old_seen={old_seen}, new_seen={new_seen})"
    );

    // The derived identities must differ from each other, or the oracle could not tell
    // the two attempts apart and every judgement above would be vacuous.
    assert_ne!(
        base_request.payload_digest, revise_request.payload_digest,
        "the two requests must have distinct digests"
    );

    assert!(matches!(
        store.resolve(&Limits::V1, &mut l),
        Resolution::Ready
    ));
    let resolved = read(&mut store, key, BYTES, &mut l).unwrap();
    let entry = record_of(&resolved).clone();
    let text = text_of(&resolved).to_vec();
    assert_eq!(entry.text_ref, sage_db_format::digest::raw_sha256(&text));
    match entry.revision.get() {
        1 => assert_eq!(
            text, old_text,
            "a resolved revision 1 must carry the old text"
        ),
        2 => assert_eq!(
            text, b"new text",
            "a resolved revision 2 must carry the new text"
        ),
        other => panic!("a resolved revision cannot be {other}"),
    }

    println!(
        "FRONTIER_UNKNOWN_SAMPLED={} EXAMINED={examined}",
        worlds.sampled
    );
}

#[test]
fn the_sequence_oracle_rejects_a_wrong_persisted_digest_or_identity() {
    use sage_db_store_tests::oracle::attempts::{
        AttemptHistory, AttemptOutcome, Commit, LogicalRequest, World,
    };

    // A discriminating control for the oracle itself: the same history, judged against
    // a world whose commit carries a request the store would never have persisted.
    // Without this, "the oracle accepted every world" could be satisfied by an oracle
    // that accepts anything.
    let acknowledged = LogicalRequest::new(WRITER, "ack-create", [0x11; 32]);
    let mut history = AttemptHistory::default();
    history.record(
        "ack-create",
        acknowledged.clone(),
        AttemptOutcome::Durable { version: 2 },
    );
    history.record(
        "unknown-revise",
        LogicalRequest::new(WRITER, "unknown-revise", [0x22; 32]),
        AttemptOutcome::Unknown,
    );

    let honest = World {
        base_version: 1,
        commits: vec![Commit {
            version: 2,
            request: acknowledged.clone(),
        }],
    };
    assert!(
        history.accepts(1, &honest).expect("model"),
        "the honest world must be accepted"
    );

    // Same version, different digest: this is the shape a fabricated
    // `[base_version as u8; 32]` digest would accidentally satisfy.
    let wrong_digest = World {
        base_version: 1,
        commits: vec![Commit {
            version: 2,
            request: LogicalRequest::new(WRITER, "ack-create", [0x33; 32]),
        }],
    };
    assert!(
        !history.accepts(1, &wrong_digest).expect("model"),
        "a world whose persisted digest differs must be rejected"
    );

    // Same digest, different principal: identity, not just content.
    let wrong_identity = World {
        base_version: 1,
        commits: vec![Commit {
            version: 2,
            request: LogicalRequest::new("someone-else", "ack-create", [0x11; 32]),
        }],
    };
    assert!(
        !history.accepts(1, &wrong_identity).expect("model"),
        "a world whose persisted principal differs must be rejected"
    );
}

/// Name an `Opened` that is not `Ready`, for failure messages.
fn opened_name(opened: &Opened<ObservingFs>) -> &'static str {
    match opened {
        Opened::Ready(_) => "Ready",
        Opened::ReadOnly { .. } => "ReadOnly",
    }
}

// ---------------------------------------------------------------------------
// Payload codec bounds
// ---------------------------------------------------------------------------

#[test]
fn priority_and_status_domains_are_closed() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];
    assert!(matches!(
        input_error(create(
            &mut store,
            "create-prio",
            &refs,
            "t",
            3,
            0,
            &baseline_a(),
            &mut l
        )),
        FrontierInputError::Payload(FrontierPayloadError::UnknownPriority(3))
    ));
    assert!(matches!(
        input_error(create(
            &mut store,
            "create-status",
            &refs,
            "t",
            0,
            2,
            &baseline_a(),
            &mut l
        )),
        FrontierInputError::Payload(FrontierPayloadError::UnknownStatus(2))
    ));
}

#[test]
fn refs_are_bounded_at_64_and_rejected_at_65() {
    use sage_db_kernel::frontier_intent::{FrontierPayloadError as E, encode_create};
    let mut l = ledger();
    let at_64: Vec<RecordKey> = (1..=64).map(RecordKey::term).collect();
    let state64 = FrontierState {
        concept_refs: &at_64,
        text: "",
        priority: 0,
        status: 0,
    };
    assert!(encode_create(&state64, &mut l).is_ok());

    let at_65: Vec<RecordKey> = (1..=65).map(RecordKey::term).collect();
    let state65 = FrontierState {
        concept_refs: &at_65,
        text: "",
        priority: 0,
        status: 0,
    };
    assert_eq!(encode_create(&state65, &mut l), Err(E::TooManyRefs(65)));
}

#[test]
fn an_unsorted_reference_list_is_refused_not_sorted() {
    use sage_db_kernel::frontier_intent::{FrontierPayloadError as E, encode_create};
    let mut l = ledger();
    let refs = [RecordKey::term(2), RecordKey::term(1)];
    let state = FrontierState {
        concept_refs: &refs,
        text: "",
        priority: 0,
        status: 0,
    };
    assert_eq!(
        encode_create(&state, &mut l),
        Err(E::RefsNotAscending { index: 1 })
    );
}

#[test]
fn the_exact_accepted_payload_maximum_is_encoded_and_one_more_is_refused() {
    use sage_db_kernel::frontier_intent::{FrontierPayloadError as E, MAX_REFS, encode_create};
    use sage_db_store::typed::MAX_PAYLOAD;

    let mut l = ledger();
    // Fixed cost: count(4) + refs + text prefix(4) + priority(2) + status(2).
    let fixed = 4 + (MAX_REFS as usize) * 18 + 4 + 2 + 2;
    let text_at_max = (MAX_PAYLOAD as usize) - fixed;
    let refs: Vec<RecordKey> = (1..=MAX_REFS).map(RecordKey::term).collect();
    let text = "x".repeat(text_at_max);
    let state = FrontierState {
        concept_refs: &refs,
        text: &text,
        priority: 0,
        status: 0,
    };
    let bytes = encode_create(&state, &mut l).expect("exactly MAX_PAYLOAD must encode");
    assert_eq!(bytes.len(), MAX_PAYLOAD as usize);

    // One more byte is refused BEFORE admission: no allocation is attempted.
    let text_over = "x".repeat(text_at_max + 1);
    let state_over = FrontierState {
        concept_refs: &refs,
        text: &text_over,
        priority: 0,
        status: 0,
    };
    assert_eq!(
        encode_create(&state_over, &mut l),
        Err(E::TooLarge {
            len: (MAX_PAYLOAD as u64) + 1,
            max: MAX_PAYLOAD
        })
    );
}

#[test]
fn the_canonical_tag_for_this_family_is_112() {
    // The canonical writer is the only authority on the event tag. Read it from the format
    // crate's own registry rather than restating this task's number.
    use sage_db_format::dispatch::CodecFamily;
    // Tag 112 is the registry's owner of the frontier family; asking the registry is what
    // makes this an assertion about the source rather than about this task's text.
    assert_eq!(CodecFamily::of(112).unwrap(), CodecFamily::FrontierEntry);
    assert_eq!(CREATE_FRONTIER, 5);
    assert_eq!(REVISE_FRONTIER, 6);
}

// ---------------------------------------------------------------------------
// F4: import gate, id exhaustion/collision, and caller-stop vs real budget classes
// ---------------------------------------------------------------------------

#[test]
fn an_unfinished_import_refuses_frontier_reads_and_writes_with_no_mutations() {
    use sage_db_types::persist::ImportStage;

    let (_inner, fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];
    // A durable acknowledged entry exists, so a read has something it *could* answer.
    create(
        &mut store,
        "create-before-gate",
        &refs,
        "pre-gate",
        0,
        0,
        &baseline_a(),
        &mut l,
    )
    .unwrap();

    // Persist an import stage: ordinary access must refuse until it completes.
    let basis = store
        .commit_basis(IdSpace::FrontierEntryId, &mut l)
        .unwrap();
    let version = store.head().unwrap() + 1;
    let plan = b"frontier import gate plan".to_vec();
    let result = b"frontier import gate result".to_vec();
    let plan_ref = sage_db_format::digest::raw_sha256(&plan);
    let result_ref = sage_db_format::digest::raw_sha256(&result);
    let audit = CreationAudit {
        recorded_by: Some(WRITER.into()),
        actor_origin: ActorOrigin::Authenticated,
        reason: Some("physical import gate fixture".into()),
        created_at: version as i64,
    };
    let record = sage_db_types::persist::ImportSessionRecord {
        import_session_id: "frontier-gate".into(),
        stage: ImportStage::Prepare,
        source_digest: [78; 32],
        plan_ref,
        expected_counts: vec![],
        import_complete: false,
        audit: audit.clone(),
        extensions: Extensions::new(),
    };
    let outcome = RequestOutcomeEvent {
        principal_id: WRITER.into(),
        request_id: format!("frontier-stage-{version}"),
        payload_digest: [version as u8; 32],
        result_ref,
        commit_version: version,
        audit,
        extensions: Extensions::new(),
    };
    let event = prepared(typed::prepare_event(
        EventInput::ImportSession(&record),
        &mut l,
    ));
    let outcome_event = prepared(typed::prepare_event(EventInput::Outcome(&outcome), &mut l));
    let stage = PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: [0x51; 16],
        kb_uuid: kb(),
        created_at: version as i64,
        principal_id: WRITER.into(),
        request_id: format!("frontier-stage-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: store.head(),
        extensions: Extensions::new(),
        events: vec![event, outcome_event],
        blobs: vec![
            BlobDeclaration {
                sha256: basis.rules.rules_ref,
                declared_len: basis.rules_len,
                source: BlobSource::Existing,
            },
            BlobDeclaration {
                sha256: plan_ref,
                declared_len: plan.len() as u64,
                source: BlobSource::Provide(plan),
            },
            BlobDeclaration {
                sha256: result_ref,
                declared_len: result.len() as u64,
                source: BlobSource::Provide(result),
            },
        ],
        rules: basis.rules,
    };
    committed_version(store.commit(stage, &Limits::V1, &mut l));

    let before = {
        let c = fs.counts();
        let b = c.borrow();
        b.total()
    };

    // A read is refused by the import gate.
    assert!(matches!(
        read(&mut store, frontier_key(1), BYTES, &mut l),
        Err(FrontierError::Command(CommandError::ImportInProgress))
    ));
    // So is a new write.
    let refs_b = [concept_b()];
    let write_refusal = create(
        &mut store,
        "create-under-gate",
        &refs_b,
        "x",
        0,
        0,
        &baseline_b(),
        &mut l,
    );
    match &write_refusal {
        Err(FrontierError::Command(CommandError::ImportInProgress)) => {}
        other => panic!("a gated write must be refused by the import gate: {other:?}"),
    }
    let after = {
        let c = fs.counts();
        let b = c.borrow();
        b.total()
    };
    assert_eq!(
        before, after,
        "an import-gated refusal must not mutate anything"
    );
}

#[test]
fn the_next_frontier_id_is_allocated_from_the_committed_mark() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let refs = [concept_a()];
    create(
        &mut store,
        "create-1",
        &refs,
        "one",
        0,
        0,
        &baseline_a(),
        &mut l,
    )
    .unwrap();
    create(
        &mut store,
        "create-2",
        &refs,
        "two",
        0,
        0,
        &baseline_a(),
        &mut l,
    )
    .unwrap();

    let first = read(&mut store, frontier_key(1), BYTES, &mut l).unwrap();
    let second = read(&mut store, frontier_key(2), BYTES, &mut l).unwrap();
    assert_eq!(record_of(&first).key, frontier_key(1));
    assert_eq!(record_of(&second).key, frontier_key(2));
    assert_eq!(text_of(&second), b"two");
    // Id 3 is genuinely absent, not merely unread.
    assert!(matches!(
        read(&mut store, frontier_key(3), BYTES, &mut l).unwrap(),
        FrontierRead::Absent { .. }
    ));
}

/// Seed one `IdAllocation` mark for a space through the real commit path.
///
/// The format layer's own guard is `mark.hwm <= mark.space.max_id()`, and `FrontierEntryId`
/// resolves to `u128::MAX`; `StateFold::apply_allocation` then takes `max(current, observed)`
/// with no further ceiling. So any mark up to `u128::MAX` is a legal, physically committable
/// input — which is what makes the two boundary tests below real tests rather than hypotheticals.
fn seed_allocation(
    store: &mut Store<ObservingFs>,
    version: u64,
    hwm: u128,
    ledger: &mut AttemptLedger<'_>,
) -> u64 {
    let allocation = IdAllocationEvent {
        allocation: Watermark {
            space: IdSpace::FrontierEntryId,
            hwm,
        },
        reason: Some("frontier boundary fixture, a legal physical mark".into()),
        extensions: Extensions::new(),
    };
    let event = prepared(typed::prepare_event(
        EventInput::IdAllocation(&allocation),
        ledger,
    ));
    commit_seed(
        store,
        Seed {
            version,
            events: vec![event],
            blobs: vec![],
            affected: vec![],
        },
        &format!("seed-allocation-{version}"),
        ledger,
    );
    version
}

#[test]
fn an_exhausted_frontier_id_space_refuses_creation_without_mutation() {
    let (_inner, fs, mut store) = seed_world();
    let mut l = ledger();

    // A physically legal mark: `u128::MAX` is within `IdSpace::FrontierEntryId::max_id()`.
    let version = seed_allocation(&mut store, 4, u128::MAX, &mut l);
    assert_eq!(store.head(), Some(version));
    // The mark really took effect: this is the folded high-water mark the writer will read.
    let basis = store
        .commit_basis(IdSpace::FrontierEntryId, &mut l)
        .unwrap();
    assert_eq!(basis.allocated_high_water, u128::MAX);

    let before = {
        let c = fs.counts();
        let b = c.borrow();
        b.total()
    };
    let refs = [concept_a()];
    match create(
        &mut store,
        "create-exhausted",
        &refs,
        "x",
        0,
        0,
        &baseline_a(),
        &mut l,
    ) {
        Err(FrontierError::Input(FrontierInputError::FrontierIdExhausted)) => {}
        other => panic!("an exhausted id space must refuse creation: {other:?}"),
    }
    let after = {
        let c = fs.counts();
        let b = c.borrow();
        b.total()
    };
    assert_eq!(before, after, "an exhausted allocation must not mutate");
    assert_eq!(store.head(), Some(version));
}

#[test]
fn an_occupied_next_frontier_id_is_a_collision_without_mutation() {
    let (_inner, fs, mut store) = seed_world();
    let mut l = ledger();

    // Physically place a frontier record at id 1 WITHOUT advancing the allocation mark.
    // P1 permits this: allocation policy is the kernel's P2 concern, not the store's.
    let key = frontier_key(1);
    let text = b"pre-existing physical entry".to_vec();
    let text_ref = sage_db_format::digest::raw_sha256(&text);
    let entry = sage_db_types::persist::FrontierEntryRecord {
        key,
        revision: RecordRevision::FIRST,
        status: FrontierStatus::Open,
        concept_refs: vec![concept_a()],
        text_ref,
        priority: 0,
        common: CommonFields {
            recorded_by: Some(WRITER.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        extensions: Extensions::new(),
        changed_fields: None,
    };
    let event = prepared(typed::prepare_event(EventInput::Frontier(&entry), &mut l));
    let version = 4;
    commit_seed(
        &mut store,
        Seed {
            version,
            events: vec![event],
            blobs: vec![blob(text)],
            affected: vec![RevPair {
                key,
                revision: RecordRevision::FIRST,
            }],
        },
        "seed-physical-frontier",
        &mut l,
    );

    // The mark was not advanced by that commit, so the writer will compute id 1 and find it taken.
    let basis = store
        .commit_basis(IdSpace::FrontierEntryId, &mut l)
        .unwrap();
    assert_eq!(
        basis.allocated_high_water, 0,
        "the physical seed must not have advanced the allocation mark"
    );
    assert!(matches!(
        read(&mut store, key, BYTES, &mut l).unwrap(),
        FrontierRead::Found { .. }
    ));

    let before = {
        let c = fs.counts();
        let b = c.borrow();
        b.total()
    };
    let refs = [concept_a()];
    match create(
        &mut store,
        "create-collision",
        &refs,
        "x",
        0,
        0,
        &baseline_a(),
        &mut l,
    ) {
        Err(FrontierError::Input(FrontierInputError::FrontierIdCollision(hit))) => {
            assert_eq!(hit, key);
        }
        other => panic!("an occupied next id must be a collision: {other:?}"),
    }
    let after = {
        let c = fs.counts();
        let b = c.borrow();
        b.total()
    };
    assert_eq!(before, after, "a collision refusal must not mutate");
    assert_eq!(store.head(), Some(version));
    // The pre-existing record is untouched, and no id 2 appeared.
    let still = read(&mut store, key, BYTES, &mut l).unwrap();
    assert_eq!(text_of(&still), b"pre-existing physical entry");
    assert!(matches!(
        read(&mut store, frontier_key(2), BYTES, &mut l).unwrap(),
        FrontierRead::Absent { .. }
    ));
}

/// Find the ledger refusal inside whatever wrapper the store's first ledger touch produced.
///
/// The class itself is what must be preserved. Whether a stop surfaces bare or nested inside
/// the step that happened to consume the ledger first (authorization, basis read) is the
/// store's existing shape, not this family's decision — and the payload is intact either way.
fn refusal_payload(error: &FrontierError) -> Option<Refusal> {
    fn from_command(error: &CommandError) -> Option<Refusal> {
        match error {
            CommandError::Refused(refusal) => Some(*refusal),
            CommandError::Auth(sage_db_kernel::auth::AuthError::Refused(refusal)) => Some(*refusal),
            CommandError::Basis(sage_db_store::view::CommitReadError::Refused(refusal)) => {
                Some(*refusal)
            }
            CommandError::Record(sage_db_store::view::RecordReadError::Refused(refusal)) => {
                Some(*refusal)
            }
            CommandError::Blob(sage_db_store::view::BlobReadError::Refused(refusal)) => {
                Some(*refusal)
            }
            _ => None,
        }
    }
    match error {
        FrontierError::Command(command) => from_command(command),
        _ => None,
    }
}

#[test]
fn caller_stop_and_byte_budget_keep_their_own_classes() {
    use sage_db_types::ledger::BudgetUnit;
    use sage_db_types::query::{ControlStop, QueryControl};

    let (_inner, _fs, mut store) = seed_world();
    let refs = [concept_a()];

    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let mut stopped = AttemptLedger::open(BYTES, ENTRIES, &Stop).unwrap();
    match create(
        &mut store,
        "stopped",
        &refs,
        "x",
        0,
        0,
        &baseline_a(),
        &mut stopped,
    ) {
        Err(error) => assert_eq!(
            refusal_payload(&error),
            Some(Refusal::Stopped(ControlStop::Cancelled)),
            "a caller stop must keep its own class: {error:?}"
        ),
        Ok(other) => panic!("a stopped ledger must not commit: {other:?}"),
    }

    // A zero-byte ledger refuses on Bytes, not on Entries and not as a stop.
    let mut tiny = AttemptLedger::open(0, 0, &RunToCompletion).unwrap();
    match create(
        &mut store,
        "tiny",
        &refs,
        "x",
        0,
        0,
        &baseline_a(),
        &mut tiny,
    ) {
        Err(error) => match refusal_payload(&error) {
            Some(Refusal::Budget(budget)) => assert_eq!(budget.unit(), BudgetUnit::Bytes),
            other => panic!("a zero-byte ledger must refuse on Bytes, got {other:?}: {error:?}"),
        },
        Ok(other) => panic!("a zero-byte ledger must not commit: {other:?}"),
    }
    assert_eq!(tiny.stats().live_bytes, 0);

    // Nothing was created under either refusal.
    let mut l = ledger();
    assert!(matches!(
        read(&mut store, frontier_key(1), BYTES, &mut l).unwrap(),
        FrontierRead::Absent { .. }
    ));
}

#[test]
fn an_entries_only_ledger_refuses_on_entries_not_bytes() {
    use sage_db_types::ledger::BudgetUnit;

    let (_inner, _fs, mut store) = seed_world();
    let refs = [concept_a()];
    // Plenty of bytes, no entry allowance: the unit must be Entries.
    let mut entries_only = AttemptLedger::open(1 << 20, 0, &RunToCompletion).unwrap();
    match create(
        &mut store,
        "no-entries",
        &refs,
        "x",
        0,
        0,
        &baseline_a(),
        &mut entries_only,
    ) {
        Err(error) => match refusal_payload(&error) {
            Some(Refusal::Budget(budget)) => assert_eq!(budget.unit(), BudgetUnit::Entries),
            other => {
                panic!("an entries-only ledger must refuse on Entries, got {other:?}: {error:?}")
            }
        },
        Ok(other) => panic!("an entries-only ledger must not commit: {other:?}"),
    }
}
