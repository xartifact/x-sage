export { kb, LAYERS, conceptsDir, layerDir, graphFile, manifestFile, frontierFile, proposalsDir, checkpointsDir } from "./kb.ts";
export type { KbContext } from "./kb.ts";

export { loadConcepts, loadConceptsInLayer, findConceptFile, readConceptRaw } from "./concepts.ts";
export type { LoadedConcept, LoadConceptsOptions, LoadConceptsResult } from "./concepts.ts";

export { buildGraph, readStoredGraph, writeGraph, semanticGraph, queryEdges, validateGraphState } from "./graph.ts";
export type { EdgeHit, GraphValidationReport } from "./graph.ts";

export { activate, search, findPath, isActivationStrategy } from "./query.ts";
export type { Activation, ActivatedNeighbor, ActivateOptions, ActivationStrategy, PathResult } from "./query.ts";

export { buildVizPayload } from "./viz.ts";
export type { VizNode, VizEdge, VizPayload } from "./viz.ts";

export {
  createCheckpoint,
  getCheckpoint,
  saveCheckpoint,
  listFrontier,
  appendFrontier,
  listProposals,
  submitEdgeProposal,
  submitAtomProposal,
  submitBodyProposal,
  readBody,
  mergeProposals,
  renderManifest,
  writeManifest,
} from "./write.ts";
export type {
  ProposalLists,
  ProposeEdgeInput,
  ProposeAtomInput,
  ProposeBodyInput,
  MergeOutcome,
} from "./write.ts";

export { validateConcepts } from "./validate.ts";
export type { ConceptValidationReport } from "./validate.ts";

export { handleRequest, wantsText } from "./api.ts";
export type { ApiMethod, ApiRequest, ApiResponse } from "./api.ts";

export {
  renderActivation,
  renderCheckpoint,
  renderConceptAi,
  renderEdges,
  renderFrontier,
  renderGraphSection,
  renderJson,
  renderMergeOutcome,
  renderNeighborAtom,
  renderPath,
  renderProposals,
  renderSearchResults,
  renderValidationReport,
  renderVizStats,
} from "./render.ts";
