//! Complete `depends-on` prerequisite query with a dependent-last order.
//!
//! # What this is
//!
//! One evaluation of `source depends-on ?` on a [`SnapshotView`], returning the existing [`Answer`]
//! (every row, annotation, asserted/derived/disputed flag, strongest support and proof root intact)
//! plus the reachable prerequisites in **dependent-last** order: if `B` depends on `C`, then `C`
//! precedes `B`.
//!
//! # What it is not
//!
//! * **No second reach evaluator and no rule switch.** Rows come from the same optimized executor
//!   every fact query uses, through the crate-private `facts::solve_with_work` seam; dependency
//!   edges come from `Adjacency::build`, so the formal virtual definition seeds and the legacy
//!   authored-only behaviour are inherited rather than re-derived.
//! * **No parallel meter.** One [`Work`] pays evaluation, adjacency build, the unique-pair
//!   collection, every membership/degree/decrement comparison and the topology. Nothing resets,
//!   subtracts or reconstructs a residual budget.
//! * **Only `DependsOn`.** A query naming any other relation is refused before membership, because
//!   a symmetric or otherwise non-transitive relation has no prerequisite order and answering it
//!   here would fabricate one.
//! * **Not the query DSL.** Unfiltered only: the existing [`FactQuery`] has no confidence field, so
//!   a `min_conf` API is deliberately absent rather than guessed.
//!
//! # Ordering
//!
//! Dependent-last, ties by ascending [`TermId`]. Layer order is **not** a topological order and is
//! never used. Duplicate supports are retained in the answer for its semantics, but the order is
//! computed from **one canonical unique-pair set**: the same set supplies both the in-degrees and
//! every decrement, so a duplicate support can neither inflate a count nor release a term early.

use crate::facts::{Answer, FactQuery, FactQueryError, SnapshotField, solve_with_work};
use sage_db_infer::meter::{Work, grow};
use sage_db_infer::path::Adjacency;
use sage_db_types::TermId;
use sage_db_types::query::{ExecStats, QueryControl, QueryError};
use sage_db_types::relation::RelationId;
use sage_db_types::snapshot::SnapshotView;

/// A complete `depends-on` answer plus its prerequisite order.
#[derive(Debug)]
pub struct RequiresAnswer {
    answer: Answer,
    order: Vec<TermId>,
    cyclic: Vec<TermId>,
    steps_used: u64,
    unique_pairs: u64,
    /// Total work through the one meter this call used (evaluation + topology).
    stats: ExecStats,
}

impl RequiresAnswer {
    /// The complete underlying answer, untouched.
    #[must_use]
    pub const fn answer(&self) -> &Answer {
        &self.answer
    }

    /// Take the answer, transferring ownership without copying.
    #[must_use]
    pub fn into_answer(self) -> Answer {
        self.answer
    }

    /// Every returned row in the executor's canonical order.
    #[must_use]
    pub fn rows(&self) -> &[sage_db_types::FactRow] {
        self.answer.rows()
    }

    /// Reachable prerequisites, dependent-last, ties by ascending `TermId`.
    ///
    /// The source itself is not included: this is the prerequisite set, not the query's subject.
    #[must_use]
    pub fn prerequisite_order(&self) -> &[TermId] {
        &self.order
    }

    /// Reachable terms the order could not place.
    ///
    /// Under canonical validation this is always empty: a cyclic `depends-on` graph is not a legal
    /// candidate, so it is refused by the executor before ordering. The field exists so that a
    /// future relaxation cannot silently return a short order as if it were complete.
    #[must_use]
    pub fn cyclic(&self) -> &[TermId] {
        &self.cyclic
    }

    /// Steps this call spent from the caller's meter, across every phase.
    ///
    /// Equal to `stats().steps`; kept because it predates the broader figure and is the narrower
    /// statement.
    #[must_use]
    pub const fn steps_used(&self) -> u64 {
        self.steps_used
    }

    /// TOTAL work through this call's single meter: evaluation **and** topology.
    ///
    /// Captured from the same final `Work` that paid every phase, so it is one cumulative figure
    /// rather than a sum of per-phase reconstructions. `answer().stats()` remains the executor's own
    /// phase figures and is deliberately left alone: the two answer different questions.
    #[must_use]
    pub const fn stats(&self) -> ExecStats {
        self.stats
    }

    /// Distinct reachable `(dependent, prerequisite)` pairs the topology actually used.
    ///
    /// This is the observable that a broken duplicate handling changes, because a duplicate
    /// support inflates the raw seed count but not this figure.
    #[must_use]
    pub const fn unique_pairs(&self) -> u64 {
        self.unique_pairs
    }

    /// Whether the order covers the whole reachable prerequisite set.
    #[must_use]
    pub fn is_acyclic(&self) -> bool {
        self.cyclic.is_empty()
    }
}

/// Why a requires query did not complete.
#[derive(Debug)]
pub enum RequiresError {
    /// The request named a different snapshot than the view, refused before any work.
    SnapshotMismatch {
        /// Which field differed.
        field: SnapshotField,
    },
    /// The query named a relation other than `depends-on`.
    ///
    /// A symmetric or non-transitive relation has no prerequisite order; fabricating one would be
    /// a false answer, so the request is refused before any membership work.
    UnsupportedRelation {
        /// The relation the caller named.
        found: RelationId,
    },
    /// The real executor or the shared meter refused, or the caller stopped it.
    Executor(QueryError),
    /// A local topology invariant did not hold.
    ///
    /// Reported instead of clamping, so an accounting bug can never turn into a quietly wrong
    /// order. It indicates a defect in this module, not caller input.
    Invariant {
        /// Which invariant.
        detail: &'static str,
    },
}

impl From<FactQueryError> for RequiresError {
    fn from(error: FactQueryError) -> Self {
        match error {
            FactQueryError::SnapshotMismatch { field } => Self::SnapshotMismatch { field },
            FactQueryError::Executor(error) => Self::Executor(error),
        }
    }
}

impl From<QueryError> for RequiresError {
    fn from(error: QueryError) -> Self {
        Self::Executor(error)
    }
}

/// Answer `source depends-on ?` completely and derive its prerequisite order.
///
/// # Errors
///
/// [`RequiresError::UnsupportedRelation`] for any relation other than `depends-on`;
/// [`RequiresError::SnapshotMismatch`] when the request names another snapshot;
/// [`RequiresError::Executor`] for a real budget, row-limit, stop, unknown-term or invalid-request
/// refusal; [`RequiresError::Invariant`] if a local topology invariant fails. No variant ever
/// carries a partial order.
pub fn requires<V: SnapshotView + ?Sized>(
    view: &V,
    query: FactQuery,
    control: &dyn QueryControl,
) -> Result<RequiresAnswer, RequiresError> {
    // A prerequisite order exists only for `depends-on`. Refused before any membership work, so a
    // symmetric relation can never reach a cycle branch or produce a fabricated order.
    if query.relation != RelationId::DependsOn {
        return Err(RequiresError::UnsupportedRelation {
            found: query.relation,
        });
    }
    let relation = query.relation;
    let mut work = Work::new(query.budget);
    let before = work.steps();

    let answer = solve_with_work(view, query, control, &mut work)?;
    // No post-hoc row comparison against the original ceiling: that check could only fire when the
    // evaluator had already refused, because the evaluator charges every row it holds. The
    // topology instead charges its own rows through the SAME meter as it materialises them, so a
    // caller who capped rows is refused at the point the work would exceed the remaining
    // allowance. `max_steps` and bytes are not substitutes for rows.
    let order = prerequisite_order(view, relation, &answer, control, &mut work)?;
    let steps_used = work.steps().saturating_sub(before);
    // Total work from the SAME final meter — no second meter, no per-phase reconstruction.
    let stats = ExecStats {
        steps: work.steps(),
        work_rows: work.work_rows(),
        work_bytes: work.work_bytes(),
        proof_nodes: work.proof_nodes(),
    };

    Ok(RequiresAnswer {
        answer,
        order: order.ordered,
        cyclic: order.cyclic,
        steps_used,
        unique_pairs: order.unique_pairs,
        stats,
    })
}

/// One order, what it could not place, and the distinct pair count it used.
struct Ordering {
    ordered: Vec<TermId>,
    cyclic: Vec<TermId>,
    unique_pairs: u64,
}

/// Charge one comparison on the caller's meter, then run it.
///
/// Every membership, degree and decrement comparison in this module goes through one of these, so
/// the accounted cost is the work that actually happened rather than the edge count.
fn charged_eq(
    work: &mut Work,
    control: &dyn QueryControl,
    a: TermId,
    b: TermId,
) -> Result<bool, QueryError> {
    work.step(1)?;
    work.check(control)?;
    Ok(a == b)
}

/// Charge one ordering comparison on the caller's meter, then run it.
///
/// Distinct from [`charged_eq`]: the tie-break needs `<`, not `==`, and it must be charged and
/// checkpointed the same way.
fn charged_lt(
    work: &mut Work,
    control: &dyn QueryControl,
    a: TermId,
    b: TermId,
) -> Result<bool, QueryError> {
    work.step(1)?;
    work.check(control)?;
    Ok(a < b)
}

/// Find one term's slot, charging each comparison.
fn find_charged(
    slots: &mut [(TermId, u32)],
    wanted: TermId,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<Option<usize>, QueryError> {
    for (index, (term, _)) in slots.iter().enumerate() {
        if charged_eq(work, control, *term, wanted)? {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// Select a zero-degree term, attending every visited slot before inspecting it.
fn select_candidate(
    remaining: &[(TermId, u32)],
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<Option<usize>, QueryError> {
    let mut chosen: Option<usize> = None;
    for (index, (term, degree)) in remaining.iter().enumerate() {
        work.step(1)?;
        work.check(control)?;
        if *degree == 0 {
            let better = match chosen {
                None => true,
                Some(best) => charged_lt(work, control, *term, remaining[best].0)?,
            };
            if better {
                chosen = Some(index);
            }
        }
    }
    Ok(chosen)
}

/// Derive the dependent-last order from the **same** adjacency the evaluator used.
fn prerequisite_order<V: SnapshotView + ?Sized>(
    view: &V,
    relation: RelationId,
    answer: &Answer,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<Ordering, RequiresError> {
    let adjacency = Adjacency::build(view.graph(), relation, control, work)?;

    // The reachable prerequisite set: every row's object, already in canonical order.
    //
    // INHERITED PRECONDITION, not assumed: `infer::assemble` emits one row per reached target
    // (`Reach::touch` pushes each term at most once, guarded by its `reached` state) and then
    // sorts by `FactRow::sort_key = (to, relation, from)`. For a single `SourceQuery` the
    // relation and source are constant, so the rows arrive sorted by `to` and unique. `to` is
    // therefore already a strictly ascending unique sequence, and re-sorting here would be
    // uncharged `O(n log n)` work that cannot change the result.
    //
    // The adjacent strict-order check below is the future-proof refusal: if a producer ever
    // emitted an unsorted or duplicated sequence, this fails loudly instead of silently
    // reordering. It charges one comparison per adjacent pair, so the invariant is metered.
    let rows = answer.rows();
    let mut terms: Vec<TermId> = Vec::new();
    grow(&mut terms, rows.len(), work)?;
    let mut previous: Option<TermId> = None;
    for row in rows {
        work.step(1)?;
        work.check(control)?;
        if let Some(prior) = previous {
            // Strictly ascending: equal or descending means the producer contract was broken.
            if !charged_lt(work, control, prior, row.to)? {
                return Err(RequiresError::Invariant {
                    detail: "answer rows are not strictly ascending by target",
                });
            }
        }
        previous = Some(row.to);
        // One topology row per projected prerequisite, charged BEFORE it is materialised.
        work.row()?;
        terms.push(row.to);
    }

    // In-degree per reachable term.
    let mut indegree: Vec<(TermId, u32)> = Vec::new();
    grow(&mut indegree, terms.len(), work)?;
    for term in &terms {
        work.step(1)?;
        work.check(control)?;
        // One row per in-degree entry, charged before the push.
        work.row()?;
        indegree.push((*term, 0));
    }

    // ONE canonical unique-pair set, used for BOTH the degrees and the decrements.
    //
    // `visit_seed_endpoints` offers every retained seed, including duplicates. Retaining only the
    // first occurrence makes a duplicate support unable to inflate a count *or* release a
    // dependent early — the defect a raw-seed decrement would create.
    let mut pairs: Vec<(TermId, TermId)> = Vec::new();
    grow(&mut pairs, terms.len(), work)?;
    adjacency.visit_seed_endpoints(control, work, |from, to, work| {
        work.step(1)?;
        let from_known = find_charged(&mut indegree, from, work, control)?.is_some();
        let to_known = find_charged(&mut indegree, to, work, control)?.is_some();
        if !(from_known && to_known) {
            return Ok(());
        }
        // Distinctness is decided by an explicit pair walk that charges every comparison.
        for (known_from, known_to) in &pairs {
            if charged_eq(work, control, *known_from, from)?
                && charged_eq(work, control, *known_to, to)?
            {
                return Ok(());
            }
        }
        let needed = pairs.len() + 1;
        work.row()?;
        grow(&mut pairs, needed, work)?;
        pairs.push((from, to));
        // `from` depends on `to`, so `to` must be established first.
        let slot = find_charged(&mut indegree, from, work, control)?.ok_or(
            QueryError::InvalidRequest {
                reason: "requires topology: a known term lost its in-degree slot",
            },
        )?;
        indegree[slot].1 = indegree[slot]
            .1
            .checked_add(1)
            .ok_or(QueryError::InvalidRequest {
                reason: "requires topology: in-degree overflow",
            })?;
        Ok(())
    })?;

    let unique_pairs = u64::try_from(pairs.len()).map_err(|_| RequiresError::Invariant {
        detail: "pair count does not fit u64",
    })?;

    // Kahn with ascending-TermId selection: dependent-last, stable ties.
    let mut ordered: Vec<TermId> = Vec::new();
    grow(&mut ordered, indegree.len(), work)?;
    let mut remaining = indegree;
    while !remaining.is_empty() {
        work.check(control)?;
        // The slot visit itself checkpoints, including nonzero degrees and the first candidate;
        // tie-break comparisons have their own charged checkpoint.
        let chosen = select_candidate(&remaining, work, control)?;
        let Some(index) = chosen else {
            // No zero-in-degree term remains. Under canonical validation this is unreachable
            // (a cyclic depends-on graph is refused by the executor), so returning a partial
            // order would be a silent lie: report an invariant failure instead.
            return Err(RequiresError::Invariant {
                detail: "reachable set has no zero in-degree term after canonical validation",
            });
        };
        // Charge the tail shift BEFORE it happens, so a budget refusal cannot fire after the memory
        // has already moved. `Vec::remove(index)` moves `len - index - 1` elements down one slot.
        // Control is consulted on EVERY shift-charge iteration, not once at the loop exit: a large
        // early index would otherwise charge its whole shift count with no chance to stop.
        let shifts = remaining.len().saturating_sub(index).saturating_sub(1);
        for _ in 0..shifts {
            work.step(1)?;
            work.check(control)?;
        }
        let (term, _) = remaining.remove(index);
        // One topology row per emitted order entry, charged before the push.
        work.row()?;
        ordered.push(term);
        // Decrement using the SAME unique pair set, never the raw seed walk.
        for (dependent, prerequisite) in &pairs {
            work.step(1)?;
            if charged_eq(work, control, *prerequisite, term)? {
                let slot = find_charged(&mut remaining, *dependent, work, control)?.ok_or(
                    RequiresError::Invariant {
                        detail: "a placed prerequisite's dependent has no slot",
                    },
                )?;
                remaining[slot].1 =
                    remaining[slot]
                        .1
                        .checked_sub(1)
                        .ok_or(RequiresError::Invariant {
                            detail: "in-degree underflow (duplicate release)",
                        })?;
            }
        }
    }
    Ok(Ordering {
        ordered,
        cyclic: Vec::new(),
        unique_pairs,
    })
}

#[cfg(test)]
mod control_tests {
    use super::select_candidate;
    use core::cell::Cell;
    use sage_db_infer::meter::Work;
    use sage_db_types::TermId;
    use sage_db_types::budget::Budget;
    use sage_db_types::query::{ControlStop, QueryControl, QueryError};

    struct StopAt {
        calls: Cell<usize>,
        allowed: usize,
    }

    impl QueryControl for StopAt {
        fn checkpoint(&self) -> Result<(), ControlStop> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            if call >= self.allowed {
                Err(ControlStop::DeadlineExceeded)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn a_nonzero_degree_scan_stops_at_the_next_slot() {
        let remaining = [
            (TermId::new(1), 1),
            (TermId::new(2), 1),
            (TermId::new(3), 0),
        ];
        let control = StopAt {
            calls: Cell::new(0),
            allowed: 1,
        };
        let mut work = Work::new(Budget::DEFAULT);
        assert_eq!(
            select_candidate(&remaining, &mut work, &control),
            Err(QueryError::Stopped(ControlStop::DeadlineExceeded))
        );
        assert_eq!(control.calls.get(), 2);
        assert_eq!(work.steps(), 2);
    }

    #[test]
    fn the_first_zero_degree_candidate_checks_control_before_selection() {
        let remaining = [(TermId::new(1), 0)];
        let control = StopAt {
            calls: Cell::new(0),
            allowed: 0,
        };
        let mut work = Work::new(Budget::DEFAULT);
        assert_eq!(
            select_candidate(&remaining, &mut work, &control),
            Err(QueryError::Stopped(ControlStop::DeadlineExceeded))
        );
        assert_eq!(control.calls.get(), 1);
    }
}
