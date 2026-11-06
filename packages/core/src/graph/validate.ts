import type { Graph } from "../types/index.ts";
import { RELATION_SPECS, layerViolation } from "./semantics.ts";
import { findEdgeCycles } from "./closure.ts";

export interface GraphValidationIssue {
  type:
    | "dangling-edge"
    | "orphan-node"
    | "edge-cycle"
    | "layer-violation"
    | "self-reference";
  message: string;
  details: { from?: string; target: string; edgeType?: string };
}

export interface GraphValidationResult {
  valid: boolean;
  issues: GraphValidationIssue[];
}

/**
 * Validate graph integrity against the declared relation semantics.
 *
 * Checks:
 * 1. dangling-edge — an edge references a node id that doesn't exist
 * 2. orphan-node — a node with no incoming and no outgoing edges
 * 3. self-reference — a node relating to itself (a true contradiction of the
 *    author's intent for every relation type, including symmetric ones)
 * 4. layer-violation — an edge whose direction breaks its relation's layer rule
 *    (`is-a`/`part-of`/`instantiates` may only point to an equal or more
 *    abstract layer, i.e. layer number must not increase along the edge)
 * 5. edge-cycle — a cycle among relations declared acyclic
 *    (`is-a`, `part-of`, `depends-on`, `instantiates`)
 *
 * Deliberately NOT checked: disjointness between `contradicts` and `is-a`.
 * `contradicts` here models dialectical tension, not logical disjointness —
 * three concepts (goal-drift, latent-goal, pseudo-need) are specialisations of
 * a concept they stand in tension with. Treating that as an error would delete
 * the KB's most valuable structure.
 */
export function validateGraph(graph: Graph): GraphValidationResult {
  const issues: GraphValidationIssue[] = [];
  const nodeIds = new Set(graph.nodes.map((n) => n.id));
  const layerOf = new Map(graph.nodes.map((n) => [n.id, n.layer]));

  for (const edge of graph.edges) {
    if (!nodeIds.has(edge.from)) {
      issues.push({
        type: "dangling-edge",
        message: `Edge references unknown source node: ${edge.from}`,
        details: { from: edge.from, target: edge.target, edgeType: edge.type },
      });
      continue;
    }
    if (!nodeIds.has(edge.target)) {
      issues.push({
        type: "dangling-edge",
        message: `Edge references unknown target node: ${edge.target}`,
        details: { from: edge.from, target: edge.target, edgeType: edge.type },
      });
      continue;
    }

    if (edge.from === edge.target) {
      issues.push({
        type: "self-reference",
        message: `Node "${edge.from}" has a "${edge.type}" edge to itself`,
        details: { from: edge.from, target: edge.target, edgeType: edge.type },
      });
      continue;
    }

    const spec = RELATION_SPECS[edge.type];
    const violation = layerViolation(
      layerOf.get(edge.from)!,
      layerOf.get(edge.target)!,
      spec,
    );
    if (violation !== null) {
      issues.push({
        type: "layer-violation",
        message: `"${edge.type}" from "${edge.from}" to "${edge.target}": ${violation}`,
        details: { from: edge.from, target: edge.target, edgeType: edge.type },
      });
    }
  }

  const incoming = new Map<string, number>();
  const outgoing = new Map<string, number>();
  for (const node of graph.nodes) {
    incoming.set(node.id, 0);
    outgoing.set(node.id, 0);
  }
  for (const edge of graph.edges) {
    if (outgoing.has(edge.from)) {
      outgoing.set(edge.from, (outgoing.get(edge.from) ?? 0) + 1);
    }
    if (incoming.has(edge.target)) {
      incoming.set(edge.target, (incoming.get(edge.target) ?? 0) + 1);
    }
  }

  for (const node of graph.nodes) {
    if ((incoming.get(node.id) ?? 0) === 0 && (outgoing.get(node.id) ?? 0) === 0) {
      issues.push({
        type: "orphan-node",
        message: `Node "${node.id}" is isolated (no incoming or outgoing edges)`,
        details: { target: node.id },
      });
    }
  }

  for (const cycle of findEdgeCycles(graph)) {
    issues.push({
      type: "edge-cycle",
      message: `"${cycle.type}" cycle: ${cycle.path.join(" → ")}`,
      details: {
        from: cycle.path[0]!,
        target: cycle.path[cycle.path.length - 1]!,
        edgeType: cycle.type,
      },
    });
  }

  return { valid: issues.length === 0, issues };
}
