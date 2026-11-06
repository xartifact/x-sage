import {
  EdgeTypeSchema,
  ConfidenceSchema,
  EDGE_TYPES,
  type Confidence,
  type EdgeType,
} from "@x-sage/core";
import { completeSymmetricEdges } from "@x-sage/core";
import { formatManifest } from "@x-sage/core/manifest";
import { CheckpointSchema } from "@x-sage/core";
import type { KbContext } from "./kb.ts";
import {
  activate,
  buildGraph,
  buildVizPayload,
  createCheckpoint,
  findPath,
  getCheckpoint,
  isActivationStrategy,
  listFrontier,
  listProposals,
  mergeProposals,
  queryEdges,
  renderManifest,
  saveCheckpoint,
  search,
  semanticGraph,
  submitAtomProposal,
  submitEdgeProposal,
  validateConcepts,
  writeGraph,
  writeManifest,
} from "./index.ts";
import {
  renderActivation,
  renderCheckpoint,
  renderEdges,
  renderFrontier,
  renderGraphSection,
  renderGraphStatus,
  renderJson,
  renderMergeOutcome,
  renderPath,
  renderProposals,
  renderSearchResults,
  renderValidationReport,
  renderVizStats,
} from "./render.ts";

/**
 * The single request dispatcher.
 *
 * Both front ends go through `handleRequest`: the HTTP server adapts a real
 * request into the `ApiRequest` shape, and the CLI builds the same shape from
 * argv and calls it in-process. One routing table, one set of error messages,
 * one renderer — a client machine gets byte-identical output to a local run.
 *
 * Deliberately free of `hono`, `fetch` and any Response object so the service
 * layer stays transport-agnostic.
 */

export type ApiMethod = "GET" | "POST" | "PUT";

export interface ApiRequest {
  method: ApiMethod;
  /** Path below `/api`, e.g. `/concepts/decision/edges`. Always starts with `/`. */
  path: string;
  query: Record<string, string | undefined>;
  /** Parsed JSON body for POST/PUT. */
  body?: unknown;
}

export interface ApiResponse {
  status: number;
  contentType: string;
  body: string;
}

const JSON_TYPE = "application/json; charset=utf-8";
const TEXT_TYPE = "text/plain; charset=utf-8";
const MARKDOWN_TYPE = "text/markdown; charset=utf-8";

/** A failure that carries the HTTP status it should become. */
class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

function json(status: number, value: unknown): ApiResponse {
  // Trailing newline keeps CLI stdout uniform: the same body serves a terminal
  // (which reads better ending on its own line) and an HTTP client.
  return { status, contentType: JSON_TYPE, body: JSON.stringify(value) + "\n" };
}

function text(status: number, body: string, contentType = TEXT_TYPE): ApiResponse {
  return { status, contentType, body };
}

/** Parse a positive integer query parameter, or undefined when absent. */
function intParam(query: ApiRequest["query"], name: string): number | undefined {
  const raw = query[name];
  if (raw === undefined || raw === "") return undefined;
  const parsed = Number.parseInt(raw, 10);
  if (Number.isNaN(parsed)) {
    throw new ApiError(400, `Invalid ${name}: ${raw}`);
  }
  return parsed;
}

function boolParam(query: ApiRequest["query"], name: string): boolean {
  return query[name] === "true";
}

/** Output format requested by the caller: `json` unless stated otherwise. */
type Format = "json" | "text" | "markdown" | "manifest";

function parseFormat(query: ApiRequest["query"], accept?: string): Format {
  const raw = query.format;
  if (raw === "markdown") return "markdown";
  if (raw === "text" || raw === "ai" || raw === "human" || raw === "json" || raw === "manifest") {
    return raw === "ai" || raw === "human" ? "text" : raw;
  }
  // Content negotiation: a client that asks for text gets text. Markdown is
  // only chosen when the request can return a whole concept file, which the
  // concept route checks separately.
  if (accept !== undefined && accept.includes("text/markdown")) return "text";
  if (accept !== undefined && accept.includes("text/plain") && !accept.includes("json")) return "text";
  return "json";
}

/**
 * Whether the caller asked for human-readable output.
 *
 * Shared with the HTTP layer: the auth middleware fails before the dispatcher
 * runs and still has to answer in the requested format.
 */
export function wantsText(format: string | null, accept?: string | null): boolean {
  if (format === "text" || format === "ai" || format === "human" || format === "manifest") return true;
  if (format !== null) return false;
  return accept?.includes("text/plain") === true && accept.includes("json") === false;
}

function requireBody(request: ApiRequest): Record<string, unknown> {
  const body = request.body;
  if (typeof body !== "object" || body === null || Array.isArray(body)) {
    throw new ApiError(400, "Expected a JSON object body");
  }
  return body as Record<string, unknown>;
}

function requireField(body: Record<string, unknown>, field: string): string {
  const value = body[field];
  if (typeof value !== "string" || value === "") {
    throw new ApiError(400, `Missing field: ${field}`);
  }
  return value;
}

function optionalField(body: Record<string, unknown>, field: string): string | undefined {
  const value = body[field];
  return typeof value === "string" && value !== "" ? value : undefined;
}



/**
 * Dispatch one API request against a KB.
 *
 * `accept` is used only for content negotiation; the CLI passes the format it
 * wants as a query parameter so its output never depends on header defaults.
 */
export async function handleRequest(
  ctx: KbContext,
  request: ApiRequest,
  accept?: string,
): Promise<ApiResponse> {
  const { method, path, query } = request;
  const format = parseFormat(query, accept);

  try {
    return await route(ctx, method, path, query, request, format);
  } catch (err) {
    const status =
      err instanceof ApiError
        ? err.status
        : (err instanceof Error ? err.message : String(err)).includes("not found")
          ? 404
          : 400;
    const message =
      err instanceof ApiError
        ? err.message
        : err instanceof Error
          ? err.message
          : String(err);

    // Errors honour the requested format too. A CLI asking for text would
    // otherwise print a JSON object to stderr, which reads as a crash rather
    // than a message.
    if (format === "text" || format === "markdown" || format === "manifest") {
      return text(status, `${message}\n`);
    }
    return json(status, { error: message });
  }
}

async function route(
  ctx: KbContext,
  method: ApiMethod,
  path: string,
  query: ApiRequest["query"],
  request: ApiRequest,
  format: Format,
): Promise<ApiResponse> {
  // ---- identity ----
  if (path === "/health" && method === "GET") {
    return json(200, { ok: true, kb: ctx.root });
  }

  if (path === "/meta" && method === "GET") {
    const graph = await buildGraph(ctx);
    return json(200, { kb: ctx.root, nodes: graph.nodes.length, edges: graph.edges.length });
  }

  // ---- manifest (the AI's opening move) ----
  if (path === "/manifest" && method === "GET") {
    const markdown = await renderManifest(ctx);
    // `markdown` ends with a newline; one more matches the old `console.log`.
    return format === "json" ? json(200, { markdown }) : text(200, `${markdown}\n`, MARKDOWN_TYPE);
  }

  // Regenerating MANIFEST.md is idempotent: the same KB always yields the same
  // file. PUT states that; a POST verb-in-path would not.
  if (path === "/manifest" && method === "PUT") {
    const result = await writeManifest(ctx);
    if (format === "text") {
      return text(200, `MANIFEST.md written (${result.count} concepts).\n`);
    }
    return json(200, { concepts: result.count });
  }

  // ---- visualiser payload ----
  if (path === "/viz" && method === "GET") {
    const payload = await buildVizPayload(ctx);
    return format === "text" ? text(200, renderVizStats(payload)) : json(200, payload);
  }

  // ---- graph ----
  if (path === "/graph" && method === "GET") {
    const graph = await buildGraph(ctx);
    return json(200, boolParam(query, "semantic") ? semanticGraph(graph) : graph);
  }

  // Rebuilding graph.json from the concept files is idempotent, so it is a PUT
  // on the graph resource rather than a verb path.
  if (path === "/graph" && method === "PUT") {
    const body = request.body === undefined ? {} : requireBody(request);
    const graph = await writeGraph(ctx);
    const manifest = body.manifest === true ? await writeManifest(ctx) : null;
    if (format === "text") {
      const lines = [`Generated graph.json: ${graph.nodes.length} nodes, ${graph.edges.length} edges.`];
      if (manifest !== null) lines.push(`MANIFEST.md written (${manifest.count} concepts).`);
      return text(200, `${lines.join("\n")}\n`);
    }
    return json(200, {
      nodes: graph.nodes.length,
      edges: graph.edges.length,
      manifest: manifest === null ? null : { concepts: manifest.count },
    });
  }

  // ---- search ----
  if (path === "/search" && method === "GET") {
    const keyword = query.q;
    if (keyword === undefined || keyword === "") {
      throw new ApiError(400, "Missing query parameter: q");
    }
    const results = await search(ctx, keyword);
    if (format === "manifest") return text(200, `${formatManifest(results)}\n`, MARKDOWN_TYPE);
    if (format === "text") return text(200, renderSearchResults(keyword, results));
    return json(200, { keyword, results });
  }

  // ---- path ----
  if (path === "/path" && method === "GET") {
    const from = query.from;
    const to = query.to;
    if (from === undefined || to === undefined) {
      throw new ApiError(400, "Missing query parameters: from, to");
    }
    const result = await findPath(ctx, from, to);
    return format === "text" ? text(200, renderPath(result)) : json(200, result);
  }

  // ---- validation ----
  if (path === "/validate" && method === "GET") {
    const report = await validateConcepts(ctx, { graph: boolParam(query, "graph") });
    const status = report.valid ? 200 : 422;

    // `format=graph` is `graph validate`: only the integrity half, no file list.
    if (query.format === "graph") {
      if (report.graph === undefined) {
        throw new ApiError(400, "format=graph requires graph=true");
      }
      return text(status, renderGraphStatus(report.graph));
    }
    if (format === "text") return text(status, renderValidationReport(report, ctx.root));
    return json(status, report);
  }

  // ---- per-concept routes ----
  const conceptMatch = /^\/concepts\/([^/]+)(\/edges)?$/.exec(path);
  if (conceptMatch !== null) {
    const conceptId = decodeURIComponent(conceptMatch[1]!);
    const isEdges = conceptMatch[2] !== undefined;

    if (!isEdges && method === "GET") {
      return conceptActivation(ctx, conceptId, query, format);
    }
    if (isEdges && method === "GET") {
      return conceptEdges(ctx, conceptId, query, format);
    }
  }

  // ---- frontier ----
  if (path === "/frontier" && method === "GET") {
    const entries = await listFrontier(ctx);
    return format === "text" ? text(200, renderFrontier(entries)) : json(200, entries);
  }

  // ---- proposals ----
  if (path === "/proposals" && method === "GET") {
    const lists = await listProposals(ctx);
    return format === "text" ? text(200, renderProposals(lists)) : json(200, lists);
  }

  if (path === "/proposals/merge" && method === "POST") {
    const body = request.body === undefined ? {} : requireBody(request);
    const outcome = await mergeProposals(ctx, optionalField(body, "file") ?? "all");
    return format === "text" ? text(200, renderMergeOutcome(outcome)) : json(200, outcome);
  }

  // One collection, two kinds. The kind travels in the body as `kind`, so the
  // path names a resource instead of an action.
  if (path === "/proposals" && method === "POST") {
    const body = requireBody(request);
    const kind = optionalField(body, "kind") ?? "edge";

    if (kind === "edge") {
      return proposeEdge(ctx, body, format);
    }
    if (kind === "atom") {
      return proposeAtom(ctx, body, format);
    }
    throw new ApiError(400, `Unknown proposal kind: ${kind}. Must be edge or atom`);
  }

  // ---- checkpoints ----
  const checkpointMatch = /^\/checkpoints\/([^/]+)$/.exec(path);
  if (checkpointMatch !== null) {
    const concept = decodeURIComponent(checkpointMatch[1]!);

    if (method === "GET") {
      const checkpoint = await getCheckpoint(ctx, concept, intParam(query, "session"));
      if (checkpoint === null) {
        throw new ApiError(404, `No checkpoints found for: ${concept}`);
      }
      return format === "text" ? text(200, renderCheckpoint(checkpoint)) : json(200, checkpoint);
    }

    if (method === "POST") {
      const body = request.body === undefined ? {} : requireBody(request);
      const sessionNumber = body.sessionNumber;
      const checkpoint = await createCheckpoint(
        ctx,
        concept,
        typeof sessionNumber === "number" ? sessionNumber : undefined,
      );
      return json(201, checkpoint);
    }

    if (method === "PUT") {
      const body = requireBody(request);
      if (body.concept !== concept) {
        throw new ApiError(
          400,
          `Body concept "${String(body.concept)}" does not match path concept "${concept}"`,
        );
      }
      const checkpoint = CheckpointSchema.safeParse(body);
      if (!checkpoint.success) {
        // A malformed checkpoint would be written verbatim and break every later
        // read; report the first field instead of dumping the zod issue tree.
        const first = checkpoint.error.issues[0];
        throw new ApiError(
          400,
          first === undefined ? "Invalid checkpoint body" : `Invalid checkpoint.${first.path.join(".") || "(root)"}: ${first.message}`,
        );
      }
      await saveCheckpoint(ctx, checkpoint.data);
      return json(200, { ok: true });
    }
  }

  throw new ApiError(404, "Not Found");
}

async function conceptActivation(
  ctx: KbContext,
  conceptId: string,
  query: ApiRequest["query"],
  format: Format,
): Promise<ApiResponse> {
  const strategy = query.strategy;
  if (strategy !== undefined && !isActivationStrategy(strategy)) {
    throw new ApiError(400, `Invalid strategy: ${strategy}. Must be exploration, validation or generation`);
  }

  const depth = intParam(query, "depth");
  if (depth !== undefined && depth < 1) {
    throw new ApiError(400, `Invalid depth: ${depth}`);
  }

  const result = await activate(ctx, conceptId, {
    ...(strategy === undefined ? {} : { strategy }),
    ...(depth === undefined ? {} : { depth }),
  });

  if (format === "markdown") {
    // `raw` is the file body, already newline-terminated. The old CLI printed it
    // with console.log, which added one more; consumers' diffing depends on it.
    return text(200, `${result.raw}\n`, MARKDOWN_TYPE);
  }
  if (format === "text") return text(200, renderActivation(result));

  // `markdown` duplicates `raw`; JSON consumers use one name.
  const { raw, ...rest } = result;
  return json(200, { ...rest, markdown: raw });
}

async function conceptEdges(
  ctx: KbContext,
  conceptId: string,
  query: ApiRequest["query"],
  format: Format,
): Promise<ApiResponse> {
  const direction = query.dir ?? "both";
  if (direction !== "in" && direction !== "out" && direction !== "both") {
    throw new ApiError(400, `Invalid dir: ${direction}. Must be in, out or both`);
  }

  let type: EdgeType | undefined;
  const typeRaw = query.type;
  if (typeRaw !== undefined) {
    const parsed = EdgeTypeSchema.safeParse(typeRaw);
    if (!parsed.success) {
      throw new ApiError(400, `Invalid edge type: ${typeRaw}. Must be one of: ${EDGE_TYPES.join(", ")}`);
    }
    type = parsed.data as EdgeType;
  }

  const graph = await buildGraph(ctx);
  if (!graph.nodes.some((node) => node.id === conceptId)) {
    throw new ApiError(404, `Concept not found: ${conceptId}`);
  }

  // Symmetric completion always applies so one asserted tension is visible from
  // both ends; closure is opt-in because it is inference, not authorship.
  // Starting from the semantic graph would keep closure edges even when they
  // were not asked for, so the two views are built from their own base.
  const derived = boolParam(query, "derived");
  const view = derived
    ? semanticGraph(graph)
    : { nodes: graph.nodes, edges: completeSymmetricEdges(graph).edges };
  const hits = queryEdges(view, conceptId, {
    ...(type === undefined ? {} : { type }),
    direction,
  });

  return format === "text"
    ? text(200, renderEdges(conceptId, hits))
    : json(200, { id: conceptId, direction, edges: hits });
}

async function proposeEdge(
  ctx: KbContext,
  body: Record<string, unknown>,
  format: Format,
): Promise<ApiResponse> {
  const parsedType = EdgeTypeSchema.safeParse(body.type);
  if (!parsedType.success) {
    throw new ApiError(400, "Invalid or missing field: type");
  }
  const confidenceRaw = optionalField(body, "confidence") ?? "hypothetical";
  const parsedConfidence = ConfidenceSchema.safeParse(confidenceRaw);
  if (!parsedConfidence.success) {
    throw new ApiError(400, `Invalid confidence: ${confidenceRaw}`);
  }

  const from = requireField(body, "from");
  const target = requireField(body, "target");
  const filename = await submitEdgeProposal(ctx, {
    from,
    type: parsedType.data as EdgeType,
    target,
    confidence: parsedConfidence.data as Confidence,
  });

  if (format === "text") {
    return text(
      201,
      `Proposed edge: ${from} --${parsedType.data}--> ${target} (${parsedConfidence.data})\nWritten to: proposals/edges/${filename}\n`,
    );
  }
  return json(201, { file: filename });
}

async function proposeAtom(
  ctx: KbContext,
  body: Record<string, unknown>,
  format: Format,
): Promise<ApiResponse> {
  const reason = optionalField(body, "reason");
  const conceptId = requireField(body, "conceptId");
  const proposedAtom = requireField(body, "proposedAtom");
  const filename = await submitAtomProposal(ctx, {
    conceptId,
    proposedAtom,
    ...(reason === undefined ? {} : { reason }),
  });

  if (format === "text") {
    return text(201, `Proposed atom update for: ${conceptId}\nWritten to: proposals/atoms/${filename}\n`);
  }
  return json(201, { file: filename });
}

/** Re-exported so the server can render a graph-only report without a KB. */
export { renderGraphSection };
