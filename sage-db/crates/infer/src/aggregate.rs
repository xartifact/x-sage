//! Active-support aggregation and rule seeds.
//!
//! Each authored assertion keeps its own identity, revision, confidence and
//! direction. Aggregation produces, per `(from, relation, target)`:
//!
//! - the effective annotation (`max` over supports),
//! - whether any support is `contested` (the dispute flag),
//! - the single strongest direct-authored support;
//! - whether a non-circular rule route reaches the fact.
//!
//! It never merges two assertions into one record, and never rewrites a symmetric
//! assertion's direction (`docs/design/axiomatics.md` §4).
//!
//! ## Source flags
//!
//! `has_asserted` means an authored assertion exists **in this exact direction**.
//! A reverse reading via R2 is *derived*: the author did not state it that way.
//! `R2(R2(a))` returns to the authored direction and adds no new source, so it must
//! never set `has_derived` on its own.
//!
//! `disputed` is over **all** valid supports and paths, not just the winning proof:
//! `max` may raise the annotation but must not erase a recorded objection.

use sage_db_types::{
    Annotation, AssertionId, AssertionRecord, BaseGraph, Confidence, QueryControl, QueryError,
    RecordRevision, RelationId, TermId,
};

use crate::meter::Work;

/// One aggregated fact key: an authored direction.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FactKey {
    /// Subject.
    pub from: TermId,
    /// Relation.
    pub relation: RelationId,
    /// Object.
    pub to: TermId,
}

/// The strongest direct-authored support for one direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SupportWitness {
    /// Assertion identity.
    pub id: AssertionId,
    /// Revision active at this snapshot.
    pub revision: RecordRevision,
    /// Authored confidence.
    pub confidence: Confidence,
}

/// Aggregated state for one fact direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FactState {
    /// Effective annotation from direct authors.
    pub direct: Annotation,
    /// A direct authored assertion exists in this direction.
    pub has_asserted: bool,
    /// Some direct or derived support is contested.
    pub disputed: bool,
    /// Strongest direct support, if any.
    ///
    /// "Strongest" is by confidence, then lowest assertion id, so the choice is
    /// stable. It is a witness, not an exhaustive support list.
    pub witness: Option<SupportWitness>,
}

impl FactState {
    /// Empty state: nothing known about this direction.
    pub const EMPTY: Self = Self {
        direct: Annotation::ABSENT,
        has_asserted: false,
        disputed: false,
        witness: None,
    };
}

/// All aggregated direct facts plus derived route flags.
#[derive(Debug, Default)]
pub struct Aggregates {
    entries: Vec<(FactKey, FactState)>,
    reachable: Vec<bool>,
}

impl Aggregates {
    /// Look up a key in O(log n).
    #[must_use]
    pub fn get(&self, key: &FactKey) -> Option<&FactState> {
        self.entries
            .binary_search_by(|(candidate, _)| candidate.cmp(key))
            .ok()
            .map(|position| &self.entries[position].1)
    }

    /// Whether a non-circular rule route reaches this key.
    #[must_use]
    pub fn is_reachable(&self, key: &FactKey) -> bool {
        self.entries
            .binary_search_by(|(candidate, _)| candidate.cmp(key))
            .ok()
            .is_some_and(|position| self.reachable[position])
    }

    /// All keys in canonical order.
    pub fn keys(&self) -> impl Iterator<Item = &FactKey> {
        self.entries.iter().map(|(key, _)| key)
    }

    /// Number of distinct authored directions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no authored assertions exist.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Aggregate every authored assertion in `graph`.
///
/// # Errors
///
/// Propagates budget and control failures; nothing is returned on a partial scan.
pub fn aggregate(
    graph: &BaseGraph,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<Aggregates, QueryError> {
    let assertion_count = graph.assertion_count();
    let mut entries: Vec<(FactKey, FactState)> = Vec::new();
    crate::meter::grow(&mut entries, assertion_count, work)?;

    for assertion in graph.assertions() {
        work.step(1)?;
        work.periodic_check(control)?;
        let key = FactKey {
            from: assertion.from,
            relation: assertion.relation,
            to: assertion.target,
        };
        let state = FactState {
            direct: Annotation::present(assertion.confidence),
            has_asserted: true,
            disputed: assertion.confidence == Confidence::Contested,
            witness: Some(SupportWitness {
                id: assertion.id,
                revision: assertion.revision,
                confidence: assertion.confidence,
            }),
        };
        entries.push((key, state));
    }

    // Sort then fold duplicates: one row per authored direction, with the effective
    // annotation, dispute flag and strongest witness merged across supports.
    entries.sort_unstable_by_key(|(key, _)| *key);
    let mut merged: Vec<(FactKey, FactState)> = Vec::new();
    crate::meter::grow(&mut merged, entries.len(), work)?;
    for (key, state) in entries {
        match merged.last_mut() {
            Some((last_key, last_state)) if *last_key == key => {
                merge_into(last_state, state);
            }
            _ => merged.push((key, state)),
        }
    }

    mark_derived_routes(graph, &mut merged, control, work)?;
    let mut reachable: Vec<bool> = Vec::new();
    crate::meter::grow(&mut reachable, merged.len(), work)?;
    reachable.resize(merged.len(), false);
    Ok(Aggregates {
        entries: merged,
        reachable,
    })
}

/// Merge one support into an existing aggregate.
fn merge_into(into: &mut FactState, support: FactState) {
    into.direct = into.direct.join(support.direct);
    into.has_asserted |= support.has_asserted;
    into.disputed |= support.disputed;
    into.witness = match (into.witness, support.witness) {
        (None, other) | (other, None) => other,
        (Some(left), Some(right)) => Some(stronger_witness(left, right)),
    };
}

/// Pick the stronger witness: higher confidence, then lower assertion id.
///
/// The id tiebreak is what makes the choice deterministic when two supports carry
/// the same confidence.
fn stronger_witness(left: SupportWitness, right: SupportWitness) -> SupportWitness {
    if (left.confidence, std::cmp::Reverse(left.id.get()))
        >= (right.confidence, std::cmp::Reverse(right.id.get()))
    {
        left
    } else {
        right
    }
}

/// Mark facts reachable by a non-circular rule route.
///
/// This runs **after** direct aggregation and only records that *some* legitimate
/// route exists; it never invents a stronger annotation (that is the BFS's job) and
/// never clears a flag another route set.
fn mark_derived_routes(
    graph: &BaseGraph,
    entries: &mut Vec<(FactKey, FactState)>,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<(), QueryError> {
    let profile = graph.rules().profile();
    // Derived keys go into their own buffer, then merge-join once. Inserting into
    // `entries` while scanning would be O(n) per insert and could reallocate outside
    // the ledger.
    let mut derived: Vec<FactKey> = Vec::new();
    crate::meter::grow(&mut derived, graph.assertion_count(), work)?;

    // R2: a symmetric relation read from the other side is a derived route. It must
    // not count as `asserted` in that direction, and a round trip adds nothing.
    for assertion in graph.assertions() {
        work.step(1)?;
        work.periodic_check(control)?;
        if !profile.spec(assertion.relation).symmetric {
            continue;
        }
        derived.push(FactKey {
            from: assertion.target,
            relation: assertion.relation,
            to: assertion.from,
        });
    }

    // R3: a direct definition assertion is a derived route to `depends-on`.
    if profile == sage_db_types::RuleProfile::FormalV1 {
        for assertion in graph.assertions() {
            work.step(1)?;
            work.periodic_check(control)?;
            if !profile.spec(assertion.relation).implies_dependency {
                continue;
            }
            derived.push(FactKey {
                from: assertion.from,
                relation: RelationId::DependsOn,
                to: assertion.target,
            });
        }
    }

    if derived.is_empty() {
        return Ok(());
    }
    derived.sort_unstable();
    derived.dedup();

    // Merge-join two sorted sequences into a third, reserved exactly.
    let mut joined: Vec<(FactKey, FactState)> = Vec::new();
    crate::meter::grow(&mut joined, entries.len() + derived.len(), work)?;
    let mut existing = entries.iter().peekable();
    let mut additions = derived.iter().peekable();
    loop {
        work.periodic_check(control)?;
        match (existing.peek(), additions.peek()) {
            (Some((key, _)), Some(derived_key)) => match key.cmp(derived_key) {
                std::cmp::Ordering::Less => {
                    joined.push(*existing.next().expect("peeked"));
                }
                std::cmp::Ordering::Greater => {
                    // A purely derived fact still needs a row so the BFS can refine it.
                    joined.push((*additions.next().expect("peeked"), FactState::EMPTY));
                }
                std::cmp::Ordering::Equal => {
                    joined.push(*existing.next().expect("peeked"));
                    additions.next();
                }
            },
            (Some(_), None) => joined.push(*existing.next().expect("peeked")),
            (None, Some(_)) => {
                joined.push((*additions.next().expect("peeked"), FactState::EMPTY));
            }
            (None, None) => break,
        }
    }
    *entries = joined;
    Ok(())
}

/// Whether `assertion` can contribute a *derived* route for its own direction.
///
/// Used by proof construction to find an alternate route that justifies a fact
/// without claiming authorship.
#[must_use]
pub fn is_rule_route(graph: &BaseGraph, assertion: &AssertionRecord) -> bool {
    let profile = graph.rules().profile();
    profile.spec(assertion.relation).symmetric
        || profile.spec(assertion.relation).implies_dependency
}
