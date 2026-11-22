import { expect, test } from "bun:test";
import { appendFrontierRow, parseFrontierMarkdown } from "@x-sage/core/frontier";
import { createApp } from "./app.ts";
import { connectNativeFrontierDocument } from "./native-frontier-document.ts";

const KB_UUID = "77".repeat(16);
const READ_TOKEN = "2a".repeat(32);
const WRITE_TOKEN = "2b".repeat(32);
const PUBLIC_TOKEN = "public-write";
const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { ignoreBOM: true });
const OCTETS = "application/octet-stream";

test("public Frontier keeps raw document semantics across a large read, stale CAS and duplicate", async () => {
  const concurrent = { concept: "another", source: "right", reason: "concurrent", priority: "low" } as const;
  const entry = {
    concept: "Δ | old",
    source: "source\n| extra | row | with a | parse projection |",
    reason: "x".repeat(255 * 1024),
    priority: "high",
  } as const;
  const seed = "\uFEFF# 原始文档\r\n\r\n## 未决概念\r\n"
    + "| 概念 | 来源 | 未解问题 | 优先级 |\r\n|---|---|---|---|\r\n"
    + "| seed | its-source | unchanged | medium |\r\n\r\n"
    + "## 另一节\r\n" + "p".repeat(136 * 1024) + "\r\n";
  const body = JSON.stringify(entry);
  expect(encoder.encode(body).length).toBeLessThanOrEqual(256 * 1024);
  expect(encoder.encode(seed).length).toBeGreaterThan(128 * 1024);
  let raw = seed;
  let revision = 1n;
  let writes = 0;
  const requestIds = new Set<string>();
  const db = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      expect(new URL(request.url).pathname).toBe("/db/v1/frontier-document");
      expect(request.headers.get("authorization")).toBe(
        `Bearer ${request.method === "GET" ? READ_TOKEN : WRITE_TOKEN}`,
      );
      if (request.method === "GET") {
        const text = encoder.encode(raw);
        const body = new Uint8Array(25 + text.byteLength);
        body.set(Buffer.from(KB_UUID, "hex"));
        new DataView(body.buffer).setBigUint64(16, revision, true);
        body[24] = 1;
        body.set(text, 25);
        return new Response(body, {
          headers: { "content-type": OCTETS, "content-length": String(body.byteLength) },
        });
      }
      expect(request.method).toBe("PUT");
      expect(request.headers.get("content-type")).toBe(OCTETS);
      const body = await request.arrayBuffer();
      expect(body.byteLength).toBeGreaterThan(256 * 1024);
      const bytes = new Uint8Array(body);
      expect(Buffer.from(bytes.subarray(0, 16)).toString("hex")).toBe(KB_UUID);
      const requestId = decoder.decode(bytes.subarray(24, 56));
      expect(requestId).toMatch(/^[0-9a-f]{32}$/);
      expect(requestIds.has(requestId)).toBeFalse();
      requestIds.add(requestId);
      writes++;
      if (writes === 1) {
        raw = appendFrontierRow(raw, concurrent);
        revision++;
        return new Response(null, { status: 412 });
      }
      expect(new DataView(body).getBigUint64(16, true)).toBe(revision);
      raw = decoder.decode(bytes.subarray(56));
      revision++;
      return new Response(null, { status: 204 });
    },
  });
  try {
    // A filesystem fallback cannot recover a document from this nonexistent KB.
    const app = createApp({
      kbRoot: "/frontier-path-must-not-be-opened",
      writeToken: PUBLIC_TOKEN,
      nativeTermReader: {
        kbSlug: "default",
        async readTermText() { throw new Error("unexpected term read"); },
        async readTermDocument() { throw new Error("unexpected term read"); },
        frontier: connectNativeFrontierDocument({
          url: db.url.toString(),
          kbUuid: KB_UUID,
          readToken: READ_TOKEN,
          writeToken: WRITE_TOKEN,
        }),
      },
    });
    const post = (token?: string, postBody = body) => app.request("/api/frontier", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        ...(token ? { authorization: `Bearer ${token}` } : {}),
      },
      body: postBody,
    });

    expect(await (await app.request("/api/frontier?format=text")).text())
      .toBe(JSON.stringify(parseFrontierMarkdown(seed), null, 2) + "\n");
    expect((await post()).status).toBe(401);
    expect(writes).toBe(0);
    const response = await post(PUBLIC_TOKEN);
    expect(response.status).toBe(201);
    expect(await response.json()).toEqual(entry);
    const expected = appendFrontierRow(appendFrontierRow(seed, concurrent), entry);
    expect(raw).toBe(expected);
    expect(writes).toBe(2);
    expect(await (await app.request("/api/frontier")).json()).toEqual(parseFrontierMarkdown(expected));
    // The legacy comparison matches individual full lines. A rendered row
    // containing an embedded newline is appended again, exactly as before.
    expect((await post(PUBLIC_TOKEN)).status).toBe(201);
    const repeated = appendFrontierRow(expected, entry);
    expect(repeated).not.toBe(expected);
    expect(writes).toBe(3);
    expect(raw).toBe(repeated);

    const single = { concept: "single", source: "a | b", reason: "c", priority: "medium" } as const;
    expect((await post(PUBLIC_TOKEN, JSON.stringify(single))).status).toBe(201);
    const withSingle = appendFrontierRow(repeated, single);
    expect((await post(PUBLIC_TOKEN, JSON.stringify(single))).status).toBe(201);
    expect(writes).toBe(4);
    expect(raw).toBe(withSingle);
    expect((await app.request("/api/frontier?kb=foreign")).status).toBe(404);
    expect((await app.request("/api/search?q=seed")).status).toBe(503);
  } finally {
    await db.stop(true);
  }
});
