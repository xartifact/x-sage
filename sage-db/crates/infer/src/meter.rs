//! Shared work accounting for every phase of one query.
//!
//! One [`Work`] instance covers setup, validation, aggregation, BFS, proof
//! construction and result assembly. A per-container budget would let each spend a
//! full `max_work_bytes` while the process holds all of them at once, so nothing here
//! grants a container its own allowance.
//!
//! # Capacity, not length
//!
//! [`grow`] charges the **capacity delta** of a vector, because capacity is what the
//! allocator keeps alive. Charging logical length would understate a vector that grew
//! by doubling; charging a nominal size for a vector that is never filled would be
//! worse. Every derived structure in this crate is reserved once, at its known bound,
//! and charged for exactly that.
//!
//! # Cancellation
//!
//! Checked on entry, at bounded step intervals during work, and again before a
//! successful return. Step charges are the clock: this layer never reads one.

use sage_db_types::{Budget, BudgetLimit, QueryControl, QueryError, WorkMeter};

/// Steps between mandatory control checks.
const CHECK_INTERVAL: u64 = sage_db_types::CONTROL_CHECK_INTERVAL_STEPS;

/// Query-scoped resource accounting.
#[derive(Debug)]
pub struct Work {
    meter: WorkMeter,
    since_check: u64,
}

impl Work {
    /// Start accounting against `budget`.
    #[must_use]
    pub const fn new(budget: Budget) -> Self {
        Self {
            meter: WorkMeter::new(budget),
            since_check: 0,
        }
    }

    /// Limits in force.
    #[must_use]
    pub fn budget(&self) -> Budget {
        self.meter.budget()
    }

    /// Check cancellation before doing anything else.
    ///
    /// # Errors
    ///
    /// Propagates the control stop.
    pub fn check_entry(&self, control: &dyn QueryControl) -> Result<(), QueryError> {
        control.checkpoint().map_err(QueryError::Stopped)
    }

    /// Check cancellation again before returning success.
    ///
    /// # Errors
    ///
    /// Propagates the control stop.
    pub fn check_exit(&self, control: &dyn QueryControl) -> Result<(), QueryError> {
        control.checkpoint().map_err(QueryError::Stopped)
    }

    /// Charge `count` adjacency or rule-instance scans.
    ///
    /// # Errors
    ///
    /// Fails with [`BudgetLimit::Steps`]; the charge is not recorded.
    pub fn step(&mut self, count: u64) -> Result<(), QueryError> {
        self.meter
            .charge_steps(count)
            .map_err(QueryError::BudgetExceeded)?;
        self.since_check += count;
        Ok(())
    }

    /// Charge one live work row.
    ///
    /// # Errors
    ///
    /// Fails with [`BudgetLimit::WorkRows`].
    pub fn row(&mut self) -> Result<(), QueryError> {
        self.meter
            .charge_row()
            .map_err(QueryError::BudgetExceeded)?;
        Ok(())
    }

    /// Reserve `bytes` of working memory.
    ///
    /// # Errors
    ///
    /// Fails with [`BudgetLimit::WorkBytes`].
    pub fn bytes(&mut self, bytes: u64) -> Result<(), QueryError> {
        self.meter
            .reserve_bytes(bytes)
            .map_err(QueryError::BudgetExceeded)?;
        Ok(())
    }

    /// Account for one allocated proof slot.
    ///
    /// # Errors
    ///
    /// Fails with [`BudgetLimit::ProofNodes`].
    pub fn proof_node(&mut self) -> Result<(), QueryError> {
        self.meter
            .charge_proof_node()
            .map_err(QueryError::BudgetExceeded)?;
        Ok(())
    }

    /// Check control if the step interval has elapsed.
    ///
    /// # Errors
    ///
    /// Propagates the control stop.
    pub fn periodic_check(&mut self, control: &dyn QueryControl) -> Result<(), QueryError> {
        if self.since_check >= CHECK_INTERVAL {
            self.since_check = 0;
            control.checkpoint().map_err(QueryError::Stopped)?;
        }
        Ok(())
    }

    /// Unconditionally consult control. Used at phase boundaries.
    ///
    /// # Errors
    ///
    /// Propagates the control stop.
    pub fn check(&mut self, control: &dyn QueryControl) -> Result<(), QueryError> {
        self.since_check = 0;
        control.checkpoint().map_err(QueryError::Stopped)
    }

    /// Working bytes reserved so far.
    #[must_use]
    pub fn work_bytes(&self) -> u64 {
        self.meter.work_bytes()
    }

    /// Steps charged so far.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.meter.steps()
    }

    /// Rows charged so far.
    #[must_use]
    pub fn work_rows(&self) -> u64 {
        self.meter.work_rows()
    }

    /// Proof nodes charged so far.
    #[must_use]
    pub fn proof_nodes(&self) -> u32 {
        self.meter.proof_nodes()
    }

    /// Working bytes still available to this query.
    ///
    /// This is the number a sub-budget must be derived from: a container taking
    /// `max_work_bytes` for itself would double-spend the same memory.
    #[must_use]
    pub fn remaining_bytes(&self) -> u64 {
        self.budget()
            .max_work_bytes
            .saturating_sub(self.meter.work_bytes())
    }
}

/// Grow `vector` to hold at least `needed` elements, charging the capacity delta.
///
/// Returns the new capacity so callers can record exact bounds. `needed` is the total
/// element count required, not an increment.
///
/// # Errors
///
/// Fails with [`BudgetLimit::WorkBytes`] when the delta would exceed the budget, or
/// when the allocator refuses.
pub fn grow<T>(vector: &mut Vec<T>, needed: usize, work: &mut Work) -> Result<usize, QueryError> {
    if needed <= vector.capacity() {
        return Ok(vector.capacity());
    }
    let element = core::mem::size_of::<T>();
    // Reserve exactly: `try_reserve_exact` makes capacity predictable, so the charge
    // is the true delta rather than a doubling guess.
    let additional = needed.saturating_sub(vector.len());
    let delta = (needed.saturating_sub(vector.capacity())).saturating_mul(element);
    work.bytes(delta as u64)?;
    vector
        .try_reserve_exact(additional)
        .map_err(|_| QueryError::BudgetExceeded(BudgetLimit::WorkBytes))?;
    Ok(vector.capacity())
}

/// Charge for a fixed-size allocation of `count` elements of `T`.
///
/// # Errors
///
/// Fails with [`BudgetLimit::WorkBytes`].
pub fn charge_slots<T>(count: usize, work: &mut Work) -> Result<(), QueryError> {
    let bytes = (count as u64).saturating_mul(core::mem::size_of::<T>() as u64);
    work.bytes(bytes)
}

/// Charge `count` rows and their growth.
///
/// # Errors
///
/// Fails with [`BudgetLimit::WorkRows`].
pub fn charge_rows(count: usize, work: &mut Work) -> Result<(), QueryError> {
    for _ in 0..count {
        work.row()?;
    }
    Ok(())
}

/// A queue whose backing storage is charged on growth.
///
/// `VecDeque::push_back` can reallocate outside any budget. This keeps a `Vec` with a
/// head cursor instead: growth goes through [`grow`], so every allocation is both
/// fallible and charged. `T: Copy` matches both uses in this crate (term ids and
/// indices), and avoids an interior `Option` that would double the charged size.
#[derive(Debug)]
pub struct BoundedQueue<T: Copy> {
    items: Vec<T>,
    head: usize,
}

impl<T: Copy> BoundedQueue<T> {
    /// Empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: Vec::new(),
            head: 0,
        }
    }

    /// Push one item, charging any growth.
    ///
    /// # Errors
    ///
    /// Fails with [`BudgetLimit::WorkBytes`] when growth would exceed the budget.
    pub fn push(&mut self, item: T, work: &mut Work) -> Result<(), QueryError> {
        let needed = self.items.len() + 1;
        if needed > self.items.capacity() {
            crate::meter::grow(&mut self.items, needed, work)?;
        }
        self.items.push(item);
        Ok(())
    }

    /// Pop the oldest item.
    pub fn pop(&mut self) -> Option<T> {
        if self.head >= self.items.len() {
            return None;
        }
        let item = self.items[self.head];
        self.head += 1;
        if self.head == self.items.len() {
            self.items.clear();
            self.head = 0;
        }
        Some(item)
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.head >= self.items.len()
    }
}

impl<T: Copy> Default for BoundedQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Checked `usize -> u32` conversion for values that index positions.
///
/// # Errors
///
/// Returns [`BudgetLimit::WorkRows`] rather than saturating onto another record.
pub fn narrow(value: usize) -> Result<u32, QueryError> {
    u32::try_from(value).map_err(|_| QueryError::BudgetExceeded(BudgetLimit::WorkRows))
}
