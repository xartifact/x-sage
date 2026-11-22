//! Shared resource ledger and typed refusals for the storage layers.
//!
//! This is the **single** ledger definition used by `sage-db-format` and
//! `sage-db-store`. Both directions of drift are excluded by design:
//!
//! * it is not the P0 inference budget ([`crate::budget::Budget`]), which bounds
//!   evaluation steps/rows/bytes for one query, not a commit or an open;
//! * it is not a second trait invented by one owner: the accepted S2 seam and the
//!   accepted D10 format contract both bind to these names.
//!
//! Two units are deliberately separate. A directory entry is not a byte; charging
//! `count × ENTRY_COST_BYTES` through a byte ledger puts a fabricated number in a
//! field that is otherwise measured, and every later report inherits the lie.
//!
//! Reservation accounting here is **not** memory. `admitted` is cumulative reserved
//! bytes, `outstanding` is reserved-but-not-yet-released; neither is peak RSS or
//! allocated bytes, and no report may present them as such.

use core::fmt;

/// The unit a refusal was measured in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BudgetUnit {
    /// Bytes admitted for a buffer, a decode or a write.
    Bytes,
    /// Directory entries or counted elements admitted for a traversal.
    Entries,
}

impl BudgetUnit {
    /// Stable name for logs and typed error payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::Entries => "entries",
        }
    }
}

impl fmt::Display for BudgetUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A refusal from the shared ledger.
///
/// This is **not** a data verdict. A budget refusal means "this request's
/// remaining allowance was too small"; the same input can succeed with a larger
/// budget. It must never be reported as corruption, because the operator action
/// differs: retry with budget versus stop the KB and restore.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BudgetError {
    unit: BudgetUnit,
    requested: u64,
    remaining: u64,
}

impl BudgetError {
    /// Construct a refusal for `requested` exceeding `remaining` in `unit`.
    #[must_use]
    pub const fn new(unit: BudgetUnit, requested: u64, remaining: u64) -> Self {
        Self {
            unit,
            requested,
            remaining,
        }
    }

    /// The unit the request was measured in.
    #[must_use]
    pub const fn unit(self) -> BudgetUnit {
        self.unit
    }

    /// How much was asked for.
    #[must_use]
    pub const fn requested(self) -> u64 {
        self.requested
    }

    /// How much remained when the request was refused.
    #[must_use]
    pub const fn remaining(self) -> u64 {
        self.remaining
    }
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "budget refused: {} {} requested, {} remaining",
            self.requested, self.unit, self.remaining
        )
    }
}

impl std::error::Error for BudgetError {}

// Cancellation is NOT redefined here. P0 already declares it in
// [`crate::query::ControlStop`] (Cancelled | DeadlineExceeded), and two structurally
// identical enums would make `?` on one fail against the other at every call site.
// The storage layer re-exports the P0 type so there is exactly one definition.
pub use crate::query::ControlStop;

/// The one shared ledger.
///
/// One instance spans a whole attempt or a whole open/resolution: decoding,
/// hashing, filesystem buffers, sinks and digests all charge the same object. No
/// phase creates its own counter, and nothing resets between phases.
///
/// Every implementation must admit **before** acquiring the resource, so a refusal
/// costs neither I/O nor an allocation.
pub trait SharedLedger {
    /// Admit `bytes`, or refuse.
    ///
    /// # Errors
    ///
    /// [`BudgetError`] with [`BudgetUnit::Bytes`] when the request does not fit.
    fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError>;

    /// Admit `count` entries or elements, or refuse.
    ///
    /// # Errors
    ///
    /// [`BudgetError`] with [`BudgetUnit::Entries`] when the request does not fit.
    fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError>;

    /// Ask whether the caller wants this operation to continue.
    ///
    /// # Errors
    ///
    /// [`ControlStop`] when the caller's policy says stop. This is not a resource
    /// refusal and must be reported separately from one.
    fn checkpoint(&mut self) -> Result<(), ControlStop>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        bytes: u64,
        entries: u64,
    }

    impl SharedLedger for Fixed {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            if bytes > self.bytes {
                return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.bytes));
            }
            self.bytes -= bytes;
            Ok(())
        }

        fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
            if count > self.entries {
                return Err(BudgetError::new(BudgetUnit::Entries, count, self.entries));
            }
            self.entries -= count;
            Ok(())
        }

        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            Ok(())
        }
    }

    #[test]
    fn bytes_and_entries_are_independent_allowances() {
        let mut led = Fixed {
            bytes: 0,
            entries: 4,
        };
        assert_eq!(led.try_charge_entries(4), Ok(()));
        let err = led.try_charge(1).expect_err("no byte allowance");
        assert_eq!(err.unit(), BudgetUnit::Bytes);
        assert_eq!(err.requested(), 1);
        assert_eq!(err.remaining(), 0);
    }

    #[test]
    fn a_refusal_names_the_unit_it_was_measured_in() {
        let mut led = Fixed {
            bytes: 8,
            entries: 0,
        };
        assert_eq!(led.try_charge(8), Ok(()));
        let err = led
            .try_charge_entries(1)
            .expect_err("no entry allowance left");
        assert_eq!(err.unit(), BudgetUnit::Entries);
        assert_eq!(
            err.to_string(),
            "budget refused: 1 entries requested, 0 remaining"
        );
    }

    #[test]
    fn refusals_are_not_control_stops() {
        // Distinct types on purpose: mapping one onto the other would let a retry
        // read as a cancellation, or a cancellation read as a quota problem.
        let budget: BudgetError = BudgetError::new(BudgetUnit::Bytes, 2, 1);
        let stop = ControlStop::Cancelled;
        assert_ne!(budget.to_string(), stop.to_string());
        assert_eq!(stop.as_str(), "cancelled");
    }
}
