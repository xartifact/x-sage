import type { Database } from "bun:sqlite";
import {
  MAX_EVIDENCE_BYTES,
  MAX_QUERY_CHARS,
  MAX_SEARCH_LIMIT,
  ProposalPayloadSchema,
  ReviewSchema,
  UploadVectorsSchema,
  type KnowledgeProposal,
  type SearchResponse,
} from "@x-sage/core/knowledge";
import { ApiError, binary, json, text, MARKDOWN_TYPE, type ApiResponse } from "./http.ts";
import type { KbContext } from "./kb.ts";
import { authorityOf } from "./authority.ts";
import {
  AssetTooLargeError,
  AssetTypeError,
  ConflictError,
  ImportConflictError,
  ImportRefusedError,
  InvalidProposalError,
  NotFoundError,
  ReviewStateError,
  conceptClaims,
  conceptDocument,
  conceptsFromStore,
  evidenceFromStore,
  evidenceForTarget,
  expectedRevisionFor,
  getProposal,
  hybridSearch,
  importKnowledgeBase,
  listKnowledgeProposals,
  openStore,
  readAsset,
  reviewProposal,
  storeStatus,
  uploadAsset,
  submitProposal,
  upsertVectors,
} from "./store/index.ts";

/**
 * The structured-knowledge REST surface.
 *
 * Kept apart from the legacy routing table so the two authorities read clearly:
 * `/api/*` continues to serve Markdown-era reads, `/api/knowledge/*` serves the
 * structured store. Callers switch by migration, not by guessing which route a
 * given KB happens to answer.
 *
 * Every error thrown by the store maps to a status here, in one place:
 * not-found → 404, revision conflict → 409, invalid payload → 400, and a
 * refused second import → 409 with the reason.
 */

export interface KnowledgeRouteRequest {
  method: "GET" | "POST" | "PUT";
  /** Path below `/api/knowledge`, e.g. `/proposals/prp_12ab34/review`. */
  path: string;
  query: Record<string, string | undefined>;
  body?: unknown;
  format: "json" | "text" | "markdown" | "manifest";
}

/** Assets are binary; a JSON envelope keeps the API uniform and bounded. */
const MAX_ASSET_JSON_BYTES = Math.ceil((MAX_EVIDENCE_BYTES * 4) / 3) + 1024;

export async function handleKnowledgeRequest(
  ctx: KbContext,
  request: KnowledgeRouteRequest,
): Promise<ApiResponse> {
  try {
    return await route(ctx, request);
  } catch (err) {
    throw mapStoreError(err);
  }
}

function mapStoreError(err: unknown): Error {
  if (err instanceof ApiError) return err;
  if (err instanceof NotFoundError) return new ApiError(404, err.message);
  if (err instanceof ConflictError) return new ApiError(409, err.message);
  if (err instanceof ReviewStateError) return new ApiError(409, err.message);
  if (err instanceof ImportRefusedError) return new ApiError(409, err.message);
  if (err instanceof ImportConflictError) return new ApiError(409, err.message);
  if (err instanceof InvalidProposalError) return new ApiError(400, err.message);
  if (err instanceof AssetTooLargeError) return new ApiError(413, err.message);
  if (err instanceof AssetTypeError) return new ApiError(415, err.message);
  if (err instanceof Error && /no such table|not a database/i.test(err.message)) {
    return new ApiError(503, "The knowledge store is unavailable or corrupt");
  }
  return err instanceof Error ? err : new Error(String(err));
}

function requireObject(body: unknown): Record<string, unknown> {
  if (typeof body !== "object" || body === null || Array.isArray(body)) {
    throw new ApiError(400, "Expected a JSON object body");
  }
  return body as Record<string, unknown>;
}

function requireString(body: Record<string, unknown>, field: string): string {
  const value = body[field];
  if (typeof value !== "string" || value === "") throw new ApiError(400, `Missing field: ${field}`);
  return value;
}

function withDb<T>(ctx: KbContext, fn: (db: Database) => T): T {
  const db = openStore(ctx.root);
  try {
    return fn(db);
  } finally {
    db.close();
  }
}

function requireStructured(ctx: KbContext): void {
  if (authorityOf(ctx) === "markdown") {
    throw new ApiError(
      409,
      "This knowledge base is still Markdown-authored. Run POST /api/knowledge/import (mode=authority) to migrate it explicitly.",
    );
  }
}

async function route(ctx: KbContext, request: KnowledgeRouteRequest): Promise<ApiResponse> {
  const { method, path, query, format } = request;

  // ---- status (works before and after migration) ----
  if (path === "/status" && method === "GET") {
    const status = await storeStatus(ctx);
    return json(200, status);
  }

  // ---- explicit migration ----
  if (path === "/import" && method === "POST") {
    const body = request.body === undefined ? { mode: "authority" } : requireObject(request.body);
    if (body.mode !== "authority") {
      throw new ApiError(400, 'Import requires mode="authority": Markdown becomes a read-only archive once imported');
    }
    const report = await importKnowledgeBase(ctx, { reimport: body.reimport === true });
    const status = report.counts.conflicts > 0 ? 207 : 201;
    if (format === "text") {
      const lines = [
        `${report.firstImport ? "Imported" : "Re-imported"} knowledge base: ${report.counts.created} created, ${report.counts.updated} updated, ${report.counts.unchanged} unchanged, ${report.counts.conflicts} conflict(s), ${report.counts.invalid} invalid`,
      ];
      if (report.backupDir !== null) lines.push(`Markdown snapshot: ${report.backupDir}`);
      for (const outcome of report.concepts) {
        if (outcome.result === "conflict" || outcome.result === "invalid") {
          lines.push(`  ! ${outcome.sourcePath || outcome.conceptId}: ${outcome.detail ?? outcome.result}`);
        }
      }
      return text(status, `${lines.join("\n")}\n`);
    }
    return json(status, report);
  }

  requireStructured(ctx);

  // ---- concepts ----
  if (path === "/concepts" && method === "GET") {
    const concepts = withDb(ctx, (db) =>
      conceptsFromStore(db).map((concept) => ({
        id: concept.id,
        atom: concept.atom,
        layer: concept.layer,
        revision: concept.revision,
      })),
    );
    return json(200, { concepts });
  }

  const documentMatch = /^\/concepts\/([^/]+)\/document$/.exec(path);
  if (documentMatch !== null && method === "GET") {
    const conceptId = decodeURIComponent(documentMatch[1]!);
    const view = withDb(ctx, (db) => conceptDocument(db, conceptId));
    if (view === null) throw new ApiError(404, `Concept not found: ${conceptId}`);
    if (format === "markdown" || format === "text") {
      return text(200, view.document, MARKDOWN_TYPE);
    }
    return json(200, view);
  }

  const claimsMatch = /^\/concepts\/([^/]+)\/claims$/.exec(path);
  if (claimsMatch !== null && method === "GET") {
    const conceptId = decodeURIComponent(claimsMatch[1]!);
    const claims = withDb(ctx, (db) => conceptClaims(db, conceptId));
    return json(200, { conceptId, claims });
  }

  const evidenceMatch = /^\/concepts\/([^/]+)\/evidence$/.exec(path);
  if (evidenceMatch !== null && method === "GET") {
    const conceptId = decodeURIComponent(evidenceMatch[1]!);
    const view = withDb(ctx, (db) => evidenceForTarget(db, "concept", conceptId));
    return json(200, { conceptId, ...view });
  }

  const claimEvidenceMatch = /^\/claims\/([^/]+)\/evidence$/.exec(path);
  if (claimEvidenceMatch !== null && method === "GET") {
    const claimId = decodeURIComponent(claimEvidenceMatch[1]!);
    const view = withDb(ctx, (db) => evidenceForTarget(db, "claim", claimId));
    return json(200, { claimId, ...view });
  }

  const evidenceDetail = /^\/evidence\/([^/]+)$/.exec(path);
  if (evidenceDetail !== null && method === "GET") {
    const id = decodeURIComponent(evidenceDetail[1]!);
    const item = withDb(ctx, (db) => evidenceFromStore(db).find((candidate) => candidate.id === id) ?? null);
    if (item === null) throw new ApiError(404, `Evidence not found: ${id}`);
    return json(200, item);
  }

  // ---- revision discovery ----
  // An agent about to propose needs the exact baseline string; asking the server
  // for it beats re-deriving the rule from documentation, and it cannot drift
  // from what the review will actually check.
  if (path === "/revisions" && method === "GET") {
    const kind = query.kind;
    const subject = query.subject;
    if (kind === undefined || subject === undefined) {
      throw new ApiError(400, "Missing query parameters: kind, subject");
    }
    const parsedKind = ProposalPayloadSchema.shape.kind.safeParse(kind);
    if (!parsedKind.success) throw new ApiError(400, `Invalid kind: ${kind}`);
    const revision = withDb(ctx, (db) =>
      // One source of truth: whatever `submitProposal` will compare against.
      // Duplicating any branch here is what let concept-markdown advertise a
      // value its own submission rejected.
      expectedRevisionFor(db, parsedKind.data, subject, {}),
    );
    return json(200, { kind: parsedKind.data, subject, expectedRevision: revision });
  }

  // ---- proposals ----
  if (path === "/proposals" && method === "POST") {
    const body = requireObject(request.body);
    const proposal = withDb(ctx, (db) =>
      submitProposal(db, {
        kind: body.kind as never,
        subject: requireString(body, "subject"),
        expectedRevision: requireString(body, "expectedRevision"),
        reason: requireString(body, "reason"),
        author: requireString(body, "author"),
        payload: requireObject(body.payload ?? {}),
      }),
    );
    if (format === "text") {
      return text(201, `Submitted ${proposal.kind} proposal ${proposal.id} (pending review)\n`);
    }
    return json(201, proposal);
  }

  if (path === "/proposals" && method === "GET") {
    const status = query.status;
    if (status !== undefined && !["pending", "approved", "rejected", "conflicted"].includes(status)) {
      throw new ApiError(400, `Invalid status: ${status}`);
    }
    const proposals = withDb(ctx, (db) =>
      listKnowledgeProposals(db, status as KnowledgeProposal["status"] | undefined),
    );
    if (format === "text") {
      if (proposals.length === 0) return text(200, "No knowledge proposals.\n");
      return text(
        200,
        `${proposals
          .map(
            (proposal) =>
              `${proposal.status.padEnd(10)} ${proposal.kind.padEnd(18)} ${proposal.subject}  [${proposal.id}] ${proposal.reason}`,
          )
          .join("\n")}\n`,
      );
    }
    return json(200, { proposals });
  }

  const proposalMatch = /^\/proposals\/([^/]+)$/.exec(path);
  if (proposalMatch !== null && method === "GET") {
    const id = decodeURIComponent(proposalMatch[1]!);
    const proposal = withDb(ctx, (db) => getProposal(db, id));
    if (proposal === null) throw new ApiError(404, `Proposal not found: ${id}`);
    return json(200, proposal);
  }

  const reviewMatch = /^\/proposals\/([^/]+)\/review$/.exec(path);
  if (reviewMatch !== null && method === "POST") {
    const id = decodeURIComponent(reviewMatch[1]!);
    const parsed = ReviewSchema.safeParse(request.body);
    if (!parsed.success) {
      const first = parsed.error.issues[0];
      throw new ApiError(400, `Invalid review: ${first?.path.join(".") ?? ""} ${first?.message ?? ""}`.trim());
    }
    const result = withDb(ctx, (db) =>
      reviewProposal(db, id, parsed.data.decision, parsed.data.reviewer, parsed.data.note),
    );
    if (format === "text") {
      return text(
        200,
        `${result.proposal.status === "approved" ? "Approved" : "Rejected"} ${id}` +
          (result.applied.length > 0 ? `; applied: ${result.applied.join(", ")}\n` : "\n"),
      );
    }
    return json(200, result);
  }

  // ---- hybrid retrieval ----
  if (path === "/search" && method === "POST") {
    const body = requireObject(request.body);
    const searchQuery = requireString(body, "query");
    if (searchQuery.length > MAX_QUERY_CHARS) {
      throw new ApiError(400, `Query is ${searchQuery.length} characters, limit is ${MAX_QUERY_CHARS}`);
    }
    const limitRaw = body.limit;
    if (limitRaw !== undefined && (typeof limitRaw !== "number" || !Number.isInteger(limitRaw) || limitRaw < 1)) {
      throw new ApiError(400, "limit must be a positive integer");
    }
    if (typeof limitRaw === "number" && limitRaw > MAX_SEARCH_LIMIT) {
      throw new ApiError(400, `limit is ${limitRaw}, maximum is ${MAX_SEARCH_LIMIT}`);
    }

    const vectorRaw = body.vector;
    if (vectorRaw !== undefined && (typeof vectorRaw !== "object" || vectorRaw === null)) {
      throw new ApiError(400, "vector must be an object");
    }
    const vector =
      vectorRaw === undefined
        ? undefined
        : {
            model: requireString(vectorRaw as Record<string, unknown>, "model"),
            values: requireValues((vectorRaw as Record<string, unknown>).values),
          };

    const validatedVector = vector;
    const response: SearchResponse = withDb(ctx, (db) =>
      hybridSearch(db, {
        query: searchQuery,
        ...(limitRaw === undefined ? {} : { limit: limitRaw }),
        ...(validatedVector === undefined ? {} : { vector: validatedVector }),
        ...(body.conceptId === undefined ? {} : { conceptId: String(body.conceptId) }),
      }),
    );
    return json(200, response);
  }

  // ---- vectors ----
  if (path === "/vectors" && method === "POST") {
    const parsed = UploadVectorsSchema.safeParse(request.body);
    if (!parsed.success) {
      const first = parsed.error.issues[0];
      throw new ApiError(
        400,
        `Invalid vectors: ${first?.path.join(".") ?? ""} ${first?.message ?? ""}`.trim(),
      );
    }
    if (parsed.data.vectors.some((vector) => vector.values.length > 4096)) {
      throw new ApiError(413, "Vector exceeds 4096 dimensions");
    }
    const result = withDb(ctx, (db) => upsertVectors(db, parsed.data));
    return json(201, result);
  }

  // ---- assets ----
  if (path === "/assets" && method === "POST") {
    const body = requireObject(request.body);
    const data = requireString(body, "data");
    if (data.length > MAX_ASSET_JSON_BYTES) {
      throw new ApiError(413, `Asset payload exceeds ${MAX_EVIDENCE_BYTES} bytes of image data`);
    }
    if (!/^[A-Za-z0-9+/=\r\n]+$/.test(data)) throw new ApiError(400, "data must be base64");
    const bytes = new Uint8Array(Buffer.from(data, "base64"));
    const declared = typeof body.mediaType === "string" ? body.mediaType : undefined;
    const db = openStore(ctx.root);
    try {
      return json(201, await uploadAsset(db, ctx.root, bytes, declared));
    } finally {
      db.close();
    }
  }

  const assetMatch = /^\/assets\/([^/]+)$/.exec(path);
  if (assetMatch !== null && method === "GET") {
    const sha = assetMatch[1]!;
    const asset = await readAsset(ctx.root, sha);
    if (asset === null) throw new ApiError(404, `Asset not found: ${sha}`);
    // Bytes already passed the content-address and signature checks.
    return binary(200, asset.bytes, asset.mediaType);
  }

  throw new ApiError(404, "Not Found");
}

function requireValues(value: unknown): number[] {
  if (!Array.isArray(value) || value.length === 0) throw new ApiError(400, "vector.values must be a non-empty array");
  if (value.length > 4096) throw new ApiError(413, "vector.values exceeds 4096 dimensions");
  return value.map((entry) => {
    if (typeof entry !== "number" || !Number.isFinite(entry)) {
      throw new ApiError(400, "vector.values must be finite numbers");
    }
    return entry;
  });
}
