//! Bounded proof construction and independent replay.
//!
//! A proof names the authored assertion revisions it rests on, so an independent
//! replay can re-derive the fact from the base graph without trusting this producer.
//!
//! # Direction
//!
//! `Transitive { left, right }` is `from → mid` then `mid → to`. The chain is walked
//! backwards from the target and then composed forwards, so `left` is always the
//! prefix nearer the source.
//!
//! # Symmetry
//!
//! A backwards read of an authored assertion is not a bare `Asserted` leaf: the
//! authored triple is the wrong direction. It becomes `Symmetry { premise }`, where
//! the premise is the authored leaf. Replay therefore sees the reversal explicitly and
//! can check that the relation really is symmetric under the active profile.
//!
//! # In-query reuse
//!
//! Rows of one query share a prefix. Building each row's chain from scratch costs
//! `O(rows × depth)`; on the 1 000-node chain that is ~500 k arena nodes, far past any
//! sane budget. [`Prover`] memoises the composed prefix per `(level, term)`, so a
//! chain query costs `O(nodes)` nodes. The key includes the level because a prefix
//! valid at `confirmed` may not exist at `hypothetical` — that distinction is what
//! prevents the splicing bug in `path.rs`.
//!
//! This is not a cross-query cache: the memo lives and dies with one `solve`.

use sage_db_types::{
    Annotation, AssertionId, BaseGraph, Confidence, ProofArena, ProofNode, ProofRef, QueryControl,
    QueryError, RecordRevision, RelationId, TermId,
};

use crate::meter::Work;
use crate::path::{Reach, Witness};

/// A fully replayed fact.
///
/// Replay yields the whole triple, not just a confidence: a proof that recovers the
/// right annotation for the wrong endpoints or relation is still wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReplayedFact {
    /// Subject in the proof's orientation.
    pub from: TermId,
    /// Relation the proof composes.
    pub relation: RelationId,
    /// Object in the proof's orientation.
    pub to: TermId,
    /// Annotation recovered by replaying the rules.
    pub annotation: Annotation,
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
    /// A node references a position outside the arena.
    DanglingReference,
    /// A `Transitive` composition does not join end to end.
    Disconnected,
    /// A `Transitive` node composes a relation that is not transitive under the
    /// active profile.
    RelationsNotTransitive(RelationId),
    /// A `Transitive` node joins two different relations.
    RelationMismatch {
        /// Relation of the left premise.
        left: RelationId,
        /// Relation of the right premise.
        right: RelationId,
    },
    /// A `Symmetry` node's relation is not symmetric under the active profile.
    RelationsNotSymmetric(RelationId),
    /// A `DefinitionImpliesDependency` node appeared under a profile without R3.
    RuleNotActive,
    /// A `DefinitionImpliesDependency` premise is not a definition assertion.
    PremiseNotDefinition(RelationId),
    /// The proof's recovered triple differs from the claimed one.
    FactMismatch {
        /// Triple the proof recovers.
        recovered: ReplayedFact,
        /// Triple that was claimed.
        claimed: ReplayedFact,
    },
    /// The proof was built for a different snapshot than the graph supplied.
    SnapshotMismatch,
    /// The arena graph is cyclic, so no finite derivation exists.
    CyclicProof,
    /// Replay exceeded its work budget.
    BudgetExhausted,
    /// The caller cancelled, or the deadline expired during replay.
    Stopped,
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
            Self::DanglingReference => f.write_str("proof references a missing node"),
            Self::Disconnected => f.write_str("transitive premises do not join"),
            Self::RelationsNotTransitive(relation) => {
                write!(
                    f,
                    "relation {relation} is not transitive under this profile"
                )
            }
            Self::RelationMismatch { left, right } => {
                write!(f, "composition joins {left} to {right}")
            }
            Self::RelationsNotSymmetric(relation) => {
                write!(f, "relation {relation} is not symmetric under this profile")
            }
            Self::RuleNotActive => f.write_str("definition implication is not active here"),
            Self::PremiseNotDefinition(relation) => {
                write!(f, "{relation} is not a definition relation")
            }
            Self::FactMismatch { recovered, claimed } => write!(
                f,
                "proof recovers {:?} but {:?} was claimed",
                recovered, claimed
            ),
            Self::SnapshotMismatch => f.write_str("proof belongs to another snapshot"),
            Self::CyclicProof => f.write_str("proof contains a cycle"),
            Self::BudgetExhausted => f.write_str("replay exceeded its budget"),
            Self::Stopped => f.write_str("replay was stopped by the caller"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// Proof producer for one query, memoising composed prefixes.
///
/// The memo is keyed by `(level, term)`: the term whose prefix ends there, and the
/// threshold pass that produced the parent link. Sharing a prefix across levels would
/// resurrect the splicing bug.
pub struct Prover {
    memo: Vec<Option<[Option<ProofRef>; 3]>>,
    table: Vec<TermId>,
    /// Scratch chain, reused across rows.
    ///
    /// A fresh `Vec` per row would charge `rows x depth` bytes for a buffer that is
    /// only ever `depth` long; on a 1 000-node chain that is ~48 MB of pure waste.
    chain: Vec<Witness>,
}

impl Prover {
    /// Create a prover over `graph`, reserved once against `work`.
    ///
    /// # Errors
    ///
    /// Propagates budget failures.
    pub fn new(graph: &BaseGraph, work: &mut Work) -> Result<Self, QueryError> {
        let bound = graph.term_count();
        let mut memo: Vec<Option<[Option<ProofRef>; 3]>> = Vec::new();
        let mut table: Vec<TermId> = Vec::new();
        // The chain only ever holds one route, so it starts empty and grows on demand
        // rather than to the whole term count.
        let chain: Vec<Witness> = Vec::new();
        crate::meter::grow(&mut memo, bound, work)?;
        crate::meter::grow(&mut table, bound, work)?;
        memo.resize(bound, None);
        table.extend(graph.terms().iter().map(|meta| meta.id));
        Ok(Self { memo, table, chain })
    }

    fn index_of(&self, term: TermId) -> Option<usize> {
        self.table.binary_search(&term).ok()
    }

    fn cached(&self, term: TermId, level_position: usize) -> Option<ProofRef> {
        let index = self.index_of(term)?;
        self.memo[index]
            .as_ref()
            .and_then(|slots| slots[level_position])
    }

    fn store(&mut self, term: TermId, level_position: usize, reference: ProofRef) -> Option<()> {
        let index = self.index_of(term)?;
        self.memo[index].get_or_insert([None; 3])[level_position] = Some(reference);
        Some(())
    }

    /// Build (or reuse) the proof of `target` at `level`.
    ///
    /// The route is composed iteratively from the source outward, so depth cannot
    /// overflow the stack, and each `(level, node)` prefix is built once.
    ///
    /// # Errors
    ///
    /// Propagates arena, budget and control failures.
    #[allow(clippy::too_many_arguments)]
    pub fn prove(
        &mut self,
        graph: &BaseGraph,
        source: TermId,
        target: TermId,
        level: Confidence,
        reach: &Reach,
        arena: &mut ProofArena,
        control: &dyn QueryControl,
        work: &mut Work,
    ) -> Result<ProofRef, QueryError> {
        let level_position = crate::path::level_index(level).ok_or(QueryError::InvalidRequest {
            reason: "unknown confidence level",
        })?;

        // 1. Walk back to the source, collecting links into the shared scratch.
        self.chain.clear();
        let mut current = target;
        let mut guard = 0usize;
        while current != source {
            work.step(1)?;
            work.periodic_check(control)?;
            if let Some(cached) = self.cached(current, level_position) {
                // The prefix up to `current` already exists; compose from there.
                let prefix = cached;
                return self.finish(
                    graph,
                    current,
                    Some(prefix),
                    level_position,
                    arena,
                    control,
                    work,
                );
            }
            let Some(witness) = reach.parent_at(current, level) else {
                return Err(QueryError::InvalidRequest {
                    reason: "no proof route for a returned fact at its reported level",
                });
            };
            // Charge the scratch's capacity delta; a bare push could reallocate
            // outside the ledger.
            let needed = self.chain.len() + 1;
            if needed > self.chain.capacity() {
                crate::meter::grow(&mut self.chain, needed, work)?;
            }
            self.chain.push(witness);
            current = witness.key.from;
            guard += 1;
            if guard > graph.term_count() {
                return Err(QueryError::InvalidRequest {
                    reason: "proof chain does not terminate",
                });
            }
        }

        self.finish(graph, source, None, level_position, arena, control, work)
    }

    /// Compose the remembered chain on top of an optional cached `prefix`.
    ///
    /// `level_position` is the **request's** threshold, not the last edge's
    /// confidence. Keying the memo on an edge's strength would file a weak prefix
    /// under a stronger level: on `A-S(contested)-B-T` the contested `A->B` prefix
    /// would be stored under `confirmed` and later reused for a `confirmed` row whose
    /// annotation the proof cannot actually justify.
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        graph: &BaseGraph,
        prefix_end: TermId,
        prefix: Option<ProofRef>,
        level_position: usize,
        arena: &mut ProofArena,
        control: &dyn QueryControl,
        work: &mut Work,
    ) -> Result<ProofRef, QueryError> {
        // `composed` covers `start -> prefix_end`; it grows toward the target, so each
        // new edge is appended on the right, never prepended.
        let mut composed = prefix;
        let mut cursor = prefix_end;
        // Walk the shared scratch in place: index instead of draining it into a fresh
        // `Vec`, so no per-row allocation can hide here.
        for index in (0..self.chain.len()).rev() {
            work.step(1)?;
            work.periodic_check(control)?;
            let witness = self.chain[index];
            debug_assert_eq!(witness.key.from, cursor);
            let edge = self.edge(arena, &witness, control, work)?;
            composed = Some(match composed {
                None => edge,
                Some(left) => push(
                    arena,
                    ProofNode::Transitive { left, right: edge },
                    control,
                    work,
                )?,
            });
            cursor = witness.key.to;
            // Memoise every prefix under the request's level, so a sibling row at the
            // same level reuses it and a row at another level cannot.
            self.store(cursor, level_position, composed.expect("just set"));
        }
        composed.ok_or_else(|| {
            let _ = graph;
            QueryError::InvalidRequest {
                reason: "empty proof for a returned fact",
            }
        })
    }

    /// One link: a leaf, optionally wrapped by the rule that licensed it.
    fn edge(
        &self,
        arena: &mut ProofArena,
        witness: &Witness,
        control: &dyn QueryControl,
        work: &mut Work,
    ) -> Result<ProofRef, QueryError> {
        let leaf = push(
            arena,
            ProofNode::Asserted {
                assertion_id: witness.assertion,
                revision: witness.revision,
            },
            control,
            work,
        )?;
        if witness.reversed {
            // A backwards read is a reversal, not a leaf in the wrong direction.
            return push(arena, ProofNode::Symmetry { premise: leaf }, control, work);
        }
        if witness.via_definition {
            return push(
                arena,
                ProofNode::DefinitionImpliesDependency { premise: leaf },
                control,
                work,
            );
        }
        Ok(leaf)
    }
}

/// Push one node, charging both the node budget and the arena's capacity growth.
///
/// The arena's allowance is `already-charged arena capacity + remaining total budget`,
/// computed from the shared [`Work`]: passing only the remaining figure would re-charge
/// memory the arena already occupies. `max_work_bytes` on the arena is therefore
/// "total working memory this query may hold", which is exactly the shared ledger.
fn push(
    arena: &mut ProofArena,
    node: ProofNode,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<ProofRef, QueryError> {
    // Node count first: it is a pure arithmetic limit, so a refusal here costs nothing.
    work.proof_node()?;
    work.periodic_check(control)?;
    if arena.len() >= work.budget().max_proof_nodes as usize {
        return Err(QueryError::Proof(
            sage_db_types::ProofArenaError::NodeBudgetExceeded {
                attempted: crate::meter::narrow(arena.len() + 1)?,
            },
        ));
    }

    let capacity_before = arena.capacity();
    // Predict exactly what `ProofArena::push` will reserve: it doubles on overflow.
    // Mirrors `ProofArena::push`: 0 -> 1 slot, otherwise double. Predicting a
    // different number would charge the wrong delta and break the exact-remainder case.
    let wanted = if arena.len() == capacity_before {
        if capacity_before == 0 {
            1
        } else {
            capacity_before.saturating_mul(2)
        }
    } else {
        capacity_before
    };
    let growth_slots = wanted.saturating_sub(capacity_before);
    let growth_bytes = (growth_slots as u64).saturating_mul(ProofNode::slot_bytes());

    // Everything the *other* containers have taken. Computed before this push so the
    // growth charge below is not counted against the arena's own ceiling.
    let other_spent = work.work_bytes().saturating_sub(arena.bytes());
    let ceiling = work.budget().max_work_bytes.saturating_sub(other_spent);

    // Refuse *before* the arena touches the allocator: charging after a successful
    // `push` would mean the memory was already taken when the budget said no.
    if growth_bytes > 0 {
        work.bytes(growth_bytes)?;
    }

    let reference = arena
        .push(node, work.budget().max_proof_nodes, ceiling)
        .map_err(QueryError::from)?;

    debug_assert_eq!(
        arena.capacity(),
        wanted,
        "arena capacity growth must match the figure charged before the push"
    );
    Ok(reference)
}

/// Replay every row's proof in one arena pass.
///
/// One table indexed by arena node is filled once, in topological order, and each row
/// is then a lookup. Replaying per row would be `rows x depth` work; this is
/// `O(arena_nodes + rows)`.
///
/// # Errors
///
/// Returns a typed [`ReplayError`] for the first structural or semantic failure, the
/// caller's stop, or an exhausted budget.
pub fn replay_rows(
    graph: &BaseGraph,
    arena: &ProofArena,
    rows: &[sage_db_types::FactRow],
    control: &dyn QueryControl,
    budget: sage_db_types::Budget,
) -> Result<Vec<ReplayedFact>, ReplayError> {
    let mut work = crate::meter::Work::new(budget);
    work.check_entry(control)
        .map_err(|_| ReplayError::Stopped)?;
    let facts = replay_rows_with(graph, arena, rows, control, &mut work)?;
    work.check_exit(control).map_err(|_| ReplayError::Stopped)?;
    Ok(facts)
}

/// Replay rows against a caller-owned ledger.
///
/// Verification is allowed its own budget, but only **one** per verification: the
/// arena sweep, the value table, the output vector and the row comparison all draw
/// from this single [`Work`], whose reservations accumulate monotonically. Creating a
/// second `Work` mid-verification would restart the accounting from zero and hand the
/// same allowance out again.
///
/// # Errors
///
/// Returns a typed [`ReplayError`] for structural failures, stops and budget refusals.
pub fn replay_rows_with(
    graph: &BaseGraph,
    arena: &ProofArena,
    rows: &[sage_db_types::FactRow],
    control: &dyn QueryControl,
    work: &mut crate::meter::Work,
) -> Result<Vec<ReplayedFact>, ReplayError> {
    // One slot per arena node, reserved fallibly and charged to the shared ledger.
    let mut values: Vec<Option<ReplayedFact>> = Vec::new();
    crate::meter::grow(&mut values, arena.len(), work).map_err(record_budget)?;
    values.resize(arena.len(), None);
    // A held value is a live work row: `max_work_rows = 0` must refuse here rather
    // than silently verifying an unbounded arena.
    crate::meter::charge_rows(arena.len(), work).map_err(record_budget)?;

    // Nodes are appended in topological order, so a single forward sweep resolves
    // every premise before its use. No stack, no recursion, no per-row walk.
    for position in 0..arena.len() {
        work.step(1).map_err(record_budget)?;
        work.periodic_check(control)
            .map_err(|_| ReplayError::Stopped)?;
        let value = resolve(graph, arena, position, &values)?;
        values[position] = Some(value);
    }

    let mut out = Vec::new();
    crate::meter::grow(&mut out, rows.len(), work).map_err(record_budget)?;
    crate::meter::charge_rows(rows.len(), work).map_err(record_budget)?;
    for row in rows {
        work.step(1).map_err(record_budget)?;
        work.periodic_check(control)
            .map_err(|_| ReplayError::Stopped)?;
        let root = row.proof.ok_or(ReplayError::DanglingReference)?;
        let recovered = *values
            .get(root.get() as usize)
            .and_then(Option::as_ref)
            .ok_or(ReplayError::DanglingReference)?;
        out.push(recovered);
    }
    Ok(out)
}

/// Resolve one arena node from already-resolved premises.
fn resolve(
    graph: &BaseGraph,
    arena: &ProofArena,
    position: usize,
    values: &[Option<ReplayedFact>],
) -> Result<ReplayedFact, ReplayError> {
    let node = arena
        .get(ProofRef::new(
            u32::try_from(position).map_err(|_| ReplayError::DanglingReference)?,
        ))
        .ok_or(ReplayError::DanglingReference)?;
    let rules = graph.rules();
    match node {
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
            Ok(ReplayedFact {
                from: assertion.from,
                relation: assertion.relation,
                to: assertion.target,
                annotation: Annotation::present(assertion.confidence),
            })
        }
        ProofNode::Symmetry { premise } => {
            let inner = premise_value(premise, values)?;
            if !rules.spec(inner.relation).symmetric {
                return Err(ReplayError::RelationsNotSymmetric(inner.relation));
            }
            Ok(ReplayedFact {
                from: inner.to,
                relation: inner.relation,
                to: inner.from,
                annotation: inner.annotation,
            })
        }
        ProofNode::DefinitionImpliesDependency { premise } => {
            let inner = premise_value(premise, values)?;
            if !rules.spec(inner.relation).implies_dependency {
                return Err(ReplayError::RuleNotActive);
            }
            if !rules.spec(inner.relation).is_definition {
                return Err(ReplayError::PremiseNotDefinition(inner.relation));
            }
            Ok(ReplayedFact {
                from: inner.from,
                relation: RelationId::DependsOn,
                to: inner.to,
                annotation: inner.annotation,
            })
        }
        ProofNode::Transitive { left, right } => {
            let left_value = premise_value(left, values)?;
            let right_value = premise_value(right, values)?;
            if left_value.relation != right_value.relation {
                return Err(ReplayError::RelationMismatch {
                    left: left_value.relation,
                    right: right_value.relation,
                });
            }
            if !rules.spec(left_value.relation).transitive {
                return Err(ReplayError::RelationsNotTransitive(left_value.relation));
            }
            if left_value.to != right_value.from {
                return Err(ReplayError::Disconnected);
            }
            Ok(ReplayedFact {
                from: left_value.from,
                relation: left_value.relation,
                to: right_value.to,
                annotation: left_value.annotation.meet(right_value.annotation),
            })
        }
    }
}

fn premise_value(
    reference: ProofRef,
    values: &[Option<ReplayedFact>],
) -> Result<ReplayedFact, ReplayError> {
    values
        .get(reference.get() as usize)
        .and_then(Option::as_ref)
        .copied()
        .ok_or(ReplayError::DanglingReference)
}

fn record_budget(_error: QueryError) -> ReplayError {
    ReplayError::BudgetExhausted
}

/// Replay one proof with an explicit budget and control.
///
/// # Errors
///
/// Returns a typed [`ReplayError`].
pub fn replay_with_limit(
    graph: &BaseGraph,
    arena: &ProofArena,
    root: ProofRef,
    control: &dyn QueryControl,
    budget: sage_db_types::Budget,
) -> Result<ReplayedFact, ReplayError> {
    let row = sage_db_types::FactRow {
        from: TermId::new(0),
        relation: RelationId::DependsOn,
        to: TermId::new(0),
        annotation: Annotation::ABSENT,
        has_asserted: false,
        has_derived: false,
        disputed: false,
        strongest_support: None,
        proof: Some(root),
    };
    let facts = replay_rows(graph, arena, &[row], control, budget)?;
    facts
        .into_iter()
        .next()
        .ok_or(ReplayError::DanglingReference)
}

/// Replay one proof under a small default budget.
///
/// Convenience only: it delegates to [`replay_with_limit`] with a finite budget rather
/// than an unlimited one, so a hostile arena cannot run unbounded.
///
/// # Errors
///
/// Returns a typed [`ReplayError`].
pub fn replay(
    graph: &BaseGraph,
    arena: &ProofArena,
    root: ProofRef,
) -> Result<ReplayedFact, ReplayError> {
    replay_with_limit(
        graph,
        arena,
        root,
        &sage_db_types::RunToCompletion,
        sage_db_types::Budget::DEFAULT,
    )
}
#[cfg(test)]
mod allocation_property {
    //! Properties of the real [`push`]: refusal must happen *before* the arena
    //! allocates, so a rejected push leaves len, capacity, bytes and contents
    //! untouched. Written against production `push`, not a copied ceiling formula.
    use super::*;
    use sage_db_types::{Budget, RunToCompletion};

    fn snapshot_of(arena: &ProofArena) -> (usize, usize, u64, Vec<ProofNode>) {
        (
            arena.len(),
            arena.capacity(),
            arena.bytes(),
            arena.nodes().to_vec(),
        )
    }

    fn node(id: u128) -> ProofNode {
        ProofNode::Asserted {
            assertion_id: AssertionId::new(id),
            revision: RecordRevision::FIRST,
        }
    }

    /// Empty arena, ledger already too full for one more slot: must reject and
    /// leave the arena untouched (no capacity, no bytes, no contents).
    #[test]
    fn empty_arena_is_untouched_when_the_ledger_cannot_afford_one_slot() {
        let slot = ProofNode::slot_bytes();
        let budget = Budget {
            max_work_bytes: slot * 4,
            ..Budget::DEFAULT
        };
        let mut work = Work::new(budget);
        work.bytes(slot * 3 + 1)
            .expect("partially consume the ledger");
        assert!(
            work.remaining_bytes() < slot,
            "precondition: less than one slot left"
        );

        let mut arena = ProofArena::new();
        let before = snapshot_of(&arena);
        let outcome = push(&mut arena, node(1), &RunToCompletion, &mut work);
        println!(
            "empty arena, remaining={} slot={} -> {:?}",
            work.remaining_bytes(),
            slot,
            outcome.as_ref().map_err(|e| e.to_string())
        );
        assert!(outcome.is_err(), "must be a typed rejection");
        assert_eq!(
            snapshot_of(&arena),
            before,
            "a refused push must not allocate: len/capacity/bytes/contents changed"
        );
    }

    /// Arena already at capacity and about to grow, but the ledger cannot cover the
    /// growth: must reject before reserving.
    #[test]
    fn growing_arena_is_untouched_when_the_ledger_cannot_afford_the_growth() {
        let slot = ProofNode::slot_bytes();
        let budget = Budget {
            max_work_bytes: slot * 8,
            ..Budget::DEFAULT
        };
        let mut work = Work::new(budget);
        let mut arena = ProofArena::new();
        // Fill to capacity under the full ledger.
        push(&mut arena, node(1), &RunToCompletion, &mut work).expect("first push");
        push(&mut arena, node(2), &RunToCompletion, &mut work).expect("second push");
        assert_eq!(
            arena.len(),
            arena.capacity(),
            "precondition: next push must grow"
        );
        let capacity_before = arena.capacity();

        // Spend the ledger down so the *growth* is unaffordable but a single slot is not.
        let spent = work.work_bytes();
        let room = budget.max_work_bytes.saturating_sub(spent);
        let growth = (capacity_before as u64) * slot; // doubling: delta == current capacity
        assert!(room >= 1, "precondition: some room left");
        if room > growth {
            work.bytes(room - growth + 1)
                .expect("consume below the growth delta");
        }
        assert!(
            work.remaining_bytes() < growth,
            "precondition: growth unaffordable (remaining {} growth {growth})",
            work.remaining_bytes()
        );

        let before = snapshot_of(&arena);
        let outcome = push(&mut arena, node(3), &RunToCompletion, &mut work);
        println!(
            "growing arena, remaining={} growth={growth} -> {:?}",
            work.remaining_bytes(),
            outcome.as_ref().map_err(|e| e.to_string())
        );
        assert!(outcome.is_err(), "must be a typed rejection");
        assert_eq!(
            snapshot_of(&arena),
            before,
            "refused growth must not reserve capacity or push the node"
        );
    }

    /// Exactly enough ledger for one slot: must succeed and charge that slot.
    #[test]
    fn exact_remaining_succeeds_and_is_charged() {
        let slot = ProofNode::slot_bytes();
        let budget = Budget {
            max_work_bytes: slot * 4,
            ..Budget::DEFAULT
        };
        let mut work = Work::new(budget);
        let mut arena = ProofArena::new();
        let before = work.work_bytes();
        push(&mut arena, node(1), &RunToCompletion, &mut work).expect("affordable push");
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.bytes(), arena.capacity() as u64 * slot);
        assert_eq!(
            work.work_bytes() - before,
            arena.bytes(),
            "the charge must equal the arena's real resident bytes"
        );
    }
}
