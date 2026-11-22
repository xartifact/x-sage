//! Identity and budget contracts.
//!
//! These are the counter and meter rules the store relies on: no identity is ever
//! reused, exhaustion rejects a write, a revision can never be zero, and a caller
//! can tighten a budget but never widen it.

use sage_db_types::{
    AssertionId, Budget, BudgetLimit, Counter, KbVersion, RecordRevision, TermId, WorkMeter,
};

#[test]
fn every_counter_reports_its_own_exhaustion_instead_of_wrapping() {
    let term = TermId::new(u32::MAX);
    assert_eq!(
        term.checked_next().unwrap_err().counter(),
        Counter::TermId,
        "an exhausted term space must not yield a wrapped ID"
    );

    let assertion = AssertionId::new(u128::MAX);
    assert_eq!(
        assertion.checked_next().unwrap_err().counter(),
        Counter::AssertionId
    );

    let revision = RecordRevision::new(u64::MAX).unwrap();
    assert_eq!(
        revision.checked_next().unwrap_err().counter(),
        Counter::RecordRevision
    );

    let version = KbVersion::new(u64::MAX);
    assert_eq!(
        version.checked_next().unwrap_err().counter(),
        Counter::KbVersion
    );
}

#[test]
fn counters_advance_one_step_below_the_limit() {
    assert_eq!(
        TermId::new(u32::MAX - 1).checked_next().unwrap(),
        TermId::new(u32::MAX)
    );
    assert_eq!(
        KbVersion::new(41).checked_next().unwrap(),
        KbVersion::new(42)
    );
    assert_eq!(
        AssertionId::new(0).checked_next().unwrap(),
        AssertionId::new(1)
    );
}

#[test]
fn revision_zero_is_absence_not_a_revision() {
    // Zero would order below FIRST and let a stale baseline look "newer" than a
    // freshly created record, so the type refuses to represent it at all.
    assert!(RecordRevision::new(0).is_none());
    assert_eq!(RecordRevision::new(1).unwrap(), RecordRevision::FIRST);
    assert_eq!(RecordRevision::FIRST.checked_next().unwrap().get(), 2);
}

#[test]
fn version_distinguishes_pre_genesis_from_genesis() {
    assert!(!KbVersion::PRE_GENESIS.is_genesis_or_later());
    assert!(KbVersion::GENESIS.is_genesis_or_later());
    assert!(KbVersion::new(7).is_genesis_or_later());
}

#[test]
fn version_is_not_a_revision() {
    // Same numeric value, different meaning: these must stay separate types so a
    // caller cannot pass a KB version where a record revision is required.
    let version = KbVersion::new(3);
    let revision = RecordRevision::new(3).unwrap();
    assert_eq!(version.get(), revision.get());
    // Compile-time separation is the contract; this line documents it.
    // let _: KbVersion = revision;
}

#[test]
fn tightening_a_budget_takes_the_stricter_limit_per_field() {
    let server = Budget {
        max_steps: 100,
        max_work_rows: 50,
        max_work_bytes: 1000,
        max_rows: 10,
        max_output_bytes: 500,
        max_proof_nodes: 20,
    };
    let client = Budget {
        max_steps: 1000,       // wider: must not widen the server limit
        max_work_rows: 5,      // tighter: wins
        max_work_bytes: 1000,  // equal
        max_rows: 100,         // wider
        max_output_bytes: 100, // tighter: wins
        max_proof_nodes: 0,    // tightest possible
    };

    let effective = client.tightened_to(server);
    assert_eq!(effective.max_steps, 100);
    assert_eq!(effective.max_work_rows, 5);
    assert_eq!(effective.max_work_bytes, 1000);
    assert_eq!(effective.max_rows, 10);
    assert_eq!(effective.max_output_bytes, 100);
    assert_eq!(effective.max_proof_nodes, 0);
    assert_eq!(
        effective,
        server.tightened_to(client),
        "tightening is commutative"
    );
}

#[test]
fn meter_charges_each_limit_separately_and_refuses_to_overshoot() {
    let budget = Budget {
        max_steps: 3,
        max_work_rows: 2,
        max_work_bytes: 10,
        max_rows: 2,
        max_output_bytes: 10,
        max_proof_nodes: 1,
    };

    let mut meter = WorkMeter::new(budget);
    meter.charge_steps(2).unwrap();
    assert_eq!(meter.steps(), 2);
    assert_eq!(meter.charge_steps(2), Err(BudgetLimit::Steps));
    assert_eq!(meter.steps(), 2, "a refused charge must not be recorded");

    meter.reserve_bytes(4).unwrap();
    assert_eq!(meter.reserve_bytes(7), Err(BudgetLimit::WorkBytes));
    assert_eq!(meter.work_bytes(), 4);

    meter.charge_row().unwrap();
    meter.charge_row().unwrap();
    assert_eq!(meter.charge_row(), Err(BudgetLimit::WorkRows));

    meter.charge_proof_node().unwrap();
    assert_eq!(
        meter.charge_proof_node(),
        Err(BudgetLimit::ProofNodes),
        "a single-node arena cannot accept a second node"
    );
}

#[test]
fn step_charge_overflow_is_a_budget_failure_not_a_wrap() {
    let budget = Budget {
        max_steps: u64::MAX,
        ..Budget::DEFAULT
    };
    let mut meter = WorkMeter::new(budget);
    meter.charge_steps(u64::MAX).unwrap();
    // Adding one more would wrap to zero and silently look like "no work done".
    assert_eq!(meter.charge_steps(1), Err(BudgetLimit::Steps));
}
