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

// Manifest module
export { parseConceptFile, formatConceptFile } from "./manifest/index.ts";
export { generateManifest, formatManifest } from "./manifest/index.ts";

// Graph module
export { extractSubgraph, hopSubgraph } from "./graph/index.ts";
export { traverse, shortestPath } from "./graph/index.ts";

// Checkpoint module
export { readCheckpoint, writeCheckpoint } from "./checkpoint/index.ts";

// Frontier module
export { readFrontier, writeFrontier, addFrontierEntry } from "./frontier/index.ts";
export type { FrontierEntry } from "./frontier/index.ts";
