import { expect, test } from "bun:test";
import type { CommitCommandRequest, Envelope } from "../../../packages/protocol/src/generated.ts";
import { createApp } from "./app.ts";
import { connectNativeTermReader } from "./native-term-reader.ts";

const KB = "1".repeat(32);
const READ = "2".repeat(64);
const WRITE = "3".repeat(64);
const PUBLIC = "isolated-public-token";

const concept = {
  frontmatter: {
    id: "audit-create",
    atom: "Verify the shape of the deployed legacy write",
    layer: 1,
    domain: ["delivery"],
    confidence: "confirmed",
    source: "personal",
    triggers: ["audit"],
    edges: [{ type: "depends-on", target: "agent", confidence: "confirmed" }],
  },
  body: "# Audit\n\nA locally isolated test concept.",
};

function rpcJson(value: unknown, status = 200) {
  const json = JSON.stringify(value);
  return new Response(json, {
    status,
    headers: { "content-type": "application/json", "content-length": String(Buffer.byteLength(json)) },
  });
}

test("Bun preserves the deployed concept-create contract and sends only the dedicated write credential", async () => {
  const basis = (version: string) => ({ kbUuid: KB, version, manifestSha256: "4".repeat(64) });
  const agent = {
    slug: "agent", atom: "Agent", layer: 0, domains: ["test"],
    confidence: "confirmed", source: "personal", triggers: ["agent"], key: { kind: 1, id: "7" }, revision: "6",
  };
  const target = {
    slug: "other", atom: "Other", layer: 0, domains: ["test"],
    confidence: "confirmed", source: "domain", triggers: ["other"], key: { kind: 1, id: "12" }, revision: "2",
  };
  const newTerm = {
    slug: "audit-create", atom: concept.frontmatter.atom, layer: 1, domains: ["delivery"],
    confidence: "confirmed", source: "personal", triggers: ["audit"], key: { kind: 1, id: "13" }, revision: "1",
  };
  const pending = { key: { kind: 2, id: "12" }, revision: "1", relation: "depends-on",
    fromSlug: "audit-create", targetSlug: "agent", confidence: "confirmed", declarationOrder: 0 };
  let published = false;
  let reads = 0;
  let writes = 0;
  let simulation: "success" | "unknown" | "duplicate" = "success";
  const backend = Bun.serve({
    hostname: "127.0.0.1", port: 0,
    async fetch(request) {
      const method = new URL(request.url).pathname;
      const data = await request.json() as CommitCommandRequest & { envelope: Envelope; slug?: string };
      expect(data.envelope.kbUuid).toBe(KB);
      if (method.endsWith("/ReadGraphSnapshot")) {
        expect(request.headers.get("authorization")).toBe(`Bearer ${READ}`);
        reads += 1;
        return rpcJson({
          basis: basis(published ? "10" : "9"),
          terms: published ? [agent, target, newTerm] : [agent, target],
          assertions: published ? [pending] : [],
        });
      }
      if (method.endsWith("/ReadTermDocument")) {
        expect(request.headers.get("authorization")).toBe(`Bearer ${READ}`);
        expect(data.slug).toBe("audit-create");
        return rpcJson({
          text: { basis: basis("10"), key: newTerm.key, revision: newTerm.revision, slug: newTerm.slug,
            body: concept.body + "\n", atom: newTerm.atom },
          ...newTerm, assertions: [pending],
        });
      }
      expect(method.endsWith("/CommitCommand")).toBe(true);
      expect(request.headers.get("authorization")).toBe(`Bearer ${WRITE}`);
      writes += 1;
      expect(data.envelope.expectVersion).toBe(published ? "10" : "9");
      if (simulation === "unknown") {
        expect(data.envelope.readSet).toEqual([
          { key: agent.key, revision: "6" },
          { key: target.key, revision: "2" },
        ]);
        expect(data.payload?.directConceptCreate?.assertions.map((item) => item.target?.id))
          .toEqual(["12", "7"]);
        return rpcJson({ outcome: { unknown: { requestId: data.envelope.requestId } } });
      }
      if (simulation === "duplicate") {
        return rpcJson({ error: { code: "ERROR_CODE_CONCEPT_SLUG_EXISTS" } }, 409);
      }
      expect(data.envelope.readSet).toEqual([{ key: agent.key, revision: "6" }]);
      expect(data.payload?.directConceptCreate).toEqual({
        term: {
          slug: "audit-create", atom: concept.frontmatter.atom, body: concept.body + "\n",
          layer: 1, domains: ["delivery"], confidence: 2, source: 0, triggers: ["audit"],
          declaredAuthor: "", reason: "Created through POST /api/concepts",
        },
        assertions: [{ target: agent.key, relation: 3, confidence: 2 }],
      });
      published = true;
      return rpcJson({ outcome: { committed: { recorded: {
        affected: [
          { key: newTerm.key, revision: "1" },
          { key: pending.key, revision: "1" },
        ],
        commitVersion: "10",
      } } } });
    },
  });

  try {
    const app = createApp({
      kbRoot: "/no-old-authority-fallback",
      writeToken: PUBLIC,
      nativeTermReader: connectNativeTermReader({
        url: backend.url.href, kbUuid: KB, kbSlug: "default", token: READ, writeConceptToken: WRITE,
      }),
    });
    function post(input: typeof concept, publicToken?: string) {
      return app.request("/api/concepts", {
        method: "POST",
        headers: { "content-type": "application/json",
          ...(publicToken === undefined ? {} : { authorization: `Bearer ${publicToken}` }) },
        body: JSON.stringify(input),
      });
    }
    expect((await post(concept)).status).toBe(401);
    expect((await post(concept, WRITE)).status).toBe(401);
    expect(reads + writes).toBe(0);
    expect((await post({ ...concept, body: " " }, PUBLIC)).status).toBe(400);
    expect(reads + writes).toBe(0);
    const dangling = { ...concept, frontmatter: { ...concept.frontmatter,
      edges: [{ ...concept.frontmatter.edges[0]!, target: "not-present" }] } };
    const invalid = await post(dangling, PUBLIC);
    expect(invalid.status).toBe(400);
    expect(await invalid.json()).toEqual({ error: "Edge references unknown target node: not-present" });
    expect(writes).toBe(0);
    const created = await post(concept, PUBLIC);
    expect(created.status).toBe(201);
    expect(await created.text()).toBe('{"id":"audit-create"}\n');
    const readback = await app.request("/api/concepts/audit-create/body");
    expect(readback.status).toBe(200);
    const json = await readback.json() as { body: string; frontmatter: typeof concept.frontmatter };
    expect(json.body).toBe(concept.body + "\n");
    expect(json.frontmatter.edges).toEqual(concept.frontmatter.edges);
    const duplicate = await post(concept, PUBLIC);
    expect(duplicate.status).toBe(409);
    expect(await duplicate.json()).toEqual({ error: "Concept already exists" });
    expect(writes).toBe(1);

    simulation = "duplicate";
    const novel = { ...concept, frontmatter: { ...concept.frontmatter, id: "new-slug" } };
    const raced = await post(novel, PUBLIC);
    expect(raced.status).toBe(409);
    expect(await raced.json()).toEqual({ error: "Concept already exists" });
    simulation = "unknown";
    const unknown = await post({
      ...novel, frontmatter: {
        ...novel.frontmatter, edges: [
          { type: "enables", target: "other", confidence: "hypothetical" },
          ...novel.frontmatter.edges,
        ],
      },
    }, PUBLIC);
    expect(unknown.status).toBe(503);
    expect(await unknown.json()).toEqual({ error: "Creation outcome is unknown; check the concept before retrying" });
    expect(writes).toBe(3);
  } finally {
    await backend.stop(true);
  }
});
