import { expect, test } from "bun:test";
import type { ProposalMergeDecision, ProposalMergeFile, ProposalMergePort } from "@x-sage/service";

import { createApp } from "./app.ts";

const row = (file: string, status = "pending") => ({ file, status });

function appWithPort(list: Awaited<ReturnType<ProposalMergePort["list"]>>, decide: (target: ProposalMergeFile) => ProposalMergeDecision) {
  const calls: string[] = [];
  const app = createApp({
    kbRoot: "/nonexistent-local-kb",
    writeToken: "review-secret",
    nativeTermReader: {
      kbSlug: "default",
      async readTermText() { throw new Error("unexpected read"); },
      async readTermDocument() { throw new Error("unexpected read"); },
      proposalMerge: {
        async list() { return list; },
        async decide(target) {
          calls.push(`${target.directory}/${target.file}`);
          return decide(target);
        },
      },
    },
  });
  return { app, calls };
}

function post(app: ReturnType<typeof createApp>, body?: unknown, query = "", token = "review-secret") {
  return app.request("/api/proposals/merge" + query, {
    method: "POST",
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
}

test("all keeps selection order and applies later successes after known failures", async () => {
  const { app, calls } = appWithPort({
    edges: [row("01-edge.md"), row("02-fail.md"), row("03-example.md", "example")],
    atoms: [row("04-atom.md"), row("05-approved.md", "approved")],
    bodies: [row("06-conflict.json"), row("07-body.json")],
  }, ({ file }) => {
    if (file === "02-fail.md") return { kind: "failed", message: "malformed edge" };
    if (file === "06-conflict.json") return { kind: "conflict", message: "revised by earlier atom" };
    return { kind: "applied" };
  });
  const response = await post(app);
  expect(response.status).toBe(409);
  expect(await response.json()).toEqual({
    merged: ["01-edge.md", "04-atom.md", "07-body.json"],
    failed: [{ file: "02-fail.md", error: "malformed edge" }],
    conflicts: [{ file: "06-conflict.json", error: "revised by earlier atom" }],
    derivedStale: true,
  });
  expect(calls).toEqual([
    "edges/01-edge.md", "edges/02-fail.md", "atoms/04-atom.md",
    "bodies/06-conflict.json", "bodies/07-body.json",
  ]);
});

test("named files preserve directory order, already-approved retries and not-found errors", async () => {
  const { app, calls } = appWithPort({
    edges: [row("same.md", "approved")],
    atoms: [row("same.md", "example")],
    bodies: [row("body.json")],
  }, ({ directory, file }) => {
    if (directory === "edges") return { kind: "alreadyApplied" };
    if (directory === "atoms") return { kind: "failed", message: `Proposal ${file} has status "example", expected "pending"` };
    return { kind: "applied" };
  });
  const named = await post(app, { file: "same.md" });
  expect(named.status).toBe(422);
  expect(await named.json()).toEqual({
    merged: ["same.md"],
    failed: [{ file: "same.md", error: 'Proposal same.md has status "example", expected "pending"' }],
    conflicts: [],
    derivedStale: false,
  });
  const bodyOnly = await post(app, { file: "body.json" }, "?format=text");
  expect(bodyOnly.status).toBe(200);
  expect(await bodyOnly.text()).toContain("graph.json and MANIFEST.md are unchanged (body-only).");
  const absent = await post(app, { file: "missing.md" });
  expect(absent.status).toBe(404);
  expect(await absent.json()).toEqual({ error: "No such proposal: missing.md" });
  expect(calls).toEqual(["edges/same.md", "atoms/same.md", "bodies/body.json"]);
});

test("unknown decisions preserve known prefix and prevent further attempts", async () => {
  for (const failure of ["typed", "transport"] as const) {
    const { app, calls } = appWithPort({
      edges: [row("before.md")], atoms: [row("ambiguous.md")], bodies: [row("after.json")],
    }, ({ file }) => {
      if (file !== "ambiguous.md") return { kind: "applied" };
      if (failure === "transport") throw new Error("connection closed after sending request");
      return { kind: "unknown" };
    });
    const response = await post(app, {});
    expect(response.status).toBe(503);
    expect(await response.json()).toEqual({
      merged: ["before.md"],
      failed: [],
      conflicts: [],
      derivedStale: true,
      error: "Database outcome unknown for proposal ambiguous.md; resolve this decision before continuing",
    });
    expect(calls).toEqual(["edges/before.md", "atoms/ambiguous.md"]);
  }
});

test("bad body, wrong KB and unauthorized writes do not call the decision port", async () => {
  const { app, calls } = appWithPort(
    { edges: [row("one.md")], atoms: [], bodies: [] },
    () => ({ kind: "applied" }),
  );
  expect((await post(app, [])).status).toBe(400);
  expect((await post(app, {}, "?kb=other")).status).toBe(404);
  expect((await post(app, {}, "", "wrong-token")).status).toBe(401);
  expect(calls).toEqual([]);
});
