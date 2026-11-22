//! Test-side fixtures for the reference evaluator.
//!
//! Every graph here is built in memory from raw records. No test opens a real KB
//! directory, writes a database or mutates shared state.

// One copy of this module is compiled into every test binary, so items used by only
// some of them would otherwise be reported as dead code.
#![allow(dead_code, unreachable_pub)]

use sage_db_types::{
    AssertionId, AssertionRecord, BaseGraph, Budget, Confidence, Digest, GraphDigest, KbUuid,
    KbVersion, RecordRevision, RelationId, RuleProfile, RuleSet, RulesDigest, SemanticLayer,
    SnapshotId, SourceQuery, TermId, TermMeta,
};

use sage_db_reference::{Meter, ReferenceExecutor};
use sage_db_types::{QueryControl, RunToCompletion, SourceExecutor, SourceResult};

pub const KB: KbUuid = KbUuid::from_bytes([0x5a; 16]);

/// Rules for `profile`, with a digest that is *opaque* — no hash convention is
/// implemented in the reference crate.
pub fn rules(profile: RuleProfile) -> RuleSet {
    RuleSet::from_trusted_parts(profile, RulesDigest::new(Digest::from_bytes([0x77; 32])))
}

/// Snapshot identity for a fixture at `version`.
pub fn snapshot(profile: RuleProfile, version: u64) -> SnapshotId {
    SnapshotId {
        kb_uuid: KB,
        version: KbVersion::new(version),
        rules: rules(profile),
        graph_digest: GraphDigest::new(Digest::from_bytes([0x66; 32])),
    }
}

/// Incremental builder so a test reads as the graph it describes.
#[derive(Clone)]
pub struct Builder {
    profile: RuleProfile,
    version: u64,
    next_term: u32,
    next_assertion: u128,
    terms: Vec<TermMeta>,
    assertions: Vec<AssertionRecord>,
}

impl Builder {
    pub fn new(profile: RuleProfile) -> Self {
        Self {
            profile,
            version: 1,
            next_term: 0,
            next_assertion: 1,
            terms: Vec::new(),
            assertions: Vec::new(),
        }
    }

    pub fn version(mut self, version: u64) -> Self {
        self.version = version;
        self
    }

    /// Add a term at `layer`.
    pub fn term(&mut self, layer: SemanticLayer) -> TermId {
        let id = TermId::new(self.next_term);
        self.next_term += 1;
        self.terms.push(TermMeta { id, layer });
        id
    }

    /// Add `count` terms at `layer`.
    pub fn terms_at(&mut self, layer: SemanticLayer, count: u32) -> Vec<TermId> {
        (0..count).map(|_| self.term(layer)).collect()
    }

    /// Add an authored assertion.
    pub fn edge(
        &mut self,
        from: TermId,
        relation: RelationId,
        to: TermId,
        confidence: Confidence,
    ) -> AssertionId {
        self.edge_rev(from, relation, to, confidence, 1)
    }

    /// Add a `confirmed` authored assertion.
    pub fn edge_c(&mut self, from: TermId, relation: RelationId, to: TermId) -> AssertionId {
        self.edge(from, relation, to, Confidence::Confirmed)
    }

    /// Add an authored assertion with an explicit revision.
    pub fn edge_rev(
        &mut self,
        from: TermId,
        relation: RelationId,
        to: TermId,
        confidence: Confidence,
        revision: u64,
    ) -> AssertionId {
        let id = AssertionId::new(self.next_assertion);
        self.next_assertion += 1;
        self.assertions.push(AssertionRecord {
            id,
            revision: RecordRevision::new(revision).expect("revision is non-zero"),
            from,
            relation,
            target: to,
            confidence,
        });
        id
    }

    /// Add an authored assertion under an explicit identity, so a test can name the
    /// record it will later retract.
    pub fn edge_id(
        &mut self,
        id: AssertionId,
        from: TermId,
        relation: RelationId,
        to: TermId,
        confidence: Confidence,
    ) {
        self.assertions.push(AssertionRecord {
            id,
            revision: RecordRevision::FIRST,
            from,
            relation,
            target: to,
            confidence,
        });
    }

    /// Drop every assertion with `id`, as a retraction would.
    pub fn retract(&mut self, id: AssertionId) {
        self.assertions.retain(|assertion| assertion.id != id);
    }

    /// Drop every assertion with these endpoints and relation.
    pub fn retract_triple(&mut self, from: TermId, relation: RelationId, to: TermId) {
        self.assertions.retain(|assertion| {
            !(assertion.from == from && assertion.relation == relation && assertion.target == to)
        });
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        snapshot(self.profile, self.version)
    }

    pub fn ruleset(&self) -> RuleSet {
        rules(self.profile)
    }

    pub fn profile(&self) -> RuleProfile {
        self.profile
    }

    pub fn terms(&self) -> &[TermMeta] {
        &self.terms
    }

    pub fn assertions(&self) -> &[AssertionRecord] {
        &self.assertions
    }

    /// Reverse the assertion order, to prove results do not depend on input order.
    pub fn reverse_assertions(&mut self) {
        self.assertions.reverse();
    }

    pub fn build(&self) -> BaseGraph {
        BaseGraph::new(
            self.snapshot_id(),
            self.terms.clone(),
            self.assertions.clone(),
            Budget::DEFAULT,
        )
        .expect("fixture graph must be constructible")
    }
}

/// Query `source -relation-> *` against `graph`.
pub fn query(graph: &BaseGraph, source: TermId, relation: RelationId) -> SourceQuery {
    SourceQuery {
        snapshot: graph.snapshot(),
        source,
        relation,
        with_proof: false,
        budget: Budget::DEFAULT,
    }
}

/// Run the reference executor and require success.
pub fn solve(
    graph: &BaseGraph,
    source: TermId,
    relation: RelationId,
    with_proof: bool,
) -> SourceResult {
    let mut request = query(graph, source, relation);
    request.with_proof = with_proof;
    solve_with(graph, &request, &RunToCompletion).expect("reference evaluation must succeed")
}

/// Run the reference executor with an explicit request and control.
pub fn solve_with(
    graph: &BaseGraph,
    request: &SourceQuery,
    control: &dyn QueryControl,
) -> Result<SourceResult, sage_db_types::QueryError> {
    ReferenceExecutor::new().solve(graph, request, control)
}

/// A work meter for calling the internals directly.
pub fn work() -> Meter {
    Meter::new(Budget::DEFAULT)
}
