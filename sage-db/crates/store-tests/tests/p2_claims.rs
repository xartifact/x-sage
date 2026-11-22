//! P2 claim-edit vertical slice on a real public Store and the model backend.
//!
//! Every command here goes through the production entry points — `bootstrap`,
//! `Store::commit`, `open_existing`, `submit_claim_edit`, `review_claim_edit` — with the
//! real ledger, the real canonical codecs and a real fault injector. Seed rows are built
//! through the canonical `typed` bridge rather than by hand-assembling field bytes, so a
//! seeded record is byte-identical to what a writer would persist.
//!
//! What this file does **not** claim: platform durability, a full P2 graph validation, or
//! that a live `resolve` exercises both crash outcomes. The separate Root control
//! restores and judges every sampled world before resolving the original handle.

#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::path::Path;
use std::rc::Rc;

use fixture::{BYTES, ENTRIES, ROOT, RULES, kb};
use observing_fs::ObservingFs;

use sage_db_format::digest::raw_sha256;
use sage_db_kernel::auth::{AuthError, Capability, CapabilitySet, Grants};
use sage_db_kernel::claims::{
    CommandError, InputError, RequestMeta, StoredField, review_claim_edit, submit_claim_edit,
};
use sage_db_kernel::intent::{ClaimEdit, PayloadError, ReviewDecision};
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
use sage_db_types::framing::FormatOutcome;
use sage_db_types::framing::{Limits, Refusal};
use sage_db_types::identity::{KbUuid, RecordRevision};
use sage_db_types::ledger::{BudgetError, BudgetUnit};
use sage_db_types::persist::{
    ActorOrigin, ClaimKind, ClaimRecord, ClaimStatus, CommonFields, CreationAudit, Extensions,
    IdSpace, ProposalKind, ProposalRecord, ProposalStatus, RecordKey, RecordKind,
    RequestOutcomeEvent, RevPair,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};
use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};

const PRINCIPAL: &str = "real-proposer";
const REVIEWER: &str = "real-reviewer";

/// Independent P2 payload bytes for the existing external sequence oracle.
/// This does not call the intent encoder being exercised by the command.
fn root_request_digest(kind: u16, payload: &[u8], read_set: &[RevPair]) -> [u8; 32] {
    let ops = [typed::RequestOp::new(kind, 1, payload)];
    let frame = typed::RequestFrame::new(&ops, read_set, None);
    let bytes = match typed::encode_request(&frame, &mut ledger()) {
        typed::RequestOutcome::Ok(bytes) => bytes,
        other => panic!("Root oracle frame refused: {other:?}"),
    };
    sage_db_format::digest::digest(sage_db_format::digest::Domain::Request, &bytes)
}

fn root_literal_key(out: &mut Vec<u8>, kind: u16, id: u128) {
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
}

fn root_literal_str(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&u32::try_from(text.len()).unwrap().to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

#[test]
fn root_declared_author_accepts_800_utf8_bytes_and_rejects_801() {
    let (_inner, fs, mut store) = seed_world();
    let author = "é".repeat(400);
    assert_eq!(author.len(), 800);
    let baseline = current_baseline(&store);
    let edit = ClaimEdit {
        subject: term_key(1),
        claim: claim_key(1),
        text: "attribution boundary",
        declared_author: &author,
    };
    let outcome = submit_claim_edit(
        &mut store,
        &proposer(),
        meta("author-800"),
        edit,
        &baseline,
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap();
    assert_eq!(committed_version(&outcome), 3);
    let proposed = proposal_at(&store, proposal_key(1));
    assert_eq!(proposed.declared_author, author);
    assert_eq!(proposed.common.recorded_by.as_deref(), Some(PRINCIPAL));
    let before = fs.counts().borrow().total();
    let too_long = format!("{author}x");
    assert_eq!(too_long.len(), 801);
    let oversized = ClaimEdit {
        declared_author: &too_long,
        ..edit
    };
    assert_eq!(
        input_error(submit_claim_edit(
            &mut store,
            &proposer(),
            meta("author-801"),
            oversized,
            &baseline,
            &Limits::V1,
            &mut ledger()
        )),
        InputError::DeclaredAuthorTooLong { len: 801, max: 800 }
    );
    assert_eq!(fs.counts().borrow().total(), before);
    assert_eq!(store.head(), Some(3));
    assert!(proposal_at_absent(&store, proposal_key(2)));
    assert!(matches!(
        review(
            &mut store,
            "approve-author-800",
            proposal_key(1),
            ReviewDecision::Approve,
            "valid attribution",
            &mut ledger()
        ),
        Ok(HandleOutcome::Committed { version: 4, .. })
    ));
    let decided = proposal_at(&store, proposal_key(1));
    assert_eq!(decided.declared_author, author);
    assert_eq!(decided.common.recorded_by.as_deref(), Some(REVIEWER));
}

#[test]
fn root_every_sampled_unknown_world_preserves_the_claim_and_decision_together() {
    use sage_db_store_tests::oracle::attempts::{
        AttemptHistory, AttemptOutcome, Commit, LogicalRequest, World,
    };

    const SUBMIT: &str = "root-atomic-submit";
    const REVIEW: &str = "root-atomic-review";
    const TEXT: &str = "Root atomic replacement — exact bytes";
    const NOTE: &str = "Root approved the authored change";
    const AUTHOR: &str = "declared-author, not the executor";
    let (inner, _fs, mut store) = seed_world();
    let baseline = current_baseline(&store);
    let mut submit_payload = Vec::new();
    root_literal_key(&mut submit_payload, 1, 1);
    root_literal_key(&mut submit_payload, 3, 1);
    root_literal_str(&mut submit_payload, TEXT);
    root_literal_str(&mut submit_payload, AUTHOR);
    let submit_digest = root_request_digest(1, &submit_payload, &baseline);
    let submitted = submit(&mut store, SUBMIT, TEXT, &mut ledger()).unwrap();
    assert_eq!(committed_version(&submitted), 3);
    let old_claim = claim_at(&store, claim_key(1));
    let old_proposal = proposal_at(&store, proposal_key(1));
    let pin = store.published_pin().unwrap();
    let pin_entries = pin.entries().to_vec();

    let mut review_payload = Vec::new();
    root_literal_key(&mut review_payload, 7, 1);
    review_payload.extend_from_slice(&0u16.to_le_bytes());
    root_literal_str(&mut review_payload, NOTE);
    let review_set = [RevPair {
        key: proposal_key(1),
        revision: RecordRevision::FIRST,
    }];
    let review_digest = root_request_digest(2, &review_payload, &review_set);
    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    let outcome = review(
        &mut store,
        REVIEW,
        proposal_key(1),
        ReviewDecision::Approve,
        NOTE,
        &mut ledger(),
    )
    .unwrap();
    assert!(matches!(outcome, HandleOutcome::Unknown));
    assert!(inner.faults_handle().borrow().all_fired());
    assert!(store.authority_undecided());

    // Sample NOW, before resolve can synchronize the visible candidate root.
    let worlds = inner.model().crash_states();
    assert!(!worlds.states.is_empty());
    assert_eq!(worlds.sampled, worlds.states.len());
    let submit_request = LogicalRequest::new(PRINCIPAL, SUBMIT, submit_digest);
    let review_request = LogicalRequest::new(REVIEWER, REVIEW, review_digest);
    let mut history = AttemptHistory::default();
    history.record(
        "ack-submit",
        submit_request.clone(),
        AttemptOutcome::Durable { version: 3 },
    );
    history.record(
        "unknown-review",
        review_request.clone(),
        AttemptOutcome::Unknown,
    );
    let mut examined = 0;
    let mut old_seen = false;
    let mut new_seen = false;
    for (index, state) in worlds.states.into_iter().enumerate() {
        let restored =
            ModelFileSystem::from_durable_state(ModelCapabilities::full(), Path::new(ROOT), state)
                .unwrap_or_else(|e| panic!("world {index} restore failed: {e:?}"));
        let fs = ObservingFs::new(Rc::new(restored));
        let mut reopened =
            match open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger())
                .unwrap_or_else(|e| panic!("world {index} open failed: {e:?}"))
            {
                Opened::Ready(store) => store,
                _ => panic!("world {index} did not become Ready"),
            };
        let head = reopened.head().unwrap();
        assert!(
            matches!(head, 3 | 4),
            "base ACK3 was lost in world {index}: {head}"
        );
        let mut commits = Vec::new();
        match reopened.outcome(PRINCIPAL, SUBMIT) {
            sage_db_store::open::OutcomeLookup::Found {
                outcome,
                original_version,
                ..
            } => {
                assert_eq!(original_version, 3);
                assert_eq!(outcome.event().payload_digest, submit_digest);
                commits.push(Commit {
                    version: original_version,
                    request: submit_request.clone(),
                });
            }
            other => panic!("world {index} lost the acknowledged submission: {other:?}"),
        }
        let actual_claim = claim_at(&reopened, claim_key(1));
        let actual_proposal = proposal_at(&reopened, proposal_key(1));
        if head == 3 {
            old_seen = true;
            assert_eq!(actual_claim, old_claim);
            assert_eq!(actual_proposal, old_proposal);
            assert!(matches!(
                reopened.outcome(REVIEWER, REVIEW),
                sage_db_store::open::OutcomeLookup::Absent { .. }
            ));
        } else {
            new_seen = true;
            let mut expected_claim = old_claim.clone();
            expected_claim.revision = RecordRevision::new(2).unwrap();
            expected_claim.text_ref = raw_sha256(TEXT.as_bytes());
            expected_claim.common.recorded_by = Some(REVIEWER.into());
            expected_claim.common.actor_origin = ActorOrigin::Authenticated;
            expected_claim.common.reason = Some(NOTE.into());
            expected_claim.common.updated_at = Some(meta(REVIEW).created_at);
            let mut expected_proposal = old_proposal.clone();
            expected_proposal.revision = RecordRevision::new(2).unwrap();
            expected_proposal.status = ProposalStatus::Approved;
            expected_proposal.review_note = Some(NOTE.into());
            expected_proposal.common.recorded_by = Some(REVIEWER.into());
            expected_proposal.common.actor_origin = ActorOrigin::Authenticated;
            expected_proposal.common.reason = Some(NOTE.into());
            expected_proposal.common.updated_at = Some(meta(REVIEW).created_at);
            assert_eq!(actual_claim, expected_claim);
            assert_eq!(actual_proposal, expected_proposal);
            match reopened.outcome(REVIEWER, REVIEW) {
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
                    panic!("world {index} has candidate content without its outcome: {other:?}")
                }
            }
        }
        assert!(
            history
                .accepts(
                    2,
                    &World {
                        base_version: 2,
                        commits
                    }
                )
                .unwrap(),
            "sequence oracle rejected world {index}"
        );
        let actual_text = reopened
            .read_committed_blob(actual_claim.text_ref, BYTES, &mut ledger())
            .unwrap();
        assert_eq!(
            actual_text.as_bytes(),
            if head == 3 {
                b"original claim text".as_slice()
            } else {
                TEXT.as_bytes()
            }
        );
        examined += 1;
    }
    assert_eq!(examined, worlds.sampled);
    assert!(
        old_seen && new_seen,
        "this control must actually exercise BOTH whole alternatives"
    );
    assert_eq!(pin.entries(), pin_entries);
    println!(
        "ROOT_P2_ATOMIC_SAMPLED={} EXAMINED={examined} OLD={old_seen} NEW={new_seen}",
        worlds.sampled
    );
}

/// The one prepared event, or a panic naming the verdict that refused it.
fn prepared(outcome: FormatOutcome<OwnedEvent>) -> OwnedEvent {
    match outcome {
        FormatOutcome::Ok(event) => event,
        other => panic!("canonical preparation refused: {other:?}"),
    }
}

/// The caller-input verdict of a refused command.
fn input_error<T: core::fmt::Debug>(result: Result<T, CommandError>) -> InputError {
    match result {
        Err(CommandError::Input(error)) => error,
        other => panic!("expected a caller-input verdict, got {other:?}"),
    }
}

/// The authorization verdict of a refused command.
fn auth_error<T: core::fmt::Debug>(result: Result<T, CommandError>) -> AuthError {
    match result {
        Err(CommandError::Auth(error)) => error,
        other => panic!("expected an authorization verdict, got {other:?}"),
    }
}

/// The refusal of a refused command, whether it surfaced first at authorization or later.
fn refusal_of<T: core::fmt::Debug>(result: Result<T, CommandError>) -> Refusal {
    match result {
        Err(CommandError::Refused(refusal))
        | Err(CommandError::Auth(AuthError::Refused(refusal))) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap()
}

fn term_key(id: u32) -> RecordKey {
    RecordKey::term(id)
}

fn claim_key(id: u128) -> RecordKey {
    RecordKey::new(RecordKind::Claim, id)
}

fn proposal_key(id: u128) -> RecordKey {
    RecordKey::new(RecordKind::Proposal, id)
}

fn proposer() -> Grants<'static> {
    Grants::new(PRINCIPAL, kb(), CapabilitySet::of(&[Capability::Propose]))
}

fn reviewer() -> Grants<'static> {
    Grants::new(REVIEWER, kb(), CapabilitySet::of(&[Capability::Review]))
}

/// Per-request metadata whose transaction id is unique to the request.
///
/// A transaction's `tx_id` is part of its published file name, so two attempts that reuse
/// one would collide at publication; deriving it from the request id keeps every attempt
/// distinct while the canonical digest stays independent of it.
fn meta(request_id: &str) -> RequestMeta<'_> {
    let digest = raw_sha256(request_id.as_bytes());
    let mut tx_id = [0u8; 16];
    tx_id.copy_from_slice(&digest[..16]);
    RequestMeta {
        request_id,
        tx_id,
        created_at: 1_700_000_100_000_000,
    }
}

fn claim_record(id: u128, concept: u32) -> ClaimRecord {
    ClaimRecord {
        key: claim_key(id),
        revision: RecordRevision::FIRST,
        status: ClaimStatus::Accepted,
        concept: term_key(concept),
        kind: ClaimKind::Premise,
        text_ref: raw_sha256(b"original claim text"),
        confidence: Confidence::Hypothetical,
        common: CommonFields {
            recorded_by: Some(PRINCIPAL.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_001,
            updated_at: None,
        },
        legacy_id: Some("legacy-claim".into()),
        legacy_content_digest: Some([3; 32]),
        extensions: {
            let mut ext = Extensions::new();
            ext.insert("fixture.note", b"preserved".to_vec()).unwrap();
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
    principal: &str,
) -> RequestOutcomeEvent {
    RequestOutcomeEvent {
        principal_id: principal.into(),
        request_id: request_id.into(),
        payload_digest: digest,
        result_ref,
        commit_version: version,
        audit: CreationAudit {
            recorded_by: Some(principal.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_000 + version as i64,
        },
        extensions: Extensions::new(),
    }
}

/// One seeded commit: the caller states its non-outcome events and their blobs.
struct Seed {
    version: u64,
    events: Vec<OwnedEvent>,
    blobs: Vec<BlobDeclaration>,
    affected: Vec<RevPair>,
}

/// Commit a seed transaction on `store` with the real rules binding.
fn commit_seed(
    store: &mut Store<ObservingFs>,
    seed: Seed,
    request_id: &str,
    ledger: &mut AttemptLedger<'_>,
) {
    let basis = store
        .commit_basis(sage_db_types::persist::IdSpace::ProposalId, ledger)
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
            PRINCIPAL,
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
        tx_id: [0x11 + seed.version as u8; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000 + seed.version as i64,
        principal_id: PRINCIPAL.into(),
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

fn blob(bytes: Vec<u8>) -> BlobDeclaration {
    BlobDeclaration {
        sha256: raw_sha256(&bytes),
        declared_len: bytes.len() as u64,
        source: BlobSource::Provide(bytes),
    }
}

/// Bootstrap a KB holding two live terms and two accepted claims on them.
///
/// Terms come from the fixture's canonical `TermCreated` image; claims go through the
/// `typed` bridge, so every seeded row is byte-identical to a writer's own output.
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

    let claim_one = claim_record(1, 1);
    let claim_two = claim_record(2, 2);
    let mut events = vec![fixture::term(1, 1), fixture::term(2, 1)];
    for record in [&claim_one, &claim_two] {
        events.push(prepared(typed::prepare_event(
            EventInput::Claim(typed::ClaimEvent::Created, record),
            &mut l,
        )));
    }
    let blobs = vec![
        blob(fixture::body(1, 1)),
        blob(fixture::body(2, 1)),
        blob(b"original claim text".to_vec()),
    ];
    let affected = vec![
        RevPair {
            key: term_key(1),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: term_key(2),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: claim_one.key,
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: claim_two.key,
            revision: RecordRevision::FIRST,
        },
    ];
    commit_seed(
        &mut store,
        Seed {
            version: 2,
            events,
            blobs,
            affected,
        },
        "seed-2",
        &mut l,
    );
    (inner, fs, store)
}

fn claim_at(store: &Store<ObservingFs>, key: RecordKey) -> ClaimRecord {
    let mut l = ledger();
    match store.record(key, &mut l).unwrap() {
        sage_db_store::view::RecordLookup::Found {
            record: FoldedRecord::Claim(claim),
            ..
        } => claim.clone(),
        other => panic!("expected a claim at {key:?}: {other:?}"),
    }
}

fn proposal_at(store: &Store<ObservingFs>, key: RecordKey) -> ProposalRecord {
    let mut l = ledger();
    match store.record(key, &mut l).unwrap() {
        sage_db_store::view::RecordLookup::Found {
            record: FoldedRecord::Proposal(proposal),
            ..
        } => proposal.clone(),
        other => panic!("expected a proposal at {key:?}: {other:?}"),
    }
}

fn record_revision(store: &Store<ObservingFs>, key: RecordKey) -> Option<RecordRevision> {
    let mut l = ledger();
    match store.record(key, &mut l).unwrap() {
        sage_db_store::view::RecordLookup::Found { record, .. } => Some(match record {
            FoldedRecord::Term(term) => term.revision,
            FoldedRecord::Claim(claim) => claim.revision,
            FoldedRecord::Proposal(proposal) => proposal.revision,
            other => panic!("unexpected record kind: {other:?}"),
        }),
        sage_db_store::view::RecordLookup::Absent { .. } => None,
        other => panic!("incomplete record read: {other:?}"),
    }
}

fn submit(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    text: &str,
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, CommandError> {
    let edit = ClaimEdit {
        subject: term_key(1),
        claim: claim_key(1),
        text,
        declared_author: "declared-author, not the executor",
    };
    let read_set = [
        RevPair {
            key: term_key(1),
            revision: record_revision(store, term_key(1)).unwrap(),
        },
        RevPair {
            key: claim_key(1),
            revision: record_revision(store, claim_key(1)).unwrap(),
        },
    ];
    submit_claim_edit(
        store,
        &proposer(),
        meta(request_id),
        edit,
        &read_set,
        &Limits::V1,
        ledger,
    )
}

/// Replay the exact request `submit` made earlier: same payload, same read set.
///
/// An exact retry is byte-identical to the original — including its now-stale baselines —
/// which is precisely what makes "answer from the stored outcome" distinguishable from
/// "re-evaluate the current world".
fn submit_exact(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    text: &str,
    read_set: &[RevPair],
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, CommandError> {
    let edit = ClaimEdit {
        subject: term_key(1),
        claim: claim_key(1),
        text,
        declared_author: "declared-author, not the executor",
    };
    submit_claim_edit(
        store,
        &proposer(),
        meta(request_id),
        edit,
        read_set,
        &Limits::V1,
        ledger,
    )
}

/// The two-object baseline a fresh submit would declare right now.
fn current_baseline(store: &Store<ObservingFs>) -> [RevPair; 2] {
    [
        RevPair {
            key: term_key(1),
            revision: record_revision(store, term_key(1)).unwrap(),
        },
        RevPair {
            key: claim_key(1),
            revision: record_revision(store, claim_key(1)).unwrap(),
        },
    ]
}

fn submit_at(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    subject: u32,
    claim: u128,
    text: &str,
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, CommandError> {
    let edit = ClaimEdit {
        subject: term_key(subject),
        claim: claim_key(claim),
        text,
        declared_author: "author",
    };
    let read_set = [
        RevPair {
            key: term_key(subject),
            revision: record_revision(store, term_key(subject)).unwrap(),
        },
        RevPair {
            key: claim_key(claim),
            revision: record_revision(store, claim_key(claim)).unwrap(),
        },
    ];
    submit_claim_edit(
        store,
        &proposer(),
        meta(request_id),
        edit,
        &read_set,
        &Limits::V1,
        ledger,
    )
}

fn review(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    proposal: RecordKey,
    decision: ReviewDecision,
    note: &str,
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, CommandError> {
    let revision = record_revision(store, proposal).unwrap();
    review_claim_edit(
        store,
        &reviewer(),
        meta(request_id),
        proposal,
        revision,
        decision,
        note,
        &Limits::V1,
        ledger,
    )
}

/// Replay the exact review request made earlier, carrying its ORIGINAL proposal revision.
fn review_exact(
    store: &mut Store<ObservingFs>,
    request_id: &str,
    proposal: RecordKey,
    expected_revision: RecordRevision,
    decision: ReviewDecision,
    note: &str,
    ledger: &mut AttemptLedger<'_>,
) -> Result<HandleOutcome, CommandError> {
    review_claim_edit(
        store,
        &reviewer(),
        meta(request_id),
        proposal,
        expected_revision,
        decision,
        note,
        &Limits::V1,
        ledger,
    )
}

fn committed_version(outcome: &HandleOutcome) -> u64 {
    match outcome {
        HandleOutcome::Committed { version, .. } => *version,
        other => panic!("expected a committed outcome: {other:?}"),
    }
}

/// The outcome payload bytes of a committed transaction, read back through the store.
fn result_bytes(store: &mut Store<ObservingFs>, outcome: &HandleOutcome) -> Vec<u8> {
    let reference = match outcome {
        HandleOutcome::Committed { recorded, .. }
        | HandleOutcome::AlreadyCommitted { recorded, .. } => match recorded {
            sage_db_store::commit::RecordedOutcome::Outcome(shared) => shared.event().result_ref,
            other => panic!("no recorded outcome: {other:?}"),
        },
        other => panic!("no committed outcome: {other:?}"),
    };
    let mut l = ledger();
    let blob = store
        .read_committed_blob(reference, 1 << 20, &mut l)
        .unwrap();
    blob.as_bytes().to_vec()
}

#[test]
fn submit_pending_approve_atomic_reopen_and_exact_retry() {
    let (_inner, fs, mut store) = seed_world();
    let mut l = ledger();
    let before_claims = claim_at(&store, claim_key(1));
    assert_eq!(before_claims.revision, RecordRevision::FIRST);
    let original_baseline = current_baseline(&store);

    // 1. Submit: a pending proposal, no content change.
    let submit_outcome = submit(&mut store, "submit-1", "edited claim text", &mut l).unwrap();
    let submit_version = committed_version(&submit_outcome);
    let proposal = proposal_at(&store, proposal_key(1));
    assert_eq!(proposal.status, ProposalStatus::Pending);
    assert_eq!(proposal.kind, ProposalKind::ClaimEdit);
    assert_eq!(proposal.subject, term_key(1));
    assert_eq!(
        proposal.declared_author,
        "declared-author, not the executor"
    );
    assert_eq!(proposal.base_revisions.len(), 2);
    assert_eq!(
        claim_at(&store, claim_key(1)).revision,
        RecordRevision::FIRST
    );

    // The result blob is the §4.2 canonical shape with kind 0 and the counter advanced.
    let result = result_bytes(&mut store, &submit_outcome);
    assert_eq!(u16::from_le_bytes([result[0], result[1]]), 0);
    assert_eq!(
        u64::from_le_bytes(result[2..10].try_into().unwrap()),
        submit_version
    );
    assert_eq!(u32::from_le_bytes(result[10..14].try_into().unwrap()), 1);
    assert_eq!(
        u16::from_le_bytes([result[14], result[15]]),
        RecordKind::Proposal.as_u16()
    );
    // 2 kind bytes + 8 version + 4 affected count + one 26-byte pair = 40.
    assert_eq!(
        u32::from_le_bytes(result[40..44].try_into().unwrap()),
        1,
        "submit advances exactly one counter"
    );
    assert_eq!(
        u16::from_le_bytes([result[44], result[45]]),
        IdSpace::ProposalId.as_u16()
    );
    assert_eq!(
        u128::from_le_bytes(result[46..62].try_into().unwrap()),
        1,
        "the allocation mark is the id just used"
    );
    assert_eq!(result.len(), 62, "no trailing bytes in the result blob");

    // 2. Drop and reopen: the pending proposal and the untouched content survive.
    drop(store);
    let mut l2 = ledger();
    let mut store = match open_existing(
        fs.clone(),
        Path::new(ROOT),
        Some(kb()),
        &Limits::V1,
        &mut l2,
    )
    .unwrap()
    {
        Opened::Ready(store) => store,
        Opened::ReadOnly { .. } => panic!("expected a durable reopen, got a read-only one"),
    };
    assert_eq!(
        proposal_at(&store, proposal_key(1)).status,
        ProposalStatus::Pending
    );
    assert_eq!(
        claim_at(&store, claim_key(1)).text_ref,
        raw_sha256(b"original claim text")
    );

    // 3. Approve: claim and proposal move together, every other claim field preserved.
    let mut l3 = ledger();
    let approve = review(
        &mut store,
        "approve-1",
        proposal_key(1),
        ReviewDecision::Approve,
        "reviewed: reads correctly",
        &mut l3,
    )
    .unwrap();
    committed_version(&approve);
    let approved_claim = claim_at(&store, claim_key(1));
    assert_eq!(approved_claim.revision, RecordRevision::new(2).unwrap());
    assert_eq!(approved_claim.text_ref, raw_sha256(b"edited claim text"));
    assert_eq!(approved_claim.kind, before_claims.kind);
    assert_eq!(approved_claim.concept, before_claims.concept);
    assert_eq!(approved_claim.confidence, before_claims.confidence);
    assert_eq!(approved_claim.status, ClaimStatus::Accepted);
    assert_eq!(approved_claim.legacy_id, before_claims.legacy_id);
    assert_eq!(
        approved_claim.legacy_content_digest,
        before_claims.legacy_content_digest
    );
    assert_eq!(approved_claim.extensions, before_claims.extensions);
    assert_eq!(
        approved_claim.common.created_at,
        before_claims.common.created_at
    );
    assert_eq!(
        approved_claim.common.reason.as_deref(),
        Some("reviewed: reads correctly")
    );
    assert_eq!(approved_claim.common.recorded_by.as_deref(), Some(REVIEWER));
    let decided = proposal_at(&store, proposal_key(1));
    assert_eq!(decided.status, ProposalStatus::Approved);
    assert_eq!(decided.revision, RecordRevision::new(2).unwrap());
    assert_eq!(
        decided.declared_author, "declared-author, not the executor",
        "the declared author is attribution and never becomes the reviewer"
    );
    assert_eq!(
        decided.review_note.as_deref(),
        Some("reviewed: reads correctly")
    );

    // 4. Drop and reopen: the edited bytes and both record states are durable together.
    drop(store);
    let mut l4 = ledger();
    let mut store = match open_existing(
        fs.clone(),
        Path::new(ROOT),
        Some(kb()),
        &Limits::V1,
        &mut l4,
    )
    .unwrap()
    {
        Opened::Ready(store) => store,
        Opened::ReadOnly { .. } => panic!("expected a durable reopen, got a read-only one"),
    };
    let reopened_claim = claim_at(&store, claim_key(1));
    assert_eq!(reopened_claim.revision, approved_claim.revision);
    assert_eq!(reopened_claim.text_ref, raw_sha256(b"edited claim text"));
    assert_eq!(
        proposal_at(&store, proposal_key(1)).status,
        ProposalStatus::Approved
    );

    // 5. Exact retries: the ORIGINAL version and payload, and no mutating seam call.
    let mut l5 = ledger();
    let before = fs.counts().borrow().total();
    let retried_submit = submit_exact(
        &mut store,
        "submit-1",
        "edited claim text",
        &original_baseline,
        &mut l5,
    )
    .unwrap();
    let retried_approve = review_exact(
        &mut store,
        "approve-1",
        proposal_key(1),
        RecordRevision::FIRST,
        ReviewDecision::Approve,
        "reviewed: reads correctly",
        &mut l5,
    )
    .unwrap();
    assert_eq!(fs.counts().borrow().total(), before);
    match (&retried_submit, &retried_approve) {
        (
            HandleOutcome::AlreadyCommitted {
                version: submit_version_again,
                ..
            },
            HandleOutcome::AlreadyCommitted {
                version: approve_version_again,
                ..
            },
        ) => {
            assert_eq!(*submit_version_again, submit_version);
            assert_eq!(*approve_version_again, committed_version(&approve));
        }
        other => panic!("an exact retry must return the original outcome: {other:?}"),
    }
    // And the retried payloads are byte-identical to the originals.
    assert_eq!(
        result_bytes(&mut store, &retried_submit),
        result_bytes(&mut store, &submit_outcome)
    );
    assert_eq!(
        result_bytes(&mut store, &retried_approve),
        result_bytes(&mut store, &approve)
    );

    // A different request id on the same content is a NEW request, not a retry.
    let mut l6 = ledger();
    let second = submit(&mut store, "submit-2", "edited claim text", &mut l6).unwrap();
    committed_version(&second);
    assert_ne!(proposal_at(&store, proposal_key(2)).key, proposal_key(1));
}

#[test]
fn second_approval_after_a_competing_edit_is_a_durable_conflict() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    submit(&mut store, "submit-a", "first edit", &mut l).unwrap();
    submit(&mut store, "submit-b", "second edit", &mut l).unwrap();
    // An unrelated proposal on the other claim stays pending and blocks nothing.
    submit_at(&mut store, "submit-c", 2, 2, "unrelated edit", &mut l).unwrap();
    assert_eq!(
        proposal_at(&store, proposal_key(3)).status,
        ProposalStatus::Pending
    );

    let approve = review(
        &mut store,
        "approve-a",
        proposal_key(1),
        ReviewDecision::Approve,
        "first wins",
        &mut l,
    )
    .unwrap();
    committed_version(&approve);
    let after_first = claim_at(&store, claim_key(1));
    assert_eq!(after_first.text_ref, raw_sha256(b"first edit"));

    // The second proposal's two-object baseline is now stale: a committed conflict.
    let conflicted = review(
        &mut store,
        "approve-b",
        proposal_key(2),
        ReviewDecision::Approve,
        "stale baseline",
        &mut l,
    )
    .unwrap();
    committed_version(&conflicted);
    let after_second = claim_at(&store, claim_key(1));
    assert_eq!(
        after_second.text_ref,
        raw_sha256(b"first edit"),
        "a conflict never writes content"
    );
    assert_eq!(after_second.revision, after_first.revision);
    assert_eq!(
        proposal_at(&store, proposal_key(2)).status,
        ProposalStatus::Conflicted
    );
    // The conflict is a committed transaction whose result is still kind 0.
    let result = result_bytes(&mut store, &conflicted);
    assert_eq!(u16::from_le_bytes([result[0], result[1]]), 0);

    // The unrelated proposal is still legal to decide.
    let unrelated = review(
        &mut store,
        "approve-c",
        proposal_key(3),
        ReviewDecision::Approve,
        "independent",
        &mut l,
    )
    .unwrap();
    committed_version(&unrelated);
    assert_eq!(
        claim_at(&store, claim_key(2)).text_ref,
        raw_sha256(b"unrelated edit")
    );
}

#[test]
fn a_claim_status_change_makes_the_pending_edit_a_durable_conflict() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    submit(&mut store, "submit-x", "would-be edit", &mut l).unwrap();

    // A later revision supersedes the claim the proposal was written against. Its revision
    // and its status both moved, so no approval of the old candidate can be correct.
    let mut superseded = claim_at(&store, claim_key(1));
    assert_eq!(superseded.revision, RecordRevision::FIRST);
    superseded.revision = RecordRevision::new(2).unwrap();
    superseded.status = ClaimStatus::Superseded;
    superseded.common.reason = Some("superseded elsewhere".into());
    superseded.common.updated_at = Some(1_700_000_000_000_900);
    let version = store.head().unwrap() + 1;
    let event = prepared(typed::prepare_event(
        EventInput::Claim(typed::ClaimEvent::Superseded, &superseded),
        &mut l,
    ));
    let result_bytes = typed::result_payload(
        version,
        &[RevPair {
            key: superseded.key,
            revision: superseded.revision,
        }],
        &[],
        &mut l,
    )
    .unwrap();
    let result_ref = raw_sha256(&result_bytes);
    let events = vec![
        event,
        prepared(typed::prepare_event(
            EventInput::Outcome(&outcome_event(
                version,
                "seed-supersede",
                [3; 32],
                result_ref,
                PRINCIPAL,
            )),
            &mut l,
        )),
    ];
    let basis = store.commit_basis(IdSpace::ProposalId, &mut l).unwrap();
    let prepared_tx = PreparedTransaction {
        version,
        prev_version: Some(version - 1),
        tx_id: [0x33; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_003,
        principal_id: PRINCIPAL.into(),
        request_id: "seed-supersede".into(),
        payload_digest: [3; 32],
        expect_version: Some(version - 1),
        extensions: Extensions::new(),
        events,
        blobs: vec![
            BlobDeclaration {
                sha256: basis.rules.rules_ref,
                declared_len: basis.rules_len,
                source: BlobSource::Existing,
            },
            // The claim's prose is already durable from the seed and is re-declared as
            // present with its real length, never as a zero-length placeholder.
            BlobDeclaration {
                sha256: superseded.text_ref,
                declared_len: b"original claim text".len() as u64,
                source: BlobSource::Existing,
            },
            BlobDeclaration {
                sha256: result_ref,
                declared_len: result_bytes.len() as u64,
                source: BlobSource::Provide(result_bytes),
            },
        ],
        rules: basis.rules,
    };
    match store.commit(prepared_tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed {
            version: committed, ..
        } => assert_eq!(committed, version),
        other => panic!("supersede seed failed: {other:?}"),
    }

    // The approval reaches the baseline check first: the outcome is a committed conflict,
    // not an unpersisted "claim is not accepted" refusal that would leave it pending.
    let conflicted = review(
        &mut store,
        "approve-x",
        proposal_key(1),
        ReviewDecision::Approve,
        "obsolete candidate",
        &mut l,
    )
    .unwrap();
    committed_version(&conflicted);
    assert_eq!(
        proposal_at(&store, proposal_key(1)).status,
        ProposalStatus::Conflicted
    );
    // No content was written: the claim is still exactly the superseding revision.
    assert_eq!(claim_at(&store, claim_key(1)), superseded);
}

#[test]
fn reject_is_durable_and_touches_no_content() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    submit(&mut store, "submit-r", "rejected edit", &mut l).unwrap();
    let before = claim_at(&store, claim_key(1));
    let reject = review(
        &mut store,
        "reject-r",
        proposal_key(1),
        ReviewDecision::Reject,
        "not this one",
        &mut l,
    )
    .unwrap();
    committed_version(&reject);
    let after = claim_at(&store, claim_key(1));
    assert_eq!(after, before, "rejection changes no claim field");
    let proposal = proposal_at(&store, proposal_key(1));
    assert_eq!(proposal.status, ProposalStatus::Rejected);
    assert_eq!(proposal.review_note.as_deref(), Some("not this one"));
    // A decided proposal cannot be newly reviewed.
    let mut l2 = ledger();
    assert_eq!(
        input_error(review(
            &mut store,
            "reject-again",
            proposal_key(1),
            ReviewDecision::Approve,
            "second opinion",
            &mut l2,
        )),
        InputError::ProposalNotPending(ProposalStatus::Rejected)
    );
}

#[test]
fn capabilities_kb_and_input_bounds_are_enforced_before_any_write() {
    let (_inner, fs, mut store) = seed_world();
    let before = fs.counts().borrow().total();
    let mut l = ledger();

    let edit = ClaimEdit {
        subject: term_key(1),
        claim: claim_key(1),
        text: "t",
        declared_author: "a",
    };
    let read_set = [
        RevPair {
            key: term_key(1),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: claim_key(1),
            revision: RecordRevision::FIRST,
        },
    ];

    // A reviewer cannot propose, and a grant scoped to another KB is refused outright.
    assert_eq!(
        auth_error(submit_claim_edit(
            &mut store,
            &reviewer(),
            meta("cap-1"),
            edit,
            &read_set,
            &Limits::V1,
            &mut l
        )),
        AuthError::MissingCapability {
            capability: Capability::Propose
        }
    );
    let wrong_kb = Grants::new(
        PRINCIPAL,
        KbUuid::from_bytes([9; 16]),
        CapabilitySet::of(&[Capability::Propose]),
    );
    assert_eq!(
        auth_error(submit_claim_edit(
            &mut store,
            &wrong_kb,
            meta("cap-2"),
            edit,
            &read_set,
            &Limits::V1,
            &mut l
        )),
        AuthError::WrongKb {
            expected: kb(),
            found: KbUuid::from_bytes([9; 16])
        }
    );

    // A missing baseline row, and a baseline naming the wrong object.
    assert_eq!(
        input_error(submit_claim_edit(
            &mut store,
            &proposer(),
            meta("cap-3"),
            edit,
            &read_set[..1],
            &Limits::V1,
            &mut l
        )),
        InputError::ReadSetLength {
            expected: 2,
            found: 1
        }
    );
    let swapped = [
        RevPair {
            key: claim_key(1),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: term_key(1),
            revision: RecordRevision::FIRST,
        },
    ];
    assert_eq!(
        input_error(submit_claim_edit(
            &mut store,
            &proposer(),
            meta("cap-4"),
            edit,
            &swapped,
            &Limits::V1,
            &mut l
        )),
        InputError::ReadSetKey {
            index: 0,
            expected: term_key(1),
            found: claim_key(1)
        }
    );

    // Empty text and an over-ceiling author are caller verdicts, not encoder internals.
    let empty = ClaimEdit { text: "", ..edit };
    assert_eq!(
        input_error(submit_claim_edit(
            &mut store,
            &proposer(),
            meta("cap-5"),
            empty,
            &read_set,
            &Limits::V1,
            &mut l
        )),
        InputError::Payload(PayloadError::EmptyText)
    );
    let long_author = "a".repeat(usize::try_from(typed::MAX_DECLARED_AUTHOR).unwrap() + 1);
    let too_long = ClaimEdit {
        declared_author: &long_author,
        ..edit
    };
    assert_eq!(
        input_error(submit_claim_edit(
            &mut store,
            &proposer(),
            meta("cap-6"),
            too_long,
            &read_set,
            &Limits::V1,
            &mut l
        )),
        InputError::DeclaredAuthorTooLong {
            len: long_author.len(),
            max: typed::MAX_DECLARED_AUTHOR
        }
    );

    // A stale submitted baseline is refused before any authoritative write.
    let stale = [
        RevPair {
            key: term_key(1),
            revision: RecordRevision::new(7).unwrap(),
        },
        RevPair {
            key: claim_key(1),
            revision: RecordRevision::FIRST,
        },
    ];
    assert_eq!(
        input_error(submit_claim_edit(
            &mut store,
            &proposer(),
            meta("cap-7"),
            edit,
            &stale,
            &Limits::V1,
            &mut l
        )),
        InputError::StaleBaseline {
            key: term_key(1),
            expected: RecordRevision::new(7).unwrap()
        }
    );

    // A subject that is not the claim's concept.
    let wrong_subject = ClaimEdit {
        subject: term_key(2),
        ..edit
    };
    let wrong_read = [
        RevPair {
            key: term_key(2),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: claim_key(1),
            revision: RecordRevision::FIRST,
        },
    ];
    assert_eq!(
        input_error(submit_claim_edit(
            &mut store,
            &proposer(),
            meta("cap-8"),
            wrong_subject,
            &wrong_read,
            &Limits::V1,
            &mut l
        )),
        InputError::SubjectMismatch {
            declared: term_key(2),
            actual: term_key(1)
        }
    );

    // Wrong-key-kind positions are refused before any payload is built.
    let bad_positions = ClaimEdit {
        subject: claim_key(1),
        claim: term_key(1),
        ..edit
    };
    assert_eq!(
        input_error(submit_claim_edit(
            &mut store,
            &proposer(),
            meta("cap-9"),
            bad_positions,
            &read_set,
            &Limits::V1,
            &mut l
        )),
        InputError::WrongKeyKind {
            expected: RecordKind::Term,
            found: RecordKind::Claim
        }
    );

    assert_eq!(
        fs.counts().borrow().total(),
        before,
        "no refused input reached the seam"
    );
}

#[test]
fn stored_payload_defects_are_distinguished_from_caller_input() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    // Four otherwise structured proposals each pin one malformed/unsupported field,
    // plus a healthy no-pin control sharing the pinned proposal's other values.
    let base = [
        RevPair {
            key: term_key(1),
            revision: RecordRevision::FIRST,
        },
        RevPair {
            key: claim_key(1),
            revision: RecordRevision::FIRST,
        },
    ];
    let unsupported_frame = {
        let payload = b"unsupported".to_vec();
        let ops = [typed::RequestOp::new(999, 1, &payload)];
        let frame = typed::RequestFrame::new(&ops, &base, None);
        match typed::encode_request(&frame, &mut l) {
            typed::RequestOutcome::Ok(bytes) => bytes,
            other => panic!("frame encode: {other:?}"),
        }
    };
    let truncated_frame = {
        let edit = ClaimEdit {
            subject: term_key(1),
            claim: claim_key(1),
            text: "x",
            declared_author: "a",
        };
        let payload = match sage_db_kernel::intent::encode_submit_edit(&edit, &mut l) {
            Ok(bytes) => bytes,
            Err(error) => panic!("payload encode: {error:?}"),
        };
        let cut = &payload[..payload.len() - 1];
        let ops = [typed::RequestOp::new(
            sage_db_kernel::intent::SUBMIT_CLAIM_EDIT,
            sage_db_kernel::intent::OPERATION_VERSION,
            cut,
        )];
        let frame = typed::RequestFrame::new(&ops, &base, None);
        match typed::encode_request(&frame, &mut l) {
            typed::RequestOutcome::Ok(bytes) => bytes,
            other => panic!("frame encode: {other:?}"),
        }
    };
    let mismatch_frame = {
        let edit = ClaimEdit {
            subject: term_key(2),
            claim: claim_key(1),
            text: "x",
            declared_author: "a",
        };
        let payload = match sage_db_kernel::intent::encode_submit_edit(&edit, &mut l) {
            Ok(bytes) => bytes,
            Err(error) => panic!("payload encode: {error:?}"),
        };
        let ops = [typed::RequestOp::new(
            sage_db_kernel::intent::SUBMIT_CLAIM_EDIT,
            sage_db_kernel::intent::OPERATION_VERSION,
            &payload,
        )];
        let frame = typed::RequestFrame::new(&ops, &base, None);
        match typed::encode_request(&frame, &mut l) {
            typed::RequestOutcome::Ok(bytes) => bytes,
            other => panic!("frame encode: {other:?}"),
        }
    };

    // A canonically encoded frame that pins a whole-KB `expected_version`. P1 accepts it
    // (the pin is a legal frame field), but this submission schema names its two objects
    // explicitly and never pins a version, so the reviewer must refuse it by name instead
    // of ignoring the field.
    let pinned_frame = {
        let edit = ClaimEdit {
            subject: term_key(1),
            claim: claim_key(1),
            text: "pinned",
            declared_author: "a",
        };
        let payload = match sage_db_kernel::intent::encode_submit_edit(&edit, &mut l) {
            Ok(bytes) => bytes,
            Err(error) => panic!("payload encode: {error:?}"),
        };
        let ops = [typed::RequestOp::new(
            sage_db_kernel::intent::SUBMIT_CLAIM_EDIT,
            sage_db_kernel::intent::OPERATION_VERSION,
            &payload,
        )];
        let frame = typed::RequestFrame::new(&ops, &base, Some(2));
        match typed::encode_request(&frame, &mut l) {
            typed::RequestOutcome::Ok(bytes) => bytes,
            other => panic!("frame encode: {other:?}"),
        }
    };

    let healthy_frame = {
        let edit = ClaimEdit {
            subject: term_key(1),
            claim: claim_key(1),
            text: "pinned",
            declared_author: "a",
        };
        let payload = sage_db_kernel::intent::encode_submit_edit(&edit, &mut l).unwrap();
        let ops = [typed::RequestOp::new(1, 1, &payload)];
        match typed::encode_request(&typed::RequestFrame::new(&ops, &base, None), &mut l) {
            typed::RequestOutcome::Ok(bytes) => bytes,
            other => panic!("healthy frame refused: {other:?}"),
        }
    };

    let mut events = Vec::new();
    let mut blobs = Vec::new();
    let mut affected = Vec::new();
    for (index, (bytes, subject)) in [
        (unsupported_frame, term_key(1)),
        (truncated_frame, term_key(1)),
        (mismatch_frame, term_key(1)),
        (pinned_frame, term_key(1)),
        (healthy_frame, term_key(1)),
    ]
    .into_iter()
    .enumerate()
    {
        let key = proposal_key(u128::try_from(index).unwrap() + 1);
        let record = ProposalRecord {
            key,
            revision: RecordRevision::FIRST,
            status: ProposalStatus::Pending,
            kind: ProposalKind::ClaimEdit,
            subject,
            base_revisions: vec![
                RevPair {
                    key: term_key(1),
                    revision: RecordRevision::FIRST,
                },
                RevPair {
                    key: claim_key(1),
                    revision: RecordRevision::FIRST,
                },
            ],
            payload_ref: raw_sha256(&bytes),
            review_note: None,
            declared_author: "a".into(),
            common: CommonFields {
                recorded_by: Some(PRINCIPAL.into()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1_700_000_000_000_050,
                updated_at: None,
            },
            legacy_id: None,
            extensions: Extensions::new(),
            changed_fields: None,
        };
        events.push(prepared(typed::prepare_event(
            EventInput::Proposal {
                decided: false,
                record: &record,
            },
            &mut l,
        )));
        blobs.push(blob(bytes));
        affected.push(RevPair {
            key,
            revision: RecordRevision::FIRST,
        });
    }
    commit_seed(
        &mut store,
        Seed {
            version: 3,
            events,
            blobs,
            affected,
        },
        "seed-3",
        &mut l,
    );

    let mut l2 = ledger();
    assert_eq!(
        input_error(review(
            &mut store,
            "review-unsupported",
            proposal_key(1),
            ReviewDecision::Approve,
            "n",
            &mut l2
        )),
        InputError::UnsupportedStoredOperation {
            kind: 999,
            version: 1
        }
    );
    assert_eq!(
        input_error(review(
            &mut store,
            "review-truncated",
            proposal_key(2),
            ReviewDecision::Approve,
            "n",
            &mut l2
        )),
        InputError::PersistedPayload(PayloadError::Truncated)
    );
    assert_eq!(
        input_error(review(
            &mut store,
            "review-mismatch",
            proposal_key(3),
            ReviewDecision::Approve,
            "n",
            &mut l2
        )),
        InputError::StoredMetadataMismatch(StoredField::Subject)
    );
    assert_eq!(
        input_error(review(
            &mut store,
            "review-pinned",
            proposal_key(4),
            ReviewDecision::Approve,
            "n",
            &mut l2
        )),
        InputError::StoredExpectedVersion { pinned: 2 }
    );
    assert_eq!(
        proposal_at(&store, proposal_key(4)).status,
        ProposalStatus::Pending,
        "a refused stored schema leaves the proposal pending"
    );
    assert_eq!(
        claim_at(&store, claim_key(1)).revision,
        RecordRevision::FIRST
    );
    // The positive differs from the pinned request ONLY in the unsupported frame
    // pin (its target Proposal identity is metadata outside that frame).
    assert!(matches!(
        review(
            &mut store,
            "review-healthy",
            proposal_key(5),
            ReviewDecision::Approve,
            "n",
            &mut l2
        ),
        Ok(HandleOutcome::Committed { version: 4, .. })
    ));
    assert_eq!(
        proposal_at(&store, proposal_key(5)).status,
        ProposalStatus::Approved
    );
    assert_eq!(
        claim_at(&store, claim_key(1)).text_ref,
        raw_sha256(b"pinned")
    );
}

#[test]
fn a_generated_length_invariant_is_internal_and_never_caller_input() {
    // The writer's own layout invariant is not something a caller wrote, so it must not be
    // reported as `Input`. Pinned here because the mapping is reachable only through the
    // codec's internal arm, which no public encoder can trip.
    assert!(
        matches!(
            CommandError::from(PayloadError::InternalInvariant),
            CommandError::Internal(_)
        ),
        "an internal payload invariant must stay internal"
    );
    assert!(
        matches!(
            CommandError::from(PayloadError::Refused(Refusal::Budget(BudgetError::new(
                BudgetUnit::Bytes,
                1,
                0
            )))),
            CommandError::Refused(Refusal::Budget(_))
        ),
        "a resource refusal stays canonical"
    );
    assert!(
        matches!(
            CommandError::from(PayloadError::Truncated),
            CommandError::Input(InputError::Payload(PayloadError::Truncated))
        ),
        "malformed caller bytes stay caller input"
    );
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
        refusal_of(submit(&mut store, "stopped", "t", &mut stopped)),
        Refusal::Stopped(ControlStop::Cancelled)
    );

    let mut tiny = AttemptLedger::open(0, 0, &RunToCompletion).unwrap();
    assert!(matches!(
        submit(&mut store, "tiny", "t", &mut tiny),
        Err(CommandError::Refused(Refusal::Budget(error)))
            if error.unit() == BudgetUnit::Bytes
    ));
    assert_eq!(tiny.stats().live_bytes, 0);
    assert!(proposal_at_absent(&store, proposal_key(1)));
}

fn proposal_at_absent(store: &Store<ObservingFs>, key: RecordKey) -> bool {
    let mut l = ledger();
    matches!(
        store.record(key, &mut l).unwrap(),
        sage_db_store::view::RecordLookup::Absent { .. }
    )
}

#[test]
fn unknown_underneath_approval_resolves_to_a_whole_world_not_half() {
    let (inner, fs, mut store) = seed_world();
    let mut l = ledger();
    submit(&mut store, "submit-f", "faulted edit", &mut l).unwrap();
    let claim_before = claim_at(&store, claim_key(1));

    // The publication point reports an unknown effect after it took effect.
    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    let mut l2 = ledger();
    let outcome = review(
        &mut store,
        "approve-f",
        proposal_key(1),
        ReviewDecision::Approve,
        "faulted review",
        &mut l2,
    )
    .unwrap();
    assert!(matches!(outcome, HandleOutcome::Unknown));
    assert!(inner.faults_handle().borrow().all_fired());
    assert!(store.authority_undecided());

    // Resolve: whichever side won, the claim and the proposal agree.
    let mut l3 = ledger();
    assert!(matches!(
        store.resolve(&Limits::V1, &mut l3),
        Resolution::Ready
    ));
    let claim_now = claim_at(&store, claim_key(1));
    let proposal_now = proposal_at(&store, proposal_key(1));
    match proposal_now.status {
        ProposalStatus::Approved => {
            assert_eq!(claim_now.text_ref, raw_sha256(b"faulted edit"));
            assert_eq!(claim_now.revision, RecordRevision::new(2).unwrap());
        }
        ProposalStatus::Pending => {
            assert_eq!(claim_now, claim_before);
        }
        other => panic!("a resolved world cannot be half-decided: {other:?}"),
    }

    // The same holds after a fresh open of the root.
    drop(store);
    let mut l4 = ledger();
    let store = match open_existing(
        fs.clone(),
        Path::new(ROOT),
        Some(kb()),
        &Limits::V1,
        &mut l4,
    )
    .unwrap()
    {
        Opened::Ready(store) => store,
        Opened::ReadOnly { .. } => panic!("expected a durable reopen, got a read-only one"),
    };
    let reopened_claim = claim_at(&store, claim_key(1));
    let reopened_proposal = proposal_at(&store, proposal_key(1));
    assert_eq!(reopened_claim, claim_now);
    assert_eq!(reopened_proposal.status, proposal_now.status);
}

#[test]
fn a_retry_after_a_commit_is_answered_before_a_now_stale_baseline() {
    let (_inner, fs, mut store) = seed_world();
    let mut l = ledger();
    let original_baseline = current_baseline(&store);
    let submit_outcome = submit(&mut store, "submit-s", "text one", &mut l).unwrap();
    let submit_version = committed_version(&submit_outcome);
    review(
        &mut store,
        "approve-s",
        proposal_key(1),
        ReviewDecision::Approve,
        "ok",
        &mut l,
    )
    .unwrap();
    // The proposal is decided and the claim moved: every baseline this request named is
    // stale now. The retry must still answer with the original outcome and no writes.
    let before = fs.counts().borrow().total();
    let mut l2 = ledger();
    let retried = submit_exact(
        &mut store,
        "submit-s",
        "text one",
        &original_baseline,
        &mut l2,
    )
    .unwrap();
    assert_eq!(fs.counts().borrow().total(), before);
    match retried {
        HandleOutcome::AlreadyCommitted { version, recorded } => {
            assert_eq!(version, submit_version);
            match recorded {
                sage_db_store::commit::RecordedOutcome::Outcome(shared) => {
                    assert_eq!(
                        shared.event().result_ref,
                        match &submit_outcome {
                            HandleOutcome::Committed {
                                recorded: sage_db_store::commit::RecordedOutcome::Outcome(original),
                                ..
                            } => original.event().result_ref,
                            other => panic!("expected a recorded outcome: {other:?}"),
                        }
                    );
                }
                other => panic!("expected an outcome event: {other:?}"),
            }
        }
        other => panic!("expected AlreadyCommitted, got {other:?}"),
    }
    // A different digest under the same request id is a mismatch, not a retry.
    let mut l3 = ledger();
    assert!(
        matches!(
            submit_exact(
                &mut store,
                "submit-s",
                "text two",
                &original_baseline,
                &mut l3
            ),
            Err(CommandError::IdempotencyMismatch)
        ),
        "a different digest under the same request id is a mismatch, not a retry"
    );
}
