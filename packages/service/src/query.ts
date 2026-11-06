import {
  generateManifest,
  searchManifest,
  type Graph,
  type GraphNode,
  type ManifestRow,
} from "@x-sage/core";
import { traverse, hopSubgraph, shortestPath, targetsOf } from "@x-sage/core/graph";
import type { KbContext } from "./kb.ts";
import { buildGraph, semanticGraph } from "./graph.ts";
import { findConceptFile, readConceptRaw } from "./concepts.ts";

/** Relation priority per activation mode; earlier entries expand first. */
const STRATEGY_EDGE_PRIORITY = {
  exploration: ["enables", "depends-on", "contradicts", "analogous-to"],
  validation: ["contradicts", "depends-on", "enables"],
  generation: ["instantiates", "depends-on", "enables"],
} as const;

export type ActivationStrategy = keyof typeof STRATEGY_EDGE_PRIORITY;

export function isActivationStrategy(value: string): value is ActivationStrategy {
  return value in STRATEGY_EDGE_PRIORITY;
}

export interface ActivateOptions {
  strategy?: ActivationStrategy;
  /** Hops to expand. Defaults to 1, or 2 when a strategy is given. */
  depth?: number;
}

/**
 * A neighbour carried in the activation payload.
 *
 * Edges are not repeated per neighbour: every neighbour used to carry the whole
 * `sub.edges` array, which was 9 identical copies of 34 edges (28 KB of a 38 KB
 * response). The shared array lives once on `Activation.directEdges`.
 *
 * `requires` / `loadNext` are precomputed here because a client has no graph to
 * derive them from.
 */
export interface ActivatedNeighbor {
  node: GraphNode;
  requires: string[];
  loadNext: string[];
}

/**
 * Everything an activation consumer needs, and nothing more.
 *
 * Fully serialisable: no `Graph` object crosses this boundary. The semantic
 * graph stays inside this module and is reduced to the two derived lists
 * (`requires`, `loadNext`) so the same payload serves a JSON client and the
 * server-side text renderer.
 */
export interface Activation {
  concept: string;
  strategy: ActivationStrategy | null;
  depth: number;
  /** Full markdown of the activated concept. */
  raw: string;
  molecule: string;
  triggers: string[];
  node: GraphNode;
  /** Asserted edges among the activated set, for the EDGES line. */
  directEdges: Graph["edges"];
  /** Transitive prerequisites (`depends-on` closure). */
  requires: string[];
  /** Asserted navigation targets (`enables`). */
  loadNext: string[];
  neighbors: ActivatedNeighbor[];
  /** Every node id in the activated set, in BFS order. */
  visited: string[];
}

/**
 * Resolve one concept plus its neighbourhood.
 *
 * Returns data rather than text: the CLI renders an AI-oriented transcript and
 * the HTTP API returns JSON, both from this same result.
 */
export async function activate(
  ctx: KbContext,
  conceptId: string,
  options: ActivateOptions = {},
): Promise<Activation> {
  const asserted = await buildGraph(ctx);
  const semantic = semanticGraph(asserted);

  const node = asserted.nodes.find((candidate) => candidate.id === conceptId);
  if (!node) {
    throw new Error(`Concept not found: ${conceptId}`);
  }

  const concept = await findConceptFile(ctx, conceptId);
  const raw = await readConceptRaw(ctx, conceptId);

  let visited: string[];
  let depth: number;
  if (options.strategy) {
    depth = options.depth ?? 2;
    visited = traverse(semantic, [conceptId], {
      edgeTypes: [...STRATEGY_EDGE_PRIORITY[options.strategy]],
      direction: "both",
      maxDepth: depth,
    });
  } else {
    depth = options.depth ?? 1;
    visited = traverse(asserted, [conceptId], { direction: "both", maxDepth: depth });
  }

  const sub = hopSubgraph(asserted, visited, 0);
  const neighbors = sub.nodes
    .filter((candidate) => candidate.id !== conceptId)
    .map((candidate) => ({
      node: candidate,
      // Closure-based, like the activated concept: REQUIRES means "everything
      // that must be understood first", not just the asserted prerequisites.
      requires: targetsOf(semantic, candidate.id, "depends-on"),
      loadNext: targetsOf(semantic, candidate.id, "enables"),
    }));

  return {
    concept: conceptId,
    strategy: options.strategy ?? null,
    depth,
    raw,
    molecule: concept.file.molecule,
    triggers: concept.file.frontmatter.triggers,
    node,
    directEdges: sub.edges,
    requires: targetsOf(semantic, conceptId, "depends-on"),
    loadNext: targetsOf(semantic, conceptId, "enables"),
    neighbors,
    visited,
  };
}

/** Keyword lookup over the manifest. */
export async function search(ctx: KbContext, keyword: string): Promise<ManifestRow[]> {
  const rows = await generateManifest(ctx.root);
  return searchManifest(rows, keyword);
}

export interface PathResult {
  from: string;
  to: string;
  /** Ordered node ids, empty when no path exists. */
  path: string[];
  nodes: GraphNode[];
}

/** Shortest undirected path between two concepts. */
export async function findPath(ctx: KbContext, fromId: string, toId: string): Promise<PathResult> {
  const graph = await buildGraph(ctx);
  const path = shortestPath(graph, fromId, toId) ?? [];
  const nodes = path
    .map((id) => graph.nodes.find((node) => node.id === id))
    .filter((node): node is GraphNode => node !== undefined);
  return { from: fromId, to: toId, path, nodes };
}
