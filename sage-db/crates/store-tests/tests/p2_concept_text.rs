//! P2 concept-text lifecycle on the real public Store and the model backend.
//!
//! Every command here goes through the production entry points — `bootstrap`,
//! `Store::commit`, `open_existing`, `submit_concept_text`, `review_concept_text` — with the
//! real ledger, the real canonical codecs and a real fault injector. Seed rows are built
//! through the canonical `typed` bridge, so a seeded term is byte-identical to what a writer
//! would persist.
//!
//! # What this file does not claim
//!
//! Platform durability. `HandleOutcome::Unknown` is returned as the ambiguous attempt it is,
//! and the one Unknown test restores **every** sampled world and judges both the term and the
//! proposal against `AttemptHistory`; a world that fails to restore is a panic, never a skip.
//! Test-local allocations are not allocator evidence.

#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
// The support backend is shared with other targets that use the parts this file does not.
#[allow(dead_code)]
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::path::Path;
use std::rc::Rc;

use fixture::{BYTES, ENTRIES, ROOT, RULES, kb};
use observing_fs::ObservingFs;

use sage_db_format::digest::raw_sha256;
use sage_db_kernel::auth::{AuthError, Capability, CapabilitySet, Grants};
use sage_db_kernel::claims::{CommandError, InputError, RequestMeta};
use sage_db_kernel::concept::{
    ConceptTextError, ConceptTextInputError, review_concept_text, submit_concept_text,
};
use sage_db_kernel::concept_intent::{
    self, ConceptTextEdit, ConceptTextPayloadError, DECISION_APPROVE, DECISION_REJECT, MASK_ATOM,
    MASK_BODY, encode_submit,
};
use sage_db_store::fold::FoldedRecord;
use sage_db_store::fs::CreateDir;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;

use sage_db_store::open::open_existing;
use sage_db_store::typed::{self, EventInput};
use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::framing::{FormatOutcome, Limits, Refusal};
use sage_db_types::identity::{KbUuid, RecordRevision};
use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::persist::{
    ActorOrigin, CommonFields, CreationAudit, Extensions, IdAllocationEvent, IdSpace, ProposalKind,
    ProposalRecord, ProposalStatus, RecordKey, RecordKind, RequestOutcomeEvent, RevPair,
    TermRecord, Watermark,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};
use sage_db_types::query::{ControlStop, RunToCompletion};

/// The executor's finite resource policy for re-reading an *unchanged* committed body.
///
/// Deliberately generous here: this suite's bodies are small, and the point of the parameter is
/// that it comes from the caller rather than from the request frame ceiling.
const MAX_EXISTING_BODY: u64 = 4 * 1024 * 1024;

const PRINCIPAL: &str = "concept-author";
const REVIEWER: &str = "concept-reviewer";
const OTHER_KB: [u8; 16] = [0x77; 16];

// ---------------------------------------------------------------------------
// Ledger doubles
// ---------------------------------------------------------------------------

fn ledger() -> AttemptLedger<'static> {
    static CONTROL: RunToCompletion = RunToCompletion;
    AttemptLedger::open(BYTES, ENTRIES, &CONTROL).expect("a finite ledger opens")
}

/// A ledger that refuses every byte charge.
struct NoBytes;

impl SharedLedger for NoBytes {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        Err(BudgetError::new(BudgetUnit::Bytes, bytes, 0))
    }
    fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

/// A ledger that refuses the first entry charge.
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

/// A ledger that stops after a fixed number of checkpoints.
struct StopsAfter {
    remaining: u32,
}

impl SharedLedger for StopsAfter {
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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn term_key() -> RecordKey {
    RecordKey::term(1)
}

fn proposal_key(id: u128) -> RecordKey {
    RecordKey::new(RecordKind::Proposal, id)
}

fn writer() -> Grants<'static> {
    Grants::new(PRINCIPAL, kb(), CapabilitySet::of(&[Capability::Propose]))
}

fn reviewer() -> Grants<'static> {
    Grants::new(REVIEWER, kb(), CapabilitySet::of(&[Capability::Review]))
}

fn no_capabilities() -> Grants<'static> {
    Grants::new(PRINCIPAL, kb(), CapabilitySet::NONE)
}

fn meta(request_id: &str) -> RequestMeta<'_> {
    // A transaction's `tx_id` is part of its published file name, so two attempts reusing
    // one collide at publication. Deriving it from the request id keeps attempts distinct;
    // the exact-retry test relies on the same request id producing the same digest, not the
    // same tx_id (which the canonical digest excludes anyway).
    let mut tx_id = [0u8; 16];
    let bytes = request_id.as_bytes();
    tx_id[0] = bytes.len() as u8;
    for (at, byte) in bytes.iter().take(15).enumerate() {
        tx_id[at + 1] = *byte;
    }
    RequestMeta {
        request_id,
        tx_id,
        created_at: 1_700_000_000_000_000,
    }
}

fn state<'a>(field_mask: u8, atom: Option<&'a str>, body: Option<&'a str>) -> ConceptTextEdit<'a> {
    ConceptTextEdit {
        subject: term_key(),
        field_mask,
        atom,
        body,
        declared_author: "declared, not the executor",
    }
}

fn baseline() -> [RevPair; 1] {
    [RevPair {
        key: term_key(),
        revision: RecordRevision::FIRST,
    }]
}

/// The atom this fixture's term is created with, so a preserved atom can be compared exactly.
const ATOM: &str = "fixture atom, preserved unless edited";
/// The body this fixture's term is created with.
const BODY: &[u8] = b"fixture body bytes\n";

fn prepared(outcome: FormatOutcome<OwnedEvent>) -> OwnedEvent {
    match outcome {
        FormatOutcome::Ok(event) => event,
        other => panic!("canonical preparation failed: {other:?}"),
    }
}

/// The term the whole suite edits, built through the canonical writer.
fn term_record(retired: bool) -> TermRecord {
    let body_ref = raw_sha256(BODY);
    TermRecord {
        key: term_key(),
        revision: RecordRevision::FIRST,
        retired,
        slug: "fixture-term".to_owned(),
        atom: ATOM.to_owned(),
        layer: sage_db_types::layer::SemanticLayer::L2,
        domains: vec!["product".to_owned(), "testing".to_owned()],
        source: sage_db_types::persist::TermSource::Domain,
        confidence: sage_db_types::confidence::Confidence::Confirmed,
        triggers: vec!["fixture".to_owned(), "concept-text".to_owned()],
        body_ref,
        common: CommonFields {
            recorded_by: Some("seed-actor".to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("seed".to_owned()),
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        legacy_id: Some("legacy-term".to_owned()),
        legacy_source_path: Some("concepts/L2/fixture.md".to_owned()),
        legacy_content_digest: Some([0xC1; 32]),
        legacy_source_file_digest: Some([0xC2; 32]),
        legacy_source_revision: Some([0xC3; 32]),
        extensions: {
            let mut ext = Extensions::new();
            ext.insert("fixture-note", b"kept".to_vec())
                .expect("one small extension");
            ext
        },
        changed_fields: None,
    }
}

fn outcome_event(
    version: u64,
    request_id: &str,
    digest: [u8; 32],
    result_ref: [u8; 32],
) -> RequestOutcomeEvent {
    RequestOutcomeEvent {
        principal_id: PRINCIPAL.into(),
        request_id: request_id.into(),
        payload_digest: digest,
        result_ref,
        commit_version: version,
        audit: CreationAudit {
            recorded_by: Some(PRINCIPAL.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000 + version as i64,
        },
        extensions: Extensions::new(),
    }
}

/// Seed one active term (and, when asked, one retired one) at version 2.
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
    .expect("bootstrap");

    let term = term_record(false);
    let mut events = vec![prepared(typed::prepare_event(
        EventInput::Term(typed::TermEvent::Created, &term),
        &mut l,
    ))];
    commit_seed(&mut store, &mut l, 2, std::mem::take(&mut events), &term);
    (inner, fs, store)
}

/// Seed a term whose committed body is `body`, so a test can exceed any new-payload bound.
fn seed_big_body(body: Vec<u8>) -> (Rc<ModelFileSystem>, ObservingFs, Store<ObservingFs>) {
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

    let mut term = term_record(false);
    term.body_ref = raw_sha256(&body);
    let events = vec![prepared(typed::prepare_event(
        EventInput::Term(typed::TermEvent::Created, &term),
        &mut l,
    ))];
    let affected = [RevPair {
        key: term.key,
        revision: RecordRevision::FIRST,
    }];
    let basis = store
        .commit_basis(IdSpace::ProposalId, &mut l)
        .expect("basis");
    let result_bytes = typed::result_payload(2, &affected, &[], &mut l).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    let mut events = events;
    events.push(prepared(typed::prepare_event(
        EventInput::Outcome(&outcome_event(2, "seed-big", [0x06; 32], result_ref)),
        &mut l,
    )));
    let blobs = vec![
        BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: BlobSource::Existing,
        },
        BlobDeclaration {
            sha256: raw_sha256(&body),
            declared_len: body.len() as u64,
            source: BlobSource::Provide(body),
        },
        BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: BlobSource::Provide(result_bytes),
        },
    ];
    let prepared_tx = PreparedTransaction {
        version: 2,
        prev_version: Some(1),
        tx_id: [0x60; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: PRINCIPAL.to_owned(),
        request_id: "seed-big".to_owned(),
        payload_digest: [0x06; 32],
        expect_version: Some(1),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared_tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed { version, .. } => assert_eq!(version, 2),
        other => panic!("big-body seed failed: {other:?}"),
    }
    (inner, fs, store)
}

/// Commit one seeded transaction carrying the term and its body blob.
fn commit_seed(
    store: &mut Store<ObservingFs>,
    ledger: &mut AttemptLedger<'_>,
    version: u64,
    events: Vec<OwnedEvent>,
    term: &TermRecord,
) {
    commit_seed_versioned(store, ledger, version, events, term, 1);
}

/// Seed with an explicit revision count in the affected set.
///
/// Two revisions of the same term are two `RevPair`s in the result payload, so the caller
/// states how many revisions the transaction produced.
fn commit_seed_versioned(
    store: &mut Store<ObservingFs>,
    ledger: &mut AttemptLedger<'_>,
    version: u64,
    mut events: Vec<OwnedEvent>,
    term: &TermRecord,
    revisions: u64,
) {
    let basis = store
        .commit_basis(IdSpace::ProposalId, ledger)
        .expect("basis");
    let mut affected = Vec::new();
    for revision in 1..=revisions {
        affected.push(RevPair {
            key: term.key,
            revision: RecordRevision::new(revision).expect("revision >= 1"),
        });
    }
    let result_bytes = typed::result_payload(version, &affected, &[], ledger).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    events.push(prepared(typed::prepare_event(
        EventInput::Outcome(&outcome_event(
            version,
            &format!("seed-{version}"),
            [version as u8; 32],
            result_ref,
        )),
        ledger,
    )));
    let blobs = vec![
        BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: BlobSource::Existing,
        },
        BlobDeclaration {
            sha256: raw_sha256(BODY),
            declared_len: BODY.len() as u64,
            source: BlobSource::Provide(BODY.to_vec()),
        },
        BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: BlobSource::Provide(result_bytes),
        },
    ];
    let prepared = PreparedTransaction {
        version,
        prev_version: Some(version - 1),
        tx_id: [0x50 + version as u8; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000 + version as i64,
        principal_id: PRINCIPAL.to_owned(),
        request_id: format!("seed-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: Some(version - 1),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, ledger) {
        HandleOutcome::Committed { version: at, .. } => assert_eq!(at, version),
        other => panic!("seed commit at {version} failed: {other:?}"),
    }
}

fn submitted_version(outcome: &HandleOutcome) -> u64 {
    match outcome {
        HandleOutcome::Committed { version, .. } => *version,
        other => panic!("expected a committed result, got {other:?}"),
    }
}

fn proposal_at(
    store: &Store<ObservingFs>,
    key: RecordKey,
    l: &mut AttemptLedger<'_>,
) -> ProposalRecord {
    match store.record(key, l).unwrap() {
        sage_db_store::view::RecordLookup::Found {
            record: FoldedRecord::Proposal(record),
            ..
        } => (*record).clone(),
        other => panic!("expected a proposal: {other:?}"),
    }
}

fn term_at(store: &Store<ObservingFs>, l: &mut AttemptLedger<'_>) -> TermRecord {
    match store.record(term_key(), l).unwrap() {
        sage_db_store::view::RecordLookup::Found {
            record: FoldedRecord::Term(record),
            ..
        } => (*record).clone(),
        other => panic!("expected the term: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Submit
// ---------------------------------------------------------------------------

#[test]
fn a_pending_proposal_persists_the_exact_frame_and_survives_reopen() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let edit = state(MASK_ATOM | MASK_BODY, Some("new atom"), Some("new body"));
    let outcome = submit_concept_text(
        &mut store,
        &writer(),
        meta("submit-1"),
        edit,
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit succeeds");
    let version = submitted_version(&outcome);
    assert_eq!(version, 3);

    let key = proposal_key(1);
    let proposal = proposal_at(&store, key, &mut l);
    assert_eq!(proposal.status, ProposalStatus::Pending);
    assert_eq!(proposal.kind, ProposalKind::ConceptMarkdown);
    assert_eq!(proposal.subject, term_key());
    assert_eq!(proposal.declared_author, "declared, not the executor");
    assert_eq!(proposal.base_revisions, baseline().to_vec());

    // The persisted payload is the request frame itself, byte for byte.
    let mut expect_payload = Vec::new();
    let edit_payload = encode_submit(&edit, &mut l).expect("payload");
    let ops = [typed::RequestOp::new(7, 1, &edit_payload)];
    let expected_baseline = baseline();
    let frame = typed::RequestFrame::new(&ops, &expected_baseline, None);
    let expected = match typed::encode_request(&frame, &mut l) {
        typed::RequestOutcome::Ok(bytes) => bytes,
        other => panic!("the frame must encode: {other:?}"),
    };
    expect_payload.extend_from_slice(&expected);
    assert_eq!(raw_sha256(&expect_payload), proposal.payload_ref);

    let blob = store
        .read_committed_blob(proposal.payload_ref, typed::MAX_FRAME_BYTES, &mut l)
        .expect("committed payload");
    assert_eq!(blob.as_bytes(), expect_payload.as_slice());

    // Real drop + reopen: the pending proposal and its payload survive the process boundary
    // the model provides.
    drop(store);
    let mut reopened = match open_existing(_fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut l) {
        Ok(sage_db_store::open::Opened::Ready(store)) => store,
        Ok(other) => panic!(
            "the committed root must reopen Ready: {:?}",
            other.durability()
        ),
        Err(error) => panic!("reopen failed: {error:?}"),
    };
    let persisted = proposal_at(&reopened, key, &mut l);
    assert_eq!(
        persisted, proposal,
        "the full proposal record survived reopen"
    );
    let blob = reopened
        .read_committed_blob(persisted.payload_ref, typed::MAX_FRAME_BYTES, &mut l)
        .expect("the committed payload survived reopen");
    assert_eq!(blob.as_bytes(), expect_payload.as_slice());
}

#[test]
fn an_atom_only_edit_leaves_the_body_ref_untouched() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    submit_concept_text(
        &mut store,
        &writer(),
        meta("submit-atom"),
        state(MASK_ATOM, Some("atom only"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");
    let key = proposal_key(1);
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("review-atom"),
        key,
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("approve");

    let term = term_at(&store, &mut l);
    assert_eq!(term.atom, "atom only", "the atom was replaced");
    assert_eq!(
        term.body_ref,
        raw_sha256(BODY),
        "an unnamed body keeps its committed ref exactly"
    );
    assert_eq!(term.revision, RecordRevision::new(2).expect("rev 2"));
}

#[test]
fn a_body_only_edit_replaces_the_blob_and_keeps_the_atom() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    submit_concept_text(
        &mut store,
        &writer(),
        meta("submit-body"),
        state(MASK_BODY, None, Some("body only")),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("review-body"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("approve");

    let term = term_at(&store, &mut l);
    assert_eq!(term.atom, ATOM, "an unnamed atom is preserved verbatim");
    assert_eq!(term.body_ref, raw_sha256(b"body only"));
    let blob = store
        .read_committed_blob(term.body_ref, 1024, &mut l)
        .expect("the new body blob is committed");
    assert_eq!(blob.as_bytes(), b"body only");
}

#[test]
fn an_empty_body_is_a_legal_replacement_and_is_not_confused_with_absent() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    submit_concept_text(
        &mut store,
        &writer(),
        meta("submit-empty"),
        state(MASK_BODY, None, Some("")),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("review-empty"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("approve");
    let term = term_at(&store, &mut l);
    assert_eq!(
        term.body_ref,
        raw_sha256(b""),
        "the empty body is its own blob"
    );
    assert_ne!(term.body_ref, raw_sha256(BODY), "the old body is not kept");
}

#[test]
fn unicode_and_nul_bytes_in_a_body_round_trip_exactly() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let body = "héllo → 世界\0tail";
    submit_concept_text(
        &mut store,
        &writer(),
        meta("submit-unicode"),
        state(MASK_BODY, None, Some(body)),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("review-unicode"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("approve");
    let term = term_at(&store, &mut l);
    let blob = store
        .read_committed_blob(term.body_ref, 1024, &mut l)
        .expect("body blob");
    assert_eq!(blob.as_bytes(), body.as_bytes());
}

// ---------------------------------------------------------------------------
// Authorization and input refusals
// ---------------------------------------------------------------------------

#[test]
fn capabilities_are_not_implied_by_one_another() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let edit = state(MASK_ATOM, Some("a"), None);

    // A reviewer cannot submit, even though review is the stronger-looking capability.
    match submit_concept_text(
        &mut store,
        &reviewer(),
        meta("wrong-cap"),
        edit,
        &baseline(),
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Command(CommandError::Auth(AuthError::MissingCapability {
            capability,
            ..
        }))) => assert_eq!(capability, Capability::Propose),
        other => panic!("a reviewer must not submit: {other:?}"),
    }

    // And an author cannot review.
    submit_concept_text(
        &mut store,
        &writer(),
        meta("submit-a"),
        edit,
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");
    match review_concept_text(
        &mut store,
        &writer(),
        meta("wrong-cap-review"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Command(CommandError::Auth(AuthError::MissingCapability {
            capability,
            ..
        }))) => assert_eq!(capability, Capability::Review),
        other => panic!("an author must not review: {other:?}"),
    }

    // No capabilities at all is refused for both.
    assert!(
        submit_concept_text(
            &mut store,
            &no_capabilities(),
            meta("none"),
            edit,
            &baseline(),
            &Limits::V1,
            &mut l,
        )
        .is_err()
    );
}

#[test]
fn a_grant_for_another_kb_is_refused() {
    let other = KbUuid::from_bytes(OTHER_KB);
    let grants = Grants::new(PRINCIPAL, other, CapabilitySet::of(&[Capability::Propose]));
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    match submit_concept_text(
        &mut store,
        &grants,
        meta("wrong-kb"),
        state(MASK_ATOM, Some("a"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Command(CommandError::Auth(AuthError::WrongKb { .. }))) => {}
        other => panic!("a foreign KB grant must be refused: {other:?}"),
    }
}

#[test]
fn a_wrong_kind_or_malformed_read_set_is_refused_before_any_write() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    // A claim key is not a term.
    let mut edit = state(MASK_ATOM, Some("a"), None);
    edit.subject = RecordKey::new(RecordKind::Claim, 1);
    match submit_concept_text(
        &mut store,
        &writer(),
        meta("wrong-kind"),
        edit,
        &baseline(),
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Command(CommandError::Input(InputError::WrongKeyKind {
            expected,
            found,
        }))) => {
            assert_eq!(expected, RecordKind::Term);
            assert_eq!(found, RecordKind::Claim);
        }
        other => panic!("a non-term subject must be refused: {other:?}"),
    }

    // A read set naming another key is refused.
    let wrong_set = [RevPair {
        key: RecordKey::term(9),
        revision: RecordRevision::FIRST,
    }];
    match submit_concept_text(
        &mut store,
        &writer(),
        meta("wrong-set"),
        state(MASK_ATOM, Some("a"), None),
        &wrong_set,
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Command(CommandError::Input(InputError::ReadSetKey { .. }))) => {}
        other => panic!("a read set naming another key must be refused: {other:?}"),
    }
}

#[test]
fn a_stale_or_missing_subject_is_refused_without_writing() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    let stale = [RevPair {
        key: term_key(),
        revision: RecordRevision::new(5).expect("rev 5"),
    }];
    match submit_concept_text(
        &mut store,
        &writer(),
        meta("stale"),
        state(MASK_ATOM, Some("a"), None),
        &stale,
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Command(CommandError::Input(InputError::StaleBaseline {
            ..
        }))) => {}
        other => panic!("a stale baseline must be refused: {other:?}"),
    }

    // Nothing was committed: no proposal exists.
    assert!(matches!(
        store.record(proposal_key(1), &mut l).unwrap(),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
}

#[test]
fn a_retired_subject_is_refused() {
    // The fixture's term is active, so retire it first through a second seed transaction is
    // out of scope here; instead assert the path on a term created retired.
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
    // A retirement is a LATER revision of the record, so it carries revision 2 and an
    // update time; a creation revision cannot be retired.
    let retired = {
        let mut record = term_record(true);
        record.revision = RecordRevision::new(2).expect("rev 2");
        record.common.updated_at = Some(1_700_000_100_000_000);
        record
    };
    // The record's body is unchanged, so the seed still declares the same body bytes.
    let mut l2 = ledger();
    let created = {
        let mut record = term_record(false);
        record.revision = RecordRevision::FIRST;
        record
    };
    let events = vec![
        prepared(typed::prepare_event(
            EventInput::Term(typed::TermEvent::Created, &created),
            &mut l2,
        )),
        prepared(typed::prepare_event(
            EventInput::Term(typed::TermEvent::Retired, &retired),
            &mut l2,
        )),
    ];
    commit_seed_versioned(&mut store, &mut l, 2, events, &retired, 2);

    match submit_concept_text(
        &mut store,
        &writer(),
        meta("retired"),
        state(MASK_ATOM, Some("a"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Input(ConceptTextInputError::SubjectRetired(key))) => {
            assert_eq!(key, term_key());
        }
        other => panic!("a retired subject must be refused: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Review outcomes
// ---------------------------------------------------------------------------

#[test]
fn an_approved_edit_preserves_every_field_it_does_not_name() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let before = term_at(&store, &mut l);
    submit_concept_text(
        &mut store,
        &writer(),
        meta("submit-preserve"),
        state(MASK_ATOM, Some("edited atom"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("review-preserve"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("approve");

    let after = term_at(&store, &mut l);
    assert_eq!(after.atom, "edited atom");
    assert_eq!(after.slug, before.slug, "slug preserved");
    assert_eq!(after.layer, before.layer, "layer preserved");
    assert_eq!(after.domains, before.domains, "domains preserved");
    assert_eq!(after.source, before.source, "source preserved");
    assert_eq!(after.confidence, before.confidence, "confidence preserved");
    assert_eq!(after.triggers, before.triggers, "triggers preserved");
    assert_eq!(after.retired, before.retired, "retirement preserved");
    assert_eq!(after.legacy_id, before.legacy_id, "legacy id preserved");
    assert_eq!(
        after.legacy_source_path, before.legacy_source_path,
        "legacy path preserved"
    );
    assert_eq!(
        after.legacy_content_digest, before.legacy_content_digest,
        "legacy digest preserved"
    );
    assert_eq!(after.extensions, before.extensions, "extensions preserved");
    assert_eq!(
        after.common.created_at, before.common.created_at,
        "creation time preserved"
    );
    assert_ne!(after.revision, before.revision, "revision advanced");
    assert_eq!(
        after.common.updated_at,
        Some(1_700_000_000_000_000),
        "the update time came from the reviewer's clock"
    );
    assert_eq!(
        after.common.recorded_by.as_deref(),
        Some(REVIEWER),
        "the audit names the authenticated reviewer, not the declared author"
    );

    // The proposal is approved in the same transaction.
    let proposal = proposal_at(&store, proposal_key(1), &mut l);
    assert_eq!(proposal.status, ProposalStatus::Approved);
    assert_eq!(proposal.review_note.as_deref(), Some("approve"));
}

#[test]
fn a_rejection_closes_the_proposal_and_changes_no_term() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    submit_concept_text(
        &mut store,
        &writer(),
        meta("submit-reject"),
        state(MASK_ATOM, Some("never applied"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");
    let before = term_at(&store, &mut l);
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("review-reject"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_REJECT,
        "not this wording",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("reject");
    let after = term_at(&store, &mut l);
    assert_eq!(after.atom, before.atom, "a rejection changes no text");
    assert_eq!(after.revision, before.revision, "and no revision");
    let proposal = proposal_at(&store, proposal_key(1), &mut l);
    assert_eq!(proposal.status, ProposalStatus::Rejected);
}

#[test]
fn a_stale_term_baseline_becomes_a_durable_conflict_not_a_term_edit() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    submit_concept_text(
        &mut store,
        &writer(),
        meta("submit-conflict"),
        state(MASK_ATOM, Some("candidate"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");

    // Advance the term behind the proposal's back: an ordinary body edit reviewed first.
    submit_concept_text(
        &mut store,
        &writer(),
        meta("submit-2"),
        state(MASK_BODY, None, Some("moved on")),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("second submit");
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("review-2"),
        proposal_key(2),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve the newer one",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("approve newer");

    let moved = term_at(&store, &mut l);
    assert_eq!(moved.body_ref, raw_sha256(b"moved on"));

    // Now the first proposal's baseline is stale.
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("review-conflict"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "too late",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("a stale baseline is a committed decision");

    let proposal = proposal_at(&store, proposal_key(1), &mut l);
    assert_eq!(proposal.status, ProposalStatus::Conflicted);
    let after = term_at(&store, &mut l);
    assert_eq!(
        after.atom, ATOM,
        "the conflicted candidate never touched the atom"
    );
    assert_eq!(after.revision, moved.revision, "and no term revision moved");
}

#[test]
fn an_unrelated_stale_pending_proposal_does_not_block_an_approval() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    // Two pending proposals against the same baseline.
    submit_concept_text(
        &mut store,
        &writer(),
        meta("pending-1"),
        state(MASK_ATOM, Some("first candidate"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit 1");
    submit_concept_text(
        &mut store,
        &writer(),
        meta("pending-2"),
        state(MASK_BODY, None, Some("second candidate")),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit 2");

    // Approving the second leaves the first pending; nothing scans it.
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("approve-2"),
        proposal_key(2),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("approve");
    let first = proposal_at(&store, proposal_key(1), &mut l);
    assert_eq!(
        first.status,
        ProposalStatus::Pending,
        "an unrelated pending proposal is untouched"
    );
    assert_eq!(
        term_at(&store, &mut l).body_ref,
        raw_sha256(b"second candidate")
    );
}

// ---------------------------------------------------------------------------
// Retry, mismatch, Unknown
// ---------------------------------------------------------------------------

#[test]
fn an_exact_retry_returns_the_original_outcome_without_mutating() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let edit = state(MASK_ATOM, Some("retry atom"), None);
    let first = submit_concept_text(
        &mut store,
        &writer(),
        meta("retry-submit"),
        edit,
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");
    let version = submitted_version(&first);

    let before = store.retained_entries().count();
    let retry = submit_concept_text(
        &mut store,
        &writer(),
        meta("retry-submit"),
        edit,
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("an exact retry is not an error");
    match retry {
        HandleOutcome::AlreadyCommitted { version: at, .. } => assert_eq!(at, version),
        other => panic!("an exact retry must return the original outcome: {other:?}"),
    }
    assert_eq!(
        store.retained_entries().count(),
        before,
        "an exact retry commits nothing"
    );
}

#[test]
fn the_same_request_id_with_different_content_is_refused() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    submit_concept_text(
        &mut store,
        &writer(),
        meta("same-id"),
        state(MASK_ATOM, Some("one"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");
    match submit_concept_text(
        &mut store,
        &writer(),
        meta("same-id"),
        state(MASK_ATOM, Some("two"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Command(CommandError::IdempotencyMismatch)) => {}
        other => panic!("a different payload under the same id must be refused: {other:?}"),
    }
}

#[test]
fn every_sampled_unknown_world_keeps_the_term_and_the_proposal_in_step() {
    use sage_db_store_tests::oracle::attempts::{
        AttemptHistory, AttemptOutcome, Commit, LogicalRequest, World,
    };

    let (inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    submit_concept_text(
        &mut store,
        &writer(),
        meta("unknown-submit"),
        state(MASK_ATOM, Some("unknown candidate"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("submit");
    let submit_version = 3;
    let old_term = term_at(&store, &mut l);
    let old_proposal = proposal_at(&store, proposal_key(1), &mut l);

    // The digest each attempt actually recorded, for the oracle's request identity.
    let edit_payload = {
        let mut scratch = ledger();
        encode_submit(
            &state(MASK_ATOM, Some("unknown candidate"), None),
            &mut scratch,
        )
        .expect("payload")
    };
    let submit_digest = {
        let ops = [typed::RequestOp::new(7, 1, &edit_payload)];
        let baseline_set = baseline();
        let frame = typed::RequestFrame::new(&ops, &baseline_set, None);
        match typed::request_digest_of(
            &match typed::encode_request(&frame, &mut ledger()) {
                typed::RequestOutcome::Ok(bytes) => bytes,
                other => panic!("frame: {other:?}"),
            },
            &mut ledger(),
        ) {
            typed::RequestOutcome::Ok(digest) => digest,
            other => panic!("digest: {other:?}"),
        }
    };

    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    let outcome = review_concept_text(
        &mut store,
        &reviewer(),
        meta("unknown-review"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve under fault",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("Unknown is an outcome, never an error");
    assert!(matches!(outcome, HandleOutcome::Unknown));
    assert!(inner.faults_handle().borrow().all_fired());
    assert!(store.authority_undecided());

    // Sample NOW, before resolve can synchronise the visible candidate root.
    let worlds = inner.model().crash_states();
    assert!(!worlds.states.is_empty(), "the fault produced no worlds");
    assert_eq!(
        worlds.sampled,
        worlds.states.len(),
        "every world was sampled"
    );

    let submit_request = LogicalRequest::new(PRINCIPAL, "unknown-submit", submit_digest);
    let mut history = AttemptHistory::default();
    history.record(
        "ack-submit",
        submit_request.clone(),
        AttemptOutcome::Durable {
            version: submit_version,
        },
    );
    let review_digest_computed = {
        // The review frame's own digest, reconstructed from the same encoder.
        let payload = concept_intent::encode_review(
            proposal_key(1),
            DECISION_APPROVE,
            "approve under fault",
            &mut ledger(),
        )
        .expect("review payload");
        let ops = [typed::RequestOp::new(8, 1, &payload)];
        let review_set = [RevPair {
            key: proposal_key(1),
            revision: RecordRevision::FIRST,
        }];
        let frame = typed::RequestFrame::new(&ops, &review_set, None);
        match typed::request_digest_of(
            &match typed::encode_request(&frame, &mut ledger()) {
                typed::RequestOutcome::Ok(bytes) => bytes,
                other => panic!("review frame: {other:?}"),
            },
            &mut ledger(),
        ) {
            typed::RequestOutcome::Ok(digest) => digest,
            other => panic!("review digest: {other:?}"),
        }
    };
    let review_request = LogicalRequest::new(REVIEWER, "unknown-review", review_digest_computed);
    history.record(
        "unknown-review",
        review_request.clone(),
        AttemptOutcome::Unknown,
    );
    let review_digest = review_request.payload_digest;

    let mut old_seen = false;
    let mut new_seen = false;
    for (index, world) in worlds.states.into_iter().enumerate() {
        // A world that will not restore is a failure, never a skip.
        let restored =
            ModelFileSystem::from_durable_state(ModelCapabilities::full(), Path::new(ROOT), world)
                .unwrap_or_else(|error| panic!("world {index} restore failed: {error:?}"));
        let fs = ObservingFs::new(Rc::new(restored));
        let reopened =
            match open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger())
                .unwrap_or_else(|error| panic!("world {index} open failed: {error:?}"))
            {
                sage_db_store::open::Opened::Ready(store) => store,
                _ => panic!("world {index} did not become Ready"),
            };
        let head = reopened.head().expect("a world always has a head");
        assert!(
            matches!(head, 3 | 4),
            "the acknowledged base version 3 was lost in world {index}: {head}"
        );

        let mut commits = Vec::new();
        // The base ACK must still be queryable in every world.
        match reopened.outcome(PRINCIPAL, "unknown-submit") {
            sage_db_store::open::OutcomeLookup::Found {
                outcome,
                original_version,
                ..
            } => {
                assert_eq!(original_version, submit_version);
                assert_eq!(outcome.event().payload_digest, submit_digest);
                commits.push(Commit {
                    version: original_version,
                    request: submit_request.clone(),
                });
            }
            other => panic!("world {index} lost the acknowledged submission: {other:?}"),
        }

        let mut term_reader = ledger();
        let actual_term = term_at(&reopened, &mut term_reader);
        let actual_proposal = proposal_at(&reopened, proposal_key(1), &mut term_reader);
        if head == 3 {
            old_seen = true;
            // No candidate applied: both objects are untouched and the review has no outcome.
            assert_eq!(
                actual_term, old_term,
                "world {index}: term changed without a commit"
            );
            assert_eq!(
                actual_proposal, old_proposal,
                "world {index}: proposal changed without a commit"
            );
            assert!(matches!(
                reopened.outcome(REVIEWER, "unknown-review"),
                sage_db_store::open::OutcomeLookup::Absent { .. }
            ));
        } else {
            new_seen = true;
            // The whole candidate applied: term AND proposal, with the review's own outcome.
            let mut expected_term = old_term.clone();
            expected_term.atom = "unknown candidate".to_owned();
            expected_term.revision = RecordRevision::new(2).expect("rev 2");
            expected_term.common.recorded_by = Some(REVIEWER.into());
            expected_term.common.actor_origin = ActorOrigin::Authenticated;
            expected_term.common.reason = Some("approve under fault".into());
            expected_term.common.updated_at = Some(meta("unknown-review").created_at);
            let mut expected_proposal = old_proposal.clone();
            expected_proposal.revision = RecordRevision::new(2).expect("rev 2");
            expected_proposal.status = ProposalStatus::Approved;
            expected_proposal.review_note = Some("approve under fault".into());
            expected_proposal.common.recorded_by = Some(REVIEWER.into());
            expected_proposal.common.actor_origin = ActorOrigin::Authenticated;
            expected_proposal.common.reason = Some("approve under fault".into());
            expected_proposal.common.updated_at = Some(meta("unknown-review").created_at);
            assert_eq!(
                actual_term, expected_term,
                "world {index}: the candidate did not apply atomically"
            );
            assert_eq!(
                actual_proposal, expected_proposal,
                "world {index}: the proposal decision did not apply with the content"
            );
            match reopened.outcome(REVIEWER, "unknown-review") {
                sage_db_store::open::OutcomeLookup::Found {
                    outcome,
                    original_version,
                    ..
                } => {
                    assert_eq!(original_version, 4);
                    assert_eq!(outcome.event().payload_digest, review_digest);
                    commits.push(Commit {
                        version: original_version,
                        request: review_request.clone(),
                    });
                }
                other => {
                    panic!("world {index} has the candidate content without its outcome: {other:?}")
                }
            }
        }

        // `accepts` answers `Ok(false)` for a world the oracle considers illegal, so
        // `.is_ok()` would silently accept a rejected world. Both the error and the `false`
        // must fail here.
        assert!(
            history
                .accepts(
                    2,
                    &World {
                        base_version: 2,
                        commits: commits.clone(),
                    },
                )
                .expect("the oracle must not error on a sampled world"),
            "world {index} contradicts the recorded attempt history"
        );
    }
    assert!(old_seen, "no sampled world kept the pre-review state");
    assert!(new_seen, "no sampled world applied the review");
}

// ---------------------------------------------------------------------------
// Payload codec bounds
// ---------------------------------------------------------------------------

#[test]
fn the_maximum_payload_is_accepted_and_one_more_byte_is_refused_before_admission() {
    use sage_db_store::typed::{MAX_PAYLOAD, MAX_TEXT_BYTES};

    let mut l = ledger();
    // Submit: key(18) + mask(1) + [atom] + [body] + author prefix(4) + author.
    // With only the body selected, a valid short atom absent, and the author at its canonical
    // maximum, the body is what can grow: total = 18 + 1 + 4 + body + 4 + MAX_DECLARED_AUTHOR.
    // The atom cannot carry the payload to the ceiling: its business bound is 200 units.
    let fixed = 18 + 1 + 4 + 4 + sage_db_store::typed::MAX_DECLARED_AUTHOR as usize;
    let body_at_max = MAX_PAYLOAD as usize - fixed;
    let body = "x".repeat(body_at_max);
    let edit = ConceptTextEdit {
        subject: term_key(),
        field_mask: MASK_BODY,
        atom: None,
        body: Some(&body),
        declared_author: &"a".repeat(sage_db_store::typed::MAX_DECLARED_AUTHOR as usize),
    };
    let bytes = encode_submit(&edit, &mut l).expect("exactly MAX_PAYLOAD must encode");
    assert_eq!(bytes.len(), MAX_PAYLOAD as usize);

    let over = "x".repeat(body_at_max + 1);
    let edit_over = ConceptTextEdit {
        body: Some(&over),
        ..edit
    };
    assert!(
        matches!(
            encode_submit(&edit_over, &mut l),
            Err(ConceptTextPayloadError::TooLarge { .. })
        ),
        "one byte past the ceiling must be refused"
    );
    let _ = MAX_TEXT_BYTES;
}

#[test]
fn the_business_atom_bound_is_200_utf16_units_for_bmp_text() {
    // The shipped validators are `z.string().min(1).max(200)` and JavaScript string length is
    // UTF-16 code units, so a BMP atom of 200 characters is accepted and 201 is refused.
    let mut l = ledger();
    let at_max = "x".repeat(200);
    let ok = ConceptTextEdit {
        subject: term_key(),
        field_mask: MASK_ATOM,
        atom: Some(&at_max),
        body: None,
        declared_author: "",
    };
    let bytes = encode_submit(&ok, &mut l).expect("200 BMP units is the accepted maximum");
    let decoded = concept_intent::decode_submit(&bytes, &mut l).expect("round trip");
    assert_eq!(decoded.atom.map(str::len), Some(200));

    let over = "x".repeat(201);
    let bad = ConceptTextEdit {
        atom: Some(&over),
        ..ok
    };
    assert_eq!(
        encode_submit(&bad, &mut l),
        Err(ConceptTextPayloadError::AtomUtf16TooLong {
            units: 201,
            max: 200,
        }),
        "201 BMP units is the first rejected length"
    );
}

#[test]
fn the_business_atom_bound_counts_astral_characters_as_two_units() {
    // An astral character is one Rust `char` but two UTF-16 code units, which is exactly what
    // the JS validators charge it. 100 astral characters are therefore 200 units (accepted)
    // and 101 are 202 (refused) — a byte- or scalar-based counter would accept both.
    let mut l = ledger();
    let at_max = "\u{1F600}".repeat(100);
    assert_eq!(at_max.chars().count(), 100);
    assert_eq!(at_max.encode_utf16().count(), 200);
    assert_eq!(at_max.len(), 400, "four UTF-8 bytes per astral character");

    let ok = ConceptTextEdit {
        subject: term_key(),
        field_mask: MASK_ATOM,
        atom: Some(&at_max),
        body: None,
        declared_author: "",
    };
    let bytes = encode_submit(&ok, &mut l).expect("100 astral characters is exactly 200 units");
    let decoded = concept_intent::decode_submit(&bytes, &mut l).expect("round trip");
    assert_eq!(decoded.atom, Some(at_max.as_str()));

    let over = "\u{1F600}".repeat(101);
    assert_eq!(over.encode_utf16().count(), 202);
    let bad = ConceptTextEdit {
        atom: Some(&over),
        ..ok
    };
    assert_eq!(
        encode_submit(&bad, &mut l),
        Err(ConceptTextPayloadError::AtomUtf16TooLong {
            units: 202,
            max: 200,
        }),
        "101 astral characters is 202 units, so it is refused"
    );
}

#[test]
fn the_byte_bound_reports_first_when_both_atom_bounds_are_exceeded() {
    // 270 CJK characters exceed BOTH bounds: 270 UTF-16 units and 810 UTF-8 bytes.
    // This input proves deterministic byte-first error priority, not independent bounds.
    let mut l = ledger();
    let wide = "世".repeat(270);
    assert_eq!(wide.encode_utf16().count(), 270);
    assert_eq!(wide.len(), 810);
    let edit = ConceptTextEdit {
        subject: term_key(),
        field_mask: MASK_ATOM,
        atom: Some(&wide),
        body: None,
        declared_author: "",
    };
    assert_eq!(
        encode_submit(&edit, &mut l),
        Err(ConceptTextPayloadError::AtomTooLong { len: 810, max: 800 }),
        "the canonical byte ceiling owns this refusal"
    );
}

#[test]
fn a_caller_stop_and_a_budget_refusal_stay_distinct() {
    let mut l = ledger();
    let edit = state(MASK_ATOM, Some("a"), None);

    let mut stopping = StopsAfter { remaining: 0 };
    match encode_submit(&edit, &mut stopping) {
        Err(ConceptTextPayloadError::Refused(Refusal::Stopped(ControlStop::Cancelled))) => {}
        other => panic!("a stop must be a stop: {other:?}"),
    }

    // Encoding charges BYTES (a decode pass additionally charges one entry), so the
    // distinct-budget probe is a byte-refusing ledger at encode and an entry-refusing one at
    // decode. Both must stay budget refusals, not stops.
    let mut no_bytes = NoBytes;
    match encode_submit(&edit, &mut no_bytes) {
        Err(ConceptTextPayloadError::Refused(Refusal::Budget(error))) => {
            assert_eq!(error.unit(), BudgetUnit::Bytes);
        }
        other => panic!("a byte refusal must be a budget refusal: {other:?}"),
    }
    let bytes = encode_submit(&edit, &mut l).expect("encode");
    let mut no_entries = NoEntries;
    match concept_intent::decode_submit(&bytes, &mut no_entries) {
        Err(ConceptTextPayloadError::Refused(Refusal::Budget(error))) => {
            assert_eq!(error.unit(), BudgetUnit::Entries);
        }
        other => panic!("an entry refusal must be a budget refusal: {other:?}"),
    }
}

#[test]
fn an_approval_of_a_really_unregistered_stored_frame_is_refused_without_mutating() {
    // The review asked for the real thing rather than vocabulary: a Pending ConceptMarkdown
    // proposal whose committed payload blob is a VALID P1 frame carrying an operation this
    // schema does not register. No public writer emits that, so the fixture builds it directly
    // out of canonical pieces — a real frame (op 6/v1, the frontier review op), a real payload
    // blob, a real read set equal to `base_revisions`, and a real `RequestOutcomeEvent`.
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    // The frame: op_kind 6 (REVISE_FRONTIER), version 1, a small opaque payload, and a read set
    // that is exactly the proposal's own baseline.
    let baseline_set = baseline();
    let unregistered_payload = b"\x06\x00\x00\x00opaque-frontier-review".to_vec();
    let ops = [typed::RequestOp::new(6, 1, &unregistered_payload)];
    let frame = typed::RequestFrame::new(&ops, &baseline_set, None);
    let frame_bytes = match typed::encode_request(&frame, &mut l) {
        typed::RequestOutcome::Ok(bytes) => bytes,
        other => panic!("the fixture frame must encode: {other:?}"),
    };
    let payload_ref = raw_sha256(&frame_bytes);

    let proposal = ProposalRecord {
        key: proposal_key(1),
        revision: RecordRevision::FIRST,
        status: ProposalStatus::Pending,
        kind: ProposalKind::ConceptMarkdown,
        subject: term_key(),
        base_revisions: baseline_set.to_vec(),
        payload_ref,
        review_note: None,
        declared_author: "fixture-declared-author".to_owned(),
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        legacy_id: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };

    let affected = [RevPair {
        key: proposal.key,
        revision: RecordRevision::FIRST,
    }];
    let basis = store
        .commit_basis(IdSpace::ProposalId, &mut l)
        .expect("basis");
    let result_bytes = typed::result_payload(3, &affected, &[], &mut l).expect("result payload");
    let result_ref = raw_sha256(&result_bytes);
    let events = vec![
        prepared(typed::prepare_event(
            EventInput::Proposal {
                decided: false,
                record: &proposal,
            },
            &mut l,
        )),
        prepared(typed::prepare_event(
            EventInput::Outcome(&outcome_event(
                3,
                "seed-unregistered",
                [0x07; 32],
                result_ref,
            )),
            &mut l,
        )),
    ];
    let blobs = vec![
        BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: BlobSource::Existing,
        },
        BlobDeclaration {
            sha256: payload_ref,
            declared_len: frame_bytes.len() as u64,
            source: BlobSource::Provide(frame_bytes),
        },
        BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: BlobSource::Provide(result_bytes),
        },
    ];
    let prepared_tx = PreparedTransaction {
        version: 3,
        prev_version: Some(2),
        tx_id: [0x64; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: PRINCIPAL.to_owned(),
        request_id: "seed-unregistered".to_owned(),
        payload_digest: [0x07; 32],
        expect_version: Some(2),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared_tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed { version, .. } => assert_eq!(version, 3),
        other => panic!("the unregistered-frame seed failed: {other:?}"),
    }

    // Sanity: the stored proposal really does carry op 6/v1, so the reviewer's refusal is about
    // an operation it does not register rather than about a metadata mismatch.
    let stored = proposal_at(&store, proposal_key(1), &mut l);
    let blob = store
        .read_committed_blob(stored.payload_ref, typed::MAX_FRAME_BYTES, &mut l)
        .expect("the fixture payload is committed");
    let decoded = match typed::decode_request(blob.as_bytes(), &mut l) {
        typed::RequestOutcome::Ok(decoded) => decoded,
        other => panic!("the fixture frame must decode: {other:?}"),
    };
    assert_eq!(decoded.ops.len(), 1);
    assert_eq!((decoded.ops[0].kind, decoded.ops[0].version), (6, 1));

    // Approving it must be refused as an unregistered stored operation, and nothing may move.
    let term_before = term_at(&store, &mut l);
    let proposal_before = proposal_at(&store, proposal_key(1), &mut l);
    match review_concept_text(
        &mut store,
        &reviewer(),
        meta("review-unregistered"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Command(CommandError::Input(
            InputError::UnsupportedStoredOperation { kind, version },
        ))) => {
            assert_eq!(
                kind, 6,
                "the refusal names the operation it could not interpret"
            );
            assert_eq!(version, 1);
        }
        other => panic!("an unregistered stored operation must be refused: {other:?}"),
    }
    assert_eq!(
        term_at(&store, &mut l),
        term_before,
        "the term must not move"
    );
    assert_eq!(
        proposal_at(&store, proposal_key(1), &mut l),
        proposal_before,
        "the proposal must stay Pending"
    );
}

// ---------------------------------------------------------------------------
// R2: consecutive approvals, changed_fields, the existing-body ceiling, id limits
// ---------------------------------------------------------------------------

#[test]
fn two_approvals_in_sequence_start_from_revision_two_and_keep_both_bodies() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    // First edit: the term is at revision 1, so the copy is a creation image.
    submit_concept_text(
        &mut store,
        &writer(),
        meta("first-submit"),
        state(MASK_BODY, None, Some("first body")),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("first submit");
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("first-review"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "first approval",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("the first approval commits");

    let after_first = term_at(&store, &mut l);
    assert_eq!(after_first.revision, RecordRevision::new(2).expect("rev 2"));
    assert_eq!(after_first.body_ref, raw_sha256(b"first body"));
    let first_body = store
        .read_committed_blob(after_first.body_ref, 1024, &mut l)
        .expect("first body blob");
    assert_eq!(first_body.as_bytes(), b"first body");

    // Second edit: the term is now at revision 2, so the canonical copy MUST be a `Revised`
    // image. A creation image here is refused by the canonical writer, which is exactly the
    // defect R2 fixes.
    let baseline_at_two = [RevPair {
        key: term_key(),
        revision: RecordRevision::new(2).expect("rev 2"),
    }];
    submit_concept_text(
        &mut store,
        &writer(),
        meta("second-submit"),
        state(MASK_ATOM, Some("second atom"), None),
        &baseline_at_two,
        &Limits::V1,
        &mut l,
    )
    .expect("second submit");
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("second-review"),
        proposal_key(2),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "second approval",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("a second text edit of the same concept must commit");

    let after_second = term_at(&store, &mut l);
    assert_eq!(
        after_second.revision,
        RecordRevision::new(3).expect("rev 3"),
        "the second approval starts from revision 2"
    );
    assert_eq!(after_second.atom, "second atom");
    assert_eq!(
        after_second.body_ref,
        raw_sha256(b"first body"),
        "an unnamed body keeps the body the first edit committed"
    );

    // The first body is still committed history, not rewritten or dropped.
    let old = store
        .read_committed_blob(raw_sha256(b"first body"), 1024, &mut l)
        .expect("the earlier body blob remains committed");
    assert_eq!(old.as_bytes(), b"first body");

    // A text-only edit leaves the base graph's identity alone: same terms, same assertions.
    let mut basis_ledger = ledger();
    let basis = store
        .commit_basis(IdSpace::TermId, &mut basis_ledger)
        .expect("basis");
    assert!(basis.basis.version.get() >= 4);
}

#[test]
fn a_later_revision_changed_fields_is_copied_and_preserved() {
    // `changed_fields` is only legal on a non-first revision, so the seed must be revision 2.
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

    let created = term_record(false);
    let mut revised = created.clone();
    revised.revision = RecordRevision::new(2).expect("rev 2");
    revised.atom = "atom at rev 2".to_owned();
    revised.common.updated_at = Some(1_700_000_100_000_000);
    // Tags 9 (updated_at) and 17 (atom) changed: legal only on a later revision.
    revised.changed_fields = Some(vec![9, 17]);

    // A fresh bootstrap has only Genesis, so both revisions are seeded in ONE version-2
    // transaction: the creation image, then the revision-2 image carrying `changed_fields`.
    let mut seed_ledger = ledger();
    let events = vec![
        prepared(typed::prepare_event(
            EventInput::Term(typed::TermEvent::Created, &created),
            &mut seed_ledger,
        )),
        prepared(typed::prepare_event(
            EventInput::Term(typed::TermEvent::Revised, &revised),
            &mut seed_ledger,
        )),
    ];
    let affected = [
        RevPair {
            key: created.key,
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: revised.key,
            revision: RecordRevision::new(2).expect("rev 2"),
        },
    ];
    let basis = store
        .commit_basis(IdSpace::ProposalId, &mut seed_ledger)
        .expect("basis");
    let result_bytes =
        typed::result_payload(2, &affected, &[], &mut seed_ledger).expect("result payload");
    let result_ref = raw_sha256(&result_bytes);
    let mut events = events;
    events.push(prepared(typed::prepare_event(
        EventInput::Outcome(&outcome_event(2, "seed-rev2", [0x03; 32], result_ref)),
        &mut seed_ledger,
    )));
    let blobs = vec![
        BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: BlobSource::Existing,
        },
        BlobDeclaration {
            sha256: raw_sha256(BODY),
            declared_len: BODY.len() as u64,
            source: BlobSource::Provide(BODY.to_vec()),
        },
        BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: BlobSource::Provide(result_bytes),
        },
    ];
    let prepared_tx = PreparedTransaction {
        version: 2,
        prev_version: Some(1),
        tx_id: [0x61; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: PRINCIPAL.to_owned(),
        request_id: "seed-rev2".to_owned(),
        payload_digest: [0x03; 32],
        expect_version: Some(1),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared_tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed { version, .. } => assert_eq!(version, 2),
        other => panic!("seed commit failed: {other:?}"),
    }

    // Approve a body-only edit against revision 2 and assert `changed_fields` survives.
    let baseline_two = [RevPair {
        key: term_key(),
        revision: RecordRevision::new(2).expect("rev 2"),
    }];
    submit_concept_text(
        &mut store,
        &writer(),
        meta("cf-submit"),
        state(MASK_BODY, None, Some("new body after rev 2")),
        &baseline_two,
        &Limits::V1,
        &mut l,
    )
    .expect("submit at revision 2");
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("cf-review"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve at rev 2",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("approve at revision 2");

    let after = term_at(&store, &mut l);
    assert_eq!(after.revision, RecordRevision::new(3).expect("rev 3"));
    assert_eq!(
        after.changed_fields,
        Some(vec![9, 17]),
        "the advisory changed_fields value is preserved verbatim"
    );
    assert_eq!(after.body_ref, raw_sha256(b"new body after rev 2"));
}

#[test]
fn an_atom_only_approval_refuses_a_body_above_the_caller_ceiling_without_mutating() {
    // A committed body may legitimately exceed the 64 KiB a NEW payload's text is bounded by:
    // only the codec bounds new text. The existing-body ceiling is a caller policy, and
    // supplying a small one must refuse the read rather than truncate or guess.
    use sage_db_store::typed::MAX_FRAME_BYTES;

    let big_len = MAX_FRAME_BYTES as usize + 17;
    let big_body = vec![0xABu8; big_len];
    let (_inner, _fs, mut store) = seed_big_body(big_body.clone());

    let mut l = ledger();
    submit_concept_text(
        &mut store,
        &writer(),
        meta("big-submit"),
        state(MASK_ATOM, Some("atom only over a big body"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    )
    .expect("an atom-only edit names no body");
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("big-review-small"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        // Smaller than the committed body: the read refuses.
        1024,
        &Limits::V1,
        &mut l,
    )
    .expect_err("a body above the caller's ceiling must refuse");

    // Nothing moved: same revision, same atom, same body ref.
    let unchanged = term_at(&store, &mut l);
    assert_eq!(unchanged.revision, RecordRevision::FIRST);
    assert_eq!(unchanged.atom, ATOM);

    // An adequate ceiling commits, and the unchanged body's bytes and hash are retained.
    review_concept_text(
        &mut store,
        &reviewer(),
        meta("big-review-ok"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        MAX_EXISTING_BODY,
        &Limits::V1,
        &mut l,
    )
    .expect("an adequate ceiling commits");
    let after = term_at(&store, &mut l);
    assert_eq!(after.revision, RecordRevision::new(2).expect("rev 2"));
    assert_eq!(after.atom, "atom only over a big body");
    assert_eq!(
        after.body_ref,
        raw_sha256(&big_body),
        "the unchanged body keeps its exact committed hash"
    );
    let blob = store
        .read_committed_blob(after.body_ref, MAX_EXISTING_BODY, &mut l)
        .expect("the big body remains committed");
    assert_eq!(blob.as_bytes(), big_body.as_slice());

    // An exact retry with a ZERO ceiling still returns the original outcome: the policy is
    // checked only after the idempotency lookup.
    let retry = review_concept_text(
        &mut store,
        &reviewer(),
        meta("big-review-ok"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        0,
        &Limits::V1,
        &mut l,
    )
    .expect("an exact retry returns before the ceiling is consulted");
    assert!(matches!(retry, HandleOutcome::AlreadyCommitted { .. }));
}

#[test]
fn a_preoccupied_next_proposal_id_is_reported_as_a_collision_without_mutating() {
    // Frontier R3 proved this state is physically reachable through the public API, so it is
    // driven here rather than declared unreachable: seed proposal id 1 while the allocation
    // mark stays at 0, then submit and observe the collision.
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    let mut seed_ledger = ledger();
    let proposal = ProposalRecord {
        key: proposal_key(1),
        revision: RecordRevision::FIRST,
        status: ProposalStatus::Pending,
        kind: ProposalKind::ConceptMarkdown,
        subject: term_key(),
        base_revisions: baseline().to_vec(),
        payload_ref: raw_sha256(b"occupying payload"),
        review_note: None,
        declared_author: "occupant".to_owned(),
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        legacy_id: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    let affected = [RevPair {
        key: proposal.key,
        revision: RecordRevision::FIRST,
    }];
    let basis = store
        .commit_basis(IdSpace::ProposalId, &mut seed_ledger)
        .expect("basis");
    let result_bytes = typed::result_payload(3, &affected, &[], &mut seed_ledger).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    let events = vec![
        prepared(typed::prepare_event(
            EventInput::Proposal {
                decided: false,
                record: &proposal,
            },
            &mut seed_ledger,
        )),
        prepared(typed::prepare_event(
            EventInput::Outcome(&outcome_event(3, "seed-occupied", [0x04; 32], result_ref)),
            &mut seed_ledger,
        )),
    ];
    // No IdAllocation event: the mark deliberately stays at 0 while id 1 is occupied.
    let blobs = vec![
        BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: BlobSource::Existing,
        },
        BlobDeclaration {
            sha256: raw_sha256(b"occupying payload"),
            declared_len: b"occupying payload".len() as u64,
            source: BlobSource::Provide(b"occupying payload".to_vec()),
        },
        BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: BlobSource::Provide(result_bytes),
        },
    ];
    let prepared_tx = PreparedTransaction {
        version: 3,
        prev_version: Some(2),
        tx_id: [0x62; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: PRINCIPAL.to_owned(),
        request_id: "seed-occupied".to_owned(),
        payload_digest: [0x04; 32],
        expect_version: Some(2),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared_tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed { version, .. } => assert_eq!(version, 3),
        other => panic!("occupying seed failed: {other:?}"),
    }

    // The mark is still 0, so submit allocates 1 and finds it occupied.
    let before = store.retained_entries().count();
    match submit_concept_text(
        &mut store,
        &writer(),
        meta("collide"),
        state(MASK_ATOM, Some("a"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Input(ConceptTextInputError::ProposalIdCollision(key))) => {
            assert_eq!(key, proposal_key(1));
        }
        other => panic!("a preoccupied next id must be a collision: {other:?}"),
    }
    assert_eq!(
        store.retained_entries().count(),
        before,
        "a collision commits nothing"
    );
}

#[test]
fn an_exhausted_proposal_id_space_is_reported_as_exhausted_without_mutating() {
    // A real `IdAllocation` carrying `hwm = u128::MAX` puts the mark at the ceiling, so the
    // next `checked_add(1)` overflows. R3 showed this is physically reachable.
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let mut seed_ledger = ledger();

    let allocation = IdAllocationEvent {
        allocation: Watermark {
            space: IdSpace::ProposalId,
            hwm: u128::MAX,
        },
        reason: None,
        extensions: Extensions::new(),
    };
    let basis = store
        .commit_basis(IdSpace::ProposalId, &mut seed_ledger)
        .expect("basis");
    let result_bytes =
        typed::result_payload(3, &[], &[allocation.allocation], &mut seed_ledger).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    let events = vec![
        prepared(typed::prepare_event(
            EventInput::IdAllocation(&allocation),
            &mut seed_ledger,
        )),
        prepared(typed::prepare_event(
            EventInput::Outcome(&outcome_event(3, "seed-exhausted", [0x05; 32], result_ref)),
            &mut seed_ledger,
        )),
    ];
    let blobs = vec![
        BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: BlobSource::Existing,
        },
        BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: BlobSource::Provide(result_bytes),
        },
    ];
    let prepared_tx = PreparedTransaction {
        version: 3,
        prev_version: Some(2),
        tx_id: [0x63; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: PRINCIPAL.to_owned(),
        request_id: "seed-exhausted".to_owned(),
        payload_digest: [0x05; 32],
        expect_version: Some(2),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared_tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed { version, .. } => assert_eq!(version, 3),
        other => panic!("exhausting seed failed: {other:?}"),
    }

    let before = store.retained_entries().count();
    match submit_concept_text(
        &mut store,
        &writer(),
        meta("exhausted"),
        state(MASK_ATOM, Some("a"), None),
        &baseline(),
        &Limits::V1,
        &mut l,
    ) {
        Err(ConceptTextError::Input(ConceptTextInputError::ProposalIdExhausted)) => {}
        other => panic!("an exhausted id space must be reported: {other:?}"),
    }
    assert_eq!(
        store.retained_entries().count(),
        before,
        "an exhausted id space commits nothing"
    );
}
