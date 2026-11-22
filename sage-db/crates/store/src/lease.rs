//! Store-owned reservation accounting for one attempt.
//!
//! This module is the strong capability the store holds. It extends
//! [`SharedLedger`] without redeclaring any of its methods, so existing
//! implementations and call sites are untouched and `sage-db-types` gains no
//! dependency.
//!
//! # Two quantities that must not be confused
//!
//! * **Live byte ceiling** ([`AttemptLedger::open`]'s `limit_bytes`): bounds bytes
//!   admitted and not yet released. Releasing restores headroom.
//! * **Per-attempt entry-work allowance** (`limit_entries`): bounds cumulative entry
//!   charges for this attempt. It is *work* — a traversal looked at N things — not
//!   retained storage, so nothing returns it and no token carries it. Confusing the
//!   two would let a work allowance masquerade as a lease.
//!
//! Cumulative work is reported separately as `admitted_*` and is never a ceiling.
//!
//! # One state transition per observation
//!
//! Every check, admission, transfer and release reads and writes the whole accounting
//! state under a single lock, so no reader can see `live != untransferred + held`. The
//! phantom-credit and wrap failures independent atomics allow are unrepresentable
//! here rather than clamped.
//!
//! # No shared facade, no early release
//!
//! Each [`AttemptLedger`] owns its own counter, so no constructor lets one attempt
//! adopt another's untransferred credit. Untransferred credit is returned only by the
//! attempt owner's `Drop`; there is deliberately no public release method, because one
//! would let a caller free accounting while the scratch it describes still lives.

use core::fmt;
use std::sync::{Mutex, MutexGuard};

use sage_db_types::ledger::{BudgetError, BudgetUnit, ControlStop, SharedLedger};
use sage_db_types::query::QueryControl;
use triomphe::Arc;

/// Fallible, because the counter's own allocation bootstraps admission: a store that
/// cannot allocate the counter must refuse to open rather than abort.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LedgerOpenError {
    /// The shared counter could not be allocated.
    Allocation,
}

impl fmt::Display for LedgerOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("lease ledger open refused: counter allocation failed")
    }
}

impl std::error::Error for LedgerOpenError {}

/// Why an ownership transfer was refused.
///
/// Deliberately **not** a budget variant. A transfer shortfall means "this attempt
/// never admitted that many untransferred bytes", which no larger allowance fixes;
/// reporting it as budget exhaustion would send an operator to retry something that
/// cannot succeed. New admission is the only path that refuses for budget, and it
/// reports [`BudgetError`] directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransferError {
    /// Not enough untransferred bytes in this attempt.
    Untransferred {
        /// Bytes the caller asked to take ownership of.
        requested_bytes: u64,
        /// Untransferred bytes this attempt actually holds.
        available_bytes: u64,
    },
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Untransferred {
                requested_bytes,
                available_bytes,
            } => write!(
                f,
                "ownership transfer refused: {requested_bytes} bytes requested, \
                 {available_bytes} untransferred"
            ),
        }
    }
}

impl std::error::Error for TransferError {}

/// One counter's accounting, for reporting and for tests that must observe it after
/// an owner has been dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LedgerStats {
    /// Ceiling for live bytes.
    pub limit_bytes: u64,
    /// Per-attempt allowance for cumulative entry work.
    pub limit_entries: u64,
    /// Bytes admitted and not yet released.
    pub live_bytes: u64,
    /// Live bytes this attempt holds and has not transferred.
    pub untransferred_bytes: u64,
    /// Live bytes owned by tokens that may outlive this attempt.
    pub held_bytes: u64,
    /// Cumulative entry charges over this attempt. Never a ceiling.
    ///
    /// `None` means the cumulative figure is **not representable** — the diagnostic
    /// exhausted, not the allowance. It is deliberately not `0`, because reporting a
    /// wrapped zero would read as "no work was done".
    pub admitted_entries: Option<u128>,
    /// Cumulative bytes admitted over this attempt. Never a ceiling.
    ///
    /// `None` means the same thing: the diagnostic is exhausted while the governed
    /// `u64` live state may still be perfectly valid. Never rendered as zero.
    pub admitted_bytes: Option<u128>,
}

/// The whole accounting state: scalars, so one guard covers one transition.
#[derive(Debug)]
struct State {
    limit_bytes: u64,
    limit_entries: u64,
    live_bytes: u64,
    untransferred_bytes: u64,
    held_bytes: u64,
    /// Cumulative diagnostic. Wider than the allowance it describes.
    admitted_bytes: Option<u128>,
    /// Cumulative diagnostic. Wider than the allowance it describes.
    admitted_entries: Option<u128>,
    /// Allowance actually consumed, in the allowance's own width, so the check is exact.
    spent_entries: u64,
}

impl State {
    /// The invariant this module exists to keep. A release that would break it is a
    /// bug, not a state to clamp.
    fn consistent(&self) -> bool {
        self.live_bytes == self.untransferred_bytes + self.held_bytes
    }

    /// Cumulative byte update.
    ///
    /// The sweep is fallible: the number of charges is not bounded by anything in this
    /// API, so a `u128` total **can** be exhausted even though each addend is `u64`.
    /// Checked, and an exhausted diagnostic becomes `None` rather than a panic, a wrap,
    /// or a forged refusal. Live state is untouched by this and stays exact.
    fn add_admitted_bytes(&mut self, bytes: u64) {
        self.admitted_bytes = self
            .admitted_bytes
            .and_then(|total| total.checked_add(u128::from(bytes)));
    }

    /// Cumulative entry update, same fallible sweep.
    fn add_admitted_entries(&mut self, count: u64) {
        self.admitted_entries = self
            .admitted_entries
            .and_then(|total| total.checked_add(u128::from(count)));
        self.spent_entries += count;
    }

    const fn stats(&self) -> LedgerStats {
        LedgerStats {
            limit_bytes: self.limit_bytes,
            limit_entries: self.limit_entries,
            live_bytes: self.live_bytes,
            untransferred_bytes: self.untransferred_bytes,
            held_bytes: self.held_bytes,
            admitted_entries: self.admitted_entries,
            admitted_bytes: self.admitted_bytes,
        }
    }
}

/// The counter handle shared by one [`AttemptLedger`] and its tokens.
#[derive(Debug)]
struct Counter(Arc<Mutex<State>>);

impl Counter {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic elsewhere must not turn accounting into a poison error callers
        // cannot express: the state is plain scalars, so recovering it is sound.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn stats(&self) -> LedgerStats {
        self.lock().stats()
    }
}

/// A move-only owned reservation over part of one counter.
///
/// The extent is inline and the counter is an owned reference, so release follows
/// from Rust ownership: there is no `Clone`, therefore exactly one `Drop`, which may
/// happen long after the [`AttemptLedger`] that minted it is gone.
#[derive(Debug)]
pub struct OwnedReservation {
    counter: Counter,
    bytes: u64,
}

impl OwnedReservation {
    /// Bytes this lease owns.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// State of the counter this lease releases against.
    #[must_use]
    pub fn stats(&self) -> LedgerStats {
        self.counter.stats()
    }
}

impl Drop for OwnedReservation {
    fn drop(&mut self) {
        let mut state = self.counter.lock();
        state.held_bytes -= self.bytes;
        state.live_bytes -= self.bytes;
        debug_assert!(
            state.consistent(),
            "lease release broke the accounting invariant"
        );
    }
}

/// The strong capability: the canonical ledger plus ownership operations.
///
/// `try_charge`, `try_charge_entries` and `checkpoint` are inherited unchanged, so an
/// existing `&mut dyn SharedLedger` callback keeps working when handed this object and
/// no other implementation has to learn a new method.
pub trait LeaseLedger: SharedLedger {
    /// Admit fresh capacity and hand back an owned lease over it.
    ///
    /// # Errors
    ///
    /// [`BudgetError`] naming the unit that did not fit. Nothing is admitted when this
    /// refuses.
    fn reserve_owned(&mut self, bytes: u64) -> Result<OwnedReservation, BudgetError>;

    /// Take ownership of capacity this attempt already holds.
    ///
    /// A transfer never admits, so it cannot charge twice, and it cannot report budget
    /// exhaustion — see [`TransferError`].
    ///
    /// # Errors
    ///
    /// [`TransferError`] when this attempt does not hold that much untransferred
    /// credit. Nothing moves when this refuses.
    fn adopt_retained(&mut self, bytes: u64) -> Result<OwnedReservation, TransferError>;
}

/// Accounting for exactly one attempt or one open.
///
/// The facade's lifetime is the attempt's, not the KB's. Dropping it returns the credit
/// this attempt still holds untransferred; tokens already issued keep the counter alive
/// and release their own extent when they go.
pub struct AttemptLedger<'c> {
    counter: Counter,
    control: &'c dyn QueryControl,
}

impl<'c> AttemptLedger<'c> {
    /// Open accounting for one attempt.
    ///
    /// `limit_bytes` is the **live** byte ceiling and is restored as leases are
    /// released. `limit_entries` is this attempt's **cumulative entry-work**
    /// allowance and is not restored. `control` is the caller's real cancellation
    /// policy; it is borrowed, never copied, and it is consulted by
    /// [`SharedLedger::checkpoint`].
    ///
    /// # Errors
    ///
    /// [`LedgerOpenError::Allocation`] when the counter cannot be allocated.
    pub fn open(
        limit_bytes: u64,
        limit_entries: u64,
        control: &'c dyn QueryControl,
    ) -> Result<Self, LedgerOpenError> {
        let state = State {
            limit_bytes,
            limit_entries,
            live_bytes: 0,
            untransferred_bytes: 0,
            held_bytes: 0,
            admitted_bytes: Some(0),
            admitted_entries: Some(0),
            spent_entries: 0,
        };
        let counter = Arc::try_new(Mutex::new(state)).map_err(|_| LedgerOpenError::Allocation)?;
        Ok(Self {
            counter: Counter(counter),
            control,
        })
    }

    /// State of this attempt's counter.
    #[must_use]
    pub fn stats(&self) -> LedgerStats {
        self.counter.stats()
    }

    fn mint(&self, bytes: u64) -> OwnedReservation {
        OwnedReservation {
            counter: Counter(Arc::clone(&self.counter.0)),
            bytes,
        }
    }
}

impl fmt::Debug for AttemptLedger<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttemptLedger")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl SharedLedger for AttemptLedger<'_> {
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
        let mut state = self.counter.lock();
        let room = state
            .limit_bytes
            .checked_sub(state.live_bytes)
            .expect("live_bytes never exceeds limit_bytes");
        if bytes > room {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, room));
        }
        // Cumulative work is a diagnostic: after headroom is reused it can exceed
        // `limit_bytes` and still be a valid figure. Adding it here must never be the
        // thing that decides whether this charge is allowed.
        state.add_admitted_bytes(bytes);
        state.live_bytes += bytes;
        state.untransferred_bytes += bytes;
        debug_assert!(state.consistent());
        Ok(())
    }

    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
        let mut state = self.counter.lock();
        // Entry work is cumulative for the attempt: no release path exists because none
        // is meant to. This is the allowance named at open, not a lease.
        let room = state
            .limit_entries
            .checked_sub(state.spent_entries)
            .expect("spent_entries never exceeds limit_entries");
        if count > room {
            return Err(BudgetError::new(BudgetUnit::Entries, count, room));
        }
        // The allowance check above already bounds `count`, so "allowance fits but the
        // word does not" is unreachable: count <= room <= limit_entries - admitted, and
        // admitted <= limit_entries is maintained, so the sum stays within limit_entries.
        state.add_admitted_entries(count);
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<(), ControlStop> {
        // Delegate to the caller's real control, and take NO lock: caller code must not
        // run inside the guard, where a panic would poison a half-visible transition.
        self.control.checkpoint()
    }
}

impl LeaseLedger for AttemptLedger<'_> {
    fn reserve_owned(&mut self, bytes: u64) -> Result<OwnedReservation, BudgetError> {
        let mut state = self.counter.lock();
        let room = state
            .limit_bytes
            .checked_sub(state.live_bytes)
            .expect("live_bytes never exceeds limit_bytes");
        if bytes > room {
            return Err(BudgetError::new(BudgetUnit::Bytes, bytes, room));
        }
        state.add_admitted_bytes(bytes);
        state.live_bytes += bytes;
        state.held_bytes += bytes;
        debug_assert!(state.consistent());
        drop(state);
        Ok(self.mint(bytes))
    }

    fn adopt_retained(&mut self, bytes: u64) -> Result<OwnedReservation, TransferError> {
        let mut state = self.counter.lock();
        if bytes > state.untransferred_bytes {
            return Err(TransferError::Untransferred {
                requested_bytes: bytes,
                available_bytes: state.untransferred_bytes,
            });
        }
        state.untransferred_bytes -= bytes;
        state.held_bytes += bytes;
        debug_assert!(state.consistent());
        drop(state);
        Ok(self.mint(bytes))
    }
}

impl Drop for AttemptLedger<'_> {
    fn drop(&mut self) {
        // Only what this attempt never transferred. Issued tokens keep their own extent
        // and their own reference to the counter.
        let mut state = self.counter.lock();
        state.live_bytes -= state.untransferred_bytes;
        state.untransferred_bytes = 0;
        debug_assert!(
            state.consistent(),
            "attempt drop broke the accounting invariant"
        );
    }
}

#[cfg(test)]
mod diagnostic_overflow {
    //! Drives the production cumulative-arithmetic helper to its representable limit.
    //!
    //! This lives inside the module because `State` is private, and it deliberately
    //! calls `State::add_admitted_bytes` rather than re-implementing the arithmetic: a
    //! test that computes its own `checked_add` proves nothing about production code.

    use super::State;

    fn state_at(admitted: Option<u128>) -> State {
        State {
            limit_bytes: 8,
            limit_entries: 1,
            live_bytes: 0,
            untransferred_bytes: 0,
            held_bytes: 0,
            admitted_bytes: admitted,
            admitted_entries: Some(0),
            spent_entries: 0,
        }
    }

    /// A production add at the representable limit yields `None` — not a panic, not a
    /// wrapped zero.
    #[test]
    fn production_add_reports_exhaustion_as_none() {
        let mut state = state_at(Some(u128::MAX));
        state.add_admitted_bytes(1);
        assert_eq!(
            state.admitted_bytes, None,
            "exhaustion must be None, never a wrapped number"
        );
    }

    /// `None` is sticky: a later add cannot resurrect a plausible-looking total.
    #[test]
    fn exhaustion_is_sticky() {
        let mut state = state_at(None);
        state.add_admitted_bytes(1);
        assert_eq!(state.admitted_bytes, None, "None must not become Some(1)");
        state.add_admitted_bytes(0);
        assert_eq!(state.admitted_bytes, None);
    }

    /// Live accounting is untouched by diagnostic exhaustion and stays exact.
    #[test]
    fn live_accounting_stays_exact_when_the_diagnostic_is_gone() {
        let mut state = state_at(Some(u128::MAX));
        state.add_admitted_bytes(1);
        assert_eq!(state.admitted_bytes, None);

        // The governed quantities are independent of the diagnostic.
        state.live_bytes += 8;
        state.untransferred_bytes += 8;
        assert!(
            state.consistent(),
            "live == untransferred + held still holds"
        );
        assert_eq!(state.live_bytes, 8);
        assert_eq!(state.untransferred_bytes, 8);

        // And the diagnostic still refuses to invent a total.
        state.add_admitted_bytes(0);
        assert_eq!(state.admitted_bytes, None);
    }

    /// Below the limit the production helper still accumulates normally, so the test
    /// above is not passing merely because the helper is inert.
    #[test]
    fn production_add_accumulates_below_the_limit() {
        let mut state = state_at(Some(0));
        state.add_admitted_bytes(5);
        state.add_admitted_bytes(7);
        assert_eq!(state.admitted_bytes, Some(12));
    }
}
