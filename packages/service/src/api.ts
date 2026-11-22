import { appendFrontierRow, parseFrontierMarkdown, type FrontierEntry } from "@x-sage/core/frontier";
import { emptyCheckpoint } from "./write.ts";
import { ConceptIdSchema, type ConceptFrontmatter, type Graph, type ManifestRow } from '@x-sage/core/types';
import { catalog, CatalogUnavailableError } from './catalog.ts';
import { mkdir, writeFile, link, unlink } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import { join } from 'node:path';
import { ConceptFrontmatterSchema, validateGraph, withKbLock, LockTimeoutError } from '@x-sage/core';
import { activateInGraph, pathInGraph, type Activation } from "./query.ts";
import { validateGraphSnapshot } from "./validate.ts";
import { mergeProposalFiles, ProposalMergeUnresolvedError, type ProposalMergePort } from "./proposal-merge.ts";
import { vizFacts, vizFromGraph } from "./viz.ts";
import { loadConcepts } from './concepts.ts';
import { LAYERS } from './kb.ts';
import {
  EdgeTypeSchema,
  ConfidenceSchema,
  EDGE_TYPES,
  type Confidence,
  type EdgeType,
} from "@x-sage/core";
import { completeSymmetricEdges } from "@x-sage/core";
import { conceptRevision, formatManifest, searchManifest } from "@x-sage/core/manifest";
import { CheckpointSchema } from "@x-sage/core";
import {
  ProposalConflictError,
  ProposalTargetNotFoundError,
  diffBodies,
  formatUnifiedDiff,
  readBodyProposal,
  listBodyProposals,
  type BodyProposal,
  type BodyReadback,
} from "@x-sage/core/proposals";
import { proposalsDir, requireMarkdownSource, type KbContext } from "./kb.ts";
import { ApiError, json, text, MARKDOWN_TYPE, type ApiResponse } from "./http.ts";
import {
  activate,
  appendFrontier,
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
  readBody,
  submitAtomProposal,
  submitBodyProposal,
  submitEdgeProposal,
  validateConcepts,
  writeGraph,
  writeManifest,
  type MergeOutcome,
  type ProposalLists,
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

interface NativeGraphWriteBasis {
  version: string;
  terms: ReadonlyMap<string, { id: string; revision: string }>;
}

/** Operations available to the Bun service for its configured database. */
export interface NativeCheckpointStore {
  inspect(concept: string, session: number | undefined, withContent: boolean): Promise<{
    version: string;
    session?: number;
    checkpoint?: import("@x-sage/core").Checkpoint;
  }>;
  upsert(version: string, checkpoint: import("@x-sage/core").Checkpoint): Promise<"committed" | "stale">;
}

interface NativeProposalView {
  lists: ProposalLists;
  readBody(conceptId: string): Promise<Pick<BodyReadback, "body" | "revision">>;
}

/** Store reads available to the Bun service for its configured database. */
export interface NativeProposalSubmitter {
  edge(input: import("./write.ts").ProposeEdgeInput): Promise<string>;
  atom(input: import("./write.ts").ProposeAtomInput): Promise<string>;
  body(input: import("./write.ts").ProposeBodyInput): Promise<string>;
}

export interface NativeTermReader {
  readonly kbSlug: string;
  submitProposal?: NativeProposalSubmitter;
  readonly kbName?: string;
  publishArtifacts?(version: string, files: { graph?: string; manifest?: string }): Promise<void>;
  readonly frontier?: {
    read(): Promise<{ revision: bigint; raw: string | null }>;
    /** False means the known old revision was definitely not applied. */
    replace(expected: bigint, document: string): Promise<boolean>;
  };
  checkpoints?: NativeCheckpointStore;
  readGraphSnapshot?(includeBodies?: boolean): Promise<{
    graph: Graph;
    manifest: ManifestRow[];
    version?: string;
    writeBasis?: NativeGraphWriteBasis;
    bodies?: ReadonlyMap<string, string>;
  }>;
  createConcept?(
    concept: ConceptFrontmatter,
    body: string,
    basis: NativeGraphWriteBasis,
  ): Promise<"committed" | "duplicate" | "conflict" | "invalid" | "unknown">;
  readActivation?(slug: string): Promise<({ graph: Graph } & Pick<Activation, "raw" | "molecule" | "triggers">) | null>;
  readProposalFiles?(): Promise<NativeProposalView>;
  readonly proposalMerge?: ProposalMergePort;
  readTermText(slug: string): Promise<string | null>;
  readTermDocument(slug: string): Promise<{ body: string; frontmatter: ConceptFrontmatter } | null>;
}

export type ApiMethod = "GET" | "POST" | "PUT";

export interface ApiRequest {
  method: ApiMethod;
  /** Path below `/api`, e.g. `/concepts/decision/edges`. Always starts with `/`. */
  path: string;
  query: Record<string, string | undefined>;
  /** Parsed JSON body for POST/PUT. */
  body?: unknown;
}

export type { ApiResponse } from "./http.ts";

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

function requireConceptUpload(request: ApiRequest): {
  meta: ConceptFrontmatter;
  molecule: string;
} {
  const body = requireBody(request);
  const parsed = ConceptFrontmatterSchema.safeParse(body.frontmatter);
  if (!parsed.success || !ConceptIdSchema.safeParse(parsed.data.id).success) {
    throw new ApiError(400, "Invalid concept metadata or id");
  }
  const molecule = requireField(body, "body");
  if (!molecule.trim()) throw new ApiError(400, "Concept body cannot be empty");
  return { meta: parsed.data, molecule };
}

function validateNewConceptGraph(graph: Graph, meta: ConceptFrontmatter) {
  const proposed = {
    nodes: [...graph.nodes, { ...meta, filePath: "" }],
    edges: [...graph.edges, ...meta.edges.map((edge) => ({ ...edge, from: meta.id }))],
  };
  const issues = validateGraph(proposed).issues.filter((issue) => issue.type !== "orphan-node");
  if (issues.length) throw new ApiError(400, issues.map((issue) => issue.message).join("; "));
}

function optionalField(body: Record<string, unknown>, field: string): string | undefined {
  const value = body[field];
  return typeof value === "string" && value !== "" ? value : undefined;
}

async function frontierRoute(
  request: ApiRequest,
  format: ReturnType<typeof parseFormat>,
  read: () => Promise<FrontierEntry[]>,
  append: (entry: FrontierEntry) => Promise<void>,
): Promise<ApiResponse> {
  if (request.method === "GET") {
    const entries = await read();
    return format === "text" ? text(200, renderFrontier(entries)) : json(200, entries);
  }

  const body = requireBody(request);
  const priority = optionalField(body, "priority") ?? "medium";
  if (priority !== "high" && priority !== "medium" && priority !== "low") {
    throw new ApiError(400, `Invalid priority: ${priority}. Must be high, medium or low`);
  }
  const entry = {
    concept: requireField(body, "concept"),
    source: requireField(body, "source"),
    reason: requireField(body, "reason"),
    priority,
  } satisfies FrontierEntry;
  await append(entry);
  if (format === "text") {
    return text(201, `Added frontier entry: ${entry.concept} (${entry.priority})\n`);
  }
  return json(201, entry);
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
  nativeTermReader?: NativeTermReader,
): Promise<ApiResponse> {
  const { method, path, query } = request;
  const format = parseFormat(query, accept);

  try {
    if (nativeTermReader) {
      if (method === "POST" && path === "/proposals/merge" && nativeTermReader.proposalMerge) {
        if (query.kb !== undefined && query.kb !== nativeTermReader.kbSlug) {
          throw new ApiError(404, "Unknown knowledge base");
        }
        const body = request.body === undefined ? {} : requireBody(request);
        const target = optionalField(body, "file") ?? "all";
        try {
          return mergeResponse(await mergeProposalFiles(nativeTermReader.proposalMerge, target), format);
        } catch (error) {
          if (error instanceof ProposalTargetNotFoundError) throw error;
          if (error instanceof ProposalMergeUnresolvedError) {
            if (format !== "text") return json(503, { ...error.partial, error: error.message });
            const { merged, failed, conflicts } = error.partial;
            const prefix = merged.length + failed.length + conflicts.length > 0 ? renderMergeOutcome(error.partial) : "";
            return text(503, `${prefix}${error.message}\n`);
          }
          throw new ApiError(503, "Database unavailable");
        }
      }

      // An incomplete adapter fails closed. Unconnected operations cannot reach
      // an older authority when this database connection is attached.
      if (method === "POST" && path === "/proposals" && nativeTermReader.submitProposal) {
        if (query.kb !== undefined && query.kb !== nativeTermReader.kbSlug) {
          throw new ApiError(404, "Unknown knowledge base");
        }
        return await submitProposalRoute(ctx, request, format, nativeTermReader.submitProposal);
      }
      if (method === "POST" && path === "/concepts" && nativeTermReader.createConcept && nativeTermReader.readGraphSnapshot) {
        if (query.kb !== undefined && query.kb !== nativeTermReader.kbSlug) {
          throw new ApiError(404, "Unknown knowledge base");
        }
        const { meta, molecule } = requireConceptUpload(request);
        try {
          const { graph, writeBasis } = await nativeTermReader.readGraphSnapshot();
          if (!writeBasis) throw new ApiError(503, "Database unavailable");
          if (writeBasis.terms.has(meta.id)) throw new ApiError(409, "Concept already exists");
          validateNewConceptGraph(graph, meta);
          const outcome = await nativeTermReader.createConcept(meta, molecule + "\n", writeBasis);
          switch (outcome) {
            case "committed":
              return json(201, { id: meta.id });
            case "duplicate":
              throw new ApiError(409, "Concept already exists");
            case "conflict":
              throw new ApiError(409, "Knowledge base changed; retry concept creation");
            case "invalid":
              throw new ApiError(400, "Concept creation refused by database");
            case "unknown":
              throw new ApiError(503, "Creation outcome is unknown; check the concept before retrying");
          }
        } catch (error) {
          if (error instanceof ApiError) throw error;
          throw new ApiError(503, "Database unavailable");
        }
      }
      if (path === "/knowledge-bases" && method === "GET" && nativeTermReader.kbName !== undefined) {
        return json(200, { knowledgeBases: [{
          id: nativeTermReader.kbSlug,
          name: nativeTermReader.kbName,
          isDefault: true,
        }] });
      }
      if (method === "PUT" && (path === "/graph" || path === "/manifest") &&
          nativeTermReader.publishArtifacts && nativeTermReader.readGraphSnapshot) {
        if (query.kb !== undefined && query.kb !== nativeTermReader.kbSlug) {
          throw new ApiError(404, "Unknown knowledge base");
        }
        const regenerateManifest = path === "/manifest" ||
          (request.body !== undefined && requireBody(request).manifest === true);
        try {
          const { graph, manifest, version } = await nativeTermReader.readGraphSnapshot();
          if (version === undefined) throw new ApiError(503, "Database unavailable");
          await nativeTermReader.publishArtifacts(version, {
            ...(path === "/graph" ? { graph: JSON.stringify(graph, null, 2) } : {}),
            ...(regenerateManifest ? { manifest: formatManifest(manifest) } : {}),
          });
          if (path === "/manifest") return manifestRebuilt(format, manifest.length);
          return graphRebuilt(format, graph, regenerateManifest ? manifest.length : null);
        } catch (error) {
          if (error instanceof ApiError) throw error;
          throw new ApiError(503, "Projection regeneration unavailable");
        }
      }
      const checkpointMatch = /^\/checkpoints\/([^/]+)$/.exec(path);
      if (checkpointMatch && nativeTermReader.checkpoints) {
        if (query.kb !== undefined && query.kb !== nativeTermReader.kbSlug) {
          throw new ApiError(404, "Unknown knowledge base");
        }
        const checkpointSlug = decodeURIComponent(checkpointMatch[1]!);
        try {
          return await checkpointRoute(ctx, request, format, checkpointSlug, nativeTermReader.checkpoints);
        } catch (error) {
          if (error instanceof ApiError) throw error;
          throw new ApiError(503, "Database unavailable");
        }
      }
      const match = /^\/concepts\/([^/]+)\/body$/.exec(path);
      const edgeMatch = /^\/concepts\/([^/]+)\/edges$/.exec(path);
      const activationMatch = /^\/concepts\/([^/]+)$/.exec(path);
      const proposalMatch = /^\/proposals\/([^/]+)$/.exec(path);
      const proposalRead = path === "/proposals" || proposalMatch !== null;
      const canReadProposals = proposalRead && nativeTermReader.readProposalFiles !== undefined;
      const graphRead = edgeMatch !== null || ["/graph", "/manifest", "/search", "/meta", "/path", "/validate", "/viz"].includes(path);
      const frontier = path === "/frontier" && (method === "GET" || method === "POST")
        ? nativeTermReader.frontier
        : undefined;
      const canActivate = activationMatch !== null && nativeTermReader.readActivation !== undefined;
      if (!frontier && (method !== "GET" || (!match && !canActivate && !canReadProposals && (!graphRead || !nativeTermReader.readGraphSnapshot)))) {
        throw new ApiError(503, "Operation is not connected to x-sage-db");
      }
      if (query.kb !== undefined && query.kb !== nativeTermReader.kbSlug) {
        throw new ApiError(404, "Unknown knowledge base");
      }
      const slug = match && decodeURIComponent(match[1]!);
      const edgeSlug = edgeMatch && decodeURIComponent(edgeMatch[1]!);
      const activationSlug = activationMatch && decodeURIComponent(activationMatch[1]!);
      const proposalFile = proposalMatch && decodeURIComponent(proposalMatch[1]!);
      try {
        if (proposalRead && nativeTermReader.readProposalFiles) {
          const view = await nativeTermReader.readProposalFiles();
          if (proposalFile !== null) return await proposalDetail(ctx, proposalFile, format, view);
          return format === "text" ? text(200, renderProposals(view.lists)) : json(200, view.lists);
        }
        if (activationSlug !== null && nativeTermReader.readActivation) {
          return await conceptActivation(ctx, activationSlug, query, format, nativeTermReader.readActivation.bind(nativeTermReader));
        }
        if (frontier) {
          return await frontierRoute(
            request,
            format,
            async () => parseFrontierMarkdown((await frontier.read()).raw ?? ""),
            async (entry) => {
              for (let attempt = 0; attempt < 5; attempt++) {
                const { revision, raw } = await frontier.read();
                if (raw === null) throw new ApiError(404, `frontier.md not found in ${ctx.root}`);
                let next: string;
                try {
                  next = appendFrontierRow(raw, entry);
                } catch (error) {
                  const message = error instanceof Error ? error.message : String(error);
                  throw new ApiError(400, message);
                }
                if (next === raw || await frontier.replace(revision, next)) return;
              }
              throw new ApiError(503, "Frontier document is busy; retry the write");
            },
          );
        }
        if (graphRead && nativeTermReader.readGraphSnapshot) {
          if (edgeSlug !== null) {
            return await conceptEdges(ctx, edgeSlug, query, format, async () => {
              const snapshot = await nativeTermReader.readGraphSnapshot!();
              return snapshot.graph;
            });
          }
          const keyword = query.q;
          if (path === "/search" && (keyword === undefined || keyword === "")) {
            throw new ApiError(400, "Missing query parameter: q");
          }
          const { from, to } = query;
          if (path === "/path" && (from === undefined || to === undefined)) {
            throw new ApiError(400, "Missing query parameters: from, to");
          }
          const includeGraph = boolParam(query, "graph");
          if (path === "/validate" && query.format === "graph" && !includeGraph) {
            throw new ApiError(400, "format=graph requires graph=true");
          }
          const { graph, manifest, bodies } = await nativeTermReader.readGraphSnapshot(path === "/viz" && format !== "text");
          if (path === "/meta") {
            return json(200, { kb: ctx.root, nodes: graph.nodes.length, edges: graph.edges.length });
          }
          if (path === "/path" && from !== undefined && to !== undefined) {
            const result = pathInGraph(graph, from, to);
            return format === "text" ? text(200, renderPath(result)) : json(200, result);
          }
          if (path === "/validate") {
            const report = validateGraphSnapshot(graph, includeGraph);
            const status = report.valid ? 200 : 422;
            if (query.format === "graph" && report.graph) return text(status, renderGraphStatus(report.graph));
            return format === "text" ? text(status, renderValidationReport(report, ctx.root)) : json(status, report);
          }
          if (path === "/viz") {
            if (format === "text") return text(200, renderVizStats(vizFacts(graph.nodes, graph.edges)));
            if (bodies === undefined) throw new Error("Missing database bodies");
            return json(200, vizFromGraph(graph, manifest, bodies));
          }
          if (path === "/graph") {
            return json(200, boolParam(query, "semantic") ? semanticGraph(graph) : graph);
          }
          if (path === "/manifest") {
            const markdown = formatManifest(manifest);
            return format === "json" ? json(200, { markdown }) : text(200, `${markdown}\n`, MARKDOWN_TYPE);
          }
          if (keyword === undefined) throw new ApiError(400, "Missing query parameter: q");
          const results = searchManifest(manifest, keyword);
          if (format === "manifest") return text(200, `${formatManifest(results)}\n`, MARKDOWN_TYPE);
          if (format === "text") return text(200, renderSearchResults(keyword, results));
          return json(200, { keyword, results });
        }
        if (slug === null) throw new ApiError(503, "Operation is not connected to x-sage-db");
        if (format === "markdown" || format === "text") {
          const body = await nativeTermReader.readTermText(slug);
          if (body === null) throw new ApiError(404, `Concept not found: ${slug}`);
          return text(200, body, MARKDOWN_TYPE);
        }
        const record = await nativeTermReader.readTermDocument(slug);
        if (record === null) throw new ApiError(404, `Concept not found: ${slug}`);
        return json(200, {
          conceptId: record.frontmatter.id,
          body: record.body,
          revision: conceptRevision(record.frontmatter, record.body),
          frontmatter: record.frontmatter,
          atom: record.frontmatter.atom,
        });
      } catch (error) {
        if (error instanceof ApiError || error instanceof ProposalTargetNotFoundError) throw error;
        throw new ApiError(503, "Database unavailable");
      }
    }
    const entries = path === '/knowledge-bases' || query.kb !== undefined ? await catalog(ctx) : [];
    if (path === '/knowledge-bases' && method === 'GET') return json(200, { knowledgeBases: entries.map(({id,name,root}) => ({id,name,isDefault:root === ctx.root})) });
    const selected = query.kb === undefined ? undefined : entries.find(entry => entry.id === query.kb);
    if (query.kb !== undefined && !selected) throw new ApiError(404, 'Unknown knowledge base');
    const scoped = selected ? { root: selected.root } : ctx;
    await requireMarkdownSource(scoped);
    return await route(scoped, method, path, query, request, format);
  } catch (err) {
    // Status is chosen from the error type first. The old fallback matched on
    // the word "not found" in the message, which meant a conflict or a bad
    // request had to be phrased to look like a 404 to get one — and a typo'd
    // merge target silently reported success.
    const status =
      err instanceof CatalogUnavailableError || err instanceof LockTimeoutError
        ? 503
        : err instanceof ApiError
        ? err.status
        : err instanceof ProposalTargetNotFoundError
          ? 404
          : err instanceof ProposalConflictError
            ? 409
            : (err instanceof Error ? err.message : String(err)).includes("not found")
              ? 404
              : 400;
    const message =
      err instanceof LockTimeoutError
        ? "Knowledge base is busy; retry after the current write completes"
        : err instanceof ApiError
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

function mergeResponse(outcome: MergeOutcome, format: Format): ApiResponse {
  // The CLI treats conflicts as 409 and failures as 422 even after some approvals.
  const status = outcome.conflicts.length > 0 ? 409 : outcome.failed.length > 0 ? 422 : 200;
  return format === "text" ? text(status, renderMergeOutcome(outcome)) : json(status, outcome);
}

async function route(
  ctx: KbContext,
  method: ApiMethod,
  path: string,
  query: ApiRequest["query"],
  request: ApiRequest,
  format: Format,
): Promise<ApiResponse> {
  if (path === '/concepts' && method === 'POST') {
    const { meta, molecule } = requireConceptUpload(request);
    // Share the proposal merge lock so creation and merge cannot interleave.
    // Do not steal a lock solely because an unusually slow writer took >30s.
    return withKbLock(ctx.root, async () => {
    const existing = await loadConcepts(ctx);
    if (existing.concepts.some(c => c.file.frontmatter.id === meta.id)) throw new ApiError(409, 'Concept already exists');
    const graph = await buildGraph(ctx);
    validateNewConceptGraph(graph, meta);
    const dir = join(ctx.root, 'concepts', LAYERS[meta.layer]!);
    await mkdir(dir,{recursive:true});
    const temp = join(dir, '.'+randomUUID()+'.tmp');
    try {
      await writeFile(temp, '---\n'+JSON.stringify(meta,null,2)+'\n---\n'+molecule+'\n',{flag:'wx'});
      await link(temp,join(dir,meta.id+'.md'));
    } catch(error) {
      if ((error as {code?:string}).code === 'EEXIST') throw new ApiError(409,'Concept already exists');
      throw error;
    } finally { await unlink(temp).catch(()=>{}); }
    // The source concept is committed. Report a repairable derived-artifact
    // failure without falsely telling the caller creation failed.
    try { await writeGraph(ctx); await writeManifest(ctx); }
    catch { return json(201,{id:meta.id, warning:'Concept created; rebuild graph and manifest to repair generated artifacts'}); }
    return json(201,{id:meta.id});
    }, { staleAfterMs: Number.POSITIVE_INFINITY });
  }
  // ---- identity ----
  if (path === "/health" && method === "GET") {
    return json(200, { ok: true, authority: "markdown", kb: ctx.root });
  }

  if (path === "/meta" && method === "GET") {
    const graph = await buildGraph(ctx);
    return json(200, { kb: ctx.root, authority: "markdown", nodes: graph.nodes.length, edges: graph.edges.length });
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
    return manifestRebuilt(format, result.count);
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
    return graphRebuilt(format, graph, manifest?.count ?? null);
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

  // ---- concept body (read + propose) ----
  const bodyMatch = /^\/concepts\/([^/]+)\/body$/.exec(path);
  if (bodyMatch !== null) {
    const conceptId = decodeURIComponent(bodyMatch[1]!);
    if (method === "GET") {
      const result = await readBody(ctx, conceptId);
      // `--format markdown` is the body alone: a reviewer diffing a candidate
      // against the live file wants prose, not a frontmatter block they were
      // told not to touch.
      if (format === "markdown" || format === "text") {
        return text(200, result.body, MARKDOWN_TYPE);
      }
      return json(200, result);
    }
    throw new ApiError(405, "Method Not Allowed: use GET /concepts/:id/body");
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

  if (path === "/frontier" && (method === "GET" || method === "POST")) {
    return frontierRoute(request, format, () => listFrontier(ctx), (entry) => appendFrontier(ctx, entry));
  }

  // ---- one proposal, with its diff ----
  const proposalMatch = /^\/proposals\/([^/]+)$/.exec(path);
  if (proposalMatch !== null && method === "GET") {
    const file = decodeURIComponent(proposalMatch[1]!);
    return proposalDetail(ctx, file, format);
  }

  // ---- proposals ----
  if (path === "/proposals" && method === "GET") {
    const lists = await listProposals(ctx);
    return format === "text" ? text(200, renderProposals(lists)) : json(200, lists);
  }

  if (path === "/proposals/merge" && method === "POST") {
    const body = request.body === undefined ? {} : requireBody(request);
    return mergeResponse(await mergeProposals(ctx, optionalField(body, "file") ?? "all"), format);
  }

  // One collection, two kinds. The kind travels in the body as `kind`, so the
  // path names a resource instead of an action.
  if (path === "/proposals" && method === "POST") {
    return await submitProposalRoute(ctx, request, format);
  }

  const checkpointMatch = /^\/checkpoints\/([^/]+)$/.exec(path);
  if (checkpointMatch) return checkpointRoute(ctx, request, format, decodeURIComponent(checkpointMatch[1]!));

  throw new ApiError(404, "Not Found");
}

function manifestRebuilt(format: Format, count: number): ApiResponse {
  if (format === "text") return text(200, `MANIFEST.md written (${count} concepts).\n`);
  return json(200, { concepts: count });
}

function graphRebuilt(format: Format, graph: Graph, manifestCount: number | null): ApiResponse {
  if (format === "text") {
    const lines = [`Generated graph.json: ${graph.nodes.length} nodes, ${graph.edges.length} edges.`];
    if (manifestCount !== null) lines.push(`MANIFEST.md written (${manifestCount} concepts).`);
    return text(200, `${lines.join("\n")}\n`);
  }
  return json(200, {
    nodes: graph.nodes.length,
    edges: graph.edges.length,
    manifest: manifestCount === null ? null : { concepts: manifestCount },
  });
}

async function checkpointRoute(
  ctx: KbContext,
  request: ApiRequest,
  format: Format,
  concept: string,
  native?: NativeCheckpointStore,
): Promise<ApiResponse> {
  const { method, query } = request;
  if (!ConceptIdSchema.safeParse(concept).success) throw new ApiError(400, "Invalid checkpoint concept id");
  if (query.session !== undefined && (!/^[1-9][0-9]*$/.test(query.session) || !Number.isSafeInteger(Number(query.session)))) {
    throw new ApiError(400, "Invalid checkpoint session");
  }

  if (method === "GET") {
    const session = intParam(query, "session");
    const checkpoint = native
      ? (await native.inspect(concept, session, true)).checkpoint
      : await getCheckpoint(ctx, concept, session);
    if (checkpoint == null) throw new ApiError(404, `No checkpoints found for: ${concept}`);
    return format === "text" ? text(200, renderCheckpoint(checkpoint)) : json(200, checkpoint);
  }

  if (method === "POST") {
    const body = request.body === undefined ? {} : requireBody(request);
    const session = body.sessionNumber;
    if (session !== undefined && (typeof session !== "number" || !Number.isSafeInteger(session) || session < 1)) {
      throw new ApiError(400, "Invalid checkpoint session");
    }
    if (!native) return json(201, await createCheckpoint(ctx, concept, session));
    const checkpoint = await writeNativeCheckpoint(native, concept, session, (ordinal) => emptyCheckpoint(concept, ordinal));
    return json(201, checkpoint);
  }

  if (method === "PUT") {
    const body = requireBody(request);
    if (body.concept !== concept) {
      throw new ApiError(400, `Body concept "${String(body.concept)}" does not match path concept "${concept}"`);
    }
    const checkpoint = CheckpointSchema.safeParse(body);
    if (!checkpoint.success) {
      const first = checkpoint.error.issues[0];
      throw new ApiError(
        400,
        first === undefined ? "Invalid checkpoint body" : `Invalid checkpoint.${first.path.join(".") || "(root)"}: ${first.message}`,
      );
    }
    if (native) {
      await writeNativeCheckpoint(native, concept, checkpoint.data.session, () => checkpoint.data);
    } else {
      await saveCheckpoint(ctx, checkpoint.data);
    }
    return json(200, { ok: true });
  }

  throw new ApiError(404, "Not Found");
}

async function writeNativeCheckpoint(
  store: NativeCheckpointStore,
  concept: string,
  session: number | undefined,
  payload: (ordinal: number) => import("@x-sage/core").Checkpoint,
): Promise<import("@x-sage/core").Checkpoint> {
  for (let attempt = 0; attempt < 3; attempt++) {
    const observation = await store.inspect(concept, session, false);
    const ordinal = session ?? (observation.session ?? 0) + 1;
    if (!Number.isSafeInteger(ordinal) || ordinal < 1) throw new ApiError(409, "Checkpoint sessions exhausted");
    const checkpoint = payload(ordinal);
    if (await store.upsert(observation.version, checkpoint) === "committed") return checkpoint;
  }
  throw new ApiError(503, "Knowledge base is busy; retry after the current write completes");
}

async function conceptActivation(
  ctx: KbContext,
  conceptId: string,
  query: ApiRequest["query"],
  format: Format,
  readActivation?: NativeTermReader["readActivation"],
): Promise<ApiResponse> {
  const strategy = query.strategy;
  if (strategy !== undefined && !isActivationStrategy(strategy)) {
    throw new ApiError(400, `Invalid strategy: ${strategy}. Must be exploration, validation or generation`);
  }

  const depth = intParam(query, "depth");
  if (depth !== undefined && depth < 1) {
    throw new ApiError(400, `Invalid depth: ${depth}`);
  }

  const options = {
    ...(strategy === undefined ? {} : { strategy }),
    ...(depth === undefined ? {} : { depth }),
  };
  let result: Activation;
  if (readActivation !== undefined) {
    const source = await readActivation(conceptId);
    if (source === null) throw new ApiError(404, `Concept not found: ${conceptId}`);
    const { graph, raw, molecule, triggers } = source;
    result = activateInGraph(graph, conceptId, { raw, molecule, triggers }, options);
  } else {
    result = await activate(ctx, conceptId, options);
  }

  if (format === "markdown") {
    // `raw` is the concept file, already newline-terminated. The old CLI printed
    // it with console.log, which added one more; consumers' diffing depends on it.
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
  loadGraph?: () => Promise<Graph>,
): Promise<ApiResponse> {
  const directionRaw = query.dir ?? "both";
  if (directionRaw !== "in" && directionRaw !== "out" && directionRaw !== "both") {
    throw new ApiError(400, `Invalid dir: ${directionRaw}. Must be in, out or both`);
  }
  const direction: "in" | "out" | "both" = directionRaw;

  let type: EdgeType | undefined;
  const typeRaw = query.type;
  if (typeRaw !== undefined) {
    const parsed = EdgeTypeSchema.safeParse(typeRaw);
    if (!parsed.success) {
      throw new ApiError(400, `Invalid edge type: ${typeRaw}. Must be one of: ${EDGE_TYPES.join(", ")}`);
    }
    type = parsed.data as EdgeType;
  }

  // Symmetric completion always applies so one asserted tension is visible from
  // both ends; closure is opt-in because it is inference, not authorship.
  // Starting from the semantic graph would keep closure edges even when they
  // were not asked for, so the two views are built from their own base.
  const derived = boolParam(query, "derived");
  const edgeQuery = { ...(type === undefined ? {} : { type }), direction };

  const graph = await (loadGraph ? loadGraph() : buildGraph(ctx));
  if (!graph.nodes.some((node) => node.id === conceptId)) {
    throw new ApiError(404, `Concept not found: ${conceptId}`);
  }
  const view = derived
    ? semanticGraph(graph)
    : { nodes: graph.nodes, edges: completeSymmetricEdges(graph).edges };
  const hits = queryEdges(view, conceptId, edgeQuery);

  return format === "text"
    ? text(200, renderEdges(conceptId, hits))
    : json(200, { id: conceptId, direction, edges: hits });
}

async function submitProposalRoute(
  ctx: KbContext,
  request: ApiRequest,
  format: Format,
  native?: NativeProposalSubmitter,
): Promise<ApiResponse> {
  const body = requireBody(request);
  const kind = optionalField(body, "kind") ?? "edge";
  if (kind === "edge") return proposeEdge(ctx, body, format, native);
  if (kind === "atom") return proposeAtom(ctx, body, format, native);
  if (kind === "body") return proposeBodyEdit(ctx, body, format, native);
  throw new ApiError(400, `Unknown proposal kind: ${kind}. Must be edge, atom or body`);
}

async function proposeEdge(
  ctx: KbContext,
  body: Record<string, unknown>,
  format: Format,
  native?: NativeProposalSubmitter,
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
  const input = {
    from,
    type: parsedType.data as EdgeType,
    target,
    confidence: parsedConfidence.data as Confidence,
  };
  const filename = await (native ? native.edge(input) : submitEdgeProposal(ctx, input));

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
  native?: NativeProposalSubmitter,
): Promise<ApiResponse> {
  const reason = optionalField(body, "reason");
  const conceptId = requireField(body, "conceptId");
  const proposedAtom = requireField(body, "proposedAtom");
  const input = {
    conceptId,
    proposedAtom,
    ...(reason === undefined ? {} : { reason }),
  };
  const filename = await (native ? native.atom(input) : submitAtomProposal(ctx, input));

  if (format === "text") {
    return text(201, `Proposed atom update for: ${conceptId}\nWritten to: proposals/atoms/${filename}\n`);
  }
  return json(201, { file: filename });
}

/**
 * Submit a body proposal.
 *
 * The revision is required rather than derived server-side: a proposal is only
 * meaningful relative to the version its author read, and silently rebasing it
 * would apply a body written against different text.
 */
async function proposeBodyEdit(
  ctx: KbContext,
  body: Record<string, unknown>,
  format: Format,
  native?: NativeProposalSubmitter,
): Promise<ApiResponse> {
  const conceptId = requireField(body, "conceptId");
  const expectedRevision = requireField(body, "expectedRevision");
  const proposedBody = requireField(body, "proposedBody");
  const reason = requireField(body, "reason");

  const input = {
    conceptId,
    expectedRevision,
    proposedBody,
    reason,
  };
  const filename = await (native ? native.body(input) : submitBodyProposal(ctx, input));

  if (format === "text") {
    return text(201, `Proposed body update for: ${conceptId}\nWritten to: proposals/bodies/${filename}\n`);
  }
  return json(201, { file: filename, conceptId, expectedRevision });
}

/**
 * One proposal plus its diff.
 *
 * Body proposals diff against the live concept body; edge and atom proposals
 * have no two-sided text, so they return their fields and an empty diff. The
 * diff is computed per request instead of stored: the concept may have moved
 * since the proposal was written, and a stale diff would review text that no
 * longer exists.
 */
async function proposalDetail(
  ctx: KbContext,
  file: string,
  format: Format,
  view?: NativeProposalView,
): Promise<ApiResponse> {
  let proposal: BodyProposal;
  try {
    if (view === undefined) {
      proposal = await readBodyProposal(proposalsDir(ctx), file);
    } else {
      const body = view.lists.bodies.find((candidate) => candidate.file === file);
      if (body === undefined) throw new ProposalTargetNotFoundError(file);
      proposal = body;
    }
  } catch {
    // Not a body proposal: report it through the existing list so a caller can
    // tell "wrong kind" from "no such file".
    const lists = view?.lists ?? await listProposals(ctx);
    const known = [...lists.edges, ...lists.atoms].find((candidate) => candidate.file === file);
    if (known === undefined) throw new ProposalTargetNotFoundError(file);
    return format === "text"
      ? text(200, `Proposal ${file} has no body diff.\n`)
      : json(200, { file, kind: "proposedAtom" in known ? "atom" : "edge", proposal: known });
  }

  const live = await (view ? view.readBody(proposal.conceptId) : readBody(ctx, proposal.conceptId));
  const hunks = diffBodies(live.body, proposal.proposedBody);
  const diff = formatUnifiedDiff(hunks, `${proposal.conceptId} (current)`, `${proposal.conceptId} (proposed)`);

  if (format === "text") {
    return text(
      200,
      [
        `Proposal: ${file}`,
        `Concept:  ${proposal.conceptId}`,
        `Reason:   ${proposal.reason}`,
        `Status:   ${proposal.status}`,
        `Revision: ${proposal.expectedRevision.slice(0, 12)} (current ${live.revision.slice(0, 12)})`,
        "",
        diff,
      ].join("\n"),
    );
  }
  return json(200, {
    file,
    kind: "body",
    proposal,
    currentRevision: live.revision,
    applies: proposal.expectedRevision === live.revision,
    diff,
    hunks,
  });
}

/** Re-exported so the server can render a graph-only report without a KB. */
export { renderGraphSection };
