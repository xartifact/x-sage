export { openStore, storeExists, knowledgeDbFile, assetRoot } from "./schema.ts";
export { storeStatus } from "./import.ts";
export type { StoreStatus } from "./import.ts";
export { importKnowledgeBase, ImportConflictError, ImportRefusedError } from "./import.ts";
export type { ImportReport, ImportFileOutcome } from "./import.ts";

export {
  buildGraphFromStore,
  manifestFromStore,
  conceptsFromStore,
  conceptFromStore,
  relationsFromStore,
  claimsFromStore,
  acceptedClaimsFromStore,
  evidenceFromStore,
  conceptDocument,
} from "./reads.ts";
export type { ConceptDocumentView } from "./reads.ts";

export {
  submitProposal,
  listProposals as listKnowledgeProposals,
  getProposal,
  reviewProposal,
  relationSetRevision,
  expectedRevisionFor,
  validateProposalPayload,
  recordAsset,
  uploadAsset,
  upsertVectors,
  requireSubjectText,
  evidenceForTarget,
  conceptClaims,
  assertGraphStillValid,
  NotFoundError,
  ConflictError,
  InvalidProposalError,
  ReviewStateError,
} from "./writes.ts";
export type {
  SubmitProposalInput,
  ReviewResult,
  EvidenceView,
  VectorUploadResult,
} from "./writes.ts";

export { hybridSearch, loadSubjects } from "./search.ts";
export type { SearchOptions } from "./search.ts";

export { storeAsset, readAsset, sniffImageMediaType, AssetTooLargeError, AssetTypeError } from "./assets.ts";
export { evidenceFor, listRelationRows, conceptAuthorityRevision } from "./entities.ts";
