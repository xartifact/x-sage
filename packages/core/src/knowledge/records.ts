import { createHash } from "node:crypto";
import { z } from "zod";
import {
  ConfidenceSchema,
  ConceptIdSchema,
  EdgeSchema,
  SourceSchema,
  type Graph,
  type GraphEdge,
  type GraphNode,
  type ManifestRow,
} from "../types/index.ts";
import { SHA256Schema } from "./types.ts";

/**
 * The concept row.
 *
 * A mirror of the concept frontmatter without `edges`: relations are their own
 * table, so an edge has one home. Keeping a copy inside the concept record is
 * what made the Markdown era's `requires`/`enables` fields drift — two places
 * to write one fact.
 */
export const ConceptRecordSchema = z.object({
  id: ConceptIdSchema,
  atom: z.string().min(1).max(200),
  layer: z.union([z.literal(0), z.literal(1), z.literal(2), z.literal(3)]),
  domain: z.array(z.string()).min(1),
  confidence: ConfidenceSchema,
  source: SourceSchema,
  triggers: z.array(z.string()).min(1),
  body: z.string().min(1),
  /** Where the record was imported from, kept for provenance and export layout. */
  sourcePath: z.string().max(500),
  /** Hash of the authority content; changes whenever any of the above changes. */
  revision: SHA256Schema,
  createdAt: z.string(),
  updatedAt: z.string(),
});

export type ConceptRecord = z.infer<typeof ConceptRecordSchema>;

export const RelationRecordSchema = EdgeSchema.extend({
  from: ConceptIdSchema,
  /** The concept the author wrote the relation on; mirrors EdgeSchema.from. */
  createdAt: z.string(),
});

export type RelationRecord = z.infer<typeof RelationRecordSchema>;

/**
 * Build the asserted graph from structured rows.
 *
 * Pure: no disk, no database. The graph algorithms in `core/graph` consume this
 * output unchanged, so moving the authority from files to SQLite did not fork
 * the reasoning layer.
 */
export function graphFromRecords(
  concepts: readonly ConceptRecord[],
  relations: readonly RelationRecord[],
): Graph {
  const nodes: GraphNode[] = concepts.map((concept) => ({
    id: concept.id,
    atom: concept.atom,
    layer: concept.layer,
    domain: concept.domain,
    confidence: concept.confidence,
    filePath: concept.sourcePath,
  }));

  // Key order matches `generateGraph`'s object literal, so a response serialised
  // from either path is byte-identical (JSON.stringify preserves insertion order).
  const edges: GraphEdge[] = relations.map((relation) => ({
    type: relation.type,
    target: relation.target,
    confidence: relation.confidence,
    from: relation.from,
  }));

  return { nodes, edges };
}

/** Manifest rows are a projection of the same records; no separate table. */
export function manifestFromRecords(concepts: readonly ConceptRecord[]): ManifestRow[] {
  return [...concepts]
    .sort((a, b) => a.layer - b.layer || a.id.localeCompare(b.id))
    .map((concept) => ({
      id: concept.id,
      atom: concept.atom,
      layer: concept.layer,
      domain: concept.domain,
      confidence: concept.confidence,
      source: concept.source,
      triggers: concept.triggers,
    }));
}

/** Canonical revision of a concept's authority content, edges excluded. */
export function conceptRecordRevision(
  record: Omit<ConceptRecord, "revision" | "createdAt" | "updatedAt">,
): string {
  return createHash("sha256")
    .update(
      JSON.stringify({
        id: record.id,
        atom: record.atom,
        layer: record.layer,
        domain: record.domain,
        confidence: record.confidence,
        source: record.source,
        triggers: record.triggers,
        body: record.body,
      }),
    )
    .digest("hex");
}

/** Revision of one claim's authoritative text; a vector is bound to it. */
export function claimRevision(text: string, kind: string, confidence: string): string {
  return createHash("sha256").update(JSON.stringify({ text, kind, confidence })).digest("hex");
}

/**
 * The exact text an external agent must embed for each subject type.
 *
 * Published here, in one place, because the store validates against it: a
 * vector carries the hash of the text it was computed from, and a changed
 * subject makes that hash stop matching. If each agent decided for itself what
 * to embed, every vector would drift independently and staleness could not be
 * detected at all.
 */
export function embeddingTextForConcept(concept: Pick<ConceptRecord, "atom" | "body">): string {
  return `${concept.atom}\n\n${concept.body}`;
}

export function embeddingTextForClaim(claim: { text: string }): string {
  return claim.text;
}

export function embeddingTextForEvidence(evidence: { title: string; excerpt?: string | undefined }): string {
  return evidence.excerpt === undefined ? evidence.title : `${evidence.title}\n\n${evidence.excerpt}`;
}

/** Hash of an embedding text; equal only when the embedded text is equal. */
export function embeddingRevision(text: string): string {
  return createHash("sha256").update(text).digest("hex");
}
