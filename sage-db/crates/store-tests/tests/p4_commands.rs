//! P4 authenticated command application adapter, over the real store and real kernel.
//!
//! Every request here travels the production path: a generated protocol DTO is decoded from
//! nothing (constructed directly), a real [`CredentialVerifier`] authenticates a real bearer
//! hash against the real store's KB on the caller's real ledger, the protocol's paired
//! validation runs on that **same** ledger, and the canonical kernel command executes against
//! a real [`Store`] on the model filesystem.
//!
//! What this file does **not** claim: HTTP, network ingress, response encoding, decode bounds,
//! or a deadline timer. None of those exist in this slice.

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
use sage_db_kernel::auth::{Capability, CapabilitySet};
use sage_db_store::fs::CreateDir;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::typed::{self, EventInput};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::confidence::Confidence;
use sage_db_types::framing::Limits;
use sage_db_types::identity::{KbUuid, RecordRevision};
use sage_db_types::persist::{
    ActorOrigin, ClaimKind, ClaimRecord, ClaimStatus, CommonFields, CreationAudit, Extensions,
    IdSpace, ProposalKind, RecordKey, RecordKind, RequestOutcomeEvent, RevPair,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};
use sage_db_types::query::RunToCompletion;

use sage_db_protocol::generated::{
    ConceptText, Decision, Envelope, FrontierState as WireFrontierState, RecordKey as WireKey,
    RevPair as WireRevPair, ReviewDecisionPayload, ReviewPayload, SessionCheckpointState,
    SubmitPayload, commit_payload, review_payload, submit_payload,
};
use sage_db_protocol::validation::{ValidationError, ValidationLimits};
use sage_db_server::commands::{
    CommandContext, CommandRefusal, ExecutionMeta, WriteCommand, commit, review, submit,
};
use sage_db_server::credentials::{
    BindingStatus, CredentialBinding, CredentialVerifier, TokenHash, VerifierLimits,
};

const PROPOSER_TOKEN: &[u8] = b"proposer-opaque-token-0123456789";
const REVIEWER_TOKEN: &[u8] = b"reviewer-opaque-token-0123456789";
const PROPOSER: &str = "authenticated-proposer";
const REVIEWER: &str = "authenticated-reviewer";

// ---------------------------------------------------------------------------
// World construction
// ---------------------------------------------------------------------------

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

fn wire_key(key: RecordKey) -> WireKey {
    WireKey {
        kind: u32::from(key.kind().as_u16()),
        id: key.id().to_string(),
    }
}

fn wire_pair(key: RecordKey, revision: RecordRevision) -> WireRevPair {
    WireRevPair {
        key: Some(wire_key(key)),
        revision: revision.get().to_string(),
    }
}

fn envelope(read_set: Vec<WireRevPair>, request_id: &str) -> Envelope {
    Envelope {
        protocol_version: 1,
        kb_uuid: kb().to_string(),
        request_id: request_id.to_owned(),
        deadline_ms: None,
        read_set,
        expect_version: None,
    }
}

/// A transaction id unique to the request, as an executor would supply.
fn execution(request_id: &str) -> ExecutionMeta {
    let digest = raw_sha256(request_id.as_bytes());
    let mut tx_id = [0u8; 16];
    tx_id.copy_from_slice(&digest[..16]);
    ExecutionMeta {
        tx_id,
        created_at: 1_700_000_200_000_000,
    }
}

fn token_hash(token: &[u8]) -> TokenHash {
    let digest = raw_sha256(token);
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    TokenHash::from_sha256(bytes)
}

/// The deployment's credential table: one proposer, one reviewer, both bound to this KB.
fn verifier() -> CredentialVerifier<'static> {
    let bindings = vec![
        CredentialBinding::new(
            token_hash(PROPOSER_TOKEN),
            PROPOSER,
            kb(),
            // A writer role: propose plus auxiliary writes (sessions and frontier entries).
            CapabilitySet::of(&[Capability::Propose, Capability::WriteAux]),
            BindingStatus::Active,
        ),
        CredentialBinding::new(
            token_hash(REVIEWER_TOKEN),
            REVIEWER,
            kb(),
            CapabilitySet::of(&[Capability::Review]),
            BindingStatus::Active,
        ),
    ];
    // Leaked deliberately: the fixture outlives every request in this file, and the verifier
    // borrows its table for the process lifetime, exactly as a deployment would.
    let bindings: &'static [CredentialBinding<'static>] = Box::leak(bindings.into_boxed_slice());
    let mut l = ledger();
    CredentialVerifier::new(bindings, VerifierLimits::default(), &mut l).unwrap()
}

fn prepared(outcome: sage_db_types::framing::FormatOutcome<OwnedEvent>) -> OwnedEvent {
    match outcome {
        sage_db_types::framing::FormatOutcome::Ok(event) => event,
        other => panic!("canonical preparation refused: {other:?}"),
    }
}

fn blob(bytes: Vec<u8>) -> BlobDeclaration {
    BlobDeclaration {
        sha256: raw_sha256(&bytes),
        declared_len: bytes.len() as u64,
        source: BlobSource::Provide(bytes),
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
            recorded_by: Some(PROPOSER.into()),
            actor_origin: ActorOrigin::Authenticated,
            reason: None,
            created_at: 1_700_000_000_000_001,
            updated_at: None,
        },
        legacy_id: Some("legacy-claim".into()),
        legacy_content_digest: Some([3; 32]),
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

fn outcome_event(
    version: u64,
    request_id: &str,
    result_ref: [u8; 32],
    principal: &str,
) -> RequestOutcomeEvent {
    RequestOutcomeEvent {
        principal_id: principal.into(),
        request_id: request_id.into(),
        payload_digest: [version as u8; 32],
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

/// Bootstrap a KB holding two live terms and two accepted claims on them.
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
    let mut blobs = vec![
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

    let version = 2;
    let basis = store.commit_basis(IdSpace::ProposalId, &mut l).unwrap();
    let result_bytes = typed::result_payload(version, &affected, &[], &mut l).unwrap();
    let result_ref = raw_sha256(&result_bytes);
    events.push(prepared(typed::prepare_event(
        EventInput::Outcome(&outcome_event(version, "seed-2", result_ref, PROPOSER)),
        &mut l,
    )));
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
    let prepared_tx = PreparedTransaction {
        version,
        prev_version: Some(version - 1),
        tx_id: [0x11 + version as u8; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000 + version as i64,
        principal_id: PROPOSER.into(),
        request_id: "seed-2".into(),
        payload_digest: [version as u8; 32],
        expect_version: Some(version - 1),
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared_tx, &Limits::V1, &mut l) {
        HandleOutcome::Committed { version: at, .. } => assert_eq!(at, version),
        other => panic!("seed commit failed: {other:?}"),
    }
    (inner, fs, store)
}

/// Build a context over the seeded world.
fn context<'a>(
    store: &'a mut Store<ObservingFs>,
    verifier: &'a CredentialVerifier<'static>,
    token: &'a [u8],
    request_id: &str,
) -> CommandContext<'a, ObservingFs> {
    CommandContext {
        store,
        verifier,
        bearer: token,
        execution: execution(request_id),
        limits: Limits::V1,
        validation: ValidationLimits::DEFAULT,
        max_existing_body_len: 1 << 20,
    }
}

/// A read-only grant on this KB, for reading committed state back through the real readers.
fn reader() -> sage_db_kernel::auth::Grants<'static> {
    sage_db_kernel::auth::Grants::new("p4-reader", kb(), CapabilitySet::of(&[Capability::Read]))
}

/// Read one frontier entry back through the **kernel's** reader, including its text bytes.
fn frontier_at(
    store: &mut Store<ObservingFs>,
    key: RecordKey,
) -> (
    sage_db_types::persist::FrontierEntryRecord,
    Vec<u8>,
    sage_db_store::view::ViewBasis,
) {
    let mut l = ledger();
    match sage_db_kernel::frontier::read_frontier(store, &reader(), key, 1 << 20, &mut l) {
        Ok(sage_db_kernel::frontier::FrontierRead::Found { record, text }) => {
            let basis = text.basis();
            let sage_db_store::fold::FoldedRecord::FrontierEntry(entry) = record.record() else {
                panic!("read_frontier returned a non-frontier record");
            };
            (entry.clone(), text.as_bytes().to_vec(), basis)
        }
        Ok(sage_db_kernel::frontier::FrontierRead::Absent { basis }) => {
            panic!(
                "expected a frontier record at {key:?}, absent at version {}",
                basis.version.get()
            )
        }
        Err(error) => panic!("expected a frontier record at {key:?}: {error:?}"),
    }
}

/// Read one session checkpoint back through the **kernel's** reader, including its content.
fn checkpoint_at(
    store: &mut Store<ObservingFs>,
    key: RecordKey,
) -> (
    sage_db_types::persist::SessionCheckpointRecord,
    Vec<u8>,
    sage_db_store::view::ViewBasis,
) {
    let mut l = ledger();
    match sage_db_kernel::session::read_session_checkpoint(store, &reader(), key, 1 << 20, &mut l) {
        Ok(sage_db_kernel::session::SessionRead::Found { record, content }) => {
            let basis = content.basis();
            let sage_db_store::fold::FoldedRecord::SessionCheckpoint(checkpoint) = record.record()
            else {
                panic!("read_session_checkpoint returned a non-checkpoint record");
            };
            (checkpoint.clone(), content.as_bytes().to_vec(), basis)
        }
        Ok(sage_db_kernel::session::SessionRead::Absent { basis }) => {
            panic!(
                "expected a checkpoint at {key:?}, absent at version {}",
                basis.version.get()
            )
        }
        Err(error) => panic!("expected a checkpoint at {key:?}: {error:?}"),
    }
}

fn read_pair_for(key: RecordKey, revision: RecordRevision) -> Vec<WireRevPair> {
    vec![wire_pair(key, revision)]
}

/// Read one proposal through the store's real record view.
fn proposal_at(
    store: &Store<ObservingFs>,
    key: RecordKey,
) -> sage_db_types::persist::ProposalRecord {
    let mut l = ledger();
    match store.record(key, &mut l).unwrap() {
        sage_db_store::view::RecordLookup::Found {
            record: sage_db_store::fold::FoldedRecord::Proposal(proposal),
            ..
        } => proposal.clone(),
        other => panic!("expected a proposal at {key:?}: {other:?}"),
    }
}

fn claim_edit_envelope(text: &str, request_id: &str) -> (Envelope, SubmitPayload) {
    // A claim edit reads both the subject term and the claim it edits.
    let env = envelope(
        vec![
            wire_pair(term_key(1), RecordRevision::FIRST),
            wire_pair(claim_key(1), RecordRevision::FIRST),
        ],
        request_id,
    );
    let payload = SubmitPayload {
        kind: Some(submit_payload::Kind::ClaimEdit(
            sage_db_protocol::generated::ClaimEdit {
                subject: Some(wire_key(term_key(1))),
                claim: Some(wire_key(claim_key(1))),
                text: text.to_owned(),
                declared_author: "declared-attribution".to_owned(),
            },
        )),
    };
    (env, payload)
}

// ---------------------------------------------------------------------------
// The eight shapes reach the canonical kernel path
// ---------------------------------------------------------------------------

#[test]
fn submit_claim_edit_through_the_authenticated_adapter_commits_and_attributes_the_caller() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    let (env, payload) = claim_edit_envelope("adapter claim text", "p4-submit-claim");
    let mut l = ledger();
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-submit-claim");

    let result = submit(&mut ctx, &env, &payload, &mut l).unwrap();
    assert_eq!(
        result.command,
        sage_db_server::commands::WriteCommand::SubmitClaimEdit
    );
    let HandleOutcome::Committed { version, recorded } = result.outcome else {
        panic!("expected a commit, got a different outcome");
    };
    assert_eq!(version, 3);

    let _ = recorded;
    // The audit principal is the credential's; the declared author stays attribution.
    let found = proposal_at(&store, proposal_key(1));
    assert_eq!(found.common.recorded_by.as_deref(), Some(PROPOSER));
    assert_eq!(found.declared_author, "declared-attribution");
    assert_eq!(found.kind, ProposalKind::ClaimEdit);
}

#[test]
fn submit_concept_text_preserves_the_absent_versus_empty_distinction() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    // An atom-only edit: `body` stays absent, which is a different statement from `Some("")`.
    let env = envelope(
        read_pair_for(term_key(1), RecordRevision::FIRST),
        "p4-atom-only",
    );
    let payload = SubmitPayload {
        kind: Some(submit_payload::Kind::ConceptText(ConceptText {
            subject: Some(wire_key(term_key(1))),
            atom: Some("atom replaced".to_owned()),
            body: None,
            declared_author: "author".to_owned(),
            reason: None,
        })),
    };
    let mut l = ledger();
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-atom-only");
    let result = submit(&mut ctx, &env, &payload, &mut l).unwrap();
    assert!(matches!(
        result.outcome,
        HandleOutcome::Committed { version: 3, .. }
    ));

    let found = proposal_at(&store, proposal_key(1));
    // The proposal is the concept-markdown family, written against the named term, with the
    // author's single baseline. The field mask itself travels inside the payload blob.
    assert_eq!(found.kind, ProposalKind::ConceptMarkdown);
    assert_eq!(found.subject, term_key(1));
    assert_eq!(found.base_revisions.len(), 1);
    assert_eq!(found.common.recorded_by.as_deref(), Some(PROPOSER));
}

#[test]
fn unauthenticated_request_never_reaches_a_store_lookup() {
    let (_inner, fs, mut store) = seed_world();
    let verifier = verifier();
    let (env, payload) = claim_edit_envelope("must not happen", "p4-unauth");
    let mut l = ledger();
    let before = fs.counts().borrow().total();
    let head = store.head();
    let mut ctx = context(&mut store, &verifier, b"not-a-token", "p4-unauth");

    let refused = submit(&mut ctx, &env, &payload, &mut l).unwrap_err();
    assert!(matches!(refused, CommandRefusal::Unauthenticated(_)));
    // Nothing was read from the store and nothing was written.
    assert_eq!(fs.counts().borrow().total(), before);
    assert_eq!(store.head(), head);
}

#[test]
fn a_token_bound_to_another_kb_fails_opaque() {
    let (_inner, _fs, mut store) = seed_world();
    // A verifier whose only binding belongs to a DIFFERENT KB.
    let other_kb = KbUuid::from_bytes([0xAB; 16]);
    let bindings = vec![CredentialBinding::new(
        token_hash(PROPOSER_TOKEN),
        PROPOSER,
        other_kb,
        CapabilitySet::of(&[Capability::Propose]),
        BindingStatus::Active,
    )];
    let bindings: &'static [CredentialBinding<'static>] = Box::leak(bindings.into_boxed_slice());
    let mut startup = ledger();
    let foreign =
        CredentialVerifier::new(bindings, VerifierLimits::default(), &mut startup).unwrap();

    let (env, payload) = claim_edit_envelope("wrong kb", "p4-wrong-kb");
    let mut l = ledger();
    let mut ctx = context(&mut store, &foreign, PROPOSER_TOKEN, "p4-wrong-kb");

    let refused = submit(&mut ctx, &env, &payload, &mut l).unwrap_err();
    // One opaque class: no principal, no KB, no capability detail.
    assert!(matches!(refused, CommandRefusal::Unauthenticated(_)));
}

#[test]
fn a_missing_capability_is_the_kernels_authorization_refusal_not_an_auth_bypass() {
    let (_inner, _fs, mut store) = seed_world();
    // A proposer token has no Review capability; Admin is not inherited by anyone.
    let verifier = verifier();
    let env = envelope(
        read_pair_for(proposal_key(1), RecordRevision::FIRST),
        "p4-no-review-cap",
    );
    let review = ReviewPayload {
        family: Some(review_payload::Family::ClaimEdit(ReviewDecisionPayload {
            proposal: Some(wire_key(proposal_key(1))),
            decision: Decision::Approve as i32,
            note: "nope".to_owned(),
        })),
    };
    let mut l = ledger();
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-no-review-cap");

    let refused = sage_db_server::commands::review(&mut ctx, &env, &review, &mut l).unwrap_err();
    // Authentication SUCCEEDED; the kernel then refused the missing capability.
    let CommandRefusal::Claims(error) = refused else {
        panic!("expected a kernel authorization refusal, got {refused:?}");
    };
    assert!(matches!(
        error,
        sage_db_kernel::claims::CommandError::Auth(_)
    ));
}

#[test]
fn expect_version_is_refused_before_any_mutation() {
    let (_inner, fs, mut store) = seed_world();
    let verifier = verifier();
    let mut env = envelope(
        read_pair_for(term_key(1), RecordRevision::FIRST),
        "p4-expect",
    );
    // Even the CURRENT version is refused: these commands cannot honour a whole-KB pin.
    env.expect_version = Some("2".to_owned());
    let payload = SubmitPayload {
        kind: Some(submit_payload::Kind::ClaimEdit(
            sage_db_protocol::generated::ClaimEdit {
                subject: Some(wire_key(term_key(1))),
                claim: Some(wire_key(claim_key(1))),
                text: "should not commit".to_owned(),
                declared_author: "author".to_owned(),
            },
        )),
    };
    let mut l = ledger();
    let before = fs.counts().borrow().total();
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-expect");

    let refused = submit(&mut ctx, &env, &payload, &mut l).unwrap_err();
    match refused {
        // A scalar: the refusal path carries the validated version, not a copied string.
        CommandRefusal::UnsupportedExpectedVersion { expected } => assert_eq!(expected, 2u64),
        other => panic!("expected an explicit unsupported-version refusal, got {other:?}"),
    }
    assert_eq!(fs.counts().borrow().total(), before);
    assert_eq!(store.head(), Some(2));
}

#[test]
fn a_malformed_read_set_is_refused_as_a_caller_error() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    // Revision 0 is not a revision: "no revision" is absence, never zero.
    let env = envelope(
        vec![WireRevPair {
            key: Some(wire_key(term_key(1))),
            revision: "0".to_owned(),
        }],
        "p4-zero-rev",
    );
    let payload = SubmitPayload {
        kind: Some(submit_payload::Kind::ClaimEdit(
            sage_db_protocol::generated::ClaimEdit {
                subject: Some(wire_key(term_key(1))),
                claim: Some(wire_key(claim_key(1))),
                text: "x".to_owned(),
                declared_author: "a".to_owned(),
            },
        )),
    };
    let mut l = ledger();
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-zero-rev");

    let refused = submit(&mut ctx, &env, &payload, &mut l).unwrap_err();
    // Transport admission now rejects revision zero before adapter conversion.
    // Preserve the exact malformed-field verdict at the boundary that actually sees it.
    assert!(matches!(
        refused,
        CommandRefusal::Invalid(ValidationError::Malformed {
            field: "read_set.revision",
            ..
        })
    ));
}

#[test]
fn a_foreign_key_is_refused_rather_than_converted() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    // Kind 99 is not in the closed record registry.
    let env = envelope(Vec::new(), "p4-foreign-key");
    let payload = SubmitPayload {
        kind: Some(submit_payload::Kind::ClaimEdit(
            sage_db_protocol::generated::ClaimEdit {
                subject: Some(WireKey {
                    kind: 99,
                    id: "1".to_owned(),
                }),
                claim: Some(wire_key(claim_key(1))),
                text: "x".to_owned(),
                declared_author: "a".to_owned(),
            },
        )),
    };
    let mut l = ledger();
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-foreign-key");

    let refused = submit(&mut ctx, &env, &payload, &mut l).unwrap_err();
    // The protocol gate refuses the kind before any conversion: an unknown numeric kind is a
    // syntax fault, not something to coerce into a registry entry or a default.
    match refused {
        CommandRefusal::Invalid(ValidationError::Malformed { field, .. }) => {
            assert!(field.contains("subject"), "unexpected field: {field}")
        }
        other => panic!("expected a protocol malformed-field refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Review: wire family first, then the kernel's own verdict
// ---------------------------------------------------------------------------

/// Submit a claim edit as PROPOSER, so a review has something pending to decide.
fn propose_claim(store: &mut Store<ObservingFs>, verifier: &CredentialVerifier<'static>, id: &str) {
    let (env, payload) = claim_edit_envelope("candidate text for review", id);
    let mut l = ledger();
    let mut ctx = context(store, verifier, PROPOSER_TOKEN, id);
    let outcome = submit(&mut ctx, &env, &payload, &mut l).expect("the proposal is accepted");
    assert!(matches!(
        outcome.outcome,
        HandleOutcome::Committed { version: 3, .. }
    ));
}

#[test]
fn review_claim_edit_approves_through_the_adapter_and_attributes_the_reviewer() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    propose_claim(&mut store, &verifier, "p4-propose-for-review");

    let env = envelope(
        read_pair_for(proposal_key(1), RecordRevision::FIRST),
        "p4-review-claim",
    );
    let payload = ReviewPayload {
        family: Some(review_payload::Family::ClaimEdit(ReviewDecisionPayload {
            proposal: Some(wire_key(proposal_key(1))),
            decision: Decision::Approve as i32,
            note: "approved through the adapter".to_owned(),
        })),
    };
    let mut l = ledger();
    let mut ctx = context(&mut store, &verifier, REVIEWER_TOKEN, "p4-review-claim");

    let result = review(&mut ctx, &env, &payload, &mut l).unwrap();
    assert_eq!(result.command, WriteCommand::ReviewClaimEdit);
    assert!(matches!(
        result.outcome,
        HandleOutcome::Committed { version: 4, .. }
    ));

    let decided = proposal_at(&store, proposal_key(1));
    assert_eq!(
        decided.status,
        sage_db_types::persist::ProposalStatus::Approved
    );
    // The audit principal is the REVIEWER's credential; the author stays attribution.
    assert_eq!(decided.common.recorded_by.as_deref(), Some(REVIEWER));
    assert_eq!(decided.declared_author, "declared-attribution");
}

#[test]
fn an_extra_review_read_pair_is_refused_rather_than_partly_ignored() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    propose_claim(&mut store, &verifier, "p4-propose-extra");

    // Two pairs: the review kernel builds its own single pair from the revision it is given,
    // so a second pair would silently go unchecked. Refusing is the honest answer.
    let env = envelope(
        vec![
            wire_pair(proposal_key(1), RecordRevision::FIRST),
            wire_pair(term_key(1), RecordRevision::FIRST),
        ],
        "p4-review-extra",
    );
    let payload = ReviewPayload {
        family: Some(review_payload::Family::ClaimEdit(ReviewDecisionPayload {
            proposal: Some(wire_key(proposal_key(1))),
            decision: Decision::Approve as i32,
            note: "n".to_owned(),
        })),
    };
    let mut l = ledger();
    let head = store.head();
    let mut ctx = context(&mut store, &verifier, REVIEWER_TOKEN, "p4-review-extra");

    let refused = review(&mut ctx, &env, &payload, &mut l).unwrap_err();
    assert!(
        matches!(&refused, CommandRefusal::MalformedField { field } if field.contains("read_set")),
        "expected a read-set refusal, got {refused:?}"
    );
    assert_eq!(store.head(), head);
}

#[test]
fn a_wrong_review_family_is_the_kernels_typed_refusal_not_a_new_guess() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    propose_claim(&mut store, &verifier, "p4-propose-wrong-family");

    // The stored proposal is a CLAIM edit, but the wire says concept text. The adapter must
    // dispatch on the wire family and let the kernel refuse, rather than reading the stored
    // kind to pick a family.
    let env = envelope(
        read_pair_for(proposal_key(1), RecordRevision::FIRST),
        "p4-review-family",
    );
    let payload = ReviewPayload {
        family: Some(review_payload::Family::ConceptText(ReviewDecisionPayload {
            proposal: Some(wire_key(proposal_key(1))),
            decision: Decision::Approve as i32,
            note: "wrong family".to_owned(),
        })),
    };
    let mut l = ledger();
    let mut ctx = context(&mut store, &verifier, REVIEWER_TOKEN, "p4-review-family");

    let refused = review(&mut ctx, &env, &payload, &mut l).unwrap_err();
    let CommandRefusal::Concept(error) = refused else {
        panic!("expected the concept family's typed refusal, got {refused:?}");
    };
    // A real typed kernel verdict, not an invented one.
    assert!(!format!("{error:?}").is_empty());
    assert_eq!(
        proposal_at(&store, proposal_key(1)).status,
        sage_db_types::persist::ProposalStatus::Pending
    );
}

#[test]
fn an_exact_retry_after_a_review_returns_the_stored_outcome() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    propose_claim(&mut store, &verifier, "p4-propose-retry");

    let env = envelope(
        read_pair_for(proposal_key(1), RecordRevision::FIRST),
        "p4-review-retry",
    );
    let payload = ReviewPayload {
        family: Some(review_payload::Family::ClaimEdit(ReviewDecisionPayload {
            proposal: Some(wire_key(proposal_key(1))),
            decision: Decision::Approve as i32,
            note: "once".to_owned(),
        })),
    };
    let mut first = ledger();
    let mut ctx = context(&mut store, &verifier, REVIEWER_TOKEN, "p4-review-retry");
    let initial = review(&mut ctx, &env, &payload, &mut first).unwrap();
    assert!(matches!(
        initial.outcome,
        HandleOutcome::Committed { version: 4, .. }
    ));

    // The same request again: the kernel's own idempotency, not the adapter's.
    let mut second = ledger();
    let mut ctx = context(&mut store, &verifier, REVIEWER_TOKEN, "p4-review-retry");
    let retried = review(&mut ctx, &env, &payload, &mut second).unwrap();
    assert!(
        matches!(
            retried.outcome,
            HandleOutcome::AlreadyCommitted { version: 4, .. }
        ),
        "an exact retry returns the stored outcome"
    );
    assert_eq!(store.head(), Some(4));
}

// ---------------------------------------------------------------------------
// Commit family: session checkpoints and frontier entries
// ---------------------------------------------------------------------------

#[test]
fn create_frontier_commits_and_records_the_refs_it_declared() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    let env = envelope(
        read_pair_for(term_key(1), RecordRevision::FIRST),
        "p4-create-frontier",
    );
    let payload = sage_db_protocol::generated::CommitPayload {
        kind: Some(commit_payload::Kind::CreateFrontier(WireFrontierState {
            concept_refs: vec![wire_key(term_key(1))],
            text: "an open question".to_owned(),
            priority: 0,
            status: 0,
        })),
    };
    let mut l = ledger();
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-create-frontier");

    let result = commit(&mut ctx, &env, &payload, &mut l).unwrap();
    assert_eq!(result.command, WriteCommand::CreateFrontier);
    assert!(matches!(
        result.outcome,
        HandleOutcome::Committed { version: 3, .. }
    ));
    assert_eq!(store.head(), Some(3));

    // The counter alone cannot distinguish a correct record from one with the wrong refs,
    // text, priority or status, so read it back through the kernel's own reader.
    let key = RecordKey::new(RecordKind::FrontierEntry, 1);
    let (entry, text, basis) = frontier_at(&mut store, key);
    assert_eq!(entry.concept_refs, vec![term_key(1)]);
    assert_eq!(text, b"an open question");
    assert_eq!(entry.priority, 0);
    assert_eq!(entry.status, sage_db_types::persist::FrontierStatus::Open);
    assert_eq!(entry.revision, RecordRevision::FIRST);
    assert_eq!(entry.common.recorded_by.as_deref(), Some(PROPOSER));
    assert_eq!(
        entry.common.actor_origin,
        sage_db_types::persist::ActorOrigin::Authenticated
    );
    // The record's text_ref names the bytes actually committed; both reads share one basis.
    assert_eq!(entry.text_ref, raw_sha256(&text));
    assert_eq!(basis.version.get(), 3);
}

#[test]
fn a_frontier_ref_that_is_not_a_term_is_refused_before_any_write() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    // A Claim where the refs must be Terms.
    let env = envelope(Vec::new(), "p4-frontier-non-term");
    let payload = sage_db_protocol::generated::CommitPayload {
        kind: Some(commit_payload::Kind::CreateFrontier(WireFrontierState {
            concept_refs: vec![wire_key(claim_key(1))],
            text: "bad ref".to_owned(),
            priority: 0,
            status: 0,
        })),
    };
    let mut l = ledger();
    let mut ctx = context(
        &mut store,
        &verifier,
        PROPOSER_TOKEN,
        "p4-frontier-non-term",
    );

    let refused = commit(&mut ctx, &env, &payload, &mut l).unwrap_err();
    match refused {
        CommandRefusal::Invalid(ValidationError::Malformed { field, .. }) => {
            assert!(field.contains("concept_refs"), "unexpected field: {field}")
        }
        other => panic!("expected a protocol refs refusal, got {other:?}"),
    }
    assert_eq!(store.head(), Some(2));
}

#[test]
fn create_session_checkpoint_commits_through_the_adapter() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    let env = envelope(
        read_pair_for(term_key(1), RecordRevision::FIRST),
        "p4-create-session",
    );
    let payload = sage_db_protocol::generated::CommitPayload {
        kind: Some(commit_payload::Kind::CreateSessionCheckpoint(
            SessionCheckpointState {
                concept: Some(wire_key(term_key(1))),
                session_no: 1,
                content: "session state".to_owned(),
            },
        )),
    };
    let mut l = ledger();
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-create-session");

    let result = commit(&mut ctx, &env, &payload, &mut l).unwrap();
    assert_eq!(result.command, WriteCommand::CreateSessionCheckpoint);
    assert!(matches!(
        result.outcome,
        HandleOutcome::Committed { version: 3, .. }
    ));

    let key = RecordKey::new(RecordKind::SessionCheckpoint, 1);
    let (checkpoint, content, basis) = checkpoint_at(&mut store, key);
    assert_eq!(
        checkpoint.location,
        sage_db_types::persist::CheckpointLocation::Term {
            concept: term_key(1),
            session_no: 1,
        }
    );
    assert_eq!(content, b"session state");
    assert_eq!(checkpoint.revision, RecordRevision::FIRST);
    assert_eq!(checkpoint.common.recorded_by.as_deref(), Some(PROPOSER));
    assert_eq!(checkpoint.content_ref, raw_sha256(&content));
    assert_eq!(basis.version.get(), 3);
}

#[test]
fn revise_frontier_forwards_the_complete_new_ref_list() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    // Create first, so revision has a real target and a real revision.
    let create_env = envelope(
        read_pair_for(term_key(1), RecordRevision::FIRST),
        "p4-frontier-create-for-revise",
    );
    let create_payload = sage_db_protocol::generated::CommitPayload {
        kind: Some(commit_payload::Kind::CreateFrontier(WireFrontierState {
            concept_refs: vec![wire_key(term_key(1))],
            text: "first".to_owned(),
            priority: 0,
            status: 0,
        })),
    };
    let mut l = ledger();
    let mut ctx = context(
        &mut store,
        &verifier,
        PROPOSER_TOKEN,
        "p4-frontier-create-for-revise",
    );
    commit(&mut ctx, &create_env, &create_payload, &mut l).unwrap();

    let entry = RecordKey::new(RecordKind::FrontierEntry, 1);
    // The complete NEW state: two refs, then the entry itself.
    let revise_env = envelope(
        vec![
            wire_pair(term_key(1), RecordRevision::FIRST),
            wire_pair(term_key(2), RecordRevision::FIRST),
            wire_pair(entry, RecordRevision::FIRST),
        ],
        "p4-frontier-revise",
    );
    let revise_payload = sage_db_protocol::generated::CommitPayload {
        kind: Some(commit_payload::Kind::ReviseFrontier(
            sage_db_protocol::generated::ReviseFrontier {
                frontier: Some(wire_key(entry)),
                state: Some(WireFrontierState {
                    concept_refs: vec![wire_key(term_key(1)), wire_key(term_key(2))],
                    text: "now with two refs".to_owned(),
                    priority: 1,
                    status: 0,
                }),
            },
        )),
    };
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-frontier-revise");
    let result = commit(&mut ctx, &revise_env, &revise_payload, &mut l).unwrap();
    assert_eq!(result.command, WriteCommand::ReviseFrontier);
    assert!(matches!(
        result.outcome,
        HandleOutcome::Committed { version: 4, .. }
    ));

    let (entry, text, basis) = frontier_at(&mut store, entry);
    // The COMPLETE new ref list, in the caller's order — not the create's single ref, and not
    // a ref carried over from the old record.
    assert_eq!(entry.concept_refs, vec![term_key(1), term_key(2)]);
    assert_eq!(text, b"now with two refs");
    assert_eq!(entry.priority, 1);
    assert_eq!(entry.status, sage_db_types::persist::FrontierStatus::Open);
    // A real second revision of the SAME record, not a fresh one.
    assert_eq!(entry.revision, RecordRevision::new(2).unwrap());
    assert_eq!(entry.key, RecordKey::new(RecordKind::FrontierEntry, 1));
    assert_eq!(entry.common.recorded_by.as_deref(), Some(PROPOSER));
    assert_eq!(entry.text_ref, raw_sha256(&text));
    assert_eq!(basis.version.get(), 4);
}

#[test]
fn revise_session_checkpoint_commits_against_the_named_checkpoint() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    let create_env = envelope(
        read_pair_for(term_key(1), RecordRevision::FIRST),
        "p4-session-create-for-revise",
    );
    let create_payload = sage_db_protocol::generated::CommitPayload {
        kind: Some(commit_payload::Kind::CreateSessionCheckpoint(
            SessionCheckpointState {
                concept: Some(wire_key(term_key(1))),
                session_no: 1,
                content: "one".to_owned(),
            },
        )),
    };
    let mut l = ledger();
    let mut ctx = context(
        &mut store,
        &verifier,
        PROPOSER_TOKEN,
        "p4-session-create-for-revise",
    );
    commit(&mut ctx, &create_env, &create_payload, &mut l).unwrap();

    let checkpoint = RecordKey::new(RecordKind::SessionCheckpoint, 1);
    // Ascending canonical order: the Term precedes the checkpoint.
    let revise_env = envelope(
        vec![
            wire_pair(term_key(1), RecordRevision::FIRST),
            wire_pair(checkpoint, RecordRevision::FIRST),
        ],
        "p4-session-revise",
    );
    let revise_payload = sage_db_protocol::generated::CommitPayload {
        kind: Some(commit_payload::Kind::ReviseSessionCheckpoint(
            sage_db_protocol::generated::ReviseSessionCheckpoint {
                checkpoint: Some(wire_key(checkpoint)),
                state: Some(SessionCheckpointState {
                    concept: Some(wire_key(term_key(1))),
                    session_no: 1,
                    content: "two".to_owned(),
                }),
            },
        )),
    };
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-session-revise");
    let result = commit(&mut ctx, &revise_env, &revise_payload, &mut l).unwrap();
    assert_eq!(result.command, WriteCommand::ReviseSessionCheckpoint);
    assert!(matches!(
        result.outcome,
        HandleOutcome::Committed { version: 4, .. }
    ));

    let (checkpoint, content, basis) = checkpoint_at(&mut store, checkpoint);
    assert_eq!(
        checkpoint.location,
        sage_db_types::persist::CheckpointLocation::Term {
            concept: term_key(1),
            session_no: 1,
        }
    );
    // The new content, not the created one.
    assert_eq!(content, b"two");
    assert_eq!(checkpoint.revision, RecordRevision::new(2).unwrap());
    assert_eq!(
        checkpoint.key,
        RecordKey::new(RecordKind::SessionCheckpoint, 1)
    );
    assert_eq!(checkpoint.common.recorded_by.as_deref(), Some(PROPOSER));
    assert_eq!(checkpoint.content_ref, raw_sha256(&content));
    assert_eq!(basis.version.get(), 4);
}

#[test]
fn a_bytes_or_entries_stop_reaches_the_caller_as_a_real_refusal() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    let (env, payload) = claim_edit_envelope("budget", "p4-budget");
    let head = store.head();
    {
        let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-budget");

        // A ledger with no entry allowance at all: the first charge must refuse, and the
        // refusal must be the canonical budget family rather than a generic failure.
        let mut starved = AttemptLedger::open(BYTES, 0, &RunToCompletion).unwrap();
        match submit(&mut ctx, &env, &payload, &mut starved) {
            Err(CommandRefusal::Refused(sage_db_types::framing::Refusal::Budget(error))) => {
                // A real canonical budget refusal naming the unit, not a flattened failure.
                assert_eq!(error.unit(), sage_db_types::ledger::BudgetUnit::Entries);
                assert!(
                    error.remaining() < error.requested(),
                    "the charge did not fit"
                );
            }
            other => panic!("expected a canonical budget refusal, got {other:?}"),
        }
    }
    // The borrow has ended; the head is unchanged because the refusal came first.
    assert_eq!(store.head(), head);
}

#[test]
fn review_concept_text_applies_the_atom_and_preserves_submission_reason() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    // Propose an atom replacement, so review has a real candidate to apply.
    let submit_env = envelope(
        read_pair_for(term_key(1), RecordRevision::FIRST),
        "p4-propose-concept",
    );
    let submit_payload = SubmitPayload {
        kind: Some(submit_payload::Kind::ConceptText(ConceptText {
            subject: Some(wire_key(term_key(1))),
            atom: Some("atom after review".to_owned()),
            body: None,
            declared_author: "concept-author".to_owned(),
            reason: Some("来源依据已更新".to_owned()),
        })),
    };
    let mut l = ledger();
    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-propose-concept");
    submit(&mut ctx, &submit_env, &submit_payload, &mut l).unwrap();

    let pending = proposal_at(&store, proposal_key(1));
    assert_eq!(pending.common.reason.as_deref(), Some("来源依据已更新"));

    let review_env = envelope(
        read_pair_for(proposal_key(1), RecordRevision::FIRST),
        "p4-review-concept",
    );
    let review_payload = ReviewPayload {
        family: Some(review_payload::Family::ConceptText(ReviewDecisionPayload {
            proposal: Some(wire_key(proposal_key(1))),
            decision: Decision::Approve as i32,
            note: "atom is better".to_owned(),
        })),
    };
    let mut ctx = context(&mut store, &verifier, REVIEWER_TOKEN, "p4-review-concept");
    let result = review(&mut ctx, &review_env, &review_payload, &mut l).unwrap();
    assert_eq!(result.command, WriteCommand::ReviewConceptText);
    assert!(matches!(
        result.outcome,
        HandleOutcome::Committed { version: 4, .. }
    ));

    let applied = proposal_at(&store, proposal_key(1));
    assert_eq!(
        applied.status,
        sage_db_types::persist::ProposalStatus::Approved
    );
    assert_eq!(applied.common.recorded_by.as_deref(), Some(REVIEWER));
    assert_eq!(applied.review_note.as_deref(), Some("atom is better"));
    let blob = store
        .read_committed_blob(applied.payload_ref, typed::MAX_FRAME_BYTES, &mut l)
        .expect("persisted request");
    let original = sage_db_kernel::concept::decode_stored_intent(&applied, blob.as_bytes(), &mut l)
        .expect("decided proposal remains decodable");
    assert_eq!(original.reason, Some("来源依据已更新"));
    assert_eq!(original.atom, Some("atom after review"));
    drop(blob);

    let mut ctx = context(&mut store, &verifier, PROPOSER_TOKEN, "p4-propose-concept");
    let retry = submit(&mut ctx, &submit_env, &submit_payload, &mut l).unwrap();
    assert!(matches!(
        retry.outcome,
        HandleOutcome::AlreadyCommitted { version: 3, .. }
    ));
    let mut changed = submit_payload;
    let Some(submit_payload::Kind::ConceptText(text)) = &mut changed.kind else {
        panic!("concept text fixture");
    };
    text.reason = Some("a changed reason".to_owned());
    let error = submit(&mut ctx, &submit_env, &changed, &mut l).unwrap_err();
    assert!(matches!(
        error,
        CommandRefusal::Concept(sage_db_kernel::concept::ConceptTextError::Command(
            sage_db_kernel::claims::CommandError::IdempotencyMismatch
        ))
    ));
    assert_eq!(store.head(), Some(4));
}

#[test]
fn review_concept_text_rejects_without_touching_the_subject() {
    let (_inner, _fs, mut store) = seed_world();
    let verifier = verifier();
    let submit_env = envelope(
        read_pair_for(term_key(1), RecordRevision::FIRST),
        "p4-propose-concept-reject",
    );
    let submit_payload = SubmitPayload {
        kind: Some(submit_payload::Kind::ConceptText(ConceptText {
            subject: Some(wire_key(term_key(1))),
            atom: Some("worse atom".to_owned()),
            body: None,
            declared_author: "concept-author".to_owned(),
            reason: None,
        })),
    };
    let mut l = ledger();
    let mut ctx = context(
        &mut store,
        &verifier,
        PROPOSER_TOKEN,
        "p4-propose-concept-reject",
    );
    submit(&mut ctx, &submit_env, &submit_payload, &mut l).unwrap();

    let review_env = envelope(
        read_pair_for(proposal_key(1), RecordRevision::FIRST),
        "p4-reject-concept",
    );
    let review_payload = ReviewPayload {
        family: Some(review_payload::Family::ConceptText(ReviewDecisionPayload {
            proposal: Some(wire_key(proposal_key(1))),
            decision: Decision::Reject as i32,
            note: "no".to_owned(),
        })),
    };
    let mut ctx = context(&mut store, &verifier, REVIEWER_TOKEN, "p4-reject-concept");
    let result = review(&mut ctx, &review_env, &review_payload, &mut l).unwrap();
    assert!(matches!(
        result.outcome,
        HandleOutcome::Committed { version: 4, .. }
    ));
    let decided = proposal_at(&store, proposal_key(1));
    assert_eq!(
        decided.status,
        sage_db_types::persist::ProposalStatus::Rejected
    );
}
