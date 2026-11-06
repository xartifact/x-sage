import type { EdgeType, Graph, GraphEdge } from "../types/index.ts";
import { RELATION_SPECS, edgeKey } from "./semantics.ts";

const CONFIDENCE_RANK: Record<string, number> = {
  confirmed: 2,
  hypothetical: 1,
  contested: 0,
};

/**
 * Derive transitive edges for every relation whose spec declares transitivity.
 *
 * Derived edges are marked with `derived: ["a", "b"]` holding the intermediate
 * node ids, and carry the weakest confidence along the path — a conclusion is
 * never more certain than the link that is least certain.
 *
 * Scope is intentionally one relation type at a time. Composing `part-of` with
 * `is-a` (the way OWL's sub-property chains would) mixes mereology with
 * subsumption and produces claims the authors never made.
 */
export function transitiveClosure(graph: Graph, types?: EdgeType[]): Graph {
  const edges: GraphEdge[] = [...graph.edges];
  const existing = new Set(
    edges.map((edge) => edgeKey(edge.from, edge.type, edge.target)),
  );

  const targetTypes = types ?? (Object.keys(RELATION_SPECS) as EdgeType[]).filter(
    (type) => RELATION_SPECS[type].transitive,
  );

  for (const type of targetTypes) {
    if (!RELATION_SPECS[type].transitive) continue;

    // Adjacency limited to this relation, in the asserted direction.
    const outgoing = new Map<string, GraphEdge[]>();
    for (const edge of edges) {
      if (edge.type !== type) continue;
      const bucket = outgoing.get(edge.from);
      if (bucket) bucket.push(edge);
      else outgoing.set(edge.from, [edge]);
    }

    // Breadth-first from each node, remembering the first hop so a derived edge
    // can name one concrete witness path instead of an arbitrary one.
    for (const origin of graph.nodes) {
      const queue: Array<{ node: string; firstHop: string; confidence: string }> = [];
      const visited = new Set<string>([origin.id]);

      for (const direct of outgoing.get(origin.id) ?? []) {
        queue.push({
          node: direct.target,
          firstHop: direct.target,
          confidence: direct.confidence,
        });
      }

      for (let cursor = 0; cursor < queue.length; cursor++) {
        const current = queue[cursor]!;
        if (visited.has(current.node)) continue;
        visited.add(current.node);

        if (current.node !== origin.id) {
          const key = edgeKey(origin.id, type, current.node);
          if (!existing.has(key)) {
            existing.add(key);
            edges.push({
              from: origin.id,
              type,
              target: current.node,
              confidence: current.confidence as GraphEdge["confidence"],
              derived: [current.firstHop],
            });
          }
        }

        for (const next of outgoing.get(current.node) ?? []) {
          if (visited.has(next.target)) continue;
          const weaker =
            CONFIDENCE_RANK[next.confidence]! <= CONFIDENCE_RANK[current.confidence]!
              ? next.confidence
              : current.confidence;
          queue.push({
            node: next.target,
            firstHop: current.firstHop,
            confidence: weaker,
          });
        }
      }
    }
  }

  return { nodes: graph.nodes, edges };
}

export interface EdgeCycle {
  type: EdgeType;
  /** Node ids in traversal order; the first node repeats as the last. */
  path: string[];
}

/**
 * Find every cycle among relations declared acyclic.
 *
 * Iterative DFS with an explicit stack is used rather than recursion so a
 * pathological graph cannot blow the call stack during validation.
 */
export function findEdgeCycles(graph: Graph): EdgeCycle[] {
  const cycles: EdgeCycle[] = [];
  const acyclicTypes = (Object.keys(RELATION_SPECS) as EdgeType[]).filter(
    (type) => RELATION_SPECS[type].acyclic,
  );

  for (const type of acyclicTypes) {
    const outgoing = new Map<string, string[]>();
    for (const edge of graph.edges) {
      if (edge.type !== type) continue;
      const bucket = outgoing.get(edge.from);
      if (bucket) bucket.push(edge.target);
      else outgoing.set(edge.from, [edge.target]);
    }

    const state = new Map<string, 0 | 1 | 2>();
    const reported = new Set<string>();

    for (const node of graph.nodes) {
      if ((state.get(node.id) ?? 0) !== 0) continue;

      const stack: Array<{ id: string; index: number }> = [
        { id: node.id, index: 0 },
      ];
      state.set(node.id, 1);

      while (stack.length > 0) {
        const top = stack[stack.length - 1]!;
        const neighbours = outgoing.get(top.id) ?? [];

        if (top.index >= neighbours.length) {
          state.set(top.id, 2);
          stack.pop();
          continue;
        }

        const next = neighbours[top.index]!;
        top.index++;

        const nextState = state.get(next) ?? 0;
        if (nextState === 1) {
          const start = stack.findIndex((frame) => frame.id === next);
          const path = [...stack.slice(start).map((frame) => frame.id), next];
          // The same cycle is reachable from every member; report it once.
          const signature = [...path.slice(0, -1)].sort().join("\u0000");
          if (!reported.has(signature)) {
            reported.add(signature);
            cycles.push({ type, path });
          }
        } else if (nextState === 0) {
          state.set(next, 1);
          stack.push({ id: next, index: 0 });
        }
      }
    }
  }

  return cycles;
}
