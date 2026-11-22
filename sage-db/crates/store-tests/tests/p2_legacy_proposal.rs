//! Publication and reopen of required legacy proposal metadata.

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
    claims::{CommandError, InputError, RequestMeta},
    concept::{ConceptTextError, submit_legacy_concept_text},
    concept_intent::{ConceptTextEdit, MASK_ATOM, MASK_BODY},
    legacy_proposal::{
        LegacyDirectory, LegacyIntent, LegacyProposal, LegacyProposalFile, decode_stored,
        lookup_alias,
    },
    relations::{self, RelationAssert, RelationAssertError, submit_legacy_relation_assert},
    term_create::{DirectCreate, Draft, create_direct},
};
use sage_db_store::{
    fold::FoldedRecord,
    fs::CreateDir,
    handle::{HandleOutcome, HandleRefusal, Store},
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
    persist::{ProposalStatus, RecordKey, RecordKind, RevPair, TermSource},
    query::RunToCompletion,
    relation::RelationId,
};

const EDGE: &str = "2026-11-21-agent-enables-risk.md";
const ATOM: &str = "2026-11-21T20-20-20-123Z-agent-abcd12.md";
const BODY: &str = "2026-11-21T20-20-20-123Z-agent-body-abcd12.json";

fn ledger() -> AttemptLedger<'static> {
    AttemptLedger::open(BYTES, ENTRIES, &RunToCompletion).unwrap()
}

fn grants(capability: Capability) -> Grants<'static> {
    Grants::new("test-principal", kb(), CapabilitySet::of(&[capability]))
}

fn meta(request_id: &str) -> RequestMeta<'_> {
    let digest = sage_db_format::digest::raw_sha256(request_id.as_bytes());
    RequestMeta {
        request_id,
        tx_id: digest[..16].try_into().unwrap(),
        created_at: 1_700_000_000_000_000,
    }
}

fn pair(id: u32) -> RevPair {
    RevPair {
        key: RecordKey::term(id),
        revision: RecordRevision::FIRST,
    }
}

fn seed() -> (ObservingFs, Store<ObservingFs>) {
    let fs = ObservingFs::new(Rc::new(ModelFileSystem::new(ModelCapabilities::full())));
    let mut store = bootstrap(
        fs.clone(),
        Path::new(ROOT),
        CreateDir::IfMissing,
        kb(),
        RULES,
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap();
    for slug in ["agent", "risk"] {
        create_term(&mut store, slug);
    }
    assert_eq!(store.head(), Some(3));
    (fs, store)
}

fn create_term(store: &mut Store<ObservingFs>, slug: &str) {
    create_direct(
        store,
        &grants(Capability::WriteConcept),
        meta(slug),
        DirectCreate {
            draft: Draft {
                slug,
                atom: "Original atom",
                body: "# Original\n",
                layer: SemanticLayer::L0,
                domains: &["Test".into()],
                confidence: Confidence::Confirmed,
                source: TermSource::Personal,
                triggers: &["test".into()],
                declared_author: "",
                reason: "Seed",
            },
            initial_relations: &[],
            read_set: &[],
            expect_version: None,
        },
        Budget::DEFAULT,
        &RunToCompletion,
        &Limits::V1,
        &mut ledger(),
    )
    .unwrap();
}

fn submit_edge(
    store: &mut Store<ObservingFs>,
    request: &str,
    observed_version: u64,
    filename: &str,
    confidence: Confidence,
) -> Result<HandleOutcome, RelationAssertError> {
    let read_set = [1, 2].map(|id| {
        let key = RecordKey::term(id);
        let RecordLookup::Found {
            record: FoldedRecord::Term(term),
            ..
        } = store.record(key, &mut ledger()).unwrap()
        else {
            panic!("fixture target")
        };
        RevPair {
            key,
            revision: term.revision,
        }
    });
    submit_legacy_relation_assert(
        store,
        &grants(Capability::Propose),
        meta(request),
        RelationAssert {
            from: RecordKey::term(1),
            target: RecordKey::term(2),
            relation: RelationId::Enables as u16,
            confidence: confidence.as_u16(),
            declaration_order: 91,
            declared_author: "CLI",
            reason: "(待填写)",
        },
        LegacyProposal {
            file: LegacyProposalFile::Edge { filename },
            observed_version,
        },
        &read_set,
        &Limits::V1,
        &mut ledger(),
    )
}

fn inspect(
    store: &mut Store<ObservingFs>,
    directory: LegacyDirectory,
    filename: &str,
    check: impl FnOnce(LegacyProposal<'_>, LegacyIntent<'_>),
) {
    let found = lookup_alias(
        store,
        &grants(Capability::Read),
        directory,
        filename,
        &mut ledger(),
    )
    .unwrap()
    .expect("exact filename is present");
    let RecordLookup::Found {
        record: FoldedRecord::Proposal(row),
        ..
    } = store.record(found.proposal.key, &mut ledger()).unwrap()
    else {
        panic!("alias must resolve to a proposal")
    };
    let payload_ref = row.payload_ref;
    let payload = store
        .read_committed_blob(payload_ref, typed::MAX_FRAME_BYTES, &mut ledger())
        .unwrap();
    let RecordLookup::Found {
        record: FoldedRecord::Proposal(row),
        basis,
        ..
    } = store.record(found.proposal.key, &mut ledger()).unwrap()
    else {
        panic!("alias must resolve to a proposal")
    };
    assert_eq!(basis, payload.basis());
    assert!(row.legacy_id.is_none() && row.extensions.entries().is_empty());
    let decoded = decode_stored(row, payload.as_bytes(), &mut ledger())
        .unwrap()
        .unwrap();
    check(decoded.compatibility, decoded.intent);
}

#[test]
fn a_same_filename_edge_replaces_projection_and_original_receipt_survives_restart() {
    let (fs, mut store) = seed();
    assert!(matches!(
        submit_edge(&mut store, "first", 3, EDGE, Confidence::Hypothetical).unwrap(),
        HandleOutcome::Committed { version: 4, .. },
    ));
    assert!(matches!(
        submit_edge(&mut store, "second", 4, EDGE, Confidence::Confirmed).unwrap(),
        HandleOutcome::Committed { version: 5, .. },
    ));
    drop(store);
    let Opened::Ready(mut store) =
        open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger()).unwrap()
    else {
        panic!("canonical reopen")
    };
    assert!(matches!(
        submit_edge(&mut store, "first", 3, EDGE, Confidence::Hypothetical).unwrap(),
        HandleOutcome::AlreadyCommitted { version: 4, .. },
    ));
    assert!(matches!(
        submit_edge(
            &mut store,
            "first",
            3,
            "2026-11-22-agent-enables-risk.md",
            Confidence::Hypothetical
        ),
        Err(RelationAssertError::Command(
            CommandError::IdempotencyMismatch
        )),
    ));
    inspect(
        &mut store,
        LegacyDirectory::Edges,
        EDGE,
        |metadata, intent| {
            assert_eq!(
                metadata,
                LegacyProposal {
                    file: LegacyProposalFile::Edge { filename: EDGE },
                    observed_version: 4
                }
            );
            let LegacyIntent::Edge(intent) = intent else {
                panic!("edge")
            };
            assert_eq!(intent.confidence, Confidence::Confirmed);
        },
    );
    let found = lookup_alias(
        &mut store,
        &grants(Capability::Read),
        LegacyDirectory::Edges,
        EDGE,
        &mut ledger(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(found.proposal.key, RecordKey::new(RecordKind::Proposal, 2));
    assert_eq!(found.status, ProposalStatus::Pending);
    let decide = |store: &mut Store<ObservingFs>, request, decision| {
        relations::review_relation_assert(
            store,
            &grants(Capability::Review),
            meta(request),
            found.proposal.key,
            found.proposal.revision,
            decision,
            "Review",
            Budget::DEFAULT,
            &RunToCompletion,
            &Limits::V1,
            &mut ledger(),
        )
    };
    assert!(matches!(
        decide(&mut store, "ordinary-approval", relations::DECISION_APPROVE),
        Err(RelationAssertError::Command(CommandError::Input(
            InputError::StoredExpectedVersion { pinned: 4 }
        ))),
    ));
    assert_eq!(store.head(), Some(5));
    assert!(matches!(
        decide(&mut store, "reject", relations::DECISION_REJECT).unwrap(),
        HandleOutcome::Committed { version: 6, .. },
    ));
    let terminal = lookup_alias(
        &mut store,
        &grants(Capability::Read),
        LegacyDirectory::Edges,
        EDGE,
        &mut ledger(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(terminal.proposal.key, found.proposal.key);
    assert_eq!(terminal.status, ProposalStatus::Rejected);
    assert_eq!(store.head(), Some(6), "re-resolution never writes");
}

#[test]
fn atom_and_body_use_distinct_bases_and_reject_a_moved_publication_without_writing() {
    let (_, mut store) = seed();
    let edit = |field_mask, atom, body| ConceptTextEdit {
        subject: RecordKey::term(1),
        field_mask,
        atom,
        body,
        declared_author: "CLI",
    };
    let atom_edit = edit(MASK_ATOM, Some("New atom"), None);
    let body_edit = edit(MASK_BODY, None, Some("---\nNew body\n"));
    let submit = |store: &mut Store<ObservingFs>, request, version, file, edit, reason| {
        submit_legacy_concept_text(
            store,
            &grants(Capability::Propose),
            meta(request),
            edit,
            reason,
            LegacyProposal {
                file,
                observed_version: version,
            },
            &[pair(1)],
            &Limits::V1,
            &mut ledger(),
        )
    };
    let bad_atom = LegacyProposalFile::Atom {
        filename: ATOM,
        current_atom: "A different atom",
    };
    assert!(matches!(
        submit(&mut store, "bad", 3, bad_atom, atom_edit, ""),
        Err(ConceptTextError::Command(CommandError::Input(
            InputError::LegacyProposalAtomMismatch
        )))
    ));
    let atom = LegacyProposalFile::Atom {
        filename: ATOM,
        current_atom: "Original atom",
    };
    assert!(matches!(
        submit(&mut store, "atom", 3, atom, atom_edit, "").unwrap(),
        HandleOutcome::Committed { version: 4, .. }
    ));
    let body = LegacyProposalFile::Body {
        filename: BODY,
        expected_revision: [0x5a; 32],
    };
    assert!(matches!(
        submit(&mut store, "stale-body", 3, body, body_edit, "Because"),
        Err(ConceptTextError::Command(CommandError::Store(
            HandleRefusal::ExpectVersionMismatch {
                expected: 3,
                found: Some(4),
            }
        )))
    ));
    assert!(matches!(
        submit(&mut store, "body", 4, body, body_edit, "Because").unwrap(),
        HandleOutcome::Committed { version: 5, .. }
    ));
    inspect(
        &mut store,
        LegacyDirectory::Atoms,
        ATOM,
        |metadata, intent| {
            assert_eq!(metadata.file, atom);
            let LegacyIntent::Text(intent) = intent else {
                panic!("text")
            };
            assert_eq!(intent.atom, Some("New atom"));
            assert_eq!(intent.reason, Some(""));
        },
    );
    inspect(
        &mut store,
        LegacyDirectory::Bodies,
        BODY,
        |metadata, intent| {
            assert_eq!(metadata.file, body);
            let LegacyIntent::Text(intent) = intent else {
                panic!("text")
            };
            assert_eq!(intent.body, Some("---\nNew body\n"));
            assert_eq!(intent.reason, Some("Because"));
        },
    );
    assert!(
        lookup_alias(
            &mut store,
            &grants(Capability::Read),
            LegacyDirectory::Edges,
            ATOM,
            &mut ledger()
        )
        .unwrap()
        .is_none()
    );
    assert!(
        lookup_alias(
            &mut store,
            &grants(Capability::Propose),
            LegacyDirectory::Atoms,
            ATOM,
            &mut ledger()
        )
        .is_err()
    );
    for bad_name in ["../bad.md", "2026-11-21-agent-enables-other.md"] {
        assert!(submit_edge(&mut store, "bad-name", 5, bad_name, Confidence::Confirmed).is_err());
    }
    assert_eq!(store.head(), Some(5));
}

#[test]
fn normal_legacy_merges_advance_only_the_affected_records_and_leave_conflicts_pending() {
    use sage_db_kernel::{
        concept::merge_legacy_concept_text,
        legacy_proposal::{BodyObservation, LegacyReview},
        relations::merge_legacy_relation_assert,
    };
    use sage_db_types::persist::IdSpace;

    let (fs, mut store) = seed();
    let reader = grants(Capability::Read);
    let reviewer = grants(Capability::Review);
    let resolve = |store: &mut Store<ObservingFs>, directory, filename| {
        lookup_alias(store, &reader, directory, filename, &mut ledger())
            .unwrap()
            .unwrap()
    };
    let merge_edge = |store: &mut Store<ObservingFs>, request, pair, version| {
        merge_legacy_relation_assert(
            store,
            &reviewer,
            meta(request),
            pair,
            LegacyReview {
                directory: LegacyDirectory::Edges,
                filename: EDGE,
                observed_version: version,
            },
            "Reviewed",
            Budget::DEFAULT,
            &RunToCompletion,
            &Limits::V1,
            &mut ledger(),
        )
    };
    let merge_text =
        |store: &mut Store<ObservingFs>, request, directory, filename, hash: Option<[u8; 32]>| {
            let resolved = resolve(store, directory, filename);
            merge_legacy_concept_text(
                store,
                &reviewer,
                meta(request),
                resolved.proposal,
                LegacyReview {
                    directory,
                    filename,
                    observed_version: resolved.basis.version.get(),
                },
                hash.map(|public_revision| BodyObservation { public_revision }),
                "Reviewed",
                typed::MAX_FRAME_BYTES,
                &Limits::V1,
                &mut ledger(),
            )
        };
    let submit_text = |store: &mut Store<ObservingFs>,
                       request,
                       file,
                       atom: Option<&str>,
                       body: Option<&str>,
                       revision,
                       version| {
        submit_legacy_concept_text(
            store,
            &grants(Capability::Propose),
            meta(request),
            ConceptTextEdit {
                subject: RecordKey::term(1),
                field_mask: if atom.is_some() { MASK_ATOM } else { MASK_BODY },
                atom,
                body,
                declared_author: "CLI",
            },
            "Authored reason",
            LegacyProposal {
                file,
                observed_version: version,
            },
            &[RevPair {
                key: RecordKey::term(1),
                revision,
            }],
            &Limits::V1,
            &mut ledger(),
        )
    };
    let atom = LegacyProposalFile::Atom {
        filename: ATOM,
        current_atom: "Original atom",
    };
    let body = LegacyProposalFile::Body {
        filename: BODY,
        expected_revision: [0x11; 32],
    };
    submit_edge(&mut store, "edge", 3, EDGE, Confidence::Confirmed).unwrap();
    submit_text(
        &mut store,
        "atom",
        atom,
        Some("After edge approval"),
        None,
        RecordRevision::FIRST,
        4,
    )
    .unwrap();
    submit_text(
        &mut store,
        "body",
        body,
        None,
        Some("# Different candidate\n"),
        RecordRevision::FIRST,
        5,
    )
    .unwrap();

    let first_edge = resolve(&mut store, LegacyDirectory::Edges, EDGE);
    assert!(matches!(
        merge_edge(&mut store, "first-merge", first_edge.proposal, 6).unwrap(),
        HandleOutcome::Committed { version: 7, .. },
    ));
    let RecordLookup::Found {
        record: FoldedRecord::Term(term),
        ..
    } = store.record(RecordKey::term(1), &mut ledger()).unwrap()
    else {
        panic!("term")
    };
    assert_eq!(
        term.revision,
        RecordRevision::FIRST,
        "edge need not revise Term"
    );
    assert!(matches!(
        merge_text(
            &mut store,
            "edge-only-moved-body",
            LegacyDirectory::Bodies,
            BODY,
            Some([0x12; 32])
        ),
        Err(ConceptTextError::Command(CommandError::Input(
            InputError::LegacyProposalBodyMismatch { .. }
        )))
    ));
    assert_eq!(store.head(), Some(7));
    assert_eq!(
        resolve(&mut store, LegacyDirectory::Bodies, BODY).status,
        ProposalStatus::Pending
    );

    assert!(matches!(
        merge_text(&mut store, "atom-merge", LegacyDirectory::Atoms, ATOM, None).unwrap(),
        HandleOutcome::Committed { version: 8, .. },
    ));
    let RecordLookup::Found {
        record: FoldedRecord::Term(term),
        ..
    } = store.record(RecordKey::term(1), &mut ledger()).unwrap()
    else {
        panic!("term")
    };
    assert_eq!(term.atom, "After edge approval");
    let revision = term.revision;

    submit_edge(&mut store, "edge-overwrite", 8, EDGE, Confidence::Contested).unwrap();
    assert!(matches!(
        merge_edge(&mut store, "obsolete-alias", first_edge.proposal, 9),
        Err(RelationAssertError::Command(CommandError::Input(
            InputError::LegacyProposalAliasMoved
        ))),
    ));
    let latest_edge = resolve(&mut store, LegacyDirectory::Edges, EDGE);
    assert!(matches!(
        merge_edge(&mut store, "latest-duplicate-edge", latest_edge.proposal, 9).unwrap(),
        HandleOutcome::Committed { version: 10, .. },
    ));
    assert_eq!(
        store
            .commit_basis(IdSpace::AssertionId, &mut ledger())
            .unwrap()
            .allocated_high_water,
        1
    );
    let RecordLookup::Found {
        record: FoldedRecord::Assertion(edge),
        ..
    } = store
        .record(RecordKey::new(RecordKind::Assertion, 1), &mut ledger())
        .unwrap()
    else {
        panic!("the actual published edge")
    };
    assert_eq!(edge.confidence, Confidence::Confirmed);
    assert_eq!(edge.declaration_order, 0);

    let equal_file = "2026-11-21T20-20-21-123Z-agent-body-abcdef.json";
    let equal = LegacyProposalFile::Body {
        filename: equal_file,
        expected_revision: [0x23; 32],
    };
    submit_text(
        &mut store,
        "equal",
        equal,
        None,
        Some("# Original\n"),
        revision,
        10,
    )
    .unwrap();
    assert!(matches!(
        merge_text(
            &mut store,
            "equal-merge",
            LegacyDirectory::Bodies,
            equal_file,
            Some([0x24; 32])
        )
        .unwrap(),
        HandleOutcome::Committed { version: 12, .. },
    ));
    drop(store);
    let Opened::Ready(mut store) =
        open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger()).unwrap()
    else {
        panic!("reopen canonical")
    };
    assert!(matches!(
        merge_edge(&mut store, "latest-duplicate-edge", latest_edge.proposal, 9).unwrap(),
        HandleOutcome::AlreadyCommitted { version: 10, .. },
    ));
    assert_eq!(store.head(), Some(12));
    assert_eq!(
        resolve(&mut store, LegacyDirectory::Bodies, BODY).status,
        ProposalStatus::Pending
    );
    assert_eq!(
        resolve(&mut store, LegacyDirectory::Bodies, equal_file).status,
        ProposalStatus::Approved
    );
    let RecordLookup::Found {
        record: FoldedRecord::Term(term),
        ..
    } = store.record(RecordKey::term(1), &mut ledger()).unwrap()
    else {
        panic!("term")
    };
    assert_eq!(
        term.revision, revision,
        "equal body approved without touching Term"
    );

    let apply_file = "2026-11-21T20-20-22-123Z-agent-body-abcdef.json";
    let apply = LegacyProposalFile::Body {
        filename: apply_file,
        expected_revision: [0x33; 32],
    };
    submit_text(
        &mut store,
        "apply-body",
        apply,
        None,
        Some("# New body\n"),
        revision,
        12,
    )
    .unwrap();
    assert!(matches!(
        merge_text(
            &mut store,
            "apply-body-merge",
            LegacyDirectory::Bodies,
            apply_file,
            Some([0x33; 32])
        )
        .unwrap(),
        HandleOutcome::Committed { version: 14, .. },
    ));
    let RecordLookup::Found {
        record: FoldedRecord::Term(term),
        ..
    } = store.record(RecordKey::term(1), &mut ledger()).unwrap()
    else {
        panic!("term")
    };
    assert_eq!(term.revision, revision.checked_next().unwrap());
    let body_ref = term.body_ref;
    assert_eq!(
        store
            .read_committed_blob(body_ref, typed::MAX_FRAME_BYTES, &mut ledger())
            .unwrap()
            .as_bytes(),
        b"# New body\n",
    );
    assert_eq!(
        resolve(&mut store, LegacyDirectory::Bodies, apply_file).status,
        ProposalStatus::Approved
    );
}

#[test]
fn an_absent_slug_has_a_durable_pending_intent_and_resolves_only_to_its_real_future_term() {
    use sage_db_kernel::{
        legacy_proposal::LegacyReview,
        relations::{
            RelationAssertInputError, merge_legacy_relation_assert, submit_legacy_relation_by_slug,
        },
    };
    use sage_db_types::persist::IdSpace;

    const SLUG: &str = "future--target";
    const FILE: &str = "2026-11-21-agent-enables-future--target.md";
    let (fs, mut store) = seed();
    let source = pair(1);
    let read = grants(Capability::Read);
    let propose = grants(Capability::Propose);
    let review = grants(Capability::Review);
    let before_graph = store.published_pin().unwrap().id().graph_digest;
    let submit = |store: &mut Store<ObservingFs>,
                  auth: &Grants<'_>,
                  request_id: &str,
                  target: &str,
                  version| {
        submit_legacy_relation_by_slug(
            store,
            auth,
            meta(request_id),
            RelationAssert {
                from: source.key,
                target,
                relation: RelationId::Enables as u16,
                confidence: Confidence::Hypothetical.as_u16(),
                declaration_order: 0,
                declared_author: "original attribution",
                reason: "the source proposes an authored future slug",
            },
            LegacyProposal {
                file: LegacyProposalFile::Edge { filename: FILE },
                observed_version: version,
            },
            &[source],
            &Limits::V1,
            &mut ledger(),
        )
    };
    assert!(matches!(
        submit(&mut store, &read, "submit-before-target", SLUG, 3),
        Err(RelationAssertError::Command(CommandError::Auth(_)))
    ));
    assert!(matches!(
        submit(&mut store, &propose, "wrong-basis", SLUG, 2),
        Err(RelationAssertError::Command(CommandError::Store(
            HandleRefusal::ExpectVersionMismatch {
                expected: 2,
                found: Some(3)
            }
        )))
    ));
    assert!(matches!(
        submit(&mut store, &propose, "already-known", "risk", 3),
        Err(RelationAssertError::Input(
            RelationAssertInputError::TargetSlugAlreadyExists
        ))
    ));
    assert!(matches!(
        submit(&mut store, &propose, "submit-before-target", SLUG, 3).unwrap(),
        HandleOutcome::Committed { version: 4, .. }
    ));
    assert_eq!(
        store.published_pin().unwrap().id().graph_digest,
        before_graph
    );

    drop(store);
    let Opened::Ready(mut store) =
        open_existing(fs, Path::new(ROOT), Some(kb()), &Limits::V1, &mut ledger()).unwrap()
    else {
        panic!("new pending version and old events must replay");
    };

    assert!(matches!(
        submit(&mut store, &propose, "submit-before-target", SLUG, 3).unwrap(),
        HandleOutcome::AlreadyCommitted { version: 4, .. }
    ));
    assert!(matches!(
        submit(
            &mut store,
            &propose,
            "submit-before-target",
            "another-target",
            3
        ),
        Err(RelationAssertError::Command(
            CommandError::IdempotencyMismatch
        ))
    ));

    let alias = lookup_alias(
        &mut store,
        &read,
        LegacyDirectory::Edges,
        FILE,
        &mut ledger(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(alias.status, ProposalStatus::Pending);
    let read_proposal =
        |store: &Store<ObservingFs>| match store.record(alias.proposal.key, &mut ledger()).unwrap()
        {
            RecordLookup::Found {
                record: FoldedRecord::Proposal(row),
                ..
            } => {
                assert_eq!(row.subject, source.key);
                assert_eq!(row.base_revisions, [source]);
                row.payload_ref
            }
            other => panic!("missing canonical proposal: {other:?}"),
        };
    let payload_ref = read_proposal(&store);
    let payload = store
        .read_committed_blob(payload_ref, typed::MAX_FRAME_BYTES, &mut ledger())
        .unwrap();
    let typed::RequestOutcome::Ok(frame) = typed::decode_request(payload.as_bytes(), &mut ledger())
    else {
        panic!("version 3 request frame decodes");
    };
    assert_eq!(frame.expected_version, Some(3));
    assert_eq!(frame.read_set, [source]);
    let [operation] = frame.ops.as_slice() else {
        panic!("one operation")
    };
    assert_eq!((operation.kind, operation.version), (10, 3));
    let inner = &operation.payload[3 + FILE.len()..];
    assert_eq!(&inner[..2], &1u16.to_le_bytes());
    assert_eq!(&inner[2..18], &1u128.to_le_bytes());
    assert_eq!(&inner[18..22], &(SLUG.len() as u32).to_le_bytes());
    assert_eq!(&inner[22..22 + SLUG.len()], SLUG.as_bytes());

    inspect(&mut store, LegacyDirectory::Edges, FILE, |_, intent| {
        let LegacyIntent::PendingEdge(edge) = intent else {
            panic!("v3 pending edge")
        };
        assert_eq!(edge.target_slug, SLUG);
        assert_eq!(edge.from, source.key);
        assert_eq!(edge.declared_author, "original attribution");
        assert_eq!(edge.relation, RelationId::Enables);
    });

    let decide = |store: &mut Store<ObservingFs>, version| {
        merge_legacy_relation_assert(
            store,
            &review,
            meta("merge-when-real"),
            alias.proposal,
            LegacyReview {
                directory: LegacyDirectory::Edges,
                filename: FILE,
                observed_version: version,
            },
            "Reviewed after target exists",
            Budget::DEFAULT,
            &RunToCompletion,
            &Limits::V1,
            &mut ledger(),
        )
    };
    assert!(matches!(
        decide(&mut store, 4),
        Err(RelationAssertError::Input(
            RelationAssertInputError::TargetSlugUnresolved
        ))
    ));
    assert_eq!(store.head(), Some(4));
    assert_eq!(
        store
            .commit_basis(IdSpace::AssertionId, &mut ledger())
            .unwrap()
            .allocated_high_water,
        0
    );
    let still_pending = lookup_alias(
        &mut store,
        &read,
        LegacyDirectory::Edges,
        FILE,
        &mut ledger(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(still_pending.status, ProposalStatus::Pending);

    create_term(&mut store, SLUG);
    assert_eq!(store.head(), Some(5));
    assert!(matches!(
        decide(&mut store, 5).unwrap(),
        HandleOutcome::Committed { version: 6, .. }
    ));
    let assertion = RecordKey::new(RecordKind::Assertion, 1);
    let RecordLookup::Found {
        record: FoldedRecord::Assertion(edge),
        ..
    } = store.record(assertion, &mut ledger()).unwrap()
    else {
        panic!("resolved assertion")
    };
    assert_eq!(edge.from, source.key);
    assert_eq!(edge.target, RecordKey::term(3));
    assert_eq!(edge.relation, RelationId::Enables);
    let approved = lookup_alias(
        &mut store,
        &read,
        LegacyDirectory::Edges,
        FILE,
        &mut ledger(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(approved.status, ProposalStatus::Approved);
}

#[test]
fn pending_target_preserves_the_legacy_slug_domain_before_any_term_exists() {
    let (_, mut store) = seed();
    let slug = "a".repeat(typed::MAX_SLUG_BYTES as usize + 1);
    let filename = format!("2026-11-21-agent-enables-{slug}.md");
    let result = relations::submit_legacy_relation_by_slug(
        &mut store,
        &grants(Capability::Propose),
        meta("long-authored-slug"),
        RelationAssert {
            from: RecordKey::term(1),
            target: &slug,
            relation: RelationId::Enables as u16,
            confidence: Confidence::Hypothetical.as_u16(),
            declaration_order: 0,
            declared_author: "",
            reason: "authored slug",
        },
        LegacyProposal {
            file: LegacyProposalFile::Edge {
                filename: &filename,
            },
            observed_version: 3,
        },
        &[pair(1)],
        &Limits::V1,
        &mut ledger(),
    );
    assert!(matches!(
        result.unwrap(),
        HandleOutcome::Committed { version: 4, .. }
    ));
    inspect(
        &mut store,
        LegacyDirectory::Edges,
        &filename,
        |_, intent| {
            let LegacyIntent::PendingEdge(pending) = intent else {
                panic!("pending authored slug")
            };
            assert_eq!(pending.target_slug, slug);
        },
    );
}
