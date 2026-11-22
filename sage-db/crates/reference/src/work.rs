//! Query-scoped resource accounting for the reference evaluator.
//!
//! One [`Work`] covers seeding, saturation, plan relaxation and proof
//! construction, so no phase can spend a full allowance while another is live.
//! The reference has its own copy of this plumbing on purpose: it may not import
//! `sage-db-infer`'s meter, and a shared one would make the two implementations
//! able to hide a cost model behind a common helper.
//!
//! Cancellation is checked on entry, at bounded step intervals and again before
//! any result is returned.

use sage_db_types::{Budget, BudgetLimit, ExecStats, QueryControl, QueryError, WorkMeter};

/// Steps between mandatory control checks.
const CHECK_INTERVAL: u64 = sage_db_types::CONTROL_CHECK_INTERVAL_STEPS;

/// Accounting for one evaluation.
#[derive(Debug)]
pub struct Work {
    meter: WorkMeter,
    since_check: u64,
}

impl Work {
    /// Start accounting against `budget`.
    pub const fn new(budget: Budget) -> Self {
        Self {
            meter: WorkMeter::new(budget),
            since_check: 0,
        }
    }

    /// Check cancellation before doing anything else.
    pub fn check_entry(&self, control: &dyn QueryControl) -> Result<(), QueryError> {
        control.checkpoint().map_err(QueryError::Stopped)
    }

    /// Check cancellation again before returning success.
    pub fn check_exit(&self, control: &dyn QueryControl) -> Result<(), QueryError> {
        control.checkpoint().map_err(QueryError::Stopped)
    }

    /// Charge `count` scans.
    pub fn step(&mut self, count: u64) -> Result<(), QueryError> {
        self.meter
            .charge_steps(count)
            .map_err(QueryError::BudgetExceeded)?;
        self.since_check = self.since_check.saturating_add(count);
        Ok(())
    }

    /// Charge one live fact or result row.
    pub fn row(&mut self) -> Result<(), QueryError> {
        self.meter
            .charge_row()
            .map_err(QueryError::BudgetExceeded)?;
        Ok(())
    }

    /// Reserve working bytes before they are allocated.
    pub fn bytes(&mut self, bytes: u64) -> Result<(), QueryError> {
        self.meter
            .reserve_bytes(bytes)
            .map_err(QueryError::BudgetExceeded)?;
        Ok(())
    }

    /// Account for one allocated proof node.
    pub fn proof_node(&mut self) -> Result<(), QueryError> {
        self.meter
            .charge_proof_node()
            .map_err(QueryError::BudgetExceeded)?;
        Ok(())
    }

    /// Check control if the step interval elapsed.
    pub fn periodic_check(&mut self, control: &dyn QueryControl) -> Result<(), QueryError> {
        if self.since_check >= CHECK_INTERVAL {
            self.since_check = 0;
            control.checkpoint().map_err(QueryError::Stopped)?;
        }
        Ok(())
    }

    /// The limits in force.
    pub fn budget(&self) -> Budget {
        self.meter.budget()
    }

    /// Bytes reserved so far.
    ///
    /// Exposed so a sub-budget can be derived for one structure: every work area
    /// shares one `max_work_bytes`, so a structure's ceiling is the budget minus what
    /// the others already hold.
    pub fn work_bytes(&self) -> u64 {
        self.meter.work_bytes()
    }

    /// Bytes still available in the shared work area.
    pub fn remaining_bytes(&self) -> u64 {
        self.meter
            .budget()
            .max_work_bytes
            .saturating_sub(self.meter.work_bytes())
    }

    /// Provenance for a result.
    pub fn stats(&self) -> ExecStats {
        ExecStats {
            steps: self.meter.steps(),
            work_rows: self.meter.work_rows(),
            work_bytes: self.meter.work_bytes(),
            proof_nodes: self.meter.proof_nodes(),
        }
    }
}

/// Reserve vector capacity without aborting on allocation failure.
pub fn reserve<T>(
    vector: &mut Vec<T>,
    additional: usize,
    work: &mut Work,
) -> Result<(), QueryError> {
    let bytes = (additional as u64).saturating_mul(core::mem::size_of::<T>() as u64);
    work.bytes(bytes)?;
    vector
        .try_reserve_exact(additional)
        .map_err(|_| QueryError::BudgetExceeded(BudgetLimit::WorkBytes))
}

/// Checked `usize -> u32` conversion for graph positions.
pub fn narrow(value: usize) -> Result<u32, QueryError> {
    u32::try_from(value).map_err(|_| QueryError::BudgetExceeded(BudgetLimit::WorkRows))
}

/// Copy `additional` elements' worth of bytes into the meter.
pub fn charge_copy<T>(additional: usize, work: &mut Work) -> Result<(), QueryError> {
    work.bytes((additional as u64).saturating_mul(core::mem::size_of::<T>() as u64))
}
