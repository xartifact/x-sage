import { expect, test } from "bun:test";
import { graphProjection } from "./graph-camera";
import { fallingHeight, graphLandingHeight, graphLayerGap, graphLayerHeight, unsupportedGraphNodes } from "./graph-gravity";
import { LAYERS, type Confidence, type EdgeType, type GraphData, type GraphNode, type Layer } from "./types";

const node = (id: string, layer: Layer): GraphNode => ({
  id, layer, atom: "", domain: [], confidence: "confirmed", source: "synthesis",
  triggers: [], filePath: "", molecule: "",
});
const edge = (from: string, target: string, type: EdgeType, confidence: Confidence = "confirmed") => ({
  from, target, type, confidence,
});

test("layers ascend from L0 in world space and in the fallback projection", () => {
  const size = { width: 720, height: 480 };
  const gap = graphLayerGap(size.height);
  const project = graphProjection({ yaw: 0, pitch: 0.52 }, size);
  for (const [lower, higher] of [[0, 1], [1, 2], [2, 3]] as const) {
    expect(graphLayerHeight(lower, gap)).toBeLessThan(graphLayerHeight(higher, gap));
    expect(project(360, 240, lower).y).toBeGreaterThan(project(360, 240, higher).y);
  }
  expect(graphLandingHeight(gap)).toBeLessThan(graphLayerHeight(0, gap));
});

test("only confirmed downward support connected to L0 carries upper nodes", () => {
  const data: GraphData = {
    nodes: [
      node("root", 0), node("core", 1), node("type", 2), node("instance", 3),
      node("orphan", 1), node("above-orphan", 2), node("same-level", 1),
      node("reverse", 1), node("related", 1), node("tentative", 1), node("missing-target", 1),
      node("mirrored", 1),
    ],
    edges: [
      edge("core", "root", "depends-on"),
      edge("type", "core", "is-a"),
      edge("instance", "type", "instantiates"),
      edge("above-orphan", "orphan", "depends-on"),
      edge("same-level", "core", "depends-on"),
      edge("root", "reverse", "depends-on"),
      edge("related", "root", "part-of"),
      edge("tentative", "root", "depends-on", "hypothetical"),
      edge("missing-target", "unknown", "depends-on"),
      { ...edge("mirrored", "root", "is-a"), mirrored: true },
    ],
  };
  expect([...unsupportedGraphNodes(data)].sort()).toEqual([
    "above-orphan", "mirrored", "missing-target", "orphan", "related", "reverse", "same-level", "tentative",
  ]);
});

test("gravity accelerates the fall and the floor stops it", () => {
  const gap = graphLayerGap(620);
  const floor = graphLandingHeight(gap) + 9;
  for (const layer of LAYERS.slice(1)) {
    const start = graphLayerHeight(layer, gap);
    const first = fallingHeight(start, floor, 100);
    const second = fallingHeight(start, floor, 200);
    expect(fallingHeight(start, floor, 0)).toBe(start);
    expect(first).toBeLessThan(start);
    expect(first - second).toBeGreaterThan(start - first);
    expect(fallingHeight(start, floor, 3000)).toBe(floor);
  }
});
