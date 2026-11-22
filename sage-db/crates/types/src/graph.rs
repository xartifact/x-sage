//! The immutable base-graph input contract.
//!
//! A [`BaseGraph`] is the aggregate of *active authored assertions* at one
//! committed version, together with the rule configuration it was read under. It
//! is not the set of derivable facts: closure, proofs and caches are computed per
//! query from this input.
//!
//! Access is deliberately limited to term metadata and adjacency. There is no
//! `fact()` or `explain()` accessor here, because an unbudgeted accessor would
//! move inference (and its cost) outside the executor's meter
//! (`docs/design/system.md` §3).
//!
//! Construction is fallible in three independent ways: invalid input
//! ([`GraphBuildError`]), input that does not fit a P0 [`Budget`]
//! ([`GraphBuildError::BudgetExceeded`]), or an extent whose checked arithmetic does not
//! fit `u64` ([`GraphBuildError::ExtentOverflow`]). The index is allocated under
//! admission instead of growing Vecs with infallible push. Build cost belongs to
//! snapshot construction and is not charged again per query.
//!
//! # Two entry points, one indexing body
//!
//! [`BaseGraph::new`] is the P0 entry point: it bounds a build with a [`Budget`], performs
//! no ledger work, and therefore returns [`GraphBuildError`] directly — it has no channel
//! through which a caller refusal could be reported. [`BaseGraph::index_admitted_inputs`]
//! is the snapshot entry point: the caller already byte-admitted the two input vectors, so
//! this charges only the *additional* index/grouping storage through the caller's own
//! [`SharedLedger`] and returns [`GraphBuildRefusal`].
//!
//! Both run the same private body over a **generic meter**, so there is exactly one
//! validation/index/grouping algorithm and the two cannot drift in what they accept. The
//! meter is a type parameter, not a runtime flag: an unmetered build holds no ledger, so an
//! "impossible refusal" cannot be fabricated from one — the state is unrepresentable.

use core::fmt;
use core::mem::size_of;
use core::ops::Range;

use crate::budget::Budget;
use crate::digest::{GraphDigest, RulesDigest};
use crate::framing::Refusal;
use crate::identity::{AssertionId, KbUuid, KbVersion, TermId};
use crate::ledger::SharedLedger;
use crate::record::{AssertionRecord, TermMeta};
use crate::relation::{RuleProfile, RuleSet};

/// Which committed snapshot a value was computed from, and under which rules.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SnapshotId {
    /// KB identity; two KBs never share a version space.
    pub kb_uuid: KbUuid,
    /// Committed version this graph was read at.
    pub version: KbVersion,
    /// The rule configuration in force. Carries the profile *and* its digest, so
    /// `legacy-v1` and `formal-v1` are distinguishable from the value itself.
    pub rules: RuleSet,
    /// Graph-relevant fields of this snapshot; the cache key for results.
    pub graph_digest: GraphDigest,
}

impl SnapshotId {
    /// The profile in force.
    #[must_use]
    pub const fn profile(&self) -> RuleProfile {
        self.rules.profile()
    }

    /// The rules digest.
    #[must_use]
    pub const fn rules_digest(&self) -> RulesDigest {
        self.rules.digest()
    }
}

/// Why a graph input was rejected at construction.
///
/// These checks make the accessors total: a built graph always has resolvable
/// endpoints and unique identities, so an executor never re-validates
/// references while evaluating.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum GraphBuildError {
    /// The same term ID appeared twice.
    DuplicateTerm(TermId),
    /// The same assertion ID appeared twice.
    ///
    /// Detected through the identity index, not by comparing adjacent records: two
    /// assertions sharing an ID but sitting at different endpoints can have other
    /// records between them in visit order.
    DuplicateAssertion(AssertionId),
    /// An assertion endpoint is not a term in this graph.
    UnknownTerm {
        /// Endpoint that could not be resolved.
        term: TermId,
        /// Assertion carrying the dangling reference.
        assertion: AssertionId,
    },
    /// `from == target`, which every authored relation forbids (V2).
    SelfReference(AssertionId),
    /// The input or the derived index would exceed the build budget.
    ///
    /// Checked before allocating, so a hostile or accidental oversized input is
    /// rejected instead of being admitted until the allocator fails.
    BudgetExceeded {
        /// Limit that would be exceeded.
        limit: BuildLimit,
    },
    /// The record count does not fit `u32` index positions.
    ///
    /// Saturation would silently alias two records onto one position; refusing is
    /// the only correct answer because the wire format uses `u32` positions.
    TooLarge {
        /// Records supplied.
        records: usize,
    },
    /// The resident extent does not fit `u64` and cannot be described at all.
    ///
    /// Distinct from a budget refusal: no allowance would help, and this is not an
    /// allocator verdict. The arithmetic that produced the figure genuinely failed.
    ExtentOverflow,
    /// A reservation returned more slots than the admitted extent.
    UnexpectedCapacity {
        /// Slots admitted before allocating.
        requested: usize,
        /// Actual reported backing capacity.
        actual: usize,
    },
    /// A derived index could not be allocated.
    ///
    /// Distinct from [`Self::BudgetExceeded`] and [`Self::ExtentOverflow`]: admission said
    /// the build fits, the arithmetic succeeded, but the allocator still refused. Reported
    /// instead of aborting.
    AllocationFailed,
}

/// Why a ledger-bounded build did not produce a graph.
///
/// Deliberately a **separate** type rather than another variant on
/// [`GraphBuildError`]: the graph verdicts are stable `Copy + Hash` values with a
/// fixed shape, and a caller-refusal payload ([`Refusal`]) is neither. Folding them
/// would force every consumer of the graph verdicts to carry a ledger payload it
/// never produces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphBuildRefusal {
    /// The graph itself is invalid, or could not be indexed.
    Invalid(GraphBuildError),
    /// The caller's shared ledger refused this build, or the caller stopped.
    ///
    /// The payload says which unit ran out (bytes or entries) or that the caller
    /// withdrew; those have different remedies, so it is never flattened.
    Refused(Refusal),
}

impl From<GraphBuildError> for GraphBuildRefusal {
    fn from(error: GraphBuildError) -> Self {
        Self::Invalid(error)
    }
}

/// Which build-side limit was hit.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BuildLimit {
    /// `max_work_rows`: terms plus assertions plus derived index entries.
    Rows,
    /// `max_work_bytes`: derived index bytes.
    Bytes,
}

impl BuildLimit {
    /// Stable name for error payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rows => "max_work_rows",
            Self::Bytes => "max_work_bytes",
        }
    }
}

impl fmt::Display for BuildLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for GraphBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTerm(term) => write!(f, "duplicate term {}", term.get()),
            Self::DuplicateAssertion(assertion) => {
                write!(f, "duplicate assertion {}", assertion.get())
            }
            Self::UnknownTerm { term, assertion } => write!(
                f,
                "assertion {} references unknown term {}",
                assertion.get(),
                term.get()
            ),
            Self::SelfReference(assertion) => {
                write!(f, "assertion {} is a self reference", assertion.get())
            }
            Self::BudgetExceeded { limit } => write!(f, "build budget exceeded: {limit}"),
            Self::TooLarge { records } => {
                write!(f, "{records} records exceed the u32 index position space")
            }
            Self::ExtentOverflow => f.write_str("the graph's resident extent does not fit u64"),
            Self::UnexpectedCapacity { requested, actual } => {
                write!(f, "reserved {requested} graph slots but received {actual}")
            }
            Self::AllocationFailed => f.write_str("could not allocate the derived graph index"),
        }
    }
}

impl std::error::Error for GraphBuildError {}

/// Bytes charged for one `u32` index position.
const INDEX_POSITION_BYTES: u64 = 4;

/// The resident extent a graph of these inputs will hold.
///
/// One canonical API, so a caller (the snapshot adapter) and the build itself cannot
/// disagree about what the graph costs. Every figure derives from **capacities the caller
/// actually reserved** plus the exact sizes of the derived vectors; nothing is extrapolated
/// from a row count or from `Vec` growth, and nothing is duplicated in the caller.
///
/// `bytes` is the full retained figure — moved-in inputs plus derived index — and is what
/// [`BaseGraph::admitted_bound_bytes`] reports. `input_bytes` is the part the caller has
/// already paid for; `index_bytes` is the part a metered build charges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GraphExtent {
    /// Full retained bytes: both moved-in input vectors at capacity plus the derived index
    /// at its conservative upper bound.
    pub bytes: u64,
    /// The portion attributable to the caller's already-admitted input vectors.
    pub input_bytes: u64,
    /// The portion attributable to index and adjacency storage this build derives.
    pub index_bytes: u64,
    /// Derived index storage in the ledger's **entry** unit, at the same upper bound as
    /// [`Self::index_bytes`]: two position slots per record plus two grouping slots per
    /// term. A slot is not a byte.
    pub entries: u64,
}

impl GraphExtent {
    /// The extent for inputs whose vectors already hold their final capacity.
    ///
    /// Pass `capacity()`, not `len()`: those allocations move into the graph and stay resident
    /// for its whole life, so charging `len` would understate a caller that reserved more than
    /// it filled.
    ///
    /// # Errors
    ///
    /// [`GraphBuildError::ExtentOverflow`] when any checked multiply/add fails. The figures are
    /// reported rather than saturated: a saturated extent would silently under-charge.
    pub fn for_capacities(
        term_capacity: usize,
        record_capacity: usize,
    ) -> Result<Self, GraphBuildError> {
        let terms = u64::try_from(term_capacity).map_err(|_| GraphBuildError::ExtentOverflow)?;
        let records =
            u64::try_from(record_capacity).map_err(|_| GraphBuildError::ExtentOverflow)?;

        let term_bytes = terms
            .checked_mul(size_of::<TermMeta>() as u64)
            .ok_or(GraphBuildError::ExtentOverflow)?;
        let record_bytes = records
            .checked_mul(size_of::<AssertionRecord>() as u64)
            .ok_or(GraphBuildError::ExtentOverflow)?;
        let input_bytes = term_bytes
            .checked_add(record_bytes)
            .ok_or(GraphBuildError::ExtentOverflow)?;

        // Two position vectors (`incoming_positions`, `by_id`), one u32 slot per record each.
        let positions = records
            .checked_mul(2)
            .ok_or(GraphBuildError::ExtentOverflow)?;
        let position_bytes = positions
            .checked_mul(INDEX_POSITION_BYTES)
            .ok_or(GraphBuildError::ExtentOverflow)?;
        // A grouping holds at most one key per distinct endpoint, never more than a term.
        let grouping_slots = terms
            .checked_mul(2)
            .ok_or(GraphBuildError::ExtentOverflow)?;
        let grouping_entry = (size_of::<TermId>() + size_of::<Range<u32>>()) as u64;
        let grouping_bytes = grouping_slots
            .checked_mul(grouping_entry)
            .ok_or(GraphBuildError::ExtentOverflow)?;

        let index_bytes = position_bytes
            .checked_add(grouping_bytes)
            .ok_or(GraphBuildError::ExtentOverflow)?;
        let bytes = input_bytes
            .checked_add(index_bytes)
            .ok_or(GraphBuildError::ExtentOverflow)?;
        let entries = positions
            .checked_add(grouping_slots)
            .ok_or(GraphBuildError::ExtentOverflow)?;

        Ok(Self {
            bytes,
            input_bytes,
            index_bytes,
            entries,
        })
    }
}

/// One build's admission and control channel, fixed at the type level.
///
/// The two implementations below are the whole vocabulary. An unmetered build literally has
/// no ledger, so no branch can report a refusal; a metered build's every bounded loop goes
/// through the same two methods. This is why the P0 path cannot fabricate an
/// `AllocationFailed` from an "impossible" refusal.
trait BuildMeter {
    /// Error this meter can produce.
    type Error: From<GraphBuildError>;

    /// Observe caller control and charge one entry of traversal work.
    fn step(&mut self) -> Result<(), Self::Error>;

    /// Admit the additional index storage before it is allocated.
    fn charge_index(&mut self, extent: GraphExtent) -> Result<(), Self::Error>;

    /// Report an actual failed reservation through this caller's error vocabulary.
    fn allocation_error(&self, requested: usize) -> Self::Error;
}

/// P0 meter: no ledger, no charge, no controllable stop.
struct Unmetered;

impl BuildMeter for Unmetered {
    type Error = GraphBuildError;

    fn step(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn charge_index(&mut self, _extent: GraphExtent) -> Result<(), Self::Error> {
        Ok(())
    }
    fn allocation_error(&self, _requested: usize) -> Self::Error {
        GraphBuildError::AllocationFailed
    }
}

/// Snapshot meter: the caller's own ledger, charged with real entries and real control.
struct LedgerMeter<'a> {
    ledger: &'a mut dyn SharedLedger,
}

impl BuildMeter for LedgerMeter<'_> {
    type Error = GraphBuildRefusal;

    fn step(&mut self) -> Result<(), Self::Error> {
        self.ledger
            .checkpoint()
            .map_err(|stop| GraphBuildRefusal::Refused(Refusal::Stopped(stop)))?;
        self.ledger
            .try_charge_entries(1)
            .map_err(|error| GraphBuildRefusal::Refused(Refusal::Budget(error)))
    }

    fn charge_index(&mut self, extent: GraphExtent) -> Result<(), Self::Error> {
        // Control first: a caller that already stopped must not have bytes admitted on its
        // behalf. Then bytes, then slots — each keeps its own unit and payload.
        self.ledger
            .checkpoint()
            .map_err(|stop| GraphBuildRefusal::Refused(Refusal::Stopped(stop)))?;
        self.ledger
            .try_charge(extent.index_bytes)
            .map_err(|error| GraphBuildRefusal::Refused(Refusal::Budget(error)))?;
        self.ledger
            .try_charge_entries(extent.entries)
            .map_err(|error| GraphBuildRefusal::Refused(Refusal::Budget(error)))
    }
    fn allocation_error(&self, requested: usize) -> Self::Error {
        GraphBuildRefusal::Refused(Refusal::Allocation { requested })
    }
}

/// Grouped ranges over an ordered position list.
///
/// Both vectors are sized exactly once from a counting pass, so no push can
/// reallocate: the admitted byte figure is the figure actually allocated.
#[derive(Debug)]
struct Grouping {
    keys: Vec<TermId>,
    ranges: Vec<Range<u32>>,
}

impl Grouping {
    /// Group `count` positions by `key_at`, which must be non-decreasing.
    ///
    /// The key sequence is read twice: once to count distinct keys, once to fill. Every
    /// position in **both** passes charges and observes caller control through `meter`, so a
    /// cancellation in the middle of the counting pass or the fill pass stops here.
    /// Reservations are exact and fallible, so an allocation failure becomes a typed error
    /// instead of an abort.
    fn build<F, M>(count: usize, key_at: F, meter: &mut M) -> Result<Self, M::Error>
    where
        F: Fn(usize) -> TermId,
        M: BuildMeter,
    {
        let mut distinct = 0usize;
        let mut previous: Option<TermId> = None;
        for position in 0..count {
            meter.step()?;
            let key = key_at(position);
            if previous != Some(key) {
                distinct += 1;
                previous = Some(key);
            }
        }

        let mut keys: Vec<TermId> = Vec::new();
        let mut ranges: Vec<Range<u32>> = Vec::new();
        reserve(&mut keys, distinct, meter)?;
        reserve(&mut ranges, distinct, meter)?;

        let mut start = 0usize;
        for position in 0..count {
            meter.step()?;
            let is_last = position + 1 == count;
            if is_last || key_at(position + 1) != key_at(position) {
                keys.push(key_at(position));
                ranges.push(cast_range(start..position + 1));
                start = position + 1;
            }
        }
        Ok(Self { keys, ranges })
    }

    /// Slots owned by both vectors.
    ///
    /// A *count of entries* across the two vectors, not bytes and not pairs.
    /// Callers that need bytes must not multiply this by a per-pair size — see
    /// [`Self::bytes`], which charges each vector by its own element size.
    fn slots(&self) -> u64 {
        (self.keys.capacity() + self.ranges.capacity()) as u64
    }

    /// Bytes owned by both vectors, each charged at its own element size.
    fn bytes(&self) -> u64 {
        (self.keys.capacity() as u64) * (size_of::<TermId>() as u64)
            + (self.ranges.capacity() as u64) * (size_of::<Range<u32>>() as u64)
    }

    fn range(&self, term: TermId) -> Range<u32> {
        match self.keys.binary_search(&term) {
            Ok(position) => self
                .ranges
                .get(position)
                .cloned()
                .unwrap_or_else(|| cast_range(0..0)),
            Err(_) => cast_range(0..0),
        }
    }
}

/// Reserve capacity without aborting on OOM.
fn reserve<T, M: BuildMeter>(
    vector: &mut Vec<T>,
    additional: usize,
    meter: &M,
) -> Result<(), M::Error> {
    let requested = vector
        .len()
        .checked_add(additional)
        .ok_or(GraphBuildError::ExtentOverflow)?;
    let bytes = requested
        .checked_mul(size_of::<T>())
        .ok_or(GraphBuildError::ExtentOverflow)?;
    vector
        .try_reserve_exact(additional)
        .map_err(|_| meter.allocation_error(bytes))?;
    if vector.capacity() != requested {
        return Err(GraphBuildError::UnexpectedCapacity {
            requested,
            actual: vector.capacity(),
        }
        .into());
    }
    Ok(())
}

/// Admit a P0 build and return the resident byte figure it will occupy.
///
/// `term_capacity`/`record_capacity` are the **capacity** of the caller's vectors,
/// not their length: those allocations are moved into the graph and stay resident
/// for its whole life, so charging `len` would understate a caller that reserved
/// more than it filled. Capacities are read before the move, so this costs nothing.
///
/// The figure is exactly [`GraphExtent::bytes`]; the entry figure is exactly
/// [`GraphExtent::entries`]. Peak during construction equals this figure: the sorts are in
/// place, and every derived vector is reserved exactly before being filled.
///
/// `sort_unstable_by_key` performs internal comparisons that this function does **not**
/// observe or interrupt: the metered loops below and in the indexing body are the real
/// boundary, and no finer granularity is claimed.
fn admit(
    term_capacity: usize,
    record_count: usize,
    record_capacity: usize,
    budget: Budget,
) -> Result<u64, GraphBuildError> {
    if u32::try_from(record_count).is_err() {
        return Err(GraphBuildError::TooLarge {
            records: record_count,
        });
    }
    let extent = GraphExtent::for_capacities(term_capacity, record_capacity)?;
    if extent.entries > budget.max_work_rows {
        return Err(GraphBuildError::BudgetExceeded {
            limit: BuildLimit::Rows,
        });
    }
    if extent.bytes > budget.max_work_bytes {
        return Err(GraphBuildError::BudgetExceeded {
            limit: BuildLimit::Bytes,
        });
    }
    Ok(extent.bytes)
}

/// Convert a position range. Admission rejected `count > u32::MAX`, so
/// saturation here would indicate an admission bug rather than ordinary data.
fn cast_range(range: Range<usize>) -> Range<u32> {
    match (u32::try_from(range.start), u32::try_from(range.end)) {
        (Ok(start), Ok(end)) => start..end,
        _ => 0..0,
    }
}

/// Immutable base graph at one committed version.
///
/// Construction validates and indexes once; every accessor afterwards is a slice
/// or a binary search. Deliberately not `Clone`: a second copy doubles the cost
/// of pinning a snapshot, so sharing happens through `&BaseGraph`.
#[derive(Debug)]
pub struct BaseGraph {
    snapshot: SnapshotId,
    terms: Vec<TermMeta>,
    assertions: Vec<AssertionRecord>,
    outgoing: Grouping,
    incoming_positions: Vec<u32>,
    incoming: Grouping,
    by_id: Vec<u32>,
    admitted_bytes: u64,
}

impl BaseGraph {
    /// Validate, order and index a candidate graph under a P0 [`Budget`].
    ///
    /// `budget` bounds construction only; it is the snapshot-build budget, not a
    /// per-query charge. It covers the graph's **resident** bytes after the move —
    /// including the caller's input vectors, which live as long as the graph — so
    /// a caller that hands over an over-reserved vector is charged for what it
    /// actually keeps alive. See [`Self::admitted_bound_bytes`].
    ///
    /// No ledger is involved and none is invented: this path has no refusal channel, so it
    /// returns [`GraphBuildError`] itself.
    ///
    /// # Errors
    ///
    /// - [`GraphBuildError::BudgetExceeded`] / [`GraphBuildError::TooLarge`] when the
    ///   input cannot be indexed inside `budget` or within `u32` positions;
    /// - [`GraphBuildError::ExtentOverflow`] when the resident extent cannot be described;
    /// - [`GraphBuildError::DuplicateTerm`] / [`GraphBuildError::DuplicateAssertion`]
    ///   for reused identities;
    /// - [`GraphBuildError::UnknownTerm`] / [`GraphBuildError::SelfReference`] for
    ///   dangling or self-referential assertions;
    /// - [`GraphBuildError::AllocationFailed`] only when a real `try_reserve_exact` fails.
    pub fn new(
        snapshot: SnapshotId,
        terms: Vec<TermMeta>,
        assertions: Vec<AssertionRecord>,
        budget: Budget,
    ) -> Result<Self, GraphBuildError> {
        let admitted_bytes = admit(
            terms.capacity(),
            assertions.len(),
            assertions.capacity(),
            budget,
        )?;
        Self::from_admitted_parts(snapshot, terms, assertions, admitted_bytes, &mut Unmetered)
    }

    /// Validate, order and index inputs whose bytes the caller already admitted.
    ///
    /// # Contract
    ///
    /// The two input vectors were **already byte-admitted by the caller** (before the
    /// fallible allocations that produced them). This method therefore charges only the
    /// *additional* index/grouping storage it derives — [`GraphExtent::index_bytes`] and
    /// [`GraphExtent::entries`] — and never re-charges `terms` or `assertions`, whose
    /// capacity is reported separately as [`GraphExtent::input_bytes`] so the caller can
    /// adopt the full retained figure exactly once.
    ///
    /// Every bounded loop (validation, identity index, both adjacency groupings, both
    /// position fills) observes caller control and charges one entry through `ledger`.
    /// Allocation of each derived vector follows its admission. Comparison work inside
    /// `sort_unstable_by_key` is **not** interruptible; that boundary is exact and is not
    /// claimed as metered.
    ///
    /// # Errors
    ///
    /// [`GraphBuildRefusal::Refused`] when the ledger refuses bytes or entries, or the
    /// caller stops. A real failed index reservation is `Refused(Allocation)` with its
    /// requested byte extent. Graph constraints and internal capacity/extent invariants
    /// arrive as [`GraphBuildRefusal::Invalid`], preserving their specific remedy.
    pub fn index_admitted_inputs(
        snapshot: SnapshotId,
        terms: Vec<TermMeta>,
        assertions: Vec<AssertionRecord>,
        ledger: &mut dyn SharedLedger,
    ) -> Result<Self, GraphBuildRefusal> {
        if u32::try_from(assertions.len()).is_err() {
            return Err(GraphBuildError::TooLarge {
                records: assertions.len(),
            }
            .into());
        }
        let extent = GraphExtent::for_capacities(terms.capacity(), assertions.capacity())
            .map_err(GraphBuildRefusal::Invalid)?;
        let mut meter = LedgerMeter { ledger };
        meter.charge_index(extent)?;
        Self::from_admitted_parts(snapshot, terms, assertions, extent.bytes, &mut meter)
    }

    /// The shared body of both constructors: validate, order, index.
    ///
    /// One implementation, so the two public entry points cannot drift in what they accept.
    /// `meter` is the only difference between them: [`Unmetered`] for a P0 [`Budget`] build,
    /// [`LedgerMeter`] for a caller-admitted one. Each bounded iteration calls
    /// `meter.step()`, and the empty input still performs an entry and an exit step so an
    /// already-stopped caller is never told the build succeeded.
    fn from_admitted_parts<M: BuildMeter>(
        snapshot: SnapshotId,
        mut terms: Vec<TermMeta>,
        mut assertions: Vec<AssertionRecord>,
        admitted_bytes: u64,
        meter: &mut M,
    ) -> Result<Self, M::Error> {
        // Control even for empty input.
        meter.step()?;

        // In-place everywhere: `sort_unstable_by_key` uses no scratch buffer, and
        // every key below is a total order over distinct records, so stability
        // would buy nothing.
        terms.sort_unstable_by_key(|term| term.id);
        for pair in terms.windows(2) {
            meter.step()?;
            if pair[0].id == pair[1].id {
                return Err(GraphBuildError::DuplicateTerm(pair[0].id).into());
            }
        }

        assertions.sort_unstable_by_key(AssertionRecord::visit_key);
        for assertion in &assertions {
            meter.step()?;
            if assertion.from == assertion.target {
                return Err(GraphBuildError::SelfReference(assertion.id).into());
            }
        }
        for assertion in &assertions {
            meter.step()?;
            for endpoint in [assertion.from, assertion.target] {
                if terms
                    .binary_search_by_key(&endpoint, |term| term.id)
                    .is_err()
                {
                    return Err(GraphBuildError::UnknownTerm {
                        term: endpoint,
                        assertion: assertion.id,
                    }
                    .into());
                }
            }
        }

        // Identity index, ordered by the ID alone. Duplicate detection must use
        // *this* order: two records sharing an ID can sit far apart in visit order.
        let mut by_id = position_index(assertions.len(), meter)?;
        by_id.sort_unstable_by_key(|position| assertions[*position as usize].id);
        for pair in by_id.windows(2) {
            meter.step()?;
            let left = assertions[pair[0] as usize].id;
            let right = assertions[pair[1] as usize].id;
            if left == right {
                return Err(GraphBuildError::DuplicateAssertion(left).into());
            }
        }

        // Outgoing adjacency: `assertions` is already grouped by `from`, so the
        // keys are read straight from the slice — no temporary key vector.
        let outgoing = Grouping::build(
            assertions.len(),
            |position| assertions[position].from,
            meter,
        )?;

        // Incoming adjacency: a second order over the same records, sorted by
        // (target, visit_key) so a target's edges stay in visit-key order.
        let mut incoming_positions = position_index(assertions.len(), meter)?;
        incoming_positions.sort_unstable_by_key(|position| {
            let assertion = &assertions[*position as usize];
            (assertion.target, assertion.visit_key())
        });
        let incoming = Grouping::build(
            assertions.len(),
            |position| assertions[incoming_positions[position] as usize].target,
            meter,
        )?;

        meter.step()?;

        Ok(Self {
            snapshot,
            terms,
            assertions,
            outgoing,
            incoming_positions,
            incoming,
            by_id,
            admitted_bytes,
        })
    }

    /// The extent a graph of these input capacities will occupy.
    ///
    /// Exposed so a caller can reserve exactly once, before building, and adopt afterwards
    /// instead of admitting the same bytes twice. Nothing here is re-derived in the caller.
    ///
    /// # Errors
    ///
    /// [`GraphBuildError::ExtentOverflow`] when the checked extent arithmetic fails.
    pub fn extent_for(
        term_capacity: usize,
        record_capacity: usize,
    ) -> Result<GraphExtent, GraphBuildError> {
        GraphExtent::for_capacities(term_capacity, record_capacity)
    }

    /// Slots actually owned by this graph, summed over every vector it holds.
    ///
    /// This is a **measured** figure: it reads `capacity()` from the real
    /// allocations, so a sparse graph whose groupings hold fewer keys than terms
    /// reports less than the admission bound, as it should.
    ///
    /// It is *not* OS RSS or allocator metadata. It counts owned vector slots
    /// only (records plus index entries); allocator bookkeeping, fragmentation and
    /// page granularity sit outside it. Do not present it as a hard RSS ceiling.
    #[must_use]
    pub fn owned_slots(&self) -> u64 {
        let terms = self.terms.capacity() as u64;
        let assertions = self.assertions.capacity() as u64;
        let outgoing = self.outgoing.slots();
        let incoming = self.incoming.slots();
        let positions = self.incoming_positions.capacity() as u64;
        let by_id = self.by_id.capacity() as u64;
        terms + assertions + outgoing + incoming + positions + by_id
    }

    /// Bytes actually owned by this graph, from real capacities.
    ///
    /// Measured, not the admission bound — see [`Self::owned_slots`] for the
    /// caveat that this is owned-slot accounting, not process RSS.
    #[must_use]
    pub fn owned_bytes(&self) -> u64 {
        (self.terms.capacity() as u64) * (size_of::<TermMeta>() as u64)
            + (self.assertions.capacity() as u64) * (size_of::<AssertionRecord>() as u64)
            + (self.incoming_positions.capacity() as u64) * INDEX_POSITION_BYTES
            + (self.by_id.capacity() as u64) * INDEX_POSITION_BYTES
            + self.outgoing.bytes()
            + self.incoming.bytes()
    }

    /// The conservative figure admitted against the build budget.
    ///
    /// An **upper bound**: groupings are charged at `term_capacity` entries each,
    /// while they only ever hold the number of *distinct* endpoints. Admission is
    /// deliberately conservative so a build cannot be admitted and then exceed its
    /// budget; the true figure is [`Self::owned_bytes`], which is <= this value.
    ///
    /// For [`Self::index_admitted_inputs`] this is the **full retained figure**
    /// ([`GraphExtent::bytes`]), including the caller's already-admitted input vectors, so
    /// a caller can adopt records-plus-graph exactly once without a second formula.
    #[must_use]
    pub const fn admitted_bound_bytes(&self) -> u64 {
        self.admitted_bytes
    }

    /// Snapshot this graph was read at.
    #[must_use]
    pub const fn snapshot(&self) -> SnapshotId {
        self.snapshot
    }

    /// The rule configuration this graph was indexed under.
    #[must_use]
    pub const fn rules(&self) -> RuleSet {
        self.snapshot.rules
    }

    /// Number of terms.
    #[must_use]
    pub fn term_count(&self) -> usize {
        self.terms.len()
    }

    /// Number of active assertions.
    #[must_use]
    pub fn assertion_count(&self) -> usize {
        self.assertions.len()
    }

    /// All terms, ordered by [`TermId`].
    #[must_use]
    pub fn terms(&self) -> &[TermMeta] {
        &self.terms
    }

    /// Look up one term.
    #[must_use]
    pub fn term(&self, id: TermId) -> Option<TermMeta> {
        self.terms
            .binary_search_by_key(&id, |term| term.id)
            .ok()
            .map(|position| self.terms[position])
    }

    /// All assertions, ordered by [`AssertionRecord::visit_key`].
    #[must_use]
    pub fn assertions(&self) -> &[AssertionRecord] {
        &self.assertions
    }

    /// Look up an assertion by identity.
    #[must_use]
    pub fn assertion(&self, id: AssertionId) -> Option<AssertionRecord> {
        self.by_id
            .binary_search_by_key(&id, |position| self.assertions[*position as usize].id)
            .ok()
            .map(|position| self.assertions[self.by_id[position] as usize])
    }

    /// Assertions authored with `from == term`, in visit-key order.
    #[must_use]
    pub fn assertions_from(&self, term: TermId) -> &[AssertionRecord] {
        let range = self.outgoing.range(term);
        &self.assertions[range.start as usize..range.end as usize]
    }

    /// Assertions authored with `target == term`, in visit-key order.
    ///
    /// A lookup over stored directions, not a symmetry completion: each returned
    /// assertion still reports its authored direction.
    #[must_use]
    pub fn assertions_to(&self, term: TermId) -> IncomingAssertions<'_> {
        let range = self.incoming.range(term);
        IncomingAssertions {
            assertions: &self.assertions,
            positions: &self.incoming_positions[range.start as usize..range.end as usize],
        }
    }

    /// Number of assertions authored from `term`.
    #[must_use]
    pub fn out_degree(&self, term: TermId) -> usize {
        let range = self.outgoing.range(term);
        (range.end - range.start) as usize
    }

    /// Number of assertions targeting `term`.
    #[must_use]
    pub fn in_degree(&self, term: TermId) -> usize {
        let range = self.incoming.range(term);
        (range.end - range.start) as usize
    }
}

/// Build `0..count` as `u32` positions, reserving exactly and fallibly.
///
/// The fill loop is metered, so a cancellation in the middle of the position fill stops
/// here rather than completing an index nobody asked for.
fn position_index<M: BuildMeter>(count: usize, meter: &mut M) -> Result<Vec<u32>, M::Error> {
    let mut positions: Vec<u32> = Vec::new();
    reserve(&mut positions, count, meter)?;
    for position in 0..count {
        meter.step()?;
        positions.push(counted(position));
    }
    Ok(positions)
}

fn counted(position: usize) -> u32 {
    // Admission rejected `records > u32::MAX`, so this cannot saturate.
    u32::try_from(position).unwrap_or(u32::MAX)
}

/// Assertions targeting one term, resolved through the reverse index.
///
/// Iteration is allocation-free: it walks the borrowed position slice.
#[derive(Debug, Clone, Copy)]
pub struct IncomingAssertions<'a> {
    assertions: &'a [AssertionRecord],
    positions: &'a [u32],
}

impl<'a> IncomingAssertions<'a> {
    /// Number of incoming assertions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.positions.len()
    }

    /// Whether the term has no incoming assertions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    /// Iterate in visit-key order. The returned items borrow `self`'s lifetime.
    pub fn iter(&self) -> IncomingIter<'a> {
        IncomingIter {
            assertions: self.assertions,
            positions: self.positions.iter(),
        }
    }
}

impl<'a> IntoIterator for IncomingAssertions<'a> {
    type Item = &'a AssertionRecord;
    type IntoIter = IncomingIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over assertions targeting one term.
#[derive(Clone, Debug)]
pub struct IncomingIter<'a> {
    assertions: &'a [AssertionRecord],
    positions: core::slice::Iter<'a, u32>,
}

impl<'a> Iterator for IncomingIter<'a> {
    type Item = &'a AssertionRecord;

    fn next(&mut self) -> Option<Self::Item> {
        let position = self.positions.next()?;
        // Admission proved every position is inside the assertion slice.
        self.assertions.get(*position as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confidence::Confidence;
    use crate::digest::Digest;
    use crate::identity::RecordRevision;
    use crate::ledger::{BudgetError, BudgetUnit};
    use crate::query::ControlStop;
    use crate::relation::RelationId;

    fn snapshot() -> SnapshotId {
        SnapshotId {
            kb_uuid: KbUuid::from_bytes([7; 16]),
            version: KbVersion::new(3),
            rules: RuleSet::from_trusted_parts(
                RuleProfile::FormalV1,
                RulesDigest::new(Digest::from_bytes([1; 32])),
            ),
            graph_digest: GraphDigest::new(Digest::from_bytes([2; 32])),
        }
    }

    fn terms(count: u32) -> Vec<TermMeta> {
        (0..count)
            .map(|id| TermMeta {
                id: TermId::new(id),
                layer: crate::layer::SemanticLayer::L1,
            })
            .collect()
    }

    fn assertions(count: u128, fan: u32) -> Vec<AssertionRecord> {
        (0..count)
            .map(|id| AssertionRecord {
                id: AssertionId::new(id + 1),
                revision: RecordRevision::FIRST,
                from: TermId::new((id as u32) % fan),
                relation: RelationId::IsA,
                target: TermId::new(fan + (id as u32) % fan),
                confidence: Confidence::Confirmed,
            })
            .collect()
    }

    /// A real `SharedLedger`: byte and entry allowances that can be exhausted, an
    /// admitted-work count, and a controllable stop after `stop_after` checkpoints.
    struct Meter {
        bytes: u64,
        entries: u64,
        spent_bytes: u64,
        spent_entries: u64,
        steps: u64,
        stop_after: Option<u64>,
    }

    impl Meter {
        fn new(bytes: u64, entries: u64) -> Self {
            Self {
                bytes,
                entries,
                spent_bytes: 0,
                spent_entries: 0,
                steps: 0,
                stop_after: None,
            }
        }

        fn stopping_after(steps: u64) -> Self {
            let mut meter = Self::new(u64::MAX, u64::MAX);
            meter.stop_after = Some(steps);
            meter
        }
    }

    impl SharedLedger for Meter {
        fn try_charge(&mut self, bytes: u64) -> Result<(), BudgetError> {
            if bytes > self.bytes {
                return Err(BudgetError::new(BudgetUnit::Bytes, bytes, self.bytes));
            }
            self.bytes -= bytes;
            self.spent_bytes += bytes;
            Ok(())
        }

        fn try_charge_entries(&mut self, count: u64) -> Result<(), BudgetError> {
            if count > self.entries {
                return Err(BudgetError::new(BudgetUnit::Entries, count, self.entries));
            }
            self.entries -= count;
            self.spent_entries += count;
            Ok(())
        }

        fn checkpoint(&mut self) -> Result<(), ControlStop> {
            self.steps += 1;
            if let Some(limit) = self.stop_after
                && self.steps > limit
            {
                return Err(ControlStop::Cancelled);
            }
            Ok(())
        }
    }

    fn inputs() -> (Vec<TermMeta>, Vec<AssertionRecord>) {
        // Six terms so both endpoints of every assertion resolve; three distinct `from`
        // values {0,1,2} and three distinct `target` values {3,4,5}.
        (terms(6), assertions(6, 3))
    }

    fn build_indexed(ledger: &mut Meter) -> Result<BaseGraph, GraphBuildRefusal> {
        let (terms, assertions) = inputs();
        BaseGraph::index_admitted_inputs(snapshot(), terms, assertions, ledger)
    }

    /// A metered build succeeds when the ledger has exactly the *index* figure: this is the
    /// test that proves the input arrays were not re-charged.
    #[test]
    fn index_build_charges_only_index_storage_and_not_the_input_arrays() {
        let (terms, assertions) = inputs();
        let extent =
            GraphExtent::for_capacities(terms.capacity(), assertions.capacity()).expect("extent");
        assert!(extent.input_bytes > 0);
        assert!(extent.index_bytes > 0);
        assert_eq!(extent.bytes, extent.input_bytes + extent.index_bytes);

        assert!(extent.entries > 0);

        // The byte allowance is EXACT: one byte of the caller's inputs re-charged would
        // refuse here. Entries are unbounded because traversal steps are real work on top of
        // the storage admission.
        let mut ledger = Meter::new(extent.index_bytes, u64::MAX);
        let graph = BaseGraph::index_admitted_inputs(snapshot(), terms, assertions, &mut ledger)
            .expect("exact index byte allowance must suffice");

        assert_eq!(ledger.spent_bytes, extent.index_bytes);
        assert_eq!(ledger.bytes, 0, "the whole index allowance was consumed");
        assert!(
            ledger.spent_entries >= extent.entries,
            "storage admission plus traversal steps, got {} for storage {}",
            ledger.spent_entries,
            extent.entries
        );

        // The reported bound is the full retained figure, which is what a caller adopts.
        assert_eq!(graph.admitted_bound_bytes(), extent.bytes);
        // Every retained byte is accounted for by the measured figure or is conservative
        // slack in the groupings; never the reverse.
        assert!(graph.owned_bytes() <= graph.admitted_bound_bytes());
        assert_eq!(graph.term_count(), 6);
        assert_eq!(graph.assertion_count(), 6);
    }

    /// One byte short of the index figure is refused **before** any index vector is
    /// allocated, and the refusal carries the real requested/remaining quantities.
    #[test]
    fn a_byte_shortage_refuses_before_index_allocation() {
        let (terms, assertions) = inputs();
        let extent =
            GraphExtent::for_capacities(terms.capacity(), assertions.capacity()).expect("extent");
        let mut ledger = Meter::new(extent.index_bytes - 1, extent.entries);
        let error = BaseGraph::index_admitted_inputs(snapshot(), terms, assertions, &mut ledger)
            .expect_err("one byte short must refuse");
        match error {
            GraphBuildRefusal::Refused(Refusal::Budget(budget)) => {
                assert_eq!(budget.unit(), BudgetUnit::Bytes);
                assert_eq!(budget.requested(), extent.index_bytes);
                assert_eq!(budget.remaining(), extent.index_bytes - 1);
            }
            other => panic!("expected a byte refusal, got {other:?}"),
        }
        assert_eq!(
            ledger.spent_bytes, 0,
            "a refused admission must not spend bytes"
        );
    }

    /// Entry exhaustion is its own unit and payload, and it is observed mid-traversal
    /// rather than only at entry.
    #[test]
    fn entry_budget_exhaustion_is_a_distinct_refusal() {
        let (terms, assertions) = inputs();
        let extent =
            GraphExtent::for_capacities(terms.capacity(), assertions.capacity()).expect("extent");
        let mut ledger = Meter::new(extent.index_bytes, extent.entries - 1);
        let error = BaseGraph::index_admitted_inputs(snapshot(), terms, assertions, &mut ledger)
            .expect_err("one entry short must refuse");
        match error {
            GraphBuildRefusal::Refused(Refusal::Budget(budget)) => {
                assert_eq!(budget.unit(), BudgetUnit::Entries);
                assert_eq!(budget.requested(), extent.entries);
            }
            other => panic!("expected an entry refusal, got {other:?}"),
        }
    }

    /// Cancellation is observed at **every** traversal boundary, including inside the two
    /// position fills and both grouping passes — not only at the entry step.
    ///
    /// Iterating every `k in 0..total` is what makes this true of the phases without naming
    /// them: each boundary has a `k` that stops exactly there, so a loop that forgot to meter
    /// would leave a `k` that succeeds and fail this test.
    #[test]
    fn cancellation_is_observed_at_every_traversal_boundary() {
        let (terms, assertions) = inputs();
        let term_count = terms.len() as u64;
        let record_count = assertions.len() as u64;

        let mut full = Meter::new(u64::MAX, u64::MAX);
        build_indexed(&mut full).expect("full build");
        let total = full.steps;

        // Structural count, written independently of the implementation: the index-admission
        // control check, entry, term-window comparisons, the self-reference walk, the endpoint
        // walk, `by_id` fill + window comparisons, `outgoing` count + fill,
        // `incoming_positions` fill, `incoming` count + fill, exit.
        let expected = 1
            + 1
            + term_count.saturating_sub(1)
            + record_count * 3
            + record_count.saturating_sub(1)
            + record_count * 2
            + record_count
            + record_count * 2
            + 1;
        assert_eq!(
            total, expected,
            "the metered step count must match the structural count"
        );
        assert!(total > 20, "fixture must exercise many steps, got {total}");

        for limit in 0..total {
            let mut ledger = Meter::stopping_after(limit);
            let error = build_indexed(&mut ledger).expect_err("must stop");
            assert_eq!(
                error,
                GraphBuildRefusal::Refused(Refusal::Stopped(ControlStop::Cancelled)),
                "limit {limit} must be a stop, not a budget or graph verdict"
            );
            assert_eq!(
                ledger.steps,
                limit + 1,
                "the stop must be observed on the very next step"
            );
        }

        // Allowing every step succeeds, so the boundary is exact.
        let mut exact = Meter::new(u64::MAX, u64::MAX);
        exact.stop_after = Some(total);
        build_indexed(&mut exact).expect("allowing every step must succeed");
    }

    /// The P0 path still reports graph verdicts directly and has no refusal arm.
    #[test]
    fn p0_new_reports_graph_verdicts_without_a_refusal_channel() {
        let terms = self::terms(2);
        let mut bad = self::assertions(1, 2);
        bad[0].target = TermId::new(99);
        let error: GraphBuildError =
            BaseGraph::new(snapshot(), terms, bad, Budget::DEFAULT).unwrap_err();
        assert_eq!(
            error,
            GraphBuildError::UnknownTerm {
                term: TermId::new(99),
                assertion: AssertionId::new(1),
            }
        );
    }

    /// Extent arithmetic is checked, never saturated.
    #[test]
    fn extent_overflow_is_typed_and_not_saturated() {
        let error = GraphExtent::for_capacities(usize::MAX, usize::MAX).unwrap_err();
        assert_eq!(error, GraphBuildError::ExtentOverflow);
        assert_eq!(
            BaseGraph::extent_for(usize::MAX, usize::MAX).unwrap_err(),
            GraphBuildError::ExtentOverflow
        );
    }

    /// The two entry points accept exactly the same inputs: the metered path is not laxer.
    #[test]
    fn both_entry_points_agree_on_rejection() {
        let cases: Vec<(Vec<TermMeta>, Vec<AssertionRecord>)> = vec![
            (self::terms(2), vec![bad_assertion(1, 1, 1)]),
            (
                vec![
                    TermMeta {
                        id: TermId::new(1),
                        layer: crate::layer::SemanticLayer::L1,
                    };
                    2
                ],
                Vec::new(),
            ),
        ];
        for (terms, assertions) in cases {
            let p0 = BaseGraph::new(
                snapshot(),
                terms.clone(),
                assertions.clone(),
                Budget::DEFAULT,
            )
            .unwrap_err();
            let mut ledger = Meter::new(u64::MAX, u64::MAX);
            let metered =
                BaseGraph::index_admitted_inputs(snapshot(), terms, assertions, &mut ledger)
                    .unwrap_err();
            assert_eq!(metered, GraphBuildRefusal::Invalid(p0));
        }
    }

    fn bad_assertion(id: u128, from: u32, target: u32) -> AssertionRecord {
        AssertionRecord {
            id: AssertionId::new(id),
            revision: RecordRevision::FIRST,
            from: TermId::new(from),
            relation: RelationId::IsA,
            target: TermId::new(target),
            confidence: Confidence::Confirmed,
        }
    }
}
