//! Records as the inference layer sees them.
//!
//! These are read-only projections of committed records. Body text, evidence and
//! review history stay behind blob references and are not part of the base graph
//! (`docs/design/data-model.md` §1).

use crate::confidence::Confidence;
use crate::identity::{AssertionId, RecordRevision, TermId};
use crate::layer::SemanticLayer;
use crate::relation::RelationId;

/// The minimal term projection inference needs: identity and semantic layer.
///
/// `layer` is the authored classification; `definition_depth` is never stored
/// here because it is derived per rule profile.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TermMeta {
    /// Term identity.
    pub id: TermId,
    /// Authored semantic layer, 0..=3.
    pub layer: SemanticLayer,
}

/// One authored assertion, with its own identity and revision.
///
/// Two assertions may share `(from, relation, target)`; they are separate
/// records with separate support and are never merged by aggregation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AssertionRecord {
    /// Assertion identity; unique per KB for all time.
    pub id: AssertionId,
    /// Revision of this assertion record; a withdrawn assertion keeps its
    /// history, so a revision identifies a specific evaluated state.
    pub revision: RecordRevision,
    /// Authored direction: subject.
    pub from: TermId,
    /// Relation template.
    pub relation: RelationId,
    /// Authored direction: object. Never rewritten, even for symmetric relations.
    pub target: TermId,
    /// Confidence declared by the author of this assertion.
    pub confidence: Confidence,
}

impl AssertionRecord {
    /// Total order used for adjacency and deterministic tie-breaking:
    /// `(from, relation_id, target, assertion_id)`.
    ///
    /// Both executors use this order so identical inputs produce identical
    /// visit sequences and identical proofs under the documented tie rules.
    #[must_use]
    pub fn visit_key(&self) -> (TermId, RelationId, TermId, AssertionId) {
        (self.from, self.relation, self.target, self.id)
    }
}
