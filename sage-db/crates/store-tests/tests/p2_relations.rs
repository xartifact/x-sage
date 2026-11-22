//! Relation-assert proposals on the real public Store and the model backend.
//!
//! Every command goes through the production entry points — `bootstrap`, `Store::commit`,
//! `submit_relation_assert`, `review_relation_assert` — with the real ledger, the real canonical
//! codecs and the canonical `infer::validate_candidate`. Seed rows are written through the
//! canonical `typed` bridge, so a seeded term is byte-identical to what a writer would persist.
//!
//! # What this file does not claim
//!
//! Platform durability, and no phase/P4/cutover readiness. `HandleOutcome::Unknown` is exercised
//! as the ambiguous attempt it is, with a real fault injection and a whole-world restore that
//! pansics rather than skips on a failed reopen. Test-local allocations are not allocator
//! evidence.

#![allow(dead_code)]

#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::path::Path;
use std::rc::Rc;

use fixture::{BYTES, ENTRIES, ROOT, RULES, kb};
use observing_fs::ObservingFs;

use sage_db_format::digest::raw_sha256;
use sage_db_kernel::auth::{Capability, CapabilitySet, Grants};
use sage_db_kernel::claims::{CommandError, RequestMeta};
use sage_db_kernel::relations::{
    DECISION_APPROVE, DECISION_REJECT, RelationAssert, RelationAssertError,
    RelationAssertInputError, RelationAssertPayloadError, decode_review, decode_submit,
    encode_review, encode_submit, review_relation_assert, submit_relation_assert,
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
use sage_db_types::budget::Budget;
use sage_db_types::confidence::Confidence;
use sage_db_types::framing::Limits;
use sage_db_types::identity::RecordRevision;
use sage_db_types::persist::{
    ActorOrigin, CommonFields, Extensions, IdAllocationEvent, IdSpace, ProposalKind,
    ProposalStatus, RecordKey, RecordKind, RevPair, TermRecord, TermSource, Watermark,
};
use sage_db_types::query::{ControlStop, QueryControl, RunToCompletion};
use sage_db_types::relation::RelationId;

const PRINCIPAL: &str = "relation-submitter";
const REVIEWER: &str = "relation-reviewer";
const OTHER: &str = "other-submitter";

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).expect("a finite ledger opens")
}

fn term_key(id: u32) -> RecordKey {
    RecordKey::term(id)
}

fn proposal_key(id: u128) -> RecordKey {
    RecordKey::new(RecordKind::Proposal, id)
}

fn assertion_key(id: u128) -> RecordKey {
    RecordKey::new(RecordKind::Assertion, id)
}

fn submitter() -> Grants<'static> {
    Grants::new(PRINCIPAL, kb(), CapabilitySet::of(&[Capability::Propose]))
}

fn reviewer() -> Grants<'static> {
    Grants::new(REVIEWER, kb(), CapabilitySet::of(&[Capability::Review]))
}

fn other_submitter() -> Grants<'static> {
    Grants::new(OTHER, kb(), CapabilitySet::of(&[Capability::Propose]))
}

/// Propose-only grants: no Review is implied, which the refusal test depends on.
fn no_capabilities() -> Grants<'static> {
    Grants::new(PRINCIPAL, kb(), CapabilitySet::NONE)
}

fn meta(request_id: &str) -> RequestMeta<'_> {
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

// ---------------------------------------------------------------------------
// Seed world
// ---------------------------------------------------------------------------

fn term_body(id: u32) -> Vec<u8> {
    format!("relation-term-body/{id}").into_bytes()
}

fn layer_for(id: u32) -> sage_db_types::layer::SemanticLayer {
    // `formal-v1` requires a definition target to sit STRICTLY below its source, so a legal
    // `is-a` chain here is 2 -> 1 -> 0.
    match id {
        1 => sage_db_types::layer::SemanticLayer::L2,
        2 => sage_db_types::layer::SemanticLayer::L1,
        _ => sage_db_types::layer::SemanticLayer::L0,
    }
}

fn term_record(id: u32, retired: bool) -> TermRecord {
    TermRecord {
        key: term_key(id),
        revision: RecordRevision::FIRST,
        retired,
        slug: format!("rel-term-{id}"),
        atom: format!("rel term {id}"),
        layer: layer_for(id),
        domains: vec!["default".to_owned()],
        source: TermSource::Domain,
        confidence: Confidence::Confirmed,
        triggers: vec!["rel".to_owned()],
        body_ref: raw_sha256(&term_body(id)),
        common: CommonFields {
            recorded_by: Some("seed-actor".to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("seed".to_owned()),
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

/// Seed terms 1..=3 (all active) in one commit at version 2.
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

    let terms: Vec<TermRecord> = (1..=3).map(|id| term_record(id, false)).collect();
    let mut events = Vec::new();
    for term in &terms {
        events.push(prepared(typed::prepare_event(
            EventInput::Term(typed::TermEvent::Created, term),
            &mut l,
        )));
    }
    commit_terms(&mut store, &mut l, 2, events, &terms);
    (inner, fs, store)
}

fn prepared(
    outcome: sage_db_types::framing::FormatOutcome<sage_db_types::prepared::OwnedEvent>,
) -> sage_db_types::prepared::OwnedEvent {
    match outcome {
        sage_db_types::framing::FormatOutcome::Ok(event) => event,
        other => panic!("canonical encode refused the seed: {other:?}"),
    }
}

/// Commit the seed terms with an outcome event, so the store holds a complete transaction.
fn commit_terms(
    store: &mut Store<ObservingFs>,
    l: &mut AttemptLedger<'_>,
    version: u64,
    mut events: Vec<sage_db_types::prepared::OwnedEvent>,
    terms: &[TermRecord],
) {
    let basis = store.commit_basis(IdSpace::ProposalId, l).expect("basis");
    let affected: Vec<RevPair> = terms
        .iter()
        .map(|term| RevPair {
            key: term.key,
            revision: RecordRevision::FIRST,
        })
        .collect();
    let result_bytes = typed::result_payload(version, &affected, &[], l).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    events.push(prepared(typed::prepare_event(
        EventInput::Outcome(&sage_db_types::persist::RequestOutcomeEvent {
            principal_id: "seed-actor".to_owned(),
            request_id: format!("seed-{version}"),
            payload_digest: [version as u8; 32],
            result_ref,
            commit_version: version,
            audit: sage_db_types::persist::CreationAudit {
                recorded_by: Some("seed-actor".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1_700_000_000_000_000 + version as i64,
            },
            extensions: Extensions::new(),
        }),
        l,
    )));

    let mut blobs = vec![sage_db_types::prepared::BlobDeclaration {
        sha256: basis.rules.rules_ref,
        declared_len: basis.rules_len,
        source: sage_db_types::prepared::BlobSource::Existing,
    }];
    for term in terms {
        blobs.push(sage_db_types::prepared::BlobDeclaration {
            sha256: term.body_ref,
            declared_len: term_body(term.key.id() as u32).len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(term_body(term.key.id() as u32)),
        });
    }
    blobs.push(sage_db_types::prepared::BlobDeclaration {
        sha256: result_ref,
        declared_len: result_bytes.len() as u64,
        source: sage_db_types::prepared::BlobSource::Provide(result_bytes),
    });

    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: [0x5c; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_100,
        principal_id: "seed-actor".to_owned(),
        request_id: format!("seed-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, l) {
        HandleOutcome::Committed { version: at, .. } => assert_eq!(at, version),
        other => panic!("seed commit failed: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Request helpers
// ---------------------------------------------------------------------------

fn request<'a>(
    from: u32,
    target: u32,
    relation: RelationId,
    author: &'a str,
) -> RelationAssert<'a> {
    RelationAssert {
        from: term_key(from),
        target: term_key(target),
        relation: relation.get() as u16,
        confidence: 2,
        declaration_order: 7,
        declared_author: author,
        reason: "adds an authored edge",
    }
}

fn endpoint_set(from: u32, target: u32, store: &Store<ObservingFs>) -> Vec<RevPair> {
    let (first, second) = if from < target {
        (from, target)
    } else {
        (target, from)
    };
    [(first, 0usize), (second, 1)]
        .iter()
        .map(|(id, _)| {
            let revision = current_revision(store, term_key(*id));
            RevPair {
                key: term_key(*id),
                revision,
            }
        })
        .collect()
}

fn current_revision(store: &Store<ObservingFs>, key: RecordKey) -> RecordRevision {
    let mut l = ledger();
    match store.record(key, &mut l).expect("record read") {
        sage_db_store::view::RecordLookup::Found { record, .. } => revision_of(record),
        other => panic!("expected a current record for {key:?}: {other:?}"),
    }
}

fn revision_of(record: &FoldedRecord) -> RecordRevision {
    match record {
        FoldedRecord::Term(term) => term.revision,
        FoldedRecord::Assertion(assertion) => assertion.revision,
        FoldedRecord::Proposal(proposal) => proposal.revision,
        _ => panic!("unexpected family"),
    }
}

// ---------------------------------------------------------------------------
// Payload bytes: literal widths and ordering
// ---------------------------------------------------------------------------

#[test]
fn the_submit_payload_has_the_registered_literal_layout() {
    let mut l = ledger();
    let bytes = encode_submit(&request(1, 2, RelationId::IsA, "author"), &mut l).expect("encoded");

    // from key: kind 1 (Term) LE u16, then id 1 LE u128.
    assert_eq!(&bytes[0..2], &1u16.to_le_bytes());
    assert_eq!(&bytes[2..18], &1u128.to_le_bytes());
    // target key: kind 1, id 2.
    assert_eq!(&bytes[18..20], &1u16.to_le_bytes());
    assert_eq!(&bytes[20..36], &2u128.to_le_bytes());
    // relation u16, confidence u16, declaration_order u32.
    assert_eq!(&bytes[36..38], &0u16.to_le_bytes(), "is-a is 0");
    assert_eq!(&bytes[38..40], &2u16.to_le_bytes(), "confirmed is 2");
    assert_eq!(&bytes[40..44], &7u32.to_le_bytes());
    // declared_author: u32 length then bytes.
    assert_eq!(&bytes[44..48], &6u32.to_le_bytes());
    assert_eq!(&bytes[48..54], b"author");
    // reason: u32 length then bytes.
    let reason = b"adds an authored edge";
    assert_eq!(&bytes[54..58], &(reason.len() as u32).to_le_bytes());
    assert_eq!(&bytes[58..], reason);
    // Total length is exactly the sum, with no padding.
    assert_eq!(bytes.len(), 58 + reason.len());

    // Round-trip is NOT the evidence; the literals above are. Confirm the decode agrees.
    let decoded = decode_submit(&bytes, &mut l).expect("decodes");
    assert_eq!(decoded.from, term_key(1));
    assert_eq!(decoded.target, term_key(2));
    assert_eq!(decoded.relation, RelationId::IsA);
    assert_eq!(decoded.declaration_order, 7);
}

#[test]
fn the_review_payload_has_the_registered_literal_layout() {
    let mut l = ledger();
    let bytes = encode_review(proposal_key(4), DECISION_APPROVE, "ok", &mut l).expect("encoded");
    assert_eq!(
        &bytes[0..2],
        &7u16.to_le_bytes(),
        "proposal record kind is 7"
    );
    assert_eq!(&bytes[2..18], &4u128.to_le_bytes());
    assert_eq!(&bytes[18..20], &0u16.to_le_bytes(), "approve is 0");
    assert_eq!(&bytes[20..24], &2u32.to_le_bytes());
    assert_eq!(&bytes[24..26], b"ok");
    assert_eq!(bytes.len(), 26);

    let decoded = decode_review(&bytes, &mut l).expect("decodes");
    assert_eq!(decoded.proposal, proposal_key(4));
    assert_eq!(decoded.decision, DECISION_APPROVE);
    assert_eq!(decoded.note, "ok");
}

#[test]
fn the_payload_codec_refuses_unregistered_domains_and_self_relations() {
    let mut l = ledger();
    // An unregistered relation value is refused, never cast or defaulted.
    let mut bad = request(1, 2, RelationId::IsA, "author");
    bad.relation = 99;
    assert!(matches!(
        encode_submit(&bad, &mut l),
        Err(RelationAssertPayloadError::UnknownRelation(99))
    ));
    // An unregistered confidence value likewise.
    let mut bad = request(1, 2, RelationId::IsA, "author");
    bad.confidence = 9;
    assert!(matches!(
        encode_submit(&bad, &mut l),
        Err(RelationAssertPayloadError::UnknownConfidence(9))
    ));
    // A self relation is refused at the codec, before any state is read.
    let mut bad = request(1, 1, RelationId::IsA, "author");
    bad.from = term_key(1);
    bad.target = term_key(1);
    assert!(matches!(
        encode_submit(&bad, &mut l),
        Err(RelationAssertPayloadError::SelfRelation)
    ));
    // NUL-bearing attribution is refused. Empty attribution is legal and tested below.
    let bad = request(1, 2, RelationId::IsA, "author\0");
    assert!(matches!(
        encode_submit(&bad, &mut l),
        Err(RelationAssertPayloadError::InvalidAuthor)
    ));
}

#[test]
fn attribution_bytes_use_the_canonical_eight_hundred_byte_limit() {
    assert_eq!(
        typed::MAX_DECLARED_AUTHOR,
        800,
        "attribution is not an actor identity"
    );
    let author = "a".repeat(800);
    let mut l = ledger();
    let bytes = encode_submit(&request(1, 2, RelationId::IsA, &author), &mut l)
        .expect("canonical maximum accepted");
    assert_eq!(
        &bytes[44..48],
        &800u32.to_le_bytes(),
        "independent literal length field"
    );
    assert_eq!(&bytes[48..848], author.as_bytes());
    assert_eq!(
        decode_submit(&bytes, &mut l).unwrap().declared_author,
        author
    );

    let too_long = "a".repeat(801);
    assert_eq!(
        encode_submit(&request(1, 2, RelationId::IsA, &too_long), &mut l),
        Err(RelationAssertPayloadError::InvalidAuthor)
    );
    // Mutate the independently pinned length/value; do not ask the rejected encoder
    // to manufacture its own negative decoder input.
    let mut over = bytes.clone();
    over[44..48].copy_from_slice(&801u32.to_le_bytes());
    over.insert(848, b'a');
    assert!(matches!(
        decode_submit(&over, &mut l),
        Err(RelationAssertPayloadError::InvalidAuthor)
    ));
    let empty = encode_submit(&request(1, 2, RelationId::IsA, ""), &mut l).unwrap();
    assert_eq!(&empty[44..48], &0u32.to_le_bytes());
    assert_eq!(decode_submit(&empty, &mut l).unwrap().declared_author, "");
}

fn attribution_survives_approval(author: &str) {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "author-submit",
        request(1, 2, RelationId::IsA, author),
        &set,
        &mut l,
    )
    .expect("literal attribution submitted");
    review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "review attribution",
        "author-review",
        &mut l,
    )
    .expect("literal attribution reviewed");
    match store.record(proposal_key(1), &mut l).expect("proposal") {
        sage_db_store::view::RecordLookup::Found {
            record: FoldedRecord::Proposal(row),
            ..
        } => {
            assert_eq!(row.declared_author, author);
            assert_eq!(row.status, ProposalStatus::Approved);
            assert_eq!(row.common.recorded_by.as_deref(), Some(REVIEWER));
        }
        other => panic!("expected approved proposal: {other:?}"),
    }
}

#[test]
fn empty_attribution_survives_the_real_submit_and_approval() {
    attribution_survives_approval("");
}

#[test]
fn maximum_attribution_survives_the_real_submit_and_approval() {
    attribution_survives_approval(&"a".repeat(800));
}

// ---------------------------------------------------------------------------
// Lifecycle: submit, approve, reject, conflict
// ---------------------------------------------------------------------------

fn submit(
    store: &mut Store<ObservingFs>,
    grants: &Grants<'_>,
    request_id: &str,
    req: RelationAssert<'_>,
    read_set: &[RevPair],
    l: &mut dyn sage_db_store::lease::LeaseLedger,
) -> Result<HandleOutcome, RelationAssertError> {
    submit_relation_assert(
        store,
        grants,
        meta(request_id),
        req,
        read_set,
        &Limits::V1,
        l,
    )
}

fn review(
    store: &mut Store<ObservingFs>,
    proposal: RecordKey,
    revision: RecordRevision,
    decision: u16,
    note: &str,
    request_id: &str,
    l: &mut dyn sage_db_store::lease::LeaseLedger,
) -> Result<HandleOutcome, RelationAssertError> {
    review_relation_assert(
        store,
        &reviewer(),
        meta(request_id),
        proposal,
        revision,
        decision,
        note,
        Budget::DEFAULT,
        &RunToCompletion,
        &Limits::V1,
        l,
    )
}

#[test]
fn a_pending_submission_writes_no_assertion_until_approval() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);
    let outcome = submit(
        &mut store,
        &submitter(),
        "sub-1",
        request(1, 2, RelationId::IsA, "declared-author"),
        &set,
        &mut l,
    )
    .expect("submit commits");
    assert!(matches!(outcome, HandleOutcome::Committed { .. }));

    // The proposal exists, Pending, with the relation kind and the endpoint baselines.
    let key = proposal_key(1);
    match store.record(key, &mut l).expect("proposal read") {
        sage_db_store::view::RecordLookup::Found { record, .. } => {
            let FoldedRecord::Proposal(proposal) = record else {
                panic!("proposal 1 must fold to a Proposal");
            };
            assert_eq!(proposal.status, ProposalStatus::Pending);
            assert_eq!(proposal.kind, ProposalKind::RelationAssert);
            assert_eq!(proposal.subject, term_key(1));
            assert_eq!(proposal.declared_author, "declared-author");
            assert_eq!(proposal.base_revisions, set.as_slice());
            assert!(proposal.review_note.is_none());
        }
        other => panic!("the proposal must be current: {other:?}"),
    }
    // And NO assertion exists yet.
    assert!(matches!(
        store.record(assertion_key(1), &mut l).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));

    // Approve it.
    let approved = review(
        &mut store,
        key,
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ship it",
        "rev-1",
        &mut l,
    )
    .expect("approval commits");
    assert!(matches!(approved, HandleOutcome::Committed { .. }));

    // The assertion now exists as revision 1 with the payload's edge.
    match store
        .record(assertion_key(1), &mut l)
        .expect("assertion read")
    {
        sage_db_store::view::RecordLookup::Found { record, .. } => {
            let FoldedRecord::Assertion(assertion) = record else {
                panic!("assertion 1 must fold to an Assertion");
            };
            assert_eq!(assertion.from, term_key(1));
            assert_eq!(assertion.target, term_key(2));
            assert_eq!(assertion.relation, RelationId::IsA);
            assert_eq!(assertion.confidence, Confidence::Confirmed);
            assert_eq!(assertion.declaration_order, 7);
            assert_eq!(assertion.revision, RecordRevision::FIRST);
            assert!(!assertion.retracted);
            // The audit names the REVIEWER (the publisher), never the declared author.
            assert_eq!(
                assertion.common.recorded_by.as_deref(),
                Some(REVIEWER),
                "the executing actor is the reviewer, not the attribution"
            );
        }
        other => panic!("the assertion must be current: {other:?}"),
    }
    // The proposal is Approved at revision 2 and still names the declared author.
    match store.record(key, &mut l).expect("proposal read") {
        sage_db_store::view::RecordLookup::Found { record, .. } => {
            let FoldedRecord::Proposal(proposal) = record else {
                panic!("proposal folds");
            };
            assert_eq!(proposal.status, ProposalStatus::Approved);
            assert_eq!(proposal.revision.get(), 2);
            assert_eq!(proposal.declared_author, "declared-author");
            assert_eq!(proposal.review_note.as_deref(), Some("ship it"));
        }
        other => panic!("proposal must be current: {other:?}"),
    }
}

#[test]
fn an_exact_retry_returns_the_original_outcome_without_another_mutation() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);
    let first = submit(
        &mut store,
        &submitter(),
        "sub-retry",
        request(1, 2, RelationId::IsA, "author"),
        &set,
        &mut l,
    )
    .expect("submit");
    let head_after_first = store.head();
    let version_first = match first {
        HandleOutcome::Committed { version, .. } => version,
        other => panic!("expected a commit: {other:?}"),
    };

    // The identical request returns the ORIGINAL version and writes nothing.
    let retry = submit(
        &mut store,
        &submitter(),
        "sub-retry",
        request(1, 2, RelationId::IsA, "author"),
        &set,
        &mut l,
    )
    .expect("retry is answered");
    match retry {
        HandleOutcome::AlreadyCommitted { version, .. } => assert_eq!(version, version_first),
        other => panic!("an exact retry must be AlreadyCommitted, got {other:?}"),
    }
    assert_eq!(store.head(), head_after_first, "a retry must not commit");

    // A DIFFERENT payload under the same request id is an idempotency mismatch.
    let other_set = endpoint_set(1, 3, &store);
    let mismatch = submit(
        &mut store,
        &submitter(),
        "sub-retry",
        request(1, 3, RelationId::IsA, "author"),
        &other_set,
        &mut l,
    );
    assert!(
        mismatch.is_err(),
        "a different digest for the same request id must be refused"
    );
}

#[test]
fn a_reject_commits_no_assertion() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-reject",
        request(1, 2, RelationId::IsA, "author"),
        &set,
        &mut l,
    )
    .expect("submit");

    let head_before = store.head();
    let rejected = review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_REJECT,
        "not now",
        "rev-reject",
        &mut l,
    )
    .expect("rejection commits");
    assert!(matches!(rejected, HandleOutcome::Committed { .. }));
    assert!(store.head().expect("head") > head_before.expect("head"));

    // No assertion, and the proposal is Rejected.
    assert!(matches!(
        store.record(assertion_key(1), &mut l).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
    match store
        .record(proposal_key(1), &mut l)
        .expect("proposal read")
    {
        sage_db_store::view::RecordLookup::Found { record, .. } => {
            let FoldedRecord::Proposal(proposal) = record else {
                panic!("proposal folds");
            };
            assert_eq!(proposal.status, ProposalStatus::Rejected);
        }
        other => panic!("proposal must be current: {other:?}"),
    }
}

#[test]
fn a_moved_endpoint_commits_conflicted_with_no_assertion() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-conflict",
        request(1, 2, RelationId::IsA, "author"),
        &set,
        &mut l,
    )
    .expect("submit");

    // Move endpoint 1 by committing a real revision 2 of it. This is an unrelated write from
    // the proposal's point of view, but it moves the very baseline the proposal pinned.
    let term = term_record(1, false);
    let mut bumped = term.clone();
    bumped.revision = RecordRevision::new(2).expect("revision 2");
    bumped.atom = "rel term 1 v2".to_owned();
    bumped.changed_fields = Some(vec![15u16]);
    bumped.common.updated_at = Some(1_700_000_000_000_200);
    let mut events = vec![prepared(typed::prepare_event(
        EventInput::Term(typed::TermEvent::Revised, &bumped),
        &mut l,
    ))];
    let next = store.head().expect("head") + 1;
    commit_term_revision(
        &mut store,
        &mut l,
        next,
        std::mem::take(&mut events),
        &bumped,
    );

    let decision = review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "still fine?",
        "rev-conflict",
        &mut l,
    )
    .expect("a conflict is a committed decision, not an error");
    assert!(matches!(decision, HandleOutcome::Committed { .. }));

    // The proposal is Conflicted and NO assertion was created.
    match store
        .record(proposal_key(1), &mut l)
        .expect("proposal read")
    {
        sage_db_store::view::RecordLookup::Found { record, .. } => {
            let FoldedRecord::Proposal(proposal) = record else {
                panic!("proposal folds");
            };
            assert_eq!(proposal.status, ProposalStatus::Conflicted);
        }
        other => panic!("proposal must be current: {other:?}"),
    }
    assert!(
        matches!(
            store.record(assertion_key(1), &mut l).expect("read"),
            sage_db_store::view::RecordLookup::Absent { .. }
        ),
        "a conflicted decision must not create an assertion"
    );
}

/// Commit one revised term with an outcome event.
fn commit_term_revision(
    store: &mut Store<ObservingFs>,
    l: &mut AttemptLedger<'_>,
    version: u64,
    mut events: Vec<sage_db_types::prepared::OwnedEvent>,
    term: &TermRecord,
) {
    let basis = store.commit_basis(IdSpace::ProposalId, l).expect("basis");
    let affected = [RevPair {
        key: term.key,
        revision: term.revision,
    }];
    let result_bytes = typed::result_payload(version, &affected, &[], l).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    events.push(prepared(typed::prepare_event(
        EventInput::Outcome(&sage_db_types::persist::RequestOutcomeEvent {
            principal_id: "seed-actor".to_owned(),
            request_id: format!("bump-{version}"),
            payload_digest: [version as u8; 32],
            result_ref,
            commit_version: version,
            audit: sage_db_types::persist::CreationAudit {
                recorded_by: Some("seed-actor".to_owned()),
                actor_origin: ActorOrigin::Authenticated,
                reason: None,
                created_at: 1_700_000_000_000_000 + version as i64,
            },
            extensions: Extensions::new(),
        }),
        l,
    )));
    let blobs = vec![
        sage_db_types::prepared::BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: sage_db_types::prepared::BlobSource::Existing,
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: term.body_ref,
            declared_len: term_body(term.key.id() as u32).len() as u64,
            source: sage_db_types::prepared::BlobSource::Existing,
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(result_bytes),
        },
    ];
    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: [0x5d; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_200,
        principal_id: "seed-actor".to_owned(),
        request_id: format!("bump-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, l) {
        HandleOutcome::Committed { .. } => {}
        other => panic!("term bump failed: {other:?}"),
    }
}

#[test]
fn a_duplicate_triple_becomes_an_independent_support() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    // First approval creates assertion 1 for 1 -> 2.
    let set = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-a",
        request(1, 2, RelationId::IsA, "author-a"),
        &set,
        &mut l,
    )
    .expect("submit a");
    review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-a",
        &mut l,
    )
    .expect("approve a");

    // A SECOND proposal for the SAME triple is approved independently: a new identity, not a
    // merge and not a rejection.
    submit(
        &mut store,
        &submitter(),
        "sub-b",
        request(1, 2, RelationId::IsA, "author-b"),
        &set,
        &mut l,
    )
    .expect("submit b");
    review(
        &mut store,
        proposal_key(2),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-b",
        &mut l,
    )
    .expect("approve b");

    let first = read_assertion(&store, 1);
    let second = read_assertion(&store, 2);
    assert_eq!(first.from, second.from);
    assert_eq!(first.target, second.target);
    assert_eq!(first.relation, second.relation);
    assert_ne!(
        first.key, second.key,
        "a duplicate triple must be a distinct support identity"
    );
    // Both were published by the same reviewer, so the audit actor legitimately matches; what
    // must differ is the identity, which is the support's own name.
    assert_eq!(first.common.recorded_by.as_deref(), Some(REVIEWER));
    assert_eq!(second.common.recorded_by.as_deref(), Some(REVIEWER));
    assert_ne!(first.key, second.key);
}

fn read_assertion(
    store: &Store<ObservingFs>,
    id: u128,
) -> sage_db_types::persist::AssertionRecordFull {
    let mut l = ledger();
    match store
        .record(assertion_key(id), &mut l)
        .expect("assertion read")
    {
        sage_db_store::view::RecordLookup::Found { record, .. } => match record {
            FoldedRecord::Assertion(assertion) => assertion.clone(),
            _ => panic!("assertion {id} folds to another family"),
        },
        other => panic!("assertion {id} must be current: {other:?}"),
    }
}

#[test]
fn review_requires_its_own_capability_and_propose_does_not_imply_it() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);

    // A propose-only grant cannot submit with no capabilities at all.
    let denied = submit(
        &mut store,
        &no_capabilities(),
        "sub-denied",
        request(1, 2, RelationId::IsA, "author"),
        &set,
        &mut l,
    );
    assert!(
        denied.is_err(),
        "no capability must refuse before any write"
    );

    submit(
        &mut store,
        &submitter(),
        "sub-ok",
        request(1, 2, RelationId::IsA, "author"),
        &set,
        &mut l,
    )
    .expect("submit");

    // The PROPOSE grant cannot review: capabilities never imply one another.
    let wrong = review_relation_assert(
        &mut store,
        &submitter(),
        meta("rev-wrong-cap"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        Budget::DEFAULT,
        &RunToCompletion,
        &Limits::V1,
        &mut l,
    );
    assert!(wrong.is_err(), "Propose must not authorize Review");

    // And a REVIEW grant cannot submit.
    let set_23 = endpoint_set(2, 3, &store);
    let wrong_submit = submit_relation_assert(
        &mut store,
        &reviewer(),
        meta("sub-wrong-cap"),
        request(2, 3, RelationId::IsA, "author"),
        &set_23,
        &Limits::V1,
        &mut l,
    );
    assert!(wrong_submit.is_err(), "Review must not authorize Propose");
}

#[test]
fn a_candidate_the_rules_reject_writes_nothing_and_leaves_the_proposal_pending() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    // `instantiates` is a definition relation, so formal-v1 requires target strictly below
    // source. 1 is L2 and 3 is L0, which is legal; 3 -> 1 is the illegal direction.
    let set = endpoint_set(3, 1, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-invalid",
        request(3, 1, RelationId::Instantiates, "author"),
        &set,
        &mut l,
    )
    .expect("submit is admitted: it does not validate the graph");

    let head_before = store.head();
    let refused = review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "try",
        "rev-invalid",
        &mut l,
    );
    match refused {
        Err(RelationAssertError::Input(RelationAssertInputError::InvalidCandidate {
            violations,
        })) => {
            assert!(!violations.is_empty(), "a refusal names its violations");
            assert!(
                violations.iter().any(|v| v.code() == "V3"),
                "the layer-direction rule is the one violated here: {violations:?}"
            );
        }
        other => panic!("an invalid candidate must be a typed refusal, got {other:?}"),
    }
    // NO write happened: same head, no assertion, proposal still Pending.
    assert_eq!(
        store.head(),
        head_before,
        "a validation refusal must not commit"
    );
    assert!(matches!(
        store.record(assertion_key(1), &mut l).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
    match store
        .record(proposal_key(1), &mut l)
        .expect("proposal read")
    {
        sage_db_store::view::RecordLookup::Found { record, .. } => {
            let FoldedRecord::Proposal(proposal) = record else {
                panic!("proposal folds");
            };
            assert_eq!(proposal.status, ProposalStatus::Pending, "it stays Pending");
        }
        other => panic!("proposal must be current: {other:?}"),
    }
}

#[test]
fn a_second_proposal_that_would_close_a_cycle_is_refused_at_review() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    // Proposal 1: 1 is-a 2, which is legal on its own and gets approved.
    let a = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-a",
        request(1, 2, RelationId::IsA, "a"),
        &a,
        &mut l,
    )
    .expect("submit a");
    review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-a",
        &mut l,
    )
    .expect("approve a");

    // Proposal 2: 2 is-a 1 is ALSO individually plausible, but together they are a cycle.
    let b = endpoint_set(2, 1, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-b",
        request(2, 1, RelationId::IsA, "b"),
        &b,
        &mut l,
    )
    .expect("submit b");

    // The refusal must come from the FRESH candidate (current graph + proposed), not from the
    // graph as it stood when the proposal was submitted.
    match review(
        &mut store,
        proposal_key(2),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "try",
        "rev-b",
        &mut l,
    ) {
        Err(RelationAssertError::Input(RelationAssertInputError::InvalidCandidate { .. })) => {}
        other => panic!("a cycle-forming approval must be refused, got {other:?}"),
    }
    // Only the first assertion exists.
    assert!(matches!(
        store.record(assertion_key(2), &mut l).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
}

#[test]
fn a_stale_endpoint_baseline_is_refused_at_submit() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let mut set = endpoint_set(1, 2, &store);
    // Claim a revision that is not current.
    set[0].revision = RecordRevision::new(9).expect("revision 9");
    let refused = submit(
        &mut store,
        &submitter(),
        "sub-stale",
        request(1, 2, RelationId::IsA, "author"),
        &set,
        &mut l,
    );
    assert!(
        refused.is_err(),
        "a stale baseline must be refused before writing"
    );
    assert!(matches!(
        store.record(proposal_key(1), &mut l).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
}

#[test]
fn a_missing_endpoint_and_a_self_relation_are_refused() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    // Term 9 does not exist; its baseline cannot be read.
    let mut bad = endpoint_set(1, 2, &store);
    bad[0].key = term_key(9);
    let refused = submit(
        &mut store,
        &submitter(),
        "sub-missing",
        request(9, 2, RelationId::IsA, "author"),
        &bad,
        &mut l,
    );
    assert!(refused.is_err(), "a missing endpoint must be refused");

    // The same term on both sides is refused by the codec, before any read.
    let self_set = endpoint_set(1, 1, &store);
    let refused = submit(
        &mut store,
        &submitter(),
        "sub-self",
        request(1, 1, RelationId::IsA, "author"),
        &self_set,
        &mut l,
    );
    assert!(refused.is_err(), "a self relation must be refused");
}

#[test]
fn a_read_set_that_is_not_exactly_the_endpoints_is_refused() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);

    // Too few rows: the omitted endpoint baseline cannot be inferred from the payload.
    let refused = submit(
        &mut store,
        &submitter(),
        "sub-short",
        request(1, 2, RelationId::IsA, "author"),
        &set[..1],
        &mut l,
    );
    assert!(
        refused.is_err(),
        "an omitted endpoint baseline must be refused"
    );

    // Wrong order: canonical ascending is required, not the caller's order.
    let mut reversed = set.clone();
    reversed.reverse();
    let refused = submit(
        &mut store,
        &submitter(),
        "sub-order",
        request(1, 2, RelationId::IsA, "author"),
        &reversed,
        &mut l,
    );
    assert!(refused.is_err(), "a non-canonical read set must be refused");
}

// ---------------------------------------------------------------------------
// Resource, gate and ambiguity
// ---------------------------------------------------------------------------

#[test]
fn a_real_byte_refusal_and_a_real_stop_are_distinct_before_any_write() {
    let (_inner, _fs, mut store) = seed_world();
    let mut good = ledger();
    let set = endpoint_set(1, 2, &store);

    // A ledger with a real, tiny byte ceiling: the first admission cannot fit, and the refusal
    // is the ledger's own, not a fabricated error.
    let mut tiny = AttemptLedger::open(64, ENTRIES, &RunToCompletion).expect("a tiny ledger opens");
    let refused = submit(
        &mut store,
        &submitter(),
        "sub-nobytes",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut tiny,
    );
    match refused {
        Err(RelationAssertError::Command(CommandError::Refused(
            sage_db_types::framing::Refusal::Budget(_),
        ))) => {}
        other => panic!("a byte refusal must surface as a budget Refusal, got {other:?}"),
    }

    // A real stopping control, on a real ledger: the refusal is a stop, a different class.
    struct StopNow;
    impl QueryControl for StopNow {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let mut stopping = AttemptLedger::open(BYTES, ENTRIES, &StopNow).expect("ledger");
    let stopped = submit(
        &mut store,
        &submitter(),
        "sub-stop",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut stopping,
    );
    // The stop is observed by the FIRST checkpoint, which is authorization's, so it surfaces as
    // `Auth(Refused(Stopped))`. That is the same class, carried through the shared
    // authorization seam rather than flattened into an input error.
    match stopped {
        Err(RelationAssertError::Command(CommandError::Auth(
            sage_db_kernel::auth::AuthError::Refused(sage_db_types::framing::Refusal::Stopped(_)),
        ))) => {}
        other => panic!("a stop must surface as Stopped, got {other:?}"),
    }

    // Nothing was written by either refusal.
    assert!(matches!(
        store.record(proposal_key(1), &mut good).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
}

#[test]
fn an_unfinished_import_fences_the_relation_commands() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);
    // A committed, incomplete import session closes the gate for ordinary commands.
    commit_unfinished_import(&mut store, &mut l);

    let refused = submit(
        &mut store,
        &submitter(),
        "sub-gated",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut l,
    );
    match refused {
        Err(RelationAssertError::Command(CommandError::ImportInProgress)) => {}
        other => panic!("an unfinished import must fence the command, got {other:?}"),
    }
}

/// Commit an `import_complete = false` session at the next version.
fn commit_unfinished_import(store: &mut Store<ObservingFs>, l: &mut AttemptLedger<'_>) {
    use sage_db_types::persist::{CreationAudit, ImportSessionRecord, ImportStage};

    let version = store.head().expect("head") + 1;
    let basis = store.commit_basis(IdSpace::TermId, l).expect("basis");
    let plan = b"relation-gate-plan".to_vec();
    let plan_ref = raw_sha256(&plan);
    let result_bytes = typed::result_payload(version, &[], &[], l).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    let audit = CreationAudit {
        recorded_by: Some("gate".to_owned()),
        actor_origin: ActorOrigin::Authenticated,
        reason: Some("gate fixture".to_owned()),
        created_at: 1_700_000_000_000_300,
    };
    let record = ImportSessionRecord {
        import_session_id: "relation-gate".to_owned(),
        stage: ImportStage::Prepare,
        source_digest: [7; 32],
        plan_ref,
        expected_counts: vec![],
        import_complete: false,
        audit: audit.clone(),
        extensions: Extensions::new(),
    };
    let outcome = sage_db_types::persist::RequestOutcomeEvent {
        principal_id: "gate".to_owned(),
        request_id: format!("gate-{version}"),
        payload_digest: [version as u8; 32],
        result_ref,
        commit_version: version,
        audit,
        extensions: Extensions::new(),
    };
    let events = vec![
        prepared(typed::prepare_event(EventInput::ImportSession(&record), l)),
        prepared(typed::prepare_event(EventInput::Outcome(&outcome), l)),
    ];
    let blobs = vec![
        sage_db_types::prepared::BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: sage_db_types::prepared::BlobSource::Existing,
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: plan_ref,
            declared_len: plan.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(plan),
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(result_bytes),
        },
    ];
    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: [0x5f; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_300,
        principal_id: "gate".to_owned(),
        request_id: format!("gate-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, l) {
        HandleOutcome::Committed { .. } => {}
        other => panic!("gate fixture commit failed: {other:?}"),
    }
}

#[test]
fn a_review_of_an_absent_or_already_decided_proposal_is_refused() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-x",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut l,
    )
    .expect("submit");
    review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-x",
        &mut l,
    )
    .expect("approve");

    // A second review of the now-decided proposal is not Pending.
    let again = review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "again",
        "rev-y",
        &mut l,
    );
    assert!(again.is_err(), "a decided proposal cannot be decided again");

    // An absent proposal is refused as missing, not as absent-state.
    let missing = review(
        &mut store,
        proposal_key(99),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "x",
        "rev-z",
        &mut l,
    );
    assert!(missing.is_err(), "an absent proposal must be refused");
}

#[test]
fn an_unknown_review_then_a_real_reopen_never_leaves_a_half_approval() {
    let (inner, fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-unknown",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut l,
    )
    .expect("submit");

    // Force the CURRENT replacement to report an unknown effect.
    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    let outcome = review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "under fault",
        "rev-unknown",
        &mut l,
    )
    .expect("Unknown is an outcome, never an error");
    assert!(matches!(outcome, HandleOutcome::Unknown));
    assert!(inner.faults_handle().borrow().all_fired());

    // Resolve by dropping and reopening the real bytes: the original handle holds the root
    // lock, so it must go before a second open can succeed.
    drop(store);
    let reopened =
        open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger()).expect("reopen");
    let fresh = match reopened {
        sage_db_store::open::Opened::Ready(store) => store,
        sage_db_store::open::Opened::ReadOnly { .. } => {
            panic!("the fixture must replay durably, not read-only")
        }
    };

    // The whole state after the ambiguous attempt is one of the two coherent worlds: either
    // (proposal Pending, no assertion) or (proposal Approved, assertion present). Never a
    // half approval.
    let mut read = ledger();
    let proposal_approved = match fresh.record(proposal_key(1), &mut read).expect("proposal") {
        sage_db_store::view::RecordLookup::Found { record, .. } => match record {
            FoldedRecord::Proposal(proposal) => proposal.status == ProposalStatus::Approved,
            _ => panic!("proposal folds"),
        },
        other => panic!("the proposal must exist: {other:?}"),
    };
    let assertion_present = matches!(
        fresh
            .record(assertion_key(1), &mut read)
            .expect("assertion"),
        sage_db_store::view::RecordLookup::Found { .. }
    );
    assert_eq!(
        proposal_approved, assertion_present,
        "the assertion and the approval must agree after restart (approved={proposal_approved}, assertion={assertion_present})"
    );
}

#[test]
fn a_stored_proposal_whose_payload_disagrees_with_its_subject_is_refused_at_review() {
    // A forged Pending proposal: its `subject` says term 1, but the stored payload's `from` says
    // term 2. Review must derive the edge FROM THE PAYLOAD and require the metadata to agree, so
    // this is refused rather than silently approving an edge the proposal does not name.
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    // Build a legal submit frame for 2 -> 3 ...
    let set = endpoint_set(2, 3, &store);
    let payload = encode_submit(&request(2, 3, RelationId::IsA, "forger"), &mut l).expect("codec");
    // Build the frame through the PUBLIC typed encoder, exactly as the executor does; only its
    // digest matters to the outcome event here.
    let ops = [typed::RequestOp::new(
        sage_db_kernel::relations::SUBMIT_RELATION_ASSERT,
        sage_db_kernel::relations::OPERATION_VERSION,
        &payload,
    )];
    let frame = typed::RequestFrame::new(&ops, &set, None);
    let frame_bytes = match typed::encode_request(&frame, &mut l) {
        typed::RequestOutcome::Ok(bytes) => bytes,
        other => panic!("frame encode: {other:?}"),
    };
    let digest = match typed::request_digest_of(&frame_bytes, &mut l) {
        typed::RequestOutcome::Ok(digest) => digest,
        other => panic!("frame digest: {other:?}"),
    };
    let payload_ref = raw_sha256(&frame_bytes);

    // ... and file it under a proposal whose SUBJECT is term 1 instead.
    let forged = sage_db_types::persist::ProposalRecord {
        key: proposal_key(1),
        revision: RecordRevision::FIRST,
        status: ProposalStatus::Pending,
        kind: ProposalKind::RelationAssert,
        subject: term_key(1),
        base_revisions: set.clone(),
        payload_ref,
        review_note: None,
        declared_author: "forger".to_owned(),
        common: CommonFields {
            recorded_by: Some("forger".to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("forged".to_owned()),
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        legacy_id: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    commit_forged_proposal(&mut store, &mut l, &forged, &frame_bytes, &set, digest);

    match review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "approve",
        "rev-forged",
        &mut l,
    ) {
        Err(RelationAssertError::Command(CommandError::Input(
            sage_db_kernel::claims::InputError::StoredMetadataMismatch(
                sage_db_kernel::claims::StoredField::Subject,
            ),
        ))) => {}
        other => panic!("a payload/subject disagreement must be refused, got {other:?}"),
    }
    // No assertion was created by the forged proposal.
    assert!(matches!(
        store.record(assertion_key(1), &mut l).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
}

/// Commit a hand-built Pending proposal plus its payload blob.
fn commit_forged_proposal(
    store: &mut Store<ObservingFs>,
    l: &mut AttemptLedger<'_>,
    proposal: &sage_db_types::persist::ProposalRecord,
    frame_bytes: &[u8],
    set: &[RevPair],
    digest: [u8; 32],
) {
    let version = store.head().expect("head") + 1;
    let basis = store.commit_basis(IdSpace::ProposalId, l).expect("basis");
    let payload_ref = raw_sha256(frame_bytes);
    let affected = [RevPair {
        key: proposal.key,
        revision: RecordRevision::FIRST,
    }];
    let result_bytes = typed::result_payload(version, &affected, &[], l).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    let _ = set;
    let events = vec![
        prepared(typed::prepare_event(
            EventInput::Proposal {
                decided: false,
                record: proposal,
            },
            l,
        )),
        prepared(typed::prepare_event(
            EventInput::Outcome(&sage_db_types::persist::RequestOutcomeEvent {
                principal_id: "forger".to_owned(),
                request_id: "forged".to_owned(),
                payload_digest: digest,
                result_ref,
                commit_version: version,
                audit: sage_db_types::persist::CreationAudit {
                    recorded_by: Some("forger".to_owned()),
                    actor_origin: ActorOrigin::Authenticated,
                    reason: None,
                    created_at: 1_700_000_000_000_000,
                },
                extensions: Extensions::new(),
            }),
            l,
        )),
    ];
    let blobs = vec![
        sage_db_types::prepared::BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: sage_db_types::prepared::BlobSource::Existing,
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: payload_ref,
            declared_len: frame_bytes.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(frame_bytes.to_vec()),
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(result_bytes),
        },
    ];
    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: [0x6a; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: "forger".to_owned(),
        request_id: "forged".to_owned(),
        payload_digest: digest,
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, l) {
        HandleOutcome::Committed { .. } => {}
        other => panic!("the forged proposal could not be committed: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// C7 evidence: Legacy profile, reachable V-codes, HWM exhaustion/collision
// ---------------------------------------------------------------------------

/// Canonical rules bytes for a profile, produced by its own encoder.
fn rules_for(profile: sage_db_types::relation::RuleProfile) -> Vec<u8> {
    let mut l = ledger();
    sage_db_format::rules::CanonicalRules::new(profile)
        .encode(&mut l)
        .expect("the canonical rules encoder accepts its own profile")
}

/// Seed terms 1..=3 under an explicit profile and explicit layers, at a private root.
fn seed_world_profile(
    root: &str,
    profile: sage_db_types::relation::RuleProfile,
    layers: [sage_db_types::layer::SemanticLayer; 3],
) -> (Rc<ModelFileSystem>, ObservingFs, Store<ObservingFs>) {
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = ObservingFs::new(Rc::clone(&inner));
    let mut l = ledger();
    let rules = rules_for(profile);
    let mut store = sage_db_store::init::bootstrap_with_profile(
        fs.clone(),
        Path::new(root),
        CreateDir::IfMissing,
        kb(),
        (profile, &rules),
        &Limits::V1,
        &mut l,
    )
    .expect("bootstrap with profile");

    let terms: Vec<TermRecord> = (1..=3)
        .map(|id| {
            let mut term = term_record(id, false);
            term.layer = layers[(id - 1) as usize];
            term
        })
        .collect();
    let mut events = Vec::new();
    for term in &terms {
        events.push(prepared(typed::prepare_event(
            EventInput::Term(typed::TermEvent::Created, term),
            &mut l,
        )));
    }
    commit_terms(&mut store, &mut l, 2, events, &terms);
    (inner, fs, store)
}

#[test]
fn an_exact_retry_after_a_real_reopen_returns_the_original_version() {
    let (_inner, fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);
    let first = submit(
        &mut store,
        &submitter(),
        "sub-reopen",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut l,
    )
    .expect("submit");
    let original = match first {
        HandleOutcome::Committed { version, .. } => version,
        other => panic!("expected a commit: {other:?}"),
    };
    let head = store.head();

    drop(store);
    let mut reopened =
        match open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger())
            .expect("reopen")
        {
            sage_db_store::open::Opened::Ready(store) => store,
            sage_db_store::open::Opened::ReadOnly { .. } => panic!("must replay durably"),
        };
    assert_eq!(reopened.head(), head, "a reopen must not change the head");

    let retry = submit(
        &mut reopened,
        &submitter(),
        "sub-reopen",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut ledger(),
    )
    .expect("the retry is answered from storage");
    match retry {
        HandleOutcome::AlreadyCommitted { version, .. } => assert_eq!(
            version, original,
            "an exact retry after reopen must report the ORIGINAL version"
        ),
        other => panic!("a retry must not replay after reopen, got {other:?}"),
    }
    assert_eq!(reopened.head(), head, "the retry must not commit");
}

#[test]
fn a_proposal_id_space_at_its_ceiling_refuses_rather_than_wrapping() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    commit_proposal_hwm(&mut store, &mut l, u128::MAX);

    let set = endpoint_set(1, 2, &store);
    let refused = submit(
        &mut store,
        &submitter(),
        "sub-exhaust",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut l,
    );
    match refused {
        Err(RelationAssertError::Input(RelationAssertInputError::ProposalIdExhausted)) => {}
        other => panic!("a saturated proposal id space must refuse, got {other:?}"),
    }
    assert!(matches!(
        store.record(proposal_key(1), &mut l).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
}

/// Advance the committed ProposalId watermark to `hwm` with a real allocation event.
fn commit_proposal_hwm(store: &mut Store<ObservingFs>, l: &mut AttemptLedger<'_>, hwm: u128) {
    let version = store.head().expect("head") + 1;
    let basis = store.commit_basis(IdSpace::ProposalId, l).expect("basis");
    let allocation = IdAllocationEvent {
        allocation: Watermark {
            space: IdSpace::ProposalId,
            hwm,
        },
        reason: None,
        extensions: Extensions::new(),
    };
    let result_bytes = typed::result_payload(version, &[], &[], l).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    let events = vec![
        prepared(typed::prepare_event(
            EventInput::IdAllocation(&allocation),
            l,
        )),
        prepared(typed::prepare_event(
            EventInput::Outcome(&sage_db_types::persist::RequestOutcomeEvent {
                principal_id: "seed-actor".to_owned(),
                request_id: format!("hwm-{version}"),
                payload_digest: [version as u8; 32],
                result_ref,
                commit_version: version,
                audit: sage_db_types::persist::CreationAudit {
                    recorded_by: Some("seed-actor".to_owned()),
                    actor_origin: ActorOrigin::Authenticated,
                    reason: None,
                    created_at: 1_700_000_000_000_000 + version as i64,
                },
                extensions: Extensions::new(),
            }),
            l,
        )),
    ];
    let blobs = vec![
        sage_db_types::prepared::BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: sage_db_types::prepared::BlobSource::Existing,
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(result_bytes),
        },
    ];
    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: [0x6c; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: "seed-actor".to_owned(),
        request_id: format!("hwm-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, l) {
        HandleOutcome::Committed { .. } => {}
        other => panic!("hwm commit failed: {other:?}"),
    }
}

#[test]
fn an_unknown_review_is_judged_in_every_sampled_crash_world() {
    use sage_db_store_tests::oracle::attempts::{
        AttemptHistory, AttemptOutcome, Commit, LogicalRequest, World,
    };

    let (inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let set = endpoint_set(1, 2, &store);
    let submitted = submit(
        &mut store,
        &submitter(),
        "sub-crash",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut l,
    )
    .expect("submit");
    let submit_version = match submitted {
        HandleOutcome::Committed { version, .. } => version,
        other => panic!("expected a commit: {other:?}"),
    };
    let submit_digest = canonical_digest_for(&set, request(1, 2, RelationId::IsA, "a"));

    *inner.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    let outcome = review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "under fault",
        "rev-crash",
        &mut l,
    )
    .expect("Unknown is an outcome");
    assert!(matches!(outcome, HandleOutcome::Unknown));
    assert!(inner.faults_handle().borrow().all_fired());

    let review_digest = canonical_review_digest(proposal_key(1), DECISION_APPROVE, "under fault");

    let worlds = inner.model().crash_states();
    assert!(!worlds.states.is_empty(), "the fault produced no worlds");
    assert_eq!(
        worlds.sampled,
        worlds.states.len(),
        "every world was sampled"
    );

    let mut history = AttemptHistory::default();
    history.record(
        "ack-submit",
        LogicalRequest::new(PRINCIPAL, "sub-crash", submit_digest),
        AttemptOutcome::Durable {
            version: submit_version,
        },
    );
    history.record(
        "unknown-review",
        LogicalRequest::new(REVIEWER, "rev-crash", review_digest),
        AttemptOutcome::Unknown,
    );

    let world_base = submit_version - 1;
    let mut old_seen = false;
    let mut new_seen = false;
    for (index, state) in worlds.states.into_iter().enumerate() {
        let restored =
            ModelFileSystem::from_durable_state(ModelCapabilities::full(), Path::new(ROOT), state)
                .unwrap_or_else(|error| panic!("world {index} restore failed: {error:?}"));
        let model_fs = ObservingFs::new(Rc::new(restored));
        let reopened = match open_existing(
            model_fs,
            Path::new(ROOT),
            Some(kb()),
            &Limits::V1,
            &mut ledger(),
        ) {
            Ok(sage_db_store::open::Opened::Ready(store)) => store,
            Ok(sage_db_store::open::Opened::ReadOnly { .. }) => {
                panic!("world {index} must replay durably")
            }
            Err(error) => panic!("world {index} open failed: {error:?}"),
        };
        let head = reopened.head().expect("a head");
        assert!(
            head >= submit_version,
            "world {index} lost the acknowledged submit"
        );

        let mut read = ledger();
        let mut commits = Vec::new();

        match reopened.outcome(PRINCIPAL, "sub-crash") {
            sage_db_store::open::OutcomeLookup::Found {
                outcome,
                original_version,
                ..
            } => {
                assert_eq!(
                    original_version, submit_version,
                    "world {index} moved the ACK"
                );
                assert_eq!(
                    outcome.event().payload_digest,
                    submit_digest,
                    "world {index} persisted a different submit digest"
                );
                commits.push(Commit {
                    version: original_version,
                    request: LogicalRequest::new(PRINCIPAL, "sub-crash", submit_digest),
                });
            }
            other => panic!("world {index} lost the acknowledged outcome: {other:?}"),
        }

        if head == submit_version {
            old_seen = true;
            assert!(
                matches!(
                    reopened.outcome(REVIEWER, "rev-crash"),
                    sage_db_store::open::OutcomeLookup::Absent { .. }
                ),
                "world {index} has candidate state without its outcome"
            );
            match reopened
                .record(proposal_key(1), &mut read)
                .expect("proposal")
            {
                sage_db_store::view::RecordLookup::Found { record, .. } => match record {
                    FoldedRecord::Proposal(proposal) => assert_eq!(
                        proposal.status,
                        ProposalStatus::Pending,
                        "world {index} advanced the proposal without an outcome"
                    ),
                    _ => panic!("world {index}: proposal folds"),
                },
                other => panic!("world {index}: the proposal must exist: {other:?}"),
            }
            assert!(
                matches!(
                    reopened
                        .record(assertion_key(1), &mut read)
                        .expect("assertion"),
                    sage_db_store::view::RecordLookup::Absent { .. }
                ),
                "world {index} published an assertion with no outcome"
            );
        } else {
            new_seen = true;
            match reopened.outcome(REVIEWER, "rev-crash") {
                sage_db_store::open::OutcomeLookup::Found {
                    outcome,
                    original_version,
                    ..
                } => {
                    assert_eq!(
                        original_version, head,
                        "world {index} misplaced the outcome"
                    );
                    assert_eq!(
                        outcome.event().payload_digest,
                        review_digest,
                        "world {index} persisted a different review digest"
                    );
                    commits.push(Commit {
                        version: original_version,
                        request: LogicalRequest::new(REVIEWER, "rev-crash", review_digest),
                    });
                }
                other => panic!("world {index} has candidate state without its outcome: {other:?}"),
            }
            match reopened
                .record(proposal_key(1), &mut read)
                .expect("proposal")
            {
                sage_db_store::view::RecordLookup::Found { record, .. } => match record {
                    FoldedRecord::Proposal(proposal) => assert_eq!(
                        proposal.status,
                        ProposalStatus::Approved,
                        "world {index} published the outcome without the approval"
                    ),
                    _ => panic!("world {index}: proposal folds"),
                },
                other => panic!("world {index}: the proposal must exist: {other:?}"),
            }
            assert!(
                matches!(
                    reopened
                        .record(assertion_key(1), &mut read)
                        .expect("assertion"),
                    sage_db_store::view::RecordLookup::Found { .. }
                ),
                "world {index} approved without publishing the assertion"
            );
        }

        assert!(
            history
                .accepts(
                    world_base,
                    &World {
                        base_version: world_base,
                        commits,
                    }
                )
                .expect("model"),
            "the sequence oracle rejected world {index}"
        );
    }
    assert!(
        old_seen && new_seen,
        "both whole alternatives must occur (old={old_seen}, new={new_seen})"
    );
}

/// The canonical submit frame digest, computed with the same public encoder the command uses.
fn canonical_digest_for(set: &[RevPair], req: RelationAssert<'_>) -> [u8; 32] {
    let mut l = ledger();
    let payload = encode_submit(&req, &mut l).expect("codec");
    let ops = [typed::RequestOp::new(
        sage_db_kernel::relations::SUBMIT_RELATION_ASSERT,
        sage_db_kernel::relations::OPERATION_VERSION,
        &payload,
    )];
    let frame = typed::RequestFrame::new(&ops, set, None);
    let bytes = match typed::encode_request(&frame, &mut l) {
        typed::RequestOutcome::Ok(bytes) => bytes,
        other => panic!("frame: {other:?}"),
    };
    match typed::request_digest_of(&bytes, &mut l) {
        typed::RequestOutcome::Ok(digest) => digest,
        other => panic!("digest: {other:?}"),
    }
}

/// The canonical review frame digest, computed with the same public encoder.
fn canonical_review_digest(proposal: RecordKey, decision: u16, note: &str) -> [u8; 32] {
    let mut l = ledger();
    let payload = encode_review(proposal, decision, note, &mut l).expect("codec");
    let set = [RevPair {
        key: proposal,
        revision: RecordRevision::FIRST,
    }];
    let ops = [typed::RequestOp::new(
        sage_db_kernel::relations::REVIEW_RELATION_ASSERT,
        sage_db_kernel::relations::OPERATION_VERSION,
        &payload,
    )];
    let frame = typed::RequestFrame::new(&ops, &set, None);
    let bytes = match typed::encode_request(&frame, &mut l) {
        typed::RequestOutcome::Ok(bytes) => bytes,
        other => panic!("frame: {other:?}"),
    };
    match typed::request_digest_of(&bytes, &mut l) {
        typed::RequestOutcome::Ok(digest) => digest,
        other => panic!("digest: {other:?}"),
    }
}

#[test]
fn a_legacy_layer_pattern_the_formal_profile_would_reject_is_accepted() {
    use sage_db_types::layer::SemanticLayer;
    // Terms 1 and 2 are BOTH L2, so `1 is-a 2` is a same-layer definition: legal in legacy-v1,
    // refused by formal-v1's strictly-decreasing rule.
    let (_inner, _fs, mut store) = seed_world_profile(
        "/relation-legacy-root",
        sage_db_types::relation::RuleProfile::LegacyV1,
        [SemanticLayer::L2, SemanticLayer::L2, SemanticLayer::L0],
    );
    let mut l = ledger();

    let set = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-legacy",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut l,
    )
    .expect("legacy submit");
    review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-legacy",
        &mut l,
    )
    .expect("a same-layer definition is legal under legacy-v1");
    assert_eq!(read_assertion(&store, 1).from, term_key(1));

    // The IDENTICAL same-layer shape under formal-v1 is refused: same terms, same layers,
    // different rules, so the profile comes from the canonical validator rather than a copy.
    let (_inner2, _fs2, mut formal) = seed_world_profile(
        "/relation-formal-root",
        sage_db_types::relation::RuleProfile::FormalV1,
        [SemanticLayer::L2, SemanticLayer::L2, SemanticLayer::L0],
    );
    let fset = endpoint_set(1, 2, &formal);
    submit(
        &mut formal,
        &submitter(),
        "sub-formal",
        request(1, 2, RelationId::IsA, "a"),
        &fset,
        &mut l,
    )
    .expect("formal submit");
    match review(
        &mut formal,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-formal",
        &mut l,
    ) {
        Err(RelationAssertError::Input(RelationAssertInputError::InvalidCandidate {
            violations,
        })) => {
            assert!(
                violations.iter().any(|v| v.code() == "V3"),
                "formal-v1 must refuse the same-layer definition: {violations:?}"
            );
        }
        other => panic!("formal-v1 must refuse it, got {other:?}"),
    }
}

#[test]
fn the_validator_reports_a_reachable_cycle_code_through_this_command() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    let a = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-v6a",
        request(1, 2, RelationId::IsA, "a"),
        &a,
        &mut l,
    )
    .expect("submit a");
    review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-v6a",
        &mut l,
    )
    .expect("approve a");
    let b = endpoint_set(2, 1, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-v6b",
        request(2, 1, RelationId::IsA, "b"),
        &b,
        &mut l,
    )
    .expect("submit b");
    match review(
        &mut store,
        proposal_key(2),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-v6b",
        &mut l,
    ) {
        Err(RelationAssertError::Input(RelationAssertInputError::InvalidCandidate {
            violations,
        })) => {
            assert!(
                violations.iter().any(|v| matches!(v.code(), "V6" | "V3")),
                "a closing edge must be a cycle or direction violation: {violations:?}"
            );
        }
        other => panic!("the cycle must be refused, got {other:?}"),
    }
}

#[test]
fn successive_approvals_allocate_assertion_ids_forward_and_never_reuse() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    let a = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-occ",
        request(1, 2, RelationId::IsA, "a"),
        &a,
        &mut l,
    )
    .expect("submit");
    review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-occ",
        &mut l,
    )
    .expect("approve");

    let b = endpoint_set(2, 3, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-occ-2",
        request(2, 3, RelationId::IsA, "a"),
        &b,
        &mut l,
    )
    .expect("submit 2");
    review(
        &mut store,
        proposal_key(2),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-occ-2",
        &mut l,
    )
    .expect("approve 2");
    let first = read_assertion(&store, 1);
    let second = read_assertion(&store, 2);
    assert_ne!(first.key, second.key);
    assert_eq!(first.from, term_key(1));
    assert_eq!(second.from, term_key(2));
    // Scope, stated so the name cannot over-claim: this is FORWARD allocation from the committed
    // mark. Neither proposal attempts an occupied key, so collision refusal is proven separately
    // by `an_occupied_assertion_key_is_a_physical_collision_refusal`.
}

// ---------------------------------------------------------------------------
// Physical collision and exhaustion: the refusal arms, not just forward allocation
// ---------------------------------------------------------------------------

/// A raw assertion record for `from -> target` at an explicit id, revision 1.
fn raw_assertion(id: u128, from: u32, target: u32) -> sage_db_types::persist::AssertionRecordFull {
    sage_db_types::persist::AssertionRecordFull {
        key: assertion_key(id),
        revision: RecordRevision::FIRST,
        retracted: false,
        from: term_key(from),
        relation: RelationId::IsA,
        target: term_key(target),
        confidence: Confidence::Confirmed,
        declaration_order: 1,
        common: CommonFields {
            recorded_by: Some("seed-actor".to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("occupied-key fixture".to_owned()),
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        legacy_id: None,
        legacy_content_digest: None,
        extensions: Extensions::new(),
        changed_fields: None,
    }
}

/// Commit one assertion record at an explicit id, **without** advancing the AssertionId mark.
///
/// The graph stays valid (both endpoints exist, no self relation), so the only thing this
/// creates is an occupied key at a chosen id — exactly the physical situation a collision needs.
fn commit_assertion_without_mark(
    store: &mut Store<ObservingFs>,
    l: &mut AttemptLedger<'_>,
    record: &sage_db_types::persist::AssertionRecordFull,
) {
    let version = store.head().expect("head") + 1;
    let basis = store.commit_basis(IdSpace::TermId, l).expect("basis");
    let affected = [RevPair {
        key: record.key,
        revision: RecordRevision::FIRST,
    }];
    let result_bytes = typed::result_payload(version, &affected, &[], l).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    let events = vec![
        prepared(typed::prepare_event(
            EventInput::Assertion(typed::AssertionEvent::Created, record),
            l,
        )),
        prepared(typed::prepare_event(
            EventInput::Outcome(&sage_db_types::persist::RequestOutcomeEvent {
                principal_id: "seed-actor".to_owned(),
                request_id: format!("occupied-{version}"),
                payload_digest: [version as u8; 32],
                result_ref,
                commit_version: version,
                audit: sage_db_types::persist::CreationAudit {
                    recorded_by: Some("seed-actor".to_owned()),
                    actor_origin: ActorOrigin::Authenticated,
                    reason: None,
                    created_at: 1_700_000_000_000_000 + version as i64,
                },
                extensions: Extensions::new(),
            }),
            l,
        )),
    ];
    let blobs = vec![
        sage_db_types::prepared::BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: sage_db_types::prepared::BlobSource::Existing,
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(result_bytes),
        },
    ];
    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: [0x6d; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: "seed-actor".to_owned(),
        request_id: format!("occupied-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, l) {
        HandleOutcome::Committed { .. } => {}
        other => panic!("occupied-key fixture commit failed: {other:?}"),
    }
}

#[test]
fn an_occupied_assertion_key_is_a_physical_collision_refusal() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    // Occupy assertion id 1 with a real record, WITHOUT advancing the AssertionId mark, so the
    // committed watermark stays 0 and the next allocation still computes id 1.
    let occupied = raw_assertion(1, 1, 2);
    commit_assertion_without_mark(&mut store, &mut l, &occupied);
    let head_after_fixture = store.head();

    // Submit a proposal for a DIFFERENT edge on the same terms; approval would try id 1.
    let set = endpoint_set(2, 3, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-collide",
        request(2, 3, RelationId::IsA, "a"),
        &set,
        &mut l,
    )
    .expect("submit");
    let head_before = store.head();

    match review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-collide",
        &mut l,
    ) {
        Err(RelationAssertError::Input(RelationAssertInputError::AssertionIdCollision(key))) => {
            assert_eq!(key, assertion_key(1), "the refusal names the occupied key");
        }
        other => panic!("an occupied assertion key must be a collision refusal, got {other:?}"),
    }

    // The refusal names its cause AND left no trace: same head, proposal still Pending, and the
    // occupied record is untouched.
    assert_eq!(store.head(), head_before, "a collision must not commit");
    assert!(
        head_before > head_after_fixture,
        "the submit itself did commit"
    );
    match store.record(proposal_key(1), &mut l).expect("proposal") {
        sage_db_store::view::RecordLookup::Found { record, .. } => match record {
            FoldedRecord::Proposal(proposal) => {
                assert_eq!(proposal.status, ProposalStatus::Pending, "it stays Pending")
            }
            _ => panic!("proposal folds"),
        },
        other => panic!("proposal must exist: {other:?}"),
    }
    let still = read_assertion(&store, 1);
    assert_eq!(still.from, term_key(1), "the occupied record is unchanged");
    assert_eq!(still.target, term_key(2));
}

#[test]
fn a_saturated_assertion_mark_is_an_exhaustion_refusal() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    // Saturate the AssertionId mark with a real allocation event: the next id would overflow.
    commit_assertion_hwm(&mut store, &mut l, u128::MAX);

    let set = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-aexhaust",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut l,
    )
    .expect("submit");
    let head_before = store.head();

    match review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-aexhaust",
        &mut l,
    ) {
        Err(RelationAssertError::Input(RelationAssertInputError::AssertionIdExhausted)) => {}
        other => panic!("a saturated assertion space must refuse, got {other:?}"),
    }
    assert_eq!(store.head(), head_before, "exhaustion must not commit");
    assert!(matches!(
        store.record(assertion_key(1), &mut l).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
}

/// Advance the committed AssertionId watermark, with no new assertion id created.
fn commit_assertion_hwm(store: &mut Store<ObservingFs>, l: &mut AttemptLedger<'_>, hwm: u128) {
    let version = store.head().expect("head") + 1;
    let basis = store.commit_basis(IdSpace::AssertionId, l).expect("basis");
    let allocation = IdAllocationEvent {
        allocation: Watermark {
            space: IdSpace::AssertionId,
            hwm,
        },
        reason: None,
        extensions: Extensions::new(),
    };
    let result_bytes = typed::result_payload(version, &[], &[], l).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    let events = vec![
        prepared(typed::prepare_event(
            EventInput::IdAllocation(&allocation),
            l,
        )),
        prepared(typed::prepare_event(
            EventInput::Outcome(&sage_db_types::persist::RequestOutcomeEvent {
                principal_id: "seed-actor".to_owned(),
                request_id: format!("ahwm-{version}"),
                payload_digest: [version as u8; 32],
                result_ref,
                commit_version: version,
                audit: sage_db_types::persist::CreationAudit {
                    recorded_by: Some("seed-actor".to_owned()),
                    actor_origin: ActorOrigin::Authenticated,
                    reason: None,
                    created_at: 1_700_000_000_000_000 + version as i64,
                },
                extensions: Extensions::new(),
            }),
            l,
        )),
    ];
    let blobs = vec![
        sage_db_types::prepared::BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: sage_db_types::prepared::BlobSource::Existing,
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(result_bytes),
        },
    ];
    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: [0x6e; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: "seed-actor".to_owned(),
        request_id: format!("ahwm-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, l) {
        HandleOutcome::Committed { .. } => {}
        other => panic!("assertion hwm commit failed: {other:?}"),
    }
}

#[test]
fn an_occupied_proposal_key_is_a_physical_collision_refusal() {
    // The same physical situation for the ProposalId space: an occupied key at the id the
    // allocator will next compute, with the mark NOT advanced past it.
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    let occupied = sage_db_types::persist::ProposalRecord {
        key: proposal_key(1),
        revision: RecordRevision::FIRST,
        status: ProposalStatus::Pending,
        kind: ProposalKind::RelationAssert,
        subject: term_key(1),
        base_revisions: vec![RevPair {
            key: term_key(1),
            revision: RecordRevision::FIRST,
        }],
        payload_ref: raw_sha256(b"occupied-proposal-payload"),
        review_note: None,
        declared_author: "occupant".to_owned(),
        common: CommonFields {
            recorded_by: Some("occupant".to_owned()),
            actor_origin: ActorOrigin::Authenticated,
            reason: Some("occupied proposal key".to_owned()),
            created_at: 1_700_000_000_000_000,
            updated_at: None,
        },
        legacy_id: None,
        extensions: Extensions::new(),
        changed_fields: None,
    };
    commit_proposal_without_mark(&mut store, &mut l, &occupied);

    let set = endpoint_set(1, 2, &store);
    let head_before = store.head();
    match submit(
        &mut store,
        &submitter(),
        "sub-pcollide",
        request(1, 2, RelationId::IsA, "a"),
        &set,
        &mut l,
    ) {
        Err(RelationAssertError::Input(RelationAssertInputError::ProposalIdCollision(key))) => {
            assert_eq!(key, proposal_key(1));
        }
        other => panic!("an occupied proposal key must be a collision refusal, got {other:?}"),
    }
    assert_eq!(store.head(), head_before, "a collision must not commit");
}

/// Commit one proposal record at an explicit key, without advancing the ProposalId mark.
fn commit_proposal_without_mark(
    store: &mut Store<ObservingFs>,
    l: &mut AttemptLedger<'_>,
    record: &sage_db_types::persist::ProposalRecord,
) {
    let version = store.head().expect("head") + 1;
    let basis = store.commit_basis(IdSpace::TermId, l).expect("basis");
    let affected = [RevPair {
        key: record.key,
        revision: RecordRevision::FIRST,
    }];
    let result_bytes = typed::result_payload(version, &affected, &[], l).expect("result");
    let result_ref = raw_sha256(&result_bytes);
    let events = vec![
        prepared(typed::prepare_event(
            EventInput::Proposal {
                decided: false,
                record,
            },
            l,
        )),
        prepared(typed::prepare_event(
            EventInput::Outcome(&sage_db_types::persist::RequestOutcomeEvent {
                principal_id: "occupant".to_owned(),
                request_id: format!("poccupied-{version}"),
                payload_digest: [version as u8; 32],
                result_ref,
                commit_version: version,
                audit: sage_db_types::persist::CreationAudit {
                    recorded_by: Some("occupant".to_owned()),
                    actor_origin: ActorOrigin::Authenticated,
                    reason: None,
                    created_at: 1_700_000_000_000_000 + version as i64,
                },
                extensions: Extensions::new(),
            }),
            l,
        )),
    ];
    // The proposal references a payload blob, so it must be declared or the derivation refuses.
    let payload = b"occupied-proposal-payload".to_vec();
    let blobs = vec![
        sage_db_types::prepared::BlobDeclaration {
            sha256: basis.rules.rules_ref,
            declared_len: basis.rules_len,
            source: sage_db_types::prepared::BlobSource::Existing,
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: record.payload_ref,
            declared_len: payload.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(payload),
        },
        sage_db_types::prepared::BlobDeclaration {
            sha256: result_ref,
            declared_len: result_bytes.len() as u64,
            source: sage_db_types::prepared::BlobSource::Provide(result_bytes),
        },
    ];
    let prepared = sage_db_types::prepared::PreparedTransaction {
        version,
        prev_version: store.head(),
        tx_id: [0x6f; 16],
        kb_uuid: kb(),
        created_at: 1_700_000_000_000_000,
        principal_id: "occupant".to_owned(),
        request_id: format!("poccupied-{version}"),
        payload_digest: [version as u8; 32],
        expect_version: None,
        extensions: Extensions::new(),
        events,
        blobs,
        rules: basis.rules,
    };
    match store.commit(prepared, &Limits::V1, l) {
        HandleOutcome::Committed { .. } => {}
        other => panic!("occupied proposal fixture commit failed: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// V4 / V5 and two-reviewer CAS
// ---------------------------------------------------------------------------

#[test]
fn a_second_distinct_instantiates_target_is_a_v4_cardinality_refusal() {
    use sage_db_types::layer::SemanticLayer;
    // Instantiates is a definition relation, so formal-v1 needs target strictly below source.
    // Terms: 1 = L3 (instance), 2 and 3 = L0 (two distinct targets).
    let (_inner, _fs, mut store) = seed_world_profile(
        "/relation-v4-root",
        sage_db_types::relation::RuleProfile::FormalV1,
        [SemanticLayer::L3, SemanticLayer::L0, SemanticLayer::L0],
    );
    let mut l = ledger();

    // The seeded graph already has L3 term 1 with no instantiates edge, so V5 would fire first.
    // Give it exactly one target so the graph is valid, then propose the SECOND one.
    let first = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-v4a",
        request(1, 2, RelationId::Instantiates, "a"),
        &first,
        &mut l,
    )
    .expect("submit first instantiates");
    review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-v4a",
        &mut l,
    )
    .expect("one instantiation target is legal");

    let second = endpoint_set(1, 3, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-v4b",
        request(1, 3, RelationId::Instantiates, "a"),
        &second,
        &mut l,
    )
    .expect("submit second instantiates");
    match review(
        &mut store,
        proposal_key(2),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-v4b",
        &mut l,
    ) {
        Err(RelationAssertError::Input(RelationAssertInputError::InvalidCandidate {
            violations,
        })) => {
            assert!(
                violations.iter().any(|v| v.code() == "V4"),
                "a second DISTINCT instantiates target must be V4: {violations:?}"
            );
        }
        other => panic!("V4 cardinality must refuse, got {other:?}"),
    }
    assert!(matches!(
        store.record(assertion_key(2), &mut l).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
}

#[test]
fn a_parallel_instantiates_support_at_the_same_target_is_not_v4() {
    use sage_db_types::layer::SemanticLayer;
    // The rule counts DISTINCT endpoints, not supports: two assertions at the SAME target are
    // legal. This is the case a naive support-count implementation would wrongly refuse.
    let (_inner, _fs, mut store) = seed_world_profile(
        "/relation-v4-same-root",
        sage_db_types::relation::RuleProfile::FormalV1,
        [SemanticLayer::L3, SemanticLayer::L0, SemanticLayer::L0],
    );
    let mut l = ledger();

    let first = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-v4s-a",
        request(1, 2, RelationId::Instantiates, "a"),
        &first,
        &mut l,
    )
    .expect("submit");
    review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-v4s-a",
        &mut l,
    )
    .expect("first support");

    // A SECOND support for the same endpoint pair must still approve.
    let again = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-v4s-b",
        request(1, 2, RelationId::Instantiates, "b"),
        &again,
        &mut l,
    )
    .expect("submit duplicate support");
    review(
        &mut store,
        proposal_key(2),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-v4s-b",
        &mut l,
    )
    .expect("a duplicate support at the SAME target is legal, not V4");
    assert_eq!(read_assertion(&store, 2).target, term_key(2));
}

#[test]
fn an_uncovered_active_l3_term_is_a_v5_refusal_or_is_proven_unreachable() {
    use sage_db_types::layer::SemanticLayer;
    // Seed an L3 term with NO instantiates edge. If that state is reachable through this command
    // at all, the very next approval must be refused with V5, because the validator checks the
    // FULL current graph plus the candidate. If the state cannot be reached (the seeding itself
    // would have been refused), this test asserts the deliberate narrow scope instead.
    let (_inner, _fs, mut store) = seed_world_profile(
        "/relation-v5-root",
        sage_db_types::relation::RuleProfile::FormalV1,
        [SemanticLayer::L3, SemanticLayer::L0, SemanticLayer::L2],
    );
    let mut l = ledger();

    // Term 1 is L3 with no instantiation, so ANY approval on this graph must report V5.
    let set = endpoint_set(2, 1, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-v5",
        request(2, 1, RelationId::IsA, "a"),
        &set,
        &mut l,
    )
    .expect("submit");
    match review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "ok",
        "rev-v5",
        &mut l,
    ) {
        Err(RelationAssertError::Input(RelationAssertInputError::InvalidCandidate {
            violations,
        })) => {
            assert!(
                violations.iter().any(|v| v.code() == "V5"),
                "the uncovered L3 term must be reported as V5: {violations:?}"
            );
        }
        other => panic!("V5 must refuse an uncovered L3 graph, got {other:?}"),
    }
    // No assertion was created by the refused approval.
    assert!(matches!(
        store.record(assertion_key(1), &mut l).expect("read"),
        sage_db_store::view::RecordLookup::Absent { .. }
    ));
}

#[test]
fn two_reviewers_cas_on_the_same_proposal_and_an_unrelated_pending_is_untouched() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();

    // Proposal A on 1 -> 2, and an UNRELATED pending proposal B on 2 -> 3.
    let a = endpoint_set(1, 2, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-cas-a",
        request(1, 2, RelationId::IsA, "a"),
        &a,
        &mut l,
    )
    .expect("submit a");
    let b_set = endpoint_set(2, 3, &store);
    submit(
        &mut store,
        &submitter(),
        "sub-cas-b",
        request(2, 3, RelationId::IsA, "b"),
        &b_set,
        &mut l,
    )
    .expect("submit b");

    // Reviewer 1 approves A at the revision it read.
    review(
        &mut store,
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "first",
        "rev-cas-1",
        &mut l,
    )
    .expect("reviewer one approves");

    // Reviewer 2, holding the SAME old revision, submits a NOVEL request: CAS refuses it.
    struct SecondReviewer;
    let second = Grants::new(
        "second-reviewer",
        kb(),
        CapabilitySet::of(&[Capability::Review]),
    );
    let _ = SecondReviewer;
    match review_relation_assert(
        &mut store,
        &second,
        meta("rev-cas-2"),
        proposal_key(1),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "second",
        Budget::DEFAULT,
        &RunToCompletion,
        &Limits::V1,
        &mut l,
    ) {
        Err(RelationAssertError::Command(CommandError::Input(
            sage_db_kernel::claims::InputError::StaleBaseline { key, expected },
        ))) => {
            assert_eq!(key, proposal_key(1));
            assert_eq!(
                expected,
                RecordRevision::FIRST,
                "the stale baseline is the OLD revision"
            );
        }
        other => {
            panic!("a second reviewer on the old revision must be a CAS refusal, got {other:?}")
        }
    }

    // The unrelated pending proposal B is untouched: still Pending at revision 1, and its
    // endpoints' revisions were not disturbed by a review of A.
    match store.record(proposal_key(2), &mut l).expect("proposal b") {
        sage_db_store::view::RecordLookup::Found { record, .. } => match record {
            FoldedRecord::Proposal(proposal) => {
                assert_eq!(proposal.status, ProposalStatus::Pending);
                assert_eq!(proposal.revision, RecordRevision::FIRST);
            }
            _ => panic!("proposal b folds"),
        },
        other => panic!("proposal b must still exist: {other:?}"),
    }

    // And B can still be approved afterwards, which is the point of not scanning unrelated rows.
    review(
        &mut store,
        proposal_key(2),
        RecordRevision::FIRST,
        DECISION_APPROVE,
        "later",
        "rev-cas-3",
        &mut l,
    )
    .expect("the unrelated pending proposal still approves");
    assert_eq!(read_assertion(&store, 2).from, term_key(2));
}
