import { describe, expect, test } from "bun:test";
import { browseGraph, countDomains, domainName } from "./graph-browser";
import { defaultGraphCamera, graphLabelMode, graphProjection, panGraphView, rotateGraph, showGraphLabel } from "./graph-camera";
import { EDGE_TYPES, LAYERS, type GraphData, type GraphNode, type Layer } from "./types";

const concept = (id: string, domain: string[], layer: Layer, triggers: string[]): GraphNode => ({
  id, domain, layer, triggers, molecule: `# ${id}`, atom: "A test concept",
  source: "domain", confidence: "confirmed", filePath: `${id}.md`,
});
const graph: GraphData = {
  nodes: [
    concept("plan", ["engineering", "engineering"], 1, ["rollback"]),
    concept("checklist", ["engineering", "product"], 2, ["gate"]),
    concept("catalog", ["product"], 1, ["rollback"]),
    concept("design", ["custom"], 2, ["review"]),
  ],
  edges: [
    { from: "plan", target: "checklist", type: "depends-on", confidence: "confirmed" },
    { from: "catalog", target: "plan", type: "contradicts", confidence: "confirmed" },
    { from: "plan", target: "catalog", type: "contradicts", confidence: "confirmed", mirrored: true },
    { from: "catalog", target: "design", type: "depends-on", confidence: "confirmed" },
  ],
};
const filters = { domain: "engineering", query: "", layers: new Set(LAYERS),
  relations: new Set(EDGE_TYPES), neighborsOf: null };

describe("domain browsing", () => {
  test("counts distinct concepts and retains labels declared by the knowledge base", () => {
    expect(countDomains(graph.nodes)).toEqual([
      { id: "engineering", count: 2 }, { id: "product", count: 2 }, { id: "custom", count: 1 },
    ]);
    expect(domainName("engineering")).toBe("工程");
    expect(domainName("custom")).toBe("custom");
  });

  test("intersects domain, keyword, and layer before choosing the visible edges", () => {
    const domain = browseGraph(graph, filters);
    expect(domain.nodes.map((node) => node.id)).toEqual(["plan", "checklist"]);
    expect(domain.edges.map((edge) => edge.target)).toEqual(["checklist"]);
    const search = browseGraph(graph, { ...filters, query: " ROLLBACK " });
    expect(search.nodes.map((node) => node.id)).toEqual(["plan"]);
    expect(search.edges).toEqual([]);
    expect(browseGraph(graph, { ...filters, query: "rollback", layers: new Set([2]) }).nodes).toEqual([]);
  });

  test("expands incoming and outgoing edges by one hop under the active relation filters", () => {
    const expanded = browseGraph(graph, { ...filters, query: "rollback", neighborsOf: "plan" });
    expect(expanded.matches.map((node) => node.id)).toEqual(["plan"]);
    expect(expanded.neighbors.map((node) => node.id)).toEqual(["checklist", "catalog"]);
    expect(expanded.edges).toHaveLength(2);
    const depends = browseGraph(graph, {
      ...filters, query: "rollback", neighborsOf: "plan", relations: new Set(["depends-on"]),
    });
    expect(depends.neighbors.map((node) => node.id)).toEqual(["checklist"]);
    expect(browseGraph(graph, { ...filters, neighborsOf: "catalog" }).neighbors).toEqual([]);
  });
});

test("the 3D camera separates layers vertically and changes perspective when rotated", () => {
  const size = { width: 750, height: 500 };
  const project = graphProjection(defaultGraphCamera, size);
  expect(project(375, 250, 0).y).toBeLessThan(project(375, 250, 3).y);
  const point = project(450, 275, 1);
  const rotated = graphProjection(rotateGraph(defaultGraphCamera, 50, -25), size)(450, 275, 1);
  expect(rotated.x).not.toBeCloseTo(point.x);
  expect(rotated.depth).not.toBeCloseTo(point.depth);
  expect(rotateGraph(defaultGraphCamera, 0, 10000).pitch).toBeGreaterThan(-Math.PI / 2);
});

test("panning moves the view opposite to the drag without changing zoom", () => {
  const view = { x: -100, y: 50, width: 400, height: 240 };
  expect(panGraphView(view, 80, -30)).toEqual({ x: -180, y: 80, width: 400, height: 240 });
});

test("saved 3D label preferences accept explicit show and hide and otherwise use automatic labels", () => {
  expect(graphLabelMode("show")).toBe("show");
  expect(graphLabelMode("hide")).toBe("hide");
  expect(graphLabelMode(null)).toBe("auto");
  expect(graphLabelMode("invalid")).toBe("auto");
});

test("the graph renderers share explicit and contextual name visibility", () => {
  expect(showGraphLabel("show", 3, false, false, false)).toBe(true);
  expect(showGraphLabel("hide", 0, true, true, true)).toBe(false);
  expect(showGraphLabel("auto", 3, false, true, false)).toBe(true);
  expect(showGraphLabel("auto", 3, false, false, true)).toBe(false);
  expect(showGraphLabel("auto", 3, true, false, true)).toBe(true);
  expect(showGraphLabel("auto", 0, false, false, false)).toBe(false);
  expect(showGraphLabel("auto", 0, false, false, true)).toBe(true);
});
