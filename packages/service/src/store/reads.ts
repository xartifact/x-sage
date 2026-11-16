import type { Database } from "bun:sqlite";
import {
  buildKnowledgeDocument,
  graphFromRecords,
  manifestFromRecords,
  projectEvidence,
  projectionRevision,
  type Claim,
  type ConceptRecord,
  type Evidence,
} from "@x-sage/core/knowledge";
import type { Graph, ManifestRow } from "@x-sage/core";
import {
  conceptAuthorityRevision,
  evidenceFor,
  getConceptRow,
  listAcceptedClaims,
  listClaimRows,
  listConceptRows,
  listEvidenceRows,
  listRelationRows,
} from "./entities.ts";

/**
 * Read models over the structured authority.
 *
 * Every function here takes an open database, so a caller decides when a
 * connection is opened and closed. The graph and manifest are *derived* from
 * rows on each call rather than cached: they are cheap for this KB size, and a
 * cached copy would reintroduce exactly the staleness the file era suffered
 * from (`graph.json` drifting from the concept files).
 */

export function buildGraphFromStore(db: Database): Graph {
  return graphFromRecords(listConceptRows(db), listRelationRows(db));
}

export function manifestFromStore(db: Database): ManifestRow[] {
  return manifestFromRecords(listConceptRows(db));
}

export function conceptsFromStore(db: Database): ConceptRecord[] {
  return listConceptRows(db);
}

export function conceptFromStore(db: Database, conceptId: string): ConceptRecord | null {
  return getConceptRow(db, conceptId);
}

export function relationsFromStore(db: Database) {
  return listRelationRows(db);
}

export function claimsFromStore(db: Database, conceptId: string, status?: "accepted" | "superseded" | "rejected"): Claim[] {
  return listClaimRows(db, conceptId, status);
}

export function acceptedClaimsFromStore(db: Database): Claim[] {
  return listAcceptedClaims(db);
}

export function evidenceFromStore(db: Database): Evidence[] {
  return listEvidenceRows(db);
}

export interface ConceptDocumentView {
  conceptId: string;
  /** The Markdown projection an editor loads. */
  document: string;
  /** Hash the reviewer must return with a write-back. */
  revision: string;
  /**
   * Review baseline for concept-scoped proposals: the concept record plus its
   * accepted claims. Distinct from `revision`, which is the hash of the exact
   * Markdown bytes an editor loaded.
   */
  conceptRevision: string;
  claims: Claim[];
  evidence: Array<{ id: string; kind: string; title: string }>;
}

/**
 * Export one concept as a reviewable Markdown document.
 *
 * `revision` is the hash of the exact bytes returned. An editor submits that
 * value with its edit, so a change made in between is a reported conflict rather
 * than a silently discarded edit — the same baseline rule the legacy body
 * proposals used, now applied to the whole document.
 */
export function conceptDocument(db: Database, conceptId: string): ConceptDocumentView | null {
  const concept = getConceptRow(db, conceptId);
  if (concept === null) return null;

  const claims = listAcceptedClaims(db).filter((claim) => claim.conceptId === conceptId);
  const allEvidence = listEvidenceRows(db);
  const evidence = [
    ...evidenceFor(allEvidence, "concept", conceptId),
    ...claims.flatMap((claim) => evidenceFor(allEvidence, "claim", claim.id)),
  ];

  // Evidence may be referenced twice (concept-level and claim-level); the
  // projection lists each document once.
  const byId = new Map(evidence.map((item) => [item.id, item]));

  const document = buildKnowledgeDocument({
    frontmatter: {
      id: concept.id,
      atom: concept.atom,
      layer: concept.layer,
      domain: concept.domain,
      confidence: concept.confidence,
      source: concept.source,
      triggers: concept.triggers,
      edges: listRelationRows(db)
        .filter((relation) => relation.from === conceptId && !relation.target.includes("::"))
        .map((relation) => ({ type: relation.type, target: relation.target, confidence: relation.confidence })),
    },
    body: concept.body,
    claims: claims.map((claim) => ({
      id: claim.id,
      kind: claim.kind,
      confidence: claim.confidence,
      text: claim.text,
    })),
    evidence: projectEvidence([...byId.values()]),
  });

  return {
    conceptId,
    document,
    revision: projectionRevision(document),
    conceptRevision: conceptAuthorityRevision(db, concept),
    claims,
    evidence: [...byId.values()].map((item) => ({ id: item.id, kind: item.kind, title: item.title })),
  };
}
