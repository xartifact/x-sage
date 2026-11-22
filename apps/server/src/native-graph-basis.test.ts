import { expect, test } from "bun:test";

import { createApp } from "./app.ts";
import { connectNativeTermReader } from "./native-term-reader.ts";

const basis = { kbUuid: "7".repeat(32), version: "1", manifestSha256: "a".repeat(64) };

function fixture(bodyBasis: typeof basis) {
  const methods: string[] = [];
  const worker = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch(request) {
      const method = new URL(request.url).pathname.split("/").at(-1)!;
      methods.push(method);
      const data = method === "ReadGraphSnapshot" ? {
        basis,
        terms: [{
          slug: "agent", atom: "一个主体", layer: 0, domains: ["test"],
          confidence: "confirmed", source: "personal", triggers: ["agent"],
        }],
        assertions: [],
      } : {
        basis: bodyBasis,
        key: { kind: 1, id: "1" },
        slug: "agent",
        revision: "1",
        atom: "一个主体",
        body: "\n# 原样正文\n",
      };
      const json = JSON.stringify(data);
      return new Response(json, {
        headers: { "content-type": "application/json", "content-length": String(Buffer.byteLength(json)) },
      });
    },
  });
  const app = createApp({
    kbRoot: "/nonexistent-local-kb",
    nativeTermReader: connectNativeTermReader({
      url: worker.url.origin, kbUuid: basis.kbUuid, kbSlug: "default", token: "f".repeat(64),
    }),
  });
  return { app, methods, worker };
}

test("visualizer bodies must match the exact metadata basis", async () => {
  const accepted = fixture(basis);
  try {
    const res = await accepted.app.request("/api/viz");
    expect(res.status).toBe(200);
    const payload = await res.json() as { nodes: { molecule: string }[] };
    expect(payload.nodes[0]?.molecule).toBe("# 原样正文");
    expect(accepted.methods).toEqual(["ReadGraphSnapshot", "ReadTermText"]);
  } finally {
    accepted.worker.stop(true);
  }

  for (const bodyBasis of [
    { ...basis, version: "2" },
    { ...basis, manifestSha256: "b".repeat(64) },
  ]) {
    const changed = fixture(bodyBasis);
    try {
      const res = await changed.app.request("/api/viz");
      expect(res.status).toBe(503);
      expect(await res.json()).toEqual({ error: "Database unavailable" });
      expect(changed.methods).toEqual(["ReadGraphSnapshot", "ReadTermText"]);
    } finally {
      changed.worker.stop(true);
    }
  }
});

test("visualizer stats require graph metadata only", async () => {
  const changed = fixture({ ...basis, version: "2" });
  try {
    const res = await changed.app.request("/api/viz?format=text");
    expect(res.status).toBe(200);
    expect(await res.text()).toContain("concepts: 1\nedges:    0 (0 asserted, 0 mirrored)");
    expect(changed.methods).toEqual(["ReadGraphSnapshot"]);
  } finally {
    changed.worker.stop(true);
  }
});
