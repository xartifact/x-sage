//! Seeding and repeated fixed-point saturation of R1 and R2.
//!
//! The oracle is deliberately the slowest correct thing that can be written: an
//! ordered fact table, re-scanned and re-composed until *no field of any entry
//! changes*, not until no new key appears. A key-set delta would miss a confidence
//! improvement, and a confidence-only delta would miss a later `disputed` or
//! `derived` arrival — both are explicit contract properties.
//!
//! Everything is scoped to the one relation being reported. Authoring, R2, R3 and
//! R1 all produce facts of that relation, so the oracle never closes a relation the
//! caller did not ask about.
//!
//! | rule | fires for | produced fact |
//! |---|---|---|
//! | authoring | every active assertion of the reported relation | authored direction, `asserted` |
//! | R2 | authored assertion of a symmetric relation | the reverse direction, `derived` |
//! | R3 | authored definition assertion when the reported relation is `depends-on` | `depends-on`, `derived` |
//! | R1 | two facts of a transitive relation joined at a midpoint | the composition, `derived` |
//!
//! R2 is applied to **authored** assertions only. Composing R2 with its own output
//! returns to the authored direction and is not a second source, so a round trip
//! must not be what sets `derived` there.
//!
//! Nothing here partitions by confidence, thresholds, hop counts or adjacency
//! position. Those belong to the optimized executor and must not leak into the
//! oracle.

use sage_db_types::{Annotation, Confidence, QueryControl, QueryError, RelationId, RuleSet};

use crate::facts::{self, FactKey, FactState, FactTable, SupportWitness};
use crate::index::RawIndex;
use crate::work::Work;

/// A rule contribution: present at `confidence`, `derived`, no authored witness.
fn derived_seed(confidence: Confidence) -> FactState {
    FactState {
        annotation: Annotation::present(confidence),
        asserted: false,
        derived: true,
        disputed: matches!(confidence, Confidence::Contested),
        witness: None,
    }
}

/// Seed the authored facts and both rule seeds, then saturate R1 to a fixed point.
///
/// # Errors
///
/// Propagates budget and control failures. A returned table is a complete
/// evaluation of `relation` from `source`'s graph; there is no partial table.
pub fn saturate(
    index: &RawIndex,
    rules: RuleSet,
    relation: RelationId,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<FactTable, QueryError> {
    let mut table = FactTable::new();
    let spec = rules.spec(relation);

    for assertion in index.assertions() {
        work.step(1)?;
        work.periodic_check(control)?;

        if assertion.relation == relation {
            let witness = SupportWitness {
                id: assertion.id,
                revision: assertion.revision,
                confidence: assertion.confidence,
            };
            table.merge(
                FactKey::new(assertion.from, relation, assertion.target),
                FactState::authored(assertion.confidence, witness),
                work,
            )?;

            // R2: the mirror is a separate, non-authored row.
            if spec.symmetric {
                table.merge(
                    FactKey::new(assertion.target, relation, assertion.from),
                    derived_seed(assertion.confidence),
                    work,
                )?;
            }
        }

        // R3: a definition assertion of *any* definition relation seeds
        // `depends-on` when the active profile implies dependency.
        if relation == RelationId::DependsOn && rules.implies_dependency(assertion.relation) {
            table.merge(
                FactKey::new(assertion.from, relation, assertion.target),
                derived_seed(assertion.confidence),
                work,
            )?;
        }
    }

    if spec.transitive {
        saturate_transitive(&mut table, relation, work, control)?;
    }
    Ok(table)
}

/// Apply R1 to `relation` until every entry's state stops changing.
///
/// The scan runs over a copy so the table can grow during a round; the copy is
/// charged and fallible. Convergence is on full state equality, which is what makes
/// a late `disputed`/`derived` arrival propagate downstream.
fn saturate_transitive(
    table: &mut FactTable,
    relation: RelationId,
    work: &mut Work,
    control: &dyn QueryControl,
) -> Result<(), QueryError> {
    loop {
        let snapshot = table.snapshot(work)?;
        let mut changed = false;

        for left_entry in &snapshot {
            work.step(1)?;
            work.periodic_check(control)?;
            let left = left_entry.state;
            for right_entry in facts::group_of(&snapshot, relation, left_entry.key.to) {
                // Charged and checked together: the inner pair loop is the longest
                // unbounded run in saturation, so it must not defer cancellation to
                // the outer loop.
                work.step(1)?;
                work.periodic_check(control)?;
                let right = right_entry.state;
                let candidate = FactState {
                    annotation: left.annotation.meet(right.annotation),
                    asserted: false,
                    // A composition is at least two hops: always a rule derivation.
                    derived: true,
                    // A contested support on either half makes the composed path
                    // contested, whatever the effective maximum turns out to be.
                    disputed: left.disputed || right.disputed,
                    witness: None,
                };
                if !candidate.annotation.is_present() {
                    continue;
                }
                let key = FactKey::new(left_entry.key.from, relation, right_entry.key.to);
                if table.merge(key, candidate, work)? {
                    changed = true;
                }
            }
        }

        if !changed {
            return Ok(());
        }
        work.periodic_check(control)?;
    }
}
