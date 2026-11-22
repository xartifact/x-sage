//! Ordinary Term creation through the canonical Store and the model filesystem.

#[allow(dead_code)]
#[path = "support/p16_fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "support/observing_fs.rs"]
mod observing_fs;

use std::{path::Path, rc::Rc};

use fixture::{BYTES, ENTRIES, ROOT, RULES, kb};
use observing_fs::ObservingFs;
use sage_db_kernel::{
    auth::{Capability, CapabilitySet, Grants},
    claims::{CommandError, RequestMeta},
    concept_intent::{DECISION_APPROVE, DECISION_REJECT},
    term_create::{CreateError, Draft, review, submit},
};
use sage_db_store::{
    fold::FoldedRecord,
    fs::CreateDir,
    handle::{HandleOutcome, Store},
    init::bootstrap,
    lease::AttemptLedger,
    open::{Opened, open_existing},
    typed,
    view::RecordLookup,
};
use sage_db_store_tests::model::fs_model::{ModelCapabilities, ModelFileSystem};
use sage_db_types::{
    budget::Budget,
    confidence::Confidence,
    framing::Limits,
    identity::RecordRevision,
    layer::SemanticLayer,
    persist::{IdSpace, ProposalRecord, ProposalStatus, RecordKey, RecordKind, TermSource},
    query::RunToCompletion,
};

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap()
}

fn store() -> (ObservingFs, Store<ObservingFs>) {
    store_on_model(Rc::new(ModelFileSystem::new(ModelCapabilities::full())))
}

fn store_on_model(inner: Rc<ModelFileSystem>) -> (ObservingFs, Store<ObservingFs>) {
    let fs = ObservingFs::new(inner);
    let store = bootstrap(
        fs.clone(),
        Path::new(ROOT),
        CreateDir::IfMissing,
        kb(),
        RULES,
        &Limits::V1,
        &mut ledger(),
    )
    .expect("bootstrap");
    (fs, store)
}

fn grants(capability: Capability) -> Grants<'static> {
    let principal = if capability == Capability::Review {
        "reviewer"
    } else {
        "submitter"
    };
    Grants::new(principal, kb(), CapabilitySet::of(&[capability]))
}

fn meta(request_id: &str) -> RequestMeta<'_> {
    let digest = sage_db_format::digest::raw_sha256(request_id.as_bytes());
    RequestMeta {
        request_id,
        tx_id: digest[..16].try_into().unwrap(),
        created_at: 1_700_000_000_000_000,
    }
}

fn key(id: u128) -> RecordKey {
    RecordKey::new(RecordKind::Proposal, id)
}

fn submit_draft(
    store: &mut Store<ObservingFs>,
    capability: Capability,
    request: &str,
    slug: &str,
    layer: SemanticLayer,
) -> Result<HandleOutcome, CreateError> {
    submit(
        store,
        &grants(capability),
        meta(request),
        Draft {
            slug,
            atom: "Summary",
            body: "# Body\n",
            layer,
            domains: &["D".into()],
            confidence: Confidence::Confirmed,
            source: TermSource::Domain,
            triggers: &["T".into()],
            declared_author: "",
            reason: "Reason",
        },
        &[],
        &Limits::V1,
        &mut ledger(),
    )
}

fn decide(
    store: &mut Store<ObservingFs>,
    capability: Capability,
    request: &str,
    proposal: u128,
    decision: u16,
) -> Result<HandleOutcome, CreateError> {
    review(
        store,
        &grants(capability),
        meta(request),
        key(proposal),
        RecordRevision::FIRST,
        decision,
        "Reviewed",
        Budget::DEFAULT,
        &RunToCompletion,
        &Limits::V1,
        &mut ledger(),
    )
}

fn proposal(store: &Store<ObservingFs>, id: u128) -> &ProposalRecord {
    let RecordLookup::Found {
        record: FoldedRecord::Proposal(proposal),
        ..
    } = store.record(key(id), &mut ledger()).unwrap()
    else {
        panic!("expected proposal");
    };
    proposal
}

fn term_missing(store: &Store<ObservingFs>, id: u32) -> bool {
    matches!(
        store.record(RecordKey::term(id), &mut ledger()).unwrap(),
        RecordLookup::Absent { .. }
    )
}

#[test]
fn approval_creates_only_the_reserved_term_and_recovers_both_original_outcomes() {
    let (fs, mut store) = store();
    let submitted = submit_draft(
        &mut store,
        Capability::Propose,
        "submit",
        "new-term",
        SemanticLayer::L2,
    )
    .unwrap();
    assert!(matches!(
        submitted,
        HandleOutcome::Committed { version: 2, .. }
    ));
    let pending = proposal(&store, 1);
    assert_eq!(pending.status, ProposalStatus::Pending);
    assert_eq!(pending.subject, RecordKey::term(1));
    assert!(term_missing(&store, 1));
    let digest = pending.payload_ref;
    let blob = store
        .read_committed_blob(digest, typed::MAX_FRAME_BYTES, &mut ledger())
        .unwrap();
    let typed::RequestOutcome::Ok(frame) = typed::decode_request(blob.as_bytes(), &mut ledger())
    else {
        panic!("canonical frame");
    };
    let [op] = frame.ops.as_slice() else {
        panic!("exactly one operation");
    };
    assert_eq!(op.kind, 12);
    assert_eq!(op.version, 1);
    // Literal LE prefixes, L2, Confirmed 2, Domain 1, one item per list.
    assert_eq!(op.payload, b"\x08\0\0\0new-term\x07\0\0\0Summary\x07\0\0\0# Body\n\x02\x02\0\x01\0\x01\0\0\0\x01\0\0\0D\x01\0\0\0\x01\0\0\0T\0\0\0\0\x06\0\0\0Reason");
    assert!(frame.read_set.is_empty());
    drop(blob);
    let approved = decide(
        &mut store,
        Capability::Review,
        "approve",
        1,
        DECISION_APPROVE,
    )
    .unwrap();
    assert!(matches!(
        approved,
        HandleOutcome::Committed { version: 3, .. }
    ));
    drop(store);
    let Opened::Ready(mut store) =
        open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger()).unwrap()
    else {
        panic!("durable reopen");
    };
    let pending = proposal(&store, 1);
    assert_eq!(pending.status, ProposalStatus::Approved);
    assert_eq!(pending.declared_author, "");
    let RecordLookup::Found {
        record: FoldedRecord::Term(term),
        ..
    } = store.record(RecordKey::term(1), &mut ledger()).unwrap()
    else {
        panic!("committed Term");
    };
    assert_eq!(term.slug, "new-term");
    assert_eq!(term.atom, "Summary");
    assert_eq!(term.common.recorded_by.as_deref(), Some("reviewer"));
    assert_eq!(term.domains, ["D"]);
    assert_eq!(term.triggers, ["T"]);
    let body = store
        .read_committed_blob(term.body_ref, 128, &mut ledger())
        .unwrap();
    assert_eq!(body.as_bytes(), b"# Body\n");
    assert!(matches!(
        submit_draft(
            &mut store,
            Capability::Propose,
            "submit",
            "new-term",
            SemanticLayer::L2
        )
        .unwrap(),
        HandleOutcome::AlreadyCommitted { version: 2, .. }
    ));
    assert!(matches!(
        decide(
            &mut store,
            Capability::Review,
            "approve",
            1,
            DECISION_APPROVE
        )
        .unwrap(),
        HandleOutcome::AlreadyCommitted { version: 3, .. }
    ));
    assert_eq!(store.head(), Some(3));
    assert_eq!(
        store
            .commit_basis(IdSpace::TermId, &mut ledger())
            .unwrap()
            .allocated_high_water,
        1
    );
}

#[test]
fn same_slug_race_keeps_one_active_term_and_rejection_burns_reserved_id() {
    let (_, mut store) = store();
    for request in ["first", "second"] {
        submit_draft(
            &mut store,
            Capability::Propose,
            request,
            "same-slug",
            SemanticLayer::L1,
        )
        .unwrap();
    }
    decide(
        &mut store,
        Capability::Review,
        "second-wins",
        2,
        DECISION_APPROVE,
    )
    .unwrap();
    decide(
        &mut store,
        Capability::Review,
        "first-loses",
        1,
        DECISION_APPROVE,
    )
    .unwrap();
    assert_eq!(proposal(&store, 1).status, ProposalStatus::Conflicted);
    assert_eq!(proposal(&store, 2).status, ProposalStatus::Approved);
    assert!(term_missing(&store, 1));
    assert!(!term_missing(&store, 2));
    assert!(matches!(
        submit_draft(
            &mut store,
            Capability::Propose,
            "occupied",
            "same-slug",
            SemanticLayer::L1
        ),
        Err(CreateError::SlugExists)
    ));
    submit_draft(
        &mut store,
        Capability::Propose,
        "third",
        "other-term",
        SemanticLayer::L1,
    )
    .unwrap();
    decide(&mut store, Capability::Review, "reject", 3, DECISION_REJECT).unwrap();
    assert!(term_missing(&store, 3));
    assert_eq!(proposal(&store, 3).status, ProposalStatus::Rejected);
    submit_draft(
        &mut store,
        Capability::Propose,
        "fourth",
        "other-term",
        SemanticLayer::L1,
    )
    .unwrap();
    assert_eq!(proposal(&store, 4).subject, RecordKey::term(4));
    decide(
        &mut store,
        Capability::Review,
        "fourth-wins",
        4,
        DECISION_APPROVE,
    )
    .unwrap();
    assert!(!term_missing(&store, 4));
}

#[test]
fn graph_violation_and_wrong_capability_never_create_a_term() {
    let (_, mut store) = store();
    assert!(matches!(
        submit_draft(
            &mut store,
            Capability::Review,
            "bad-author",
            "l3-item",
            SemanticLayer::L3
        ),
        Err(CreateError::Command(CommandError::Auth(_)))
    ));
    submit_draft(
        &mut store,
        Capability::Propose,
        "l3-submit",
        "l3-item",
        SemanticLayer::L3,
    )
    .unwrap();
    assert!(matches!(
        decide(
            &mut store,
            Capability::Propose,
            "bad-reviewer",
            1,
            DECISION_APPROVE
        ),
        Err(CreateError::Command(CommandError::Auth(_)))
    ));
    assert!(matches!(
        decide(&mut store, Capability::Review, "l3-review", 1, DECISION_APPROVE),
        Err(CreateError::InvalidCandidate(violations)) if violations.iter().any(|entry| entry.code()=="V5")
    ));
    assert_eq!(proposal(&store, 1).status, ProposalStatus::Pending);
    assert!(term_missing(&store, 1));
    assert_eq!(store.head(), Some(2));
}

fn direct(
    store: &mut Store<ObservingFs>,
    grant: &Grants<'_>,
    request: &str,
    slug: &str,
    layer: SemanticLayer,
    initial_relations: &[sage_db_kernel::term_create::InitialRelation],
    read_set: &[sage_db_types::persist::RevPair],
) -> Result<HandleOutcome, CreateError> {
    use sage_db_kernel::term_create::{DirectCreate, create_direct};

    create_direct(
        store,
        grant,
        meta(request),
        DirectCreate {
            draft: Draft {
                slug,
                atom: "Summary",
                body: "# Body\n",
                layer,
                domains: &["D".into()],
                confidence: Confidence::Confirmed,
                source: TermSource::Domain,
                triggers: &["T".into()],
                declared_author: "",
                reason: "Concept created by service",
            },
            initial_relations,
            read_set,
            expect_version: None,
        },
        Budget::DEFAULT,
        &RunToCompletion,
        &Limits::V1,
        &mut ledger(),
    )
}

fn initial_relation(
    relation: sage_db_types::relation::RelationId,
) -> sage_db_kernel::term_create::InitialRelation {
    sage_db_kernel::term_create::InitialRelation {
        target: RecordKey::term(1),
        relation,
        confidence: Confidence::Confirmed,
    }
}

fn baseline(revision: RecordRevision) -> sage_db_types::persist::RevPair {
    sage_db_types::persist::RevPair {
        key: RecordKey::term(1),
        revision,
    }
}

#[test]
fn direct_creation_publishes_term_assertions_marks_and_one_reopenable_result() {
    use sage_db_store::commit::RecordedOutcome;
    use sage_db_types::{
        persist::{RevPair, Watermark},
        relation::RelationId,
    };

    let (fs, mut store) = store();
    let writer = grants(Capability::WriteConcept);
    assert!(matches!(
        direct(
            &mut store,
            &writer,
            "seed",
            "agent",
            SemanticLayer::L0,
            &[],
            &[]
        )
        .unwrap(),
        HandleOutcome::Committed { version: 2, .. }
    ));
    let relations = [
        initial_relation(RelationId::DependsOn),
        initial_relation(RelationId::Enables),
    ];
    let read_set = [baseline(RecordRevision::FIRST)];
    let HandleOutcome::Committed {
        version: 3,
        recorded: RecordedOutcome::Outcome(committed),
    } = direct(
        &mut store,
        &writer,
        "create",
        "audit-create",
        SemanticLayer::L1,
        &relations,
        &read_set,
    )
    .unwrap()
    else {
        panic!("the whole operation commits in one version");
    };
    assert_eq!(store.head(), Some(3));
    drop(store);
    let Opened::Ready(mut store) =
        open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger()).unwrap()
    else {
        panic!("durable reopen");
    };
    let RecordLookup::Found {
        record: FoldedRecord::Term(term),
        ..
    } = store.record(RecordKey::term(2), &mut ledger()).unwrap()
    else {
        panic!("committed Term");
    };
    assert_eq!(term.slug, "audit-create");
    assert_eq!(term.common.recorded_by.as_deref(), Some("submitter"));
    let term_key = term.key;
    let body_ref = term.body_ref;
    assert_eq!(
        store
            .read_committed_blob(body_ref, typed::MAX_FRAME_BYTES, &mut ledger())
            .unwrap()
            .as_bytes(),
        b"# Body\n"
    );
    let mut affected = vec![RevPair {
        key: term_key,
        revision: RecordRevision::FIRST,
    }];
    for (index, initial) in relations.iter().enumerate() {
        let key = RecordKey::new(RecordKind::Assertion, (index + 1) as u128);
        let RecordLookup::Found {
            record: FoldedRecord::Assertion(assertion),
            ..
        } = store.record(key, &mut ledger()).unwrap()
        else {
            panic!("initial assertion");
        };
        assert_eq!(assertion.from, term_key);
        assert_eq!(assertion.target, initial.target);
        assert_eq!(assertion.relation, initial.relation);
        assert_eq!(assertion.confidence, initial.confidence);
        assert_eq!(assertion.declaration_order as usize, index);
        affected.push(RevPair {
            key,
            revision: RecordRevision::FIRST,
        });
    }
    let marks = [
        Watermark {
            space: IdSpace::TermId,
            hwm: 2,
        },
        Watermark {
            space: IdSpace::AssertionId,
            hwm: 2,
        },
    ];
    for mark in &marks {
        assert_eq!(
            store
                .commit_basis(mark.space, &mut ledger())
                .unwrap()
                .allocated_high_water,
            mark.hwm
        );
    }
    assert_eq!(
        store
            .read_committed_blob(
                committed.event().result_ref,
                typed::MAX_FRAME_BYTES,
                &mut ledger()
            )
            .unwrap()
            .as_bytes(),
        typed::result_payload(3, &affected, &marks, &mut ledger()).unwrap(),
    );
    assert_eq!(
        direct(
            &mut store,
            &writer,
            "create",
            "audit-create",
            SemanticLayer::L1,
            &relations,
            &read_set
        )
        .unwrap(),
        HandleOutcome::AlreadyCommitted {
            version: 3,
            recorded: RecordedOutcome::Outcome(committed)
        },
    );
    assert!(matches!(
        direct(
            &mut store,
            &writer,
            "create",
            "audit-create",
            SemanticLayer::L1,
            &relations[..1],
            &read_set
        ),
        Err(CreateError::Command(CommandError::IdempotencyMismatch)),
    ));
    assert!(matches!(
        direct(
            &mut store,
            &writer,
            "novel",
            "audit-create",
            SemanticLayer::L1,
            &relations,
            &read_set
        ),
        Err(CreateError::SlugExists),
    ));
    assert_eq!(store.head(), Some(3));
}

#[test]
fn direct_creation_refusals_leave_no_term_assertion_or_outcome() {
    use sage_db_kernel::{auth::AuthError, claims::InputError};
    use sage_db_store::open::OutcomeLookup;
    use sage_db_types::relation::RelationId;

    let (_, mut store) = store();
    let writer = grants(Capability::WriteConcept);
    direct(
        &mut store,
        &writer,
        "seed",
        "agent",
        SemanticLayer::L0,
        &[],
        &[],
    )
    .unwrap();
    let rels = [initial_relation(RelationId::IsA)];
    let stale = [baseline(RecordRevision::FIRST.checked_next().unwrap())];
    let read_set = [baseline(RecordRevision::FIRST)];
    let normal = Grants::new(
        "normal",
        kb(),
        CapabilitySet::of(&[
            Capability::Read,
            Capability::Propose,
            Capability::Review,
            Capability::WriteAux,
            Capability::Admin,
        ]),
    );
    assert!(matches!(
        direct(
            &mut store,
            &normal,
            "auth",
            "refused",
            SemanticLayer::L0,
            &rels,
            &read_set
        ),
        Err(CreateError::Command(CommandError::Auth(
            AuthError::MissingCapability {
                capability: Capability::WriteConcept,
            }
        ))),
    ));
    assert!(matches!(
        direct(
            &mut store,
            &writer,
            "baseline",
            "refused",
            SemanticLayer::L0,
            &rels,
            &stale
        ),
        Err(CreateError::Command(CommandError::Input(
            InputError::StaleBaseline { .. }
        ))),
    ));
    assert!(matches!(
        direct(&mut store, &writer, "graph", "refused", SemanticLayer::L0, &rels, &read_set),
        Err(CreateError::InvalidCandidate(violations)) if violations.iter().any(|v| v.code() == "V3"),
    ));
    assert!(matches!(
        direct(
            &mut store,
            &writer,
            "missing-baseline",
            "refused",
            SemanticLayer::L1,
            &rels,
            &[]
        ),
        Err(CreateError::Field("read_set")),
    ));
    assert_eq!(store.head(), Some(2));
    assert!(term_missing(&store, 2));
    assert!(matches!(
        store
            .record(RecordKey::new(RecordKind::Assertion, 1), &mut ledger())
            .unwrap(),
        RecordLookup::Absent { .. },
    ));
    for request in ["baseline", "graph", "missing-baseline"] {
        assert!(matches!(
            store.outcome("submitter", request),
            OutcomeLookup::Absent { .. }
        ));
    }
    assert_eq!(
        store
            .commit_basis(IdSpace::TermId, &mut ledger())
            .unwrap()
            .allocated_high_water,
        1
    );
    assert_eq!(
        store
            .commit_basis(IdSpace::AssertionId, &mut ledger())
            .unwrap()
            .allocated_high_water,
        0
    );
}

#[test]
fn direct_creation_unknown_current_never_reopens_as_a_partial_creation() {
    use sage_db_store::open::OutcomeLookup;
    use sage_db_store_tests::faults::{FaultPoint, FaultSchedule, Injection};
    use sage_db_types::relation::RelationId;

    let model = Rc::new(ModelFileSystem::new(ModelCapabilities::full()));
    let (_, mut store) = store_on_model(Rc::clone(&model));
    let writer = grants(Capability::WriteConcept);
    direct(
        &mut store,
        &writer,
        "seed",
        "agent",
        SemanticLayer::L0,
        &[],
        &[],
    )
    .unwrap();

    *model.faults_handle().borrow_mut() = FaultSchedule::none().at(
        FaultPoint::ReplaceCurrent,
        Injection::FailUnknownAfterEffect,
        1,
    );
    assert_eq!(
        direct(
            &mut store,
            &writer,
            "unknown",
            "audit-create",
            SemanticLayer::L1,
            &[
                initial_relation(RelationId::DependsOn),
                initial_relation(RelationId::Enables)
            ],
            &[baseline(RecordRevision::FIRST)],
        )
        .unwrap(),
        HandleOutcome::Unknown,
    );
    assert!(model.faults_handle().borrow().all_fired());
    let worlds = model.model().crash_states();
    assert_eq!(worlds.sampled, worlds.states.len());
    drop(store);

    let mut complete = false;
    let mut absent = false;
    for state in worlds.states {
        let fs =
            ModelFileSystem::from_durable_state(ModelCapabilities::full(), Path::new(ROOT), state)
                .expect("restore each sampled world");
        let Opened::Ready(store) = open_existing(
            ObservingFs::new(Rc::new(fs)),
            Path::new(ROOT),
            Some(kb()),
            &Limits::V1,
            &mut ledger(),
        )
        .expect("reopen world") else {
            panic!("each world replays Ready");
        };
        let committed = match store.head() {
            Some(2) => {
                absent = true;
                false
            }
            Some(3) => {
                complete = true;
                true
            }
            other => panic!("unexpected head {other:?}"),
        };
        assert!(!term_missing(&store, 1), "acknowledged target survives");
        assert_eq!(!term_missing(&store, 2), committed);
        for id in [1, 2] {
            assert_eq!(
                matches!(
                    store
                        .record(RecordKey::new(RecordKind::Assertion, id), &mut ledger())
                        .unwrap(),
                    RecordLookup::Found {
                        record: FoldedRecord::Assertion(_),
                        ..
                    },
                ),
                committed,
            );
        }
        match store.outcome("submitter", "unknown") {
            OutcomeLookup::Found {
                original_version: 3,
                ..
            } => assert!(committed),
            OutcomeLookup::Absent { .. } => assert!(!committed),
            other => panic!("unexpected outcome {other:?}"),
        }
        assert_eq!(
            store
                .commit_basis(IdSpace::TermId, &mut ledger())
                .unwrap()
                .allocated_high_water,
            if committed { 2 } else { 1 },
        );
        assert_eq!(
            store
                .commit_basis(IdSpace::AssertionId, &mut ledger())
                .unwrap()
                .allocated_high_water,
            if committed { 2 } else { 0 },
        );
    }
    assert!(complete && absent, "fault exposes both coherent states");
}

#[test]
fn private_direct_create_uses_generated_ingress_and_an_explicit_dedicated_grant() {
    use sage_db_kernel::auth::AuthError;
    use sage_db_protocol::{ingress, validation::ValidationLimits};
    use sage_db_server::{
        commands::{
            CommandContext, CommandRefusal, ExecutionMeta, WriteCommand, commit_with_validation,
        },
        credentials::{CredentialBinding, CredentialVerifier, TokenHash, VerifierLimits},
        responses::{ResponseLimits, WriteFamily, assemble_write},
    };
    use sage_db_store::handle::HandleRefusal;

    let (_, mut store) = store();
    let writer = grants(Capability::WriteConcept);
    direct(
        &mut store,
        &writer,
        "seed",
        "agent",
        SemanticLayer::L0,
        &[],
        &[],
    )
    .unwrap();
    let hash = |token: &[u8]| TokenHash::from_sha256(sage_db_format::digest::raw_sha256(token));
    let writer_token = b"isolated-concept-writer-proof-token";
    let existing_token = b"isolated-existing-grants-proof-token";
    let bindings = [
        CredentialBinding::active(
            hash(writer_token),
            "bun-concept",
            kb(),
            CapabilitySet::of(&[Capability::WriteConcept]),
        ),
        CredentialBinding::active(
            hash(existing_token),
            "existing-grants",
            kb(),
            CapabilitySet::of(&[
                Capability::Read,
                Capability::Propose,
                Capability::Review,
                Capability::WriteAux,
                Capability::Admin,
            ]),
        ),
    ];
    let verifier =
        CredentialVerifier::new(&bindings, VerifierLimits::default(), &mut ledger()).unwrap();
    let payload = |request, expected, slug| {
        format!(
            r##"{{
            "envelope": {{
                "protocolVersion": 1, "kbUuid": "{}", "requestId": "{request}",
                "expectVersion": "{expected}",
                "readSet": [{{"key": {{"kind": 1, "id": "1"}}, "revision": "1"}}]
            }},
            "payload": {{"directConceptCreate": {{
                "term": {{
                    "slug": "{slug}", "atom": "Verify direct write", "body": "# Audit\n",
                    "layer": 1, "domains": ["delivery"], "confidence": 2, "source": 0,
                    "triggers": ["audit"], "reason": "Created through POST /api/concepts"
                }},
                "assertions": [{{"target": {{"kind": 1, "id": "1"}}, "relation": 3, "confidence": 2}}]
            }}}}
        }}"##,
            kb()
        )
    };
    let send = |store: &mut Store<ObservingFs>, body: String, token: &[u8]| {
        let mut ledger = ledger();
        let decoded = ingress::decode_commit_command_request(
            body.as_bytes(),
            ingress::IngressLimits::new(8192, 8192),
            &mut ledger,
        )
        .unwrap();
        let request = decoded.dto();
        let envelope = request.envelope.as_ref().unwrap();
        let RequestMeta {
            tx_id, created_at, ..
        } = meta(&envelope.request_id);
        let mut context = CommandContext {
            store,
            verifier: &verifier,
            bearer: token,
            execution: ExecutionMeta { tx_id, created_at },
            limits: Limits::V1,
            validation: ValidationLimits::DEFAULT,
            max_existing_body_len: 8192,
        };
        commit_with_validation(
            &mut context,
            envelope,
            request.payload.as_ref().unwrap(),
            &mut ledger,
            Budget::DEFAULT,
            &RunToCompletion,
        )
    };
    assert!(matches!(
        send(
            &mut store,
            payload("create", 2, "audit-create"),
            existing_token
        ),
        Err(CommandRefusal::DirectConcept(CreateError::Command(
            CommandError::Auth(AuthError::MissingCapability {
                capability: Capability::WriteConcept
            })
        ))),
    ));
    let committed = send(
        &mut store,
        payload("create", 2, "audit-create"),
        writer_token,
    )
    .unwrap();
    assert_eq!(committed.command, WriteCommand::DirectConceptCreate);
    assert!(matches!(
        committed.outcome,
        HandleOutcome::Committed { version: 3, .. }
    ));
    let _response = assemble_write(
        &mut store,
        &committed.outcome,
        WriteFamily::Commit,
        "create",
        &ResponseLimits {
            max_result_bytes: 8192,
            limits: Limits::V1,
            max_history_events: 128,
        },
        &mut ledger(),
    )
    .expect("production response assembler verifies the result blob");
    assert!(matches!(
        send(
            &mut store,
            payload("create", 2, "audit-create"),
            writer_token
        )
        .unwrap()
        .outcome,
        HandleOutcome::AlreadyCommitted { version: 3, .. },
    ));
    assert!(matches!(
        send(
            &mut store,
            payload("create", 3, "audit-create"),
            writer_token
        ),
        Err(CommandRefusal::DirectConcept(CreateError::Command(
            CommandError::IdempotencyMismatch
        ))),
    ));
    assert!(matches!(
        send(
            &mut store,
            payload("duplicate", 2, "audit-create"),
            writer_token
        ),
        Err(CommandRefusal::DirectConcept(CreateError::SlugExists)),
    ));
    assert!(matches!(
        send(&mut store, payload("moved", 2, "new-slug"), writer_token),
        Err(CommandRefusal::DirectConcept(CreateError::Command(
            CommandError::Store(HandleRefusal::ExpectVersionMismatch {
                expected: 2,
                found: Some(3)
            })
        ))),
    ));
    assert_eq!(store.head(), Some(3));
}
