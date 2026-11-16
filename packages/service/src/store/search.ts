import type { Database } from "bun:sqlite";
import {
  DEFAULT_SEARCH_LIMIT,
  MAX_QUERY_CHARS,
  MAX_SEARCH_LIMIT,
  bm25,
  cosine,
  embeddingRevision,
  embeddingTextForClaim,
  embeddingTextForConcept,
  embeddingTextForEvidence,
  hasCjk,
  reciprocalRankFusion,
  truncate,
  type AssertedRelation,
  type SearchEvidenceRef,
  type SearchHit,
  type SearchResponse,
  type SearchSignal,
  type VectorSubjectType,
  type Evidence,
  type RelationRecord,
} from "@x-sage/core/knowledge";
import {
  evidenceFor,
  listAcceptedClaims,
  listConceptRows,
  listEvidenceRows,
  listRelationRows,
} from "./entities.ts";

/**
 * Hybrid retrieval over the structured authority.
 *
 * Two axes, never merged into one unexplained number:
 *
 * - **lexical** (BM25 over the query terms) finds the words the author used;
 * - **vector** (cosine against externally supplied embeddings) finds related
 *   meaning the author did not spell out.
 *
 * They are fused by rank, and every hit carries `signals` describing which axes
 * matched and how, plus the *asserted* relations of its concept. Vector
 * associations are reported separately so no consumer can mistake a similarity
 * for an edge someone asserted.
 *
 * Stale vectors are excluded, not down-weighted: an embedding of text that has
 * since changed describes a document that no longer exists.
 */

interface VectorRow {
  subject_type: string;
  subject_id: string;
  model: string;
  dimensions: number;
  revision: string;
  values_blob: Uint8Array;
  norm: number;
}

interface Subject {
  key: string;
  kind: SearchHit["kind"];
  id: string;
  conceptId: string | null;
  label: string;
  text: string;
  revision: string;
  claims: string[];
}

/**
 * Load the embeddable subjects of a KB.
 *
 * Evidence is included because a review's decisive material is often a document
 * or a screenshot, and looking only at concepts makes it unreachable.
 */
export function loadSubjects(db: Database): Subject[] {
  const subjects: Subject[] = [];

  for (const concept of listConceptRows(db)) {
    subjects.push({
      key: `concept\u0000${concept.id}`,
      kind: "concept",
      id: concept.id,
      conceptId: concept.id,
      label: concept.atom,
      text: embeddingTextForConcept(concept),
      revision: embeddingRevision(embeddingTextForConcept(concept)),
      claims: [],
    });
  }

  for (const claim of listAcceptedClaims(db)) {
    subjects.push({
      key: `claim\u0000${claim.id}`,
      kind: "claim",
      id: claim.id,
      conceptId: claim.conceptId,
      label: truncate(claim.text, 120),
      text: embeddingTextForClaim(claim),
      revision: embeddingRevision(embeddingTextForClaim(claim)),
      claims: [claim.id],
    });
  }

  for (const item of listEvidenceRows(db)) {
    const text = embeddingTextForEvidence(item);
    subjects.push({
      key: `evidence\u0000${item.id}`,
      kind: "evidence",
      id: item.id,
      conceptId: null,
      label: item.title,
      text,
      revision: embeddingRevision(text),
      claims: [],
    });
  }

  return subjects;
}

export interface SearchOptions {
  query: string;
  limit?: number;
  /** Vector axis; both are required — a model without a vector query is a mistake. */
  vector?: { model: string; values: number[]; provider?: string };
  /** Restrict to one concept and its claims/evidence (architecture-review scoping). */
  conceptId?: string;
}

function parseBlob(blob: Uint8Array): Float32Array {
  // The blob is written from a Float32Array's buffer, so length is exact.
  if (blob.byteLength % 4 !== 0) throw new Error("Vector blob is not a Float32Array");
  const copy = new Uint8Array(blob.byteLength);
  copy.set(blob);
  return new Float32Array(copy.buffer);
}

const signalSort = (a: SearchSignal, b: SearchSignal): number => a.axis.localeCompare(b.axis);

/**
 * Run one bounded hybrid search.
 *
 * Bounded: the query is capped at MAX_QUERY_CHARS, the limit at MAX_SEARCH_LIMIT,
 * and the vector axis only considers rows whose model and dimension match the
 * query exactly — a 768-dim query never silently scores 1536-dim rows.
 */
export function hybridSearch(db: Database, options: SearchOptions): SearchResponse {
  const query = options.query.trim();
  if (query === "") throw new Error("Missing search query");
  if (query.length > MAX_QUERY_CHARS) {
    throw new Error(`Query is ${query.length} characters, limit is ${MAX_QUERY_CHARS}`);
  }
  const limit = Math.min(Math.max(options.limit ?? DEFAULT_SEARCH_LIMIT, 1), MAX_SEARCH_LIMIT);

  const allSubjects = loadSubjects(db);
  const subjects = options.conceptId === undefined
    ? allSubjects
    : allSubjects.filter((subject) => {
        if (subject.conceptId === options.conceptId) return true;
        // Evidence attached to the concept's claims belongs to the same scope.
        return subject.kind === "evidence" && evidenceScope(db, subject.id, options.conceptId!);
      });

  const notes: string[] = [];
  if (hasCjk(query)) notes.push("CJK query: indexed as character bigrams");
  if (options.vector !== undefined) {
    if (options.vector.values.length === 0) throw new Error("Vector query is empty");
  }

  const lexical = bm25(
    subjects.map((subject) => ({ id: subject.key, text: subject.text })),
    query,
  );

  const vectorState =
    options.vector === undefined
      ? null
      : { model: options.vector.model, dimensions: options.vector.values.length, excludedStale: 0, missing: 0, invalid: 0 };
  const vectorScores: Array<{ key: string; cosine: number; subjectType: VectorSubjectType }> = [];

  if (options.vector !== undefined && vectorState !== null) {
    const rows = db
      .query<VectorRow, [string, number]>("SELECT * FROM vectors WHERE model = ? AND dimensions = ?")
      .all(options.vector.model, options.vector.values.length);
    const byKey = new Map(rows.map((row) => [`${row.subject_type}\u0000${row.subject_id}`, row]));

    for (const subject of subjects) {
      const row = byKey.get(subject.key);
      if (row === undefined) {
        vectorState.missing += 1;
        continue;
      }
      if (row.revision !== subject.revision) {
        vectorState.excludedStale += 1;
        continue;
      }
      const values = Array.from(parseBlob(row.values_blob));
      // A row that predates the float32 guard (or was written by an older
      // client) can hold non-finite values. Skipping it keeps every other hit
      // answerable, where letting cosine() throw killed the whole query.
      if (!values.every(Number.isFinite)) {
        vectorState.invalid += 1;
        continue;
      }
      vectorScores.push({
        key: subject.key,
        cosine: cosine(options.vector.values, values),
        subjectType: subject.kind,
      });
    }

    if (vectorState.missing > 0) notes.push(`${vectorState.missing} subject(s) have no vector for ${options.vector.model}`);
    if (vectorState.excludedStale > 0) {
      notes.push(`${vectorState.excludedStale} vector(s) excluded as stale (text changed since embedding)`);
    }
    if (vectorState.invalid > 0) {
      notes.push(`${vectorState.invalid} vector(s) skipped as invalid (non-finite values); re-upload them`);
    }
  }

  const ranks = reciprocalRankFusion([
    { name: "lexical", entries: lexical.map((entry) => ({ id: entry.id, score: entry.score })) },
    ...(vectorScores.length === 0
      ? []
      : [{ name: "vector", entries: vectorScores.map((entry) => ({ id: entry.key, score: entry.cosine })) }]),
  ]);

  const asserted = listRelationRows(db);
  const evidence = listEvidenceRows(db);
  const subjectByKey = new Map(subjects.map((subject) => [subject.key, subject]));

  const results: SearchHit[] = [];
  for (const fused of ranks) {
    if (results.length >= limit) break;
    const subject = subjectByKey.get(fused.id);
    if (subject === undefined) continue;

    const signals: SearchSignal[] = [];
    const lexicalEntry = lexical.find((entry) => entry.id === fused.id);
    if (lexicalEntry !== undefined) {
      signals.push({ axis: "lexical", score: lexicalEntry.score, terms: lexicalEntry.matched });
    }
    const vectorEntry = vectorScores.find((entry) => entry.key === fused.id);
    if (vectorEntry !== undefined && options.vector !== undefined) {
      signals.push({
        axis: "vector",
        score: vectorEntry.cosine,
        model: options.vector.model,
        dimensions: options.vector.values.length,
        cosine: vectorEntry.cosine,
      });
    }
    signals.sort(signalSort);

    results.push(buildHit(db, subject, fused.score, signals, asserted, evidence, vectorScores));
  }

  return {
    query,
    mode: options.vector === undefined ? "lexical" : "hybrid",
    vector: vectorState,
    results,
    limit,
    notes,
  };
}

function evidenceScope(db: Database, evidenceId: string, conceptId: string): boolean {
  const item = listEvidenceRows(db).find((candidate) => candidate.id === evidenceId);
  if (item === undefined) return false;
  const claimIds = new Set(listAcceptedClaims(db).filter((claim) => claim.conceptId === conceptId).map((claim) => claim.id));
  return item.targets.some(
    (target) =>
      (target.type === "concept" && target.id === conceptId) || (target.type === "claim" && claimIds.has(target.id)),
  );
}

function buildHit(
  db: Database,
  subject: Subject,
  score: number,
  signals: SearchSignal[],
  asserted: readonly RelationRecord[],
  evidence: readonly Evidence[],
  vectorScores: readonly { key: string; cosine: number; subjectType: VectorSubjectType }[],
): SearchHit {
  const conceptId = subject.conceptId;
  const assertedRelations: AssertedRelation[] = [];
  const vectorAssociations: SearchHit["vectorAssociations"] = [];

  if (conceptId !== null) {
    for (const relation of asserted) {
      if (relation.from === conceptId) {
        assertedRelations.push({
          direction: "out",
          type: relation.type,
          target: relation.target,
          confidence: relation.confidence,
        });
      }
      if (relation.target === conceptId) {
        assertedRelations.push({
          direction: "in",
          type: relation.type,
          target: relation.from,
          confidence: relation.confidence,
        });
      }
    }
  }

  // Associations are listed by subject, never folded into `assertedRelations`.
  const associationRows = db
    .query<{ subject_type: string; subject_id: string; model: string }, [string, string]>(
      `SELECT subject_type, subject_id, model FROM vectors
       WHERE (subject_type = 'concept' AND subject_id = ?) OR (subject_type = 'claim' AND subject_id IN (SELECT id FROM claims WHERE concept_id = ?))`,
    )
    .all(subject.id, subject.id);
  for (const row of associationRows) {
    // The association is the existence of an embedding, not a similarity to
    // this query; a fabricated 0 here would read as "measured, unrelated".
    const scored = vectorScores.find(
      (entry) => entry.key === `${row.subject_type}\u0000${row.subject_id}`,
    );
    vectorAssociations.push({
      subjectType: row.subject_type as VectorSubjectType,
      subjectId: row.subject_id,
      model: row.model,
      cosine: scored?.cosine ?? null,
    });
  }

  const claimTexts =
    subject.kind === "claim"
      ? [truncate(subject.text, 400)]
      : listAcceptedClaims(db)
          .filter((claim) => claim.conceptId === conceptId)
          .map((claim) => truncate(claim.text, 240));

  const evidenceRefs: SearchEvidenceRef[] =
    subject.kind === "evidence"
      ? [{ id: subject.id, kind: evidence.find((item) => item.id === subject.id)?.kind ?? "text", title: subject.label }]
      : evidenceFor(evidence, conceptId === null ? "claim" : "concept", conceptId ?? subject.id).map((item) => ({
          id: item.id,
          kind: item.kind,
          title: item.title,
        }));

  return {
    kind: subject.kind,
    id: subject.id,
    conceptId,
    label: subject.label,
    score,
    signals,
    assertedRelations,
    vectorAssociations,
    claims: claimTexts,
    evidence: evidenceRefs,
  };
}
