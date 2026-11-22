//! Wire round-trips over the **same generated DTOs**, in both mappings.
//!
//! Every test here encodes with `prost` (the binary mapping) and with `pbjson` (the standard
//! protobuf JSON mapping), decodes the result back, and asserts the decoded message equals
//! the original. Two properties are under test that a hand-written DTO model could not give:
//!
//! * the JSON is the *standard* mapping — 64-bit integers travel as decimal strings, so an
//!   id above 2^53 or a `u128::MAX` survives unchanged and a JSON *number* is refused rather
//!   than rounded;
//! * presence survives — proto3 `optional` keeps `None` distinguishable from `Some("")`, and
//!   a oneof keeps an empty variant distinguishable from an unset one.
//!
//! What this file does not claim: that decoding is bounded. That is a property of the
//! decoder and of future network ingress, not of these generated types.

use prost::Message;
use sage_db_protocol::generated::CommitCommandResponse as _CommitResponseAlias;
use sage_db_protocol::generated::{
    CommitCommandResponse, CommitPayload, ConceptText, Decision, Envelope, ErrorCode,
    FrontierState, GetRequestOutcomeResponse, OutcomeAbsent, OutcomeFound, OutcomeIncomplete,
    ProposalStatus, RecordKey, RecordedCommit, RevPair, ReviewDecisionPayload, ReviewPayload,
    ReviewProposalResponse, SessionCheckpointState, SubmitPayload, Unknown, ViewBasis,
    commit_payload, get_request_outcome_response, review_payload, submit_payload, write_outcome,
};

/// Encode with prost, decode with prost, and require equality.
fn binary_round_trip<M>(message: &M) -> M
where
    M: Message + Default + PartialEq + core::fmt::Debug,
{
    let bytes = message.encode_to_vec();
    M::decode(bytes.as_slice()).expect("prost decodes what prost encoded")
}

/// Encode with pbjson, decode with pbjson, and require equality.
fn json_round_trip<M>(message: &M) -> M
where
    M: serde::Serialize + serde::de::DeserializeOwned + PartialEq + core::fmt::Debug,
{
    let text = serde_json::to_string(message).expect("pbjson serialises");
    serde_json::from_str(&text).expect("pbjson deserialises what it serialised")
}

/// A KB uuid in the canonical 32-character lowercase hex form.
fn kb() -> String {
    "5a".repeat(16)
}

fn key(kind: u32, id: &str) -> RecordKey {
    RecordKey {
        kind,
        id: id.into(),
    }
}

#[test]
fn an_id_above_2_pow_53_survives_binary_and_json_exactly() {
    // 2^53 + 1 is the first integer a f64 cannot represent.
    let id = "9007199254740993";
    let envelope = Envelope {
        protocol_version: 1,
        kb_uuid: kb(),
        request_id: "req-bigint".into(),
        deadline_ms: Some(u64::MAX),
        read_set: vec![RevPair {
            key: Some(key(1, id)),
            revision: "18446744073709551615".into(),
        }],
        expect_version: Some("18446744073709551615".into()),
    };
    assert_eq!(binary_round_trip(&envelope), envelope);
    assert_eq!(json_round_trip(&envelope), envelope);

    // The JSON carries the value as a STRING, which is what makes it exact.
    let text = serde_json::to_string(&envelope).expect("serialises");
    assert!(
        text.contains("\"id\":\"9007199254740993\""),
        "id must be a JSON string, got {text}"
    );
}

#[test]
fn a_u128_max_record_id_survives_both_mappings() {
    let id = u128::MAX.to_string();
    assert_eq!(id, "340282366920938463463374607431768211455");
    let frontier = FrontierState {
        concept_refs: vec![key(1, &id)],
        text: "t".into(),
        priority: 2,
        status: 1,
    };
    let payload = CommitPayload {
        kind: Some(commit_payload::Kind::CreateFrontier(frontier)),
    };
    assert_eq!(binary_round_trip(&payload), payload);
    assert_eq!(json_round_trip(&payload), payload);
    let text = serde_json::to_string(&payload).expect("serialises");
    assert!(
        text.contains(&format!("\"id\":\"{id}\"")),
        "u128::MAX must round-trip as a string, got {text}"
    );
}

#[test]
fn a_json_numeric_id_is_refused_rather_than_rounded() {
    // The failure a JSON number would cause: serde_json parses it into f64/u64 and the
    // generated deserializer expects a string, so the decode must fail rather than coerce.
    let numeric = r#"{"kind":1,"id":9007199254740993}"#;
    let refused = serde_json::from_str::<RecordKey>(numeric);
    assert!(
        refused.is_err(),
        "a numeric id must be refused, not silently rounded: {refused:?}"
    );

    // The string form of the same value is accepted and stays exact.
    let exact = r#"{"kind":1,"id":"9007199254740993"}"#;
    let decoded: RecordKey = serde_json::from_str(exact).expect("string id decodes");
    assert_eq!(decoded.id, "9007199254740993");
}

#[test]
fn proto3_optional_keeps_none_distinct_from_empty() {
    // `None` (field absent) and `Some("")` (field present and empty) are different states:
    // an editor clearing a body must be able to say so.
    let absent = ConceptText {
        subject: Some(key(1, "1")),
        atom: None,
        body: None,
        declared_author: String::new(),
        reason: None,
    };
    let empty = ConceptText {
        subject: Some(key(1, "1")),
        atom: Some(String::new()),
        body: Some(String::new()),
        declared_author: String::new(),
        reason: None,
    };
    for payload in [
        SubmitPayload {
            kind: Some(submit_payload::Kind::ConceptText(absent.clone())),
        },
        SubmitPayload {
            kind: Some(submit_payload::Kind::ConceptText(empty.clone())),
        },
    ] {
        assert_eq!(binary_round_trip(&payload), payload);
        assert_eq!(json_round_trip(&payload), payload);
    }
    // And the two are not equal to each other, so the distinction is real.
    assert_ne!(absent, empty);

    // The JSON omits an absent field and emits an empty string for a present empty one.
    let absent_json = serde_json::to_string(&absent).expect("serialises");
    assert!(
        !absent_json.contains("atom"),
        "absent atom must be omitted: {absent_json}"
    );
    let empty_json = serde_json::to_string(&empty).expect("serialises");
    assert!(
        empty_json.contains("\"atom\":\"\""),
        "present-but-empty atom must appear: {empty_json}"
    );
}

#[test]
fn unicode_and_nul_are_carried_unmodified() {
    let body = "line\n\u{4e2d}\u{6587}\u{1f3af} nul:\u{0} quote:\" slash:\\ end";
    let payload = CommitPayload {
        kind: Some(commit_payload::Kind::CreateSessionCheckpoint(
            SessionCheckpointState {
                concept: Some(key(1, "7")),
                session_no: 3,
                content: body.into(),
            },
        )),
    };
    assert_eq!(binary_round_trip(&payload), payload);
    let back = json_round_trip(&payload);
    assert_eq!(back, payload);
    // The exact bytes survive, including the NUL.
    match back.kind {
        Some(commit_payload::Kind::CreateSessionCheckpoint(state)) => {
            assert_eq!(state.content.as_bytes(), body.as_bytes());
        }
        other => panic!("unexpected variant {other:?}"),
    }
}

#[test]
fn every_oneof_variant_round_trips_and_stays_distinct() {
    let claim = submit_payload::Kind::ClaimEdit(sage_db_protocol::generated::ClaimEdit {
        subject: Some(key(1, "1")),
        claim: Some(key(4, "2")),
        text: "t".into(),
        declared_author: "a".into(),
    });
    let concept = submit_payload::Kind::ConceptText(ConceptText {
        subject: Some(key(1, "1")),
        atom: Some("a".into()),
        body: Some("b".into()),
        declared_author: "a".into(),
        reason: None,
    });
    let relation =
        submit_payload::Kind::RelationAssert(sage_db_protocol::generated::RelationAssert {
            from: Some(key(1, "1")),
            target: Some(key(1, "2")),
            relation: Some(0),
            confidence: Some(0),
            declaration_order: 1,
            declared_author: String::new(),
            reason: "r".into(),
        });
    for kind in [claim, concept, relation] {
        let payload = SubmitPayload { kind: Some(kind) };
        assert_eq!(binary_round_trip(&payload), payload);
        assert_eq!(json_round_trip(&payload), payload);
        // Exactly one variant is set after decoding.
        match &payload.kind {
            Some(submit_payload::Kind::ClaimEdit(_))
            | Some(submit_payload::Kind::ConceptText(_))
            | Some(submit_payload::Kind::RelationAssert(_)) => {}
            other => panic!("oneof lost its variant: {other:?}"),
        }
    }

    // All four commit variants.
    let variants = vec![
        commit_payload::Kind::CreateSessionCheckpoint(SessionCheckpointState {
            concept: Some(key(1, "1")),
            session_no: 1,
            content: "c".into(),
        }),
        commit_payload::Kind::ReviseSessionCheckpoint(
            sage_db_protocol::generated::ReviseSessionCheckpoint {
                checkpoint: Some(key(8, "2")),
                state: Some(SessionCheckpointState {
                    concept: Some(key(1, "1")),
                    session_no: 1,
                    content: "c2".into(),
                }),
            },
        ),
        commit_payload::Kind::CreateFrontier(FrontierState {
            concept_refs: vec![key(1, "1")],
            text: "f".into(),
            priority: 0,
            status: 0,
        }),
        commit_payload::Kind::ReviseFrontier(sage_db_protocol::generated::ReviseFrontier {
            frontier: Some(key(8, "1")),
            state: Some(FrontierState {
                concept_refs: vec![],
                text: "f2".into(),
                priority: 1,
                status: 1,
            }),
        }),
    ];
    for kind in variants {
        let payload = CommitPayload { kind: Some(kind) };
        assert_eq!(binary_round_trip(&payload), payload);
        assert_eq!(json_round_trip(&payload), payload);
    }
}

/// One review decision body, shared by both families.
fn review_body(decision: Decision) -> ReviewDecisionPayload {
    ReviewDecisionPayload {
        proposal: Some(key(8, "1")),
        decision: decision as i32,
        note: "because".into(),
    }
}

#[test]
fn review_shape_is_one_wire_shape_for_every_decision_and_both_families() {
    for decision in [Decision::Approve, Decision::Reject] {
        for family in [
            review_payload::Family::ClaimEdit(review_body(decision)),
            review_payload::Family::ConceptText(review_body(decision)),
            review_payload::Family::RelationAssert(review_body(decision)),
        ] {
            let payload = ReviewPayload {
                family: Some(family),
            };
            assert_eq!(binary_round_trip(&payload), payload);
            assert_eq!(json_round_trip(&payload), payload);
        }
    }
}

#[test]
fn the_review_family_is_carried_on_the_wire_and_distinguishable() {
    // The two families hash different operation kinds (claim-edit op 2, concept-text op 8), so
    // the request shape must identify the family without reading stored state.
    let claim = ReviewPayload {
        family: Some(review_payload::Family::ClaimEdit(review_body(
            Decision::Approve,
        ))),
    };
    let concept = ReviewPayload {
        family: Some(review_payload::Family::ConceptText(review_body(
            Decision::Approve,
        ))),
    };
    assert_ne!(claim, concept);
    assert_ne!(
        binary_round_trip(&claim).encode_to_vec(),
        binary_round_trip(&concept).encode_to_vec(),
        "the two families must not share a wire encoding"
    );
    // JSON keeps the oneof key name, so a consumer cannot mistake one family for the other.
    let text = serde_json::to_string(&concept).expect("serialises");
    assert!(text.contains("conceptText"), "got {text}");
    assert!(!text.contains("claimEdit"), "got {text}");
}

/// One recorded commit body, as both `Committed` and `AlreadyCommitted` carry it.
fn recorded_commit(version: &str) -> RecordedCommit {
    RecordedCommit {
        affected: vec![RevPair {
            key: Some(key(1, "1")),
            revision: "2".into(),
        }],
        counters: Vec::new(),
        commit_version: version.into(),
        result_ref: "ab".repeat(32),
    }
}

/// The four outcomes every write method can produce, as the shared `WriteOutcome`.
fn outcomes() -> Vec<write_outcome::Outcome> {
    vec![
        write_outcome::Outcome::Committed(sage_db_protocol::generated::Committed {
            recorded: Some(recorded_commit("7")),
            detail: None,
        }),
        // The ORIGINAL version, plus the decision the original attempt recorded.
        write_outcome::Outcome::AlreadyCommitted(sage_db_protocol::generated::AlreadyCommitted {
            recorded: Some(recorded_commit("7")),
            original_status: Some(ProposalStatus::Rejected as i32),
        }),
        write_outcome::Outcome::Unknown(Unknown {
            request_id: "req-1".into(),
        }),
        write_outcome::Outcome::Error(sage_db_protocol::generated::CommandError {
            code: ErrorCode::Conflict as i32,
            message: "stale baseline".into(),
            budget: None,
        }),
    ]
}

#[test]
fn every_write_method_response_carries_all_four_outcomes_through_the_shared_type() {
    // One vocabulary, three methods: a schema with per-method dialects is how an invented
    // `commit_version` reaches a caller on the method that lacks the `Unknown` variant.
    use sage_db_protocol::generated::{SubmitProposalResponse, WriteOutcome};

    for outcome in outcomes() {
        let shared = WriteOutcome {
            outcome: Some(outcome.clone()),
        };
        // Submit, Review and Commit all carry the identical value.
        let submit = SubmitProposalResponse {
            outcome: Some(shared.clone()),
        };
        let review = ReviewProposalResponse {
            outcome: Some(shared.clone()),
        };
        let commit = CommitCommandResponse {
            outcome: Some(shared.clone()),
        };
        for (label, json) in [
            ("submit", serde_json::to_string(&submit).unwrap()),
            ("review", serde_json::to_string(&review).unwrap()),
            ("commit", serde_json::to_string(&commit).unwrap()),
        ] {
            assert!(
                json.contains("outcome"),
                "{label} response must carry the shared outcome, got {json}"
            );
        }
        assert_eq!(binary_round_trip(&shared), shared);
        assert_eq!(json_round_trip(&shared), shared);
    }
}

#[test]
fn unknown_carries_no_version_and_already_committed_carries_the_original() {
    let unknown = CommitCommandResponse {
        outcome: Some(sage_db_protocol::generated::WriteOutcome {
            outcome: Some(write_outcome::Outcome::Unknown(Unknown {
                request_id: "req-1".into(),
            })),
        }),
    };
    let committed = CommitCommandResponse {
        outcome: Some(sage_db_protocol::generated::WriteOutcome {
            outcome: Some(write_outcome::Outcome::AlreadyCommitted(
                sage_db_protocol::generated::AlreadyCommitted {
                    recorded: Some(recorded_commit("7")),
                    original_status: None,
                },
            )),
        }),
    };
    assert_ne!(unknown, committed);
    assert_eq!(binary_round_trip(&unknown), unknown);
    assert_eq!(json_round_trip(&unknown), unknown);

    let text = serde_json::to_string(&unknown).expect("serialises");
    assert!(
        !text.contains("commitVersion"),
        "Unknown must carry no version, got {text}"
    );
    assert!(
        !text.contains("originalStatus"),
        "Unknown must carry no original status, got {text}"
    );
}

#[test]
fn a_review_result_reports_the_persisted_status_not_the_requested_decision() {
    // An approval that finds a stale baseline commits CONFLICTED. A response echoing the
    // request's APPROVE would state something false about stored data.
    let committed = sage_db_protocol::generated::Committed {
        recorded: Some(RecordedCommit {
            affected: vec![RevPair {
                key: Some(key(8, "1")),
                revision: "2".into(),
            }],
            counters: Vec::new(),
            commit_version: "8".into(),
            result_ref: "cd".repeat(32),
        }),
        detail: Some(sage_db_protocol::generated::committed::Detail::Review(
            sage_db_protocol::generated::ReviewCommitted {
                proposal: Some(key(8, "1")),
                status: ProposalStatus::Conflicted as i32,
            },
        )),
    };
    let response = ReviewProposalResponse {
        outcome: Some(sage_db_protocol::generated::WriteOutcome {
            outcome: Some(write_outcome::Outcome::Committed(committed)),
        }),
    };
    let text = serde_json::to_string(&response).expect("serialises");
    assert!(
        text.contains("PROPOSAL_STATUS_CONFLICTED") || text.contains("CONFLICTED"),
        "the persisted status must reach the wire, got {text}"
    );
    assert!(
        !text.contains("APPROVE"),
        "the response must not echo the requested decision, got {text}"
    );
    assert_eq!(binary_round_trip(&response), response);
}

#[test]
fn a_lost_ack_is_recoverable_because_already_committed_carries_the_same_body() {
    // The closure requirement: a client that lost the first Submit ACK must be able to recover
    // the identifiers it lost — above all the allocated proposal id — not merely a digest.
    let recorded = recorded_commit("9");
    let original = sage_db_protocol::generated::Committed {
        recorded: Some(recorded.clone()),
        detail: Some(sage_db_protocol::generated::committed::Detail::Submit(
            sage_db_protocol::generated::SubmitCommitted {
                proposal: Some(key(8, "42")),
            },
        )),
    };
    let retried = sage_db_protocol::generated::AlreadyCommitted {
        recorded: Some(recorded.clone()),
        original_status: None,
    };
    // The same recorded body, so the retry recovers the same version, refs and counters.
    assert_eq!(
        original.recorded.as_ref(),
        retried.recorded.as_ref(),
        "AlreadyCommitted must carry the same recorded body as the original Committed"
    );
    // The proposal id is reachable from the original commit's affected set.
    let affected = &recorded.affected;
    assert_eq!(affected.len(), 1);
    assert_eq!(
        affected[0].key.as_ref().map(|k| k.id.as_str()),
        Some("1"),
        "the allocated id is recoverable, not just its digest"
    );

    // An outcome lookup serves the same body from the hash-verified blob.
    let found = OutcomeFound {
        original_version: "9".into(),
        result_ref: recorded.result_ref.clone(),
        request_digest: "ef".repeat(32),
        basis: None,
        recorded: Some(recorded.clone()),
        original_status: Some(ProposalStatus::Pending as i32),
    };
    assert_eq!(found.recorded.as_ref(), Some(&recorded));

    for message in [
        serde_json::to_string(&original).unwrap(),
        serde_json::to_string(&retried).unwrap(),
        serde_json::to_string(&found).unwrap(),
    ] {
        assert!(
            message.contains("recorded") && message.contains("commitVersion"),
            "the recorded body must reach the wire, got {message}"
        );
    }
}

#[test]
fn an_empty_affected_list_inside_a_present_body_is_a_complete_result() {
    // C1, asserted as behaviour rather than only as documentation: presence lives on the OUTER
    // optional, so a present body with empty arrays is a legitimate complete commit — it must
    // NOT be confusable with an absent body, and must not be rejected as "not loaded".
    let empty_but_present = RecordedCommit {
        affected: Vec::new(),
        counters: Vec::new(),
        commit_version: "4".into(),
        result_ref: "00".repeat(32),
    };
    assert_eq!(
        binary_round_trip(&empty_but_present),
        empty_but_present,
        "a complete result with no affected records round-trips intact"
    );
    let present = sage_db_protocol::generated::Committed {
        recorded: Some(empty_but_present.clone()),
        detail: None,
    };
    let absent = sage_db_protocol::generated::Committed {
        recorded: None,
        detail: None,
    };
    assert_ne!(
        present, absent,
        "a present body with empty arrays is not the same value as an absent body"
    );
    let text = serde_json::to_string(&present).expect("serialises");
    assert!(
        text.contains("commitVersion"),
        "the complete result's version must reach the wire, got {text}"
    );
    let absent_text = serde_json::to_string(&absent).expect("serialises");
    assert!(
        !absent_text.contains("recorded"),
        "an absent body must not appear at all, got {absent_text}"
    );
}

#[test]
fn an_unloaded_recorded_body_is_absent_not_an_empty_commit() {
    // Presence is explicit. A server that did not read the blob omits `recorded`; a client must
    // not read that as "a commit with no affected records and no version".
    let unloaded = OutcomeFound {
        original_version: "9".into(),
        result_ref: "ab".repeat(32),
        request_digest: "ef".repeat(32),
        basis: None,
        recorded: None,
        original_status: None,
    };
    let text = serde_json::to_string(&unloaded).expect("serialises");
    assert!(
        !text.contains("recorded"),
        "an unloaded body must not appear at all, got {text}"
    );
    assert_eq!(binary_round_trip(&unloaded), unloaded);

    // And an empty `recorded` is a DIFFERENT value from an absent one.
    let empty_body = OutcomeFound {
        recorded: Some(RecordedCommit {
            affected: Vec::new(),
            counters: Vec::new(),
            commit_version: String::new(),
            result_ref: String::new(),
        }),
        ..unloaded.clone()
    };
    assert_ne!(empty_body, unloaded);
}

#[test]
fn absence_and_incompleteness_are_explicit_not_empty_responses() {
    let basis = ViewBasis {
        kb_uuid: kb(),
        version: "9".into(),
        manifest_sha256: "cd".repeat(32),
    };
    let absent = GetRequestOutcomeResponse {
        result: Some(get_request_outcome_response::Result::Absent(
            OutcomeAbsent {
                basis: Some(basis.clone()),
            },
        )),
    };
    let incomplete = GetRequestOutcomeResponse {
        result: Some(get_request_outcome_response::Result::Incomplete(
            OutcomeIncomplete {
                reason: "coverage".into(),
            },
        )),
    };
    let found = GetRequestOutcomeResponse {
        result: Some(get_request_outcome_response::Result::Found(OutcomeFound {
            original_version: "7".into(),
            result_ref: "ab".repeat(32),
            request_digest: "ef".repeat(32),
            basis: Some(basis),
            // Loaded from the committed result blob, so the retry can recover the original ids.
            recorded: Some(recorded_commit("7")),
            original_status: Some(ProposalStatus::Approved as i32),
        })),
    };
    for response in [absent, incomplete, found] {
        assert_eq!(binary_round_trip(&response), response);
        assert_eq!(json_round_trip(&response), response);
    }
    // An empty message is a distinct state from any of them, and is never read as "absent".
    let empty = GetRequestOutcomeResponse { result: None };
    assert_ne!(
        empty,
        GetRequestOutcomeResponse {
            result: Some(get_request_outcome_response::Result::Absent(
                OutcomeAbsent { basis: None }
            )),
        }
    );
}

#[test]
fn the_error_vocabulary_round_trips_by_name_and_number() {
    for code in [
        ErrorCode::InvalidArgument,
        ErrorCode::Unauthenticated,
        ErrorCode::PermissionDenied,
        ErrorCode::Conflict,
        ErrorCode::IdempotencyMismatch,
        ErrorCode::Budget,
        ErrorCode::Cancelled,
        ErrorCode::Deadline,
        ErrorCode::Unsupported,
        ErrorCode::Corrupt,
        ErrorCode::ReadOnly,
        ErrorCode::CommitUnknown,
        ErrorCode::ConceptSlugExists,
        ErrorCode::LegacyProposalAtomMismatch,
        ErrorCode::LegacyProposalBodyMismatch,
        ErrorCode::LegacyProposalTargetUnresolved,
    ] {
        let error = sage_db_protocol::generated::CommandError {
            code: code as i32,
            message: "refused".into(),
            budget: None,
        };
        assert_eq!(binary_round_trip(&error), error);
        assert_eq!(json_round_trip(&error), error);
        // JSON uses the PROTO enum value name (the standard mapping), not the Rust ident.
        let text = serde_json::to_string(&error).expect("serialises");
        let proto_name = match code {
            ErrorCode::InvalidArgument => "ERROR_CODE_INVALID_ARGUMENT",
            ErrorCode::Unauthenticated => "ERROR_CODE_UNAUTHENTICATED",
            ErrorCode::PermissionDenied => "ERROR_CODE_PERMISSION_DENIED",
            ErrorCode::Conflict => "ERROR_CODE_CONFLICT",
            ErrorCode::IdempotencyMismatch => "ERROR_CODE_IDEMPOTENCY_MISMATCH",
            ErrorCode::Budget => "ERROR_CODE_BUDGET",
            ErrorCode::Cancelled => "ERROR_CODE_CANCELLED",
            ErrorCode::Deadline => "ERROR_CODE_DEADLINE",
            ErrorCode::Unsupported => "ERROR_CODE_UNSUPPORTED",
            ErrorCode::Corrupt => "ERROR_CODE_CORRUPT",
            ErrorCode::ReadOnly => "ERROR_CODE_READ_ONLY",
            ErrorCode::CommitUnknown => "ERROR_CODE_COMMIT_UNKNOWN",
            ErrorCode::ConceptSlugExists => "ERROR_CODE_CONCEPT_SLUG_EXISTS",
            ErrorCode::LegacyProposalAtomMismatch => "ERROR_CODE_LEGACY_PROPOSAL_ATOM_MISMATCH",
            ErrorCode::LegacyProposalBodyMismatch => "ERROR_CODE_LEGACY_PROPOSAL_BODY_MISMATCH",
            ErrorCode::LegacyProposalTargetUnresolved => {
                "ERROR_CODE_LEGACY_PROPOSAL_TARGET_UNRESOLVED"
            }
            ErrorCode::Unspecified => panic!("unspecified is not in the accepted set"),
        };
        assert!(
            text.contains(proto_name),
            "proto enum name {proto_name} missing from {text}"
        );
    }
}

// The alias exists so the import list above proves the type name is what the generator emits.
const _: fn() = || {
    let _: _CommitResponseAlias = CommitCommandResponse { outcome: None };
};
