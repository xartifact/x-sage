import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { mkdtemp, cp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  activate,
  buildGraph,
  buildVizPayload,
  findPath,
  kb,
  queryEdges,
  search,
  semanticGraph,
  validateConcepts,
  validateGraphState,
} from "../src/index.ts";

/**
 * Integration tests against the real KB.
 *
 * These assert behaviour that broke during development and would fail silently:
 * the inbound edge query, symmetric deduplication, and validation of a KB whose
 * files do not parse. Fixtures are copied to a temp dir so writes never touch
 * the repository.
 */

const REAL_KB = resolve(import.meta.dir, "../../../kb/default");
const ctx = kb(REAL_KB);

async function copyKb(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "x-sage-test-"));
  await cp(REAL_KB, dir, { recursive: true });
  return dir;
}

describe("graph", () => {
  test("builds the expected node and edge counts", async () => {
    const graph = await buildGraph(ctx);
    expect(graph.nodes).toHaveLength(22);
    expect(graph.edges).toHaveLength(94);
  });

  test("semantic view adds mirrored and derived edges", async () => {
    const graph = await buildGraph(ctx);
    const semantic = semanticGraph(graph);
    expect(semantic.edges.length).toBeGreaterThan(graph.edges.length);
    // 7 symmetric edges get a mirrored half; closure adds transitive depends-on.
    expect(semantic.edges.length).toBe(137);
  });
});

describe("queryEdges", () => {
  test("answers 'what points at goal' for a relation goal never declares", async () => {
    // goal.md contains zero contradicts edges, yet five concepts contradict it.
    // Reading only outbound edges — what `activate` prints — misses all of them.
    const semantic = semanticGraph(await buildGraph(ctx));
    const hits = queryEdges(semantic, "goal", { type: "contradicts", direction: "in" });
    expect(hits).toHaveLength(5);
    expect(hits.map((hit) => hit.other.id).sort()).toEqual([
      "constraint",
      "goal-drift",
      "latent-goal",
      "need",
      "risk-asymmetry",
    ]);
  });

  test("collapses a symmetric relation instead of listing it twice", async () => {
    const semantic = semanticGraph(await buildGraph(ctx));
    const hits = queryEdges(semantic, "need", { type: "contradicts" });
    const ids = hits.map((hit) => hit.other.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  test("labels symmetric hits as undirected", async () => {
    const semantic = semanticGraph(await buildGraph(ctx));
    for (const hit of queryEdges(semantic, "goal", { type: "contradicts" })) {
      expect(hit.direction).toBe("both");
    }
  });

  test("excludes closure edges from the asserted graph", async () => {
    const asserted = await buildGraph(ctx);
    const hits = queryEdges(asserted, "decision", { type: "depends-on", direction: "out" });
    // decision asserts 5 prerequisites; context and value only appear via closure.
    expect(hits.map((hit) => hit.other.id)).not.toContain("value");
  });

  test("marks closure edges as derived", async () => {
    const semantic = semanticGraph(await buildGraph(ctx));
    const hits = queryEdges(semantic, "decision", { type: "depends-on", direction: "out" });
    const value = hits.find((hit) => hit.other.id === "value");
    expect(value?.derived).toBe(true);
  });
});

describe("activate", () => {
  test("returns one molecule and atom-only neighbours", async () => {
    const result = await activate(ctx, "decision");
    expect(result.molecule.length).toBeGreaterThan(0);
    expect(result.neighbors.length).toBeGreaterThan(0);
    expect(result.neighbors.every((neighbor) => neighbor.node.id !== "decision")).toBe(true);
  });

  test("REQUIRES includes prerequisites that are only reachable transitively", async () => {
    // The closure is precomputed server-side: a client has no graph to derive it.
    const result = await activate(ctx, "decision");
    // Asserted directly: agent, tension, constraint, goal, need.
    // Via closure: context, value.
    expect(result.requires).toContain("context");
    expect(result.requires).toContain("value");
  });

  test("neighbours carry their own REQUIRES/LOAD-NEXT and no edge arrays", async () => {
    const result = await activate(ctx, "decision");
    const neighbour = result.neighbors[0];
    expect(neighbour).toBeDefined();
    expect(Array.isArray(neighbour!.requires)).toBe(true);
    expect(Array.isArray(neighbour!.loadNext)).toBe(true);
    // Repeating the shared edge list per neighbour used to add 28 KB.
    expect("edges" in neighbour!).toBe(false);
  });

  test("the DTO is JSON-round-trippable", async () => {
    const result = await activate(ctx, "decision");
    const parsed = JSON.parse(JSON.stringify(result));
    expect(parsed.requires).toEqual(result.requires);
    expect(parsed.node.id).toBe("decision");
    // A Graph object would have blown the payload up with nodes+edges.
    expect(parsed.semantic).toBeUndefined();
  });

  test("validation strategy widens the neighbourhood beyond the default", async () => {
    const plain = await activate(ctx, "goal");
    const validation = await activate(ctx, "goal", { strategy: "validation" });
    expect(validation.visited.length).toBeGreaterThan(plain.visited.length);
  });

  test("throws a readable error for an unknown concept", async () => {
    expect(activate(ctx, "does-not-exist")).rejects.toThrow("Concept not found: does-not-exist");
  });
});

describe("search and path", () => {
  test("ranks an exact id match first", async () => {
    const results = await search(ctx, "risk");
    expect(results[0]?.id).toBe("risk");
  });

  test("finds a path through intermediate concepts", async () => {
    const result = await findPath(ctx, "risk", "value");
    expect(result.path).toEqual(["risk", "tension", "value"]);
  });

  test("returns an empty path when unreachable", async () => {
    const result = await findPath(ctx, "risk", "no-such-concept");
    expect(result.path).toHaveLength(0);
  });
});

describe("buildVizPayload", () => {
  test("includes molecules and mirrored edges", async () => {
    const payload = await buildVizPayload(ctx);
    expect(payload.nodes).toHaveLength(22);
    expect(payload.nodes.every((node) => node.molecule.length > 0)).toBe(true);
    expect(payload.meta.assertedEdgeCount).toBe(94);
    expect(payload.edges.length).toBe(101);
    expect(payload.edges.filter((edge) => edge.mirrored)).toHaveLength(7);
  });
});

describe("validation", () => {
  test("the shipped KB is clean", async () => {
    const report = await validateConcepts(ctx, { graph: true });
    expect(report.valid).toBe(true);
    expect(report.failed).toBe(0);
    expect(report.graph?.issues).toHaveLength(0);
  });

  test("reports a broken concept file without throwing", async () => {
    // A file that fails to parse makes the graph unbuildable. `validate` must
    // enumerate it, not die with a serialized ZodError.
    const dir = await copyKb();
    try {
      await writeFile(
        join(dir, "concepts", "L1-core", "broken.md"),
        "---\nid: broken\natom: x\nlayer: 9\n---\nbody\n",
      );
      const report = await validateConcepts(kb(dir), { graph: true });
      expect(report.valid).toBe(false);
      expect(report.failed).toBe(1);

      const broken = report.files.find((file) => file.filePath.includes("broken"));
      expect(broken?.valid).toBe(false);
      expect(broken?.error).toContain("layer");
      // The graph is unbuildable, so staleness carries one readable line.
      expect(report.graph?.staleness?.error).toBeString();
      expect(report.graph?.staleness?.error).not.toContain("{");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("detects a stale graph.json", async () => {
    const dir = await copyKb();
    try {
      // Drop one edge from graph.json without touching the concept files.
      const graphPath = join(dir, "graph.json");
      const parsed = JSON.parse(await Bun.file(graphPath).text());
      parsed.edges = parsed.edges.slice(1);
      await writeFile(graphPath, JSON.stringify(parsed, null, 2));

      const report = await validateGraphState(kb(dir));
      expect(report.valid).toBe(false);
      expect(report.staleness?.missing.length).toBeGreaterThan(0);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});