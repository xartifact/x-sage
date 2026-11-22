//! Bounded widest-path evaluation with per-threshold provenance.
//!
//! The carrier is `bottom + {contested, hypothetical, confirmed}` with `min` along a
//! path and `max` across paths (`docs/design/axiomatics.md` §2).
//!
//! # Three independent threshold levels
//!
//! Strength and provenance are computed per level. A single per-node "best parent"
//! is **wrong**, and the counterexample is short:
//!
//! ```text
//! A -> B            hypothetical
//! A -> X -> Y -> B  confirmed (3 hops)
//! B -> T            hypothetical
//! ```
//!
//! `B`'s strongest annotation is confirmed (via the long route), yet `T`'s strongest
//! annotation is only hypothetical, and under *that* annotation the shortest route is
//! `A -> B -> T` (2 hops). Splicing `T`'s hypothetical target onto `B`'s confirmed
//! 3-hop parent yields a 4-hop proof for a 2-hop fact. So each level keeps its own
//! `(reached, hops, parent)` state, and a row at level `L` is proved from pass `L`.
//!
//! # Dense positions, no id-indexed allocation
//!
//! Positions come from binary-searching the graph's own sorted term table, so a sparse
//! `TermId` space costs nothing. Every parallel array is sized once to `term_count`
//! and reserved fallibly; nothing is indexed by `TermId::get()`.
//!
//! # Modes
//!
//! | mode | relations | semantics |
//! |---|---|---|
//! | `Transitive` | `is-a`, `part-of`, `depends-on` | closure over authored edges |
//! | `Symmetric` | `contradicts`, `analogous-to` | exactly one hop, both directions |
//! | `Direct` | `enables`, `instantiates` | exactly one authored hop |
//!
//! `Direct` exists because [`sage_db_types::SourceQuery`] asks for the complete set of
//! facts from a term, and a non-transitive relation still has authored facts.

use sage_db_types::{
    Annotation, AssertionId, BaseGraph, Confidence, QueryControl, QueryError, RecordRevision,
    RelationId, RuleProfile, TermId,
};

use crate::aggregate::FactKey;
use crate::meter::Work;

/// Confidence levels, strongest first. Index 0 is the strongest.
pub(crate) const LEVELS: [Confidence; 3] = [
    Confidence::Confirmed,
    Confidence::Hypothetical,
    Confidence::Contested,
];

/// Index of `level` inside [`LEVELS`].
pub(crate) fn level_index(level: Confidence) -> Option<usize> {
    LEVELS.iter().position(|candidate| *candidate == level)
}

/// One traversed edge with the support that licensed it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Witness {
    /// Reported direction of the fact.
    pub key: FactKey,
    /// Authored assertion providing this support.
    pub assertion: AssertionId,
    /// Revision of that assertion at this snapshot.
    pub revision: RecordRevision,
    /// Confidence the edge contributes.
    pub confidence: Confidence,
    /// True when this edge is an R3 seed from a definition assertion.
    pub via_definition: bool,
    /// True when this edge reads an authored assertion backwards (one R2).
    pub reversed: bool,
}

/// How one relation is evaluated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    /// Transitive closure over authored edges.
    Transitive,
    /// Exactly one hop, including backwards reads of authored edges.
    Symmetric,
    /// Exactly one authored hop, no reversal and no closure.
    Direct,
}

/// One traversable edge: the stored assertion plus how it is being read.
#[derive(Clone, Copy, Debug)]
struct TraversalEdge {
    from: TermId,
    to: TermId,
    assertion: AssertionId,
    revision: RecordRevision,
    confidence: Confidence,
    via_definition: bool,
    reversed: bool,
}

/// Precomputed adjacency restricted to the relation being evaluated.
#[derive(Debug)]
pub struct Adjacency {
    edges: Vec<TraversalEdge>,
    by_source: Vec<(TermId, std::ops::Range<u32>)>,
    mode: Mode,
}

impl Adjacency {
    /// Collect the edges that participate in this evaluation.
    ///
    /// # Errors
    ///
    /// Propagates budget and control failures.
    pub fn build(
        graph: &BaseGraph,
        relation: RelationId,
        control: &dyn QueryControl,
        work: &mut Work,
    ) -> Result<Self, QueryError> {
        let rules = graph.rules();
        let spec = rules.spec(relation);
        let mode = if spec.transitive {
            Mode::Transitive
        } else if spec.symmetric {
            Mode::Symmetric
        } else {
            Mode::Direct
        };
        // A symmetric relation records both orientations; reserving exactly the
        // authored count would grow the buffer silently on the second push.
        let capacity = if mode == Mode::Symmetric {
            graph.assertion_count().saturating_mul(2)
        } else {
            graph.assertion_count()
        };

        let mut edges: Vec<TraversalEdge> = Vec::new();
        crate::meter::grow(&mut edges, capacity, work)?;
        let mut by_source: Vec<(TermId, std::ops::Range<u32>)> = Vec::new();
        crate::meter::grow(&mut by_source, graph.term_count(), work)?;

        for assertion in graph.assertions() {
            work.step(1)?;
            work.periodic_check(control)?;
            let is_r3_seed = relation == RelationId::DependsOn
                && rules.profile() == RuleProfile::FormalV1
                && rules.spec(assertion.relation).implies_dependency;
            if assertion.relation != relation && !is_r3_seed {
                continue;
            }
            edges.push(TraversalEdge {
                from: assertion.from,
                to: assertion.target,
                assertion: assertion.id,
                revision: assertion.revision,
                confidence: assertion.confidence,
                via_definition: is_r3_seed,
                reversed: false,
            });
            if mode == Mode::Symmetric {
                edges.push(TraversalEdge {
                    from: assertion.target,
                    to: assertion.from,
                    assertion: assertion.id,
                    revision: assertion.revision,
                    confidence: assertion.confidence,
                    via_definition: false,
                    reversed: true,
                });
            }
        }

        // Group by source. Sorting by `(from, to, assertion)` gives each source's
        // edges in the documented order, which is what makes BFS tie-breaking stable.
        edges.sort_unstable_by_key(|edge| (edge.from, edge.to, edge.assertion));

        let mut start = 0usize;
        for position in 0..edges.len() {
            let is_last = position + 1 == edges.len();
            if is_last || edges[position + 1].from != edges[position].from {
                by_source.push((
                    edges[position].from,
                    crate::meter::narrow(start)?..crate::meter::narrow(position + 1)?,
                ));
                start = position + 1;
            }
        }
        Ok(Self {
            edges,
            by_source,
            mode,
        })
    }

    /// Visit the endpoints of this relation's canonical single-hop seeds.
    ///
    /// Includes virtual definition seeds for formal-v1 `depends-on`, exactly as the
    /// evaluator does. This borrows the already-built adjacency: no new edge table or
    /// second rule implementation. Each visited edge is charged on the caller's SAME
    /// work meter and control is consulted even when the adjacency is empty.
    ///
    /// # Errors
    /// Preserves the caller's stop, work-budget refusal or visitor's query error.
    pub fn visit_seed_endpoints(
        &self,
        control: &dyn QueryControl,
        work: &mut Work,
        mut visitor: impl FnMut(TermId, TermId, &mut Work) -> Result<(), QueryError>,
    ) -> Result<(), QueryError> {
        work.check(control)?;
        for edge in &self.edges {
            work.step(1)?;
            work.periodic_check(control)?;
            visitor(edge.from, edge.to, work)?;
        }
        work.check(control)
    }

    fn from(&self, term: TermId) -> &[TraversalEdge] {
        match self
            .by_source
            .binary_search_by_key(&term, |(source, _)| *source)
        {
            Ok(position) => {
                let range = self.by_source[position].1.clone();
                &self.edges[range.start as usize..range.end as usize]
            }
            Err(_) => &[],
        }
    }

    /// Number of traversable edges.
    #[must_use]
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    /// Whether nothing is traversable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

/// Per-level provenance for one term.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LevelState {
    pub(crate) reached: bool,
    pub(crate) hops: u32,
    pub(crate) parent: Option<Witness>,
}

impl LevelState {
    const UNREACHED: Self = Self {
        reached: false,
        hops: u32::MAX,
        parent: None,
    };
}

/// Result of a single-source evaluation.
///
/// The term table is a copy of the graph's sorted ids; positions are found by binary
/// search, so storage is proportional to the graph, never to the largest id.
#[derive(Debug)]
pub struct Reach {
    order: Vec<TermId>,
    table: Vec<TermId>,
    levels: Vec<[LevelState; 3]>,
    disputed: Vec<bool>,
    derived: Vec<bool>,
}

impl Reach {
    /// Create an empty reach, reserving every parallel array once.
    ///
    /// # Errors
    ///
    /// Propagates budget failures from the reservations.
    pub(crate) fn new(graph: &BaseGraph, work: &mut Work) -> Result<Self, QueryError> {
        let bound = graph.term_count();
        let mut table: Vec<TermId> = Vec::new();
        let mut levels: Vec<[LevelState; 3]> = Vec::new();
        let mut disputed: Vec<bool> = Vec::new();
        let mut derived: Vec<bool> = Vec::new();
        let mut order: Vec<TermId> = Vec::new();
        crate::meter::grow(&mut table, bound, work)?;
        crate::meter::grow(&mut levels, bound, work)?;
        crate::meter::grow(&mut disputed, bound, work)?;
        crate::meter::grow(&mut derived, bound, work)?;
        crate::meter::grow(&mut order, bound, work)?;

        table.extend(graph.terms().iter().map(|meta| meta.id));
        levels.resize(bound, [LevelState::UNREACHED; 3]);
        disputed.resize(bound, false);
        derived.resize(bound, false);
        Ok(Self {
            order,
            table,
            levels,
            disputed,
            derived,
        })
    }

    /// Dense position of `term`, or `None` when it is not a graph term.
    fn index_of(&self, term: TermId) -> Option<usize> {
        self.table.binary_search(&term).ok()
    }

    /// Position of a reached term, recording it on first contact.
    fn touch(&mut self, term: TermId) -> Option<usize> {
        let index = self.index_of(term)?;
        if !self.levels[index].iter().any(|state| state.reached) {
            // Nothing reached this term yet; remember it for ordered iteration.
            // Reserved to the graph's term count, so this cannot grow in practice;
            // the guard makes a future bound change fail loudly instead of reallocating.
            debug_assert!(self.order.len() < self.order.capacity());
            self.order.push(term);
        }
        Some(index)
    }

    /// Terms reached, in first-contact order.
    #[must_use]
    pub fn terms(&self) -> &[TermId] {
        &self.order
    }

    /// Best annotation across levels, for a reached term.
    #[must_use]
    pub fn annotation(&self, term: TermId) -> Option<Annotation> {
        let index = self.index_of(term)?;
        let states = &self.levels[index];
        let mut best = Annotation::ABSENT;
        for (position, level) in LEVELS.iter().enumerate() {
            if states[position].reached {
                best = best.join(Annotation::present(*level));
            }
        }
        Some(best)
    }

    /// The levels at which `term` was reached, strongest first.
    ///
    /// A row's proof must be built from the level that produced its reported
    /// annotation; mixing levels is the splicing bug this type exists to prevent.
    #[must_use]
    pub fn levels_of(&self, term: TermId) -> Vec<Confidence> {
        let Some(index) = self.index_of(term) else {
            return Vec::new();
        };
        LEVELS
            .iter()
            .enumerate()
            .filter(|(position, _)| self.levels[index][*position].reached)
            .map(|(_, level)| *level)
            .collect()
    }

    /// Hop count at the strongest level that reached `term`.
    #[must_use]
    pub fn hops(&self, term: TermId) -> Option<u32> {
        let index = self.index_of(term)?;
        LEVELS
            .iter()
            .enumerate()
            .find(|(position, _)| self.levels[index][*position].reached)
            .map(|(position, _)| self.levels[index][position].hops)
    }

    /// Hop count at one specific level.
    #[must_use]
    pub fn hops_at(&self, term: TermId, level: Confidence) -> Option<u32> {
        let index = self.index_of(term)?;
        let position = level_index(level)?;
        let state = self.levels[index][position];
        state.reached.then_some(state.hops)
    }

    /// Parent witness at a specific level.
    #[must_use]
    pub fn parent_at(&self, term: TermId, level: Confidence) -> Option<Witness> {
        let index = self.index_of(term)?;
        let position = level_index(level)?;
        let state = self.levels[index][position];
        state.reached.then_some(state.parent).flatten()
    }

    /// Whether any valid route reaching `term` is disputed.
    #[must_use]
    pub fn disputed(&self, term: TermId) -> bool {
        self.index_of(term)
            .is_some_and(|index| self.disputed[index])
    }

    /// Whether a non-circular rule route reaches `term`.
    #[must_use]
    pub fn derived(&self, term: TermId) -> bool {
        self.index_of(term).is_some_and(|index| self.derived[index])
    }
}

/// Evaluate `relation` from `source`.
///
/// # Errors
///
/// Propagates budget and control failures. A returned [`Reach`] is complete under
/// this budget.
pub fn evaluate(
    graph: &BaseGraph,
    adjacency: &Adjacency,
    relation: RelationId,
    source: TermId,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<Reach, QueryError> {
    let mut reach = Reach::new(graph, work)?;
    let Some(root) = reach.index_of(source) else {
        return Err(QueryError::UnknownTerm(source));
    };
    reach.touch(source);
    for state in &mut reach.levels[root] {
        state.reached = true;
        state.hops = 0;
    }

    for (position, level) in LEVELS.iter().enumerate() {
        work.check(control)?;
        level_pass(
            LevelRequest {
                adjacency,
                relation,
                source,
                level_position: position,
                level: *level,
            },
            &mut reach,
            control,
            work,
        )?;
    }
    flags_pass(adjacency, source, &mut reach, control, work)?;
    Ok(reach)
}

/// One level's BFS, writing that level's provenance only.
struct LevelRequest<'a> {
    adjacency: &'a Adjacency,
    relation: RelationId,
    source: TermId,
    level_position: usize,
    level: Confidence,
}

fn level_pass(
    request: LevelRequest<'_>,
    reach: &mut Reach,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<(), QueryError> {
    let LevelRequest {
        adjacency,
        relation,
        source,
        level_position,
        level,
    } = request;
    let root = reach
        .index_of(source)
        .ok_or(QueryError::UnknownTerm(source))?;
    let mut queue: crate::meter::BoundedQueue<TermId> = crate::meter::BoundedQueue::new();
    queue.push(source, work)?;
    reach.levels[root][level_position] = LevelState {
        reached: true,
        hops: 0,
        parent: None,
    };

    // `Direct` and `Symmetric` are exactly one hop: after the source there is nothing
    // to expand.
    let expand = adjacency.mode == Mode::Transitive;
    let bound = adjacency.len().saturating_add(1);
    let mut visited = 0usize;

    while let Some(current) = queue.pop() {
        work.row()?;
        work.periodic_check(control)?;
        visited += 1;
        if visited > bound {
            return Err(QueryError::InvalidRequest {
                reason: "evaluation visited more nodes than the graph contains",
            });
        }

        let Some(current_index) = reach.index_of(current) else {
            continue;
        };
        let current_hops = reach.levels[current_index][level_position].hops;

        for edge in adjacency.from(current) {
            work.step(1)?;
            work.periodic_check(control)?;
            if edge.confidence < level {
                continue;
            }
            let Some(target_index) = reach.touch(edge.to) else {
                continue;
            };
            let candidate_hops = current_hops.saturating_add(1);
            let state = &reach.levels[target_index][level_position];
            if state.reached && candidate_hops >= state.hops {
                // First qualifying predecessor in BFS order wins. Comparing
                // confidences after the fact would be a different, weaker rule.
                continue;
            }
            reach.levels[target_index][level_position] = LevelState {
                reached: true,
                hops: candidate_hops,
                parent: Some(Witness {
                    key: FactKey {
                        from: current,
                        relation,
                        to: edge.to,
                    },
                    assertion: edge.assertion,
                    revision: edge.revision,
                    confidence: edge.confidence,
                    via_definition: edge.via_definition,
                    reversed: edge.reversed,
                }),
            };
            if expand {
                queue.push(edge.to, work)?;
            }
        }
        if !expand {
            break;
        }
    }
    Ok(())
}

/// One unrestricted pass, deciding provenance flags.
///
/// Both flags describe routes that *exist*, not routes that won on strength, so this
/// pass ignores thresholds: a contested or longer-derived route is recorded even when
/// a stronger shorter route supplies the annotation.
fn flags_pass(
    adjacency: &Adjacency,
    source: TermId,
    reach: &mut Reach,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<(), QueryError> {
    let count = reach.table.len();
    let mut distance: Vec<u32> = Vec::new();
    let mut contested: Vec<bool> = Vec::new();
    crate::meter::grow(&mut distance, count, work)?;
    crate::meter::grow(&mut contested, count, work)?;
    distance.resize(count, u32::MAX);
    contested.resize(count, false);

    let root = reach
        .index_of(source)
        .ok_or(QueryError::UnknownTerm(source))?;
    let mut queue: crate::meter::BoundedQueue<usize> = crate::meter::BoundedQueue::new();
    queue.push(root, work)?;
    distance[root] = 0;

    let expand = adjacency.mode == Mode::Transitive;

    while let Some(current_index) = queue.pop() {
        work.row()?;
        work.periodic_check(control)?;
        let current = reach.table[current_index];
        let current_distance = distance[current_index];
        let current_contested = contested[current_index];

        for edge in adjacency.from(current) {
            work.step(1)?;
            work.periodic_check(control)?;
            let Some(target_index) = reach.index_of(edge.to) else {
                continue;
            };
            let candidate_distance = current_distance.saturating_add(1);
            let candidate_contested = current_contested || edge.confidence == Confidence::Contested;

            if candidate_contested {
                reach.disputed[target_index] = true;
            }
            if edge.via_definition || edge.reversed || candidate_distance >= 2 {
                reach.derived[target_index] = true;
            }

            let improves = candidate_distance < distance[target_index];
            let unlocks_contested = candidate_contested && !contested[target_index];
            if improves {
                distance[target_index] = candidate_distance;
                contested[target_index] = candidate_contested;
                if expand {
                    queue.push(target_index, work)?;
                }
            } else if unlocks_contested {
                contested[target_index] = true;
                if expand {
                    queue.push(target_index, work)?;
                }
            }
        }
        if !expand {
            break;
        }
    }
    Ok(())
}
