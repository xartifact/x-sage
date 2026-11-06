import type { EdgeType, Graph, GraphEdge, Confidence } from "../types/index.ts";

/**
 * Formal semantics for each relation type.
 *
 * This table is the single source of truth for what each edge means. Before it
 * existed, relation behaviour was scattered across ad-hoc filters in the CLI,
 * the viz app and the validator, and nothing declared which relations are
 * transitive, symmetric or acyclic.
 *
 * Layer rules use the L0..L3 scale where L0 is the most abstract:
 * - "to-abstract"      — target.layer <= source.layer (may stay level)
 * - "to-concrete-strict"— target.layer >  source.layer
 * - "any"              — unconstrained
 *
 * Direction is named by where the arrow points, not by an up/down metaphor
 * where the reader has to remember which end of L0..L3 is "up".
 */
export interface RelationSpec {
  /** Edges compose: A r B and B r C imply A r C. */
  transitive: boolean;
  /** A r B implies B r A; the reverse edge is derived, not authored. */
  symmetric: boolean;
  /** The relation forms a strict partial order; cycles are modelling errors. */
  acyclic: boolean;
  layer: "to-abstract" | "to-concrete-strict" | "any";
  /** Human label used by validation messages and docs. */
  label: string;
}

export const RELATION_SPECS: Record<EdgeType, RelationSpec> = {
  "is-a": {
    transitive: true,
    symmetric: false,
    acyclic: true,
    layer: "to-abstract",
    label: "subsumption",
  },
  "part-of": {
    transitive: true,
    symmetric: false,
    acyclic: true,
    layer: "to-abstract",
    label: "mereology",
  },
  "depends-on": {
    // Transitive by construction: if X needs Y loaded first, and Y needs Z,
    // then X cannot be understood before Z.
    transitive: true,
    symmetric: false,
    acyclic: true,
    layer: "any",
    label: "prerequisite order",
  },
  enables: {
    // Deliberately NOT the inverse of depends-on. `risk` and `decision` are
    // mutually enabling (each produces the other) but neither is the other's
    // prerequisite, so deriving one from the other would create a false cycle.
    // `enables` is asserted navigation: understanding A helps understand B.
    transitive: false,
    symmetric: false,
    acyclic: false,
    layer: "any",
    label: "navigation",
  },
  instantiates: {
    // L3 instance → L2/L1 abstract. Layer must strictly decrease (get more
    // abstract); an instance of something less abstract than itself is a
    // modelling error, not a stylistic one.
    transitive: false,
    symmetric: false,
    acyclic: true,
    layer: "to-abstract",
    label: "abstract to concrete",
  },
  contradicts: {
    // Dialectical opposition, NOT owl:disjointWith. A concept may be a
    // specialisation of X while standing in tension with X (goal-drift,
    // latent-goal, pseudo-need all do exactly this), so no disjointness check.
    transitive: false,
    symmetric: true,
    acyclic: false,
    layer: "any",
    label: "dialectical tension",
  },
  "analogous-to": {
    transitive: false,
    symmetric: true,
    acyclic: false,
    layer: "any",
    label: "cross-domain analogy",
  },
};

export const EDGE_TYPES = Object.keys(RELATION_SPECS) as EdgeType[];

const CONFIDENCE_RANK: Record<Confidence, number> = {
  confirmed: 2,
  hypothetical: 1,
  contested: 0,
};


export function edgeKey(from: string, type: EdgeType, target: string): string {
  return `${from}\u0000${type}\u0000${target}`;
}

/**
 * Add the reverse edge for every symmetric relation that only declares one
 * direction. The result is a new graph; the input is not mutated.
 *
 * Symmetric completion is materialised (unlike transitive closure) because it
 * costs one edge per asserted edge and makes `contradicts` readable as
 * undirected without every consumer re-implementing the reversal.
 */
export function completeSymmetricEdges(graph: Graph): Graph {
  const edges: GraphEdge[] = [...graph.edges];
  const seen = new Map<string, number>();
  for (const [index, edge] of edges.entries()) {
    seen.set(edgeKey(edge.from, edge.type, edge.target), index);
  }

  for (const edge of [...edges]) {
    const spec = RELATION_SPECS[edge.type];
    if (!spec.symmetric) continue;

    const reverseKey = edgeKey(edge.target, edge.type, edge.from);
    const existingIndex = seen.get(reverseKey);

    if (existingIndex === undefined) {
      seen.set(reverseKey, edges.length);
      edges.push({
        from: edge.target,
        type: edge.type,
        target: edge.from,
        confidence: edge.confidence,
      });
    } else {
      // Both directions asserted: keep the weaker confidence so symmetry never
      // upgrades a hypothetical tension into a confirmed one.
      const existing = edges[existingIndex]!;
      const keepExisting =
        CONFIDENCE_RANK[existing.confidence] <= CONFIDENCE_RANK[edge.confidence];
      edges[existingIndex] = {
        ...existing,
        confidence: keepExisting ? existing.confidence : edge.confidence,
      };
    }
  }

  return { nodes: graph.nodes, edges };
}

/**
 * Check the layer rule for an edge. Returns null when the edge is legal.
 */
export function layerViolation(
  sourceLayer: number,
  targetLayer: number,
  spec: RelationSpec,
): string | null {
  if (spec.layer === "to-abstract" && targetLayer > sourceLayer) {
    return `target layer L${targetLayer} is more concrete than source layer L${sourceLayer}; "${spec.label}" may only point to an equal or more abstract layer`;
  }
  if (spec.layer === "to-concrete-strict" && targetLayer <= sourceLayer) {
    return `target layer L${targetLayer} is not more concrete than source layer L${sourceLayer}; "${spec.label}" must point to a strictly more concrete layer`;
  }
  return null;
}
