import type { Confidence, EdgeType, GraphEdge, GraphNode, Source } from "@x-sage/core";
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
  const edges: VizEdge[] = [];
  const layerCounts: Record<string, number> = { L0: 0, L1: 0, L2: 0, L3: 0 };
  const confidenceCounts: Record<Confidence, number> = {
    confirmed: 0,
    hypothetical: 0,
    contested: 0,
  };

  for (const concept of concepts) {
    const { frontmatter, molecule } = concept.file;
    nodes.push({
      id: frontmatter.id,
      atom: frontmatter.atom,
      layer: frontmatter.layer,
      domain: frontmatter.domain,
      confidence: frontmatter.confidence,
      source: frontmatter.source,
      triggers: frontmatter.triggers,
      filePath: concept.filePath,
      molecule,
    });

    layerCounts[`L${frontmatter.layer}`] = (layerCounts[`L${frontmatter.layer}`] ?? 0) + 1;
    confidenceCounts[frontmatter.confidence] += 1;

    for (const edge of frontmatter.edges) {
      edges.push({
        from: frontmatter.id,
        type: edge.type,
        target: edge.target,
        confidence: edge.confidence,
        mirrored: false,
      });

      // Symmetric relations are asserted once. Mirroring here lets the viz draw
      // them as undirected without every consumer re-implementing the reversal.
      if (edge.type === "contradicts" || edge.type === "analogous-to") {
        edges.push({
          from: edge.target,
          type: edge.type,
          target: frontmatter.id,
          confidence: edge.confidence,
          mirrored: true,
        });
      }
    }
  }

  const assertedEdgeCount = edges.filter((edge) => !edge.mirrored).length;
  const edgeTypes = [...new Set(edges.map((edge) => edge.type))].sort();

  return {
    nodes,
    edges,
    meta: {
      conceptCount: nodes.length,
      assertedEdgeCount,
      edgeTypes,
      generatedAt: new Date().toISOString(),
      confidenceCounts,
      layerCounts,
    },
  };
}
