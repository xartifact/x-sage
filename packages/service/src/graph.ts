import {
  GraphSchema,
  type Graph,
  type GraphNode,
  type GraphEdge,
  completeSymmetricEdges,
  transitiveClosure,
  validateGraph,
  formatZodError,
  type GraphValidationIssue,
} from "@x-sage/core";
import { generateGraph as coreGenerateGraph } from "@x-sage/core/graph";
import { readFile, writeFile } from "node:fs/promises";
import { graphFile, type KbContext } from "./kb.ts";

/**
 * Build the graph from concept files.
 *
 * `@x-sage/core`'s `generateGraph` takes a raw filesystem path; this wraps it so
 * callers stay on `KbContext` like every other service function.
 */
export async function buildGraph(ctx: KbContext): Promise<Graph> {
  try {
    return await coreGenerateGraph(ctx.root);
  } catch (err) {
    // core parses with zod directly, so a bad file would otherwise surface as a
    // serialized ZodError object. Normalise it to one readable line.
    const formatted = formatZodError(err);
    if (formatted !== null) {
      throw new Error(formatted);
    }
    throw err;
  }
}

/** Read the generated `graph.json`, or null when it has never been generated. */
export async function readStoredGraph(ctx: KbContext): Promise<Graph | null> {
  try {
    const raw = await readFile(graphFile(ctx), "utf-8");
    return GraphSchema.parse(JSON.parse(raw));
  } catch {
    return null;
  }
}

/** Regenerate and persist `graph.json`. */
export async function writeGraph(ctx: KbContext): Promise<Graph> {
  const graph = await buildGraph(ctx);
  await writeFile(graphFile(ctx), JSON.stringify(graph, null, 2), "utf-8");
  return graph;
}

/**
 * The graph with derived relations materialised.
 *
 * Symmetric relations are mirrored and acyclic relations are closed
 * transitively. Reasoning code (activation, queries) uses this view; anything
 * that reports what an author wrote uses the asserted graph instead.
 */
export function semanticGraph(graph: Graph): Graph {
  return transitiveClosure(completeSymmetricEdges(graph));
}

export interface EdgeHit {
  direction: "in" | "out" | "both";
  type: GraphEdge["type"];
  confidence: GraphEdge["confidence"];
  other: GraphNode;
  derived: boolean;
}

/**
 * Edges touching one concept, optionally filtered by relation type and direction.
 *
 * Symmetric relations are collapsed: the semantic view materialises both
 * directions, so without deduplication one asserted tension appears twice.
 * Those hits are reported as `direction: "both"`, because labelling a mirrored
 * edge "out" would claim the author wrote something they did not.
 */
export function queryEdges(
  graph: Graph,
  conceptId: string,
  options: { type?: GraphEdge["type"]; direction?: "in" | "out" | "both" } = {},
): EdgeHit[] {
  const byId = new Map(graph.nodes.map((node) => [node.id, node]));
  const seen = new Set<string>();
  const hits: EdgeHit[] = [];
  const direction = options.direction ?? "both";

  for (const edge of graph.edges) {
    if (options.type !== undefined && edge.type !== options.type) continue;

    const outgoing = edge.from === conceptId;
    const incoming = edge.target === conceptId;
    if (!outgoing && !incoming) continue;

    const otherId = outgoing ? edge.target : edge.from;
    if (otherId === conceptId) continue;
    const other = byId.get(otherId);
    if (!other) continue;

    const symmetric = edge.type === "contradicts" || edge.type === "analogous-to";
    if (symmetric) {
      const key = `${edge.type}\u0000${otherId}`;
      if (seen.has(key)) continue;
      seen.add(key);
    }

    hits.push({
      direction: symmetric ? "both" : outgoing ? "out" : "in",
      type: edge.type,
      confidence: edge.confidence,
      other,
      derived: edge.derived !== undefined,
    });
  }

  return hits
    .filter((hit) => direction === "both" || hit.direction === "both" || hit.direction === direction)
    .sort(
      (a, b) =>
        a.direction.localeCompare(b.direction) ||
        a.type.localeCompare(b.type) ||
        a.other.id.localeCompare(b.other.id),
    );
}

export interface GraphValidationReport {
  /** Problems in the asserted graph: dangling edges, cycles, layer violations. */
  issues: GraphValidationIssue[];
  /** Null when graph.json was never generated. */
  staleness: {
    missing: string[];
    extra: string[];
    nodeDrift: boolean;
    /** Set when the graph could not be rebuilt at all (e.g. a broken file). */
    error?: string;
  } | null;
  valid: boolean;
}

const edgeIdentity = (edge: GraphEdge): string =>
  `${edge.from}\u0000${edge.type}\u0000${edge.target}\u0000${edge.confidence}`;

/**
 * Validate the graph and check that `graph.json` still matches the concept files.
 *
 * Staleness matters because `graph.json` is generated: a stale file means every
 * later check is reasoning about a snapshot the sources no longer describe.
 */
export async function validateGraphState(ctx: KbContext): Promise<GraphValidationReport> {
  const stored = await readStoredGraph(ctx);

  // A concept file that fails to parse makes the regenerated graph unbuildable.
  // Report that as a blocking problem instead of letting the parser's error
  // escape: `validate` exists to enumerate what is wrong, not to die on the
  // first bad file.
  let regenerated: Graph;
  try {
    regenerated = await buildGraph(ctx);
  } catch (err) {
    return {
      issues: [],
      staleness: {
        missing: [],
        extra: [],
        nodeDrift: true,
        error: err instanceof Error ? err.message : String(err),
      },
      valid: false,
    };
  }

  if (stored === null) {
    return {
      issues: [],
      staleness: { missing: [], extra: [], nodeDrift: true },
      valid: false,
    };
  }

  const onDisk = new Set(stored.edges.map(edgeIdentity));
  const fromFiles = new Set(regenerated.edges.map(edgeIdentity));
  const missing = [...fromFiles].filter((key) => !onDisk.has(key));
  const extra = [...onDisk].filter((key) => !fromFiles.has(key));
  const nodeDrift =
    stored.nodes.length !== regenerated.nodes.length ||
    stored.nodes.some((node) => !regenerated.nodes.some((other) => other.id === node.id));

  const issues = validateGraph(stored).issues;
  const stale = missing.length > 0 || extra.length > 0 || nodeDrift;

  return {
    issues,
    staleness: {
      missing: missing.map((key) => key.replaceAll("\u0000", " ")),
      extra: extra.map((key) => key.replaceAll("\u0000", " ")),
      nodeDrift,
    },
    valid: issues.length === 0 && !stale,
  };
}
