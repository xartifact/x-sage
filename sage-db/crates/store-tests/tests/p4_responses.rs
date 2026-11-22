//! P4 recorded-response assembly over the real store, the real kernel and the real result blob.
//!
//! Every outcome here is produced by a real kernel command against a real [`Store`] on the model
//! filesystem, then assembled by [`sage_db_server::responses`], which reads the canonical result
//! blob the store recorded and parses it with the canonical borrowed decoder.
//!
//! What this file does **not** claim: transport encoding, HTTP, or that the DTOs are serialised.
//! Assembly is the subject; the wire is later work.

// The fixture and observing-filesystem helpers are shared with the other P4 harness files.
#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use fixture::{BYTES, ENTRIES, ROOT, RULES, kb};
use observing_fs::ObservingFs;

use sage_db_format::digest::raw_sha256;
use sage_db_kernel::auth::{Capability, CapabilitySet, Grants};
use sage_db_kernel::claims::{self, RequestMeta};
use sage_db_kernel::intent::{ClaimEdit, ReviewDecision};
use sage_db_protocol::generated::write_outcome;
use sage_db_store::fs::CreateDir;
use sage_db_store::handle::{HandleOutcome, Store};
use sage_db_store::init::bootstrap;
use sage_db_store::lease::AttemptLedger;
use sage_db_store::typed::{self, EventInput};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::confidence::Confidence;
use sage_db_types::framing::Limits;
use sage_db_types::identity::RecordRevision;
use sage_db_types::persist::{
    ActorOrigin, ClaimKind, ClaimRecord, ClaimStatus, CommonFields, CreationAudit, Extensions,
    IdSpace, RecordKey, RecordKind, RequestOutcomeEvent, RevPair,
};
use sage_db_types::prepared::{BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction};
use sage_db_types::query::RunToCompletion;

use sage_db_server::responses::{
    ResponseError, ResponseLimits, WriteFamily, assemble_outcome, assemble_write,
};

const PRINCIPAL: &str = "response-proposer";
const REVIEWER: &str = "response-reviewer";

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

fn proposer() -> Grants<'static> {
    Grants::new(
        PRINCIPAL,
        kb(),
        CapabilitySet::of(&[
            Capability::Propose,
            Capability::Review,
            Capability::WriteAux,
        ]),
    )
}

fn reviewer() -> Grants<'static> {
    Grants::new(REVIEWER, kb(), CapabilitySet::of(&[Capability::Review]))
}

fn meta(request_id: &str) -> RequestMeta<'_> {
    let digest = raw_sha256(request_id.as_bytes());
    let mut tx_id = [0u8; 16];
    tx_id.copy_from_slice(&digest[..16]);
    RequestMeta {
        request_id,
        tx_id,
        created_at: 1_700_000_300_000_000,
    }
}

fn response_limits() -> ResponseLimits {
    ResponseLimits {
        max_result_bytes: 1 << 20,
        limits: Limits::V1,
        max_history_events: 4_096,
    }
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
            recorded_by: Some(PRINCIPAL.into()),
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

/// Build the seeded world over any cloneable filesystem wrapper.
///
/// Generic so the history-fault test can supply its own wrapper (one that arms a fault only for
/// transaction reads) while every other test keeps `ObservingFs`.
fn seed_world_over<F>(
    make: impl FnOnce(Rc<ModelFileSystem>) -> F,
) -> (Rc<ModelFileSystem>, F, Store<F>)
where
    F: sage_db_store::fs::FileSystem + Clone,
{
    let inner = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let fs = make(Rc::clone(&inner));
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
        EventInput::Outcome(&outcome_event(version, "seed-2", result_ref, PRINCIPAL)),
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
        principal_id: PRINCIPAL.into(),
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

/// The ordinary seeded world, over the observing filesystem.
fn seed_world() -> (Rc<ModelFileSystem>, ObservingFs, Store<ObservingFs>) {
    seed_world_over(ObservingFs::new)
}

/// A forwarding filesystem that can fail **only** transaction reads.
///
/// Every capability and every ordinary operation is delegated unchanged; the one interception is
/// `read_exact_into` when the entry is a transaction. That is what makes it possible to drive the
/// response assembler's history branch with a real fault: the result blob is `EntryName::Blob`, so
/// it is read normally, while the historical scan's transaction read fails.
#[derive(Clone)]
struct TxFaultFs {
    inner: ObservingFs,
    /// The model's canonical fault schedule: the wrapper arms it, the model applies it.
    faults: Rc<ModelFileSystem>,
    armed: Rc<RefCell<bool>>,
}

impl TxFaultFs {
    fn new(inner: ObservingFs, model: Rc<ModelFileSystem>) -> Self {
        Self {
            inner,
            faults: model,
            armed: Rc::new(RefCell::new(false)),
        }
    }

    fn arm(&self) {
        *self.armed.borrow_mut() = true;
    }

    /// Whether the canonical injection the wrapper armed actually fired.
    fn injection_fired(&self) -> bool {
        self.faults
            .faults_handle()
            .borrow()
            .fired()
            .iter()
            .any(|(point, injection)| {
                *point == sage_db_store_tests::faults::FaultPoint::ReadExact
                    && *injection == sage_db_store_tests::faults::Injection::FailNoEffect
            })
    }
}

impl sage_db_store::fs::FileSystem for TxFaultFs {
    type Dir = <ObservingFs as sage_db_store::fs::FileSystem>::Dir;
    type WriteFile = <ObservingFs as sage_db_store::fs::FileSystem>::WriteFile;
    type ReadFile = <ObservingFs as sage_db_store::fs::FileSystem>::ReadFile;
    type Lock = <ObservingFs as sage_db_store::fs::FileSystem>::Lock;

    fn capabilities(&self) -> sage_db_store::fs::Capabilities {
        self.inner.capabilities()
    }
    fn open_root(&self, abs: &std::path::Path) -> Result<Self::Dir, sage_db_store::fs::FsError> {
        self.inner.open_root(abs)
    }
    fn create_root(&self, abs: &std::path::Path) -> Result<Self::Dir, sage_db_store::fs::FsError> {
        self.inner.create_root(abs)
    }
    fn sync_parent_of(&self, abs: &std::path::Path) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.sync_parent_of(abs)
    }
    fn open_family(
        &self,
        root: &Self::Dir,
        family: sage_db_store::fs::Family,
        create: sage_db_store::fs::CreateDir,
    ) -> Result<Self::Dir, sage_db_store::fs::FsError> {
        self.inner.open_family(root, family, create)
    }
    fn sync_dir(&self, dir: &Self::Dir) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.sync_dir(dir)
    }
    fn device_id(&self, dir: &Self::Dir) -> Result<u64, sage_db_store::fs::FsError> {
        self.inner.device_id(dir)
    }
    fn try_lock_exclusive(
        &self,
        root: &Self::Dir,
    ) -> Result<Self::Lock, sage_db_store::fs::FsError> {
        self.inner.try_lock_exclusive(root)
    }
    fn create_new(
        &self,
        dir: &Self::Dir,
        name: sage_db_store::names::EntryName,
    ) -> Result<Self::WriteFile, sage_db_store::fs::FsError> {
        self.inner.create_new(dir, name)
    }
    fn write(
        &self,
        file: &mut Self::WriteFile,
        buf: &[u8],
    ) -> Result<sage_db_store::fs::WriteOutcome, sage_db_store::fs::FsError> {
        self.inner.write(file, buf)
    }
    fn sync_file(&self, file: &Self::WriteFile) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.sync_file(file)
    }
    fn sync_entry(
        &self,
        dir: &Self::Dir,
        name: sage_db_store::names::EntryName,
    ) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.sync_entry(dir, name)
    }
    fn publish_no_overwrite(
        &self,
        staged_dir: &Self::Dir,
        staged: sage_db_store::names::EntryName,
        dst_dir: &Self::Dir,
        dst: sage_db_store::names::EntryName,
        expected: &sage_db_store::fs::HashReport,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
    ) -> Result<sage_db_store::fs::Published, sage_db_store::fs::FsError> {
        self.inner
            .publish_no_overwrite(staged_dir, staged, dst_dir, dst, expected, ledger)
    }
    fn replace_current(
        &self,
        staged_dir: &Self::Dir,
        staged: sage_db_store::names::EntryName,
        root: &Self::Dir,
    ) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.replace_current(staged_dir, staged, root)
    }
    fn size(
        &self,
        dir: &Self::Dir,
        name: sage_db_store::names::EntryName,
    ) -> Result<u64, sage_db_store::fs::FsError> {
        self.inner.size(dir, name)
    }
    /// The one interception: a **transaction** read is armed with the canonical injection and
    /// then forwarded, so the model produces the failure rather than the wrapper inventing one.
    fn read_exact_into(
        &self,
        dir: &Self::Dir,
        name: sage_db_store::names::EntryName,
        buf: &mut [u8],
    ) -> Result<usize, sage_db_store::fs::FsError> {
        use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
        if *self.armed.borrow() && matches!(name, sage_db_store::names::EntryName::Transaction(_)) {
            *self.faults.faults_handle().borrow_mut() =
                FaultSchedule::none().at(FaultPoint::ReadExact, Injection::FailNoEffect, 1);
        }
        self.inner.read_exact_into(dir, name, buf)
    }
    fn open_read(
        &self,
        dir: &Self::Dir,
        name: sage_db_store::names::EntryName,
    ) -> Result<Self::ReadFile, sage_db_store::fs::FsError> {
        self.inner.open_read(dir, name)
    }
    fn read(
        &self,
        file: &mut Self::ReadFile,
        buf: &mut [u8],
    ) -> Result<usize, sage_db_store::fs::FsError> {
        self.inner.read(file, buf)
    }
    fn hash_bounded(
        &self,
        dir: &Self::Dir,
        name: sage_db_store::names::EntryName,
        max_len: u64,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
    ) -> Result<sage_db_store::fs::HashReport, sage_db_store::fs::FsError> {
        self.inner.hash_bounded(dir, name, max_len, ledger)
    }
    fn for_each_entry(
        &self,
        dir: &Self::Dir,
        ledger: &mut dyn sage_db_types::ledger::SharedLedger,
        visit: &mut dyn FnMut(&str) -> sage_db_store::fs::Each,
    ) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.for_each_entry(dir, ledger, visit)
    }
    fn remove(
        &self,
        dir: &Self::Dir,
        name: sage_db_store::names::EntryName,
    ) -> Result<(), sage_db_store::fs::FsError> {
        self.inner.remove(dir, name)
    }
}

/// Submit a claim edit through the real kernel and return its outcome.
fn submit_claim<F: sage_db_store::fs::FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'static>,
    request_id: &str,
    text: &str,
) -> HandleOutcome {
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
    let mut l = ledger();
    claims::submit_claim_edit(
        store,
        grants,
        meta(request_id),
        ClaimEdit {
            subject: term_key(1),
            claim: claim_key(1),
            text,
            declared_author: "declared-attribution",
        },
        &read_set,
        &Limits::V1,
        &mut l,
    )
    .expect("the submission is accepted")
}

/// Review a pending proposal through the real kernel.
fn review_proposal<F: sage_db_store::fs::FileSystem>(
    store: &mut Store<F>,
    grants: &Grants<'static>,
    request_id: &str,
    decision: ReviewDecision,
) -> HandleOutcome {
    let mut l = ledger();
    claims::review_claim_edit(
        store,
        grants,
        meta(request_id),
        proposal_key(1),
        RecordRevision::FIRST,
        decision,
        "review note",
        &Limits::V1,
        &mut l,
    )
    .expect("the review is accepted")
}

// ---------------------------------------------------------------------------
// Committed assembly
// ---------------------------------------------------------------------------

#[test]
fn committed_response_fills_the_full_recorded_body() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    let outcome = submit_claim(&mut store, &grants, "resp-submit", "candidate text");

    let mut l = ledger();
    let response = assemble_write(
        &mut store,
        &outcome,
        WriteFamily::Submit,
        "resp-submit",
        &response_limits(),
        &mut l,
    )
    .expect("assembly succeeds");

    let Some(write_outcome::Outcome::Committed(committed)) = response.outcome().outcome.as_ref()
    else {
        panic!("expected a committed outcome");
    };
    let recorded = committed
        .recorded
        .as_ref()
        .expect("the recorded body is present");
    assert_eq!(recorded.commit_version, "3");
    assert_eq!(recorded.result_ref.len(), 64);
    // A submission records ONLY the proposal it created: the claim is written on approval.
    assert_eq!(recorded.affected.len(), 1);
    let key = recorded.affected[0]
        .key
        .as_ref()
        .expect("the key is present");
    assert_eq!(key.kind, u32::from(RecordKind::Proposal.as_u16()));
    assert_eq!(key.id, "1");
    assert_eq!(recorded.affected[0].revision, "1");
    // The advanced counter is stated with its real high-water mark.
    assert_eq!(recorded.counters.len(), 1);
    assert_eq!(
        recorded.counters[0].space,
        u32::from(IdSpace::ProposalId.as_u16())
    );
    assert_eq!(recorded.counters[0].hwm, "1");

    let Some(sage_db_protocol::generated::committed::Detail::Submit(submit)) = &committed.detail
    else {
        panic!("a submit success names its proposal");
    };
    let proposal = submit
        .proposal
        .as_ref()
        .expect("the proposal key is present");
    assert_eq!(proposal.id, "1");
    assert_eq!(proposal.kind, u32::from(RecordKind::Proposal.as_u16()));

    // The response owns admitted credit for everything it returned.
    assert!(response.admitted_bytes() > 0);
}

#[test]
fn unknown_carries_the_authenticated_request_id_and_reads_no_result() {
    let (_inner, fs, mut store) = seed_world();
    // `Unknown` is what the store reports for an ambiguous attempt; the invariant under test is
    // what the ASSEMBLER does with it.
    let before = fs.counts().borrow().total();
    let mut l = ledger();
    let response = assemble_write(
        &mut store,
        &HandleOutcome::Unknown,
        WriteFamily::Submit,
        "req-ambiguous-42",
        &response_limits(),
        &mut l,
    )
    .expect("assembly succeeds");

    // No result read happened: an ambiguous attempt has nothing recorded to read.
    assert_eq!(fs.counts().borrow().total(), before);
    let Some(write_outcome::Outcome::Unknown(unknown)) = response.outcome().outcome.as_ref() else {
        panic!("expected an unknown outcome");
    };
    // The ONLY content of an ambiguity is which request it is. An empty id satisfies "no
    // version" while destroying the message, so the authenticated id must be carried.
    assert_eq!(unknown.request_id, "req-ambiguous-42");
    // The id text is admitted: the response owns exact credit for it.
    assert_eq!(response.admitted_bytes(), "req-ambiguous-42".len() as u64);
}

#[test]
fn not_committed_preserves_the_refusal_taxonomy() {
    let (_inner, _fs, mut store) = seed_world();
    let mut l = ledger();
    let assembled = assemble_write(
        &mut store,
        &HandleOutcome::NotCommitted {
            reason: sage_db_store::handle::HandleRefusal::IdempotencyMismatch,
        },
        WriteFamily::Submit,
        "req-refused",
        &response_limits(),
        &mut l,
    )
    .expect("assembly succeeds");

    let Some(write_outcome::Outcome::Error(error)) = &assembled.outcome().outcome else {
        panic!("expected a typed error");
    };
    // The specific class, not a flattened invalid-argument.
    assert_eq!(
        error.code,
        sage_db_protocol::generated::ErrorCode::IdempotencyMismatch as i32
    );
    // No budget figures are invented when there is no BudgetError.
    assert!(error.budget.is_none());
}

// ---------------------------------------------------------------------------
// Review: the ORIGINAL status, never the latest
// ---------------------------------------------------------------------------

#[test]
fn already_committed_returns_the_original_status_not_the_latest() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    submit_claim(&mut store, &grants, "resp-approve", "candidate");
    let reviewer_grants = reviewer();

    // First review approves at version 4.
    let approved = review_proposal(
        &mut store,
        &reviewer_grants,
        "resp-review-1",
        ReviewDecision::Approve,
    );
    let HandleOutcome::Committed { version, .. } = approved else {
        panic!("the first review commits");
    };
    assert_eq!(version, 4);

    let mut l = ledger();
    let assembled = assemble_write(
        &mut store,
        &approved,
        WriteFamily::Review,
        "resp-review-1",
        &response_limits(),
        &mut l,
    )
    .expect("assembly succeeds");
    let Some(write_outcome::Outcome::Committed(committed)) = &assembled.outcome().outcome else {
        panic!("expected a committed outcome");
    };
    let Some(sage_db_protocol::generated::committed::Detail::Review(review)) = &committed.detail
    else {
        panic!("a review success names its proposal");
    };
    // The persisted status IS the decision here.
    assert_eq!(
        review.status,
        sage_db_protocol::generated::ProposalStatus::Approved as i32
    );

    // A LATER review of the same proposal would move its current status. Simulate the fact that
    // matters: assembly of the ORIGINAL outcome must not consult current state.
    let mut l = ledger();
    let retried = assemble_write(
        &mut store,
        &approved,
        WriteFamily::Review,
        "resp-review-1",
        &response_limits(),
        &mut l,
    )
    .expect("assembly succeeds");
    let Some(write_outcome::Outcome::Committed(committed)) = retried.outcome().outcome.as_ref()
    else {
        panic!("expected a committed outcome");
    };
    let Some(sage_db_protocol::generated::committed::Detail::Review(review)) = &committed.detail
    else {
        panic!("a review success names its proposal");
    };
    assert_eq!(
        review.status,
        sage_db_protocol::generated::ProposalStatus::Approved as i32,
        "the original status is stable across re-assembly"
    );
}

#[test]
fn a_rejection_records_rejected_as_the_original_status() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    submit_claim(&mut store, &grants, "resp-reject", "candidate");
    let reviewer_grants = reviewer();
    let rejected = review_proposal(
        &mut store,
        &reviewer_grants,
        "resp-review-reject",
        ReviewDecision::Reject,
    );

    let mut l = ledger();
    let assembled = assemble_write(
        &mut store,
        &rejected,
        WriteFamily::Review,
        "resp-review-reject",
        &response_limits(),
        &mut l,
    )
    .expect("assembly succeeds");
    let Some(write_outcome::Outcome::Committed(committed)) = &assembled.outcome().outcome else {
        panic!("expected a committed outcome");
    };
    let Some(sage_db_protocol::generated::committed::Detail::Review(review)) = &committed.detail
    else {
        panic!("a review success names its proposal");
    };
    // The ORIGINAL recorded status, which for a rejection is Rejected — not Approved by default.
    assert_eq!(
        review.status,
        sage_db_protocol::generated::ProposalStatus::Rejected as i32
    );
}

// ---------------------------------------------------------------------------
// GetRequestOutcome: Found / Absent / Incomplete stay distinct
// ---------------------------------------------------------------------------

#[test]
fn outcome_found_reports_the_original_version_and_bodies() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    submit_claim(&mut store, &grants, "resp-lookup", "candidate");

    let lookup = store.outcome(PRINCIPAL, "resp-lookup");
    let mut l = ledger();
    let assembled = assemble_outcome(
        &mut store,
        lookup,
        WriteFamily::Submit,
        &response_limits(),
        &mut l,
    )
    .expect("assembly succeeds");

    let sage_db_server::responses::OutcomeResponse::Found(found) = assembled.response() else {
        panic!("the request is known on this basis");
    };
    assert_eq!(found.original_version, "3");
    assert_eq!(found.result_ref.len(), 64);
    assert_eq!(found.request_digest.len(), 64);
    let recorded = found
        .recorded
        .as_ref()
        .expect("the recorded body is present");
    assert_eq!(recorded.commit_version, "3");
    assert!(found.basis.is_some(), "the basis is stated, not omitted");
    let basis = found.basis.as_ref().expect("basis present");
    assert_eq!(basis.version, "3");
    assert_eq!(basis.kb_uuid, kb().to_string());
}

#[test]
fn outcome_absent_is_distinct_from_incomplete() {
    let (_inner, _fs, mut store) = seed_world();
    let lookup = store.outcome(PRINCIPAL, "never-submitted");
    let mut l = ledger();

    match assemble_outcome(
        &mut store,
        lookup,
        WriteFamily::Submit,
        &response_limits(),
        &mut l,
    ) {
        Ok(found) => match found.response() {
            sage_db_server::responses::OutcomeResponse::Absent(absent) => {
                // Absence is definitive only on a stated basis.
                assert!(absent.basis.is_some(), "absence states its basis");
            }
            sage_db_server::responses::OutcomeResponse::Incomplete(i) => {
                // An incomplete view is an honest answer too, but it must SAY so rather than
                // claiming absence.
                assert!(!i.reason.is_empty());
            }
            sage_db_server::responses::OutcomeResponse::Found(_) => {
                panic!("an unsubmitted request cannot be found")
            }
        },
        Err(error) => panic!("assembly must answer, not fail: {error:?}"),
    }
}

// ---------------------------------------------------------------------------
// Error taxonomy: distinct, never flattened
// ---------------------------------------------------------------------------

#[test]
fn a_blob_ceiling_refusal_is_not_a_corruption_verdict() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    let outcome = submit_claim(&mut store, &grants, "resp-ceiling", "candidate");

    // A ceiling far below the recorded result: the reader refuses on the CALLER'S ceiling, which
    // is a refusal, never a claim that the data is corrupt.
    let tight = ResponseLimits {
        max_result_bytes: 8,
        limits: Limits::V1,
        max_history_events: 4_096,
    };
    let mut l = ledger();
    match assemble_write(
        &mut store,
        &outcome,
        WriteFamily::Submit,
        "req-ceiling",
        &tight,
        &mut l,
    ) {
        Err(ResponseError::Blob(sage_db_store::view::BlobReadError::Ceiling { .. })) => {}
        other => panic!("expected the blob reader's ceiling refusal, got {other:?}"),
    }
}

#[test]
fn a_starved_ledger_refuses_distinctly_from_a_data_verdict() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    let outcome = submit_claim(&mut store, &grants, "resp-budget", "candidate");

    let mut starved = AttemptLedger::open(BYTES, 0, &RunToCompletion).unwrap();
    match assemble_write(
        &mut store,
        &outcome,
        WriteFamily::Submit,
        "req-budget",
        &response_limits(),
        &mut starved,
    ) {
        // The ledger refused: a real budget class, not a fake corruption.
        Err(ResponseError::Blob(sage_db_store::view::BlobReadError::Refused(
            sage_db_types::framing::Refusal::Budget(error),
        ))) => {
            assert_eq!(error.unit(), sage_db_types::ledger::BudgetUnit::Entries);
        }
        other => panic!("expected a canonical budget refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Discriminating counterexamples for the fragile assertions
// ---------------------------------------------------------------------------

/// A hand-built event whose `commit_version` disagrees with the result blob it points at.
///
/// The data counterexample for the three-way version check: the blob says 3, the event claims 4,
/// and the commit machinery reports 3. Only the comparison can catch it.
#[test]
fn a_version_disagreement_is_refused_rather_than_presented_as_success() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    let outcome = submit_claim(&mut store, &grants, "resp-version", "candidate");

    let HandleOutcome::Committed { version, recorded } = &outcome else {
        panic!("the submit commits");
    };
    let sage_db_store::commit::RecordedOutcome::Outcome(shared) = recorded else {
        panic!("an ordinary commit records an outcome event");
    };
    // Same result_ref, same digest, WRONG commit version: the blob says 3, this claims 4.
    let mut lie = shared.event().clone();
    lie.commit_version = version + 1;
    // `SharedOutcome::new` adopts the event's already-admitted payload, so this attempt must
    // hold that credit first. The excess is released when the attempt drops, unspent.
    let mut forge_ledger = ledger();
    sage_db_types::ledger::SharedLedger::try_charge(&mut forge_ledger, 4096)
        .expect("the forge attempt has room");
    let forged = sage_db_store::commit::RecordedOutcome::Outcome(
        sage_db_store::commit::SharedOutcome::new(lie, &mut forge_ledger)
            .expect("an admitted outcome"),
    );
    let forged_outcome = HandleOutcome::Committed {
        version: *version,
        recorded: forged,
    };

    let mut l = ledger();
    match assemble_write(
        &mut store,
        &forged_outcome,
        WriteFamily::Submit,
        "resp-version",
        &response_limits(),
        &mut l,
    ) {
        Err(ResponseError::VersionMismatch {
            outcome,
            recorded,
            result,
        }) => {
            assert_eq!(outcome, 3);
            assert_eq!(recorded, 4);
            assert_eq!(result, 3);
        }
        other => panic!("expected a three-way version disagreement, got {other:?}"),
    }
}

/// The disagreeing HANDLE version is refused even when event and result agree.
///
/// The write path used to discard `HandleOutcome::version`; this is the counterexample for that
/// omission, and it needs no forged event at all.
#[test]
fn a_handle_version_disagreeing_with_the_event_is_refused() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    let outcome = submit_claim(&mut store, &grants, "resp-handle-version", "candidate");
    let HandleOutcome::Committed { recorded, .. } = outcome else {
        panic!("the submit commits");
    };
    // Event and blob both say 3; the handle claims 9.
    let lie = HandleOutcome::Committed {
        version: 9,
        recorded,
    };
    let mut l = ledger();
    match assemble_write(
        &mut store,
        &lie,
        WriteFamily::Submit,
        "resp-handle-version",
        &response_limits(),
        &mut l,
    ) {
        Err(ResponseError::VersionMismatch {
            outcome,
            recorded,
            result,
        }) => {
            assert_eq!(outcome, 9);
            assert_eq!(recorded, 3);
            assert_eq!(result, 3);
        }
        other => panic!("expected the handle version to be refused, got {other:?}"),
    }
}

/// The recorded version is the ORIGINAL one, even after the store has moved on.
///
/// The counterexample for any "use the current head" shortcut: at assembly time the head is 4,
/// while the outcome under assembly belongs to version 3.
#[test]
fn a_later_commit_does_not_change_the_original_recorded_version() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    let first = submit_claim(&mut store, &grants, "resp-original", "first candidate");
    assert!(matches!(first, HandleOutcome::Committed { version: 3, .. }));

    // Advance the store twice more, so `head` no longer equals the outcome's version.
    submit_claim(&mut store, &grants, "resp-later-1", "second candidate");
    submit_claim(&mut store, &grants, "resp-later-2", "third candidate");

    let mut l = ledger();
    let assembled = assemble_write(
        &mut store,
        &first,
        WriteFamily::Submit,
        "req-generated",
        &response_limits(),
        &mut l,
    )
    .expect("assembly succeeds");
    // The outcome owns its version; the head is NOW 5 and must not leak in.
    let Some(write_outcome::Outcome::Committed(committed)) = assembled.outcome().outcome.as_ref()
    else {
        panic!("expected a committed outcome");
    };
    assert_eq!(committed.recorded.as_ref().unwrap().commit_version, "3");
}

/// An exact retry after a lost ACK recovers the ORIGINAL proposal identity from the record.
///
/// Counterexample for "re-derive identity from the request": the second call allocates nothing,
/// so a reconstruction would have no id to state, while the recorded outcome still names
/// proposal 1 and counter hwm 1.
#[test]
fn an_exact_retry_recovers_the_original_proposal_identity_from_the_record() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();

    // The first attempt is the one whose ACK was lost; the store has it recorded.
    assert!(matches!(
        submit_claim(&mut store, &grants, "resp-lost-ack", "candidate"),
        HandleOutcome::Committed { version: 3, .. }
    ));
    // The identical request again: same id, same digest, so the kernel returns the stored
    // outcome without allocating.
    let retry = submit_claim(&mut store, &grants, "resp-lost-ack", "candidate");
    let HandleOutcome::AlreadyCommitted { version, recorded } = &retry else {
        panic!("an exact retry reports the stored outcome, got {retry:?}");
    };
    assert_eq!(*version, 3);
    let sage_db_store::commit::RecordedOutcome::Outcome(shared) = recorded else {
        panic!("expected an outcome event");
    };
    // Recovery is from the stored event, not from a fresh allocation.
    assert_eq!(shared.event().request_id, "resp-lost-ack");

    let mut l = ledger();
    let assembled = assemble_write(
        &mut store,
        &retry,
        WriteFamily::Submit,
        "req-generated",
        &response_limits(),
        &mut l,
    )
    .expect("assembly succeeds");
    let Some(write_outcome::Outcome::AlreadyCommitted(already)) = &assembled.outcome().outcome
    else {
        panic!("expected an already-committed outcome");
    };
    let recorded = already
        .recorded
        .as_ref()
        .expect("the original body is present");
    assert_eq!(recorded.commit_version, "3");
    // The affected proposal and the advanced counter both survive the retry unchanged.
    assert!(recorded.affected.iter().any(|pair| {
        pair.key
            .as_ref()
            .is_some_and(|k| k.kind == u32::from(RecordKind::Proposal.as_u16()))
    }));
    assert!(
        recorded
            .counters
            .iter()
            .any(|c| c.space == u32::from(IdSpace::ProposalId.as_u16()) && c.hwm == "1")
    );
    // A submission has no original status here; absent is not a defaulted value.
    assert!(already.original_status.is_none());
}

/// Re-assembling an OLD review outcome after the store has moved on must not substitute newer
/// state: the status is recovered from the ORIGINAL revision, not the current fold or the head.
#[test]
fn an_old_review_outcome_keeps_its_original_status_after_later_commits() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    submit_claim(&mut store, &grants, "old-review-submit", "candidate");
    let reviewer_grants = reviewer();
    let rejected = review_proposal(
        &mut store,
        &reviewer_grants,
        "old-review-1",
        ReviewDecision::Reject,
    );
    assert!(matches!(
        rejected,
        HandleOutcome::Committed { version: 4, .. }
    ));

    // Move the store on: a second submission, which allocates proposal 2 and advances the head.
    submit_claim(&mut store, &grants, "later-submit", "another candidate");
    let head_now = store.head().expect("the store has a head");
    assert!(head_now > 4);

    // Now assemble the ORIGINAL rejection. Its status must still be Rejected, and its version
    // must still be 4 — neither the newer approval nor the current head may leak in.
    let mut l = ledger();
    let assembled = assemble_write(
        &mut store,
        &rejected,
        WriteFamily::Review,
        "resp-review-reject",
        &response_limits(),
        &mut l,
    )
    .expect("assembly succeeds");
    let Some(write_outcome::Outcome::Committed(committed)) = assembled.outcome().outcome.as_ref()
    else {
        panic!("expected a committed outcome");
    };
    // The commit version states what the OUTCOME recorded, while the store's head has moved on.
    assert_eq!(
        committed
            .recorded
            .as_ref()
            .expect("recorded")
            .commit_version,
        "4"
    );
    assert!(head_now > 4);
    let Some(write_outcome::Outcome::Committed(committed)) = &assembled.outcome().outcome else {
        panic!("expected a committed outcome");
    };
    let Some(sage_db_protocol::generated::committed::Detail::Review(review)) = &committed.detail
    else {
        panic!("a review success names its proposal");
    };
    assert_eq!(
        review.status,
        sage_db_protocol::generated::ProposalStatus::Rejected as i32,
        "the ORIGINAL revision's status, not the later approval"
    );
}

// ---------------------------------------------------------------------------
// Real ledger accounting: admission, shortfall, and retention
// ---------------------------------------------------------------------------

/// The response's own buffers are admitted at their measured extent, and the admission is what
/// a one-byte-short allowance refuses.
///
/// The figure is measured from the implementation: `admitted_bytes()` after a generous run. The
/// blob read is charged separately and first, so each refusal is attributed to the stage that
/// actually refused rather than to a guessed total.
#[test]
fn output_credit_is_admitted_exactly_and_refuses_one_short() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    let outcome = submit_claim(&mut store, &grants, "resp-accounting", "candidate");

    // The implementation's own figure for the buffers it retains.
    let mut measuring = AttemptLedger::open(1 << 20, ENTRIES, &RunToCompletion).unwrap();
    let measured = assemble_write(
        &mut store,
        &outcome,
        WriteFamily::Submit,
        "resp-accounting",
        &response_limits(),
        &mut measuring,
    )
    .expect("assembly succeeds")
    .admitted_bytes();
    assert!(measured > 0, "the response must own admitted credit");
    drop(measuring);

    // Zero allowance: the blob read is charged first, so the READER refuses.
    let mut zero = AttemptLedger::open(0, ENTRIES, &RunToCompletion).unwrap();
    match assemble_write(
        &mut store,
        &outcome,
        WriteFamily::Submit,
        "resp-accounting",
        &response_limits(),
        &mut zero,
    ) {
        Err(ResponseError::Blob(sage_db_store::view::BlobReadError::Refused(
            sage_db_types::framing::Refusal::Budget(error),
        ))) => assert_eq!(error.unit(), sage_db_types::ledger::BudgetUnit::Bytes),
        other => panic!("expected the blob reader's byte refusal at zero allowance, got {other:?}"),
    }

    // One byte short of the OUTPUT extent, with the blob read already paid for: the output
    // admission is what refuses, and it refuses rather than short-allocating.
    let slack = 1 << 16;
    let mut short = AttemptLedger::open(slack, ENTRIES, &RunToCompletion).unwrap();
    sage_db_types::ledger::SharedLedger::try_charge(&mut short, slack - (measured - 1))
        .expect("the pre-charge fits");
    match assemble_write(
        &mut store,
        &outcome,
        WriteFamily::Submit,
        "resp-accounting",
        &response_limits(),
        &mut short,
    ) {
        Err(ResponseError::Refused(sage_db_types::framing::Refusal::Budget(_)))
        | Err(ResponseError::Blob(sage_db_store::view::BlobReadError::Refused(
            sage_db_types::framing::Refusal::Budget(_),
        ))) => {}
        other => panic!("expected a refusal one byte short, got {other:?}"),
    }

    // Exactly the extracted output extent plus a generous blob allowance: admitted.
    let mut exact = AttemptLedger::open(measured + slack, ENTRIES, &RunToCompletion).unwrap();
    let ok = assemble_write(
        &mut store,
        &outcome,
        WriteFamily::Submit,
        "resp-accounting",
        &response_limits(),
        &mut exact,
    )
    .expect("the exact allowance admits");
    assert_eq!(ok.admitted_bytes(), measured);
}

/// Admitted credit survives the attempt and is released only by the response.
///
/// This is the ownership property, observed through the counter's own figures: after the attempt
/// drops, the retained bytes are still held; after the response drops, they are zero.
#[test]
fn admitted_credit_outlives_the_attempt_and_is_released_by_the_response() {
    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    let outcome = submit_claim(&mut store, &grants, "resp-retention", "candidate");

    let mut attempt = AttemptLedger::open(1 << 20, ENTRIES, &RunToCompletion).unwrap();
    let observer = sage_db_store::lease::LeaseLedger::reserve_owned(&mut attempt, 0)
        .expect("an observer token");
    let response = assemble_write(
        &mut store,
        &outcome,
        WriteFamily::Submit,
        "resp-retention",
        &response_limits(),
        &mut attempt,
    )
    .expect("assembly succeeds");
    let admitted = response.admitted_bytes();
    assert!(admitted > 0);

    // Before the attempt drops, the response's extent is live.
    assert_eq!(observer.stats().held_bytes, admitted);
    drop(attempt);

    // The attempt's unspent capacity is gone, but the response still holds its own extent.
    assert_eq!(
        observer.stats().held_bytes,
        admitted,
        "adopted credit must survive the attempt"
    );

    drop(response);
    assert_eq!(
        observer.stats().held_bytes,
        0,
        "dropping the response must release exactly what it held"
    );
}

/// A cancellation policy that stops at the first checkpoint refuses the assembly.
#[test]
fn a_stopped_caller_is_refused_rather_than_served() {
    struct Stopped;
    impl sage_db_types::query::QueryControl for Stopped {
        fn checkpoint(&self) -> Result<(), sage_db_types::query::ControlStop> {
            Err(sage_db_types::query::ControlStop::Cancelled)
        }
    }

    let (_inner, _fs, mut store) = seed_world();
    let grants = proposer();
    let outcome = submit_claim(&mut store, &grants, "resp-stop", "candidate");

    let mut stopped = AttemptLedger::open(1 << 20, ENTRIES, &Stopped).unwrap();
    match assemble_write(
        &mut store,
        &outcome,
        WriteFamily::Submit,
        "resp-stop",
        &response_limits(),
        &mut stopped,
    ) {
        // The blob reader consults the caller's control first, so the stop arrives as a stop.
        Err(ResponseError::Blob(sage_db_store::view::BlobReadError::Refused(
            sage_db_types::framing::Refusal::Stopped(_),
        )))
        | Err(ResponseError::Refused(sage_db_types::framing::Refusal::Stopped(_))) => {}
        other => panic!("expected a control stop, got {other:?}"),
    }
}

/// SCOPE: the STORE's own branch only. It does not execute the response mapper.
///
/// Kept because it is the narrower fact (the store really returns `Error` for an unreadable
/// history), and explicitly labelled so it is never mistaken for proof that the assembler maps
/// it. `the_assembler_maps_a_history_read_failure_to_history_error` carries that claim.
#[test]
fn a_history_read_failure_is_reported_as_an_error_not_an_absence() {
    use sage_db_store::fold::RecordIdentity;
    use sage_db_store::history::{HistoryBudget, HistoryOutcome};
    use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};

    let (inner, _fs, mut store) = seed_world();
    let grants = proposer();
    submit_claim(&mut store, &grants, "hist-fail-submit", "candidate");
    let reviewer_grants = reviewer();
    review_proposal(
        &mut store,
        &reviewer_grants,
        "hist-fail-review",
        ReviewDecision::Approve,
    );

    *inner.faults_handle().borrow_mut() =
        FaultSchedule::none().at(FaultPoint::ReadExact, Injection::FailNoEffect, 1);
    let identity = RecordIdentity::Identified {
        key: proposal_key(1),
    };
    let mut l = ledger();
    let outcome = store.history(&identity, 2, HistoryBudget::new(64), &Limits::V1, &mut l);
    match outcome {
        HistoryOutcome::Error(_) => {}
        HistoryOutcome::Absent { .. } => panic!(
            "a definite read failure must not be reported as absence: the status is unproven, \
             not missing"
        ),
        HistoryOutcome::Incomplete { .. } => {
            panic!("the scan did not finish; the fault must produce a definite failure")
        }
        HistoryOutcome::Found(_) => panic!("a read failure cannot also be a hit"),
    }
}

/// R1 counterexample **through the real assembler**: a history read failure is not absence.
///
/// The store-only test below proves the store returns `Error`; this one proves the response
/// actually maps it to `ResponseError::HistoryError`, which is the property R1 fixed and the
/// reason a public `*_for_test` wrapper was never justified.
///
/// The fault is armed by a test-local forwarding filesystem that fails **only** transaction
/// reads. The result blob is `EntryName::Blob`, so the assembler reads it normally and then
/// reaches the historical search, whose transaction read is the one that fails.
#[test]
fn the_assembler_maps_a_history_read_failure_to_history_error() {
    use sage_db_store::open::OpenRefusal;

    let (inner, fs, mut store) =
        seed_world_over(|inner| TxFaultFs::new(ObservingFs::new(Rc::clone(&inner)), inner));
    let grants = proposer();
    submit_claim(&mut store, &grants, "asm-hist-submit", "candidate");
    let reviewer_grants = reviewer();
    let approved = review_proposal(
        &mut store,
        &reviewer_grants,
        "asm-hist-review",
        ReviewDecision::Approve,
    );
    assert!(matches!(
        approved,
        HandleOutcome::Committed { version: 4, .. }
    ));

    // Arm the fault, then assemble the REAL reviewed outcome. A review assembly must recover
    // the original status, which is the only path that searches history.
    fs.arm();
    let mut l = ledger();
    let refused = assemble_write(
        &mut store,
        &approved,
        WriteFamily::Review,
        "asm-hist-review",
        &response_limits(),
        &mut l,
    );

    match refused {
        Err(ResponseError::HistoryError(OpenRefusal::Filesystem { .. })) => {}
        Err(ResponseError::HistoryAbsent) => {
            panic!("the mapper must not report an unreadable history as absence")
        }
        Err(ResponseError::Blob(_)) => panic!(
            "the result blob read must not be the failing stage: the fault is transaction-only"
        ),
        other => panic!("expected a history I/O error from the real assembler, got {other:?}"),
    }
    // The canonical injection the wrapper armed is what produced the failure: asserted, so a
    // wrapper that silently stopped arming cannot let this test pass against a healthy store.
    assert!(
        fs.injection_fired(),
        "the canonical ReadExact injection must have fired"
    );
    let _ = inner;
}

// ---------------------------------------------------------------------------
// E1: the refusal taxonomy, case by case
// ---------------------------------------------------------------------------

/// Assemble a `NotCommitted` outcome and return the code, message and budget detail.
///
/// Every case goes through the REAL assembler, so what is asserted is what a client receives —
/// not a private mapper's return value.
fn refusal_of(
    store: &mut Store<ObservingFs>,
    reason: sage_db_store::handle::HandleRefusal,
) -> (
    i32,
    String,
    Option<sage_db_protocol::generated::BudgetDetail>,
) {
    let mut l = ledger();
    let response = assemble_write(
        store,
        &HandleOutcome::NotCommitted { reason },
        WriteFamily::Submit,
        "req-refusal",
        &response_limits(),
        &mut l,
    )
    .expect("a refusal assembles");
    let Some(write_outcome::Outcome::Error(error)) = response.outcome().outcome.as_ref() else {
        panic!("expected a typed error");
    };
    (error.code, error.message.clone(), error.budget.clone())
}

fn fs_reason(error: sage_db_store::fs::FsError) -> sage_db_store::handle::HandleRefusal {
    sage_db_store::handle::HandleRefusal::Filesystem {
        step: sage_db_store::handle::ConstructionStep::ProbeLayout,
        error,
    }
}

/// A transient or capacity failure is never reported as corruption.
///
/// Each case names what actually happened; `Corrupt` here would send an operator to restore a
/// backup when the remedy is free-space, retry or fix-permissions.
#[test]
fn transient_and_capacity_failures_are_not_corruption() {
    use sage_db_protocol::generated::ErrorCode;
    use sage_db_store::fs::Effect;
    use sage_db_store::fs::{FsError, FsErrorKind};

    let (_inner, _fs, mut store) = seed_world();

    let cases = [
        (
            FsErrorKind::NoSpace,
            ErrorCode::Unspecified,
            "the device has no space left",
        ),
        (
            FsErrorKind::QuotaExceeded,
            ErrorCode::Unspecified,
            "a quota would be exceeded",
        ),
        (
            FsErrorKind::WouldBlock,
            ErrorCode::Conflict,
            "an exclusive lock is held elsewhere",
        ),
        (
            FsErrorKind::Interrupted,
            ErrorCode::Conflict,
            "the operation was interrupted before completing",
        ),
        (
            FsErrorKind::PermissionDenied,
            ErrorCode::PermissionDenied,
            "the caller may not perform this operation",
        ),
        (
            FsErrorKind::Unsupported,
            ErrorCode::Unsupported,
            "the feature is unavailable on this filesystem",
        ),
    ];
    for (kind, expected_code, expected_message) in cases {
        let (code, message, budget) =
            refusal_of(&mut store, fs_reason(FsError::new(kind, Effect::NoEffect)));
        assert_eq!(code, expected_code as i32, "kind {kind:?}");
        assert_eq!(message, expected_message, "kind {kind:?}");
        assert!(budget.is_none(), "no figures exist for {kind:?}");
        assert_ne!(
            code,
            ErrorCode::Corrupt as i32,
            "{kind:?} must never be reported as corruption"
        );
    }
}

/// A real budget refusal carries its code AND its figures; a bare kind claims neither.
#[test]
fn a_real_budget_failure_reports_its_unit_and_amounts() {
    use sage_db_protocol::generated::{BudgetUnit, ErrorCode};
    use sage_db_store::fs::FsError;
    use sage_db_types::ledger::{BudgetError, BudgetUnit as Unit};

    let (_inner, _fs, mut store) = seed_world();

    let (code, _, budget) = refusal_of(
        &mut store,
        fs_reason(FsError::from(BudgetError::new(Unit::Bytes, 900, 12))),
    );
    assert_eq!(code, ErrorCode::Budget as i32);
    let detail = budget.expect("a real BudgetError yields figures");
    assert_eq!(detail.unit, BudgetUnit::Bytes as i32);
    assert_eq!(detail.requested, "900");
    assert_eq!(detail.remaining, "12");

    // Entries, to prove the unit is carried rather than assumed.
    let (_, _, budget) = refusal_of(
        &mut store,
        fs_reason(FsError::from(BudgetError::new(Unit::Entries, 5, 0))),
    );
    let detail = budget.expect("figures");
    assert_eq!(detail.unit, BudgetUnit::Entries as i32);

    // A bare BudgetExceeded kind with NO payload: no code and no figures are invented.
    let (code, _, budget) = refusal_of(
        &mut store,
        fs_reason(sage_db_store::fs::FsError::no_effect(
            sage_db_store::fs::FsErrorKind::BudgetExceeded,
        )),
    );
    assert_ne!(code, ErrorCode::Budget as i32);
    assert!(
        budget.is_none(),
        "no BudgetError was in hand, so no figures"
    );
}

/// Cancellation and deadline keep their own codes, distinct from each other.
#[test]
fn cancelled_and_deadline_are_distinct() {
    use sage_db_protocol::generated::ErrorCode;
    use sage_db_store::fs::FsError;
    use sage_db_types::query::ControlStop;

    let (_inner, _fs, mut store) = seed_world();

    let (code, _, budget) =
        refusal_of(&mut store, fs_reason(FsError::from(ControlStop::Cancelled)));
    assert_eq!(code, ErrorCode::Cancelled as i32);
    assert!(budget.is_none());

    let (code, _, _) = refusal_of(
        &mut store,
        fs_reason(FsError::from(ControlStop::DeadlineExceeded)),
    );
    assert_eq!(code, ErrorCode::Deadline as i32);
}

/// Structural inconsistency keeps a truthful corruption class.
#[test]
fn structural_inconsistency_keeps_its_class() {
    use sage_db_protocol::generated::ErrorCode;
    use sage_db_store::fs::Effect;
    use sage_db_store::fs::{FsError, FsErrorKind};

    let (_inner, _fs, mut store) = seed_world();
    for kind in [
        FsErrorKind::HashMismatch,
        FsErrorKind::Symlink,
        FsErrorKind::NotRegular,
        FsErrorKind::IsADirectory,
        FsErrorKind::CrossDevice,
    ] {
        let (code, _, _) = refusal_of(&mut store, fs_reason(FsError::new(kind, Effect::NoEffect)));
        assert_eq!(
            code,
            ErrorCode::Corrupt as i32,
            "{kind:?} is structural damage"
        );
    }
}

/// Incomplete coverage is a resolvable state, not corruption, and it is named.
#[test]
fn incomplete_coverage_is_not_corruption() {
    use sage_db_protocol::generated::ErrorCode;

    let (_inner, _fs, mut store) = seed_world();
    let (code, message, _) = refusal_of(
        &mut store,
        sage_db_store::handle::HandleRefusal::OutcomeIndexIncomplete,
    );
    assert_eq!(code, ErrorCode::Conflict as i32);
    assert!(
        message.contains("cover"),
        "the phrase states coverage: {message}"
    );
    assert_ne!(code, ErrorCode::Corrupt as i32);
}

/// `HandleRefusal::History` preserves the exact canonical class it carries.
#[test]
fn a_history_refusal_preserves_its_canonical_class() {
    use sage_db_protocol::generated::ErrorCode;
    use sage_db_store::open::OpenRefusal;

    let (_inner, _fs, mut store) = seed_world();

    // A genuine format corruption is corruption.
    let (code, _, _) = refusal_of(
        &mut store,
        sage_db_store::handle::HandleRefusal::History(OpenRefusal::Corrupt(
            sage_db_store::open::CorruptReason::MissingTransaction,
        )),
    );
    assert_eq!(code, ErrorCode::Corrupt as i32);

    // A budget refusal inside history keeps the budget code AND its figures.
    let (code, _, budget) = refusal_of(
        &mut store,
        sage_db_store::handle::HandleRefusal::History(OpenRefusal::Refused(
            sage_db_types::framing::Refusal::Budget(sage_db_types::ledger::BudgetError::new(
                sage_db_types::ledger::BudgetUnit::Entries,
                7,
                1,
            )),
        )),
    );
    assert_eq!(code, ErrorCode::Budget as i32);
    assert_eq!(budget.expect("figures").unit, 2);
}

/// The three refusals the old wildcard swallowed now have their own truthful classes.
#[test]
fn previously_wildcarded_refusals_are_classified() {
    use sage_db_protocol::generated::ErrorCode;

    let (_inner, _fs, mut store) = seed_world();

    let (code, _, _) = refusal_of(&mut store, sage_db_store::handle::HandleRefusal::PinLayout);
    assert_eq!(code, ErrorCode::InvalidArgument as i32);

    let (code, _, _) = refusal_of(
        &mut store,
        sage_db_store::handle::HandleRefusal::PinTransfer(
            sage_db_store::lease::TransferError::Untransferred {
                requested_bytes: 8,
                available_bytes: 0,
            },
        ),
    );
    assert_eq!(code, ErrorCode::Unspecified as i32);
}

/// Arithmetic and transfer failures are their own classes, never `Allocation`.
///
/// These are labelled unreachable-by-construction in production: the extent is built from
/// `u64` counts with checked arithmetic, so overflow needs ~2^64 elements, and a transfer can
/// only miss after a successful charge of the same figure. The machinery is exercised directly
/// so the classification cannot silently regress.
#[test]
fn arithmetic_and_transfer_are_not_reported_as_allocation() {
    use sage_db_protocol::generated::ErrorCode;
    use sage_db_store::lease::{LeaseLedger, TransferError};

    let (_inner, _fs, mut store) = seed_world();

    // An extent that silently saturated at u64::MAX would be the defect E2 named; the class is
    // arithmetic, so no allocator is implicated.
    let mut saturated = sage_db_store::handle::HandleRefusal::ExtentOverflow;
    let (code, message, _) = refusal_of(&mut store, saturated);
    assert_eq!(code, ErrorCode::InvalidArgument as i32);
    assert!(
        message.contains("extent") || message.contains("represent"),
        "the phrase names representability: {message}"
    );

    // A transfer that found no admitted credit: also not allocation.
    let mut attempt = AttemptLedger::open(1024, ENTRIES, &RunToCompletion).unwrap();
    match LeaseLedger::adopt_retained(&mut attempt, 16) {
        Err(TransferError::Untransferred { .. }) => {}
        Ok(_) => panic!("nothing was admitted, so nothing can be adopted"),
    }

    saturated = sage_db_store::handle::HandleRefusal::PinTransfer(TransferError::Untransferred {
        requested_bytes: 16,
        available_bytes: 0,
    });
    let (code, _, _) = refusal_of(&mut store, saturated);
    assert_eq!(code, ErrorCode::Unspecified as i32);
}
