//! Bounded navigation over the **authored** base graph: the shortest undirected path between
//! two terms, as an explicit **path** answer rather than a Fact.
//!
//! # What this is
//!
//! One question: "is there a route of authored assertions between these two terms, and if so
//! what is one shortest route?" It is the Rust counterpart of the shipped
//! `packages/core/src/graph/traverse.ts` `shortestPath` + `packages/service/src/query.ts`
//! `findPath`, restricted to what a committed snapshot actually contains.
//!
//! # What it deliberately is not
//!
//! * **Not a Fact, and not a proof.** [`query-dsl`](../../../docs/design/query-dsl.md) §3 says a
//!   bounded navigation "uses a separate operation and its result is labelled a path, not a
//!   Fact"; §5 and `axiomatics` §5 say a navigation path "must not be output as a new semantic
//!   fact". So [`Navigation`] carries nodes and *witness assertions*, never a derived fact row,
//!   never a `ProofRef`, and nothing here can be passed to a fact consumer as though the
//!   endpoints were related by derivation.
//! * **Not inference.** No rule is evaluated, no closure is taken, and no relation is composed
//!   with another: a path is a statement about authored edges. `enables` is navigation and not
//!   the inverse of `depends-on` (`axiomatics` §1), and this module never treats it as either.
//! * **Not the R3 virtual graph.** Only [`sage_db_types::graph::BaseGraph`] assertions are
//!   walked — the active authored records in this view. Virtual edges belong to the derived layer
//!   and are not part of a `BaseGraph`, so they cannot appear here by construction.
//! * **No display truncation.** A result is complete or it is an error. There is no
//!   `max_rows` prefix, because a partial path is not a shorter answer — it is a wrong one.
//!
//! # Traversal shape
//!
//! Breadth-first over **undirected** adjacency: an authored assertion `from -> to` is a
//! possible step in both directions, and every relation kind participates. BFS is what makes
//! the answer *shortest*, and the frontier is visited in the graph's own canonical order, which
//! is what makes the answer *deterministic* (see [`Navigation::tie_rule`]).
//!
//! A witness is always reported with its **authored** direction, even when the traversal
//! crossed it backwards. Rewriting `to -> from` into `from -> to` would attribute a direction
//! the author never wrote, and `axiomatics` §2 forbids exactly that ("the reverse reading only
//! produces derived, never asserted").
//!
//! # Cost: the meter sees the algorithm, not a summary of it
//!
//! Membership and reconstruction both use the graph's own **sorted term table**
//! ([`sage_db_types::graph::BaseGraph::terms`]) and binary search over a sorted predecessor
//! index, so no step is an uncharged linear scan:
//!
//! * every binary-search **comparison is charged** as a step, and control is consulted inside
//!   the search rather than only after it;
//! * BFS tables are **pre-sized to the graph's term count** and grown with the canonical
//!   [`sage_db_infer::meter::grow`], which charges the real capacity delta and uses
//!   `try_reserve_exact` at a known bound — no doubling guess and no post-allocation top-up;
//! * every materialised **row** (a visited slot, a reconstructed node, a reconstructed step, or
//!   a zero-hop node) is charged on the same meter before it is produced.
//!
//! One [`sage_db_infer::meter::Work`] pays for everything, and the finished meter's own counters
//! are reported as the canonical [`ExecStats`] — there is no second meter and no second stats
//! type.

use sage_db_infer::meter::Work;
use sage_db_types::budget::{Budget, BudgetLimit};
use sage_db_types::graph::SnapshotId;
use sage_db_types::identity::{AssertionId, RecordRevision, TermId};
use sage_db_types::query::{ControlStop, ExecStats, QueryControl, QueryError};
use sage_db_types::record::AssertionRecord;
use sage_db_types::relation::RelationId;
use sage_db_types::snapshot::SnapshotView;

use crate::facts::SnapshotField;

/// One step of a returned path, in the direction it was **traversed**.
///
/// `witness` keeps the authored direction regardless of which way the search walked it, so a
/// caller that needs to know "who asserted this" always reads the record's own `from`/`target`
/// rather than a normalised pair.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PathStep {
    /// The term the walk arrived from.
    pub from: TermId,
    /// The term the walk arrived at.
    pub to: TermId,
    /// The authored assertion that made this step possible, with its own identity and revision.
    ///
    /// Carried by value: [`AssertionRecord`] is a fixed-size copy of canonical identity fields,
    /// so this is not a second record model and not a graph copy. Its `from`/`target` are the
    /// **authored** endpoints and may be the reverse of `from`/`to` above.
    pub witness: AssertionRecord,
    /// Whether the walk followed the assertion backwards (authored `target == from`).
    pub reversed: bool,
}

impl PathStep {
    /// The relation the witnessing assertion was authored with.
    #[must_use]
    pub fn relation(&self) -> RelationId {
        self.witness.relation
    }

    /// The witness's assertion identity.
    #[must_use]
    pub fn assertion(&self) -> AssertionId {
        self.witness.id
    }

    /// The witness's record revision.
    #[must_use]
    pub fn revision(&self) -> RecordRevision {
        self.witness.revision
    }
}

/// A complete shortest navigation path.
///
/// Exists only for a finished search: construction happens after the walk succeeds, so there is
/// no variant meaning "partly explored".
///
/// **Not `Clone`, by design.** The nodes and steps are two heap tables paid for once on the
/// query's meter; a derived `Clone` would duplicate them infallibly and without any charge, so a
/// caller could hold two copies for the price of one. This mirrors the fact layer's `Answer`,
/// which likewise retains its owned result instead of offering a copy. Accessors borrow.
#[derive(PartialEq, Eq, Debug)]
pub struct Navigation {
    basis: SnapshotId,
    from: TermId,
    to: TermId,
    nodes: Vec<TermId>,
    steps: Vec<PathStep>,
    stats: ExecStats,
}

impl Navigation {
    /// The snapshot this path was computed at — the **view's** basis, never a caller's claim.
    #[must_use]
    pub fn basis(&self) -> SnapshotId {
        self.basis
    }

    /// The requested source term.
    #[must_use]
    pub fn from(&self) -> TermId {
        self.from
    }

    /// The requested destination term.
    #[must_use]
    pub fn to(&self) -> TermId {
        self.to
    }

    /// The path's terms, starting at [`Self::from`] and ending at [`Self::to`].
    ///
    /// A zero-hop result is exactly one element. A non-zero result has `steps().len() + 1`
    /// nodes; the two are maintained together so neither can disagree with the other.
    #[must_use]
    pub fn nodes(&self) -> &[TermId] {
        &self.nodes
    }

    /// The witnessing assertion for each hop, in traversal order.
    ///
    /// Empty exactly when the endpoints are identical.
    #[must_use]
    pub fn steps(&self) -> &[PathStep] {
        &self.steps
    }

    /// The number of hops.
    #[must_use]
    pub fn hops(&self) -> usize {
        self.steps.len()
    }

    /// Execution provenance, read from the **same** meter that bounded the walk.
    ///
    /// The query contract expects a result to report [`ExecStats`]; this is that type, filled
    /// from the one [`Work`] instance, not a second counter kept alongside it.
    #[must_use]
    pub fn stats(&self) -> ExecStats {
        self.stats
    }

    /// The tie rule that made this path the one returned.
    ///
    /// Frontier neighbours are visited in the graph's **canonical order**, so among several
    /// shortest paths the returned one is the first in that order. Concretely, for each
    /// frontier term the walk considers:
    ///
    /// * outgoing assertions in `(from, relation, target, assertion_id)` order — the graph's own
    ///   stored `visit_key` order, and
    /// * incoming assertions in `(target, visit_key)` order,
    ///
    /// and records a term as visited when it is first discovered. Because the frontier is
    /// expanded level by level, the reported path is the shortest one whose last hop was
    /// discovered earliest in that combined canonical scan.
    ///
    /// This is **not** claimed to reproduce the shipped JavaScript's choice: that
    /// implementation iterated a flat `graph.edges` array, so its tie-breaking depended on the
    /// caller's input order. Any statement that the two agree byte-for-byte on equal-length
    /// alternatives would be false; only "shortest, and deterministic at equal length under
    /// canonical `BaseGraph` order" is claimed here.
    #[must_use]
    pub fn tie_rule(&self) -> &'static str {
        "BFS over undirected authored assertions; ties broken by canonical BaseGraph order \
         (outgoing visit_key, then incoming (target, visit_key)); not the JS input-order choice"
    }
}

/// Why a navigation request produced no path.
///
/// Every variant is "no answer", never an empty path: a disconnected pair is
/// [`Self::NoPath`], which means the search **completed**, while a budget, stop or allocation
/// refusal means it did not.
///
/// `Copy`, because every variant is now a scalar or a copied scalar pair: the diagnostic
/// [`SnapshotField`] names the differing field instead of dragging two `SnapshotId`s into the
/// error, so a refusal costs less than the answer it stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NavigationError {
    /// The request named a snapshot other than the view's.
    ///
    /// Only the differing **field** is carried. Both ids are already in the caller's hand — it
    /// built the request and it holds the view — so copying two four-field values into every
    /// refusal would make the error larger than the answer it describes while adding nothing.
    SnapshotMismatch {
        /// Which field differed.
        field: SnapshotField,
    },
    /// An endpoint names no active term in this graph.
    ///
    /// Reported on the **requested** side, so a caller can tell which endpoint was unknown. The
    /// same unknown term on both sides is still unknown, not a zero-hop success: no term means
    /// no node, and a fabricated `[x]` path would assert that a term exists.
    UnknownTerm {
        /// Which endpoint.
        side: Endpoint,
        /// The term the request named.
        term: TermId,
    },
    /// The search finished and there is **no** route between two known terms.
    ///
    /// Distinct from every refusal below: this is a complete answer.
    NoPath {
        /// The source term.
        from: TermId,
        /// The destination term.
        to: TermId,
    },
    /// The work budget was exhausted; the search did not finish.
    BudgetExceeded(BudgetLimit),
    /// The caller asked to stop.
    Stopped(ControlStop),
    /// A table could not be allocated at the extent the walk had already admitted.
    ///
    /// Kept separate from [`Self::BudgetExceeded`]: the budget permitted the bytes and the
    /// allocator refused them, so the remedy is to free memory, not to raise the limit.
    Allocation {
        /// Bytes requested from the allocator.
        requested: u64,
    },
    /// An extent could not be described in `u64`.
    ///
    /// Neither a budget nor an allocator failure: the *count* is not a representable size, so
    /// no limit would help. **Only** returned by real arithmetic on a count — never for a
    /// consistency failure, which has its own variant.
    ExtentOverflow {
        /// The count that could not be represented.
        count: usize,
    },
    /// A reservation returned more slots than the extent admitted for it.
    ///
    /// A successful `try_reserve_exact` guarantees *at least* the ask; more means the allocator
    /// over-allocated, which would leave retained capacity unaccounted. Refused rather than
    /// accepted, matching the graph and snapshot builders. Same class as [`Self::Allocation`]:
    /// the budget permitted the extent, so raising a limit would not help.
    UnexpectedCapacity {
        /// Slots admitted before allocating.
        requested: usize,
        /// Actual reported backing capacity.
        actual: usize,
    },
    /// An internal consistency check failed.
    ///
    /// The walk reconstructed something it could not have produced — a shape defect in this
    /// module, not a caller error and not an arithmetic overflow. Carries a static diagnostic so
    /// the error path stays heap-free, and is deliberately distinct from
    /// [`Self::ExtentOverflow`]: no arithmetic failed.
    Invariant {
        /// Fixed, non-localised description of the broken invariant.
        detail: &'static str,
    },
}

/// Which requested endpoint was not a term in this graph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Endpoint {
    /// The `from` term.
    From,
    /// The `to` term.
    To,
}

impl From<QueryError> for NavigationError {
    fn from(error: QueryError) -> Self {
        match error {
            QueryError::BudgetExceeded(limit) => Self::BudgetExceeded(limit),
            QueryError::Stopped(stop) => Self::Stopped(stop),
            // `Work` yields only the two classes above on this path; anything else is a
            // programming defect, surfaced as the budget class it came from rather than
            // silently converted into a path.
            other => Self::BudgetExceeded(match other {
                QueryError::Proof(_) => BudgetLimit::ProofNodes,
                _ => BudgetLimit::Steps,
            }),
        }
    }
}

/// One visited node's predecessor in the breadth-first tree.
///
/// `via` is `None` for the source. The chain is threaded through this table and read back once
/// at the end, so no whole path is ever copied per queue entry.
#[derive(Clone, Copy)]
struct Predecessor {
    term: TermId,
    via: Option<PathStep>,
}

/// Find one shortest undirected path between two active terms of this authored graph.
///
/// # Contract
///
/// * Only active authored [`sage_db_types::graph::BaseGraph`] assertions are walked, all relation
///   kinds included, each usable in both directions.
/// * A known pair of identical endpoints yields a one-node, zero-step path.
/// * Two known endpoints with no route yield [`NavigationError::NoPath`] **after a complete
///   search**, not a refusal.
/// * An endpoint absent from the graph yields [`NavigationError::UnknownTerm`] — including the
///   `from == to` case, which must not be reported as a zero-hop path.
/// * Nothing is returned on budget exhaustion, cancellation or allocation refusal.
///
/// # Errors
///
/// [`NavigationError`]: a snapshot mismatch, an unknown endpoint, a finished search with no
/// route, or a real budget/stop/allocation/extent refusal.
pub fn shortest_path<V: SnapshotView + ?Sized>(
    view: &V,
    requested: SnapshotId,
    from: TermId,
    to: TermId,
    budget: Budget,
    control: &dyn QueryControl,
) -> Result<Navigation, NavigationError> {
    let basis = view.basis();
    // Basis binding BEFORE any membership read, and before any admission. The field is a
    // scalar diagnostic; no id is copied into the refusal.
    if requested != basis {
        return Err(NavigationError::SnapshotMismatch {
            field: differ(requested, basis),
        });
    }
    let graph = view.graph();

    let mut work = Work::new(budget);
    // Control is consulted BEFORE any work, including the endpoint lookups.
    work.check_entry(control)?;

    // The graph's own sorted term table is the canonical index: membership is a binary search
    // over it, charged per comparison, never a linear scan.
    let terms = graph.terms();
    let from_at = locate(terms, from, &mut work, control)?;
    let to_at = locate(terms, to, &mut work, control)?;
    let Some(from_at) = from_at else {
        return Err(NavigationError::UnknownTerm {
            side: Endpoint::From,
            term: from,
        });
    };
    let Some(to_at) = to_at else {
        return Err(NavigationError::UnknownTerm {
            side: Endpoint::To,
            term: to,
        });
    };

    // Identical known endpoints: a real zero-hop path with one node and no witness.
    //
    // Exactly ONE row is materialised for it: the byte reservation below buys capacity only,
    // and `write_row` charges the single write. The stats test pins `work_rows == 1`.
    if from_at == to_at {
        let mut nodes: Vec<TermId> = Vec::new();
        reserve_exact(&mut nodes, 1, &mut work)?;
        write_row(&mut work, control)?;
        nodes.push(from);
        work.check_exit(control)?;
        return Ok(Navigation {
            basis,
            from,
            to,
            nodes,
            steps: Vec::new(),
            stats: stats_of(&work),
        });
    }

    // --- Breadth-first search over undirected adjacency. ---
    //
    // Every table is reserved ONCE at the graph's term count — a real bound, because every
    // visited term is a term of this graph and is recorded at most once. Reservation is bytes
    // only; each materialised slot is charged as a row immediately before it is written.
    let bound = terms.len();
    let mut state = Tables::with_bound(bound, &mut work, control)?;
    state.push(from, None, &mut work, control)?;
    state.mark_visited(from_at, &mut work, control)?;
    let mut found: Option<usize> = None;

    while let Some(current) = state.next_frontier(&mut work, control)? {
        work.step(1)?;
        let current_term = state.term_at(current);

        // Outgoing authored edges, in canonical visit_key order.
        for assertion in graph.assertions_from(current_term) {
            work.step(1)?;
            // Per-row control: a stop is observed at the next row, not 1024 steps later.
            work.check(control)?;
            let step = PathStep {
                from: current_term,
                to: assertion.target,
                witness: *assertion,
                reversed: false,
            };
            if let Some(next) =
                state.consider(assertion.target, step, to, terms, &mut work, control)?
            {
                found = Some(next);
                break;
            }
        }
        if found.is_some() {
            break;
        }

        // Incoming authored edges, in canonical (target, visit_key) order. Iteration is
        // allocation-free over the borrowed reverse index.
        for assertion in graph.assertions_to(current_term).iter() {
            work.step(1)?;
            work.check(control)?;
            let step = PathStep {
                from: current_term,
                to: assertion.from,
                witness: *assertion,
                reversed: true,
            };
            if let Some(next) =
                state.consider(assertion.from, step, to, terms, &mut work, control)?
            {
                found = Some(next);
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }

    let Some(target) = found else {
        // A COMPLETE search that reached nothing: the pair is disconnected on this basis.
        work.check_exit(control)?;
        return Err(NavigationError::NoPath { from, to });
    };

    // Reconstruct through separately admitted chain and output tables. Each materialised
    // slot pays one work row. Destination-first output is then reversed with attended swaps;
    // no unmetered standard-library reverse walk is used.
    let depth = state.depth_of(target, &mut work, control)?;
    let node_count = depth
        .checked_add(1)
        .ok_or(NavigationError::ExtentOverflow { count: depth })?;
    let mut nodes: Vec<TermId> = Vec::new();
    let mut steps: Vec<PathStep> = Vec::new();
    reserve_exact(&mut nodes, node_count, &mut work)?;
    reserve_exact(&mut steps, depth, &mut work)?;

    // Walk the chain to collect the ORDER of slots first (no writes), then fill the two output
    // tables from the back: node[last] = destination, step[last] = the step INTO it, and so on.
    let mut chain: Vec<usize> = Vec::new();
    reserve_exact(&mut chain, node_count, &mut work)?;
    let mut cursor = Some(target);
    while let Some(at) = cursor {
        write_row(&mut work, control)?;
        chain.push(at);
        cursor = state.predecessor_of(at, &mut work, control)?;
    }
    if chain.len() != node_count {
        return Err(NavigationError::Invariant {
            detail: "predecessor chain length disagrees with its measured depth",
        });
    }

    // `chain` is destination-first. Writing it in that order produces destination-first output,
    // so the only remaining work is one attended flip per table — charged and interruptible,
    // never an unmetered `reverse()`.
    for at in &chain {
        write_row(&mut work, control)?;
        nodes.push(state.term_at(*at));
        if let Some(step) = state.step_at(*at) {
            write_row(&mut work, control)?;
            steps.push(step);
        }
    }
    attended_reverse(&mut nodes, &mut work, control)?;
    attended_reverse(&mut steps, &mut work, control)?;

    if nodes.first().copied() != Some(from) || nodes.last().copied() != Some(to) {
        return Err(NavigationError::Invariant {
            detail: "reconstructed path endpoints do not match the request",
        });
    }
    if steps.len() + 1 != nodes.len() {
        return Err(NavigationError::Invariant {
            detail: "reconstructed step count does not match its node count",
        });
    }

    work.check_exit(control)?;
    Ok(Navigation {
        basis,
        from,
        to,
        nodes,
        steps,
        stats: stats_of(&work),
    })
}

/// Reverse a table in place with every swap charged and attended.
///
/// `slice::reverse` is a single call over the whole table: no charge, no control, no estimate.
/// This does the same work explicitly so the meter sees it and a cancellation lands inside it.
fn attended_reverse<T>(
    table: &mut [T],
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<(), NavigationError> {
    let len = table.len();
    if len < 2 {
        return Ok(());
    }
    let mut left = 0usize;
    let mut right = len - 1;
    while left < right {
        work.step(1)?;
        work.check(control)?;
        table.swap(left, right);
        left += 1;
        right -= 1;
    }
    Ok(())
}

/// The first differing field of two snapshot ids, as a scalar diagnostic.
fn differ(requested: SnapshotId, view: SnapshotId) -> SnapshotField {
    if requested.kb_uuid != view.kb_uuid {
        SnapshotField::Kb
    } else if requested.version != view.version {
        SnapshotField::Version
    } else if requested.graph_digest != view.graph_digest {
        SnapshotField::GraphDigest
    } else {
        SnapshotField::Rules
    }
}

/// Binary search the graph's sorted term table, charging every comparison and checking control.
///
/// Returns the position of `term` when present. This is the canonical index the meter can see:
/// the search is `log2(n)` comparisons and each one is a charged step, so membership costs the
/// same order as it takes.
fn locate(
    terms: &[sage_db_types::record::TermMeta],
    term: TermId,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<Option<usize>, NavigationError> {
    let mut low = 0usize;
    let mut high = terms.len();
    while low < high {
        work.step(1)?;
        work.check(control)?;
        let mid = low + (high - low) / 2;
        match terms[mid].id.cmp(&term) {
            core::cmp::Ordering::Less => low = mid + 1,
            core::cmp::Ordering::Greater => high = mid,
            core::cmp::Ordering::Equal => return Ok(Some(mid)),
        }
    }
    Ok(None)
}

/// The canonical stats, read from the one meter that bounded this call.
fn stats_of(work: &Work) -> ExecStats {
    ExecStats {
        steps: work.steps(),
        work_rows: work.work_rows(),
        work_bytes: work.work_bytes(),
        proof_nodes: work.proof_nodes(),
    }
}

/// Reserve a table's BYTES at its admitted extent, exactly once.
///
/// This does **not** charge rows: a reservation is capacity, and materialisation is a separate
/// event charged immediately before each real write (see `write_row`). Charging a row here as
/// well would bill the same element twice.
///
/// The allocator is called directly rather than through `infer::meter::grow`, because `grow`
/// reports a failed `try_reserve_exact` as `BudgetExceeded` — which would make a genuine
/// allocator refusal indistinguishable from a budget ceiling. Bytes are charged on the meter
/// **before** the allocator is asked, so a refusal never leaves unadmitted capacity behind.
fn reserve_exact<T>(
    table: &mut Vec<T>,
    extent: usize,
    work: &mut Work,
) -> Result<(), NavigationError> {
    if extent == 0 {
        return Ok(());
    }
    let requested = extent_of::<T>(extent)?;
    // Charge the real extent BEFORE asking the allocator for it.
    work.bytes(requested)?;
    table
        .try_reserve_exact(extent.saturating_sub(table.len()))
        .map_err(|_| NavigationError::Allocation { requested })?;
    // A successful reservation guarantees AT LEAST the ask. More than the admitted extent means
    // the allocator over-allocated, which would under-report retained capacity: refuse it rather
    // than accept it silently (the GraphBuild / SnapshotBuild precedent). No late top-up.
    if table.capacity() > extent {
        return Err(NavigationError::UnexpectedCapacity {
            requested: extent,
            actual: table.capacity(),
        });
    }
    Ok(())
}

/// Charge exactly one materialised row and check control immediately before the write.
///
/// Every real write to a retained table goes through here, so a row is billed once and control
/// is consulted at the moment of production rather than at some coarser boundary.
fn write_row(work: &mut Work, control: &dyn QueryControl) -> Result<(), NavigationError> {
    work.row()?;
    work.check(control).map_err(NavigationError::from)
}

/// `count` slots of `T` as a byte extent, checked.
fn extent_of<T>(count: usize) -> Result<u64, NavigationError> {
    u64::try_from(count)
        .ok()
        .and_then(|n| n.checked_mul(u64::try_from(core::mem::size_of::<T>()).ok()?))
        .ok_or(NavigationError::ExtentOverflow { count })
}

/// The BFS working tables, sized once to the graph's term count.
///
/// `visited` is a bitmap over term-table positions, so membership is one bit test rather than a
/// scan, and the predecessor chain is a parallel array indexed by discovery order.
struct Tables {
    /// The term-table position of each discovered term's slot, to detect revisits in O(1).
    visited: Vec<u64>,
    /// One predecessor per discovered term, in discovery order.
    nodes: Vec<Predecessor>,
    /// Terms to expand, in discovery order; `head` is the dequeue cursor.
    queue: Vec<usize>,
    head: usize,
}

impl Tables {
    /// Reserve every table's BYTES at the graph's known term bound.
    ///
    /// Reservation buys capacity only: no row is charged here, because no row exists yet. The
    /// bitmap's zero words are real initialisations, so each one is charged as a row, a step and
    /// a control check immediately before it is written.
    fn with_bound(
        bound: usize,
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<Self, NavigationError> {
        let words = bound
            .checked_add(63)
            .ok_or(NavigationError::ExtentOverflow { count: bound })?
            / 64;
        let mut visited: Vec<u64> = Vec::new();
        reserve_exact(&mut visited, words, work)?;
        for _ in 0..words {
            work.step(1)?;
            write_row(work, control)?;
            visited.push(0);
        }
        let mut nodes: Vec<Predecessor> = Vec::new();
        reserve_exact(&mut nodes, bound, work)?;
        let mut queue: Vec<usize> = Vec::new();
        reserve_exact(&mut queue, bound, work)?;
        Ok(Self {
            visited,
            nodes,
            queue,
            head: 0,
        })
    }

    /// Mark a term-table position visited.
    ///
    /// The bitmap was reserved and zeroed at construction, so this is a masked write into an
    /// already-materialised word — no new row — but it is still charged a step and attended.
    fn mark_visited(
        &mut self,
        position: usize,
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<(), NavigationError> {
        work.step(1)?;
        work.check(control)?;
        self.visited[position / 64] |= 1u64 << (position % 64);
        Ok(())
    }

    /// Whether a term-table position was already visited.
    fn was_visited(&self, position: usize) -> bool {
        self.visited[position / 64] & (1u64 << (position % 64)) != 0
    }

    /// Append one discovered term: the node slot and its queue slot, each charged once.
    fn push(
        &mut self,
        term: TermId,
        via: Option<PathStep>,
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<usize, NavigationError> {
        let index = self.nodes.len();
        if index >= self.nodes.capacity() || index >= self.queue.capacity() {
            return Err(NavigationError::Invariant {
                detail: "discovered more terms than the graph's term bound",
            });
        }
        write_row(work, control)?;
        self.nodes.push(Predecessor { term, via });
        write_row(work, control)?;
        self.queue.push(index);
        Ok(index)
    }

    /// Pop the next frontier term, or `None` when the queue drains.
    fn next_frontier(
        &mut self,
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<Option<usize>, NavigationError> {
        if self.head >= self.queue.len() {
            return Ok(None);
        }
        work.check(control)?;
        let index = self.queue[self.head];
        self.head += 1;
        Ok(Some(index))
    }

    /// The term of a discovered slot.
    fn term_at(&self, index: usize) -> TermId {
        self.nodes[index].term
    }

    /// The step that discovered a slot.
    fn step_at(&self, index: usize) -> Option<PathStep> {
        self.nodes[index].via
    }

    /// Record `term` as newly discovered via `step`; return its slot when it is the destination.
    ///
    /// Membership is a bit test over term-table positions — the canonical sorted index — so this
    /// performs no scan. The single comparison that locates the position is charged by `locate`.
    fn consider(
        &mut self,
        term: TermId,
        step: PathStep,
        to: TermId,
        terms: &[sage_db_types::record::TermMeta],
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<Option<usize>, NavigationError> {
        // `BaseGraph` validates that every assertion endpoint is a term of this graph, so a
        // neighbour that cannot be located is a broken graph invariant, not a reason to skip it
        // silently.
        let Some(position) = locate(terms, term, work, control)? else {
            return Err(NavigationError::Invariant {
                detail: "an adjacency endpoint is not a term of this graph",
            });
        };
        if self.was_visited(position) {
            return Ok(None);
        }
        self.mark_visited(position, work, control)?;
        let index = self.push(term, Some(step), work, control)?;
        Ok((term == to).then_some(index))
    }

    /// The predecessor slot of a discovered slot, charged per comparison.
    fn predecessor_of(
        &self,
        index: usize,
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<Option<usize>, NavigationError> {
        let Some(via) = self.nodes[index].via else {
            return Ok(None);
        };
        // The predecessor is the discovered slot whose term is `via.from`. It precedes `index`
        // in discovery order, so a bounded scan over `0..index` is the exact search space; each
        // comparison and the periodic control check are charged.
        let mut found = None;
        for at in 0..index {
            work.step(1)?;
            work.check(control)?;
            if self.nodes[at].term == via.from {
                found = Some(at);
            }
        }
        Ok(found)
    }

    /// Hops from the source to a discovered slot, each step charged.
    fn depth_of(
        &self,
        target: usize,
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<usize, NavigationError> {
        let mut depth = 0usize;
        let mut cursor = Some(target);
        while let Some(at) = cursor {
            cursor = self.predecessor_of(at, work, control)?;
            if cursor.is_some() {
                depth = depth
                    .checked_add(1)
                    .ok_or(NavigationError::ExtentOverflow { count: depth })?;
            }
        }
        Ok(depth)
    }
}
