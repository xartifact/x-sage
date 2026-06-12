import type { Graph, GraphNode, GraphEdge } from "../types/index.ts";

/**
 * Extract a subgraph containing the given node IDs and all edges between them.
 */
export function extractSubgraph(graph: Graph, nodeIds: string[]): Graph {
  const nodeSet = new Set(nodeIds);
  const nodes = graph.nodes.filter((n) => nodeSet.has(n.id));
  const edges = graph.edges.filter(
    (e) => nodeSet.has(e.from) && (nodeSet.has(e.target) || nodeSet.has(e.target.split("::")[0] ?? e.target))
  );
  return { nodes, edges };
}

/**
 * Get all nodes within N hops of the given start nodes.
 */
export function hopSubgraph(graph: Graph, startIds: string[], maxHops: number): Graph {
  const visited = new Set<string>(startIds);
  let frontier = new Set<string>(startIds);

  for (let hop = 0; hop < maxHops; hop++) {
    const nextFrontier = new Set<string>();
    for (const nodeId of frontier) {
      for (const edge of graph.edges) {
        if (edge.from === nodeId && !visited.has(edge.target)) {
          nextFrontier.add(edge.target);
        }
        if (edge.target === nodeId && !visited.has(edge.from)) {
          nextFrontier.add(edge.from);
        }
      }
    }
    if (nextFrontier.size === 0) break;
    for (const id of nextFrontier) visited.add(id);
    frontier = nextFrontier;
  }

  return extractSubgraph(graph, [...visited]);
}
