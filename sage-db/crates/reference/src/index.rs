//! Raw-record index.
//!
//! The reference builds its own order over the authored records instead of using
//! [`sage_db_types::BaseGraph`]'s adjacency accessors. That is a structural
//! requirement, not a style preference: an oracle that walked the same index as
//! the implementation under test could share its bugs. Only the raw slices
//! (`terms()`, `assertions()`) and single-record lookups (`term()`,
//! `assertion()`) are consumed here.
//!
//! Ordering: terms by id, assertions by `(relation, from, target, id)`. The second
//! order is the reference's own, so nothing in this crate depends on the graph's
//! internal `visit_key` ordering.

use sage_db_types::{
    AssertionRecord, BaseGraph, QueryControl, QueryError, RelationId, TermId, TermMeta,
};

use crate::work::{Work, reserve};

/// A private, budgeted copy of the raw graph records.
#[derive(Debug, Default)]
pub struct RawIndex {
    terms: Vec<TermMeta>,
    assertions: Vec<AssertionRecord>,
}

impl RawIndex {
    /// Copy and order the raw records, charging every scan.
    pub fn build(
        graph: &BaseGraph,
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<Self, QueryError> {
        let mut terms: Vec<TermMeta> = Vec::new();
        reserve(&mut terms, graph.term_count(), work)?;
        for term in graph.terms() {
            work.step(1)?;
            work.periodic_check(control)?;
            terms.push(*term);
        }
        terms.sort_unstable_by_key(|term| term.id);

        let mut assertions: Vec<AssertionRecord> = Vec::new();
        reserve(&mut assertions, graph.assertion_count(), work)?;
        for assertion in graph.assertions() {
            work.step(1)?;
            work.periodic_check(control)?;
            assertions.push(*assertion);
        }
        assertions.sort_unstable_by_key(|assertion| {
            (
                assertion.relation.get(),
                assertion.from.get(),
                assertion.target.get(),
                assertion.id.get(),
            )
        });

        Ok(Self { terms, assertions })
    }

    /// Number of terms.
    pub fn term_count(&self) -> usize {
        self.terms.len()
    }

    /// Whether `id` is a term of this graph.
    pub fn has_term(&self, id: TermId) -> bool {
        self.terms.binary_search_by_key(&id, |term| term.id).is_ok()
    }

    /// All terms, ordered by id.
    pub fn terms(&self) -> &[TermMeta] {
        &self.terms
    }

    /// All assertions, in the reference's own order.
    pub fn assertions(&self) -> &[AssertionRecord] {
        &self.assertions
    }

    /// Authored assertions with exactly these endpoints, in id order.
    ///
    /// Assertions are ordered by `(relation, from, target, id)`, so one partition
    /// gives the group. Several supports may share a triple; they keep their own
    /// identities here.
    pub fn authored(&self, from: TermId, relation: RelationId, to: TermId) -> &[AssertionRecord] {
        let key = (relation.get(), from.get(), to.get());
        let start = self.assertions.partition_point(|assertion| {
            (
                assertion.relation.get(),
                assertion.from.get(),
                assertion.target.get(),
            ) < key
        });
        let end = self.assertions.partition_point(|assertion| {
            (
                assertion.relation.get(),
                assertion.from.get(),
                assertion.target.get(),
            ) <= key
        });
        &self.assertions[start..end]
    }

    /// The strongest authored support for one direction, charged as it is scanned.
    ///
    /// Strongest is highest confidence, then lowest assertion id, so the choice is
    /// stable and independent of record order.
    ///
    /// The scan is **charged and interruptible**. Several supports may share one
    /// triple, so this loop can be arbitrarily long even though its caller charges a
    /// single outer step; leaving it free would let a snapshot with thousands of
    /// duplicate supports be scanned without a budget charge or a cancellation check.
    ///
    /// # Errors
    ///
    /// Propagates step exhaustion and control stops.
    pub fn strongest_authored(
        &self,
        from: TermId,
        relation: RelationId,
        to: TermId,
        work: &mut Work,
        control: &dyn QueryControl,
    ) -> Result<Option<AssertionRecord>, QueryError> {
        let mut best: Option<AssertionRecord> = None;
        for assertion in self.authored(from, relation, to) {
            work.step(1)?;
            work.periodic_check(control)?;
            let better = match &best {
                None => true,
                Some(current) => {
                    (assertion.confidence, core::cmp::Reverse(assertion.id.get()))
                        > (current.confidence, core::cmp::Reverse(current.id.get()))
                }
            };
            if better {
                best = Some(*assertion);
            }
        }
        Ok(best)
    }
}
