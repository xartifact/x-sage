import { describe, expect, test } from "bun:test";
import { mkdtemp, cp, readFile, rm, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createApp } from "../src/app.ts";

/**
 * Body read/write over HTTP.
 *
 * These go through the real dispatcher, so they assert the contract a remote
 * client depends on: status codes, that a body edit leaves frontmatter alone on
 * disk, and that a stale revision is a 409 rather than a lost update.
 */

const REAL_KB = resolve(import.meta.dir, "../../../kb/default");

/** `Response.json()` is `unknown`; these tests assert specific fields. */
interface BodyReadback {
  conceptId: string;
  body: string;
  revision: string;
  atom: string;
}
interface ProposalRef {
  file: string;
}

async function isolatedApp(root?: string) {
  const dir = root ?? (await mkdtemp(join(tmpdir(), "x-sage-http-")));
  await cp(REAL_KB, join(dir, "kb"), { recursive: true });
  return { app: createApp({ kbRoot: join(dir, "kb") }), root: join(dir, "kb"), cleanup: () => rm(dir, { recursive: true, force: true }) };
}

describe("GET /api/concepts/:id/body", () => {
  test("returns the body and a revision", async () => {
    const { app, cleanup } = await isolatedApp();
    try {
      const body = (await (await app.request("/api/concepts/decision/body")).json()) as BodyReadback;
      expect(body.conceptId).toBe("decision");
      expect(body.body.length).toBeGreaterThan(0);
      // The body starts after the frontmatter, so it never opens with `---'.
      expect(body.body.trimStart().startsWith("#")).toBe(true);
      expect(body.revision).toMatch(/^[a-f0-9]{64}$/);
      expect(body.atom.length).toBeGreaterThan(0);
    } finally {
      await cleanup();
    }
  });

  test("markdown format returns the body alone", async () => {
    const { app, cleanup } = await isolatedApp();
    try {
      const res = await app.request("/api/concepts/decision/body?format=markdown");
      expect(res.headers.get("content-type")).toContain("text/markdown");
      expect(await res.text()).not.toContain("id: decision");
    } finally {
      await cleanup();
    }
  });

  test("an unknown concept is 404 and a bad id is 400", async () => {
    const { app, cleanup } = await isolatedApp();
    try {
      expect((await app.request("/api/concepts/nope/body")).status).toBe(404);
      expect((await app.request("/api/concepts/..%2Fetc%2Fpasswd/body")).status).toBe(400);
    } finally {
      await cleanup();
    }
  });
});

describe("body proposals over HTTP", () => {
  test("read, propose, diff, merge, read back", async () => {
    const { app, root, cleanup } = await isolatedApp();
    try {
      const live = (await (await app.request("/api/concepts/risk/body")).json()) as BodyReadback;
      const candidate = `${live.body}\n新增一段。\n`;

      const proposed = await app.request("/api/proposals", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          kind: "body",
          conceptId: "risk",
          expectedRevision: live.revision,
          proposedBody: candidate,
          reason: "补一段",
        }),
      });
      expect(proposed.status).toBe(201);
      const { file } = (await proposed.json()) as ProposalRef;

      const detail = (await (await app.request(`/api/proposals/${file}`)).json()) as {
        kind: string;
        applies: boolean;
        diff: string;
      };
      expect(detail.kind).toBe("body");
      expect(detail.applies).toBe(true);
      expect(detail.diff).toContain("+新增一段。");

      const merged = await app.request("/api/proposals/merge", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ file }),
      });
      expect(merged.status).toBe(200);
      expect(((await merged.json()) as { derivedStale: boolean }).derivedStale).toBe(false);

      const after = (await (await app.request("/api/concepts/risk/body")).json()) as BodyReadback;
      expect(after.body).toBe(candidate);
      expect(after.atom).toBe(live.atom);
    } finally {
      await cleanup();
    }
  });

  test("a stale revision is 409 with the current revision in the message", async () => {
    const { app, cleanup } = await isolatedApp();
    try {
      const res = await app.request("/api/proposals", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          kind: "body",
          conceptId: "risk",
          expectedRevision: "0".repeat(64),
          proposedBody: "# 风险\n\n正文\n",
          reason: "r",
        }),
      });
      expect(res.status).toBe(409);
      expect(((await res.json()) as { error: string }).error).toContain("revision");
    } finally {
      await cleanup();
    }
  });

  test("missing revision or reason is 400", async () => {
    const { app, cleanup } = await isolatedApp();
    try {
      const noReason = await app.request("/api/proposals", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ kind: "body", conceptId: "risk", expectedRevision: "x", proposedBody: "y" }),
      });
      expect(noReason.status).toBe(400);
    } finally {
      await cleanup();
    }
  });

  test("an unknown proposal file is 404, not an empty success", async () => {
    const { app, cleanup } = await isolatedApp();
    try {
      expect((await app.request("/api/proposals/nope.json")).status).toBe(404);
      const merge = await app.request("/api/proposals/merge", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ file: "nope.json" }),
      });
      expect(merge.status).toBe(404);
    } finally {
      await cleanup();
    }
  });

  test("a failed merge answers 422 instead of 200", async () => {
    const { app, root, cleanup } = await isolatedApp();
    try {
      await mkdir(join(root, "proposals", "atoms"), { recursive: true });
      await writeFile(
        join(root, "proposals", "atoms", "z-broken.md"),
        "---\nstatus: pending\nconcept: nope\n---\n## 当前 atom\na\n## 建议新 atom\nb\n",
      );
      const res = await app.request("/api/proposals/merge", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ file: "z-broken.md" }),
      });
      expect(res.status).toBe(422);
      expect(((await res.json()) as { failed: unknown[] }).failed).toHaveLength(1);
    } finally {
      await cleanup();
    }
  });

  test("a body merge does not rewrite frontmatter", async () => {
    const { app, root, cleanup } = await isolatedApp();
    try {
      const path = join(root, "concepts", "L1-core", "risk.md");
      const before = await readFile(path, "utf-8");
      const prefix = before.slice(0, before.indexOf("\n---\n", 4) + 5);

      const live = (await (await app.request("/api/concepts/risk/body")).json()) as BodyReadback;
      const proposed = await app.request("/api/proposals", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          kind: "body", conceptId: "risk", expectedRevision: live.revision,
          proposedBody: "# 风险\n\n重写。\n", reason: "r",
        }),
      });
      const { file } = (await proposed.json()) as ProposalRef;
      await app.request("/api/proposals/merge", {
        method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ file }),
      });

      const after = await readFile(path, "utf-8");
      // `domain: [product]` would become a block list if the merge re-serialised.
      expect(after.startsWith(prefix)).toBe(true);
    } finally {
      await cleanup();
    }
  });
});