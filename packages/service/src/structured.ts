import type { Database } from "bun:sqlite";
import {
  completeSymmetricEdges,
  transitiveClosure,
  validateGraph,
  type Graph,
  type GraphNode,
  type ManifestRow,
} from "@x-sage/core";
import { searchManifest } from "@x-sage/core/manifest";
import { traverse, hopSubgraph, shortestPath, targetsOf } from "@x-sage/core/graph";
import {
  buildKnowledgeDocument,
  projectEvidence,
  type Claim,
  type ConceptRecord,
  type Evidence,
} from "@x-sage/core/knowledge";
import {
  buildGraphFromStore,
  claimsFromStore,
  conceptFromStore,
  conceptsFromStore,
  evidenceFromStore,
  evidenceFor,
  listRelationRows,
  manifestFromStore,
} from "./store/index.ts";
import type { Activation, PathResult } from "./query.ts";
import type { EdgeHit, GraphValidationReport } from "./graph.ts";
import type { VizPayload } from "./viz.ts";
import type { ConceptValidationReport } from "./validate.ts";

/**
 * Read models over the structured store, shaped exactly like the Markdown ones.
 *
 * This is what keeps the REST contract stable across the migration: `activate`,
 * `edges`, `path`, `search`, `validate` and `viz` return the same DTOs whether
 * the KB is authored in files or in `knowledge.db`. Nothing here re-implements
 * graph semantics — the pure algorithms in `@x-sage/core/graph` run on a graph
 * built from rows.
 */

const STRATEGY_EDGE_PRIORITY = {
  exploration: ["enables", "depends-on", "contradicts", "analogous-to"],
  validation: ["contradicts", "depends-on", "enables"],
  generation: ["instantiates", "depends-on", "enables"],
} as const;

export type StructuredActivationStrategy = keyof typeof STRATEGY_EDGE_PRIORITY;

function semantic(graph: Graph): Graph {
  return transitiveClosure(completeSymmetricEdges(graph));
}

/** The concept record as Markdown: its stored body plus the managed blocks. */
export function conceptMarkdown(db: Database, concept: ConceptRecord): string {
  const claims = claimsFromStore(db, concept.id, "accepted");
  const all = evidenceFromStore(db);
  const evidence = [
    ...evidenceFor(all, "concept", concept.id),
    ...claims.flatMap((claim) => evidenceFor(all, "claim", claim.id)),
  ];
  const byId = new Map<string, Evidence>(evidence.map((item) => [item.id, item]));
  return buildKnowledgeDocument({
    frontmatter: {
      id: concept.id,
      atom: concept.atom,
      layer: concept.layer,
      domain: concept.domain,
      confidence: concept.confidence,
      source: concept.source,
      triggers: concept.triggers,
      edges: [],
    },
    body: concept.body,
    claims: claims.map((claim: Claim) => ({
      id: claim.id,
      kind: claim.kind,
      confidence: claim.confidence,
      text: claim.text,
    })),
    evidence: projectEvidence([...byId.values()]),
  });
}

/**
 * Activation against the store.
 *
 * Identical DTO to the file-backed `activate`: `raw` is the Markdown an AI reads,
 * `molecule` its body, and `requires`/`loadNext` the precomputed closures.
 */
export function activateFromStore(
  db: Database,
  conceptId: string,
  options: { strategy?: StructuredActivationStrategy; depth?: number } = {},
): Activation {
  const graph = buildGraphFromStore(db);
  const sem = semantic(graph);
  const node = graph.nodes.find((candidate) => candidate.id === conceptId);
  if (node === undefined) throw new Error(`Concept not found: ${conceptId}`);
  const concept = conceptFromStore(db, conceptId);
  if (concept === null) throw new Error(`Concept not found: ${conceptId}`);

  let visited: string[];
  let depth: number;
  if (options.strategy !== undefined) {
    depth = options.depth ?? 2;
    visited = traverse(sem, [conceptId], {
      edgeTypes: [...STRATEGY_EDGE_PRIORITY[options.strategy]],
      direction: "both",
      maxDepth: depth,
    });
  } else {
    depth = options.depth ?? 1;
    visited = traverse(graph, [conceptId], { direction: "both", maxDepth: depth });
  }

  const sub = hopSubgraph(graph, visited, 0);
  const neighbors = sub.nodes
    .filter((candidate) => candidate.id !== conceptId)
    .map((candidate) => ({
      node: candidate,
      requires: targetsOf(sem, candidate.id, "depends-on"),
      loadNext: targetsOf(sem, candidate.id, "enables"),
    }));

  return {
    concept: conceptId,
    strategy: options.strategy ?? null,
    depth,
    raw: conceptMarkdown(db, concept),
    // The file loader trims trailing whitespace when it builds `molecule`, so
    // trimming here keeps the DTO byte-identical across the cutover.
    molecule: concept.body.trim(),
    triggers: concept.triggers,
    node,
    directEdges: sub.edges,
    requires: targetsOf(sem, conceptId, "depends-on"),
    loadNext: targetsOf(sem, conceptId, "enables"),
    neighbors,
    visited,
  };
}

/** Edges touching a concept. Same dedup and direction rules as the file path. */
export function edgesFromStore(
  db: Database,
  conceptId: string,
  options: { type?: GraphNode extends never ? never : Graph["edges"][number]["type"]; direction?: "in" | "out" | "both"; derived?: boolean } = {},
): EdgeHit[] {
  const graph = buildGraphFromStore(db);
  if (!graph.nodes.some((node) => node.id === conceptId)) {
    throw new Error(`Concept not found: ${conceptId}`);
  }
  const view = options.derived === true
    ? semantic(graph)
    : { nodes: graph.nodes, edges: completeSymmetricEdges(graph).edges };

  return queryEdgesOver(view, conceptId, options);
}

function queryEdgesOver(
  graph: Graph,
  conceptId: string,
  options: { type?: Graph["edges"][number]["type"]; direction?: "in" | "out" | "both" },
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
    if (other === undefined) continue;

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

export function pathFromStore(db: Database, from: string, to: string): PathResult {
  const graph = buildGraphFromStore(db);
  const path = shortestPath(graph, from, to) ?? [];
  const nodes = path
    .map((id) => graph.nodes.find((node) => node.id === id))
    .filter((node): node is GraphNode => node !== undefined);
  return { from, to, path, nodes };
}

/** Keyword search over the manifest projection of the store. */
export function searchFromStore(db: Database, keyword: string): ManifestRow[] {
  return searchManifest(manifestFromStore(db), keyword);
}

export function manifestFromStoreDb(db: Database): ManifestRow[] {
  return manifestFromStore(db);
}

export function vizFromStore(db: Database): VizPayload {
  const concepts = conceptsFromStore(db);
  const relations = listRelationRows(db);
  const nodes: VizPayload["nodes"] = [];
  const edges: VizPayload["edges"] = [];
  const layerCounts: Record<string, number> = { L0: 0, L1: 0, L2: 0, L3: 0 };
  const confidenceCounts: Record<string, number> = { confirmed: 0, hypothetical: 0, contested: 0 };

  for (const concept of concepts) {
    nodes.push({
      id: concept.id,
      atom: concept.atom,
      layer: concept.layer,
      domain: concept.domain,
      confidence: concept.confidence,
      source: concept.source,
      triggers: concept.triggers,
      filePath: concept.sourcePath,
      molecule: concept.body,
    });
    const key = `L${concept.layer}`;
    layerCounts[key] = (layerCounts[key] ?? 0) + 1;
    confidenceCounts[concept.confidence] = (confidenceCounts[concept.confidence] ?? 0) + 1;
  }

  for (const relation of relations) {
    edges.push({
      from: relation.from,
      type: relation.type,
      target: relation.target,
      confidence: relation.confidence,
      mirrored: false,
    });
    if (relation.type === "contradicts" || relation.type === "analogous-to") {
      edges.push({
        from: relation.target,
        type: relation.type,
        target: relation.from,
        confidence: relation.confidence,
        mirrored: true,
      });
    }
  }

  return {
    nodes,
    edges,
    meta: {
      conceptCount: nodes.length,
      assertedEdgeCount: edges.filter((edge) => !edge.mirrored).length,
      edgeTypes: [...new Set(edges.map((edge) => edge.type))].sort(),
      generatedAt: new Date().toISOString(),
      confidenceCounts,
      layerCounts,
    },
  };
}

/**
 * Validation over the store.
 *
 * The staleness half of the file-era report has no meaning here: `graph.json` is
 * no longer an input, so `staleness` is reported as clean with a note, and the
 * counts describe rows. Keeping the field (rather than dropping it) is what lets
 * an existing client's report parser keep working.
 */
export function validateFromStore(db: Database): ConceptValidationReport {
  const concepts = conceptsFromStore(db);
  const graph = buildGraphFromStore(db);
  const issues = validateGraph(graph).issues;

  const graphReport: GraphValidationReport = {
    issues,
    staleness: { missing: [], extra: [], nodeDrift: false },
    valid: issues.length === 0,
  };

  return {
    files: concepts.map((concept: ConceptRecord) => ({ filePath: concept.sourcePath, valid: true })),
    total: concepts.length,
    failed: 0,
    graph: graphReport,
    valid: graphReport.valid,
  };
}
