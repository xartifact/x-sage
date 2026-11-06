import { describe, expect, test } from "bun:test";
import type { Graph, GraphEdge, GraphNode } from "../types/index.ts";
import { completeSymmetricEdges } from "./semantics.ts";
import { transitiveClosure, findEdgeCycles } from "./closure.ts";
import { validateGraph } from "./validate.ts";

function node(id: string, layer: 0 | 1 | 2 | 3 = 1): GraphNode {
  return { id, atom: id, layer, domain: ["test"], confidence: "confirmed", filePath: `${id}.md` };
}

function edge(
  from: string,
  type: GraphEdge["type"],
  target: string,
  confidence: GraphEdge["confidence"] = "confirmed",
): GraphEdge {
  return { from, type, target, confidence };
}

function graph(nodes: string[], edges: GraphEdge[]): Graph {
  return { nodes: nodes.map((id) => node(id)), edges };
}

describe("completeSymmetricEdges", () => {
  test("mirrors a one-directional symmetric relation", () => {
    const result = completeSymmetricEdges(graph(["a", "b"], [edge("a", "contradicts", "b")]));
    expect(result.edges).toHaveLength(2);
    expect(result.edges.map((e) => `${e.from}->${e.target}`)).toEqual(["a->b", "b->a"]);
  });

  test("leaves directed relations alone", () => {
    const result = completeSymmetricEdges(graph(["a", "b"], [edge("a", "depends-on", "b")]));
    expect(result.edges).toHaveLength(1);
  });

  test("keeps the weaker confidence when both directions are asserted", () => {
    // Symmetry must not upgrade a hypothesis into a confirmed claim.
    const result = completeSymmetricEdges(
      graph(["a", "b"], [
        edge("a", "contradicts", "b", "confirmed"),
        edge("b", "contradicts", "a", "hypothetical"),
      ]),
    );
    expect(result.edges).toHaveLength(2);
    for (const assertion of result.edges) {
      expect(assertion.confidence).toBe("hypothetical");
    }
  });

  test("does not mutate the input graph", () => {
    const input = graph(["a", "b"], [edge("a", "analogous-to", "b")]);
    completeSymmetricEdges(input);
    expect(input.edges).toHaveLength(1);
  });
});

describe("transitiveClosure", () => {
  test("derives the transitive edge and records a witness hop", () => {
    const result = transitiveClosure(
      graph(["a", "b", "c"], [edge("a", "depends-on", "b"), edge("b", "depends-on", "c")]),
    );
    const derived = result.edges.find((e) => e.from === "a" && e.target === "c");
    expect(derived).toBeDefined();
    expect(derived?.derived).toEqual(["b"]);
  });

  test("propagates the weakest confidence along the path", () => {
    const result = transitiveClosure(
      graph(["a", "b", "c"], [
        edge("a", "depends-on", "b", "confirmed"),
        edge("b", "depends-on", "c", "hypothetical"),
      ]),
    );
    expect(result.edges.find((e) => e.from === "a" && e.target === "c")?.confidence).toBe(
      "hypothetical",
    );
  });

  test("does not compose across relation types", () => {
    // is-a then part-of is a property chain; composing them would assert
    // mereology the author never wrote.
    const result = transitiveClosure(
      graph(["a", "b", "c"], [edge("a", "is-a", "b"), edge("b", "part-of", "c")]),
    );
    expect(result.edges.filter((e) => e.from === "a" && e.target === "c")).toHaveLength(0);
  });

  test("leaves non-transitive relations unexpanded", () => {
    const result = transitiveClosure(
      graph(["a", "b", "c"], [edge("a", "enables", "b"), edge("b", "enables", "c")]),
    );
    expect(result.edges).toHaveLength(2);
  });

  test("does not duplicate an already asserted edge", () => {
    const result = transitiveClosure(
      graph(["a", "b", "c"], [
        edge("a", "depends-on", "b"),
        edge("b", "depends-on", "c"),
        edge("a", "depends-on", "c"),
      ]),
    );
    expect(result.edges.filter((e) => e.from === "a" && e.target === "c")).toHaveLength(1);
  });
});

describe("findEdgeCycles", () => {
  test("reports a cycle among acyclic relations", () => {
    const cycles = findEdgeCycles(
      graph(["a", "b", "c"], [
        edge("a", "depends-on", "b"),
        edge("b", "depends-on", "c"),
        edge("c", "depends-on", "a"),
      ]),
    );
    expect(cycles).toHaveLength(1);
    expect(cycles[0]?.path).toEqual(["a", "b", "c", "a"]);
  });

  test("ignores cycles among relations that permit them", () => {
    // `enables` is navigation, not order; a mutual pair is legitimate.
    expect(
      findEdgeCycles(
        graph(["a", "b"], [edge("a", "enables", "b"), edge("b", "enables", "a")]),
      ),
    ).toHaveLength(0);
  });

  test("reports each cycle once regardless of entry point", () => {
    const cycles = findEdgeCycles(
      graph(["a", "b", "c"], [
        edge("a", "is-a", "b"),
        edge("b", "is-a", "c"),
        edge("c", "is-a", "a"),
      ]),
    );
    expect(cycles).toHaveLength(1);
  });
});

describe("validateGraph", () => {
  test("flags a layer violation when is-a points to a more concrete layer", () => {
    const g: Graph = {
      nodes: [node("abstract", 1), node("concrete", 2)],
      edges: [edge("abstract", "is-a", "concrete")],
    };
    expect(validateGraph(g).issues.map((i) => i.type)).toContain("layer-violation");
  });

  test("accepts instantiates pointing to a more abstract layer", () => {
    const g: Graph = {
      nodes: [node("instance", 3), node("parent", 2)],
      edges: [edge("instance", "instantiates", "parent")],
    };
    expect(validateGraph(g).issues).toHaveLength(0);
  });

  test("does not flag a concept that both specialises and contradicts its parent", () => {
    // Dialectical tension is not logical disjointness. Three real concepts do
    // exactly this (goal-drift, latent-goal, pseudo-need); flagging it would
    // delete the KB's most valuable structure.
    const g: Graph = {
      nodes: [node("parent", 1), node("child", 2)],
      edges: [edge("child", "is-a", "parent"), edge("child", "contradicts", "parent")],
    };
    expect(validateGraph(g).issues).toHaveLength(0);
  });

  test("flags a self-reference", () => {
    const g: Graph = { nodes: [node("a")], edges: [edge("a", "contradicts", "a")] };
    expect(validateGraph(g).issues.map((i) => i.type)).toContain("self-reference");
  });

  test("flags a dangling edge target", () => {
    const g: Graph = { nodes: [node("a")], edges: [edge("a", "is-a", "missing")] };
    expect(validateGraph(g).issues.map((i) => i.type)).toContain("dangling-edge");
  });
});
