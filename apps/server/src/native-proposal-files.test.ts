import { createHash } from "node:crypto";
import { expect, test } from "bun:test";

import { createApp } from "./app.ts";
import { connectNativeTermReader } from "./native-term-reader.ts";

const basis = { kbUuid: "7".repeat(32), version: "6", manifestSha256: "a".repeat(64) };
const supportedBody = JSON.stringify({
  file: "owner-cannot-rename.json", conceptId: "agent", expectedRevision: "e".repeat(64),
  proposedBody: "\n# New\n", reason: "test review", date: "original date", status: "pending",
});

function example(file: string, status: string) {
  const raw = `---\nstatus: ${status}\ndate: exact\n---\n- **from**: agent\n- **type**: enables\n- **target**: value\n- **confidence**: confirmed\n`;
  return {
    path: `proposals/edges/${file}`, bytesBase64: Buffer.from(raw).toString("base64"),
    length: String(Buffer.byteLength(raw)), sha256: createHash("sha256").update(raw).digest("hex"),
  };
}

function fixture(documentBasis = basis) {
  const calls: string[] = [];
  const worker = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch(request) {
      const method = new URL(request.url).pathname.split("/").at(-1)!;
      calls.push(method);
      const response = method === "ReadProposalFiles" ? {
        basis,
        files: [{ path: "proposals/bodies/retained.json", status: "approved", payload: supportedBody }],
        examples: [example("example-true.md", "example"), example("example-orphan.md", "approved")],
      } : {
        text: {
          basis: documentBasis, key: { kind: 1, id: "1" }, revision: "1",
          slug: "agent", atom: "an agent", body: "\n# Current\n",
        },
        layer: 0, domains: ["test"], confidence: "confirmed", source: "personal", triggers: ["agent"], assertions: [],
      };
      const json = JSON.stringify(response);
      return new Response(json, { headers: { "content-type": "application/json", "content-length": String(Buffer.byteLength(json)) } });
    },
  });
  const app = createApp({
    kbRoot: "/nonexistent-local-proposal-files",
    nativeTermReader: connectNativeTermReader({
      url: worker.url.origin, kbUuid: basis.kbUuid, kbSlug: "default", token: "f".repeat(64),
    }),
  });
  return { app, worker, calls };
}

test("proposal visibility and status follow current rows; examples cannot create approved rows", async () => {
  const { app, worker, calls } = fixture();
  try {
    const result = await app.request("/api/proposals");
    expect(result.status).toBe(200);
    const output = await result.json() as { bodies: { file: string; status: string; date: string }[]; edges: { file: string }[] };
    expect(output.bodies.map(({ file, status, date }) => ({ file, status, date })))
      .toEqual([{ file: "retained.json", status: "approved", date: "original date" }]);
    expect(output.edges.map(({ file }) => file)).toEqual(["example-true.md"]);
    expect(calls).toEqual(["ReadProposalFiles"]);
    expect((await app.request("/api/proposals/example-orphan.md")).status).toBe(404);
    expect((await app.request("/api/proposals/owner-cannot-rename.json")).status).toBe(404);
  } finally {
    worker.stop(true);
  }
});

test("proposal detail diffs the current concept body on the same database basis", async () => {
  const same = fixture();
  try {
    const response = await same.app.request("/api/proposals/retained.json?format=text");
    const detail = await response.text();
    expect(response.status).toBe(200);
    expect(detail).toContain("Status:   approved");
    expect(detail).toContain("-# Current");
    expect(detail).toContain("+# New");
    expect(same.calls).toEqual(["ReadProposalFiles", "ReadTermDocument"]);
  } finally {
    same.worker.stop(true);
  }

  for (const changedBasis of [{ ...basis, version: "7" }, { ...basis, manifestSha256: "b".repeat(64) }]) {
    const diverged = fixture(changedBasis);
    try {
      const response = await diverged.app.request("/api/proposals/retained.json");
      expect(response.status).toBe(503);
      expect(await response.json()).toEqual({ error: "Database unavailable" });
    } finally {
      diverged.worker.stop(true);
    }
  }
});
