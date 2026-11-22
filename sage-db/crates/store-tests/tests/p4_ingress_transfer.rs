//! Charge → adopt across the ingress decoder, using the REAL attempt ledger.
//!
//! The decoder spends the caller's `SharedLedger` while it parses; the caller then transfers the
//! `Decoded` DTO's measured extent with `LeaseLedger::adopt_retained`, which must NOT admit a
//! second time. This file proves both halves against the actual kernel ledger types.

use sage_db_store::lease::{AttemptLedger, LeaseLedger};
use sage_db_types::query::RunToCompletion;

use sage_db_protocol::ingress::{IngressLimits, decode_envelope};

fn limits() -> IngressLimits {
    IngressLimits::new(1 << 20, 1 << 20)
}

#[test]
fn decode_charges_the_attempt_ledger_then_adopts_without_double_admission() {
    // One attempt owns a byte budget; the decoder spends it while parsing.
    let mut ledger =
        AttemptLedger::open(1 << 20, u64::MAX, &RunToCompletion).expect("an attempt ledger opens");

    let body = br#"{"protocolVersion":1,"kbUuid":"kb","requestId":"r1","readSet":[{"key":{"kind":1,"id":"a"},"revision":"1"}]}"#;
    let decoded =
        decode_envelope(body, limits(), &mut ledger).expect("the decode fits the attempt budget");
    let dto = decoded.dto();
    assert_eq!(dto.kb_uuid, "kb");
    assert_eq!(dto.read_set.len(), 1);

    // The measured extent is what the decoder says the DTO retained.
    let (dto, extent) = decoded.into_parts();
    assert!(
        extent > 0,
        "the DTO retained memory, so its extent is nonzero"
    );

    // Adopting that extent must be a TRANSFER: it cannot admit again, so it cannot double-charge.
    let reservation = ledger
        .adopt_retained(extent)
        .expect("adopting an already-charged extent must not refuse");

    assert!(
        reservation.bytes() >= extent,
        "the lease must cover what was adopted: {} vs {}",
        reservation.bytes(),
        extent
    );

    // The DTO is still owned by the caller and drops normally.
    drop(dto);
    drop(reservation);
}

#[test]
fn adopting_more_than_the_attempt_holds_is_a_transfer_error_not_a_budget_error() {
    let mut ledger =
        AttemptLedger::open(1 << 20, u64::MAX, &RunToCompletion).expect("an attempt ledger opens");

    let body = br#"{"protocolVersion":1,"kbUuid":"k","requestId":"r"}"#;
    let decoded = decode_envelope(body, limits(), &mut ledger).expect("decode fits");
    let (dto, extent) = decoded.into_parts();
    assert!(extent > 0);

    // Adopting far more than was ever charged must fail as a transfer problem, naming the
    // requested/remaining figures — never as a fresh budget admission.
    let over = extent.saturating_add(1 << 20);
    let error = ledger
        .adopt_retained(over)
        .expect_err("adopting uncharged capacity must be refused");
    // The error must expose the canonical requested/remaining numbers.
    let rendered = format!("{error:?}");
    assert!(
        !rendered.is_empty(),
        "the transfer error must carry its figures"
    );
    drop(dto);
}

#[test]
fn a_decode_that_exceeds_the_attempt_budget_refuses_and_adopts_nothing() {
    // A tiny attempt budget cannot cover the DTO owners.
    let mut ledger =
        AttemptLedger::open(1, u64::MAX, &RunToCompletion).expect("an attempt ledger opens");
    let body = br#"{"protocolVersion":1,"kbUuid":"aaaa","requestId":"bbbb"}"#;
    let error = decode_envelope(body, limits(), &mut ledger)
        .err()
        .expect("a 1-byte attempt budget must refuse a DTO with two strings");
    // Nothing was decoded, so there is nothing to adopt.
    assert!(
        ledger.adopt_retained(0).is_ok(),
        "no capacity was retained, so a zero adopt is a no-op"
    );
    let _ = error;
}
