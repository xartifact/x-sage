// Types
export type { ConceptFrontmatter, Edge, EdgeType, Confidence, Source, ConceptFile } from "./types/index.ts";
export type { Graph, GraphNode, GraphEdge } from "./types/index.ts";
export type { Registry, KnowledgeBase } from "./types/index.ts";
export type { ManifestRow } from "./types/index.ts";
export type { Checkpoint } from "./types/index.ts";

// Schemas (for runtime validation)
export {
  ConceptFrontmatterSchema, EdgeSchema, EdgeTypeSchema,
  ConfidenceSchema, SourceSchema,
  GraphSchema, GraphNodeSchema, GraphEdgeSchema,
  RegistrySchema, KnowledgeBaseSchema,
  ManifestRowSchema,
  CheckpointSchema,
} from "./types/index.ts";
export { formatZodError } from "./types/index.ts";

// Filesystem
export { writeFileAtomic } from "./fs/atomic.ts";
export { withKbLock, LockTimeoutError } from "./fs/lock.ts";
export type { LockOptions } from "./fs/lock.ts";

// Manifest module
export {
  parseConceptFile,
  formatConceptFile,
  splitConceptDocument,
  joinConceptDocument,
  conceptRevision,
  normalizeBody,
  isBlankBody,
  MAX_BODY_CHARS,
} from "./manifest/index.ts";
export { generateManifest, formatManifest } from "./manifest/index.ts";

// Graph module
export { extractSubgraph, hopSubgraph } from "./graph/index.ts";
export { traverse, shortestPath } from "./graph/index.ts";
export { generateGraph, formatGraph } from "./graph/index.ts";
export { validateGraph } from "./graph/index.ts";
export type { GraphValidationIssue, GraphValidationResult } from "./graph/index.ts";
export { RELATION_SPECS, EDGE_TYPES, completeSymmetricEdges, edgeKey, layerViolation } from "./graph/index.ts";
export type { RelationSpec } from "./graph/index.ts";
export { transitiveClosure, findEdgeCycles } from "./graph/index.ts";
export type { EdgeCycle } from "./graph/index.ts";
export { targetsOf, sourcesOf, incidentEdges } from "./graph/index.ts";

// Manifest module (search)
export { searchManifest } from "./manifest/index.ts";

// Proposals module
export { proposeEdge, refineAtom } from "./proposals/index.ts";
export { listEdgeProposals, listAtomProposals } from "./proposals/index.ts";
export type { EdgeProposal, AtomProposal } from "./proposals/index.ts";
export { mergeEdgeProposal, mergeAtomProposal } from "./proposals/index.ts";

// Checkpoint module
export { readCheckpoint, writeCheckpoint } from "./checkpoint/index.ts";

// Frontier module
export { readFrontier, writeFrontier, addFrontierEntry } from "./frontier/index.ts";
export type { FrontierEntry } from "./frontier/index.ts";
