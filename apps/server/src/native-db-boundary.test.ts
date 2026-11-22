import { expect, test } from "bun:test";
import { resolve } from "node:path";
import { conceptRevision } from "@x-sage/core/manifest";
import { ConceptFrontmatterSchema } from "@x-sage/core/types";

import { createApp } from "./app.ts";

const fixture = resolve(import.meta.dir, "../../../kb/default");

test("a configured database supplies the body and Bun owns its public format", async () => {
  const called: string[] = [];
  const app = createApp({
    kbRoot: "/missing-local-kb",
    nativeTermReader: {
      kbSlug: "default",
      async readTermDocument() { throw new Error("document path is only used for JSON"); },
      async readTermText(slug) {
        called.push(slug);
        return "# 数据库正文\n";
      },
    },
  });
  const response = await app.request("/api/concepts/agent/body?format=markdown&kb=default");
  expect(response.status).toBe(200);
  expect(response.headers.get("content-type")).toBe("text/markdown; charset=utf-8");
  expect(await response.text()).toBe("# 数据库正文\n");
  expect(called).toEqual(["agent"]);
});

test("configured database errors and missing routes cannot fall through to compatible local storage", async () => {
  const app = createApp({
    kbRoot: fixture,
    writeToken: "public-token",
    nativeTermReader: {
      kbSlug: "default",
      async readTermDocument() { throw new Error("upstream offline"); },
      async readTermText(slug) {
        if (slug === "missing") return null;
        throw new Error("internal upstream detail");
      },
    },
  });
  const failed = await app.request("/api/concepts/agent/body?format=markdown");
  expect(failed.status).toBe(503);
  expect(await failed.text()).toBe("Database unavailable\n");
  expect((await app.request("/api/concepts/missing/body?format=markdown")).status).toBe(404);
  expect((await app.request("/api/concepts/agent/body?format=markdown&kb=foreign")).status).toBe(404);
  expect((await app.request("/api/concepts/agent/body")).status).toBe(503);
  expect((await app.request("/api/graph")).status).toBe(503);
  const write = await app.request("/api/proposals", {
    method: "POST",
    headers: { authorization: "Bearer public-token", "content-type": "application/json" },
    body: JSON.stringify({ kind: "atom", conceptId: "agent", proposedAtom: "never write" }),
  });
  expect(write.status).toBe(503);
});

test("legacy concept JSON uses one database snapshot and calculates the legacy revision", async () => {
  const body = "\r\n# 原样正文\r\n";
  const frontmatter = ConceptFrontmatterSchema.parse({
    id: "agent",
    atom: "正文的前置条件",
    layer: 0,
    domain: ["test"],
    confidence: "confirmed",
    source: "personal",
    triggers: ["agent"],
    edges: [
      { type: "depends-on", target: "value", confidence: "hypothetical" },
      { type: "contradicts", target: "tension", confidence: "confirmed" },
    ],
  });
  const observed: string[] = [];
  const app = createApp({
    kbRoot: "/missing-local-kb",
    nativeTermReader: {
      kbSlug: "default",
      async readTermText() { throw new Error("JSON must use the document snapshot"); },
      async readTermDocument(slug) {
        observed.push(slug);
        return { body, frontmatter };
      },
    },
  });
  const response = await app.request("/api/concepts/agent/body");
  expect(response.status).toBe(200);
  expect(await response.json()).toEqual({
    conceptId: "agent",
    body,
    revision: conceptRevision(frontmatter, body),
    frontmatter,
    atom: frontmatter.atom,
  });
  expect(observed).toEqual(["agent"]);
});
