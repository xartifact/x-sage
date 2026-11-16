import type { Database } from "bun:sqlite";
import {
  ClaimSchema,
  ConceptRecordSchema,
  RelationRecordSchema,
  conceptRecordRevision,
  claimRevision,
  type Claim,
  type ConceptRecord,
  type Evidence,
  type RelationRecord,
  type EvidenceTarget,
  EvidenceSchema,
} from "@x-sage/core/knowledge";

/**
 * Row ↔ record mapping.
 *
 * Kept in one place so the JSON columns (`domain_json`, `triggers_json`) are
 * parsed exactly once and no caller accidentally returns a raw row whose array
 * fields are strings. Every read goes through a zod schema: a database is an
 * untrusted input too — a hand-edited row must fail loudly rather than be
 * rendered into a concept that no longer satisfies the schema.
 */

interface ConceptRow {
  id: string;
  atom: string;
  layer: number;
  domain_json: string;
  confidence: string;
  source: string;
  triggers_json: string;
  body: string;
  source_path: string;
  revision: string;
  source_hash: string | null;
  created_at: string;
  updated_at: string;
}

interface RelationRow {
  from_id: string;
  type: string;
  target: string;
  confidence: string;
  created_at: string;
}

interface ClaimRow {
  id: string;
  concept_id: string;
  kind: string;
  status: string;
  text: string;
  confidence: string;
  revision: string;
  created_at: string;
  updated_at: string;
}

interface EvidenceRow {
  id: string;
  kind: string;
  title: string;
  locator: string | null;
  excerpt: string | null;
  asset_sha256: string | null;
  asset_media_type: string | null;
  asset_bytes: number | null;
  captured_at: string;
  captured_by: string;
  created_at: string;
}

function toConcept(row: ConceptRow): ConceptRecord {
  return ConceptRecordSchema.parse({
    id: row.id,
    atom: row.atom,
    layer: row.layer,
    domain: JSON.parse(row.domain_json) as unknown,
    confidence: row.confidence,
    source: row.source,
    triggers: JSON.parse(row.triggers_json) as unknown,
    body: row.body,
    sourcePath: row.source_path,
    revision: row.revision,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  });
}

function toRelation(row: RelationRow): RelationRecord {
  return RelationRecordSchema.parse({
    from: row.from_id,
    type: row.type,
    target: row.target,
    confidence: row.confidence,
    createdAt: row.created_at,
  });
}

function toClaim(row: ClaimRow): Claim {
  return ClaimSchema.parse({
    id: row.id,
    conceptId: row.concept_id,
    kind: row.kind,
    status: row.status,
    text: row.text,
    confidence: row.confidence,
    revision: Number(row.revision),
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  });
}

export function toEvidence(row: EvidenceRow, targets: EvidenceTarget[]): Evidence {
  return EvidenceSchema.parse({
    id: row.id,
    kind: row.kind,
    title: row.title,
    ...(row.locator === null ? {} : { locator: row.locator }),
    ...(row.excerpt === null ? {} : { excerpt: row.excerpt }),
    ...(row.asset_sha256 === null || row.asset_media_type === null || row.asset_bytes === null
      ? {}
      : {
          asset: {
            sha256: row.asset_sha256,
            mediaType: row.asset_media_type,
            byteSize: row.asset_bytes,
          },
        }),
    capturedAt: row.captured_at,
    capturedBy: row.captured_by,
    createdAt: row.created_at,
    targets,
  });
}

export function listConceptRows(db: Database): ConceptRecord[] {
  return db
    .query<ConceptRow, []>("SELECT * FROM concepts ORDER BY layer, id")
    .all()
    .map(toConcept);
}

export function getConceptRow(db: Database, id: string): ConceptRecord | null {
  const row = db.query<ConceptRow, [string]>("SELECT * FROM concepts WHERE id = ?").get(id);
  return row === null ? null : toConcept(row);
}

/**
 * Relations in file order: layer, then concept id, then declaration position.
 *
 * That is exactly the order `generateGraph` produces from the concept files
 * (layers in L0→L3 order, filenames sorted, edges as written), so activation
 * output survives the cutover byte for byte.
 */
export function listRelationRows(db: Database): RelationRecord[] {
  return db
    .query<RelationRow, []>(
      `SELECT r.* FROM relations r JOIN concepts c ON c.id = r.from_id
       ORDER BY c.layer, c.id, r.position, r.type, r.target`,
    )
    .all()
    .map(toRelation);
}

export function listRelationRowsFrom(db: Database, from: string): RelationRecord[] {
  return db
    .query<RelationRow, [string]>(
      "SELECT * FROM relations WHERE from_id = ? ORDER BY position, type, target",
    )
    .all(from)
    .map(toRelation);
}

/** Claims of one concept, optionally only a status. Accepted is the default for reasoning. */
export function listClaimRows(
  db: Database,
  conceptId: string,
  status?: "accepted" | "superseded" | "rejected",
): Claim[] {
  const rows =
    status === undefined
      ? db.query<ClaimRow, [string]>("SELECT * FROM claims WHERE concept_id = ? ORDER BY created_at, id").all(conceptId)
      : db
          .query<ClaimRow, [string, string]>(
            "SELECT * FROM claims WHERE concept_id = ? AND status = ? ORDER BY created_at, id",
          )
          .all(conceptId, status);
  return rows.map(toClaim);
}

export function getClaimRow(db: Database, id: string): Claim | null {
  const row = db.query<ClaimRow, [string]>("SELECT * FROM claims WHERE id = ?").get(id);
  return row === null ? null : toClaim(row);
}

export function listAcceptedClaims(db: Database): Claim[] {
  return db
    .query<ClaimRow, []>("SELECT * FROM claims WHERE status = 'accepted' ORDER BY concept_id, created_at, id")
    .all()
    .map(toClaim);
}

export function listEvidenceRows(db: Database): Evidence[] {
  const targets = db
    .query<{ evidence_id: string; target_type: string; target_id: string }, []>(
      "SELECT evidence_id, target_type, target_id FROM evidence_targets",
    )
    .all();
  const byEvidence = new Map<string, EvidenceTarget[]>();
  for (const target of targets) {
    const list = byEvidence.get(target.evidence_id) ?? [];
    list.push({ type: target.target_type as EvidenceTarget["type"], id: target.target_id });
    byEvidence.set(target.evidence_id, list);
  }

  return db
    .query<EvidenceRow, []>("SELECT * FROM evidence ORDER BY created_at DESC, id")
    .all()
    .map((row) => toEvidence(row, byEvidence.get(row.id) ?? []));
}

export function getEvidence(db: Database, id: string): Evidence | null {
  const row = db.query<EvidenceRow, [string]>("SELECT * FROM evidence WHERE id = ?").get(id);
  if (row === null) return null;
  const targets = db
    .query<{ target_type: string; target_id: string }, [string]>(
      "SELECT target_type, target_id FROM evidence_targets WHERE evidence_id = ?",
    )
    .all(id)
    .map((target) => ({ type: target.target_type as EvidenceTarget["type"], id: target.target_id }));
  return toEvidence(row, targets);
}

export function evidenceFor(
  evidence: readonly Evidence[],
  targetType: EvidenceTarget["type"],
  targetId: string,
): Evidence[] {
  return evidence.filter((item) => item.targets.some((target) => target.type === targetType && target.id === targetId));
}

/**
 * Current authority revision of a concept.
 *
 * Recomputed from the row rather than trusted from the stored column: if a row
 * was edited outside this code path, the revision must reflect what the record
 * says now, otherwise every review baseline would agree with a stale value.
 */
export function conceptRecordRevisionOf(concept: ConceptRecord): string {
  return currentConceptRevision(concept);
}

/**
 * Revision of a concept *as a review baseline*: its record plus its accepted
 * claims.
 *
 * The concept row alone is not enough. Adding a claim changes the document a
 * reviewer reads, so two proposals submitted against the same document must not
 * both apply — the second was written against a document that no longer exists.
 */
export function conceptAuthorityRevision(db: Database, concept: ConceptRecord): string {
  const claims = db
    .query<{ id: string; text: string; kind: string; confidence: string }, [string]>(
      "SELECT id, text, kind, confidence FROM claims WHERE concept_id = ? AND status = 'accepted' ORDER BY id",
    )
    .all(concept.id)
    .map((claim) => `${claim.id}\u0000${claim.kind}\u0000${claim.confidence}\u0000${claim.text}`);
  return claimRevision(
    currentConceptRevision(concept),
    "concept-baseline",
    claims.join("\u0001"),
  );
}

export function currentConceptRevision(concept: ConceptRecord): string {
  return conceptRecordRevision({
    id: concept.id,
    atom: concept.atom,
    layer: concept.layer,
    domain: concept.domain,
    confidence: concept.confidence,
    source: concept.source,
    triggers: concept.triggers,
    body: concept.body,
    sourcePath: concept.sourcePath,
  });
}

export function currentClaimRevision(claim: Claim): string {
  return claimRevision(claim.text, claim.kind, claim.confidence);
}

/**
 * Recompute and store a concept's authority revision.
 *
 * Called after every change that alters what a reviewer sees — the record, its
 * accepted claims, or the document projection. `concepts.revision` therefore
 * always equals `conceptAuthorityRevision`, which is what makes an import able
 * to tell "the file changed" from "the authority changed".
 */
export function refreshConceptRevision(db: Database, conceptId: string, timestamp: string): void {
  const concept = getConceptRow(db, conceptId);
  if (concept === null) throw new Error(`Concept not found: ${conceptId}`);
  db.run("UPDATE concepts SET revision = ?, updated_at = ? WHERE id = ?", [
    conceptAuthorityRevision(db, concept),
    timestamp,
    conceptId,
  ]);
}
