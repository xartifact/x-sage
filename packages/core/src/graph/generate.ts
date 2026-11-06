import { readdir, readFile } from "node:fs/promises";
import { join, relative } from "node:path";
import { parseConceptFile } from "../manifest/parse.ts";
import { GraphSchema, type Graph, type GraphNode, type GraphEdge } from "../types/index.ts";

/**
 * Scan a KB's concepts directory and generate a Graph from all concept files.
 */
export async function generateGraph(kbPath: string): Promise<Graph> {
  const layers = ["L0-axioms", "L1-core", "L2-support", "L3-instances"];
  const nodes: GraphNode[] = [];
  const edges: GraphEdge[] = [];

  for (const layer of layers) {
    const layerDir = join(kbPath, "concepts", layer);
    let entries: string[];
    try {
      entries = await readdir(layerDir);
    } catch {
      continue; // layer directory doesn't exist yet
    }

    for (const file of entries) {
      if (!file.endsWith(".md")) continue;
      const filePath = join(layerDir, file);
      const raw = await readFile(filePath, "utf-8");
      const concept = parseConceptFile(raw, relative(kbPath, filePath));

      const node: GraphNode = {
        id: concept.frontmatter.id,
        atom: concept.frontmatter.atom,
        layer: concept.frontmatter.layer,
        domain: concept.frontmatter.domain,
        confidence: concept.frontmatter.confidence,
        filePath: concept.filePath,
      };
      nodes.push(node);

      for (const edge of concept.frontmatter.edges) {
        edges.push({
          type: edge.type,
          target: edge.target,
          confidence: edge.confidence,
          from: concept.frontmatter.id,
        });
      }
    }
  }

  return GraphSchema.parse({ nodes, edges });
}

/**
 * Pretty-print a Graph as JSON with 2-space indentation.
 */
export function formatGraph(graph: Graph): string {
  return JSON.stringify(graph, null, 2);
}
