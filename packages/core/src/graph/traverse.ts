import type { Graph, GraphEdge } from "../types/index.ts";

/**
 * Traverse the graph from start nodes following edges of the given types.
 * Returns nodes in BFS order.
 */
export function traverse(
  graph: Graph,
  startIds: string[],
  options: {
    edgeTypes?: string[];
    direction?: "forward" | "backward" | "both";
    maxDepth?: number;
  } = {}
): string[] {
  const { edgeTypes, direction = "both", maxDepth = Infinity } = options;
  const edgeTypeSet = edgeTypes ? new Set(edgeTypes) : null;

  const visited = new Set<string>(startIds);
  const order: string[] = [...startIds];
  let frontier = [...startIds];

  for (let depth = 0; depth < maxDepth && frontier.length > 0; depth++) {
    const nextFrontier: string[] = [];

    for (const nodeId of frontier) {
      for (const edge of graph.edges) {
        if (edgeTypeSet && !edgeTypeSet.has(edge.type)) continue;

        const isForward = edge.from === nodeId;
        const isBackward = edge.target === nodeId;

        if (direction === "forward" && !isForward) continue;
        if (direction === "backward" && !isBackward) continue;
        if (direction === "both" && !isForward && !isBackward) continue;

        const neighbor = isForward ? edge.target : edge.from;
        if (!visited.has(neighbor)) {
          visited.add(neighbor);
          nextFrontier.push(neighbor);
          order.push(neighbor);
        }
      }
    }

    frontier = nextFrontier;
  }

  return order;
}

/**
 * Find the shortest path between two nodes.
 * Returns the sequence of node IDs, or null if no path exists.
 */
export function shortestPath(
  graph: Graph,
  fromId: string,
  toId: string,
): string[] | null {
  if (fromId === toId) return [fromId];

  const visited = new Set<string>([fromId]);
  const queue: { node: string; path: string[] }[] = [{ node: fromId, path: [fromId] }];

  while (queue.length > 0) {
    const current = queue.shift()!;

    for (const edge of graph.edges) {
      let neighbor: string | null = null;
      if (edge.from === current.node && !visited.has(edge.target)) {
        neighbor = edge.target;
      } else if (edge.target === current.node && !visited.has(edge.from)) {
        neighbor = edge.from;
      }

      if (neighbor !== null) {
        const newPath = [...current.path, neighbor];
        if (neighbor === toId) return newPath;
        visited.add(neighbor);
        queue.push({ node: neighbor, path: newPath });
      }
    }
  }

  return null;
}
