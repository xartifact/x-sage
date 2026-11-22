//! Test-side graph builder and fixtures.
//!
//! Everything here is a *copy* of a small synthetic graph; no test opens a real KB
//! directory or writes a database.
//!
//! Each integration-test binary compiles this module separately, so a helper used by
//! only one of them looks dead to the others. That is a property of the test layout,
//! not unused code, hence the module-level allow.
#![allow(dead_code)]

use sage_db_types::{
    AssertionId, AssertionRecord, BaseGraph, Budget, Confidence, Digest, GraphDigest, KbUuid,
    KbVersion, RecordRevision, RelationId, RuleProfile, RuleSet, RulesDigest, SemanticLayer,
    SnapshotId, TermId, TermMeta,
};

pub(crate) const KB: KbUuid = KbUuid::from_bytes([0x11; 16]);

pub(crate) fn rules(profile: RuleProfile) -> RuleSet {
    RuleSet::from_trusted_parts(profile, RulesDigest::new(Digest::from_bytes([0x22; 32])))
}

pub(crate) fn snapshot(profile: RuleProfile) -> SnapshotId {
    SnapshotId {
        kb_uuid: KB,
        version: KbVersion::new(1),
        rules: rules(profile),
        graph_digest: GraphDigest::new(Digest::from_bytes([0x33; 32])),
    }
}

/// Incremental builder so a test reads as the graph it describes.
pub(crate) struct Builder {
    profile: RuleProfile,
    next_term: u32,
    terms: Vec<TermMeta>,
    assertions: Vec<AssertionRecord>,
    next_assertion: u128,
}

impl Builder {
    pub(crate) fn new(profile: RuleProfile) -> Self {
        Self {
            profile,
            next_term: 0,
            terms: Vec::new(),
            assertions: Vec::new(),
            next_assertion: 1,
        }
    }

    /// Add a term at `layer`, returning its id.
    pub(crate) fn term(&mut self, layer: SemanticLayer) -> TermId {
        let id = TermId::new(self.next_term);
        self.next_term += 1;
        self.terms.push(TermMeta { id, layer });
        id
    }

    /// Add `count` terms at one layer, returning their ids.
    pub(crate) fn terms_at(&mut self, layer: SemanticLayer, count: u32) -> Vec<TermId> {
        (0..count).map(|_| self.term(layer)).collect()
    }

    /// Pin an assertion's revision, for replay tests.
    pub(crate) fn edge_with_revision(
        &mut self,
        from: TermId,
        relation: RelationId,
        to: TermId,
        confidence: Confidence,
        revision: u64,
    ) -> AssertionId {
        let id = self.edge(from, relation, to, confidence);
        let last = self.assertions.last_mut().expect("edge just pushed");
        last.revision = RecordRevision::new(revision).expect("non-zero revision");
        id
    }

    /// Add an authored assertion with `confidence`.
    pub(crate) fn edge(
        &mut self,
        from: TermId,
        relation: RelationId,
        to: TermId,
        confidence: Confidence,
    ) -> AssertionId {
        let id = AssertionId::new(self.next_assertion);
        self.next_assertion += 1;
        self.assertions.push(AssertionRecord {
            id,
            revision: RecordRevision::FIRST,
            from,
            relation,
            target: to,
            confidence,
        });
        id
    }

    /// Convenience: a `confirmed` edge.
    pub(crate) fn edge_c(&mut self, from: TermId, relation: RelationId, to: TermId) -> AssertionId {
        self.edge(from, relation, to, Confidence::Confirmed)
    }

    pub(crate) fn build(self) -> BaseGraph {
        BaseGraph::new(
            snapshot(self.profile),
            self.terms,
            self.assertions,
            Budget::DEFAULT,
        )
        .expect("fixture graph must be constructible")
    }
}
