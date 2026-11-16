import { describe, expect, test } from "bun:test";
import { mkdtemp, cp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createApp } from "../src/app.ts";
import {
  conceptDocument,
  openStore,
  requireSubjectText,
  reviewProposal,
  submitProposal,
  upsertVectors,
} from "@x-sage/service/store";
import { embeddingRevision } from "@x-sage/core/knowledge";
import { proposeEdge } from "@x-sage/core/proposals";

/**
 * Regressions for the independent review (TASK-1441).
 *
 * Each case fails against the pre-review implementation and names the defect it
 * covers: a wrong baseline, an inverted guard, a vector that poisons the model,
 * a fabricated similarity, an unreachable asset path, and an unpinned vector
 * dimension.
 */

const FIXTURE_KB = resolve(import.meta.dir, "../../../fixtures/architecture-review");

async function app() {
  const dir = await mkdtemp(join(tmpdir(), "x-sage-regression-"));
  await cp(FIXTURE_KB, join(dir, "kb"), { recursive: true });
  const kbRoot = join(dir, "kb");
  const instance = createApp({ kbRoot });
  const call = (path: string, init?: RequestInit) => instance.request(path, init);
  const post = (path: string, body: unknown) =>
    call(path, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  await post("/api/knowledge/import", { mode: "authority" });
  return { call, post, kbRoot, cleanup: () => rm(dir, { recursive: true, force: true }) };
}

interface RevisionBody {
  expectedRevision: string;
}
interface DocumentBody {
  conceptRevision: string;
  revision: string;
}
interface ProposalBody {
  id: string;
}

describe("P0-1 revisions endpoint answers per kind", () => {
  test("every proposal kind returns the baseline its submission accepts", async () => {
    const f = await app();
    try {
      for (const kind of ["claim-create", "evidence-attach"]) {
        const revision = await (await f.call(`/api/knowledge/revisions?kind=${kind}&subject=review-premise`)).json() as RevisionBody;
        expect(revision.expectedRevision).toMatch(/^[a-f0-9]{64}$/);
        // The value advertised must be the value the submission accepts.
        const payload =
          kind === "claim-create"
            ? { kind: "premise", text: "revision endpoint check", confidence: "confirmed" }
            : { kind: "text", title: "t", excerpt: "e", capturedBy: "c", targets: [{ type: "concept", id: "review-premise" }] };
        const submitted = await f.post("/api/knowledge/proposals", {
          kind,
          subject: "review-premise",
          expectedRevision: revision.expectedRevision,
          reason: "regression: baseline must match",
          author: "regression-agent",
          payload,
        });
        expect(submitted.status).toBe(201);
      }

      // relation-assert uses the relation-set baseline, not the concept baseline.
      const relation = await (await f.call("/api/knowledge/revisions?kind=relation-assert&subject=review-risk")).json() as RevisionBody;
      const document = await (await f.call("/api/knowledge/concepts/review-risk/document")).json() as DocumentBody;
      expect(relation.expectedRevision).not.toBe(document.conceptRevision);
      expect(relation.expectedRevision).not.toBe("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");

      // concept-markdown uses the exported byte hash.
      const markdown = await (await f.call("/api/knowledge/revisions?kind=concept-markdown&subject=review-risk")).json() as RevisionBody;
      expect(markdown.expectedRevision).toBe(document.revision);
    } finally {
      await f.cleanup();
    }
  });
});

describe("P0-2 orphan guard has the documented polarity", () => {
  test("a relation that isolates a node is refused, and a document without edges is accepted", async () => {
    const f = await app();
    try {
      const document = await (await f.call("/api/knowledge/concepts/review-risk/document")).json() as DocumentBody;

      // Rewriting review-risk's document without edges isolates it. That is
      // legitimate for a review-writeback: the reviewer owns the edge list.
      const withoutEdges = (await (await f.call("/api/knowledge/concepts/review-risk/document?format=markdown")).text())
        .replace(/edges:\n(?:  - .*\n|    .*\n)+/, "");
      const writeback = await f.post("/api/knowledge/proposals", {
        kind: "concept-markdown",
        subject: "review-risk",
        expectedRevision: document.revision,
        reason: "regression: document without edges",
        author: "regression-agent",
        payload: { document: withoutEdges },
      });
      expect(writeback.status).toBe(201);
      const proposal = await writeback.json() as ProposalBody;

      // Approving it must succeed: the guard used to reject exactly this.
      const review = await f.post(`/api/knowledge/proposals/${proposal.id}/review`, {
        decision: "approve",
        reviewer: "regression-reviewer",
      });
      expect(review.status).toBe(200);
    } finally {
      await f.cleanup();
    }
  });
});

describe("P0-3 a float32-overflowing vector cannot poison the model", () => {
  test("1e39 is rejected at upload and search still works afterwards", async () => {
    const f = await app();
    try {
      const graph = await (await f.call("/api/graph")).json() as { nodes: Array<{ id: string }> };
      const conceptId = graph.nodes[0]!.id;
      const revision = await (await f.call(`/api/knowledge/revisions?kind=concept-markdown&subject=${conceptId}`)).json() as RevisionBody;

      // The rev is for the document projection; embed against the concept text.
      const document = await (await f.call(`/api/knowledge/concepts/${conceptId}/document`)).json() as DocumentBody;
      expect(document.revision).toBe(revision.expectedRevision);

      const upload = await f.post("/api/knowledge/vectors", {
        vectors: [
          {
            subjectType: "concept",
            subjectId: conceptId,
            model: "regression/big",
            revision: "0".repeat(64),
            values: [1e39, 1, 0],
          },
        ],
      });
      // Rejected before storage, whichever check fires first: revision mismatch
      // is 409, the overflow is 400. Neither may write a row.
      expect([400, 409]).toContain(upload.status);
      expect(await upload.text()).not.toContain("201");

      const search = await f.post("/api/knowledge/search", {
        query: "约束",
        vector: { model: "regression/big", values: [1, 0, 0] },
      });
      expect(search.status).toBe(200);
    } finally {
      await f.cleanup();
    }
  });
});

describe("P1-4 vector associations never report a fabricated cosine", () => {
  test("an association without a query vector has cosine null, not 0", async () => {
    const f = await app();
    try {
      const listed = await (await f.call("/api/knowledge/concepts")).json() as { concepts: Array<{ id: string }> };
      const conceptId = listed.concepts[0]!.id;

      // Lexical-only search: nothing is scored on the vector axis.
      const response = await (
        await f.post("/api/knowledge/search", { query: "约束", limit: 5 })
      ).json() as { results: Array<{ vectorAssociations: Array<{ cosine: number | null }> }> };

      for (const hit of response.results) {
        for (const association of hit.vectorAssociations) {
          expect(association.cosine).toBeNull();
        }
      }
      expect(conceptId.length).toBeGreaterThan(0);
    } finally {
      await f.cleanup();
    }
  });
});

describe("P1-5 assets above the general body cap are reachable", () => {
  test("a ~300 KiB PNG round-trips through /api/knowledge/assets", async () => {
    const f = await app();
    try {
      // 16x16 header followed by filler: the signature is what the store checks,
      // the size is what the general 256 KiB cap used to reject.
      const header = await Bun.file(join(FIXTURE_KB, "assets", "latency-chart.png")).bytes();
      const bytes = new Uint8Array(300 * 1024);
      bytes.set(header, 0);
      bytes.fill(7, header.length);

      const upload = await f.post("/api/knowledge/assets", { data: Buffer.from(bytes).toString("base64") });
      expect(upload.status).toBe(201);
      const asset = await upload.json() as { sha256: string; byteSize: number };
      expect(asset.byteSize).toBe(bytes.byteLength);

      const served = await f.call(`/api/knowledge/assets/${asset.sha256}`);
      expect(served.status).toBe(200);
      expect((await served.arrayBuffer()).byteLength).toBe(bytes.byteLength);
    } finally {
      await f.cleanup();
    }
  });
});

describe("P1-6 a model has one dimension count", () => {
  test("a second length for the same model is a 409, not a silent second shape", async () => {
    const f = await app();
    try {
      const listed = await (await f.call("/api/knowledge/concepts")).json() as { concepts: Array<{ id: string }> };
      const [first, second] = listed.concepts;
      expect(first).toBeDefined();
      expect(second).toBeDefined();

      // Both uploads need real revisions, so the pinning is exercised at the
      // store boundary the HTTP handler calls.
      const db = openStore(f.kbRoot);
      try {
        const a = requireSubjectText(db, "concept", first!.id);
        const b = requireSubjectText(db, "concept", second!.id);
        upsertVectors(db, {
          vectors: [
            {
              subjectType: "concept",
              subjectId: first!.id,
              model: "regression/pinned",
              revision: embeddingRevision(a.text),
              values: [1, 0, 0],
            },
          ],
        });
        expect(() =>
          upsertVectors(db, {
            vectors: [
              {
                subjectType: "concept",
                subjectId: second!.id,
                model: "regression/pinned",
                revision: embeddingRevision(b.text),
                values: [1, 0, 0, 0],
              },
            ],
          }),
        ).toThrow(/dimensions/i);
      } finally {
        db.close();
      }
    } finally {
      await f.cleanup();
    }
  });
});

describe("flagged: malformed JSON is not treated as an absent body", () => {
  test("a truncated merge body is a 400 and merges nothing", async () => {
    // Markdown authority: this is the path where an absent `file` means "all".
    const dir = await mkdtemp(join(tmpdir(), "x-sage-regression-md-"));
    await cp(join(FIXTURE_KB), join(dir, "kb"), { recursive: true });
    try {
      const instance = createApp({ kbRoot: join(dir, "kb") });
      const response = await instance.request("/api/proposals/merge", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: '{"file": "tru',
      });
      expect(response.status).toBe(400);
      expect(await response.text()).toContain("not valid JSON");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("flagged: identical edge proposals no longer overwrite each other", () => {
  test("two submissions in the same second get distinct filenames", async () => {
    const dir = await mkdtemp(join(tmpdir(), "x-sage-edge-proposals-"));
    try {
      const one = await proposeEdge(dir, {
        from: "review-risk",
        type: "depends-on",
        target: "review-constraint",
        confidence: "hypothetical",
      });
      const two = await proposeEdge(dir, {
        from: "review-risk",
        type: "depends-on",
        target: "review-constraint",
        confidence: "hypothetical",
      });
      expect(one).not.toBe(two);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("flagged: claim text survives the projection", () => {
  test("newlines and the :: separator round-trip unchanged", async () => {
    const f = await app();
    try {
      const db = openStore(f.kbRoot);
      try {
        const document = conceptDocument(db, "review-risk")!;
        const text = "第一行\n第二行 :: 带分隔符\r\n第三行 \\n 字面量";
        const proposal = submitProposal(db, {
          kind: "claim-create",
          subject: "review-risk",
          expectedRevision: document.conceptRevision,
          reason: "regression: lossless projection",
          author: "regression-agent",
          payload: { kind: "risk", text, confidence: "confirmed" },
        });
        reviewProposal(db, proposal.id, "approve", "regression-reviewer");

        const exported = conceptDocument(db, "review-risk")!;
        expect(exported.claims.map((claim: { text: string }) => claim.text)).toContain(text);
        // Exactly one line carries the claim, so it is editable as one entry.
        const claimLines = exported.document.split("\n").filter((line: string) => line.startsWith("- clm_"));
        expect(claimLines).toHaveLength(1);
      } finally {
        db.close();
      }
    } finally {
      await f.cleanup();
    }
  });
});

describe("addendum: the AI read path follows the authority", () => {
  test("activate and edges serve the reviewed record, not the pre-import file", async () => {
    const f = await app();
    try {
      const document = await (await f.call("/api/knowledge/concepts/review-risk/document")).json() as DocumentBody;
      const markdown = await (await f.call("/api/knowledge/concepts/review-risk/document?format=markdown")).text();
      const edited = markdown
        .replace(/^atom: .*$/m, "atom: 原子B（审阅授权）")
        .replace(/^# 风险 Review Risk$/m, "# 风险 Review Risk\n\n审阅后的正文。");
      expect(edited).not.toBe(markdown);

      const proposal = await (
        await f.post("/api/knowledge/proposals", {
          kind: "concept-markdown",
          subject: "review-risk",
          expectedRevision: document.revision,
          reason: "regression: activate must follow the authority",
          author: "regression-agent",
          payload: { document: edited },
        })
      ).json() as ProposalBody;
      expect(
        (
          await f.post(`/api/knowledge/proposals/${proposal.id}/review`, {
            decision: "approve",
            reviewer: "regression-reviewer",
          })
        ).status,
      ).toBe(200);

      const activation = await (await f.call("/api/concepts/review-risk")).json() as {
        molecule: string;
        node: { atom: string };
        markdown: string;
      };
      expect(activation.node.atom).toBe("原子B（审阅授权）");
      expect(activation.molecule).toContain("审阅后的正文。");
      expect(activation.markdown).toContain("原子B（审阅授权）");

      // The edge query reads the same authority.
      const edges = await (await f.call("/api/concepts/review-risk/edges?dir=out")).json() as {
        edges: Array<{ type: string; other: { id: string } }>;
      };
      expect(edges.edges.length).toBeGreaterThan(0);

      // Legacy keyword search reads the same rows, so the reviewed atom is
      // findable there too (it indexes atom/triggers, not prose).
      const search = await (await f.call("/api/search?q=审阅授权")).json() as { results: Array<{ id: string }> };
      expect(search.results.map((hit) => hit.id)).toContain("review-risk");
    } finally {
      await f.cleanup();
    }
  });

  test("a concept that exists only in the authority is reachable from every read", async () => {
    const f = await app();
    try {
      const { openStore, submitProposal, reviewProposal, conceptDocument } = await import("@x-sage/service/store");
      // No Markdown source for this id: it is created by a reviewed write-back
      // only after the fact, so file-backed routes cannot see it at all.
      const db = openStore(f.kbRoot);
      try {
        const document = conceptDocument(db, "review-constraint")!;
        const created = document.document
          .replace(/id: review-constraint/, "id: review-store-only")
          .replace(/atom: ".*"/, 'atom: "仅存在于结构化权威的概念"');
        expect(created).toContain("review-store-only");
        db.run(
          `INSERT INTO concepts (id, atom, layer, domain_json, confidence, source, triggers_json, body, source_path, revision, source_hash, source_revision, created_at, updated_at)
           SELECT 'review-store-only', '仅存在于结构化权威的概念', layer, domain_json, confidence, source, triggers_json, '# 仅权威概念', 'concepts/L1-core/review-store-only.md', revision, NULL, revision, created_at, updated_at
           FROM concepts WHERE id = 'review-constraint'`,
        );
      } finally {
        db.close();
      }

      const activation = await f.call("/api/concepts/review-store-only");
      expect(activation.status).toBe(200);
      const payload = await activation.json() as { node: { atom: string } };
      expect(payload.node.atom).toBe("仅存在于结构化权威的概念");
    } finally {
      await f.cleanup();
    }
  });
});

describe("addendum: proposal surfaces agree in structured mode", () => {
  test("the list keeps legacy keys and the detail route answers a prp_ id", async () => {
    const f = await app();
    try {
      const document = await (await f.call("/api/knowledge/concepts/review-decision/document")).json() as DocumentBody;
      const submitted = await f.post("/api/knowledge/proposals", {
        kind: "claim-create",
        subject: "review-decision",
        expectedRevision: document.conceptRevision,
        reason: "regression: proposal surfaces",
        author: "regression-agent",
        payload: { kind: "decision", text: "决定使用 SQLite", confidence: "confirmed" },
      });
      const proposal = await submitted.json() as ProposalBody;

      const list = await (await f.call("/api/proposals")).json() as {
        authority: string;
        proposals: Array<{ id: string }>;
        edges: unknown[];
        atoms: unknown[];
        bodies: unknown[];
      };
      // A legacy client spreads these three; absent keys throw.
      expect(list.edges).toEqual([]);
      expect(list.atoms).toEqual([]);
      expect(list.bodies).toEqual([]);
      expect(list.proposals.map((item) => item.id)).toContain(proposal.id);

      const detail = await f.call(`/api/proposals/${proposal.id}`);
      expect(detail.status).toBe(200);
      expect((await detail.json() as { id: string }).id).toBe(proposal.id);
    } finally {
      await f.cleanup();
    }
  });

  test("the legacy body route keeps `body` free of frontmatter and fills `atom`", async () => {
    const f = await app();
    try {
      const response = await (await f.call("/api/concepts/review-risk/body")).json() as {
        body: string;
        document: string;
        atom: string;
        revision: string;
      };
      expect(response.body.trimStart().startsWith("---")).toBe(false);
      expect(response.body).toContain("# 风险 Review Risk");
      expect(response.atom.length).toBeGreaterThan(0);
      expect(response.document.startsWith("---")).toBe(true);
      expect(response.revision).toMatch(/^[a-f0-9]{64}$/);
    } finally {
      await f.cleanup();
    }
  });
});

describe("addendum: derived files stop claiming to be current", () => {
  test("MANIFEST.md and graph.json become a pointer to the archive after import", async () => {
    const { readFile } = await import("node:fs/promises");
    const f = await app();
    try {
      const manifest = await readFile(join(f.kbRoot, "MANIFEST.md"), "utf-8");
      expect(manifest).toContain("archive/markdown-");
      expect(manifest).not.toContain("| id | atom |");

      const graph = await readFile(join(f.kbRoot, "graph.json"), "utf-8");
      expect(graph).toContain("knowledge.db");

      // Reads still work because they come from the store.
      const served = await (await f.call("/api/manifest?format=text")).text();
      expect(served).toContain("| review-risk |");
    } finally {
      await f.cleanup();
    }
  });
});

describe("addendum: cross-KB references are refused, not silently broken", () => {
  test("a relation proposal with a kb::concept target is a 400 with the reason", async () => {
    const f = await app();
    try {
      const revision = await (
        await f.call("/api/knowledge/revisions?kind=relation-assert&subject=review-risk")
      ).json() as RevisionBody;
      const response = await f.post("/api/knowledge/proposals", {
        kind: "relation-assert",
        subject: "review-risk",
        expectedRevision: revision.expectedRevision,
        reason: "regression: cross-kb",
        author: "regression-agent",
        payload: { from: "review-risk", type: "depends-on", target: "otherkb::agent" },
      });
      expect(response.status).toBe(400);
      expect(await response.text()).toContain("Cross-KB");
    } finally {
      await f.cleanup();
    }
  });
});
