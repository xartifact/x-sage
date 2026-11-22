//! Public-boundary probes for the descriptor ingress decoder.
//!
//! These are the reviewer's own cases: exact refusal payloads, inline-scalar accounting,
//! UTF-8 copy checkpoints, and required, optional, and repeated null distinctions.

use sage_db_protocol::generated::{Envelope, HandshakeResponse, RecordKey};
use sage_db_protocol::ingress::{
    IngressClass, IngressLimits, decode_envelope, decode_handshake_response, decode_record_key,
};
use sage_db_types::framing::Refusal;
use sage_db_types::ledger::{BudgetError, BudgetUnit, SharedLedger};
use sage_db_types::query::ControlStop;

struct Ledger {
    remaining: u64,
    owner_admitted: bool,
    checks_after_owner: usize,
}

impl SharedLedger for Ledger {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        if bytes > self.remaining {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.remaining));
        }
        self.remaining -= bytes;
        self.owner_admitted = true;
        Ok(())
    }

    fn try_charge_entries(&mut self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        if self.owner_admitted {
            self.checks_after_owner += 1;
        }
        Ok(())
    }
}

fn limits() -> IngressLimits {
    IngressLimits::new(65536, 65536)
}

#[test]
fn exact_seed_refusal_reaches_public_caller() {
    let mut ledger = Ledger {
        remaining: 1,
        owner_admitted: false,
        checks_after_owner: 0,
    };
    let err = decode_record_key(br#"{"id":"abcd"}"#, limits(), &mut ledger)
        .err()
        .expect("copy must refuse");
    assert_eq!(err.0.class, IngressClass::Refused);
    assert_eq!(
        err.0.refusal,
        Some(Refusal::Budget(BudgetError::new(BudgetUnit::Bytes, 4, 1)))
    );
}

#[test]
fn borrowed_utf8_copy_checkpoints_between_bounded_chunks() {
    for prefix in ["a".repeat(1024), format!("{}é", "a".repeat(1023))] {
        let id = format!("{prefix}{}", "x".repeat(8192));
        let body = format!(r#"{{"id":"{id}"}}"#);
        let mut ledger = Ledger {
            remaining: 65536,
            owner_admitted: false,
            checks_after_owner: 0,
        };
        let decoded =
            decode_record_key(body.as_bytes(), limits(), &mut ledger).expect("valid UTF-8");
        assert_eq!(decoded.dto().id, id);
        assert!(
            ledger.checks_after_owner >= 8,
            "only {} checkpoints after owner admission",
            ledger.checks_after_owner
        );
    }
}

#[test]
fn inline_scalar_requires_no_heap_credit() {
    let mut ledger = Ledger {
        remaining: 0,
        owner_admitted: false,
        checks_after_owner: 0,
    };
    let decoded = decode_record_key(br#"{"kind":1}"#, IngressLimits::new(65536, 0), &mut ledger)
        .expect("inline u32 requires neither owned bytes nor shared heap credit");
    assert_eq!(decoded.dto().kind, 1);
    assert_eq!(decoded.retained_bytes(), 0);
}

#[test]
fn scalar_default_null_matches_pbjson() {
    let body = br#"{"id":null}"#;
    let want = serde_json::from_slice::<RecordKey>(body);
    let mut ledger = Ledger {
        remaining: 65536,
        owner_admitted: false,
        checks_after_owner: 0,
    };
    let admitted = decode_record_key(body, limits(), &mut ledger);
    assert_eq!(want.is_ok(), admitted.is_ok());
}

#[test]
fn optional_number_null_matches_pbjson() {
    let body = br#"{"deadlineMs":null}"#;
    let want = serde_json::from_slice::<Envelope>(body).expect("oracle accepts absence");
    let mut ledger = Ledger {
        remaining: 65536,
        owner_admitted: false,
        checks_after_owner: 0,
    };
    let admitted =
        decode_envelope(body, limits(), &mut ledger).expect("admitted decoder must accept absence");
    assert_eq!(admitted.dto(), &want);
}

#[test]
fn repeated_string_null_element_matches_pbjson() {
    let body = br#"{"capabilities":[null]}"#;
    let want = serde_json::from_slice::<HandshakeResponse>(body);
    let mut ledger = Ledger {
        remaining: 65536,
        owner_admitted: false,
        checks_after_owner: 0,
    };
    let admitted = decode_handshake_response(body, limits(), &mut ledger);
    assert_eq!(want.is_ok(), admitted.is_ok());
}
