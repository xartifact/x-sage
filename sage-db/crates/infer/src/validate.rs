//! Whole-candidate graph validation (V1–V6) and P0 lifecycle checks.
//!
//! Scope: this validates one immutable [`BaseGraph`] — the *candidate state* after
//! a set of operations. It is the P0 subset of `docs/design/inference.md` §2.
//!
//! # What this is not
//!
//! There is **no** proposal, evidence, baseline or transaction logic here. V7 in the
//! design covers "retirement has no active references, revision monotonicity,
//! evidence/proposal baseline agreement", which needs P2 record lifecycle and the
//! store's candidate transaction. P0 has neither, so this module reports only the
//! checks it can actually perform and says so in [`ValidationReport`] rather than
//! claiming V7 coverage.
//!
//! # Legacy mixed-definition cycles
//!
//! Under `legacy-v1` the design says a cycle in the definition-plus-dependency union
//! makes `definition_depth` *unavailable*; it does not invalidate data that was
//! already legal. That distinction is preserved here: the cycle is reported as
//! [`MetricStatus::Unavailable`], never as a validation failure.

use std::collections::BTreeSet;

use sage_db_types::{
    BaseGraph, Budget, BudgetLimit, QueryControl, QueryError, RelationId, RuleProfile,
    SemanticLayer, TermId,
};

use crate::meter::Work;

/// Which constraint was violated.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Violation {
    /// V1: an assertion endpoint is not a term in the graph.
    UnknownEndpoint {
        /// Missing endpoint.
        term: TermId,
        /// Assertion carrying it.
        assertion: sage_db_types::AssertionId,
    },
    /// V2: an authored relation relates a term to itself.
    SelfRelation {
        /// The self-related term.
        term: TermId,
        /// Assertion carrying it.
        assertion: sage_db_types::AssertionId,
    },
    /// V3: the relation's layer rule rejects `source -> target`.
    LayerDirection {
        /// Assertion with a disallowed direction.
        assertion: sage_db_types::AssertionId,
        /// Relation whose rule was violated.
        relation: RelationId,
        /// Authored source layer.
        source_layer: SemanticLayer,
        /// Authored target layer.
        target_layer: SemanticLayer,
    },
    /// V4: a subject instantiates more than one **distinct** target.
    ///
    /// Counts unique endpoints, not supporting assertions: three assertions
    /// pointing at the same target are legal.
    MultipleInstantiationTargets {
        /// Subject term.
        subject: TermId,
        /// Distinct targets found, in ascending order.
        targets: Vec<TermId>,
    },
    /// V5: an active L3 term has no instantiation target.
    LayerThreeWithoutTarget {
        /// The uncovered instance term.
        term: TermId,
    },
    /// V6: a relation declared acyclic contains a cycle.
    RelationCycle {
        /// Relation containing the cycle.
        relation: RelationId,
        /// One cycle, as a closed walk of terms.
        cycle: Vec<TermId>,
    },
    /// V6 (formal-v1 only): the definition-plus-dependency union contains a cycle.
    ///
    /// Under `legacy-v1` the same shape is reported as
    /// [`MetricStatus::Unavailable`], because legacy data may legally contain it.
    DefinitionUnionCycle {
        /// One cycle in the mixed union.
        cycle: Vec<TermId>,
    },
}

impl Violation {
    /// Stable identifier, matching the design's V1–V6 labels.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownEndpoint { .. } => "V1",
            Self::SelfRelation { .. } => "V2",
            Self::LayerDirection { .. } => "V3",
            Self::MultipleInstantiationTargets { .. } => "V4",
            Self::LayerThreeWithoutTarget { .. } => "V5",
            Self::RelationCycle { .. } => "V6",
            Self::DefinitionUnionCycle { .. } => "V6",
        }
    }
}

/// Whether the derived `definition_depth` metric can be computed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MetricStatus {
    /// The definition union is acyclic; depths are computable.
    Available,
    /// The union has a cycle, so depth is undefined. Under `legacy-v1` this is a
    /// metric limitation, **not** a validation failure and **not** a query error.
    Unavailable {
        /// Why the metric is unavailable.
        reason: &'static str,
    },
}

/// Outcome of validating one candidate graph.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ValidationReport {
    /// Violations found, in a deterministic order (by code, then payload).
    pub violations: Vec<Violation>,
    /// Depth-metric availability.
    pub metric: MetricStatus,
    /// Constraints this P0 validator implements.
    ///
    /// Reported explicitly so a caller cannot read silent absence of a check as
    /// evidence that the check passed.
    pub enforced: &'static [&'static str],
    /// Constraints that exist in the design but require P2 record lifecycle.
    ///
    /// Always non-empty in P0: V7 needs candidate transactions, revisions and
    /// proposal/evidence baselines that this layer does not have.
    pub not_enforced: &'static [&'static str],
}

/// Constraints implemented by [`validate`].
pub const ENFORCED: &[&str] = &[
    "V1 endpoint existence",
    "V2 no authored self-relation",
    "V3 relation layer rules (profile-dependent)",
    "V4 unique instantiation target (formal-v1)",
    "V5 L3 coverage (formal-v1)",
    "V6 per-relation acyclicity",
    "V6 definition+dependency union acyclicity (formal-v1)",
];

/// Constraints deliberately not implemented in P0.
pub const NOT_ENFORCED: &[&str] = &[
    "V7 retirement reference disposal (needs P2 record lifecycle)",
    "V7 revision monotonicity (needs the store's candidate transaction)",
    "V7 evidence/proposal baseline agreement (needs P2 proposals)",
    "reference-in-use across non-graph records (claims, evidence, vectors)",
];

impl ValidationReport {
    /// Whether the candidate may be published.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Validate `graph` against the checks in [`ENFORCED`].
///
/// The graph is already structurally sound by construction (unique identities,
/// endpoints that resolve, no self references — see `BaseGraph::new`), so V1/V2 are
/// re-checked here only to keep the report self-describing if a future builder
/// relaxes those invariants. The real work is V3–V6.
///
/// # Errors
///
/// Returns [`QueryError`] when work, rows, bytes or control stop the check. A
/// budget stop is **not** a validation verdict: the caller must not read it as
/// "valid" or "invalid".
pub fn validate(
    graph: &BaseGraph,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<ValidationReport, QueryError> {
    let profile = graph.rules().profile();
    // Diagnostics are budgeted too: a star graph can violate a rule on every edge.
    let mut violations = Vec::new();
    // Diagnostics can outnumber assertions: N isolated L3 terms produce N V5 entries
    // with zero assertions. Grow on demand rather than trusting an assertion bound.
    crate::meter::grow(&mut violations, graph.term_count(), work)?;

    check_nodes(graph, control, work)?;
    control.checkpoint()?;
    check_layer_direction(graph, profile, &mut violations, control, work)?;
    control.checkpoint()?;

    if profile == RuleProfile::FormalV1 {
        check_instantiation_cardinality(graph, &mut violations, control, work)?;
        control.checkpoint()?;
        check_l3_coverage(graph, &mut violations, control, work)?;
        control.checkpoint()?;
    }

    check_relation_cycles(graph, &mut violations, control, work)?;
    control.checkpoint()?;

    let metric = check_definition_union(graph, profile, &mut violations, control, work)?;

    // No sort: every check above walks terms and adjacency in graph order, so the
    // reported sequence is already deterministic. Sorting by a formatted debug string
    // would allocate outside the budget for no gain.

    Ok(ValidationReport {
        violations,
        metric,
        enforced: ENFORCED,
        not_enforced: NOT_ENFORCED,
    })
}

/// V1 + V2 over every authored assertion.
fn check_nodes(
    graph: &BaseGraph,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<(), QueryError> {
    for assertion in graph.assertions() {
        work.step(1)?;
        work.periodic_check(control)?;
        for endpoint in [assertion.from, assertion.target] {
            if graph.term(endpoint).is_none() {
                return Err(QueryError::UnknownTerm(endpoint));
            }
        }
        if assertion.from == assertion.target {
            // Unreachable through `BaseGraph::new`, but the report must be able to
            // describe it rather than silently pass.
            return Err(QueryError::InvalidRequest {
                reason: "graph contains an authored self-relation",
            });
        }
    }
    Ok(())
}

/// V3: apply the profile's layer rule to every authored assertion.
fn check_layer_direction(
    graph: &BaseGraph,
    profile: RuleProfile,
    violations: &mut Vec<Violation>,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<(), QueryError> {
    for assertion in graph.assertions() {
        work.step(1)?;
        work.periodic_check(control)?;
        let rule = profile.spec(assertion.relation).layer_rule;
        let (Some(source), Some(target)) =
            (graph.term(assertion.from), graph.term(assertion.target))
        else {
            continue; // reported by check_nodes
        };
        if !rule.accepts(source.layer, target.layer) {
            work.row()?;
            violations.push(Violation::LayerDirection {
                assertion: assertion.id,
                relation: assertion.relation,
                source_layer: source.layer,
                target_layer: target.layer,
            });
        }
    }
    Ok(())
}

/// V4: at most one distinct instantiation target per subject (formal-v1).
///
/// Distinct endpoints, not support count: several assertions at the same target are
/// one target, which is exactly the case the old implementation got wrong.
fn check_instantiation_cardinality(
    graph: &BaseGraph,
    violations: &mut Vec<Violation>,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<(), QueryError> {
    for term in graph.terms() {
        work.step(1)?;
        work.periodic_check(control)?;
        let mut targets: Vec<TermId> = Vec::new();
        crate::meter::grow(&mut targets, graph.assertions_from(term.id).len(), work)?;
        for assertion in graph.assertions_from(term.id) {
            work.step(1)?;
            work.periodic_check(control)?;
            if assertion.relation == RelationId::Instantiates {
                targets.push(assertion.target);
            }
        }
        targets.sort_unstable();
        targets.dedup();
        if targets.len() > 1 {
            work.row()?;
            violations.push(Violation::MultipleInstantiationTargets {
                subject: term.id,
                targets,
            });
        }
    }
    Ok(())
}

/// V5: every active L3 term instantiates something (formal-v1).
fn check_l3_coverage(
    graph: &BaseGraph,
    violations: &mut Vec<Violation>,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<(), QueryError> {
    for term in graph.terms() {
        work.step(1)?;
        work.periodic_check(control)?;
        if term.layer != SemanticLayer::L3 {
            continue;
        }
        // Charge every scanned edge: `.any()` would hide a scan over a wide L3 node.
        let mut covered = false;
        for assertion in graph.assertions_from(term.id) {
            work.step(1)?;
            work.periodic_check(control)?;
            if assertion.relation == RelationId::Instantiates {
                covered = true;
                break;
            }
        }
        if !covered {
            work.row()?;
            violations.push(Violation::LayerThreeWithoutTarget { term: term.id });
        }
    }
    Ok(())
}

/// V6: every relation marked `acyclic` must contain no cycle.
fn check_relation_cycles(
    graph: &BaseGraph,
    violations: &mut Vec<Violation>,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<(), QueryError> {
    let profile = graph.rules().profile();
    for relation in RelationId::ALL {
        if !profile.spec(relation).acyclic {
            continue;
        }
        if let Some(cycle) = find_cycle(
            graph,
            |assertion| assertion.relation == relation,
            control,
            work,
        )? {
            work.row()?;
            violations.push(Violation::RelationCycle { relation, cycle });
        }
    }
    Ok(())
}

/// V6 (formal) / metric availability (legacy) over the definition union.
fn check_definition_union(
    graph: &BaseGraph,
    profile: RuleProfile,
    violations: &mut Vec<Violation>,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<MetricStatus, QueryError> {
    let is_union_edge = |assertion: &sage_db_types::AssertionRecord| {
        assertion.relation == RelationId::DependsOn
            || profile.spec(assertion.relation).is_definition
    };

    let cycle = find_cycle(graph, is_union_edge, control, work)?;
    match (cycle, profile) {
        (None, _) => Ok(MetricStatus::Available),
        // Legacy data may legally contain this; the depth metric simply has no value.
        (Some(_), RuleProfile::LegacyV1) => Ok(MetricStatus::Unavailable {
            reason: "definition+dependency union is cyclic; definition_depth undefined",
        }),
        (Some(cycle), RuleProfile::FormalV1) => {
            work.row()?;
            violations.push(Violation::DefinitionUnionCycle { cycle });
            Ok(MetricStatus::Available)
        }
    }
}

/// Find one cycle among edges admitted by `admit`, using iterative DFS.
///
/// Iterative on purpose: a long `depends-on` chain must not blow the stack. The
/// returned walk is deterministic for a given graph because terms are visited in
/// ascending ID order and each adjacency list is already in visit-key order.
fn find_cycle<F>(
    graph: &BaseGraph,
    admit: F,
    control: &dyn QueryControl,
    work: &mut Work,
) -> Result<Option<Vec<TermId>>, QueryError>
where
    F: Fn(&sage_db_types::AssertionRecord) -> bool,
{
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Colour {
        White,
        Grey,
        Black,
    }

    let terms = graph.terms();
    let count = terms.len();
    // Every diagnostic structure is reserved once and charged: colour, the DFS frame
    // stack and the current path all grow to at most the term count.
    let mut colour: Vec<Colour> = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut path: Vec<TermId> = Vec::new();
    crate::meter::grow(&mut colour, count, work)?;
    crate::meter::grow(&mut stack, count, work)?;
    crate::meter::grow(&mut path, count, work)?;
    colour.resize(count, Colour::White);

    let position_of =
        |term: TermId| -> Option<usize> { terms.binary_search_by_key(&term, |meta| meta.id).ok() };

    for start in 0..count {
        work.step(1)?;
        work.periodic_check(control)?;
        if colour[start] != Colour::White {
            continue;
        }
        colour[start] = Colour::Grey;
        stack.push((start, 0));
        path.push(terms[start].id);

        while let Some((node, cursor)) = stack.pop() {
            // Walk the node's real adjacency with a cursor instead of materialising a
            // filtered Vec per visit. The previous version allocated and filtered the
            // full out-edge list on every frame, which is O(E^2) on a star graph and
            // was never charged.
            let adjacency = graph.assertions_from(terms[node].id);
            let mut advanced = false;
            for (position, assertion) in adjacency.iter().enumerate().skip(cursor) {
                work.step(1)?;
                work.periodic_check(control)?;
                if !admit(assertion) {
                    continue;
                }
                stack.push((node, position + 1));
                advanced = true;
                let Some(next) = position_of(assertion.target) else {
                    break;
                };
                match colour[next] {
                    Colour::White => {
                        colour[next] = Colour::Grey;
                        stack.push((next, 0));
                        path.push(assertion.target);
                    }
                    Colour::Grey => {
                        if let Some(at) = path.iter().position(|term| *term == assertion.target) {
                            let mut cycle = Vec::new();
                            crate::meter::grow(&mut cycle, path.len() - at + 1, work)?;
                            cycle.extend_from_slice(&path[at..]);
                            cycle.push(assertion.target);
                            return Ok(Some(cycle));
                        }
                    }
                    Colour::Black => {}
                }
                break;
            }
            if !advanced {
                colour[node] = Colour::Black;
                path.pop();
            }
        }
    }
    Ok(None)
}

/// Terms that appear in at least one cycle, for diagnostics.
///
/// Kept separate from [`validate`] so validation stays a pure verdict function.
#[must_use]
pub fn cyclic_terms(cycles: &[Violation]) -> BTreeSet<TermId> {
    let mut terms = BTreeSet::new();
    for violation in cycles {
        match violation {
            Violation::RelationCycle { cycle, .. } | Violation::DefinitionUnionCycle { cycle } => {
                terms.extend(cycle.iter().copied());
            }
            _ => {}
        }
    }
    terms
}

/// Convenience wrapper that runs validation against a default budget.
///
/// # Errors
///
/// Propagates [`QueryError`] from [`validate`].
pub fn validate_complete(graph: &BaseGraph) -> Result<ValidationReport, QueryError> {
    let control = sage_db_types::RunToCompletion;
    let mut work = Work::new(Budget::DEFAULT);
    work.check_entry(&control)?;
    validate(graph, &control, &mut work)
}

/// Reject a candidate whose profile forbids the shape.
///
/// # Errors
///
/// Returns [`QueryError::InvalidRequest`] naming the first violation, for callers
/// that only need a boolean decision.
pub fn require_valid(graph: &BaseGraph) -> Result<(), QueryError> {
    let report = validate_complete(graph)?;
    match report.violations.first() {
        None => Ok(()),
        Some(violation) => Err(QueryError::InvalidRequest {
            reason: match violation {
                Violation::UnknownEndpoint { .. } => "unknown endpoint",
                Violation::SelfRelation { .. } => "authored self-relation",
                Violation::LayerDirection { .. } => "layer direction violates the profile",
                Violation::MultipleInstantiationTargets { .. } => {
                    "multiple distinct instantiation targets"
                }
                Violation::LayerThreeWithoutTarget { .. } => "L3 term without instantiation",
                Violation::RelationCycle { .. } => "acyclic relation contains a cycle",
                Violation::DefinitionUnionCycle { .. } => {
                    "definition+dependency union contains a cycle"
                }
            },
        }),
    }
}

/// Report the budget limit a validation stop used, for callers that log it.
#[must_use]
pub const fn limit_of(error: &QueryError) -> Option<BudgetLimit> {
    match error {
        QueryError::BudgetExceeded(limit) => Some(*limit),
        _ => None,
    }
}
