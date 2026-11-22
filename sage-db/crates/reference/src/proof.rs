//! Strongest-derivation selection and independent proof replay.
//!
//! # Selection
//!
//! Saturation answers *which annotations are reachable*. Selection answers *which
//! derivation witnesses each of them*, and it is a second fixed point: an R1
//! candidate needs both halves selected before it can compete.
//!
//! ## Why one winner per fact is not enough
//!
//! Composition is `min`, so a *weaker* prefix can be the better building block
//! downstream: `A -h-> B` (1 hop) and `A -c-> X -c-> Y -c-> B` (3 hops) both give
//! confirmed facts at `B`, but only the 1-hop one keeps `B -h-> T` at 2 hops. Keeping
//! a single winner for `A -> B` therefore decides `A -> T` by whichever prefix won
//! locally — a decision that depends on term-id order and silently yields a proof
//! that is not shortest.
//!
//! ## What is kept
//!
//! For each `FactKey` the table holds at most one candidate per confidence level
//! (three, from the finite carrier), and within a level only the **shortest** one,
//! ties broken by the canonical [`Key`]. Each stored candidate is therefore
//! non-dominated on `(strength, hops)`: no other candidate of that key is both at
//! least as strong and strictly shorter.
//!
//! Composition relaxes over *level pairs*, taking `min` of the two levels, and only
//! displaces a stored candidate when it strictly improves `(hops, descriptor)` inside
//! its level. The reported proof is the strongest stored level, which is shortest by
//! construction. This is still one ordered-table fixed point — no confidence
//! thresholds, no per-threshold BFS passes, nothing shared with the optimized
//! executor.
//!
//! ## Reported strength is not decided here
//!
//! Selection can never *raise* what saturation reached, because every candidate's
//! level is derived from an authored assertion or an R2/R3 seed at that same level.
//! The reported annotation is still the saturated one; selection only supplies the
//! witness.
//!
//! # Replay
//!
//! [`replay`] recomputes endpoints, relation and annotation bottom-up from an arena
//! and the graph alone. It never reads this crate's table or plans, so it cannot
//! inherit a selection bug. It checks the *direction* of every node, which is what
//! makes a forged proof detectable:
//!
//! - `Asserted` must name an active assertion with the active revision, and its
//!   `from`/`relation`/`target` become that node's endpoints;
//! - `Symmetry` requires a symmetric relation under the active rules and swaps the
//!   endpoints; it may not take a rule node as its premise, because `R2(R2(a))`
//!   returns to the authored direction and is not a new source;
//! - `DefinitionImpliesDependency` requires the reported relation to be
//!   `depends-on`, a premise relation the active profile really turns into a
//!   dependency, and an authored premise;
//! - `Transitive` requires a transitive relation, both halves under that same
//!   relation, and `left.to == right.from`. Swapping the operands breaks the join
//!   unless the fact is a self reference, which construction forbids.
//!
//! The caller passes the endpoints it requires, so an internally consistent proof
//! of a *different* fact is still rejected.

use sage_db_types::{
    Annotation, AssertionId, BaseGraph, ProofArena, ProofNode, ProofRef, QueryControl, QueryError,
    RecordRevision, RelationId, RuleSet, TermId,
};

use crate::facts::{FactKey, FactTable, SupportWitness};
use crate::index::RawIndex;
use crate::work::Work;

/// One node of a selected derivation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanNode {
    /// An authored assertion, in its own direction.
    Asserted {
        /// Assertion identity.
        id: AssertionId,
        /// Revision active at the queried snapshot.
        revision: RecordRevision,
    },
    /// R2 applied once to an authored assertion.
    Symmetry {
        /// The authored direction.
        premise: u32,
    },
    /// R3 applied once to an authored definition assertion.
    DefinitionImpliesDependency {
        /// The definition direction.
        premise: u32,
    },
    /// R1 applied once: `left` is `from → mid`, `right` is `mid → to`.
    Transitive {
        /// First half.
        left: u32,
        /// Second half.
        right: u32,
    },
}

/// Canonical descriptor of a derivation, used only for tie-breaking.
///
/// Ordered by variant first, so an authored fact outranks a rule derivation of the
/// same strength and length. Two descriptors are equal only when they describe the
/// same derivation, so the comparison never depends on how a candidate was found.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Key {
    /// An authored assertion.
    Asserted(AssertionId),
    /// R2 over an authored assertion of `relation`.
    Symmetry(RelationId, AssertionId),
    /// R3 from an authored definition assertion of `relation`.
    Definition(RelationId, AssertionId),
    /// R1 over two facts, named by their endpoints.
    Transitive {
        /// First half subject.
        left_from: TermId,
        /// First half object.
        left_to: TermId,
        /// Second half subject.
        right_from: TermId,
        /// Second half object.
        right_to: TermId,
    },
}

/// Strength rank of an annotation: 0 for absent, then the confidence ordinal + 1.
///
/// `Annotation` deliberately has no `Ord` (absent must never sort above contested),
/// so selection projects it here explicitly.
const fn strength(annotation: Annotation) -> u8 {
    match annotation.confidence() {
        None => 0,
        Some(confidence) => confidence as u8 + 1,
    }
}

/// Number of confidence levels the carrier has. The table keeps at most one
/// candidate per level, per fact.
const LEVELS: usize = 3;

/// The winning derivation for one fact direction *at one confidence level*.
///
/// `node` is the shortest derivation found at `annotation`'s level; `descriptor`
/// only breaks ties between equally short ones.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Candidate {
    node: u32,
    annotation: Annotation,
    hops: u32,
    descriptor: Key,
}

/// The non-dominated candidates for one fact direction.
///
/// One slot per confidence level, in ascending level order. A slot is `None` when no
/// derivation at that strength is known. Keeping all levels is what stops a strong
/// long route from hiding a weak short one that composes better downstream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Winners {
    key: FactKey,
    slots: [Option<Candidate>; LEVELS],
}

impl Winners {
    fn new(key: FactKey) -> Self {
        Self {
            key,
            slots: [None; LEVELS],
        }
    }

    /// The candidate to report: the strongest level that has one.
    ///
    /// Within a level the stored candidate is already the shortest, so this is
    /// "strongest, then shortest" without a second comparison.
    fn reported(&self) -> Option<Candidate> {
        self.slots.iter().rev().flatten().copied().next()
    }

    /// Every stored candidate, strongest level first.
    fn levels(&self) -> impl Iterator<Item = Candidate> + '_ {
        self.slots.iter().rev().flatten().copied()
    }

    /// Whether a candidate at `annotation`/`hops`/`descriptor` would displace the
    /// stored one at its level.
    ///
    /// Checked *before* materialising, so a losing alternative costs one comparison
    /// instead of a dead plan node and its bytes.
    fn admits(&self, annotation: Annotation, hops: u32, descriptor: Key) -> bool {
        let rank = strength(annotation);
        if rank == 0 {
            return false;
        }
        match self.slots[rank as usize - 1] {
            None => true,
            Some(existing) => {
                (core::cmp::Reverse(hops), core::cmp::Reverse(descriptor))
                    > (
                        core::cmp::Reverse(existing.hops),
                        core::cmp::Reverse(existing.descriptor),
                    )
            }
        }
    }

    fn store(&mut self, candidate: Candidate) {
        let rank = strength(candidate.annotation);
        debug_assert!(rank > 0, "absent candidates are never stored");
        self.slots[rank as usize - 1] = Some(candidate);
    }
}

/// Selected derivations for the facts of one relation.
#[derive(Debug, Default)]
pub struct PlanTable {
    nodes: Vec<PlanNode>,
    annotation: Vec<Annotation>,
    hops: Vec<u32>,
    winners: Vec<Winners>,
}

impl PlanTable {
    /// Empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Derivation node at `node`.
    pub fn node(&self, node: u32) -> Option<PlanNode> {
        self.nodes.get(node as usize).copied()
    }

    /// Root node to report for `key`: the strongest stored level.
    pub fn root(&self, key: &FactKey) -> Option<u32> {
        self.winners_for(key)
            .and_then(Winners::reported)
            .map(|c| c.node)
    }

    /// Hop count of the reported derivation for `key`, when one exists.
    ///
    /// Exposed so tests can assert shortest-ness directly instead of inferring it
    /// from arena shape.
    pub fn hops(&self, key: &FactKey) -> Option<u32> {
        self.winners_for(key)
            .and_then(Winners::reported)
            .map(|c| c.hops)
    }

    /// Number of facts with at least one selected derivation.
    pub fn len(&self) -> usize {
        self.winners.len()
    }

    /// Whether nothing was selected.
    pub fn is_empty(&self) -> bool {
        self.winners.is_empty()
    }

    fn winners_for(&self, key: &FactKey) -> Option<&Winners> {
        self.winners
            .binary_search_by(|candidate| candidate.key.cmp(key))
            .ok()
            .map(|position| &self.winners[position])
    }

    /// Whether any stored level of `key` admits this candidate.
    fn admits(&self, key: FactKey, annotation: Annotation, hops: u32, descriptor: Key) -> bool {
        match self.winners_for(&key) {
            Some(winners) => winners.admits(annotation, hops, descriptor),
            None => strength(annotation) > 0,
        }
    }

    /// Materialise a support node that is not itself a reportable fact.
    fn insert_raw(
        &mut self,
        node: PlanNode,
        annotation: Annotation,
        hops: u32,
        work: &mut Work,
    ) -> Result<u32, QueryError> {
        let position = u32::try_from(self.nodes.len())
            .map_err(|_| QueryError::BudgetExceeded(sage_db_types::BudgetLimit::WorkRows))?;
        crate::work::reserve(&mut self.nodes, 1, work)?;
        crate::work::reserve(&mut self.annotation, 1, work)?;
        crate::work::reserve(&mut self.hops, 1, work)?;
        self.nodes.push(node);
        self.annotation.push(annotation);
        self.hops.push(hops);
        Ok(position)
    }

    /// Materialise a candidate and store it at its level.
    ///
    /// The caller has already proved it displaces the stored candidate with
    /// [`Self::admits`].
    fn insert(
        &mut self,
        key: FactKey,
        node: PlanNode,
        descriptor: Key,
        annotation: Annotation,
        hops: u32,
        work: &mut Work,
    ) -> Result<(), QueryError> {
        let position = self.insert_raw(node, annotation, hops, work)?;
        let candidate = Candidate {
            node: position,
            annotation,
            hops,
            descriptor,
        };
        match self
            .winners
            .binary_search_by(|existing| existing.key.cmp(&key))
        {
            Ok(existing) => {
                work.row()?;
                self.winners[existing].store(candidate);
            }
            Err(existing) => {
                work.row()?;
                crate::work::reserve(&mut self.winners, 1, work)?;
                let mut winners = Winners::new(key);
                winners.store(candidate);
                self.winners.insert(existing, winners);
            }
        }
        Ok(())
    }
}

/// Select one strongest derivation per fact of `relation`.
///
/// # Errors
///
/// Propagates budget and control failures. A selection that fails to converge is
/// reported, never returned as an arbitrary plan.
pub fn select_all(
    index: &RawIndex,
    table: &FactTable,
    rules: RuleSet,
    relation: RelationId,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<PlanTable, QueryError> {
    let mut plans = PlanTable::new();

    // Single-hop candidates are independent of any other selection.
    for entry in table.iter() {
        work.step(1)?;
        work.periodic_check(control)?;
        if entry.key.relation != relation {
            continue;
        }
        single_hop(&mut plans, index, rules, entry.key, work, control)?;
    }

    // R1 candidates need both halves selected, so relax until no level of any fact
    // is displaced any more. A round that changes nothing is the fixed point.
    //
    // Composition runs over *level pairs*: `min` of the two levels is the level of
    // the composition, so a weak short prefix competes at its own level instead of
    // being compared against — and lost to — a stronger long route to the midpoint.
    let rounds = table
        .len()
        .saturating_mul(LEVELS)
        .saturating_mul(2)
        .saturating_add(8);
    for _ in 0..rounds {
        let mut changed = false;
        for left in table.iter() {
            work.step(1)?;
            work.periodic_check(control)?;
            if left.key.relation != relation || !rules.spec(relation).transitive {
                continue;
            }
            let Some(left_winners) = plans.winners_for(&left.key).copied() else {
                continue;
            };
            for right in table.group(relation, left.key.to) {
                work.step(1)?;
                work.periodic_check(control)?;
                let Some(right_winners) = plans.winners_for(&right.key).copied() else {
                    continue;
                };
                let key = FactKey::new(left.key.from, relation, right.key.to);
                let descriptor = Key::Transitive {
                    left_from: left.key.from,
                    left_to: left.key.to,
                    right_from: right.key.from,
                    right_to: right.key.to,
                };
                for left_candidate in left_winners.levels() {
                    for right_candidate in right_winners.levels() {
                        let annotation = left_candidate.annotation.meet(right_candidate.annotation);
                        let hops = left_candidate.hops.saturating_add(right_candidate.hops);
                        if !plans.admits(key, annotation, hops, descriptor) {
                            continue;
                        }
                        plans.insert(
                            key,
                            PlanNode::Transitive {
                                left: left_candidate.node,
                                right: right_candidate.node,
                            },
                            descriptor,
                            annotation,
                            hops,
                            work,
                        )?;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            return Ok(plans);
        }
        work.periodic_check(control)?;
    }

    Err(QueryError::InvalidRequest {
        reason: "derivation selection did not converge",
    })
}

/// Push the authoring, R2 and R3 candidates for one fact direction.
fn single_hop(
    plans: &mut PlanTable,
    index: &RawIndex,
    rules: RuleSet,
    key: FactKey,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<(), QueryError> {
    // Authoring: the strongest authored support in this exact direction. Read from
    // the raw records, not from the table: the table is a derived view, and a proof
    // must cite the authored assertion itself.
    if let Some(witness) = witness_of(index, key, work, control)? {
        let descriptor = Key::Asserted(witness.id);
        let annotation = Annotation::present(witness.confidence);
        if plans.admits(key, annotation, 1, descriptor) {
            plans.insert(
                key,
                PlanNode::Asserted {
                    id: witness.id,
                    revision: witness.revision,
                },
                descriptor,
                annotation,
                1,
                work,
            )?;
        }
    }

    let spec = rules.spec(key.relation);

    // R2: one application to an authored assertion in the *opposite* direction. The
    // premise must be authored, otherwise a round trip would look like a source.
    if spec.symmetric {
        let reverse = FactKey::new(key.to, key.relation, key.from);
        if let Some(witness) = witness_of(index, reverse, work, control)? {
            let descriptor = Key::Symmetry(key.relation, witness.id);
            let annotation = Annotation::present(witness.confidence);
            if plans.admits(key, annotation, 1, descriptor) {
                // The premise is a leaf of the same candidate, so it is inserted with
                // it rather than left behind when the candidate loses.
                let premise = plans.insert_raw(assert_node(witness), annotation, 1, work)?;
                plans.insert(
                    key,
                    PlanNode::Symmetry { premise },
                    descriptor,
                    annotation,
                    1,
                    work,
                )?;
            }
        }
    }

    // R3: one application to an authored definition assertion with the same
    // endpoints, and only when this profile implies the dependency.
    if key.relation == RelationId::DependsOn {
        for definition in [
            RelationId::IsA,
            RelationId::PartOf,
            RelationId::Instantiates,
        ] {
            if !rules.implies_dependency(definition) {
                continue;
            }
            let definition_key = FactKey::new(key.from, definition, key.to);
            let Some(witness) = witness_of(index, definition_key, work, control)? else {
                continue;
            };
            let descriptor = Key::Definition(definition, witness.id);
            let annotation = Annotation::present(witness.confidence);
            if plans.admits(key, annotation, 1, descriptor) {
                let premise = plans.insert_raw(assert_node(witness), annotation, 1, work)?;
                plans.insert(
                    key,
                    PlanNode::DefinitionImpliesDependency { premise },
                    descriptor,
                    annotation,
                    1,
                    work,
                )?;
            }
        }
    }

    Ok(())
}

/// The strongest authored support for `key`, from the raw records.
fn witness_of(
    index: &RawIndex,
    key: FactKey,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<Option<SupportWitness>, QueryError> {
    // Explicitly scanned here rather than delegated to a grouping helper: the supports
    // that share one triple are independent records, and reading them is real work that
    // must be charged and interruptible even though `single_hop` charges one step.
    let assertion = index.strongest_authored(key.from, key.relation, key.to, work, control)?;
    Ok(assertion.map(|assertion| SupportWitness {
        id: assertion.id,
        revision: assertion.revision,
        confidence: assertion.confidence,
    }))
}

/// The authored leaf for `witness`.
fn assert_node(witness: SupportWitness) -> PlanNode {
    PlanNode::Asserted {
        id: witness.id,
        revision: witness.revision,
    }
}

/// Renders selected derivations into one shared, memoised arena.
///
/// The memo is keyed by plan node and shared across every row of a result, so a
/// prefix used by several facts is written once. Without it a chain would store one
/// copy of the prefix per row, which is exactly the quadratic arena growth the
/// budget must not have to absorb.
pub struct Renderer<'a> {
    plans: &'a PlanTable,
    rendered: Vec<Option<ProofRef>>,
    /// Scratch post-order stack, allocated once for the whole result.
    ///
    /// Reusing it is what keeps a multi-row result from charging its traversal
    /// scratch once per row; the renderer's own state is charged on growth only.
    stack: Vec<(u32, bool)>,
}

impl<'a> Renderer<'a> {
    /// Empty renderer over `plans`.
    pub fn new(plans: &'a PlanTable) -> Self {
        Self {
            plans,
            rendered: Vec::new(),
            stack: Vec::new(),
        }
    }

    /// Number of plan nodes already materialised.
    pub fn rendered(&self) -> usize {
        self.rendered.len()
    }

    /// Render `key` and return its root.
    ///
    /// # Errors
    ///
    /// Propagates arena limits, budget and control failures.
    pub fn render(
        &mut self,
        key: FactKey,
        arena: &mut ProofArena,
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<ProofRef, QueryError> {
        let root = self.plans.root(&key).ok_or(QueryError::InvalidRequest {
            reason: "no derivation selected for a returned fact",
        })?;
        self.render_node(root, arena, work, control)
    }

    fn render_node(
        &mut self,
        node: u32,
        arena: &mut ProofArena,
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<ProofRef, QueryError> {
        let width = self.plans.nodes.len();
        if self.rendered.len() < width {
            let missing = width - self.rendered.len();
            crate::work::reserve(&mut self.rendered, missing, work)?;
            self.rendered.resize(width, None);
        }
        if let Some(existing) = self.rendered[node as usize] {
            return Ok(existing);
        }

        // Postorder without recursion: a premise must exist before its use.
        if self.stack.capacity() == 0 {
            crate::work::reserve(&mut self.stack, 16, work)?;
        }
        self.stack.push((node, false));
        while let Some((current, expanded)) = self.stack.pop() {
            work.step(1)?;
            work.periodic_check(control)?;
            if self.rendered[current as usize].is_some() {
                continue;
            }
            let plan = self.plans.node(current).ok_or(QueryError::InvalidRequest {
                reason: "derivation references a missing node",
            })?;
            let premises: [Option<u32>; 2] = match plan {
                PlanNode::Asserted { .. } => [None, None],
                PlanNode::Symmetry { premise }
                | PlanNode::DefinitionImpliesDependency { premise } => [Some(premise), None],
                PlanNode::Transitive { left, right } => [Some(left), Some(right)],
            };
            if !expanded {
                let needed = 1 + premises
                    .iter()
                    .flatten()
                    .filter(|premise| self.rendered[**premise as usize].is_none())
                    .count();
                if self.stack.len() + needed > self.stack.capacity() {
                    crate::work::reserve(&mut self.stack, needed.max(16), work)?;
                }
                self.stack.push((current, true));
                for premise in premises.into_iter().flatten().rev() {
                    if self.rendered[premise as usize].is_none() {
                        self.stack.push((premise, false));
                    }
                }
                continue;
            }
            let proof_node = match plan {
                PlanNode::Asserted { id, revision } => ProofNode::Asserted {
                    assertion_id: id,
                    revision,
                },
                PlanNode::Symmetry { premise } => ProofNode::Symmetry {
                    premise: self.rendered[premise as usize].ok_or(missing_premise())?,
                },
                PlanNode::DefinitionImpliesDependency { premise } => {
                    ProofNode::DefinitionImpliesDependency {
                        premise: self.rendered[premise as usize].ok_or(missing_premise())?,
                    }
                }
                PlanNode::Transitive { left, right } => ProofNode::Transitive {
                    left: self.rendered[left as usize].ok_or(missing_premise())?,
                    right: self.rendered[right as usize].ok_or(missing_premise())?,
                },
            };
            work.proof_node()?;
            let reference = push_charged(arena, proof_node, work, control)?;
            self.rendered[current as usize] = Some(reference);
        }

        self.rendered[node as usize].ok_or(missing_premise())
    }
}

/// Push one node, charging the arena's **resident** growth to the shared ledger.
///
/// Three rules, all of which the arena cannot enforce for itself:
///
/// 1. **Charge the doubling, not one slot.** A `Vec` that grows keeps the doubled
///    capacity alive, so charging `len * slot_bytes` would understate real memory by
///    up to 2x. The expected growth is computed exactly as [`ProofArena::push`]
///    computes it, so the ledger figure and the resident figure agree.
/// 2. **Give the arena its share, never the whole budget.** `push` receives the bytes
///    still available after every *other* live structure (`ceiling`), because all work
///    areas share one `max_work_bytes`. Handing it `max_work_bytes` would let its own
///    ceiling check pass while the process is already over budget elsewhere.
/// 3. **Refuse before allocating.** The growth is pre-flighted against `ceiling`, so a
///    starved arena reports a typed error instead of reserving and then unwinding.
///
/// The ledger is charged *before* the allocation, per the shared-accounting rule, and
/// the charge is exactly the growth the push will take.
fn push_charged(
    arena: &mut ProofArena,
    node: ProofNode,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<ProofRef, QueryError> {
    let slot = ProofNode::slot_bytes();
    let resident = arena.bytes();
    // Bytes the ledger already holds for *other* structures. `arena.bytes()` counts
    // only this arena, and every earlier push charged its growth here, so the
    // difference is the rest of the shared work area.
    let others = work.work_bytes().saturating_sub(resident);
    let ceiling = work.budget().max_work_bytes.saturating_sub(others);

    // Predict the push exactly: doubling from `capacity`, one slot when empty.
    let growth = if arena.capacity() == arena.len() {
        let wanted = if arena.capacity() == 0 {
            1
        } else {
            arena.capacity().saturating_mul(2)
        };
        (wanted as u64)
            .saturating_mul(slot)
            .saturating_sub(resident)
    } else {
        0
    };

    if resident.saturating_add(growth) > ceiling {
        return Err(QueryError::BudgetExceeded(
            sage_db_types::BudgetLimit::WorkBytes,
        ));
    }

    work.bytes(growth)?;
    work.proof_node()?;
    work.periodic_check(control)?;

    let reference = arena
        .push(node, work.budget().max_proof_nodes, ceiling)
        .map_err(QueryError::from)?;

    debug_assert_eq!(
        arena.bytes().saturating_sub(resident),
        growth,
        "the ledger must be charged the arena's actual resident growth"
    );
    Ok(reference)
}

fn missing_premise() -> QueryError {
    QueryError::InvalidRequest {
        reason: "derivation premise was not rendered",
    }
}

/// Why a proof could not be verified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplayError {
    /// A leaf names an assertion absent from this snapshot.
    MissingAssertion(AssertionId),
    /// A leaf names a revision that is not the active one.
    RevisionMismatch {
        /// Assertion with the stale revision.
        assertion: AssertionId,
        /// Revision the leaf claims.
        claimed: RecordRevision,
        /// Revision actually present.
        actual: RecordRevision,
    },
    /// A premise points at or beyond the node that cites it.
    ForwardReference,
    /// A node looks up a position the arena does not have.
    DanglingReference,
    /// A transitive composition does not join end to end.
    Disconnected,
    /// A rule node is applied under a relation or profile that does not allow it.
    RuleNotApplicable {
        /// Relation the node claimed.
        relation: RelationId,
    },
    /// A rule premise is itself a rule node, which is not a new source.
    CircularRuleApplication,
    /// The replayed endpoints differ from the expected endpoints.
    FactMismatch,
    /// The replayed annotation differs from the expected annotation.
    AnnotationMismatch,
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingAssertion(id) => write!(f, "assertion {} is not active", id.get()),
            Self::RevisionMismatch {
                assertion,
                claimed,
                actual,
            } => write!(
                f,
                "assertion {} claims revision {}, active is {}",
                assertion.get(),
                claimed.get(),
                actual.get()
            ),
            Self::ForwardReference => f.write_str("proof premise does not precede its use"),
            Self::DanglingReference => f.write_str("proof references a missing node"),
            Self::Disconnected => f.write_str("transitive premises do not join"),
            Self::RuleNotApplicable { relation } => {
                write!(f, "rule is not applicable to {relation} under this profile")
            }
            Self::CircularRuleApplication => {
                f.write_str("rule node rests on the same rule, which is not a new source")
            }
            Self::FactMismatch => f.write_str("replayed endpoints differ from the expected fact"),
            Self::AnnotationMismatch => {
                f.write_str("replayed annotation differs from the expected annotation")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

/// What one replayed node stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReplayedFact {
    /// Subject the node proves.
    pub from: TermId,
    /// Relation the node proves.
    pub relation: RelationId,
    /// Object the node proves.
    pub to: TermId,
    /// Recomposed annotation.
    pub annotation: Annotation,
}

/// Replay `root` and require it to describe the given fact.
///
/// `graph` supplies the authoritative assertions and their revisions; nothing else
/// about the producing evaluator is consulted.
///
/// # Errors
///
/// Returns the first structural or semantic mismatch found.
pub fn replay(
    graph: &BaseGraph,
    rules: RuleSet,
    arena: &ProofArena,
    root: ProofRef,
    expected_from: TermId,
    expected_relation: RelationId,
    expected_to: TermId,
) -> Result<ReplayedFact, ReplayError> {
    let count = arena.len();
    let mut facts: Vec<Option<ReplayedFact>> = vec![None; count];

    // Premises always precede their use, so one forward pass recomputes everything
    // and a forward reference necessarily shows up as a missing premise.
    for position in 0..count {
        let node = arena
            .get(ProofRef::new(position as u32))
            .ok_or(ReplayError::DanglingReference)?;
        let fact = match node {
            ProofNode::Asserted {
                assertion_id,
                revision,
            } => {
                let assertion = graph
                    .assertion(assertion_id)
                    .ok_or(ReplayError::MissingAssertion(assertion_id))?;
                if assertion.revision != revision {
                    return Err(ReplayError::RevisionMismatch {
                        assertion: assertion_id,
                        claimed: revision,
                        actual: assertion.revision,
                    });
                }
                ReplayedFact {
                    from: assertion.from,
                    relation: assertion.relation,
                    to: assertion.target,
                    annotation: Annotation::present(assertion.confidence),
                }
            }
            ProofNode::Symmetry { premise } => {
                let inner = lookup(&facts, premise)?;
                let relation = inner.relation;
                if !rules.spec(relation).symmetric {
                    return Err(ReplayError::RuleNotApplicable { relation });
                }
                if is_rule_node(arena, premise) {
                    return Err(ReplayError::CircularRuleApplication);
                }
                // The mirror's endpoints come from the premise, swapped: a forged
                // pair of endpoints cannot survive this.
                ReplayedFact {
                    from: inner.to,
                    relation,
                    to: inner.from,
                    annotation: inner.annotation,
                }
            }
            ProofNode::DefinitionImpliesDependency { premise } => {
                let inner = lookup(&facts, premise)?;
                let relation = inner.relation;
                if !rules.implies_dependency(relation) || !rules.spec(relation).is_definition {
                    return Err(ReplayError::RuleNotApplicable { relation });
                }
                if is_rule_node(arena, premise) {
                    return Err(ReplayError::CircularRuleApplication);
                }
                ReplayedFact {
                    from: inner.from,
                    relation: RelationId::DependsOn,
                    to: inner.to,
                    annotation: inner.annotation,
                }
            }
            ProofNode::Transitive { left, right } => {
                let left_fact = lookup(&facts, left)?;
                let right_fact = lookup(&facts, right)?;
                let relation = left_fact.relation;
                if !rules.spec(relation).transitive {
                    return Err(ReplayError::RuleNotApplicable { relation });
                }
                if right_fact.relation != relation {
                    return Err(ReplayError::Disconnected);
                }
                // The semantic join, which is why swapping the operands is caught.
                if left_fact.to != right_fact.from {
                    return Err(ReplayError::Disconnected);
                }
                ReplayedFact {
                    from: left_fact.from,
                    relation,
                    to: right_fact.to,
                    annotation: left_fact.annotation.meet(right_fact.annotation),
                }
            }
        };
        facts[position] = Some(fact);
    }

    let root_fact = lookup(&facts, root)?;
    if root_fact.from != expected_from
        || root_fact.relation != expected_relation
        || root_fact.to != expected_to
    {
        return Err(ReplayError::FactMismatch);
    }
    Ok(root_fact)
}

/// The fact and annotation a proof is required to prove.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Expected {
    /// Required subject.
    pub from: TermId,
    /// Required relation.
    pub relation: RelationId,
    /// Required object.
    pub to: TermId,
    /// Required annotation.
    pub annotation: Annotation,
}

/// Replay and require the fact *and* annotation to match `expected`.
///
/// # Errors
///
/// Propagates [`replay`] failures and reports an annotation disagreement.
pub fn replay_expecting(
    graph: &BaseGraph,
    rules: RuleSet,
    arena: &ProofArena,
    root: ProofRef,
    expected: Expected,
) -> Result<(), ReplayError> {
    let replayed = replay(
        graph,
        rules,
        arena,
        root,
        expected.from,
        expected.relation,
        expected.to,
    )?;
    if replayed.annotation == expected.annotation {
        Ok(())
    } else {
        Err(ReplayError::AnnotationMismatch)
    }
}

fn lookup(
    facts: &[Option<ReplayedFact>],
    reference: ProofRef,
) -> Result<ReplayedFact, ReplayError> {
    let position = reference.get() as usize;
    if position >= facts.len() {
        return Err(ReplayError::DanglingReference);
    }
    facts[position].ok_or(ReplayError::ForwardReference)
}

fn is_rule_node(arena: &ProofArena, reference: ProofRef) -> bool {
    match arena.get(reference) {
        Some(ProofNode::Asserted { .. }) | None => false,
        Some(_) => true,
    }
}

#[cfg(test)]
mod arena_charging {
    //! Byte-level properties of [`push_charged`].
    //!
    //! These are unit tests because they assert *exact* figures for one mutation, which
    //! a whole-evaluation test cannot isolate from the other work areas. The two
    //! properties are deliberately separate: refusal-before-mutation holds even when
    //! the per-mutation charge is wrong, so passing one says nothing about the other.

    use super::{FactKey, Key, PlanTable, Renderer, push_charged};
    use crate::work::Work;
    use sage_db_types::{
        AssertionId, Budget, ProofArena, ProofNode, QueryError, RecordRevision, RunToCompletion,
        TermId,
    };

    const SLOT: u64 = 32;

    fn node(id: u128) -> ProofNode {
        ProofNode::Asserted {
            assertion_id: AssertionId::new(id),
            revision: RecordRevision::FIRST,
        }
    }

    /// A budget that admits exactly `slots` proof slots and nothing else.
    fn budget_for(slots: u64) -> Budget {
        Budget {
            max_work_bytes: slots * SLOT,
            ..Budget::DEFAULT
        }
    }

    #[test]
    fn ledger_delta_must_equal_arena_growth_on_doubling() {
        assert_eq!(
            ProofNode::slot_bytes(),
            SLOT,
            "fixture arithmetic assumes a 32-byte slot"
        );

        let mut arena = ProofArena::new();
        let mut work = Work::new(budget_for(8));
        let control = RunToCompletion;

        // Fill to capacity so the next push doubles: 2 slots -> 4 slots (+64 B).
        push_charged(&mut arena, node(1), &mut work, &control).expect("first");
        push_charged(&mut arena, node(2), &mut work, &control).expect("second");
        assert_eq!(arena.len(), 2);
        assert_eq!(arena.capacity(), 2, "next push must double");

        let ledger_before = work.work_bytes();
        let arena_before = arena.bytes();
        push_charged(&mut arena, node(3), &mut work, &control).expect("doubling");

        let ledger_delta = work.work_bytes() - ledger_before;
        let arena_delta = arena.bytes() - arena_before;

        assert_eq!(
            arena_delta, 64,
            "doubling from 2 to 4 slots must add 64 resident bytes"
        );
        assert_eq!(
            ledger_delta, arena_delta,
            "ledger charged {ledger_delta} B but the arena grew {arena_delta} B"
        );
    }

    #[test]
    fn a_push_inside_existing_capacity_is_charged_nothing() {
        let mut arena = ProofArena::new();
        let mut work = Work::new(budget_for(8));
        let control = RunToCompletion;
        push_charged(&mut arena, node(1), &mut work, &control).expect("first");
        push_charged(&mut arena, node(2), &mut work, &control).expect("second");
        // Now at len == capacity == 2; grow to 4 with one push, then one more fits.
        push_charged(&mut arena, node(3), &mut work, &control).expect("doubling");

        let ledger_before = work.work_bytes();
        let arena_before = arena.bytes();
        push_charged(&mut arena, node(4), &mut work, &control).expect("fits spare capacity");

        assert_eq!(
            arena.bytes(),
            arena_before,
            "no growth, so no resident change"
        );
        assert_eq!(
            work.work_bytes(),
            ledger_before,
            "a push that fits existing capacity must charge no bytes"
        );
    }

    #[test]
    fn third_leaf_is_refused_before_the_arena_grows() {
        // Exactly enough for two slots: the third push needs a doubling to 4.
        let mut arena = ProofArena::new();
        let mut work = Work::new(budget_for(2));
        let control = RunToCompletion;
        push_charged(&mut arena, node(1), &mut work, &control).expect("first");
        push_charged(&mut arena, node(2), &mut work, &control).expect("second");
        assert_eq!(arena.capacity(), 2);
        assert_eq!(arena.bytes(), 2 * SLOT);

        let before = (arena.len(), arena.capacity(), arena.bytes());
        let ledger_before = work.work_bytes();
        let outcome = push_charged(&mut arena, node(3), &mut work, &control);

        assert!(
            matches!(outcome, Err(QueryError::BudgetExceeded(_))),
            "a starved ledger must refuse, got {outcome:?}"
        );
        assert_eq!(
            (arena.len(), arena.capacity(), arena.bytes()),
            before,
            "refusal must not mutate the arena"
        );
        assert_eq!(
            work.work_bytes(),
            ledger_before,
            "a refused push must not charge bytes"
        );
    }

    /// The discriminating pair for the ceiling: at `len == capacity == 2` the next push
    /// needs a 64 B doubling. With 32 B left it must refuse and leave the arena alone;
    /// with 64 B left it must succeed and charge exactly 64.
    ///
    /// This is the case an implementation that passes `max_work_bytes` as the arena's
    /// ceiling gets wrong: the arena would happily double because its own allowance is
    /// the whole budget, while the shared ledger has only one slot left.
    fn primed(remaining_after_spend: u64) -> (ProofArena, Work, RunToCompletion) {
        // 8 slots total; two pre-warm pushes charge the 2-slot arena (64 B).
        let mut arena = ProofArena::new();
        let mut work = Work::new(budget_for(8));
        let control = RunToCompletion;
        push_charged(&mut arena, node(1), &mut work, &control).expect("first");
        push_charged(&mut arena, node(2), &mut work, &control).expect("second");
        assert_eq!((arena.len(), arena.capacity()), (2, 2));

        // Spend the area down so exactly `remaining_after_spend` bytes are left.
        let spare = work.remaining_bytes();
        assert!(spare >= remaining_after_spend);
        work.bytes(spare - remaining_after_spend)
            .expect("spend down to the requested remainder");
        assert_eq!(work.remaining_bytes(), remaining_after_spend);
        (arena, work, control)
    }

    #[test]
    fn one_slot_left_refuses_the_doubling_without_touching_the_arena() {
        let (mut arena, mut work, control) = primed(SLOT);
        let before = (arena.len(), arena.capacity(), arena.bytes());
        let ledger_before = work.work_bytes();

        let outcome = push_charged(&mut arena, node(3), &mut work, &control);

        assert!(
            matches!(outcome, Err(QueryError::BudgetExceeded(_))),
            "32 B left cannot cover a 64 B doubling, got {outcome:?}"
        );
        assert_eq!(
            (arena.len(), arena.capacity(), arena.bytes()),
            before,
            "refusal must not mutate the arena"
        );
        assert_eq!(
            work.work_bytes(),
            ledger_before,
            "a refused push must not charge bytes"
        );
    }

    #[test]
    fn exactly_enough_for_the_doubling_succeeds_and_charges_the_real_growth() {
        let (mut arena, mut work, control) = primed(2 * SLOT);
        let arena_before = arena.bytes();
        let ledger_before = work.work_bytes();

        push_charged(&mut arena, node(3), &mut work, &control).expect("64 B covers the doubling");

        assert_eq!(arena.capacity(), 4, "the push must have doubled");
        assert_eq!(
            arena.bytes() - arena_before,
            2 * SLOT,
            "doubling 2 -> 4 adds 64 resident bytes"
        );
        assert_eq!(
            work.work_bytes() - ledger_before,
            2 * SLOT,
            "the ledger must be charged exactly the real growth"
        );
    }

    // -----------------------------------------------------------------------
    // The same properties, driven through the real `Renderer` instead of
    // `push_charged` directly: memo and stack pre-warm included, because that is
    // where the shipped defect actually lived.
    // -----------------------------------------------------------------------

    /// Three keys, each a single authored leaf, so each `render` pushes one node.
    fn three_leaves() -> (PlanTable, [FactKey; 3]) {
        let keys = [
            FactKey::new(
                TermId::new(0),
                sage_db_types::RelationId::DependsOn,
                TermId::new(1),
            ),
            FactKey::new(
                TermId::new(0),
                sage_db_types::RelationId::DependsOn,
                TermId::new(2),
            ),
            FactKey::new(
                TermId::new(0),
                sage_db_types::RelationId::DependsOn,
                TermId::new(3),
            ),
        ];
        let mut plans = PlanTable::new();
        let mut work = Work::new(Budget::DEFAULT);
        for (index, key) in keys.iter().enumerate() {
            let id = AssertionId::new(index as u128 + 1);
            plans
                .insert(
                    *key,
                    super::PlanNode::Asserted {
                        id,
                        revision: RecordRevision::FIRST,
                    },
                    Key::Asserted(id),
                    sage_db_types::Annotation::present(sage_db_types::Confidence::Confirmed),
                    1,
                    &mut work,
                )
                .expect("plan insert");
        }
        (plans, keys)
    }

    /// A `'static` plan table plus three keys, so a returned [`Renderer`] can borrow it.
    ///
    /// Leaked on purpose: a unit test needs one fixture for the whole process, and the
    /// alternative is threading a lifetime through every assertion.
    fn static_three_leaves() -> &'static (PlanTable, [FactKey; 3]) {
        Box::leak(Box::new(three_leaves()))
    }

    /// Render the first two keys with a generous budget, then rebuild with a budget of
    /// exactly `measured_warmup + headroom`.
    ///
    /// Warm-up cost is **measured**, never hardcoded: the memo and stack reservations
    /// change with the plan table, so a constant would silently stop describing the
    /// fixture. Post-condition: `len == capacity == 2`, so the next push doubles, and
    /// exactly `headroom` bytes remain.
    fn primed_renderer(
        fixture: &'static (PlanTable, [FactKey; 3]),
        headroom: u64,
    ) -> (ProofArena, Work, Renderer<'static>) {
        let (plans, keys) = fixture;
        let control = RunToCompletion;

        // Pass 1: measure the two warm-up renders under a generous budget.
        let mut measure_arena = ProofArena::new();
        let mut measure_work = Work::new(Budget::DEFAULT);
        {
            let mut renderer = Renderer::new(plans);
            renderer
                .render(keys[0], &mut measure_arena, &mut measure_work, &control)
                .expect("first");
            renderer
                .render(keys[1], &mut measure_arena, &mut measure_work, &control)
                .expect("second");
        }
        let spent = measure_work.work_bytes();
        assert_eq!(
            (measure_arena.len(), measure_arena.capacity()),
            (2, 2),
            "warm-up must leave the arena one push short of doubling"
        );
        assert_eq!(measure_arena.bytes(), 2 * SLOT);

        // Pass 2: the arena the test acts on, with exactly `headroom` to spare.
        let mut arena = ProofArena::new();
        let mut work = Work::new(Budget {
            max_work_bytes: spent + headroom,
            ..Budget::DEFAULT
        });
        let mut renderer = Renderer::new(plans);
        renderer
            .render(keys[0], &mut arena, &mut work, &control)
            .expect("first");
        renderer
            .render(keys[1], &mut arena, &mut work, &control)
            .expect("second");
        assert_eq!(
            (arena.len(), arena.capacity(), arena.bytes()),
            (2, 2, 2 * SLOT)
        );
        assert_eq!(
            work.remaining_bytes(),
            headroom,
            "the fixture must sit exactly `headroom` bytes from the ceiling"
        );
        (arena, work, renderer)
    }

    #[test]
    fn one_slot_of_headroom_refuses_the_renderer_doubling_without_mutating() {
        let fixture = static_three_leaves();
        // One spare slot: enough for a single node, not for the pending 64 B doubling.
        let (mut arena, mut work, mut renderer) = primed_renderer(fixture, SLOT);
        let control = RunToCompletion;

        let before = (arena.len(), arena.capacity(), arena.bytes());
        let ledger_before = work.work_bytes();
        let outcome = renderer.render(fixture.1[2], &mut arena, &mut work, &control);

        assert!(
            matches!(outcome, Err(QueryError::BudgetExceeded(_))),
            "one spare slot cannot cover the doubling, got {outcome:?}"
        );
        assert_eq!(
            (arena.len(), arena.capacity(), arena.bytes()),
            before,
            "the refused render must leave the arena byte-identical"
        );
        assert_eq!(
            work.work_bytes(),
            ledger_before,
            "a refused render must not charge bytes"
        );
    }

    #[test]
    fn exactly_enough_headroom_succeeds_and_charges_the_real_delta() {
        let fixture = static_three_leaves();
        // Headroom equal to the real doubling delta, whatever the warm-up cost.
        let (mut arena, mut work, mut renderer) = primed_renderer(fixture, 2 * SLOT);
        let control = RunToCompletion;

        let arena_before = arena.bytes();
        let ledger_before = work.work_bytes();
        renderer
            .render(fixture.1[2], &mut arena, &mut work, &control)
            .expect("the real growth must be affordable");

        assert_eq!(arena.capacity(), 4, "the render must have doubled");
        assert_eq!(
            arena.bytes() - arena_before,
            2 * SLOT,
            "doubling 2 -> 4 adds 64 resident bytes"
        );
        assert_eq!(
            work.work_bytes() - ledger_before,
            arena.bytes() - arena_before,
            "the renderer must charge the arena's real growth, and no extra slot"
        );
    }

    #[test]
    fn a_push_into_spare_capacity_does_not_charge_a_slot_again() {
        let (mut arena, mut work, control) = primed(2 * SLOT);
        push_charged(&mut arena, node(3), &mut work, &control).expect("doubling");
        // Now len == 3, capacity == 4: the next push fits without growing.
        let arena_before = arena.bytes();
        let ledger_before = work.work_bytes();

        push_charged(&mut arena, node(4), &mut work, &control).expect("fits");

        assert_eq!(arena.capacity(), 4);
        assert_eq!(arena.bytes(), arena_before, "no resident growth");
        assert_eq!(
            work.work_bytes(),
            ledger_before,
            "spare capacity must not be charged a second time"
        );
    }
}

#[cfg(test)]
mod witness_scan_control {
    //! The duplicate-support scan in selection must be charged and interruptible.
    //!
    //! `single_hop` charges one outer step per fact direction, so a triple carrying
    //! thousands of supports could otherwise be read for free. These tests call
    //! [`select_all`] directly, after `RawIndex` and saturation have already been built
    //! with a generous budget, so the index and table costs cannot mask the defect: the
    //! only budget left for selection is the one each test supplies.

    use super::{FactKey, select_all};
    use crate::closure;
    use crate::index::RawIndex;
    use crate::work::Work;
    use sage_db_types::{
        Annotation, AssertionId, AssertionRecord, BaseGraph, Budget, BudgetLimit, Confidence,
        ControlStop, Digest, GraphDigest, KbUuid, KbVersion, QueryControl, QueryError,
        RecordRevision, RelationId, RuleProfile, RuleSet, RulesDigest, RunToCompletion,
        SemanticLayer, SnapshotId, SourceExecutor, SourceQuery, TermId, TermMeta,
    };

    fn rules() -> RuleSet {
        RuleSet::from_trusted_parts(
            RuleProfile::LegacyV1,
            RulesDigest::new(Digest::from_bytes([0x31; 32])),
        )
    }

    /// One `from -> to` triple carrying `supports` independent authored assertions.
    fn duplicated_supports(supports: u32) -> BaseGraph {
        let terms = vec![
            TermMeta {
                id: TermId::new(0),
                layer: SemanticLayer::L1,
            },
            TermMeta {
                id: TermId::new(1),
                layer: SemanticLayer::L1,
            },
        ];
        let assertions: Vec<AssertionRecord> = (0..supports)
            .map(|index| AssertionRecord {
                id: AssertionId::new(index as u128 + 1),
                revision: RecordRevision::FIRST,
                from: TermId::new(0),
                relation: RelationId::DependsOn,
                target: TermId::new(1),
                confidence: Confidence::Confirmed,
            })
            .collect();
        let snapshot = SnapshotId {
            kb_uuid: KbUuid::from_bytes([0x21; 16]),
            version: KbVersion::new(1),
            rules: rules(),
            graph_digest: GraphDigest::new(Digest::from_bytes([0x41; 32])),
        };
        BaseGraph::new(snapshot, terms, assertions, Budget::DEFAULT).expect("fixture graph")
    }

    /// Build index and table under a generous budget, returning the selection input.
    fn prepared(graph: &BaseGraph) -> (RawIndex, crate::facts::FactTable) {
        let control = RunToCompletion;
        let mut work = Work::new(Budget::DEFAULT);
        let index = RawIndex::build(graph, &mut work, &control).expect("index");
        let table = closure::saturate(
            &index,
            graph.rules(),
            RelationId::DependsOn,
            &mut work,
            &control,
        )
        .expect("saturation");
        (index, table)
    }

    #[test]
    fn a_duplicate_support_scan_is_charged_against_the_selection_budget() {
        // 5000 supports on one triple. Saturation folds them into one fact, so selection
        // has one fact direction and would charge a single outer step if the scan were
        // free.
        let graph = duplicated_supports(5_000);
        let (index, table) = prepared(&graph);

        // Enough bytes/rows for the plan structures, but only a few steps.
        let mut work = Work::new(Budget {
            max_steps: 3,
            ..Budget::DEFAULT
        });
        let outcome = select_all(
            &index,
            &table,
            graph.rules(),
            RelationId::DependsOn,
            &mut work,
            &RunToCompletion,
        );

        match outcome {
            Err(error) => assert_eq!(
                error,
                QueryError::BudgetExceeded(BudgetLimit::Steps),
                "a 5000-support scan must not fit a 3-step selection budget"
            ),
            Ok(_) => panic!("a 3-step budget must not admit a 5000-support scan"),
        }
    }

    #[test]
    fn a_duplicate_support_scan_observes_cancellation() {
        struct StopAfter(std::cell::Cell<u32>);

        impl QueryControl for StopAfter {
            fn checkpoint(&self) -> Result<(), ControlStop> {
                let remaining = self.0.get();
                if remaining == 0 {
                    Err(ControlStop::Cancelled)
                } else {
                    self.0.set(remaining - 1);
                    Ok(())
                }
            }
        }

        let graph = duplicated_supports(5_000);
        let (index, table) = prepared(&graph);

        // Allow one consultation (the loop entry), then stop.
        let control = StopAfter(std::cell::Cell::new(1));
        let mut work = Work::new(Budget::DEFAULT);
        let outcome = select_all(
            &index,
            &table,
            graph.rules(),
            RelationId::DependsOn,
            &mut work,
            &control,
        );

        match outcome {
            Err(error) => assert_eq!(
                error,
                QueryError::Stopped(ControlStop::Cancelled),
                "a 5000-support scan must not run past a cancellation"
            ),
            Ok(_) => panic!("a 5000-support scan must not run past a cancellation"),
        }
        assert!(
            control.0.get() <= 1,
            "it must stop at the first consultation, not after the scan"
        );
    }

    #[test]
    fn the_strongest_of_many_duplicate_supports_is_still_selected() {
        // Behaviour guard: charging must not change which support wins.
        let graph = duplicated_supports(200);
        let (index, table) = prepared(&graph);
        let mut work = Work::new(Budget::DEFAULT);
        let plans = select_all(
            &index,
            &table,
            graph.rules(),
            RelationId::DependsOn,
            &mut work,
            &RunToCompletion,
        )
        .expect("selection");

        let key = FactKey::new(TermId::new(0), RelationId::DependsOn, TermId::new(1));
        let root = plans.root(&key).expect("selected");
        match plans.node(root) {
            Some(super::PlanNode::Asserted { id, .. }) => assert_eq!(
                id,
                AssertionId::new(1),
                "equal confidence is broken by the lowest assertion id"
            ),
            other => panic!("expected an authored leaf, got {other:?}"),
        }
        assert_eq!(plans.hops(&key), Some(1));

        // And the end-to-end answer is unchanged.
        let query = SourceQuery {
            snapshot: graph.snapshot(),
            source: TermId::new(0),
            relation: RelationId::DependsOn,
            with_proof: true,
            budget: Budget::DEFAULT,
        };
        let result = crate::ReferenceExecutor::new()
            .solve(&graph, &query, &RunToCompletion)
            .expect("solve");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].annotation,
            Annotation::present(Confidence::Confirmed)
        );
    }
}
