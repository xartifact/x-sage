import { expect, test } from "bun:test";
import type { SubmitProposalRequest } from "../../../packages/protocol/src/generated.ts";
import { createApp } from "./app.ts";
import { connectNativeTermReader } from "./native-term-reader.ts";

test("an authorized old edge POST submits the unresolved slug with only its real source baseline", async () => {
  const kbUuid = "7".repeat(32);
  const basis = { kbUuid, version: "6", manifestSha256: "a".repeat(64) };
  const readToken = "1".repeat(64);
  const proposeToken = "2".repeat(64);
  const reviewToken = "3".repeat(64);
  const writeToken = "public-bun-write";
  const sourceKey = { kind: 1, id: "12" };
  const proposal = { kind: 7, id: "31" };
  const from = "agent";
  const target = "yet-uncreated-target";
  let alias: string | undefined;
  let calls = 0;
  const worker = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      calls++;
      function respond(body: unknown, status = 200) {
        const raw = JSON.stringify(body);
        return new Response(raw, { status, headers: { "Content-Type": "application/json",
          "Content-Length": String(Buffer.byteLength(raw)) } });
      }
      const method = new URL(request.url).pathname.split("/").at(-1);
      if (method === "ReadGraphSnapshot") {
        expect(request.headers.get("authorization")).toBe(`Bearer ${readToken}`);
        return respond({ basis, terms: [{
          key: sourceKey, revision: "3", slug: from, atom: "Author",
          layer: 1, domains: ["test"], source: "personal", confidence: "confirmed", triggers: ["agent"],
        }], assertions: [] });
      }
      if (method === "SubmitProposal") {
        expect(request.headers.get("authorization")).toBe(`Bearer ${proposeToken}`);
        const data = await request.json() as SubmitProposalRequest;
        expect(data.envelope?.expectVersion).toBe("6");
        expect(data.envelope?.readSet).toEqual([{ key: sourceKey, revision: "3" }]);
        expect(data.payload?.legacyRelation).toBeUndefined();
        const edge = data.payload?.legacyRelationBySlug;
        expect(edge).toEqual({
          filename: expect.stringMatching(/^\d{4}-\d{2}-\d{2}-agent-enables-yet-uncreated-target\.md$/),
          from: sourceKey, targetSlug: target, relation: 4, confidence: 2,
          declarationOrder: 0, declaredAuthor: "CLI", reason: "(待填写)",
        });
        alias = edge!.filename;
        return respond({ outcome: { committed: { recorded: { affected: [{ key: proposal, revision: "1" }] } } } });
      }
      if (method === "ReadProposalFiles") {
        expect(request.headers.get("authorization")).toBe(`Bearer ${readToken}`);
        return respond({ basis, files: [], examples: [], nativeFiles: alias ? [{
          directory: "edges", filename: alias, status: "pending", key: proposal, revision: "1",
          subjectSlug: from, edge: { targetSlug: target, relation: "enables", confidence: "confirmed" },
        }] : [] });
      }
      expect(method).toBe("ReviewProposal");
      expect(request.headers.get("authorization")).toBe(`Bearer ${reviewToken}`);
      return respond({ outcome: { error: { code: "ERROR_CODE_LEGACY_PROPOSAL_TARGET_UNRESOLVED" } } }, 422);
    },
  });
  const app = createApp({ kbRoot: "/no-legacy-authority", writeToken,
    nativeTermReader: connectNativeTermReader({ kbUuid, kbSlug: "default", url: worker.url.origin,
      token: readToken, proposalProposeToken: proposeToken, proposalReviewToken: reviewToken }),
  });
  const request = (path: string, body: unknown, auth?: string) => app.request(path, {
    method: "POST", headers: {
      "Content-Type": "application/json", ...(auth ? { authorization: `Bearer ${auth}` } : {}),
    }, body: JSON.stringify(body),
  });
  const draft = { kind: "edge", from, type: "enables", target, confidence: "confirmed" };
  try {
    expect((await request("/api/proposals", draft)).status).toBe(401);
    expect((await request("/api/proposals", draft, proposeToken)).status).toBe(401);
    expect(calls).toBe(0);
    const created = await request("/api/proposals", draft, writeToken);
    expect(created.status).toBe(201);
    expect(await created.json()).toEqual({ file: alias });
    const merged = await request("/api/proposals/merge", { file: alias }, writeToken);
    expect(merged.status).toBe(422);
    expect(await merged.json()).toEqual({
      merged: [], failed: [{ file: alias, error: `Unresolved proposal target: ${target}` }],
      conflicts: [], derivedStale: false,
    });
    const listed = await app.request("/api/proposals");
    expect(listed.status).toBe(200);
    expect(await listed.json()).toEqual({
      edges: [{ file: alias, from, type: "enables", target, confidence: "confirmed", status: "pending", date: "" }],
      atoms: [], bodies: [],
    });
  } finally {
    worker.stop(true);
  }
});
