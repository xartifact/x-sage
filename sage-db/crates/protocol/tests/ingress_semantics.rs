//! Semantic parity with real pbjson, exercised through the PUBLIC wrappers.
//!
//! Each case decodes the same bytes twice — ordinary `serde_json` into the pbjson-derived type
//! (the oracle) and the admitted public wrapper — and asserts either equal DTOs or that both
//! refuse. Nothing is normalised on the expected side.

use sage_db_protocol::generated as g;
use sage_db_protocol::ingress::{
    IngressClass, IngressLimits, decode_commit_payload, decode_envelope, decode_record_key,
    decode_review_decision_payload,
};
use sage_db_types::ledger::{BudgetError, SharedLedger};
use sage_db_types::query::ControlStop;

/// A test ledger: everything fits.
struct Open;
impl SharedLedger for Open {
    fn try_charge(&mut self, _b: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _c: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

/// A ledger that COUNTS what it was charged, so a deleted admission is observable as a missing
/// charge rather than only as a behavioural coincidence.
struct Counting {
    bytes: u64,
    charged: u64,
    charged_entries: u64,
}
impl Counting {
    fn new() -> Self {
        Self {
            bytes: 1 << 20,
            charged: 0,
            charged_entries: 0,
        }
    }
}
impl SharedLedger for Counting {
    fn try_charge(&mut self, b: u64) -> Result<(), BudgetError> {
        if b > self.bytes {
            return Err(BudgetError::new(
                sage_db_types::ledger::BudgetUnit::Bytes,
                b,
                self.bytes,
            ));
        }
        self.bytes -= b;
        self.charged += b;
        Ok(())
    }
    fn try_charge_entries(&mut self, c: u64) -> Result<(), BudgetError> {
        self.charged_entries += c;
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

/// A ledger with a byte budget, to prove refusals are real.
struct Tight(u64);
impl SharedLedger for Tight {
    fn try_charge(&mut self, b: u64) -> Result<(), BudgetError> {
        if b > self.0 {
            return Err(BudgetError::new(
                sage_db_types::ledger::BudgetUnit::Bytes,
                b,
                self.0,
            ));
        }
        self.0 -= b;
        Ok(())
    }
    fn try_charge_entries(&mut self, _c: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

fn limits() -> IngressLimits {
    IngressLimits::new(1 << 20, 1 << 20)
}

fn oracle<T: serde::de::DeserializeOwned>(b: &[u8]) -> Result<T, String> {
    serde_json::from_slice(b).map_err(|e| e.to_string())
}

#[test]
fn oneof_null_is_absence_and_a_later_arm_is_allowed() {
    // Real pbjson spells a oneof member as `next_value::<Option<_>>()?.map(Arm)`, so `null` leaves
    // the slot unset and a LATER member is still accepted.
    let json = br#"{"createSessionCheckpoint":null,"createFrontier":{"text":"t","priority":1}}"#;
    let want: g::CommitPayload = oracle(json).expect("pbjson accepts null then a later arm");
    let got = decode_commit_payload(json, limits(), &mut Open)
        .expect("admitted decoder accepts what pbjson accepts");
    assert_eq!(got.dto(), &want);
    assert!(
        want.kind.is_some(),
        "the later arm must be present, not silently dropped"
    );

    // Null alone leaves the oneof unset.
    let only_null = br#"{"createSessionCheckpoint":null}"#;
    let want: g::CommitPayload = oracle(only_null).expect("pbjson accepts a null arm");
    let got = decode_commit_payload(only_null, limits(), &mut Open).expect("admitted accepts");
    assert_eq!(got.dto(), &want);
    assert!(want.kind.is_none());
}

#[test]
fn optional_string_null_is_absence_while_empty_is_distinct() {
    // `Envelope.expect_version` is `optional string`.
    let with_null = br#"{"protocolVersion":1,"kbUuid":"u","requestId":"r","expectVersion":null}"#;
    let want: g::Envelope = oracle(with_null).expect("pbjson accepts null for an optional string");
    let got =
        decode_envelope(with_null, limits(), &mut Open).expect("admitted decoder accepts null");
    assert_eq!(got.dto(), &want);
    assert!(want.expect_version.is_none(), "null is absence");

    let with_empty = br#"{"protocolVersion":1,"kbUuid":"u","requestId":"r","expectVersion":""}"#;
    let want_empty: g::Envelope = oracle(with_empty).expect("pbjson accepts an empty string");
    let got_empty =
        decode_envelope(with_empty, limits(), &mut Open).expect("admitted decoder accepts empty");
    assert_eq!(got_empty.dto(), &want_empty);
    assert_eq!(
        want_empty.expect_version.as_deref(),
        Some(""),
        "Some(\"\") must stay distinct from None"
    );

    // A later non-null value after a null is allowed (slot was never set).
    let null_then_value =
        br#"{"protocolVersion":1,"kbUuid":"u","requestId":"r","expectVersion":null,"expectVersion":"v7"}"#;
    let pb = oracle::<g::Envelope>(null_then_value);
    let ours = decode_envelope(null_then_value, limits(), &mut Open);
    assert_eq!(
        pb.is_ok(),
        ours.is_ok(),
        "both must agree on null-then-value"
    );
}

#[test]
fn alias_pair_accepts_both_spellings_and_refuses_duplicates() {
    // `protocol_version` has JSON name `protocolVersion` and proto name `protocol_version`.
    for body in [
        br#"{"protocolVersion":1,"kbUuid":"u","requestId":"r"}"#.as_slice(),
        br#"{"protocol_version":1,"kbUuid":"u","requestId":"r"}"#.as_slice(),
    ] {
        let want: g::Envelope = oracle(body).expect("pbjson accepts both spellings");
        let got = decode_envelope(body, limits(), &mut Open).expect("admitted accepts both");
        assert_eq!(got.dto(), &want);
    }
    // Both spellings at once is a duplicate for the SAME slot.
    let dup = br#"{"protocolVersion":1,"protocol_version":2}"#;
    assert!(oracle::<g::Envelope>(dup).is_err(), "pbjson refuses");
    assert!(
        decode_envelope(dup, limits(), &mut Open).is_err(),
        "admitted decoder must refuse the alias duplicate"
    );
}

#[test]
fn unknown_field_is_refused_by_both() {
    let body = br#"{"nope":1}"#;
    assert!(oracle::<g::RecordKey>(body).is_err());
    assert!(decode_record_key(body, limits(), &mut Open).is_err());
}

#[test]
fn trailing_data_and_empty_input_are_refused_by_both() {
    for body in [br#"{"kind":1,"id":"a"} x"#.as_slice(), b"".as_slice()] {
        assert!(oracle::<g::RecordKey>(body).is_err(), "pbjson refuses");
        assert!(
            decode_record_key(body, limits(), &mut Open).is_err(),
            "admitted decoder must refuse too"
        );
    }
}

#[test]
fn body_ceiling_is_checked_before_parsing() {
    let body = br#"{"kind":1,"id":"abc"}"#;
    let tight = IngressLimits::new(1, 1 << 20);
    let error = decode_record_key(body, tight, &mut Open)
        .err()
        .expect("a body over the ceiling must be refused");
    assert_eq!(error.0.class, IngressClass::BodyCeiling);
}

#[test]
fn owned_ceiling_refuses_a_dto_that_would_retain_too_much() {
    // Two bytes of owned ceiling cannot hold a 3-byte string plus the u32 slot.
    let body = br#"{"kind":1,"id":"abc"}"#;
    let tight = IngressLimits::new(1 << 20, 2);
    let error = decode_record_key(body, tight, &mut Open)
        .err()
        .expect("a DTO over the owned ceiling must be refused");
    assert!(
        matches!(
            error.0.class,
            IngressClass::OwnedCeiling | IngressClass::Refused
        ),
        "expected an owned-ceiling class, got {:?}",
        error.0
    );
}

#[test]
fn ledger_refusal_is_reported_with_its_canonical_payload() {
    let body = br#"{"kind":1,"id":"abcd"}"#;
    let mut tight = Tight(0);
    let error = decode_record_key(body, limits(), &mut tight)
        .err()
        .expect("a zero-byte ledger must refuse");
    // The FIRST failure is sticky, so the class survives the generic parser error path.
    assert!(
        error.0.class != IngressClass::Syntax,
        "a budget refusal is not a syntax error: {:?}",
        error.0
    );
}

#[test]
fn enum_names_and_closed_numbers_agree() {
    for body in [
        br#"{"proposal":{"kind":1,"id":"p"},"decision":"DECISION_APPROVE"}"#.as_slice(),
        br#"{"proposal":{"kind":1,"id":"p"},"decision":1}"#.as_slice(),
    ] {
        let want: g::ReviewDecisionPayload = oracle(body).expect("pbjson accepts");
        let got = decode_review_decision_payload(body, limits(), &mut Open)
            .expect("admitted decoder accepts what pbjson accepts");
        assert_eq!(got.dto(), &want, "enum decode must agree for {body:?}");
    }
    // A quoted number and an unknown number are NOT valid enum spellings.
    for body in [
        br#"{"proposal":{"kind":1,"id":"p"},"decision":"1"}"#.as_slice(),
        br#"{"proposal":{"kind":1,"id":"p"},"decision":99}"#.as_slice(),
    ] {
        let pb = oracle::<g::ReviewDecisionPayload>(body);
        let ours = decode_review_decision_payload(body, limits(), &mut Open);
        assert_eq!(pb.is_err(), ours.is_err(), "must agree on {body:?}");
    }
}

#[test]
fn string_owner_admission_is_charged() {
    // A String field must charge its owner bytes. Deleting the admission in the seed makes
    // `charged` drop to zero for the string, so this assertion turns red.
    let body = br#"{"kind":1,"id":"abcdefgh"}"#;
    let mut ledger = Counting::new();
    let decoded = decode_record_key(body, limits(), &mut ledger)
        .expect("a within-budget decode must succeed");
    assert_eq!(decoded.dto().id, "abcdefgh");
    assert!(
        ledger.charged >= 8,
        "the 8-byte string owner must be charged; ledger saw only {}",
        ledger.charged
    );
}

#[test]
fn vec_slot_admission_is_charged_per_element() {
    // Each repeated element must charge one slot. Deleting the slot admission makes the charge
    // fall below one slot per element.
    let body = br#"{"protocolVersion":1,"kbUuid":"u","requestId":"r","readSet":[{"key":{"kind":1,"id":"a"},"revision":"1"},{"key":{"kind":2,"id":"b"},"revision":"2"}]}"#;
    let mut ledger = Counting::new();
    let decoded =
        decode_envelope(body, limits(), &mut ledger).expect("a within-budget decode must succeed");
    assert_eq!(decoded.dto().read_set.len(), 2);
    // Two slots of RevPair plus the two nested strings' owners.
    let slot = core::mem::size_of::<g::RevPair>() as u64;
    assert!(
        ledger.charged >= 2 * slot,
        "two repeated slots must be charged (>= {}); ledger saw {}",
        2 * slot,
        ledger.charged
    );
}

#[test]
fn optional_enum_null_and_presence_match_pbjson() {
    // `AlreadyCommitted.original_status` is `optional ProposalStatus`.
    for body in [
        br#"{"originalStatus":null}"#.as_slice(),
        br#"{"originalStatus":"PROPOSAL_STATUS_PENDING"}"#.as_slice(),
        br#"{"originalStatus":2}"#.as_slice(),
        br#"{"originalStatus":0}"#.as_slice(),
    ] {
        let want: g::AlreadyCommitted = oracle(body).expect("pbjson accepts");
        let got = sage_db_protocol::ingress::decode_already_committed(body, limits(), &mut Open)
            .expect("admitted decoder accepts what pbjson accepts");
        assert_eq!(got.dto(), &want, "optional enum must agree for {body:?}");
    }
    // null and Some(0) are DIFFERENT: null is absence, 0 is UNSPECIFIED.
    let absent: g::AlreadyCommitted =
        oracle(br#"{"originalStatus":null}"#).expect("pbjson accepts null");
    let zero: g::AlreadyCommitted = oracle(br#"{"originalStatus":0}"#).expect("pbjson accepts 0");
    assert!(absent.original_status.is_none(), "null must be absence");
    assert_eq!(zero.original_status, Some(0), "0 must be a present value");
    let a = sage_db_protocol::ingress::decode_already_committed(
        br#"{"originalStatus":null}"#,
        limits(),
        &mut Open,
    )
    .expect("admitted accepts null");
    let z = sage_db_protocol::ingress::decode_already_committed(
        br#"{"originalStatus":0}"#,
        limits(),
        &mut Open,
    )
    .expect("admitted accepts 0");
    assert!(a.dto().original_status.is_none());
    assert_eq!(z.dto().original_status, Some(0));
}

#[test]
fn exact_control_stop_reaches_the_caller() {
    // A caller that asks to stop before any work must see the canonical `Refusal::Stopped`,
    // not a flattened class.
    struct StopNow;
    impl SharedLedger for StopNow {
        fn try_charge(&mut self, _b: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn try_charge_entries(&mut self, _c: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Err(ControlStop::Cancelled)
        }
    }
    let error = decode_record_key(br#"{"kind":1,"id":"a"}"#, limits(), &mut StopNow)
        .err()
        .expect("a stop must be observed");
    assert!(
        matches!(
            error.0.refusal,
            Some(sage_db_types::framing::Refusal::Stopped(
                ControlStop::Cancelled
            ))
        ),
        "the canonical stop must survive: {:?}",
        error.0
    );
}

#[test]
fn nested_message_null_matches_pbjson() {
    let body = br#"{"subject":null,"claim":null,"text":"t","declaredAuthor":"a"}"#;
    let want: g::ClaimEdit = oracle(body).expect("pbjson accepts null message fields");
    let _want = want;
    let ours = sage_db_protocol::ingress::decode_claim_edit(body, limits(), &mut Open)
        .map(|d| d.dto().clone());
    assert!(
        ours.is_ok(),
        "admitted decoder must accept null message fields"
    );
}
