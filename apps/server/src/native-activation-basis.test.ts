import { createHash } from "node:crypto";
import { expect, test } from "bun:test";

import { createApp } from "./app.ts";
import { connectNativeTermReader } from "./native-term-reader.ts";

const basis = { kbUuid: "7".repeat(32), version: "1", manifestSha256: "a".repeat(64) };
const markdown = "---\nid: agent\natom: a concept\nlayer: 0\ndomain: [test]\nconfidence: confirmed\nsource: personal\ntriggers: [agent]\n---\n\n# Text\n";
const original = {
  bytesBase64: Buffer.from(markdown).toString("base64"),
  sha256: createHash("sha256").update(markdown).digest("hex"),
  length: String(Buffer.byteLength(markdown)),
};
const term = {
  slug: "agent",
  atom: "a concept",
  layer: 0,
  domains: ["test"],
  confidence: "confirmed",
  source: "personal",
  triggers: ["agent"],
};

function fixture(change: { bodyBasis?: typeof basis; changedEdge?: boolean; changedSource?: boolean; missingSource?: boolean }) {
  const methods: string[] = [];
  const worker = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch(request) {
      const method = new URL(request.url).pathname.split("/").at(-1)!;
      methods.push(method);
      const data = method === "ReadGraphSnapshot" ? {
        basis,
        terms: [term, { ...term, slug: "neighbor" }],
        assertions: change.changedEdge ? [{ fromSlug: "agent", targetSlug: "neighbor", relation: "enables", confidence: "confirmed" }] : [],
      } : {
        text: {
          basis: change.bodyBasis ?? basis,
          key: { kind: 1, id: "1" },
          slug: "agent",
          revision: "1",
          atom: "a concept",
          body: "\n# Text\n",
        },
        ...(change.missingSource ? {} : {
          original: change.changedSource ? { ...original, sha256: "b".repeat(64) } : original,
        }),
      };
      const json = JSON.stringify(data);
      return new Response(json, { headers: { "content-type": "application/json", "content-length": String(Buffer.byteLength(json)) } });
    },
  });
  const app = createApp({
    kbRoot: "/no-files-for-native-activation",
    nativeTermReader: connectNativeTermReader({
      url: worker.url.origin, kbUuid: basis.kbUuid, kbSlug: "default", token: "f".repeat(64),
    }),
  });
  return { app, methods, worker };
}

test("activation keeps the exact archived spelling only when the current graph and body match", async () => {
  const archived = fixture({});
  try {
    const response = await archived.app.request("/api/concepts/agent?format=markdown");
    expect(response.status).toBe(200);
    expect(await response.text()).toBe(markdown + "\n");
    expect(archived.methods).toEqual(["ReadGraphSnapshot", "ReadTermActivation"]);
  } finally {
    archived.worker.stop(true);
  }

  const newer = fixture({ changedEdge: true });
  try {
    const response = await newer.app.request("/api/concepts/agent");
    expect(response.status).toBe(200);
    const value = await response.json() as { markdown: string; loadNext: string[] };
    expect(value.markdown).not.toBe(markdown);
    expect(value.markdown).toContain("target: neighbor");
    expect(value.loadNext).toEqual(["neighbor"]);
  } finally {
    newer.worker.stop(true);
  }

  const withoutOriginal = fixture({ missingSource: true });
  try {
    const response = await withoutOriginal.app.request("/api/concepts/agent?format=markdown");
    expect(response.status).toBe(200);
    expect(await response.text()).toContain("# Text");
  } finally {
    withoutOriginal.worker.stop(true);
  }
});

test("activation rejects changed database identity or unverified source bytes", async () => {
  for (const setting of [
    { bodyBasis: { ...basis, version: "2" } },
    { bodyBasis: { ...basis, manifestSha256: "b".repeat(64) } },
    { changedSource: true },
  ]) {
    const changed = fixture(setting);
    try {
      const response = await changed.app.request("/api/concepts/agent");
      expect(response.status).toBe(503);
      expect(await response.json()).toEqual({ error: "Database unavailable" });
    } finally {
      changed.worker.stop(true);
    }
  }
});
