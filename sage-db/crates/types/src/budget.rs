//! Deterministic step, work and proof budgets.
//!
//! Limits come from `docs/design/inference.md` §8 and are enforced by the
//! executor as it allocates and expands; they are never checked only after the
//! work is done. The server may tighten them per request, never loosen them.
//!
//! A budget is *not* a way to compare the two executors. The reference evaluator
//! saturates an ordered fact set while the optimized one runs threshold BFS, so
//! the same question legitimately costs a different number of steps in each. The
//! contract is that both charge their own work correctly and each refuses on its
//! own limit; semantic comparison happens only when both complete, and resource
//! refusal is verified per implementation. Requiring "both pass under the same
//! budget" would either hide a real overrun or force one algorithm to fake the
//! other's cost model.

use core::fmt;

/// Which limit stopped the work.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BudgetLimit {
    /// Adjacency or rule-instance scans.
    Steps,
    /// Rows held by the work area.
    WorkRows,
    /// Bytes held by the work area.
    WorkBytes,
    /// Nodes in the proof arena.
    ProofNodes,
}

impl BudgetLimit {
    /// Stable name for error payloads and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Steps => "max_steps",
            Self::WorkRows => "max_work_rows",
            Self::WorkBytes => "max_work_bytes",
            Self::ProofNodes => "max_proof_nodes",
        }
    }
}

impl fmt::Display for BudgetLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-request resource limits.
///
/// Two groups with different owners:
///
/// | field | bounds | enforced by |
/// |---|---|---|
/// | `max_steps`, `max_work_rows`, `max_work_bytes`, `max_proof_nodes` | the evaluation itself | the executor, as it works (`SourceExecutor::solve`) |
/// | `max_rows`, `max_output_bytes` | the response | the later query/display layer, **after** exact computation |
///
/// Conflating them would let a small response require an unbounded work area, or
/// let a display limit masquerade as "no such fact". The P0 executor returns the
/// complete fact set and ignores the display limits.
///
/// `max_rows` and `max_output_bytes` are carried but **not yet consumed**: P0 has
/// no query/display layer, so nothing enforces them and
/// [`BudgetLimit`] deliberately has no variant for them. That is the point — a P0
/// [`crate::query::QueryError::BudgetExceeded`] can only ever name an evaluation
/// limit, so a display budget cannot be reported as a reason an evaluation failed.
/// The later display layer adds its own truncation reporting.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Budget {
    /// Adjacency or rule-instance scans allowed.
    pub max_steps: u64,
    /// Rows the work area may hold.
    pub max_work_rows: u64,
    /// Bytes the work area may hold.
    pub max_work_bytes: u64,
    /// Rows the response may display.
    pub max_rows: u32,
    /// Encoded response bytes (bodies and proofs included).
    pub max_output_bytes: u64,
    /// Proof arena nodes.
    pub max_proof_nodes: u32,
}

impl Budget {
    /// Starting configuration from `docs/design/inference.md` §8.
    ///
    /// These are initial values to be re-measured against P0/P1 benchmarks, not
    /// published performance claims.
    pub const DEFAULT: Self = Self {
        max_steps: 1_000_000,
        max_work_rows: 200_000,
        max_work_bytes: 32 * 1024 * 1024,
        max_rows: 5_000,
        max_output_bytes: 16 * 1024 * 1024,
        max_proof_nodes: 20_000,
    };

    /// Return a copy whose every limit is at most `other`'s.
    ///
    /// This is how a client's request is combined with the server ceiling: the
    /// stricter value wins, so a client can tighten but never widen the budget.
    #[must_use]
    pub const fn tightened_to(self, other: Self) -> Self {
        Self {
            max_steps: min_u64(self.max_steps, other.max_steps),
            max_work_rows: min_u64(self.max_work_rows, other.max_work_rows),
            max_work_bytes: min_u64(self.max_work_bytes, other.max_work_bytes),
            max_rows: min_u32(self.max_rows, other.max_rows),
            max_output_bytes: min_u64(self.max_output_bytes, other.max_output_bytes),
            max_proof_nodes: min_u32(self.max_proof_nodes, other.max_proof_nodes),
        }
    }
}

const fn min_u64(left: u64, right: u64) -> u64 {
    if left < right { left } else { right }
}

const fn min_u32(left: u32, right: u32) -> u32 {
    if left < right { left } else { right }
}

/// Monotonic accounting for one inference call.
///
/// Executors charge the same *kinds* of event so a budget outcome describes the
/// work actually performed. They do not, and must not, arrive at the same totals:
/// different algorithms do different amounts of work. The meter performs no
/// inference and reads no graph.
#[derive(Debug)]
pub struct WorkMeter {
    budget: Budget,
    steps: u64,
    work_rows: u64,
    work_bytes: u64,
    proof_nodes: u32,
}

impl WorkMeter {
    /// Start metering against `budget`.
    #[must_use]
    pub const fn new(budget: Budget) -> Self {
        Self {
            budget,
            steps: 0,
            work_rows: 0,
            work_bytes: 0,
            proof_nodes: 0,
        }
    }

    /// The limits in force.
    #[must_use]
    pub const fn budget(&self) -> Budget {
        self.budget
    }

    /// Steps charged so far.
    #[must_use]
    pub const fn steps(&self) -> u64 {
        self.steps
    }

    /// Work rows materialised so far.
    ///
    /// Exposed so a result's [`crate::query::ExecStats`] can be read from the meter
    /// directly instead of re-counting rows alongside it — a second counter is one
    /// more thing that can silently disagree with the charged one.
    #[must_use]
    pub const fn work_rows(&self) -> u64 {
        self.work_rows
    }

    /// Work bytes reserved so far.
    #[must_use]
    pub const fn work_bytes(&self) -> u64 {
        self.work_bytes
    }

    /// Proof nodes allocated so far.
    #[must_use]
    pub const fn proof_nodes(&self) -> u32 {
        self.proof_nodes
    }

    /// Charge `count` scans.
    ///
    /// # Errors
    ///
    /// Fails with [`BudgetLimit::Steps`] when the total would exceed the limit.
    pub fn charge_steps(&mut self, count: u64) -> Result<(), BudgetLimit> {
        self.steps = self
            .steps
            .checked_add(count)
            .filter(|total| *total <= self.budget.max_steps)
            .ok_or(BudgetLimit::Steps)?;
        Ok(())
    }

    /// Charge one materialised work row.
    ///
    /// # Errors
    ///
    /// Fails with [`BudgetLimit::WorkRows`] when the total would exceed the limit.
    pub fn charge_row(&mut self) -> Result<(), BudgetLimit> {
        self.work_rows = self
            .work_rows
            .checked_add(1)
            .filter(|total| *total <= self.budget.max_work_rows)
            .ok_or(BudgetLimit::WorkRows)?;
        Ok(())
    }

    /// Reserve work-area bytes before allocating them.
    ///
    /// # Errors
    ///
    /// Fails with [`BudgetLimit::WorkBytes`] when the total would exceed the limit.
    pub fn reserve_bytes(&mut self, bytes: u64) -> Result<(), BudgetLimit> {
        self.work_bytes = self
            .work_bytes
            .checked_add(bytes)
            .filter(|total| *total <= self.budget.max_work_bytes)
            .ok_or(BudgetLimit::WorkBytes)?;
        Ok(())
    }

    /// Account for one allocated proof node.
    ///
    /// # Errors
    ///
    /// Fails with [`BudgetLimit::ProofNodes`] when the arena is full.
    pub fn charge_proof_node(&mut self) -> Result<(), BudgetLimit> {
        self.proof_nodes = self
            .proof_nodes
            .checked_add(1)
            .filter(|total| *total <= self.budget.max_proof_nodes)
            .ok_or(BudgetLimit::ProofNodes)?;
        Ok(())
    }
}
