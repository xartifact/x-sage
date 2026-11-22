//! `sage-db-types` — the shared contract between the two executors.
//!
//! This crate is pure: no file, network or clock access, no environment reads.
//! It contains identity, records, rule templates, budgets, the base-graph input
//! contract, query/result shapes and the proof vocabulary. It contains **no**
//! inference: closure, widest-path evaluation, proof construction and replay
//! belong to `sage-db-infer` (optimized) and `sage-db-reference` (naive oracle),
//! which are independent of each other in production.
//!
//! Design authority: `docs/design/data-model.md`, `docs/design/axiomatics.md`,
//! `docs/design/inference.md`, `docs/design/query-dsl.md`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod budget;
pub mod confidence;
pub mod digest;
pub mod executor;
pub mod framing;
pub mod graph;
pub mod identity;
pub mod layer;
pub mod ledger;
pub mod persist;
pub mod prepared;
pub mod proof;
pub mod query;
pub mod record;
pub mod relation;

pub use budget::{Budget, BudgetLimit, WorkMeter};
pub use confidence::{Annotation, Confidence};
pub use digest::{Digest, EngineVersion, GraphDigest, RulesDigest};
pub use executor::SourceExecutor;
pub use framing::{
    BlobRefSink, BytesSink, CorruptReason, CountBlobRefs, DiscardBytes, FormatError, FormatOutcome,
    GenesisShapeRule, InternalReason, LimitKind, Limits, OperatorAction, Refusal,
    UnsupportedReason, classify, decode_current_bound,
};
pub use graph::{
    BaseGraph, BuildLimit, GraphBuildError, GraphExtent, IncomingAssertions, IncomingIter,
    SnapshotId,
};
pub use identity::{AssertionId, Counter, Exhausted, KbUuid, KbVersion, RecordRevision, TermId};
pub use layer::{DefinitionDepth, DepthStatus, DepthUnavailable, SemanticLayer};
pub use ledger::{BudgetError, BudgetUnit, SharedLedger};
pub use persist::{
    ActorOrigin, AssertionRecordFull, AssetRecord, ClaimKind, ClaimRecord, ClaimStatus,
    CommonFields, CountPair, CreationAudit, EvidenceKind, EvidenceLinkRecord, EvidenceRecord,
    Extensions, ExtensionsError, FrontierEntryRecord, FrontierStatus, GenesisMetadata,
    GenesisRecord, GenesisRules, IdAllocationEvent, IdAllocationRecord, IdSpace,
    ImportSessionRecord, ImportStage, KbMetadataRecord, LinkTargetKind, MediaType, ModelRecord,
    ModelStatus, NaturalKey, OptionalActorAudit, ProposalKind, ProposalRecord, ProposalStatus,
    RecordKey, RecordKind, RecordSet, RecordSetError, RecordSetRejection, RequestOutcomeEvent,
    RevPair, RuleConfigRecord, SessionCheckpointRecord, StateEntry, StateKind, TermRecord,
    TermSource, VectorRecord, VectorStatus, Watermark,
};
pub use prepared::{
    BlobDeclaration, BlobSource, OwnedEvent, PreparedTransaction, RequestCanonVersion, RulesBinding,
};
pub use proof::{ProofArena, ProofArenaError, ProofNode, ProofRef};
pub use query::{
    AssertionSupport, CONTROL_CHECK_INTERVAL_STEPS, CanonicalRow, ControlStop, ExecStats, FactRow,
    QueryControl, QueryError, ResultIdentity, RunToCompletion, SourceQuery, SourceResult,
};
pub use record::{AssertionRecord, TermMeta};
pub use relation::{LayerRule, RelationId, RelationSpec, RuleProfile, RuleSet};

pub mod snapshot;
