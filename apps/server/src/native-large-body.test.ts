import { expect, test } from "bun:test";

import { createApp } from "./app.ts";
import { connectNativeTermReader } from "./native-term-reader.ts";

const basis = { kbUuid: "7".repeat(32), version: "6", manifestSha256: "a".repeat(64) };
const body = "\ufeff# 中文 \n\n" + "📘\"\\\n".repeat(12_000);
const readToken = "a".repeat(64);
const writeToken = "b".repeat(64);
const term = {
  id: "long", atom: "A long concept", layer: 0, domain: ["test"],
  confidence: "confirmed", source: "personal", triggers: ["long"], edges: [],
};

function json(value: unknown) {
  const raw = JSON.stringify(value);
  return new Response(raw, {
    headers: { "content-type": "application/json", "content-length": String(Buffer.byteLength(raw)) },
  });
}

test("large direct creation sends UTF-8 once, outside typed JSON metadata", async () => {
  const worker = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      if (new URL(request.url).pathname.endsWith("/ReadGraphSnapshot")) {
        return json({ basis, terms: [], assertions: [] });
      }
      expect(new URL(request.url).pathname.endsWith("/CommitDirectConcept")).toBe(true);
      expect(request.headers.get("content-type")).toBe("application/octet-stream");
      expect(request.headers.get("authorization")).toBe(`Bearer ${writeToken}`);
      const frame = Buffer.from(await request.arrayBuffer());
      const end = 4 + frame.readUInt32LE(0);
      const requestMetadata = JSON.parse(frame.subarray(4, end).toString("utf8"));
      expect(requestMetadata.envelope.expectVersion).toBe(basis.version);
      expect(requestMetadata.payload.directConceptCreate.term.body).toBe("");
      expect(frame.subarray(end).toString("utf8")).toBe(body + "\n");
      return json({ outcome: { committed: { recorded: {
        commitVersion: "7", affected: [{ key: { kind: 1, id: "42" }, revision: "1" }],
      } } } });
    },
  });
  const app = createApp({
    kbRoot: "/no-md-files-for-native-create",
    nativeTermReader: connectNativeTermReader({
      url: worker.url.origin, kbUuid: basis.kbUuid, kbSlug: "default", token: readToken, writeConceptToken: writeToken,
    }),
  });
  try {
    const response = await app.request("/api/concepts", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({ frontmatter: term, body }),
    });
    expect(response.status).toBe(201);
  } finally {
    worker.stop(true);
  }
});

test("term octet reads retain Unicode and refuse out-of-bounds metadata", async () => {
  let malformed = false;
  const worker = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch(request) {
      expect(request.headers.get("accept")).toBe("application/octet-stream");
      const metadata = Buffer.from(JSON.stringify({ basis, key: { kind: 1, id: "42" }, revision: "1", slug: "long", atom: "A long concept" }));
      const prefix = Buffer.alloc(4);
      prefix.writeUInt32LE(malformed ? 128 * 1024 + 1 : metadata.length);
      const bytes = Buffer.concat([prefix, metadata, Buffer.from(body)]);
      return new Response(bytes, {
        headers: { "content-type": "application/octet-stream", "content-length": String(bytes.length) },
      });
    },
  });
  const reader = connectNativeTermReader({
    url: worker.url.origin, kbUuid: basis.kbUuid, kbSlug: "default", token: readToken,
  });
  try {
    expect(await reader.readTermText("long")).toBe(body);
    malformed = true;
    await expect(reader.readTermText("long")).rejects.toThrow();
  } finally {
    worker.stop(true);
  }
});
