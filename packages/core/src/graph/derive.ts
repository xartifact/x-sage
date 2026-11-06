import type { EdgeType, Graph, GraphEdge } from "../types/index.ts";

/**
 * Outgoing targets of one relation type, in assertion order.
 *
 * Callers that used to read the `requires` / `enables` frontmatter arrays now
 * read the graph instead: `depends-on` reproduces `requires`, `enables`
 * reproduces `enables`, and both are guaranteed to agree with `edges`.
 */
export function targetsOf(graph: Graph, id: string, type: EdgeType): string[] {
  return graph.edges
    .filter((edge) => edge.from === id && edge.type === type)
    .map((edge) => edge.target);
}

/** Incoming sources of one relation type — the reciprocal of `targetsOf`. */
export function sourcesOf(graph: Graph, id: string, type: EdgeType): string[] {
  return graph.edges
    .filter((edge) => edge.target === id && edge.type === type)
    .map((edge) => edge.from);
}

/** All edges touching a node, either direction. */
export function incidentEdges(graph: Graph, id: string): GraphEdge[] {
  return graph.edges.filter((edge) => edge.from === id || edge.target === id);
}
