import { describe, expect, test } from "bun:test";
import { mkdtemp, cp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createApp } from "../src/app.ts";

/**
 * The structured-knowledge surface over real HTTP.
 *
 * These go through the actual dispatcher and Hono adapter, so they assert what a
 * remote client depends on: migration is explicit, legacy routes keep working
 * before it, the legacy write path is refused afterwards, and a review conflict
 * is a 409 that leaves the authority untouched.
 */

const FIXTURE_KB = resolve(import.meta.dir, "../../../fixtures/architecture-review");

async function fixture() {
  const dir = await mkdtemp(join(tmpdir(), "x-sage-http-store-"));
  await cp(FIXTURE_KB, join(dir, "kb"), { recursive: true });
  const kbRoot = join(dir, "kb");
  const app = createApp({ kbRoot });
  return { app, kbRoot, cleanup: () => rm(dir, { recursive: true, force: true }) };
}

interface ProposalRef {
  id: string;
  status: string;
  kind: string;
}

interface DocumentView {
  conceptId: string;
  document: string;
  revision: string;
  conceptRevision: string;
  claims: Array<{ id: string; text: string }>;
  evidence: Array<{ id: string; title: string }>;
}

interface ImportReportBody {
  firstImport: boolean;
  backupDir: string | null;
  counts: { created: number; conflicts: number; unchanged: number };
}

/** Run a request through the app directly, without binding a port. */
async function call(app: ReturnType<typeof createApp>, path: string, init?: RequestInit): Promise<Response> {
  return app.request(path, init);
}

const jsonInit = (method: string, body: unknown): RequestInit => ({
  method,
  headers: { "content-type": "application/json" },
  body: JSON.stringify(body),
});

describe("migration over HTTP", () => {
  test("before import, legacy Markdown routes answer and the store reports markdown authority", async () => {
    const { app, cleanup } = await fixture();
    try {
      const status = await (await call(app, "/api/knowledge/status")).json() as { authority: string };
      expect(status.authority).toBe("markdown");

      const graph = await (await call(app, "/api/graph")).json() as { nodes: unknown[] };
      expect(graph.nodes).toHaveLength(6);

      // The structured reads refuse instead of silently returning nothing.
      const refused = await call(app, "/api/knowledge/concepts");
      expect(refused.status).toBe(409);
      expect(await refused.text()).toContain("import");
    } finally {
      await cleanup();
    }
  });

  test("import is explicit, reports a conflict count, and flips the authority", async () => {
    const { app, kbRoot, cleanup } = await fixture();
    try {
      const res = await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));
      expect(res.status).toBe(201);
      const report = await res.json() as ImportReportBody;
      expect(report.firstImport).toBe(true);
      expect(report.counts.created).toBe(6);
      expect(report.counts.conflicts).toBe(0);
      expect(report.backupDir).not.toBeNull();

      const status = await (await call(app, "/api/knowledge/status")).json() as { authority: string; counts: { concepts: number } };
      expect(status.authority).toBe("structured");
      expect(status.counts.concepts).toBe(6);

      // The Markdown file still exists untouched — the import is additive.
      expect(await readFile(join(kbRoot, "concepts/L1-core/review-risk.md"), "utf-8")).toContain("可审查的风险");
    } finally {
      await cleanup();
    }
  });

  test("a second import without reimport is refused with a reason", async () => {
    const { app, cleanup } = await fixture();
    try {
      expect((await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }))).status).toBe(201);
      const again = await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));
      expect(again.status).toBe(409);
      expect(await again.text()).toContain("reimport");
    } finally {
      await cleanup();
    }
  });

  test("import without mode=authority is a 400, not a silent migration", async () => {
    const { app, cleanup } = await fixture();
    try {
      const res = await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "readonly" }));
      expect(res.status).toBe(400);
      const status = await (await call(app, "/api/knowledge/status")).json() as { authority: string };
      expect(status.authority).toBe("markdown");
    } finally {
      await cleanup();
    }
  });
});

describe("legacy contract after migration", () => {
  test("reads keep their shape and now come from the store", async () => {
    const { app, cleanup } = await fixture();
    try {
      await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));

      const meta = await (await call(app, "/api/meta")).json() as { kb: string; nodes: number; edges: number; authority: string };
      expect(meta.authority).toBe("structured");
      expect(meta.nodes).toBe(6);
      expect(meta.edges).toBe(11);

      const activation = await (await call(app, "/api/concepts/review-risk")).json() as {
        concept: string;
        requires: string[];
        markdown: string;
      };
      expect(activation.concept).toBe("review-risk");
      expect(activation.requires).toContain("review-decision");
      expect(activation.markdown).toContain("id: review-risk");

      const search = await (await call(app, "/api/search?q=约束")).json() as { results: Array<{ id: string }> };
      expect(search.results[0]?.id).toBe("review-constraint");

      const validate = await call(app, "/api/validate?graph=true");
      expect(validate.status).toBe(200);

      const manifest = await (await call(app, "/api/manifest?format=text")).text();
      expect(manifest).toContain("| review-risk |");
    } finally {
      await cleanup();
    }
  });

  test("the legacy write paths are refused with the replacement endpoint", async () => {
    const { app, cleanup } = await fixture();
    try {
      await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));

      const create = await call(app, "/api/concepts", jsonInit("POST", {}));
      expect(create.status).toBe(409);
      expect(await create.text()).toContain("/api/knowledge/proposals");

      const merge = await call(app, "/api/proposals/merge", jsonInit("POST", { file: "all" }));
      expect(merge.status).toBe(409);

      const graph = await call(app, "/api/graph", { method: "PUT", headers: { "content-type": "application/json" }, body: "{}" });
      expect(graph.status).toBe(409);
    } finally {
      await cleanup();
    }
  });
});

describe("proposal review over HTTP", () => {
  test("submit → list → detail → approve → authority changed", async () => {
    const { app, cleanup } = await fixture();
    try {
      await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));
      const document = (await (
        await call(app, "/api/knowledge/concepts/review-premise/document")
      ).json()) as DocumentView;

      const submit = await call(
        app,
        "/api/knowledge/proposals",
        jsonInit("POST", {
          kind: "claim-create",
          subject: "review-premise",
          expectedRevision: document.conceptRevision,
          reason: "record the capacity assumption",
          author: "external-agent",
          payload: { kind: "premise", text: "3 倍流量假设未经验证", confidence: "hypothetical" },
        }),
      );
      expect(submit.status).toBe(201);
      const proposal = await submit.json() as ProposalRef;
      expect(proposal.status).toBe("pending");

      const pending = await (await call(app, "/api/knowledge/proposals?status=pending")).json() as { proposals: ProposalRef[] };
      expect(pending.proposals.map((item) => item.id)).toContain(proposal.id);

      // Not authoritative yet.
      const before = await (await call(app, "/api/knowledge/concepts/review-premise/claims")).json() as { claims: unknown[] };
      expect(before.claims).toEqual([]);

      const review = await call(
        app,
        `/api/knowledge/proposals/${proposal.id}/review`,
        jsonInit("POST", { decision: "approve", reviewer: "arch-reviewer" }),
      );
      expect(review.status).toBe(200);

      const after = await (await call(app, "/api/knowledge/concepts/review-premise/claims")).json() as {
        claims: Array<{ text: string; evidence: unknown[] }>;
      };
      expect(after.claims.map((claim) => claim.text)).toContain("3 倍流量假设未经验证");
    } finally {
      await cleanup();
    }
  });

  test("a stale revision is refused at submission and reported as a 409", async () => {
    const { app, cleanup } = await fixture();
    try {
      await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));
      const response = await call(
        app,
        "/api/knowledge/proposals",
        jsonInit("POST", {
          kind: "claim-create",
          subject: "review-premise",
          expectedRevision: "a".repeat(64),
          reason: "stale",
          author: "external-agent",
          payload: { kind: "premise", text: "x", confidence: "confirmed" },
        }),
      );
      expect(response.status).toBe(409);
      expect(await response.text()).toContain("Re-read");
    } finally {
      await cleanup();
    }
  });

  test("an unknown proposal and a malformed review are 404 and 400", async () => {
    const { app, cleanup } = await fixture();
    try {
      await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));
      expect((await call(app, "/api/knowledge/proposals/prp_000000000000")).status).toBe(404);
      expect(
        (await call(app, "/api/knowledge/proposals/prp_000000000000/review", jsonInit("POST", { decision: "maybe" }))).status,
      ).toBe(400);
    } finally {
      await cleanup();
    }
  });
});

describe("hybrid search over HTTP", () => {
  test("lexical and vector hits are returned with their basis", async () => {
    const { app, cleanup } = await fixture();
    try {
      await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));

      const lexical = await (
        await call(app, "/api/knowledge/search", jsonInit("POST", { query: "延迟", limit: 5 }))
      ).json() as { mode: string; results: Array<{ conceptId: string | null; signals: Array<{ axis: string }> }> };
      expect(lexical.mode).toBe("lexical");
      expect(lexical.results.length).toBeGreaterThan(0);
      expect(lexical.results[0]?.signals[0]?.axis).toBe("lexical");

      // A vector query with no stored embeddings is not an error: it reports the
      // missing rows instead of pretending the axis contributed.
      const hybrid = await (
        await call(
          app,
          "/api/knowledge/search",
          jsonInit("POST", { query: "延迟", limit: 5, vector: { model: "fixture/model", values: [1, 0, 0] } }),
        )
      ).json() as { mode: string; vector: { missing: number } | null };
      expect(hybrid.mode).toBe("hybrid");
      expect(hybrid.vector?.missing).toBeGreaterThan(0);
    } finally {
      await cleanup();
    }
  });

  test("bounded inputs are enforced", async () => {
    const { app, cleanup } = await fixture();
    try {
      await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));
      const long = await call(app, "/api/knowledge/search", jsonInit("POST", { query: "x".repeat(600) }));
      expect(long.status).toBe(400);

      const badLimit = await call(app, "/api/knowledge/search", jsonInit("POST", { query: "x", limit: 999 }));
      expect(badLimit.status).toBe(400);

      const badVector = await call(
        app,
        "/api/knowledge/search",
        jsonInit("POST", { query: "x", vector: { model: "m", values: ["a"] } }),
      );
      expect(badVector.status).toBe(400);
    } finally {
      await cleanup();
    }
  });
});

describe("assets over HTTP", () => {
  test("an uploaded image is served back byte-identical with its detected type", async () => {
    const { app, cleanup } = await fixture();
    try {
      await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));
      const png = await readFile(join(FIXTURE_KB, "assets", "latency-chart.png"));
      const data = png.toString("base64");

      const upload = await call(app, "/api/knowledge/assets", jsonInit("POST", { data }));
      expect(upload.status).toBe(201);
      const asset = await upload.json() as { sha256: string; mediaType: string; byteSize: number };
      expect(asset.mediaType).toBe("image/png");
      expect(asset.byteSize).toBe(png.byteLength);

      const served = await call(app, `/api/knowledge/assets/${asset.sha256}`);
      expect(served.status).toBe(200);
      expect(served.headers.get("content-type")).toBe("image/png");
      const bytes = new Uint8Array(await served.arrayBuffer());
      expect(bytes).toEqual(new Uint8Array(png));
    } finally {
      await cleanup();
    }
  });

  test("a non-image payload is refused rather than stored opaque", async () => {
    const { app, cleanup } = await fixture();
    try {
      await call(app, "/api/knowledge/import", jsonInit("POST", { mode: "authority" }));
      const res = await call(
        app,
        "/api/knowledge/assets",
        jsonInit("POST", { data: Buffer.from("<svg onload=alert(1)>").toString("base64"), mediaType: "image/svg+xml" }),
      );
      expect(res.status).toBe(415);
    } finally {
      await cleanup();
    }
  });
});
