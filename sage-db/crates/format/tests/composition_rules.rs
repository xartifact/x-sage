//! §4.3 genesis shape and §7.2 outcome derivation/join, against t66's independently generated arms.
//!
//! Every fixture here was produced by a hand-written D10 oracle with an asserted CRC-32C known-answer,
//! not by this crate's encoder. The two positive controls (unmodified D10 genesis and v2) are what make
//! the eight rejections meaningful: without them, a validator that refused everything would "pass".

use sage_db_format::transaction::{DiscardEvents, GENESIS_TAG, read_transaction};
use sage_db_types::framing::{CorruptReason, FormatError, FormatOutcome, GenesisShapeRule, Limits};
use sage_db_types::ledger::{BudgetError, ControlStop, SharedLedger};

struct Open;

impl SharedLedger for Open {
    fn try_charge(&mut self, _bytes: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn try_charge_entries(&mut self, _count: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        Ok(())
    }
}

fn arm(name: &str) -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/comp")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn read(bytes: &[u8]) -> FormatOutcome<u32> {
    match read_transaction(bytes, &Limits::V1, None, &mut DiscardEvents, &mut Open) {
        FormatOutcome::Ok(s) => FormatOutcome::Ok(s.events_seen),
        FormatOutcome::Format(e) => FormatOutcome::Format(e),
        FormatOutcome::Refused(r) => FormatOutcome::Refused(r),
    }
}

// ---------------------------------------------------------------------------
// Positive controls: the validator must not reject valid files
// ---------------------------------------------------------------------------

#[test]
fn unmodified_d10_genesis_and_v2_are_still_accepted() {
    // Without these two, every rejection below would be satisfied by a validator that refuses all.
    match read(&arm("comp.genesis.positive.tx.sgl")) {
        FormatOutcome::Ok(seen) => assert_eq!(seen, 10, "Genesis + 9 IdAllocation"),
        other => panic!("genesis control must pass, got {other:?}"),
    }
    match read(&arm("comp.v2.positive.tx.sgl")) {
        FormatOutcome::Ok(seen) => assert_eq!(seen, 3, "v2 carries one outcome"),
        other => panic!("v2 control must pass, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Group 1: §4.3 genesis shape
// ---------------------------------------------------------------------------

#[test]
fn genesis_must_be_exactly_one_genesis_plus_nine_allocations() {
    // A genesis failure is a SHAPE failure, not an outcome count: genesis is exempt from the outcome
    // rule entirely, so reporting `OutcomeCount` here would describe a rule it is not subject to.
    match read(&arm("comp.genesis.only-genesis.tx.sgl")) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::GenesisShape { rule })) => {
            assert_eq!(
                rule,
                GenesisShapeRule::EventCount { found: 1 },
                "one event where §4.3 requires ten"
            );
        }
        other => panic!("only-genesis must be Corrupt(GenesisShape), got {other:?}"),
    }

    // Genesis + eight allocations: nine events where ten are required.
    match read(&arm("comp.genesis.eight-alloc.tx.sgl")) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::GenesisShape { rule })) => {
            assert_eq!(rule, GenesisShapeRule::EventCount { found: 9 });
        }
        other => panic!("eight-alloc must be Corrupt(GenesisShape), got {other:?}"),
    }
}

#[test]
fn a_genesis_allocation_with_a_nonzero_high_water_mark_is_refused() {
    // §4.3 fixes every initial high water mark at 0. Without this arm, a genesis whose nine allocations
    // each claim five pre-existing ids passes: the mark was carried in `CompositionFacts` but never
    // checked, so a positive control alone could not show the check existed.
    //
    // The fixture is `comp.genesis.positive` with all nine `165` fields set to 5, block CRC and trailer
    // recomputed independently, so the only difference from a passing file is the mark itself.
    match read(&arm("comp.genesis.nonzero-hwm.tx.sgl")) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::GenesisShape { rule })) => {
            // The reported mark is the one that appeared, not a sentinel.
            assert_eq!(
                rule,
                GenesisShapeRule::NonZeroWatermark { space: 0, hwm: 5 }
            );
        }
        other => panic!("nonzero genesis hwm must be refused, got {other:?}"),
    }
}

#[test]
fn a_header_only_violation_is_refused_even_when_an_event_is_also_invalid() {
    // ORDERING ORACLE. Counting visitor calls does NOT prove no event was decoded: a decoder that
    // parses every event and only then validates the header also delivers zero visitor calls on a
    // header failure. The discriminating file therefore fails BOTH ways — a version-1 header whose
    // canon/sentinel pair disagrees, AND a first event tagged 999 (unregistered, invalid) — so the
    // reason that comes back identifies which check ran first.
    //
    //   header-first  -> Corrupt(RequestCanonMismatch)
    //   parse-first   -> UnsupportedFormat(EventTag(999))
    //
    // Verified by temporarily deferring the header check into `finish()`: that build returns
    // EventTag(999), this one returns RequestCanonMismatch. CRC and trailer recomputed independently.
    match read(&arm("comp.canon1-bad-first-event.tx.sgl")) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RequestCanonMismatch)) => {}
        other => panic!(
            "header-first must win over the invalid event; got {other:?} \
             (EventTag(999) would mean events were parsed first)"
        ),
    }

    // The visitor count is asserted only as a delivery fact, not as an ordering oracle.
    use sage_db_format::dispatch::DispatchedEvent;
    use sage_db_format::transaction::EventVisitor;
    use sage_db_types::framing::Refusal;

    #[derive(Default)]
    struct Counting {
        visits: usize,
    }
    impl EventVisitor for Counting {
        fn visit(
            &mut self,
            _seq: u32,
            _event: &DispatchedEvent<'_>,
            _ledger: &mut dyn SharedLedger,
        ) -> Result<(), Refusal> {
            self.visits += 1;
            Ok(())
        }
    }

    let mut visitor = Counting::default();
    assert!(matches!(
        read_transaction(
            &arm("comp.canon1-bad-first-event.tx.sgl"),
            &Limits::V1,
            None,
            &mut visitor,
            &mut Open,
        ),
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RequestCanonMismatch))
    ));
    assert_eq!(
        visitor.visits, 0,
        "no event is delivered to the visitor on a header failure"
    );
}

#[test]
fn the_genesis_tag_constant_names_the_singleton() {
    // A guard on the constant the shape rule keys off: 224 is Genesis in this major.
    assert_eq!(GENESIS_TAG, 224);
}

// ---------------------------------------------------------------------------
// Group 2: version ⟺ canon ⟺ sentinel
// ---------------------------------------------------------------------------

#[test]
fn version_one_requires_canon_zero_and_the_all_zero_sentinel() {
    // version 1 with canon 1 and a non-zero digest: all three disagree.
    match read(&arm("comp.version1-canon1.tx.sgl")) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::RequestCanonMismatch)) => {}
        other => panic!("must be Corrupt(RequestCanonMismatch), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Group 3: ordinary outcome count and header join
// ---------------------------------------------------------------------------

#[test]
fn non_genesis_requires_exactly_one_outcome() {
    match read(&arm("comp.v2.no-outcome.tx.sgl")) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::OutcomeCount { found })) => {
            assert_eq!(found, 0);
        }
        other => panic!("no-outcome must be Corrupt(OutcomeCount), got {other:?}"),
    }
}

#[test]
fn a_second_outcome_is_refused() {
    match read(&arm("comp.v2.two-outcomes.tx.sgl")) {
        FormatOutcome::Format(FormatError::Corrupt(CorruptReason::OutcomeCount { found })) => {
            // `found` is the number of outcomes actually present, not an event index and not a
            // seen-so-far counter. This arm has two.
            assert_eq!(found, 2, "found must be the true outcome count");
        }
        other => panic!("two-outcomes must be Corrupt(OutcomeCount), got {other:?}"),
    }
}

#[test]
fn the_outcome_must_join_the_header_field_by_field() {
    // Each arm disagrees on exactly one field, so the reported tag identifies which join failed.
    for (name, expected_tag) in [
        ("comp.v2.outcome-principal-mismatch.tx.sgl", 176u16),
        ("comp.v2.outcome-request-mismatch.tx.sgl", 177),
        ("comp.v2.outcome-version-mismatch.tx.sgl", 181),
    ] {
        match read(&arm(name)) {
            FormatOutcome::Format(FormatError::Corrupt(CorruptReason::OutcomeHeaderMismatch {
                tag,
            })) => {
                assert_eq!(tag, expected_tag, "{name}: wrong field named");
            }
            other => panic!("{name} must be Corrupt(OutcomeHeaderMismatch), got {other:?}"),
        }
    }
}
