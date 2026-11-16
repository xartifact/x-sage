import type { Database } from "bun:sqlite";
import {
  AttachEvidenceSchema,
  buildKnowledgeDocument,
  projectEvidence,
  projectionRevision,
  ClaimIdSchema,
  CreateClaimSchema,
  EditClaimSchema,
  EvidenceSchema,
  ProposalPayloadSchema,
  SHA256Schema,
  AssertRelationSchema,
  UploadVectorsSchema,
  claimRevision,
  embeddingRevision,
  embeddingTextForClaim,
  embeddingTextForConcept,
  embeddingTextForEvidence,
  graphFromRecords,
  parseKnowledgeDocument,
  type Claim,
  type ConceptRecord,
  type Evidence,
  type EvidenceAsset,
  type EvidenceTarget,
  type KnowledgeProposal,
  type ProposalKind,
  type VectorRecord,
} from "@x-sage/core/knowledge";
import { validateGraph } from "@x-sage/core";
import { newId, storeAsset } from "./assets.ts";
import { transaction, writeMeta } from "./schema.ts";
import {
  conceptAuthorityRevision,
  currentClaimRevision,
  refreshConceptRevision,
  getClaimRow,
  getConceptRow,
  evidenceFor,
  getEvidence,
  listAcceptedClaims,
  listClaimRows,
  listConceptRows,
  listEvidenceRows,
  listRelationRows,
  listRelationRowsFrom,
} from "./entities.ts";


/**
 * Authority writes.
 *
 * Every mutation of authoritative knowledge is a proposal plus a review. There
 * is no "direct edit" endpoint: the review gate is the only path, so an external
 * agent's candidate can never be mistaken for accepted knowledge, and a write
 * can always be attributed and explained afterwards.
 */

export class NotFoundError extends Error {}
export class ConflictError extends Error {
  constructor(
    message: string,
    readonly expected: string,
    readonly actual: string,
  ) {
    super(message);
  }
}
export class InvalidProposalError extends Error {}
export class ReviewStateError extends Error {}

const now = (): string => new Date().toISOString();

/** Largest magnitude representable in float32; above this the value becomes ±Infinity. */
const FLOAT32_MAX = 3.4028234663852886e38;

/** Hash of the exported document for one concept. */
function projectionRevisionOf(db: Database, conceptId: string): string {
  const concept = requireConcept(db, conceptId);
  const claims = listAcceptedClaims(db).filter((claim) => claim.conceptId === conceptId);
  const all = listEvidenceRows(db);
  const evidence = [
    ...evidenceFor(all, "concept", conceptId),
    ...claims.flatMap((claim) => evidenceFor(all, "claim", claim.id)),
  ];
  const byId = new Map(evidence.map((item) => [item.id, item]));
  return projectionRevision(
    buildKnowledgeDocument({
      frontmatter: {
        id: concept.id,
        atom: concept.atom,
        layer: concept.layer,
        domain: concept.domain,
        confidence: concept.confidence,
        source: concept.source,
        triggers: concept.triggers,
        edges: listRelationRowsFrom(db, conceptId)
          .filter((relation) => !relation.target.includes("::"))
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
    }),
  );
}

function requireConcept(db: Database, id: string): ConceptRecord {
  const concept = getConceptRow(db, id);
  if (concept === null) throw new NotFoundError(`Concept not found: ${id}`);
  return concept;
}

function targetExists(db: Database, target: EvidenceTarget): boolean {
  if (target.id.includes("::")) return false;
  if (target.type === "concept") return getConceptRow(db, target.id) !== null;
  if (target.type === "claim") return getClaimRow(db, target.id) !== null;
  const [from, type, rest] = target.id.split("|");
  if (from === undefined || type === undefined || rest === undefined) return false;
  return listRelationRowsFrom(db, from).some((relation) => relation.type === type && relation.target === rest);
}

/**
 * Revision of the asserted relation set of one concept.
 *
 * A relation proposal quotes this instead of the concept revision: edges are
 * stored outside the concept row, so a body edit must not invalidate an edge
 * proposal — and two relation proposals against the same base must.
 */
export function relationSetRevision(db: Database, conceptId: string): string {
  const edges = listRelationRowsFrom(db, conceptId)
    .map((relation) => `${relation.type}\u0000${relation.target}\u0000${relation.confidence}`)
    .sort();
  return embeddingRevision(edges.join("\u0001"));
}

/** Revision an author must quote for a given proposal kind. */
export function expectedRevisionFor(
  db: Database,
  kind: ProposalKind,
  subject: string,
  payload: Record<string, unknown>,
): string {
  if (kind === "concept-markdown") {
    // The payload IS a document, so its baseline is the exported byte hash —
    // the same string `GET …/document` hands the editor. Using the concept
    // record revision here made the export and the submission disagree.
    return projectionRevisionOf(db, subject);
  }
  if (kind === "claim-create") {
    // Payload/subject disagreement is rejected in `validateProposalPayload`.
    return conceptAuthorityRevision(db, requireConcept(db, subject));
  }
  if (kind === "claim-edit") {
    const claim = getClaimRow(db, subject);
    if (claim === null) throw new NotFoundError(`Claim not found: ${subject}`);
    return currentClaimRevision(claim);
  }
  if (kind === "relation-assert") {
    return relationSetRevision(db, subject);
  }
  if (kind === "evidence-attach") {
    return conceptAuthorityRevision(db, requireConcept(db, subject));
  }
  return conceptAuthorityRevision(db, requireConcept(db, subject));
}

export interface SubmitProposalInput {
  kind: ProposalKind;
  subject: string;
  expectedRevision: string;
  reason: string;
  author: string;
  payload: Record<string, unknown>;
}

/** Validate a payload against its kind. Throws before anything is stored. */
export function validateProposalPayload(
  kind: ProposalKind,
  payload: Record<string, unknown>,
  /** The proposal subject; a payload that contradicts it is refused here. */
  subject?: string,
): Record<string, unknown> {
  switch (kind) {
    case "claim-create": {
      // `subject` is the concept for every kind. A payload that repeats it must
      // agree: the baseline is checked against the subject, so a mismatch would
      // validate one concept and write the claim to another.
      const declared = payload.conceptId;
      if (typeof declared === "string" && subject !== undefined && declared !== subject) {
        throw new InvalidProposalError(
          `claim-create subject ${subject} disagrees with payload.conceptId ${declared}; use one concept`,
        );
      }
      return CreateClaimSchema.partial({ conceptId: true }).parse(payload);
    }
    case "claim-edit":
      return EditClaimSchema.parse(payload);
    case "evidence-attach":
      return AttachEvidenceSchema.parse(payload);
    case "relation-assert": {
      const relation = AssertRelationSchema.parse(payload);
      // Refused at submission, not only at review: one database per KB means
      // there is nothing to resolve `otherkb::concept` against, and an author
      // should learn that before a reviewer spends time on it.
      if (relation.target.includes("::")) {
        throw new InvalidProposalError(
          `Cross-KB relation target ${relation.target} is not supported: each knowledge base has its own database and cannot resolve another's rows`,
        );
      }
      return relation;
    }
    case "concept-markdown": {
      if (typeof payload.document !== "string" || payload.document.trim() === "") {
        throw new InvalidProposalError("concept-markdown needs payload.document");
      }
      if (payload.document.length > 512 * 1024) {
        throw new InvalidProposalError("Document exceeds 512 KiB");
      }
      return { document: payload.document };
    }
    default:
      throw new InvalidProposalError(`Unknown proposal kind: ${String(kind)}`);
  }
}

export function submitProposal(db: Database, input: SubmitProposalInput): KnowledgeProposal {
  const parsed = ProposalPayloadSchema.parse(input);
  const payload = validateProposalPayload(parsed.kind, parsed.payload, parsed.subject);

  const actual = expectedRevisionFor(db, parsed.kind, parsed.subject, payload);
  if (actual !== parsed.expectedRevision) {
    throw new ConflictError(
      `${parsed.kind} proposal for ${parsed.subject} was written against revision ${parsed.expectedRevision.slice(0, 12)}, current is ${actual.slice(0, 12)}. Re-read and resubmit.`,
      parsed.expectedRevision,
      actual,
    );
  }

  if (parsed.kind === "evidence-attach") {
    // Report a dangling target to the author now rather than at review: the
    // reviewer cannot fix an id the author mistyped.
    const targets = AttachEvidenceSchema.parse(payload).targets;
    for (const target of targets) {
      if (target.id.includes("::")) {
        throw new InvalidProposalError(
          `Cross-KB evidence target ${target.id} is not supported: each knowledge base has its own database and cannot resolve another's rows`,
        );
      }
      if (!targetExists(db, target)) {
        throw new NotFoundError(`Evidence target does not exist: ${target.type} ${target.id}`);
      }
    }
  }

  const id = newId("prp");
  const createdAt = now();
  db.run(
    `INSERT INTO proposals (id, kind, subject, expected_revision, payload_json, reason, author, status, created_at)
     VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?)`,
    [id, parsed.kind, parsed.subject, parsed.expectedRevision, JSON.stringify(payload), parsed.reason, parsed.author, createdAt],
  );

  return {
    id,
    kind: parsed.kind,
    subject: parsed.subject,
    expectedRevision: parsed.expectedRevision,
    payload,
    reason: parsed.reason,
    author: parsed.author,
    status: "pending",
    createdAt,
    reviewedAt: null,
    reviewer: null,
    reviewNote: null,
  };
}

export function listProposals(db: Database, status?: KnowledgeProposal["status"]): KnowledgeProposal[] {
  const rows =
    status === undefined
      ? db.query<ProposalRow, []>("SELECT * FROM proposals ORDER BY created_at DESC, id").all()
      : db
          .query<ProposalRow, [string]>("SELECT * FROM proposals WHERE status = ? ORDER BY created_at DESC, id")
          .all(status);
  return rows.map(proposalFromRow);
}

interface ProposalRow {
  id: string;
  kind: string;
  subject: string;
  expected_revision: string;
  payload_json: string;
  reason: string;
  author: string;
  status: string;
  created_at: string;
  reviewed_at: string | null;
  reviewer: string | null;
  review_note: string | null;
}

function proposalFromRow(row: ProposalRow): KnowledgeProposal {
  return {
    id: row.id,
    kind: row.kind as ProposalKind,
    subject: row.subject,
    expectedRevision: row.expected_revision,
    payload: JSON.parse(row.payload_json) as Record<string, unknown>,
    reason: row.reason,
    author: row.author,
    status: row.status as KnowledgeProposal["status"],
    createdAt: row.created_at,
    reviewedAt: row.reviewed_at,
    reviewer: row.reviewer,
    reviewNote: row.review_note,
  };
}

export function getProposal(db: Database, id: string): KnowledgeProposal | null {
  const row = db.query<ProposalRow, [string]>("SELECT * FROM proposals WHERE id = ?").get(id);
  return row === null ? null : proposalFromRow(row);
}

export interface ReviewResult {
  proposal: KnowledgeProposal;
  /** Authority ids created or changed by an approval. */
  applied: string[];
}

/**
 * Approve or reject one proposal.
 *
 * Approval is a single transaction: the revision is re-checked, the change is
 * applied, and only then is the proposal marked approved. A crash anywhere in
 * between leaves the proposal pending, which is recoverable; the reverse order
 * would lose the change while reporting success.
 */
export function reviewProposal(
  db: Database,
  id: string,
  decision: "approve" | "reject",
  reviewer: string,
  note?: string,
): ReviewResult {
  const outcome = transaction(db, (): ReviewResult | ReviewConflict => {
    const proposal = getProposal(db, id);
    if (proposal === null) throw new NotFoundError(`Proposal not found: ${id}`);
    if (proposal.status === "approved" || proposal.status === "rejected") {
      throw new ReviewStateError(`Proposal ${id} is already ${proposal.status}`);
    }
    if (decision === "reject") {
      const reviewedAt = now();
      db.run(
        "UPDATE proposals SET status = 'rejected', reviewed_at = ?, reviewer = ?, review_note = ? WHERE id = ?",
        [reviewedAt, reviewer, note ?? null, id],
      );
      return {
        proposal: { ...proposal, status: "rejected", reviewedAt, reviewer, reviewNote: note ?? null },
        applied: [],
      };
    }

    if (proposal.status === "conflicted") {
      throw new ReviewStateError(
        `Proposal ${id} is conflicted and must be resubmitted against the current revision`,
      );
    }

    const actual = expectedRevisionFor(db, proposal.kind, proposal.subject, proposal.payload);
    if (actual !== proposal.expectedRevision) {
      // Reported, not thrown: throwing here would roll back the status write and
      // leave the proposal looking pending, so the next review would redo the
      // same check with no record of the first attempt.
      return { conflict: { proposal, actual } };
    }

    const applied = applyProposal(db, proposal);
    const reviewedAt = now();
    db.run(
      "UPDATE proposals SET status = 'approved', reviewed_at = ?, reviewer = ?, review_note = ? WHERE id = ?",
      [reviewedAt, reviewer, note ?? null, id],
    );
    writeMeta(db, "last_review_at", reviewedAt);
    return { proposal: { ...proposal, status: "approved", reviewedAt, reviewer, reviewNote: note ?? null }, applied };
  });

  if ("conflict" in outcome) {
    const { proposal, actual } = outcome.conflict;
    const reviewedAt = now();
    db.run(
      "UPDATE proposals SET status = 'conflicted', reviewed_at = ?, reviewer = ?, review_note = ? WHERE id = ?",
      [reviewedAt, reviewer, `Revision moved: expected ${proposal.expectedRevision}, current ${actual}`, id],
    );
    throw new ConflictError(
      `Proposal ${id} conflicts with the current authority: expected ${proposal.expectedRevision.slice(0, 12)}, current ${actual.slice(0, 12)}`,
      proposal.expectedRevision,
      actual,
    );
  }
  return outcome;
}

interface ReviewConflict {
  conflict: { proposal: KnowledgeProposal; actual: string };
}

function applyProposal(db: Database, proposal: KnowledgeProposal): string[] {
  switch (proposal.kind) {
    case "claim-create":
      return [applyClaimCreate(db, proposal)];
    case "claim-edit":
      return [applyClaimEdit(db, proposal)];
    case "evidence-attach":
      return [applyEvidenceAttach(db, proposal)];
    case "relation-assert":
      return [applyRelationAssert(db, proposal)];
    case "concept-markdown":
      return applyMarkdownWriteback(db, proposal);
    default:
      throw new InvalidProposalError(`Unknown proposal kind: ${String(proposal.kind)}`);
  }
}

function applyClaimCreate(db: Database, proposal: KnowledgeProposal): string {
  const payload = CreateClaimSchema.omit({ conceptId: true }).parse(proposal.payload);
  const conceptId = proposal.subject;
  requireConcept(db, conceptId);

  const id = newId("clm");
  const timestamp = now();
  db.run(
    `INSERT INTO claims (id, concept_id, kind, status, text, confidence, revision, created_at, updated_at)
     VALUES (?, ?, ?, 'accepted', ?, ?, '1', ?, ?)`,
    [id, conceptId, payload.kind, payload.text, payload.confidence, timestamp, timestamp],
  );
  // The concept document now contains a claim block, so its projection changed.
  touchConcept(db, conceptId, timestamp);
  return id;
}

function applyClaimEdit(db: Database, proposal: KnowledgeProposal): string {
  const claim = getClaimRow(db, proposal.subject);
  if (claim === null) throw new NotFoundError(`Claim not found: ${proposal.subject}`);
  const payload = EditClaimSchema.parse(proposal.payload);
  const text = payload.text ?? claim.text;
  const kind = payload.kind ?? claim.kind;
  const confidence = payload.confidence ?? claim.confidence;
  if (text.trim() === "") throw new InvalidProposalError("Claim text cannot be empty");

  db.run(
    "UPDATE claims SET text = ?, kind = ?, confidence = ?, revision = ?, updated_at = ? WHERE id = ?",
    [text, kind, confidence, String(claim.revision + 1), now(), claim.id],
  );
  touchConcept(db, claim.conceptId, now());
  return claim.id;
}

function applyRelationAssert(db: Database, proposal: KnowledgeProposal): string {
  const payload = AssertRelationSchema.parse(proposal.payload);
  requireConcept(db, payload.from);
  // A dangling target is refused here rather than written: validation would
  // report it later, but a relation nobody can resolve is not knowledge.
  if (payload.target.includes("::")) {
    throw new InvalidProposalError(
      `Cross-KB relation target ${payload.target} is not supported: each knowledge base has its own database and cannot resolve another's rows`,
    );
  }
  requireConcept(db, payload.target);

  // Append at the end of the concept's declared edges; editing a relation must
  // not reorder the ones an author already had.
  const nextPosition =
    db
      .query<{ next: number }, [string]>(
        "SELECT COALESCE(MAX(position) + 1, 0) AS next FROM relations WHERE from_id = ?",
      )
      .get(payload.from)?.next ?? 0;
  db.run(
    "INSERT OR REPLACE INTO relations (from_id, type, target, confidence, position, created_at) VALUES (?, ?, ?, ?, ?, ?)",
    [payload.from, payload.type, payload.target, payload.confidence, nextPosition, now()],
  );
  assertGraphStillValid(db);
  return `${payload.from}|${payload.type}|${payload.target}`;
}

function applyEvidenceAttach(db: Database, proposal: KnowledgeProposal): string {
  const payload = AttachEvidenceSchema.parse(proposal.payload);
  for (const target of payload.targets) {
    if (target.id.includes("::")) {
      throw new InvalidProposalError(`Cross-KB evidence target ${target.id} is not supported`);
    }
    if (!targetExists(db, target)) {
      throw new NotFoundError(`Evidence target does not exist: ${target.type} ${target.id}`);
    }
  }

  if (payload.kind === "image") {
    const asset = payload.asset;
    if (asset === undefined) throw new InvalidProposalError("Image evidence needs an asset");
    const stored = db
      .query<{ sha256: string; media_type: string; byte_size: number }, [string]>(
        "SELECT sha256, media_type, byte_size FROM assets WHERE sha256 = ?",
      )
      .get(asset.sha256);
    if (stored === null) {
      throw new NotFoundError(`Asset not uploaded: ${asset.sha256}`);
    }
    if (stored.byte_size !== asset.byteSize) {
      throw new InvalidProposalError(
        `Asset ${asset.sha256} is ${stored.byte_size} bytes, proposal declares ${asset.byteSize}`,
      );
    }
  }

  const id = newId("evd");
  const createdAt = now();
  const evidence = EvidenceSchema.parse({
    id,
    kind: payload.kind,
    title: payload.title,
    ...(payload.locator === undefined ? {} : { locator: payload.locator }),
    ...(payload.excerpt === undefined ? {} : { excerpt: payload.excerpt }),
    ...(payload.asset === undefined ? {} : { asset: payload.asset }),
    capturedAt: payload.capturedAt ?? createdAt,
    capturedBy: payload.capturedBy,
    createdAt,
    targets: payload.targets,
  });

  db.run(
    `INSERT INTO evidence (id, kind, title, locator, excerpt, asset_sha256, asset_media_type, asset_bytes, captured_at, captured_by, created_at)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    [
      evidence.id,
      evidence.kind,
      evidence.title,
      evidence.locator ?? null,
      evidence.excerpt ?? null,
      evidence.asset?.sha256 ?? null,
      evidence.asset?.mediaType ?? null,
      evidence.asset?.byteSize ?? null,
      evidence.capturedAt,
      evidence.capturedBy,
      evidence.createdAt,
    ],
  );
  for (const target of evidence.targets) {
    db.run("INSERT INTO evidence_targets (evidence_id, target_type, target_id) VALUES (?, ?, ?)", [
      evidence.id,
      target.type,
      target.id,
    ]);
  }
  return evidence.id;
}

/**
 * Record that a concept's document changed without touching its record.
 *
 * `source_hash` stays as recorded: the Markdown file behind it did not change,
 * and clearing it would make the next import treat an unchanged file as an
 * edit and report a conflict against itself.
 */
function touchConcept(db: Database, conceptId: string, timestamp: string): void {
  refreshConceptRevision(db, conceptId, timestamp);
}

/**
 * Reject a graph-breaking change before it commits.
 *
 * Relation assertions are the only proposal kind that can create a cycle or a
 * layer violation, and a cycle makes the closure non-terminating for every
 * later query — so the check belongs in the write transaction, not in a
 * post-hoc report.
 */
export function assertGraphStillValid(db: Database, options: { allowOrphans?: boolean } = {}): void {
  const graph = graphFromRecords(listConceptRows(db), listRelationRows(db));
  // `allowOrphans` means "orphans are acceptable here", so the filter drops
  // them only when it is set. Written the other way round it silently disabled
  // the guard for relation assertions and rejected every write-back of a
  // concept whose document omits its edges.
  const issues = validateGraph(graph).issues.filter(
    (issue) => options.allowOrphans !== true || issue.type !== "orphan-node",
  );
  if (issues.length > 0) {
    throw new InvalidProposalError(issues.map((issue) => issue.message).join("; "));
  }
}

function applyMarkdownWriteback(db: Database, proposal: KnowledgeProposal): string[] {
  const document = typeof proposal.payload.document === "string" ? proposal.payload.document : "";
  const parsed = parseKnowledgeDocument(document);
  if (parsed.frontmatter.id !== proposal.subject) {
    throw new InvalidProposalError(
      `Document declares concept ${parsed.frontmatter.id}, proposal subject is ${proposal.subject}`,
    );
  }

  const concept = requireConcept(db, proposal.subject);
  const applied: string[] = [];
  const existingClaims = listClaimRows(db, concept.id);
  const byId = new Map(existingClaims.map((claim) => [claim.id, claim]));
  const kept = new Set<string>();

  for (const projected of parsed.claims) {
    if (!ClaimIdSchema.safeParse(projected.id).success) {
      throw new InvalidProposalError(`Invalid claim id in document: ${projected.id}`);
    }
    const existing = byId.get(projected.id);
    if (existing === undefined) {
      throw new InvalidProposalError(
        `Document refers to claim ${projected.id}, which does not exist on ${concept.id}. Create it through a claim proposal first.`,
      );
    }
    kept.add(existing.id);
    const revision = claimRevision(projected.text, projected.kind, projected.confidence);
    if (revision !== currentClaimRevision(existing)) {
      db.run(
        "UPDATE claims SET text = ?, kind = ?, confidence = ?, revision = ?, updated_at = ? WHERE id = ?",
        [projected.text, projected.kind, projected.confidence, String(existing.revision + 1), now(), existing.id],
      );
      applied.push(existing.id);
    }
  }

  // Removing a claim from the document retires it instead of deleting it: its
  // evidence and its place in an earlier review stay traceable.
  for (const claim of existingClaims) {
    if (kept.has(claim.id) || claim.status !== "accepted") continue;
    db.run("UPDATE claims SET status = 'superseded', updated_at = ? WHERE id = ?", [now(), claim.id]);
    applied.push(claim.id);
  }

  const record = {
    id: concept.id,
    atom: parsed.frontmatter.atom,
    layer: parsed.frontmatter.layer,
    domain: parsed.frontmatter.domain,
    confidence: parsed.frontmatter.confidence,
    source: parsed.frontmatter.source,
    triggers: parsed.frontmatter.triggers,
    body: parsed.body === "" ? concept.body : parsed.body,
    sourcePath: concept.sourcePath,
  };
  db.run(
    `UPDATE concepts SET atom = ?, layer = ?, domain_json = ?, confidence = ?, source = ?, triggers_json = ?, body = ?, updated_at = ?
     WHERE id = ?`,
    [
      record.atom,
      record.layer,
      JSON.stringify(record.domain),
      record.confidence,
      record.source,
      JSON.stringify(record.triggers),
      record.body,
      now(),
      concept.id,
    ],
  );

  db.run("DELETE FROM relations WHERE from_id = ?", [concept.id]);
  parsed.frontmatter.edges.forEach((edge, position) => {
    db.run(
      "INSERT OR REPLACE INTO relations (from_id, type, target, confidence, position, created_at) VALUES (?, ?, ?, ?, ?, ?)",
      [concept.id, edge.type, edge.target, edge.confidence, position, now()],
    );
  });

  // A document is allowed to carry no edges, which leaves its concept isolated.
  // Cycles and layer violations are still rejected.
  assertGraphStillValid(db, { allowOrphans: true });
  refreshConceptRevision(db, concept.id, now());
  applied.push(concept.id);
  return applied;
}

/**
 * Record an already-stored asset.
 *
 * The bytes live on disk under their hash; the row is the index the evidence
 * writer checks, so a proposal cannot reference bytes that were never uploaded.
 */
export function recordAsset(db: Database, asset: EvidenceAsset): void {
  db.run(
    `INSERT INTO assets (sha256, media_type, byte_size, path, created_at)
     VALUES (?, ?, ?, ?, ?)
     ON CONFLICT(sha256) DO NOTHING`,
    [asset.sha256, asset.mediaType, asset.byteSize, asset.sha256, now()],
  );
}

/**
 * Store image bytes and index them, in one step.
 *
 * The two halves must not be separable in the normal path: bytes without a row
 * are invisible to evidence proposals, and a row without bytes is evidence that
 * cannot be opened.
 */
export async function uploadAsset(
  db: Database,
  kbRoot: string,
  bytes: Uint8Array,
  declaredMediaType?: string,
): Promise<EvidenceAsset> {
  const asset = await storeAsset(kbRoot, bytes, declaredMediaType);
  recordAsset(db, asset);
  return asset;
}

export interface VectorUploadResult {
  stored: Array<{ id: string; subjectType: string; subjectId: string; model: string }>;
}

/**
 * Store externally computed embeddings.
 *
 * The revision check is the contract: an agent embeds the text it read, and the
 * store refuses a vector whose revision does not match the live text. Storing a
 * mismatched vector and filtering it at query time would be quieter but worse —
 * the agent would never learn that its embedding pipeline is out of date.
 */
export function upsertVectors(db: Database, raw: unknown): VectorUploadResult {
  const { vectors } = UploadVectorsSchema.parse(raw);
  const stored: VectorUploadResult["stored"] = [];

  transaction(db, () => {
    for (const input of vectors) {
      // 1e39 is a finite double and +Infinity as float32. Storing it would make
      // every later cosine() on this model throw, poisoning the whole axis until
      // the row is deleted by hand.
      const overflow = input.values.findIndex((value) => Math.abs(value) > FLOAT32_MAX);
      if (overflow !== -1) {
        throw new InvalidProposalError(
          `Vector value at index ${overflow} is ${input.values[overflow]} which overflows float32 (limit ${FLOAT32_MAX})`,
        );
      }
      const subject = requireSubjectText(db, input.subjectType, input.subjectId);
      const liveRevision = embeddingRevision(subject.text);
      if (liveRevision !== input.revision) {
        throw new ConflictError(
          `Vector for ${input.subjectType} ${input.subjectId} was computed from revision ${input.revision.slice(0, 12)}, current is ${liveRevision.slice(0, 12)}`,
          input.revision,
          liveRevision,
        );
      }

      // A model has exactly one dimension count. Two lengths under one model
      // would let a query pick whichever happens to match its own length.
      const modelShape = db
        .query<{ dimensions: number }, [string]>(
          "SELECT dimensions FROM vectors WHERE model = ? LIMIT 1",
        )
        .get(input.model);
      if (modelShape !== null && modelShape.dimensions !== input.values.length) {
        throw new ConflictError(
          `Model ${input.model} is stored with ${modelShape.dimensions} dimensions; this vector has ${input.values.length}`,
          String(modelShape.dimensions),
          String(input.values.length),
        );
      }

      const values = new Float32Array(input.values);
      const norm = Math.hypot(...input.values);
      const existing = db
        .query<{ id: string; dimensions: number }, [string, string, string]>(
          "SELECT id, dimensions FROM vectors WHERE subject_type = ? AND subject_id = ? AND model = ?",
        )
        .get(input.subjectType, input.subjectId, input.model);
      if (existing !== null && existing.dimensions !== input.values.length) {
        throw new ConflictError(
          `Existing vector for ${input.subjectType} ${input.subjectId} has ${existing.dimensions} dimensions, not ${input.values.length}`,
          String(existing.dimensions),
          String(input.values.length),
        );
      }

      const id = existing?.id ?? newId("vec");
      db.run(
        `INSERT INTO vectors (id, subject_type, subject_id, model, dimensions, revision, provider, values_blob, norm, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(subject_type, subject_id, model) DO UPDATE SET
           revision = excluded.revision,
           provider = excluded.provider,
           values_blob = excluded.values_blob,
           norm = excluded.norm,
           created_at = excluded.created_at`,
        [
          id,
          input.subjectType,
          input.subjectId,
          input.model,
          input.values.length,
          input.revision,
          input.provider ?? null,
          new Uint8Array(values.buffer),
          norm,
          now(),
        ],
      );
      stored.push({ id, subjectType: input.subjectType, subjectId: input.subjectId, model: input.model });
    }
  });

  return { stored };
}

/** The exact text a vector must have been computed from. */
export function requireSubjectText(
  db: Database,
  subjectType: VectorRecord["subjectType"],
  subjectId: string,
): { text: string; conceptId: string | null } {
  if (subjectType === "concept") {
    const concept = requireConcept(db, subjectId);
    return { text: embeddingTextForConcept(concept), conceptId: concept.id };
  }
  if (subjectType === "claim") {
    const claim = getClaimRow(db, subjectId);
    if (claim === null) throw new NotFoundError(`Claim not found: ${subjectId}`);
    return { text: embeddingTextForClaim(claim), conceptId: claim.conceptId };
  }
  const evidence = getEvidence(db, subjectId);
  if (evidence === null) throw new NotFoundError(`Evidence not found: ${subjectId}`);
  return { text: embeddingTextForEvidence(evidence), conceptId: null };
}

export interface EvidenceView {
  evidence: Evidence[];
  concepts: Array<{ id: string; atom: string }>;
  claims: Claim[];
}

/** Evidence for a concept or claim, with the sources that make it traceable. */
export function evidenceForTarget(db: Database, type: EvidenceTarget["type"], id: string): EvidenceView {
  if (type === "concept") requireConcept(db, id);
  if (type === "claim") {
    if (getClaimRow(db, id) === null) throw new NotFoundError(`Claim not found: ${id}`);
  }

  const all = listEvidenceRows(db);
  // Asking a concept for its evidence must include what supports its claims:
  // an image that backs a specific assertion is still evidence for the concept,
  // and a reviewer following the concept trail would otherwise miss it.
  const evidence =
    type === "concept"
      ? (() => {
          const claimIds = new Set(
            listClaimRows(db, id).filter((claim) => claim.status === "accepted").map((claim) => claim.id),
          );
          return [
            ...evidenceFor(all, "concept", id),
            ...all.filter((item) => item.targets.some((target) => target.type === "claim" && claimIds.has(target.id))),
          ].filter((item, index, list) => list.findIndex((other) => other.id === item.id) === index);
        })()
      : evidenceFor(all, type, id);
  const conceptIds = new Set<string>();
  const claimIds = new Set<string>();
  if (type === "claim") {
    // A claim's evidence belongs to the claim's concept too: a reviewer asking
    // "what supports this assertion" needs to know which concept it lives under.
    const claim = getClaimRow(db, id);
    if (claim !== null) conceptIds.add(claim.conceptId);
  }
  for (const item of evidence) {
    for (const target of item.targets) {
      if (target.type === "concept") conceptIds.add(target.id);
      if (target.type === "claim") claimIds.add(target.id);
    }
  }

  const concepts = [...conceptIds]
    .map((conceptId) => getConceptRow(db, conceptId))
    .filter((concept): concept is ConceptRecord => concept !== null)
    .map((concept) => ({ id: concept.id, atom: concept.atom }));
  const claims = [...claimIds]
    .map((claimId) => getClaimRow(db, claimId))
    .filter((claim): claim is Claim => claim !== null);

  return { evidence, concepts, claims };
}

/** Accepted claims with their supporting evidence, for a concept document view. */
export function conceptClaims(db: Database, conceptId: string): Array<Claim & { evidence: Array<{ id: string; title: string; kind: string }> }> {
  requireConcept(db, conceptId);
  const evidence = listEvidenceRows(db);
  return listAcceptedClaims(db)
    .filter((claim) => claim.conceptId === conceptId)
    .map((claim) => ({
      ...claim,
      evidence: evidenceFor(evidence, "claim", claim.id).map((item: Evidence) => ({
        id: item.id,
        title: item.title,
        kind: item.kind,
      })),
    }));
}

export { SHA256Schema, listClaimRows };
