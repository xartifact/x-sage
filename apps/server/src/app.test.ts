import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { createApp } from "../src/app.ts";

/**
 * HTTP contract tests.
 *
 * The app is exercised in-process via `app.request` — no port, no server
 * lifecycle, so these stay fast and cannot collide with a running dev server.
 */

const app = createApp({ kbRoot: resolve(import.meta.dir, "../../../kb/default") });

/**
 * `Response.json()` is typed `unknown`. These tests assert specific payload
 * shapes, so narrow once here rather than casting at each call site.
 */
async function json(res: Response): Promise<any> {
  return res.json();
}

describe("read endpoints", () => {
  test("health reports the KB in use", async () => {
    const res = await app.request("/api/health");
    expect(res.status).toBe(200);
    const body = await json(res);
    expect(body.ok).toBe(true);
    expect(body.kb).toEndWith("kb/default");
  });

  test("health reports build identity for deployment assertions", async () => {
    // Deployment compares these against the pushed commit/ref. If any hop drops
    // the env var the container reports defaults and a stale image passes.
    const body = await json(await app.request("/api/health"));
    expect(body).toHaveProperty("version");
    expect(body).toHaveProperty("commitHash");
    expect(body).toHaveProperty("buildRef");
  });

  test("meta reports graph size", async () => {
    const body = await json(await app.request("/api/meta"));
    expect(body.nodes).toBe(22);
    expect(body.edges).toBe(94);
  });

  test("viz payload carries molecules and meta", async () => {
    const body = await json(await app.request("/api/viz"));
    expect(body.nodes).toHaveLength(22);
    expect(body.meta.assertedEdgeCount).toBe(94);
    expect(body.edges.length).toBe(101);
  });

  test("search returns ranked results", async () => {
    const body = await json(await app.request("/api/search?q=risk"));
    expect(body.results[0].id).toBe("risk");
  });

  test("concept activation omits the semantic graph but keeps the markdown", async () => {
    const body = await json(await app.request("/api/concepts/decision"));
    expect(body.concept).toBe("decision");
    expect(body.markdown.length).toBeGreaterThan(0);
    // Shipping the full semantic graph would dwarf the useful payload.
    expect(body.semantic).toBeUndefined();
    expect(body.neighbors.length).toBeGreaterThan(0);
  });

  test("edges answers inbound queries", async () => {
    const body = await json(await app.request("/api/concepts/goal/edges?type=contradicts&dir=in"));
    expect(body.edges).toHaveLength(5);
  });

  test("path returns the node sequence", async () => {
    const body = await json(await app.request("/api/path?from=risk&to=value"));
    expect(body.path).toEqual(["risk", "tension", "value"]);
  });

  test("validate returns 200 for a clean KB", async () => {
    const res = await app.request("/api/validate?graph=true");
    expect(res.status).toBe(200);
    const body = await json(res);
    expect(body.valid).toBe(true);
  });
});

describe("error handling", () => {
  test("unknown concept is 404", async () => {
    const res = await app.request("/api/concepts/nope");
    expect(res.status).toBe(404);
    expect((await json(res)).error).toContain("Concept not found");
  });

  test("missing query parameter is 400", async () => {
    expect((await app.request("/api/search")).status).toBe(400);
    expect((await app.request("/api/path?from=risk")).status).toBe(400);
  });

  test("invalid enum values are 400", async () => {
    expect((await app.request("/api/concepts/goal/edges?type=bogus")).status).toBe(400);
    expect((await app.request("/api/concepts/goal/edges?dir=sideways")).status).toBe(400);
    expect((await app.request("/api/concepts/goal?strategy=bogus")).status).toBe(400);
  });

  test("a malformed body is rejected without writing", async () => {
    const res = await app.request("/api/proposals", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ from: "risk", type: "not-a-relation", target: "goal" }),
    });
    expect(res.status).toBe(400);
  });

  test("an unknown route is 404", async () => {
    expect((await app.request("/api/nope")).status).toBe(404);
  });
});
/**
 * Client-facing contract.
 *
 * These assert the properties a remote CLI depends on: text is rendered
 * server-side, the two edge views differ by request, writes are gated, and the
 * payload no longer repeats the edge list once per neighbour.
 */
describe("client contract", () => {
  test("format=text returns the AI transcript, not JSON", async () => {
    const res = await app.request("/api/concepts/decision?format=text");
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toContain("text/plain");
    const body = await res.text();
    expect(body).toStartWith("[CONCEPT: decision]");
    expect(body).toContain("REQUIRES: [");
  });

  test("format=markdown returns the concept file verbatim", async () => {
    const res = await app.request("/api/concepts/goal?format=markdown");
    expect(res.headers.get("content-type")).toContain("text/markdown");
    expect(await res.text()).toStartWith("---\nid: goal");
  });

  test("the manifest endpoint exists and renders the table", async () => {
    const res = await app.request("/api/manifest");
    expect(res.status).toBe(200);
    expect(await res.text()).toContain("| id | atom | layer | source | confidence | triggers |");
  });

  test("edges without derived excludes closure, with derived includes it", async () => {
    const asserted = await json(await app.request("/api/concepts/decision/edges?type=depends-on&dir=out"));
    const derived = await json(
      await app.request("/api/concepts/decision/edges?type=depends-on&dir=out&derived=true"),
    );
    expect(asserted.edges).toHaveLength(5);
    expect(derived.edges.length).toBeGreaterThan(asserted.edges.length);
    expect(derived.edges.some((edge: { derived: boolean }) => edge.derived)).toBe(true);
    expect(asserted.edges.some((edge: { derived: boolean }) => edge.derived)).toBe(false);
  });

  test("activation ships REQUIRES/LOAD-NEXT instead of a graph", async () => {
    const body = await json(await app.request("/api/concepts/decision"));
    expect(body.semantic).toBeUndefined();
    expect(body.requires).toContain("value");
    expect(body.loadNext.length).toBeGreaterThan(0);
    // Neighbours used to repeat the whole edge array (28 KB of a 38 KB body).
    expect(body.neighbors[0].edges).toBeUndefined();
    // Present as arrays even when a neighbour has no prerequisites (agent has
    // none): the renderer indexes them unconditionally.
    expect(Array.isArray(body.neighbors[0].requires)).toBe(true);
    expect(body.neighbors.some((neighbor: { loadNext: string[] }) => neighbor.loadNext.length > 0)).toBe(true);
  });

  test("responses are gzipped when the client asks", async () => {
    const res = await app.request("/api/viz", { headers: { "accept-encoding": "gzip" } });
    expect(res.status).toBe(200);
    expect(res.headers.get("content-encoding")).toBe("gzip");
  });

  test("errors follow the requested format", async () => {
    const asJson = await app.request("/api/concepts/nope");
    expect((await json(asJson)).error).toContain("Concept not found");

    const asText = await app.request("/api/concepts/nope?format=text");
    expect(asText.headers.get("content-type")).toContain("text/plain");
    expect(await asText.text()).toBe("Concept not found: nope\n");
  });
});

describe("write protection", () => {
  const guarded = createApp({ kbRoot: resolve(import.meta.dir, "../../../kb/default"), writeToken: "s3cret" });

  test("reads are allowed without a token", async () => {
    expect((await guarded.request("/api/health")).status).toBe(200);
    expect((await guarded.request("/api/concepts/decision")).status).toBe(200);
  });

  test("writes without a token are rejected as JSON", async () => {
    const res = await guarded.request("/api/proposals/merge", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ file: "nope.md" }),
    });
    expect(res.status).toBe(401);
    expect((await json(res)).error).toContain("Unauthorized");
  });

  test("a wrong token is rejected", async () => {
    const res = await guarded.request("/api/proposals/merge", {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer wrong" },
      body: JSON.stringify({ file: "nope.md" }),
    });
    expect(res.status).toBe(401);
  });

  test("the configured token is accepted", async () => {
    const res = await guarded.request("/api/proposals/merge", {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer s3cret" },
      body: JSON.stringify({ file: "nope.md" }),
    });
    expect(res.status).toBe(200);
  });

  test("an oversized body is rejected before parsing", async () => {
    const res = await app.request("/api/proposals/merge", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "x".repeat(300 * 1024),
    });
    expect(res.status).toBe(413);
  });
});
