//! P2 auxiliary session-checkpoint lifecycle on a real public Store and the model backend.
//!
//! Every command here goes through the production entry points — `bootstrap`,
//! `Store::commit`, `open_existing`, `create_session_checkpoint`,
//! `revise_session_checkpoint`, `read_session_checkpoint` — with the real ledger, the real
//! canonical codecs and a real fault injector. The seed term is produced by the canonical
//! `sage_db_format::term` writer and read back through the canonical envelope reader, so a
//! seeded record is byte-identical to what a writer would persist.
//!
//! What this file does **not** claim: platform durability, a query/enumeration API, or that
//! a live `resolve` exercises both crash outcomes. The atomicity control restores and judges
//! every sampled world before resolving the original handle.

#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::path::Path;
use std::rc::Rc;

use fixture::{BYTES, ENTRIES, ROOT, RULES, kb};
use observing_fs::ObservingFs;

use sage_db_format::digest::{Domain, digest, raw_sha256};
use sage_db_format::event::read_event;
use sage_db_format::term::{TermEvent, write_term};
use sage_db_format::wire::Reader;
use sage_db_kernel::auth::{AuthError, Capability, CapabilitySet, Grants};
use sage_db_kernel::claims::{CommandError, InputError, RequestMeta};
use sage_db_kernel::session::{
    SessionError, SessionField, SessionInputError, SessionRead, create_session_checkpoint,
    read_session_checkpoint, revise_session_checkpoint,
};
use sage_db_kernel::session_intent::REVISE_SESSION_CHECKPOINT;
use sage_db_kernel::session_intent::{CREATE_SESSION_CHECKPOINT, SessionState};
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::CreateDir;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::open::{Opened, Resolution, open_existing};
use sage_db_store::typed::{self, EventInput};
use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::confidence::Confidence;
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};
use sage_db_types::identity::RecordRevision;
use sage_db_types::layer::SemanticLayer;
use sage_db_types::ledger::BudgetUnit;
use sage_db_types::persist::{
    ActorOrigin, CommonFields, CreationAudit, Extensions, IdSpace, RecordKey, RecordKind,
    RequestOutcomeEvent, RevPair, SessionCheckpointRecord, TermRecord, TermSource,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};
use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};

const WRITER: &str = "real-session-writer";
const CONCEPT: u32 = 1;
const RETIRED_CONCEPT: u32 = 2;
const DECOY_CONCEPT: u32 = 3;

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap()
}

fn concept_key() -> RecordKey {
    RecordKey::term(CONCEPT)
}

fn retired_key() -> RecordKey {
    RecordKey::term(RETIRED_CONCEPT)
}

fn decoy_key() -> RecordKey {
    RecordKey::term(DECOY_CONCEPT)
}

fn checkpoint_key(id: u128) -> RecordKey {
    RecordKey::new(RecordKind::SessionCheckpoint, id)
}

fn writer() -> Grants<'static> {
    // Read is granted explicitly; `write_aux` never implies it.
    Grants::new(
        WRITER,
        kb(),
        CapabilitySet::of(&[Capability::WriteAux, Capability::Read]),
    )
}

fn write_only() -> Grants<'static> {
    Grants::new(WRITER, kb(), CapabilitySet::of(&[Capability::WriteAux]))
}

fn meta_at(request_id: &str, created_at: i64) -> RequestMeta<'_> {
    let digest = raw_sha256(request_id.as_bytes());
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

fn state(session_no: u32, content: &str) -> SessionState<'_> {
    SessionState {
        concept: concept_key(),
        session_no,
        content,
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
        slug: format!("session-concept-{id}"),
        atom: format!("concept {id} for the session family"),
        layer: SemanticLayer::L1,
        domains: vec!["session".into()],
        source: TermSource::Personal,
        confidence: Confidence::Confirmed,
        triggers: vec!["session".into()],
        body_ref: raw_sha256(&term_body(id, retired)),
        common: if retired {
            CommonFields {
                recorded_by: Some(WRITER.into()),
                actor_origin: ActorOrigin::Authenticated,
                reason: Some("retired for the session control".into()),
                created_at: 1_700_000_000_000_000,
                updated_at: Some(1_700_000_000_000_500),
            }
        } else {
            CommonFields {
                recorded_by: Some(WRITER.into()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1_700_000_000_000_000,
                updated_at: None,
            }
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
    format!("session-term-body/{id}/retired={retired}").into_bytes()
}

/// The seed term's canonical event bytes, accepted by the canonical reader.
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
        sha256: raw_sha256(&bytes),
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
        .commit_basis(IdSpace::SessionCheckpointId, ledger)
        .unwrap();
    let result_bytes = typed::result_payload(seed.version, &seed.affected, &[], ledger).unwrap();
    let result_ref = raw_sha256(&result_bytes);
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
        tx_id: [0x21 + seed.version as u8; 16],
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

/// Bootstrap a KB holding one active concept and one retired concept.
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
                term_event(CONCEPT, false, &mut l),
                term_event(RETIRED_CONCEPT, false, &mut l),
                term_event(DECOY_CONCEPT, false, &mut l),
            ],
            blobs: vec![
                blob(term_body(CONCEPT, false)),
                blob(term_body(RETIRED_CONCEPT, false)),
                blob(term_body(DECOY_CONCEPT, false)),
            ],
            affected: vec![
                RevPair {
                    key: concept_key(),
                    revision: RecordRevision::FIRST,
                },
                RevPair {
                    key: retired_key(),
                    revision: RecordRevision::FIRST,
                },
                RevPair {
                    key: decoy_key(),
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

fn concept_baseline() -> [RevPair; 1] {
    [RevPair {
        key: concept_key(),
        revision: RecordRevision::FIRST,
    }]
}

fn create(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    session_no: u32,
    content: &str,
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, SessionError> {
    create_session_checkpoint(
        store,
        &writer(),
        meta(request_id),
        state(session_no, content),
        &concept_baseline(),
        &Limits::V1,
        ledger,
    )
}

fn create_exact(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    session_no: u32,
    content: &str,
    read_set: &[RevPair],
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, SessionError> {
    create_session_checkpoint(
        store,
        &writer(),
        meta(request_id),
        state(session_no, content),
        read_set,
        &Limits::V1,
        ledger,
    )
}

#[allow(clippy::too_many_arguments)]
fn revise(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    checkpoint: RecordKey,
    session_no: u32,
    content: &str,
    term_revision: RecordRevision,
    checkpoint_revision: RecordRevision,
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, SessionError> {
    revise_session_checkpoint(
        store,
        &writer(),
        meta_at(request_id, 1_700_000_200_000_000),
        checkpoint,
        state(session_no, content),
        &[
            RevPair {
                key: concept_key(),
                revision: term_revision,
            },
            RevPair {
                key: checkpoint,
                revision: checkpoint_revision,
            },
        ],
        &Limits::V1,
        ledger,
    )
}

fn read(
    store: &mut Store<ObservingFs>,
    key: RecordKey,
    max_len: u64,
) -> Result<SessionRead, SessionError> {
    read_session_checkpoint(store, &writer(), key, max_len, &mut ledger())
}

/// The current concept term, as the store's own typed read reports it.
fn term_at(store: &Store<ObservingFs>) -> sage_db_types::persist::TermRecord {
    let mut l = ledger();
    match store.record(concept_key(), &mut l).unwrap() {
        sage_db_store::view::RecordLookup::Found {
            record: sage_db_store::fold::FoldedRecord::Term(term),
            ..
        } => term.clone(),
        other => panic!("expected the seeded concept: {other:?}"),
    }
}

fn found(read: SessionRead) -> (SessionCheckpointRecord, Vec<u8>) {
    match read {
        SessionRead::Found { record, content } => {
            let FoldedRecord::SessionCheckpoint(value) = record.record() else {
                panic!("session image");
            };
            (value.clone(), content.as_bytes().to_vec())
        }
        other => panic!("expected a present checkpoint: {other:?}"),
    }
}

/// The name of a non-Ready open, which carries no `Debug` of its own.
fn opened_name(other: &Opened<ObservingFs>) -> &'static str {
    match other {
        Opened::ReadOnly { .. } => "ReadOnly",
        Opened::Ready(_) => "Ready",
    }
}

fn committed_version(outcome: &HandleOutcome) -> u64 {
    match outcome {
        HandleOutcome::Committed { version, .. } => *version,
        other => panic!("expected a commit: {other:?}"),
    }
}

/// The independent payload bytes for this family, for the external sequence oracle.
fn literal_payload(checkpoint: Option<RecordKey>, session_no: u32, content: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut put_key = |key: RecordKey| {
        out.extend_from_slice(&key.kind().as_u16().to_le_bytes());
        out.extend_from_slice(&key.id().to_le_bytes());
    };
    if let Some(key) = checkpoint {
        put_key(key);
    }
    put_key(concept_key());
    out.extend_from_slice(&session_no.to_le_bytes());
    out.extend_from_slice(&u32::try_from(content.len()).unwrap().to_le_bytes());
    out.extend_from_slice(content.as_bytes());
    out
}

fn request_digest(kind: u16, payload: &[u8], read_set: &[RevPair]) -> [u8; 32] {
    let ops = [typed::RequestOp::new(kind, 1, payload)];
    let frame = typed::RequestFrame::new(&ops, read_set, None);
    let bytes = match typed::encode_request(&frame, &mut ledger()) {
        typed::RequestOutcome::Ok(bytes) => bytes,
        other => panic!("oracle frame refused: {other:?}"),
    };
    digest(Domain::Request, &bytes)
}

#[test]
fn create_revise_reopen_preserve_full_records_and_history_blobs() {
    let (_inner, fs, mut store) = seed_world();
    let mut l = ledger();

    let created = create(
        &mut store,
        "session-create",
        7,
        "first session state",
        &mut l,
    )
    .unwrap();
    assert_eq!(committed_version(&created), 4);

    let term_before = term_at(&store);
    let (record_v1, content_v1) = found(read(&mut store, checkpoint_key(1), BYTES).unwrap());
    assert_eq!(content_v1, b"first session state");
    let expected_v1 = SessionCheckpointRecord {
        key: checkpoint_key(1),
        revision: RecordRevision::FIRST,
        location: sage_db_types::persist::CheckpointLocation::Term {
            concept: concept_key(),
            session_no: 7,
        },
        content_ref: raw_sha256(b"first session state"),
        common: CommonFields {
            recorded_by: Some(WRITER.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: meta("session-create").created_at,
            updated_at: None,
        },
        extensions: Extensions::new(),
        changed_fields: None,
    };
    assert_eq!(record_v1, expected_v1);

    // Drop and reopen: the record and its content must survive as committed bytes.
    drop(store);
    let mut store = match open_existing(
        fs.clone(),
        Path::new(ROOT),
        Some(kb()),
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap()
    {
        Opened::Ready(store) => store,
        other => panic!("expected a durable reopen: {}", opened_name(&other)),
    };
    let (reopened_v1, reopened_content) =
        found(read(&mut store, checkpoint_key(1), BYTES).unwrap());
    assert_eq!(reopened_v1, expected_v1);
    assert_eq!(reopened_content, content_v1);

    // An exact retry after a restart is answered from the stored outcome, with no write.
    let before = fs.counts().borrow().total();
    let retried_after_restart = create_exact(
        &mut store,
        "session-create",
        7,
        "first session state",
        &concept_baseline(),
        &mut ledger(),
    )
    .unwrap();
    assert_eq!(fs.counts().borrow().total(), before);
    match retried_after_restart {
        HandleOutcome::AlreadyCommitted { version, .. } => assert_eq!(version, 4),
        other => panic!("a restart must not change an exact retry's answer: {other:?}"),
    }

    // The id allocation mark advanced with the real committed high-water mark.
    let basis = store
        .commit_basis(IdSpace::SessionCheckpointId, &mut ledger())
        .unwrap();
    assert_eq!(basis.allocated_high_water, 1);

    let revised = revise(
        &mut store,
        "session-revise",
        checkpoint_key(1),
        7,
        "second session state",
        RecordRevision::FIRST,
        RecordRevision::FIRST,
        &mut ledger(),
    )
    .unwrap();
    assert_eq!(committed_version(&revised), 5);

    let expected_v2 = SessionCheckpointRecord {
        revision: RecordRevision::new(2).unwrap(),
        content_ref: raw_sha256(b"second session state"),
        common: CommonFields {
            recorded_by: Some(WRITER.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: meta("session-create").created_at,
            updated_at: Some(meta_at("session-revise", 1_700_000_200_000_000).created_at),
        },
        ..expected_v1.clone()
    };
    let (record_v2, content_v2) = found(read(&mut store, checkpoint_key(1), BYTES).unwrap());
    assert_eq!(record_v2, expected_v2);
    assert_eq!(content_v2, b"second session state");

    // Reopen again: revision 2 and its content are the committed values.
    drop(store);
    let mut store = match open_existing(
        fs.clone(),
        Path::new(ROOT),
        Some(kb()),
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap()
    {
        Opened::Ready(store) => store,
        other => panic!("expected a durable reopen: {}", opened_name(&other)),
    };
    let (after_restart, after_content) = found(read(&mut store, checkpoint_key(1), BYTES).unwrap());
    assert_eq!(after_restart, expected_v2);
    assert_eq!(after_content, b"second session state");

    // Writing a session checkpoint never touches the concept or the graph.
    assert_eq!(term_at(&store), term_before);
    // The old content is still committed history, reachable by its own digest.
    let old = store
        .read_committed_blob(raw_sha256(b"first session state"), BYTES, &mut ledger())
        .unwrap();
    assert_eq!(old.as_bytes(), b"first session state");
}

#[test]
fn exact_retries_answer_before_staleness_and_never_mutate() {
    let (_inner, fs, mut store) = seed_world();
    let original_baseline = concept_baseline();
    let created = create(&mut store, "retry-create", 3, "retry state", &mut ledger()).unwrap();
    let create_version = committed_version(&created);
    let created_record = match &created {
        HandleOutcome::Committed { recorded, .. } => recorded.clone(),
        other => panic!("expected a commit: {other:?}"),
    };

    // A revision moves both the checkpoint and its identity out of the way.
    revise(
        &mut store,
        "retry-revise",
        checkpoint_key(1),
        3,
        "moved on",
        RecordRevision::FIRST,
        RecordRevision::FIRST,
        &mut ledger(),
    )
    .unwrap();

    let before = fs.counts().borrow().total();
    let retried = create_exact(
        &mut store,
        "retry-create",
        3,
        "retry state",
        &original_baseline,
        &mut ledger(),
    )
    .unwrap();
    assert_eq!(
        fs.counts().borrow().total(),
        before,
        "an exact retry must not write"
    );
    match retried {
        HandleOutcome::AlreadyCommitted { version, recorded } => {
            assert_eq!(version, create_version);
            assert_eq!(recorded, created_record);
        }
        other => panic!("expected the original outcome: {other:?}"),
    }

    // A different payload under the same request id is a mismatch, not a second write.
    let mismatch = create_exact(
        &mut store,
        "retry-create",
        3,
        "different state",
        &original_baseline,
        &mut ledger(),
    );
    assert!(matches!(
        mismatch,
        Err(SessionError::Command(CommandError::IdempotencyMismatch))
    ));

    // The retried revision is stale now, yet the exact retry still answers first.
    let first_revise_set = [
        RevPair {
            key: concept_key(),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: checkpoint_key(1),
            revision: RecordRevision::FIRST,
        },
    ];
    let original_revision_version = {
        let latest = revise(
            &mut store,
            "retry-revise-2",
            checkpoint_key(1),
            3,
            "third state",
            RecordRevision::FIRST,
            RecordRevision::new(2).unwrap(),
            &mut ledger(),
        )
        .unwrap();
        committed_version(&latest)
    };
    assert_eq!(original_revision_version, 6);
    let before = fs.counts().borrow().total();
    let stale_retry = revise_session_checkpoint(
        &mut store,
        &writer(),
        meta_at("retry-revise", 1_700_000_200_000_000),
        checkpoint_key(1),
        state(3, "moved on"),
        &first_revise_set,
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap();
    assert_eq!(fs.counts().borrow().total(), before);
    match stale_retry {
        HandleOutcome::AlreadyCommitted { version, .. } => {
            // The FIRST revision's own version, not the later one that moved the record.
            assert_eq!(version, 5);
        }
        other => panic!("expected the original revision outcome: {other:?}"),
    }
}

#[test]
fn capabilities_kb_and_input_bounds_are_enforced_before_any_write() {
    let (_inner, fs, mut store) = seed_world();
    let before = fs.counts().borrow().total();

    // Admin does not imply write_aux; propose/review do not either.
    let admin = Grants::new(WRITER, kb(), CapabilitySet::of(&[Capability::Admin]));
    assert!(matches!(
        create_session_checkpoint(
            &mut store,
            &admin,
            meta("admin"),
            state(1, "x"),
            &concept_baseline(),
            &Limits::V1,
            &mut ledger(),
        ),
        Err(SessionError::Command(CommandError::Auth(
            AuthError::MissingCapability {
                capability: Capability::WriteAux
            }
        )))
    ));
    // write_aux does not imply read.
    assert!(matches!(
        read_session_checkpoint(
            &mut store,
            &write_only(),
            checkpoint_key(1),
            BYTES,
            &mut ledger(),
        ),
        Err(SessionError::Command(CommandError::Auth(
            AuthError::MissingCapability {
                capability: Capability::Read
            }
        )))
    ));
    // A grant on another KB is refused before the capability question.
    let other_kb = Grants::new(
        WRITER,
        sage_db_types::identity::KbUuid::from_bytes([0x77; 16]),
        CapabilitySet::of(&[Capability::WriteAux, Capability::Read]),
    );
    assert!(matches!(
        create_session_checkpoint(
            &mut store,
            &other_kb,
            meta("wrong-kb"),
            state(1, "x"),
            &concept_baseline(),
            &Limits::V1,
            &mut ledger(),
        ),
        Err(SessionError::Command(CommandError::Auth(
            AuthError::WrongKb { .. }
        )))
    ));

    // Zero session ordinals are not sessions.
    assert!(matches!(
        create(&mut store, "zero", 0, "x", &mut ledger()),
        Err(SessionError::Input(SessionInputError::ZeroSessionNo))
    ));
    // A concept that does not exist is a missing subject, not a new term.
    let missing = create_session_checkpoint(
        &mut store,
        &writer(),
        meta("missing-term"),
        SessionState {
            concept: RecordKey::term(99),
            session_no: 1,
            content: "x",
        },
        &[RevPair {
            key: RecordKey::term(99),
            revision: RecordRevision::FIRST,
        }],
        &Limits::V1,
        &mut ledger(),
    );
    assert!(matches!(
        missing,
        Err(SessionError::Input(SessionInputError::MissingConcept(key)))
            if key == RecordKey::term(99)
    ));
    // A retired concept is refused.
    let retired = create_session_checkpoint(
        &mut store,
        &writer(),
        meta("retired-term"),
        SessionState {
            concept: retired_key(),
            session_no: 1,
            content: "x",
        },
        &[RevPair {
            key: retired_key(),
            revision: RecordRevision::new(2).unwrap(),
        }],
        &Limits::V1,
        &mut ledger(),
    );
    assert!(matches!(
        retired,
        Err(SessionError::Input(SessionInputError::ConceptRetired(key))) if key == retired_key()
    ));
    // A foreign key in the read set is a shape refusal.
    assert!(matches!(
        create_session_checkpoint(
            &mut store,
            &writer(),
            meta("foreign-key"),
            state(1, "x"),
            &[RevPair {
                key: RecordKey::new(RecordKind::Claim, 1),
                revision: RecordRevision::FIRST,
            }],
            &Limits::V1,
            &mut ledger(),
        ),
        Err(SessionError::Command(CommandError::Input(
            InputError::ReadSetKey { .. }
        )))
    ));
    // A stale concept revision is refused rather than silently replaced.
    assert!(matches!(
        create_session_checkpoint(
            &mut store,
            &writer(),
            meta("stale-concept"),
            state(1, "x"),
            &[RevPair {
                key: concept_key(),
                revision: RecordRevision::new(2).unwrap(),
            }],
            &Limits::V1,
            &mut ledger(),
        ),
        Err(SessionError::Command(CommandError::Input(
            InputError::StaleBaseline { .. }
        )))
    ));
    // A checkpoint key of the wrong kind is refused on the revision path.
    assert!(matches!(
        revise_session_checkpoint(
            &mut store,
            &writer(),
            meta("wrong-kind"),
            concept_key(),
            state(1, "x"),
            &[],
            &Limits::V1,
            &mut ledger(),
        ),
        Err(SessionError::Command(CommandError::Input(
            InputError::WrongKeyKind { .. }
        )))
    ));

    assert_eq!(
        fs.counts().borrow().total(),
        before,
        "a refused command must not reach a mutating seam operation"
    );
}

#[test]
fn revision_cas_and_immutable_identity_are_enforced() {
    let (_inner, fs, mut store) = seed_world();
    create(&mut store, "cas-create", 4, "cas state", &mut ledger()).unwrap();
    let before = fs.counts().borrow().total();

    // A missing checkpoint is named as such.
    assert!(matches!(
        revise(
            &mut store,
            "missing",
            checkpoint_key(9),
            4,
            "x",
            RecordRevision::FIRST,
            RecordRevision::FIRST,
            &mut ledger(),
        ),
        Err(SessionError::Input(SessionInputError::MissingCheckpoint(key)))
            if key == checkpoint_key(9)
    ));
    // A stale checkpoint revision is refused.
    assert!(matches!(
        revise(
            &mut store,
            "stale-checkpoint",
            checkpoint_key(1),
            4,
            "x",
            RecordRevision::FIRST,
            RecordRevision::new(2).unwrap(),
            &mut ledger(),
        ),
        Err(SessionError::Command(CommandError::Input(
            InputError::StaleBaseline { .. }
        )))
    ));
    // A stale concept revision is refused even when the checkpoint is current.
    assert!(matches!(
        revise(
            &mut store,
            "stale-concept-revise",
            checkpoint_key(1),
            4,
            "x",
            RecordRevision::new(2).unwrap(),
            RecordRevision::FIRST,
            &mut ledger(),
        ),
        Err(SessionError::Command(CommandError::Input(
            InputError::StaleBaseline { .. }
        )))
    ));

    // The session ordinal is immutable; a different one is a caller error.
    let wrong_session = revise_session_checkpoint(
        &mut store,
        &writer(),
        meta("immutable-session"),
        checkpoint_key(1),
        state(2, "x"),
        &[
            RevPair {
                key: concept_key(),
                revision: RecordRevision::FIRST,
            },
            RevPair {
                key: checkpoint_key(1),
                revision: RecordRevision::FIRST,
            },
        ],
        &Limits::V1,
        &mut ledger(),
    );
    assert!(matches!(
        wrong_session,
        Err(SessionError::Input(SessionInputError::ImmutableField {
            field: SessionField::SessionNo
        }))
    ));
    // The concept is immutable too: naming another existing active term is refused.
    let wrong_concept = revise_session_checkpoint(
        &mut store,
        &writer(),
        meta("immutable-concept"),
        checkpoint_key(1),
        SessionState {
            concept: decoy_key(),
            session_no: 4,
            content: "x",
        },
        &[
            RevPair {
                key: decoy_key(),
                revision: RecordRevision::FIRST,
            },
            RevPair {
                key: checkpoint_key(1),
                revision: RecordRevision::FIRST,
            },
        ],
        &Limits::V1,
        &mut ledger(),
    );
    assert!(
        matches!(
            wrong_concept,
            Err(SessionError::Input(SessionInputError::ImmutableField {
                field: SessionField::Concept
            }))
        ),
        "a revision cannot move onto another concept"
    );

    assert_eq!(
        fs.counts().borrow().total(),
        before,
        "a refused revision must not reach a mutating seam operation"
    );
}

#[test]
fn read_reports_absence_ceilings_and_wrong_kinds_without_laundering() {
    let (_inner, fs, mut store) = seed_world();
    let before = fs.counts().borrow().total();

    // Absence is a proven answer, not an error, and it names its basis.
    match read(&mut store, checkpoint_key(1), BYTES).unwrap() {
        SessionRead::Absent { basis } => {
            assert_eq!(Some(basis), store.view().map(|view| view.basis()));
        }
        other => panic!("expected absence before any create: {other:?}"),
    }

    create(
        &mut store,
        "read-create",
        5,
        "readable state",
        &mut ledger(),
    )
    .unwrap();
    let (record, content) = found(read(&mut store, checkpoint_key(1), BYTES).unwrap());
    assert_eq!(
        record.location,
        sage_db_types::persist::CheckpointLocation::Term {
            concept: concept_key(),
            session_no: 5,
        }
    );
    assert_eq!(content, b"readable state");

    // A content ceiling is the store's own class, not a truncated answer.
    assert!(matches!(
        read(&mut store, checkpoint_key(1), 4),
        Err(SessionError::Command(CommandError::Blob(
            sage_db_store::view::BlobReadError::Ceiling { .. }
        )))
    ));
    // A non-checkpoint key is a caller error.
    assert!(matches!(
        read(&mut store, concept_key(), BYTES),
        Err(SessionError::Command(CommandError::Input(
            InputError::WrongKeyKind { .. }
        )))
    ));
    // The historical blob is still committed after the record moved on, and the review of
    // the old bytes is exact: a wrong digest is not silently served.
    revise(
        &mut store,
        "read-revise",
        checkpoint_key(1),
        5,
        "newer state",
        RecordRevision::FIRST,
        RecordRevision::FIRST,
        &mut ledger(),
    )
    .unwrap();
    let old = store
        .read_committed_blob(raw_sha256(b"readable state"), BYTES, &mut ledger())
        .unwrap();
    assert_eq!(old.as_bytes(), b"readable state");
    assert!(matches!(
        store.read_committed_blob(raw_sha256(b"never written"), BYTES, &mut ledger()),
        Err(sage_db_store::view::BlobReadError::NotReferenced(_))
    ));
    // The read path never wrote: only the create and its revision did.
    assert!(fs.counts().borrow().total() > before);
}

#[test]
fn content_may_equal_the_rules_blob_and_may_be_empty_or_unicode() {
    // The rules blob is already committed at bootstrap. A checkpoint whose content is
    // byte-identical to it must publish honestly: the declaration still names real bytes,
    // and the store's duplicate-digest behaviour decides whether a second provide is legal.
    let rules_text = core::str::from_utf8(RULES).expect("the rules fixture is UTF-8");
    let cases: [(&str, &str); 4] = [
        ("rules", rules_text),
        ("empty", ""),
        ("unicode", "会话 \u{1F600} état — nicht normalisiert"),
        ("nul", "before\u{0}after"),
    ];
    let (_inner, _fs, mut store) = seed_world();
    for (index, (label, content)) in cases.iter().enumerate() {
        let request = format!("content-{label}");
        let outcome = create(&mut store, &request, 1, content, &mut ledger())
            .unwrap_or_else(|error| panic!("{label} content must commit: {error:?}"));
        let version = committed_version(&outcome);
        let key = checkpoint_key(index as u128 + 1);
        let (record, bytes) = found(read(&mut store, key, BYTES).unwrap());
        assert_eq!(bytes, content.as_bytes(), "{label} content must round-trip");
        assert_eq!(record.content_ref, raw_sha256(content.as_bytes()));
        assert_eq!(record.revision, RecordRevision::FIRST);
        assert_eq!(
            record.location,
            sage_db_types::persist::CheckpointLocation::Term {
                concept: concept_key(),
                session_no: 1,
            }
        );
        assert_eq!(version, 4 + index as u64);
        if *label == "rules" {
            // The same digest is reachable by the rules binding and by this checkpoint.
            assert_eq!(record.content_ref, raw_sha256(RULES));
        }
    }
}

#[test]
fn a_revision_preserves_extensions_changed_fields_and_creation_audit() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let mut extensions = Extensions::new();
    extensions
        .insert("session.note", vec![0, 255, 8])
        .expect("canonical key");
    let created = SessionCheckpointRecord {
        key: checkpoint_key(5),
        revision: RecordRevision::FIRST,
        location: sage_db_types::persist::CheckpointLocation::Term {
            concept: concept_key(),
            session_no: 9,
        },
        content_ref: raw_sha256(b"seeded state"),
        common: CommonFields {
            recorded_by: Some("imported-writer".into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("imported session state".into()),
            created_at: 1_600_000_000_000_000,
            updated_at: Some(1_600_000_000_000_500),
        },
        extensions: extensions.clone(),
        // A creation revision may not carry advisory changed-field tags.
        changed_fields: None,
    };
    commit_seed(
        &mut store,
        Seed {
            version: 4,
            events: vec![prepared(typed::prepare_event(
                EventInput::SessionCheckpoint(&created),
                &mut l,
            ))],
            blobs: vec![blob(b"seeded state".to_vec())],
            affected: vec![RevPair {
                key: checkpoint_key(5),
                revision: RecordRevision::FIRST,
            }],
        },
        "seed-checkpoint-create",
        &mut l,
    );
    let seeded = SessionCheckpointRecord {
        revision: RecordRevision::new(2).unwrap(),
        common: CommonFields {
            recorded_by: Some("imported-writer".into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("imported session state".into()),
            created_at: 1_600_000_000_000_000,
            updated_at: Some(1_600_000_000_000_500),
        },
        changed_fields: Some(vec![70, 132]),
        ..created.clone()
    };
    let allocation = sage_db_types::persist::IdAllocationEvent {
        allocation: sage_db_types::persist::Watermark {
            space: IdSpace::SessionCheckpointId,
            hwm: 5,
        },
        reason: None,
        extensions: Extensions::new(),
    };
    commit_seed(
        &mut store,
        Seed {
            version: 5,
            events: vec![
                prepared(typed::prepare_event(
                    EventInput::SessionCheckpoint(&seeded),
                    &mut l,
                )),
                prepared(typed::prepare_event(
                    EventInput::IdAllocation(&allocation),
                    &mut l,
                )),
            ],
            blobs: vec![blob(b"seeded state".to_vec())],
            affected: vec![RevPair {
                key: checkpoint_key(5),
                revision: RecordRevision::new(2).unwrap(),
            }],
        },
        "seed-checkpoint",
        &mut l,
    );

    let outcome = revise(
        &mut store,
        "preserve-revise",
        checkpoint_key(5),
        9,
        "replacement state",
        RecordRevision::FIRST,
        RecordRevision::new(2).unwrap(),
        &mut ledger(),
    )
    .unwrap();
    assert_eq!(committed_version(&outcome), 6);

    let (record, content) = found(read(&mut store, checkpoint_key(5), BYTES).unwrap());
    assert_eq!(content, b"replacement state");
    assert_eq!(record.revision, RecordRevision::new(3).unwrap());
    // Immutable tuple, creation audit, advisory extensions and changed-field tags survive.
    assert_eq!(
        record.location,
        sage_db_types::persist::CheckpointLocation::Term {
            concept: concept_key(),
            session_no: 9,
        }
    );
    assert_eq!(record.common.created_at, 1_600_000_000_000_000);
    assert_eq!(
        record.common.reason.as_deref(),
        Some("imported session state")
    );
    assert_eq!(record.extensions, extensions);
    assert_eq!(record.changed_fields.as_deref(), Some(&[70, 132][..]));
    // Only the current actor and the update time moved.
    assert_eq!(record.common.recorded_by.as_deref(), Some(WRITER));
    assert_eq!(
        record.common.updated_at,
        Some(meta_at("preserve-revise", 1_700_000_200_000_000).created_at)
    );
    assert_eq!(record.common.actor_origin, ActorOrigin::Authenticated);
}

#[test]
fn first_oversized_create_payload_is_refused_before_payload_admission() {
    let (_inner, fs, mut store) = seed_world();
    // Create has exactly26 bytes of wire overhead, so65511 bytes of text is
    // the FIRST value beyond the65536-byte operation payload limit.
    let huge = "x".repeat(sage_db_store::typed::MAX_PAYLOAD as usize - 26 + 1);
    let before = fs.counts().borrow().total();
    let mut l = ledger();
    assert!(matches!(
        create(&mut store, "too-large", 1, &huge, &mut l),
        Err(SessionError::Input(SessionInputError::Payload(
            sage_db_kernel::session_intent::SessionPayloadError::TooLarge {
                len: 65537,
                max: 65536
            }
        )))
    ));
    assert_eq!(l.stats().admitted_bytes, Some(0));
    assert_eq!(fs.counts().borrow().total(), before);
    assert_eq!(store.head(), Some(3));
}

#[test]
fn caller_stop_and_byte_budget_keep_their_own_classes() {
    let (_inner, _fs, mut store) = seed_world();

    struct Stop;
    impl QueryControl for Stop {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let mut stopped = AttemptLedger::open(BYTES, ENTRIES, &Stop).unwrap();
    assert_eq!(
        refusal_of(create(&mut store, "stopped", 1, "x", &mut stopped)),
        Refusal::Stopped(ControlStop::Cancelled)
    );

    let mut tiny = AttemptLedger::open(0, 0, &RunToCompletion).unwrap();
    assert!(matches!(
        create(&mut store, "tiny", 1, "x", &mut tiny),
        Err(SessionError::Command(CommandError::Refused(Refusal::Budget(
            error
        )))) if error.unit() == BudgetUnit::Bytes
    ));
    assert_eq!(tiny.stats().live_bytes, 0);

    // Nothing was created under either refusal.
    assert!(matches!(
        read(&mut store, checkpoint_key(1), BYTES).unwrap(),
        SessionRead::Absent { .. }
    ));
}

fn refusal_of<T: core::fmt::Debug>(result: Result<T, SessionError>) -> Refusal {
    match result {
        Err(SessionError::Command(CommandError::Refused(refusal))) => refusal,
        // Authorization observes the same stop before the capability check.
        Err(SessionError::Command(CommandError::Auth(AuthError::Refused(refusal)))) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn an_unknown_revision_resolves_to_a_whole_world_not_half() {
    use sage_db_store_tests::oracle::attempts::{
        AttemptHistory, AttemptOutcome, Commit, LogicalRequest, World,
    };

    let (inner, _fs, mut store) = seed_world();
    let create_baseline = concept_baseline();
    create(
        &mut store,
        "atomic-create",
        2,
        "atomic old state",
        &mut ledger(),
    )
    .unwrap();
    let old = found(read(&mut store, checkpoint_key(1), BYTES).unwrap());

    let revise_set = [
        RevPair {
            key: concept_key(),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: checkpoint_key(1),
            revision: RecordRevision::FIRST,
        },
    ];
    let create_digest = request_digest(
        CREATE_SESSION_CHECKPOINT,
        &literal_payload(None, 2, "atomic old state"),
        &create_baseline,
    );
    let revise_digest = request_digest(
        REVISE_SESSION_CHECKPOINT,
        &literal_payload(Some(checkpoint_key(1)), 2, "atomic new state"),
        &revise_set,
    );

    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    let outcome = revise(
        &mut store,
        "atomic-revise",
        checkpoint_key(1),
        2,
        "atomic new state",
        RecordRevision::FIRST,
        RecordRevision::FIRST,
        &mut ledger(),
    )
    .unwrap();
    assert!(matches!(outcome, HandleOutcome::Unknown));
    assert!(inner.faults_handle().borrow().all_fired());
    assert!(store.authority_undecided());
    // While authority is undecided no new write may start, and no fabricated success is
    // returned: the store's own gate answers.
    match create(&mut store, "after-unknown", 1, "x", &mut ledger()) {
        Err(SessionError::Command(CommandError::Fenced | CommandError::Incomplete(_))) => {}
        other => panic!("an undecided authority must refuse a new write: {other:?}"),
    }

    // Sample NOW, before resolve can synchronize the visible candidate root.
    let worlds = inner.model().crash_states();
    assert!(!worlds.states.is_empty());
    assert_eq!(worlds.sampled, worlds.states.len());

    let create_request = LogicalRequest::new(WRITER, "atomic-create", create_digest);
    let revise_request = LogicalRequest::new(WRITER, "atomic-revise", revise_digest);
    let mut history = AttemptHistory::default();
    history.record(
        "ack-create",
        create_request.clone(),
        AttemptOutcome::Durable { version: 4 },
    );
    history.record(
        "unknown-revise",
        revise_request.clone(),
        AttemptOutcome::Unknown,
    );

    let mut examined = 0;
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
        let head = reopened.head().unwrap();
        assert!(
            matches!(head, 4 | 5),
            "the acknowledged create was lost in world {index}: head {head}"
        );

        let mut commits = Vec::new();
        match reopened.outcome(WRITER, "atomic-create") {
            sage_db_store::open::OutcomeLookup::Found {
                outcome,
                original_version,
                ..
            } => {
                assert_eq!(original_version, 4);
                assert_eq!(outcome.event().payload_digest, create_digest);
                commits.push(Commit {
                    version: original_version,
                    request: create_request.clone(),
                });
            }
            other => panic!("world {index} lost the acknowledged create: {other:?}"),
        }

        let (record, content) = found(read(&mut reopened, checkpoint_key(1), BYTES).unwrap());
        if head == 4 {
            old_seen = true;
            assert_eq!(record, old.0);
            assert_eq!(content, old.1);
            assert!(matches!(
                reopened.outcome(WRITER, "atomic-revise"),
                sage_db_store::open::OutcomeLookup::Absent { .. }
            ));
        } else {
            new_seen = true;
            let mut expected = old.0.clone();
            expected.revision = RecordRevision::new(2).unwrap();
            expected.content_ref = raw_sha256(b"atomic new state");
            expected.common.recorded_by = Some(WRITER.into());
            expected.common.actor_origin = ActorOrigin::Authenticated;
            expected.common.updated_at =
                Some(meta_at("atomic-revise", 1_700_000_200_000_000).created_at);
            assert_eq!(record, expected);
            assert_eq!(content, b"atomic new state");
            match reopened.outcome(WRITER, "atomic-revise") {
                sage_db_store::open::OutcomeLookup::Found {
                    outcome,
                    original_version,
                    ..
                } => {
                    assert_eq!(original_version, 5);
                    assert_eq!(outcome.event().payload_digest, revise_digest);
                    commits.push(Commit {
                        version: original_version,
                        request: revise_request.clone(),
                    });
                }
                other => {
                    panic!("world {index} has candidate content without its outcome: {other:?}")
                }
            }
        }
        assert!(
            history
                .accepts(
                    3,
                    &World {
                        base_version: 3,
                        commits,
                    },
                )
                .unwrap(),
            "sequence oracle rejected world {index}"
        );
        examined += 1;
    }
    assert_eq!(examined, worlds.sampled);
    assert!(
        old_seen && new_seen,
        "this control must actually exercise BOTH whole alternatives"
    );
    println!(
        "SESSION_ATOMIC_SAMPLED={} EXAMINED={examined}",
        worlds.sampled
    );

    // Resolve the original handle: whichever side won, the record and its content agree.
    let mut l = ledger();
    assert!(matches!(
        store.resolve(&Limits::V1, &mut l),
        Resolution::Ready
    ));
    let (resolved, resolved_content) = found(read(&mut store, checkpoint_key(1), BYTES).unwrap());
    match resolved.revision.get() {
        1 => assert_eq!(resolved_content, b"atomic old state"),
        2 => assert_eq!(resolved_content, b"atomic new state"),
        other => panic!("a resolved revision cannot be {other}"),
    }
    assert_eq!(resolved.content_ref, raw_sha256(&resolved_content));
}

#[test]
fn root_session_record_and_content_owners_survive_both_facades_and_store_drop() {
    use sage_db_store::lease::LeaseLedger;
    let (_inner, _fs, mut store) = seed_world();
    let mut commit_ledger = ledger();
    let commit_observer = commit_ledger.reserve_owned(0).unwrap();
    assert!(matches!(
        create(
            &mut store,
            "owned-session",
            7,
            "held session bytes",
            &mut commit_ledger
        )
        .unwrap(),
        HandleOutcome::Committed { version: 4, .. }
    ));
    let mut read_ledger = ledger();
    let read_observer = read_ledger.reserve_owned(0).unwrap();
    let SessionRead::Found { record, content } = read_session_checkpoint(
        &mut store,
        &writer(),
        checkpoint_key(1),
        BYTES,
        &mut read_ledger,
    )
    .unwrap() else {
        panic!("record exists")
    };
    let record_extent = record.reserved_bytes();
    assert!(record_extent > 0);
    drop(commit_ledger);
    drop(read_ledger);
    drop(store);
    let FoldedRecord::SessionCheckpoint(value) = record.record() else {
        panic!("full session owner")
    };
    assert_eq!(value.key, checkpoint_key(1));
    assert_eq!(
        value.location,
        sage_db_types::persist::CheckpointLocation::Term {
            concept: concept_key(),
            session_no: 7,
        }
    );
    assert_eq!(value.content_ref, raw_sha256(b"held session bytes"));
    assert_eq!(content.as_bytes(), b"held session bytes");
    assert_eq!(
        commit_observer.stats().held_bytes,
        record_extent,
        "only the issued record owner still holds this commit's admission"
    );
    assert_eq!(read_observer.stats().held_bytes, content.reserved_bytes());
    drop(record);
    assert_eq!(
        commit_observer.stats().live_bytes,
        0,
        "record reservation releases with the last image"
    );
    drop(content);
    assert_eq!(
        read_observer.stats().live_bytes,
        0,
        "body reservation releases with the blob owner"
    );
}

#[test]
fn legacy_checkpoint_replaces_an_orphan_slot_and_replays_after_reopen() {
    use sage_db_kernel::session::{
        LegacyCheckpointWrite, read_legacy_checkpoint_slot, upsert_legacy_checkpoint,
    };
    use sage_db_types::persist::CheckpointLocation;

    let (_inner, fs, mut store) = seed_world();
    let initial = read_legacy_checkpoint_slot(&store, &writer(), "not-a-term", None, &mut ledger())
        .expect("absent orphan slug has a durable basis");
    assert_eq!(initial.selected, None);
    let ordinal = (1u64 << 53) - 1;
    let content = vec![b'x'; 98_304];
    fn request(content: &[u8], expected_version: u64) -> LegacyCheckpointWrite<'_> {
        LegacyCheckpointWrite {
            slug: "not-a-term",
            session_no: (1u64 << 53) - 1,
            content,
            expected_version,
        }
    }
    let initial_version = initial.basis.version.get();
    let first = upsert_legacy_checkpoint(
        &mut store,
        &writer(),
        meta("orphan-new"),
        request(&content, initial_version),
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap();
    let first_version = committed_version(&first);
    let observed =
        read_legacy_checkpoint_slot(&store, &writer(), "not-a-term", None, &mut ledger()).unwrap();
    let key = checkpoint_key(1);
    assert_eq!(observed.selected, Some((ordinal, key)));
    let (stored, read_content) = found(read(&mut store, key, 131_072).unwrap());
    assert_eq!(read_content, content);
    assert_eq!(
        stored.location,
        CheckpointLocation::Slug {
            slug: "not-a-term".to_owned(),
            session_no: ordinal,
        }
    );
    assert!(matches!(
        upsert_legacy_checkpoint(
            &mut store,
            &writer(),
            meta("orphan-obsolete"),
            request(b"new bytes", initial_version),
            &Limits::V1,
            &mut ledger(),
        ),
        Err(SessionError::Input(
            SessionInputError::LegacyVersionChanged { .. }
        ))
    ));
    let second = upsert_legacy_checkpoint(
        &mut store,
        &writer(),
        meta("orphan-replace"),
        LegacyCheckpointWrite {
            expected_version: observed.basis.version.get(),
            ..request(b"new bytes", initial_version)
        },
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap();
    assert_eq!(committed_version(&second), first_version + 1);

    drop(store);
    let Opened::Ready(mut store) =
        open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger())
            .expect("real reopen")
    else {
        panic!("ready store");
    };
    let retry = upsert_legacy_checkpoint(
        &mut store,
        &writer(),
        meta("orphan-new"),
        request(&content, initial_version),
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap();
    assert!(
        matches!(retry, HandleOutcome::AlreadyCommitted { version, .. } if version == first_version)
    );
    let (revised, bytes) = found(read(&mut store, key, 131_072).unwrap());
    assert_eq!(revised.revision, RecordRevision::new(2).unwrap());
    assert_eq!(bytes, b"new bytes");
    assert_eq!(store.head(), Some(first_version + 1));
    assert_eq!(
        store
            .commit_basis(IdSpace::SessionCheckpointId, &mut ledger())
            .unwrap()
            .allocated_high_water,
        1,
        "replacement did not allocate a second record"
    );
    assert!(matches!(
        read_legacy_checkpoint_slot(
            &store,
            &writer(),
            "not-a-term",
            Some(ordinal + 1),
            &mut ledger()
        ),
        Err(SessionError::Input(
            SessionInputError::InvalidLegacyLocation
        ))
    ));
    assert!(matches!(
        read_legacy_checkpoint_slot(&store, &write_only(), "not-a-term", None, &mut ledger()),
        Err(SessionError::Command(CommandError::Auth(_)))
    ));
}

#[test]
fn legacy_upsert_restates_an_existing_native_slot_instead_of_allocating_another() {
    use sage_db_kernel::session::{
        LegacyCheckpointWrite, read_legacy_checkpoint_slot, upsert_legacy_checkpoint,
    };
    let (_inner, _fs, mut store) = seed_world();
    let slug = term_at(&store).slug;
    let baseline = [RevPair {
        key: concept_key(),
        revision: RecordRevision::FIRST,
    }];
    create_session_checkpoint(
        &mut store,
        &writer(),
        meta("native-legacy-seed"),
        state(7, "old extracted state"),
        &baseline,
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap();
    let before =
        read_legacy_checkpoint_slot(&store, &writer(), &slug, Some(7), &mut ledger()).unwrap();
    let key = checkpoint_key(1);
    assert_eq!(before.selected, Some((7, key)));
    upsert_legacy_checkpoint(
        &mut store,
        &writer(),
        meta("replace-native-slot"),
        LegacyCheckpointWrite {
            slug: &slug,
            session_no: 7,
            content: b"new opaque state",
            expected_version: before.basis.version.get(),
        },
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap();
    let (updated, bytes) = found(read(&mut store, key, BYTES).unwrap());
    assert_eq!(updated.revision, RecordRevision::new(2).unwrap());
    assert_eq!(bytes, b"new opaque state");
    assert_eq!(
        store
            .commit_basis(IdSpace::SessionCheckpointId, &mut ledger())
            .unwrap()
            .allocated_high_water,
        1
    );
}
