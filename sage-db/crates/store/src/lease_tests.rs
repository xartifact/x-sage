//! Targeted tests for one attempt's reservation accounting.
//!
//! Four groups, each chosen because a plausible implementation gets it wrong: leaks
//! across the attempt boundary, reuse of released headroom, coherence of one state
//! transition, and the control signal.

use super::lease::{AttemptLedger, LeaseLedger, LedgerStats, TransferError};
use sage_db_types::ledger::{BudgetUnit, ControlStop, SharedLedger};
use sage_db_types::query::{QueryControl, RunToCompletion};

/// A control that stops immediately, so delegation is observable rather than assumed.
struct AlwaysStop;

impl QueryControl for AlwaysStop {
    fn checkpoint(&self) -> Result<(), ControlStop> {
        Err(ControlStop::Cancelled)
    }
}

fn live(l: &AttemptLedger<'_>) -> (u64, Option<u128>) {
    let s: LedgerStats = l.stats();
    (s.live_bytes, s.admitted_entries)
}

/// LEAK 1: untransferred credit comes back when the attempt owner dies, and issued
/// tokens survive it. This is the case v1 got wrong.
#[test]
fn dropping_the_attempt_releases_only_untransferred_credit() {
    let control = RunToCompletion;
    let (token, seen) = {
        let mut led = AttemptLedger::open(64, 8, &control).expect("counter allocates");
        led.try_charge(16).expect("admitted");
        let token = led.adopt_retained(8).expect("transfer");
        assert_eq!(live(&led), (16, Some(0)));
        let s = led.stats();
        (token, (s.live_bytes, s.held_bytes, s.untransferred_bytes))
    };
    assert_eq!(seen, (16, 8, 8), "16 live = 8 held + 8 untransferred");
    assert_eq!(token.bytes(), 8);
    let s = token.stats();
    assert_eq!(s.live_bytes, 8, "owner drop released the untransferred 8");
    assert_eq!(s.held_bytes, 8, "the issued token survived the owner");
    assert_eq!(s.untransferred_bytes, 0);
}

/// LEAK 2: entry charges are cumulative work. They are never returned, and no token
/// carries them — that is the agreed semantics, not a gap.
#[test]
fn entry_allowance_is_cumulative_work_not_a_lease() {
    let control = RunToCompletion;
    let mut led = AttemptLedger::open(64, 4, &control).expect("counter allocates");
    led.try_charge_entries(3).expect("admitted");
    assert_eq!(led.stats().admitted_entries, Some(3));
    // Work is not restored by releasing bytes.
    let token = led.reserve_owned(8).expect("fits");
    drop(token);
    assert_eq!(
        led.stats().admitted_entries,
        Some(3),
        "releasing a byte lease must not restore entry work"
    );
    let err = led
        .try_charge_entries(2)
        .expect_err("only one entry of work left");
    assert_eq!(err.unit(), BudgetUnit::Entries);
    assert_eq!(err.requested(), 2);
    assert_eq!(err.remaining(), 1);
}

/// REUSE 1: the byte ceiling bounds what is LIVE, so released capacity is usable again.
#[test]
fn released_headroom_is_reusable() {
    let control = RunToCompletion;
    let mut led = AttemptLedger::open(20, 4, &control).expect("counter allocates");
    for round in 0..5 {
        let token = led
            .reserve_owned(20)
            .unwrap_or_else(|_| panic!("round {round} should fit: capacity was released"));
        drop(token);
        let s = led.stats();
        assert_eq!(s.live_bytes, 0, "round {round}: nothing live after release");
        assert_eq!(
            s.admitted_bytes,
            Some(20 * (round + 1)),
            "cumulative work keeps counting while the ceiling does not"
        );
    }
}

/// REUSE 2: `try_charge`'s ceiling is also LIVE, not cumulative, measured on ONE facade.
///
/// The earlier version opened a fresh facade per round, which cannot distinguish a
/// live ceiling from a cumulative one — a cumulative ceiling would pass it. Here the
/// same attempt charges, transfers, drops the token, and charges again, so only a live
/// ceiling permits the third round.
#[test]
fn plain_charges_do_not_consume_the_ceiling_permanently() {
    let control = RunToCompletion;
    let mut led = AttemptLedger::open(10, 8, &control).expect("counter allocates");

    for round in 0..3 {
        led.try_charge(10)
            .unwrap_or_else(|_| panic!("round {round}: a live ceiling must allow this again"));
        assert_eq!(
            led.stats().live_bytes,
            10,
            "round {round}: the full live ceiling is in use"
        );
        // Release by transferring and dropping the token, which is the only release
        // path for admitted bytes.
        let token = led.adopt_retained(10).expect("transfer the charge");
        drop(token);
        assert_eq!(
            led.stats().live_bytes,
            0,
            "round {round}: the live ceiling is restored"
        );
        assert_eq!(
            led.stats().admitted_bytes,
            Some(10 * (round + 1)),
            "round {round}: cumulative work keeps counting; it is not the ceiling"
        );
    }
}

/// REUSE 3: the ceiling still refuses while capacity is live.
#[test]
fn the_ceiling_still_bounds_live_capacity() {
    let control = RunToCompletion;
    let mut led = AttemptLedger::open(20, 4, &control).expect("counter allocates");
    let token = led.reserve_owned(20).expect("exactly fits");
    let err = led.reserve_owned(1).expect_err("ceiling is live");
    assert_eq!(err.unit(), BudgetUnit::Bytes);
    assert_eq!(err.requested(), 1);
    assert_eq!(err.remaining(), 0);
    drop(token);
    led.reserve_owned(20)
        .expect("headroom restored after release");
}

/// ATOMICITY: a refusal moves nothing, on any path.
#[test]
fn refusals_leave_the_state_untouched() {
    let control = RunToCompletion;
    let mut led = AttemptLedger::open(10, 2, &control).expect("counter allocates");
    let before = led.stats();

    let err = led.reserve_owned(11).expect_err("does not fit");
    assert_eq!(err.unit(), BudgetUnit::Bytes);
    assert_eq!(led.stats(), before, "reserve refusal changed state");

    let err = led.try_charge(99).expect_err("does not fit");
    assert_eq!(err.unit(), BudgetUnit::Bytes);
    assert_eq!(led.stats(), before, "charge refusal changed state");

    let err = led.try_charge_entries(9).expect_err("does not fit");
    assert_eq!(err.unit(), BudgetUnit::Entries);
    assert_eq!(led.stats(), before, "entry refusal changed state");
}

/// CONTROL 1: transfer never admits, and never reports budget exhaustion.
#[test]
fn transfer_never_admits_and_never_reports_budget() {
    let control = RunToCompletion;
    let mut led = AttemptLedger::open(10, 4, &control).expect("counter allocates");
    led.try_charge(10).expect("admitted once");
    let admitted_before = led.stats().admitted_bytes;

    let token = led.adopt_retained(10).expect("the credit is there");
    assert_eq!(
        led.stats().admitted_bytes,
        admitted_before,
        "a transfer must not increase cumulative admissions"
    );

    let err = led.adopt_retained(10).expect_err("already transferred");
    assert_eq!(
        err,
        TransferError::Untransferred {
            requested_bytes: 10,
            available_bytes: 0,
        }
    );
    assert_eq!(led.stats().held_bytes, 10, "the refusal moved nothing");
    drop(token);
}

/// CONTROL 2: `checkpoint` delegates to the caller's control instead of claiming
/// success it cannot honour.
#[test]
fn checkpoint_delegates_to_the_borrowed_control() {
    let mut led = AttemptLedger::open(4, 1, &AlwaysStop).expect("counter allocates");
    assert_eq!(
        led.checkpoint(),
        Err(ControlStop::Cancelled),
        "a control that stops must be propagated, not overridden with Ok"
    );

    let never = RunToCompletion;
    let mut led2 = AttemptLedger::open(4, 1, &never).expect("counter allocates");
    assert_eq!(led2.checkpoint(), Ok(()));
}

/// CONTROL 3: one attempt cannot reach another attempt's untransferred credit.
#[test]
fn one_attempt_cannot_adopt_another_attempts_credit() {
    let control = RunToCompletion;
    let mut first = AttemptLedger::open(8, 2, &control).expect("counter allocates");
    first.try_charge(8).expect("first admits");
    let mut second = AttemptLedger::open(8, 2, &control).expect("counter allocates");
    let err = second
        .adopt_retained(8)
        .expect_err("second admitted nothing");
    assert_eq!(
        err,
        TransferError::Untransferred {
            requested_bytes: 8,
            available_bytes: 0,
        }
    );
    assert_eq!(
        first.stats().untransferred_bytes,
        8,
        "first keeps its credit"
    );
}
