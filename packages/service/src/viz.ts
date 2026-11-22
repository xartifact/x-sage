import type { Confidence, EdgeType, Graph, GraphEdge, GraphNode, ManifestRow, Source } from "@x-sage/core";
import { loadConcepts } from "./concepts.ts";
import type { KbContext } from "./kb.ts";

/** A concept as the visualiser needs it: graph metadata plus prose. */
export interface VizNode extends GraphNode {
  source: Source;
  triggers: string[];
  molecule: string;
}

export interface VizEdge extends GraphEdge {
  /**
   * True for the mirrored half of a symmetric relation, which the author did not
   * write. Named `mirrored` because `GraphEdge.derived` already means "inferred
   * transitively" and holds the witness path.
   */
  mirrored: boolean;
}

export interface VizPayload {
  nodes: VizNode[];
  edges: VizEdge[];
  meta: {
    conceptCount: number;
    assertedEdgeCount: number;
    /** Sum of unique relation types present, useful for legends. */
    edgeTypes: EdgeType[];
    generatedAt: string;
    confidenceCounts: Record<Confidence, number>;
    layerCounts: Record<string, number>;
  };
}

/**
 * The complete payload the visualiser renders.
 *
 * Replaces the standalone script that used to write `public/graph-data.json`:
 * building it from the same loader the CLI uses keeps the two views in sync, and
 * at ~2.6 ms per KB there is no reason to cache a stale copy on disk.
 */
export async function buildVizPayload(ctx: KbContext): Promise<VizPayload> {
  const { concepts } = await loadConcepts(ctx);

  const nodes: VizNode[] = [];
  const assertions: GraphEdge[] = [];
  for (const concept of concepts) {
    const { frontmatter, molecule } = concept.file;
    const node = {
      id: frontmatter.id,
      atom: frontmatter.atom,
      layer: frontmatter.layer,
      domain: frontmatter.domain,
      confidence: frontmatter.confidence,
      filePath: concept.filePath,
    };
    nodes.push(vizNode(node, frontmatter, molecule));
    for (const edge of frontmatter.edges) assertions.push({ from: frontmatter.id, ...edge });
  }
  return { nodes, ...vizFacts(nodes, assertions) };
}

/** Database bodies and graph metadata must describe the same captured basis. */
export function vizFromGraph(graph: Graph, rows: ManifestRow[], bodies: ReadonlyMap<string, string>): VizPayload {
  const metadata = new Map(rows.map((row) => [row.id, row]));
  const nodes = graph.nodes.map((node) => {
    const term = metadata.get(node.id);
    const body = bodies.get(node.id);
    if (term === undefined || body === undefined) throw new Error("Incomplete visualizer snapshot");
    return vizNode(node, term, body.trim());
  });
  return { nodes, ...vizFacts(nodes, graph.edges) };
}

function vizNode(node: GraphNode, term: Pick<ManifestRow, "source" | "triggers">, molecule: string): VizNode {
  return {
    id: node.id,
    atom: node.atom,
    layer: node.layer,
    domain: node.domain,
    confidence: node.confidence,
    source: term.source,
    triggers: term.triggers,
    filePath: node.filePath,
    molecule,
  };
}

/** Visualizer mirrors and counts need only asserted graph metadata. */
export function vizFacts(nodes: readonly GraphNode[], assertions: readonly GraphEdge[]): Pick<VizPayload, "edges" | "meta"> {
  const edges: VizEdge[] = [];
  const layerCounts: Record<string, number> = { L0: 0, L1: 0, L2: 0, L3: 0 };
  const confidenceCounts: Record<Confidence, number> = { confirmed: 0, hypothetical: 0, contested: 0 };
  for (const node of nodes) {
    layerCounts[`L${node.layer}`] = (layerCounts[`L${node.layer}`] ?? 0) + 1;
    confidenceCounts[node.confidence] += 1;
  }
  for (const { from, type, target, confidence } of assertions) {
    edges.push({ from, type, target, confidence, mirrored: false });
    if (type === "contradicts" || type === "analogous-to") {
      edges.push({ from: target, type, target: from, confidence, mirrored: true });
    }
  }
  return {
    edges,
    meta: {
      conceptCount: nodes.length,
      assertedEdgeCount: assertions.length,
      edgeTypes: [...new Set(edges.map((edge) => edge.type))].sort(),
      generatedAt: new Date().toISOString(),
      confidenceCounts,
      layerCounts,
    },
  };
}
