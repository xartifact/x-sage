//! Independent bounded path enumeration — the oracle's own cross-check.
//!
//! Saturation is a fixed point; this module is a completely different shape:
//! explicit depth-first enumeration of every simple path from the source, up to a
//! strict small-graph cap. Agreement between the two catches the failure mode the
//! task names — one reference routine that is wrong in both of its own
//! formulations.
//!
//! It shares only the raw records with saturation. It does not read the fact table,
//! the plan table or the arena, and it is never used to build a result: it exists
//! so tests can assert the two agree.
//!
//! # Bounds
//!
//! * `max_terms` caps the term table.
//! * `max_paths` caps enumerated simple paths.
//! * `max_depth` caps path length (a simple path can be at most `terms - 1` anyway).
//! * every edge expansion is charged as a step through [`Work`].
//!
//! Exceeding a cap is a typed error, never a truncated answer: a truncated
//! enumeration would look exactly like "no more paths exist".

use sage_db_types::{
    Annotation, AssertionRecord, Confidence, QueryControl, QueryError, RelationId, RuleSet, TermId,
};

use crate::work::Work;

/// Caps for one enumeration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caps {
    /// Maximum terms accepted in the graph.
    pub max_terms: usize,
    /// Maximum simple paths enumerated.
    pub max_paths: usize,
    /// Maximum hops in one path.
    pub max_depth: u32,
}

impl Caps {
    /// Defaults sized for the differential fixtures, not for a real KB.
    pub const DEFAULT: Self = Self {
        max_terms: 64,
        max_paths: 100_000,
        max_depth: 16,
    };
}

/// One enumerated path: the facts it composes, in order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Path {
    /// Facts of the path, `from → … → to`, supporting the *reported* relation.
    pub steps: Vec<PathStep>,
}

/// One hop of an enumerated path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PathStep {
    /// Subject of the hop as reported.
    pub from: TermId,
    /// Object of the hop as reported.
    pub to: TermId,
    /// Authored assertion licensing the hop.
    pub assertion: u64,
    /// Confidence the hop contributes.
    pub confidence: Confidence,
    /// Rule that licensed the hop, if it was not a plain authored fact of the
    /// reported relation.
    pub rule: Option<Rule>,
}

/// Which rule produced a hop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rule {
    /// R2: authored in the opposite direction.
    Symmetry,
    /// R3: an authored definition assertion.
    Definition,
}

impl Path {
    /// Annotation of the whole path: `min` over steps, `ABSENT` when empty.
    pub fn annotation(&self) -> Annotation {
        self.steps
            .iter()
            .fold(Annotation::present(Confidence::Confirmed), |acc, step| {
                acc.meet(Annotation::present(step.confidence))
            })
    }

    /// Whether any step is contested.
    pub fn disputed(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.confidence == Confidence::Contested)
    }

    /// Whether any step came from a rule rather than an authored fact of the
    /// reported relation.
    pub fn derived(&self) -> bool {
        self.steps.iter().any(|step| step.rule.is_some())
    }
}

/// What one enumeration found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Enumeration {
    /// Every simple path found, in deterministic order.
    pub paths: Vec<Path>,
}

impl Enumeration {
    /// Effective annotation: `max` over path annotations.
    pub fn annotation(&self) -> Annotation {
        self.paths
            .iter()
            .fold(Annotation::ABSENT, |acc, path| acc.join(path.annotation()))
    }

    /// Whether any path is contested.
    pub fn disputed(&self) -> bool {
        self.paths.iter().any(Path::disputed)
    }

    /// Whether some path of at least two hops, or of length one that used a rule,
    /// reaches the target. `R2(R2(a))` cannot arise because R2 is applied once.
    pub fn derived(&self) -> bool {
        self.paths.iter().any(Path::derived)
    }

    /// Whether a direct authored fact of the reported relation exists.
    pub fn asserted(&self) -> bool {
        self.paths
            .iter()
            .any(|path| path.steps.len() == 1 && path.steps[0].rule.is_none())
    }
}

/// The raw records one enumeration may read.
///
/// Borrowed, not copied: the enumerator is a test oracle and must not allocate a
/// second index. It only ever binary-searches and scans these slices.
#[derive(Clone, Copy, Debug)]
pub struct RawRecords<'a> {
    /// Every term, in any order.
    pub terms: &'a [sage_db_types::TermMeta],
    /// Every active assertion, in any order.
    pub assertions: &'a [AssertionRecord],
    /// Rules in force.
    pub rules: RuleSet,
}

/// Enumerate every simple path from `source` to each reachable term in `relation`.
///
/// Paths are built from one-hop edges only; a one-hop edge is an authored assertion
/// in the reported direction, or an R2 mirror, or an R3 seed. Longer paths are
/// ordinary compositions, so `min` composes correctly and no path revisits a term.
///
/// # Errors
///
/// - [`QueryError::InvalidRequest`] when the term table or path count would exceed
///   [`Caps`];
/// - budget and control failures.
pub fn enumerate(
    records: RawRecords<'_>,
    relation: RelationId,
    source: TermId,
    caps: Caps,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<Enumeration, QueryError> {
    let RawRecords {
        terms,
        assertions,
        rules,
    } = records;
    if terms.len() > caps.max_terms {
        return Err(QueryError::InvalidRequest {
            reason: "graph exceeds the small-graph cap for path enumeration",
        });
    }

    let mut edges: Vec<PathStep> = Vec::new();
    crate::work::reserve(&mut edges, assertions.len().saturating_mul(2), work)?;
    for assertion in assertions {
        work.step(1)?;
        work.periodic_check(control)?;
        if assertion.relation == relation {
            edges.push(PathStep {
                from: assertion.from,
                to: assertion.target,
                assertion: assertion.id.get() as u64,
                confidence: assertion.confidence,
                rule: None,
            });
        }
        let spec = rules.spec(assertion.relation);
        if spec.symmetric && assertion.relation == relation {
            edges.push(PathStep {
                from: assertion.target,
                to: assertion.from,
                assertion: assertion.id.get() as u64,
                confidence: assertion.confidence,
                rule: Some(Rule::Symmetry),
            });
        }
        if spec.implies_dependency && relation == RelationId::DependsOn {
            edges.push(PathStep {
                from: assertion.from,
                to: assertion.target,
                assertion: assertion.id.get() as u64,
                confidence: assertion.confidence,
                rule: Some(Rule::Definition),
            });
        }
    }
    edges.sort_unstable_by(|left, right| {
        (
            left.from.get(),
            left.to.get(),
            left.assertion,
            left.rule.map(rule_id),
        )
            .cmp(&(
                right.from.get(),
                right.to.get(),
                right.assertion,
                right.rule.map(rule_id),
            ))
    });

    let mut enumeration = Enumeration { paths: Vec::new() };
    let max_paths = caps.max_paths;
    let mut visited: Vec<TermId> = Vec::new();
    crate::work::reserve(&mut visited, caps.max_terms, work)?;
    let mut stack: Vec<PathStep> = Vec::new();
    crate::work::reserve(
        &mut stack,
        usize::try_from(caps.max_depth).unwrap_or(0) + 1,
        work,
    )?;

    // Only a transitive relation composes. For every other relation exactly one hop
    // is a fact; a longer walk would be navigation, which must not be reported as a
    // derived fact.
    let compose = rules.spec(relation).transitive;

    visited.push(source);
    walk(
        &edges,
        source,
        &mut visited,
        &mut stack,
        compose,
        &mut enumeration,
        caps.max_depth,
        max_paths,
        work,
        control,
    )?;
    Ok(enumeration)
}

#[allow(clippy::too_many_arguments)]
fn walk(
    edges: &[PathStep],
    current: TermId,
    visited: &mut Vec<TermId>,
    stack: &mut Vec<PathStep>,
    compose: bool,
    enumeration: &mut Enumeration,
    max_depth: u32,
    max_paths: usize,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<(), QueryError> {
    let start = edges.partition_point(|edge| edge.from.get() < current.get());
    let end = edges.partition_point(|edge| edge.from.get() <= current.get());
    for edge in &edges[start..end] {
        work.step(1)?;
        work.periodic_check(control)?;
        if visited.contains(&edge.to) {
            continue;
        }
        stack.push(*edge);
        if enumeration.paths.len() >= max_paths {
            return Err(QueryError::InvalidRequest {
                reason: "path enumeration exceeded its path cap",
            });
        }
        work.row()?;
        work.bytes(core::mem::size_of::<PathStep>() as u64)?;
        let mut steps: Vec<PathStep> = Vec::new();
        crate::work::reserve(&mut steps, stack.len(), work)?;
        steps.extend_from_slice(stack);
        enumeration.paths.push(Path { steps });

        // Only a transitive relation composes; a single hop is the whole fact
        // otherwise, and longer walks would be navigation, not derivation.
        if compose {
            if stack.len() as u32 >= max_depth {
                return Err(QueryError::InvalidRequest {
                    reason: "path enumeration exceeded its depth cap",
                });
            }
            visited.push(edge.to);
            walk(
                edges,
                edge.to,
                visited,
                stack,
                compose,
                enumeration,
                max_depth,
                max_paths,
                work,
                control,
            )?;
            visited.pop();
        }
        stack.pop();
    }
    Ok(())
}

const fn rule_id(rule: Rule) -> u8 {
    match rule {
        Rule::Symmetry => 1,
        Rule::Definition => 2,
    }
}
