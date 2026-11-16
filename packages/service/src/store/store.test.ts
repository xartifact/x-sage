import { describe, expect, test } from "bun:test";
import { mkdtemp, cp, readFile, rm, writeFile, readdir, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createHash } from "node:crypto";
import {
  buildGraphFromStore,
  conceptDocument,
  ConflictError,
  evidenceForTarget,
  hybridSearch,
  importKnowledgeBase,
  ImportRefusedError,
  openStore,
  expectedRevisionFor,
  relationSetRevision,
  reviewProposal,
  uploadAsset,
  storeStatus,
  submitProposal,
  upsertVectors,
} from "./index.ts";
import { activateFromStore } from "../structured.ts";
import {
  embeddingRevision,
  embeddingTextForConcept,
  embeddingTextForClaim,
} from "@x-sage/core/knowledge";

/**
 * The structured-authority slice, end to end.
 *
 * Fixtures are copied to a temp directory: the repository KBs are never written
 * by a test. The architecture-review fixture is used rather than the production
 * KB because it has a known premise, a known contested relation and two evidence
 * documents whose expected retrieval result is written down.
 */

const FIXTURE_KB = resolve(import.meta.dir, "../../../../fixtures/architecture-review");

async function fixtureKb(): Promise<{ root: string; cleanup: () => Promise<void> }> {
  const dir = await mkdtemp(join(tmpdir(), "x-sage-store-"));
  await cp(FIXTURE_KB, join(dir, "kb"), { recursive: true });
  return {
    root: join(dir, "kb"),
    cleanup: () => rm(dir, { recursive: true, force: true }),
  };
}

const claim = (text: string, kind: "premise" | "constraint" | "risk" = "premise") => ({ text, kind });

function db(root: string) {
  return openStore(root);
}

describe("explicit import", () => {
  test("creates the authority, keeps the Markdown files and writes a snapshot", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      const before = await readdir(join(root, "concepts", "L1-core"));
      const report = await importKnowledgeBase({ root });

      expect(report.firstImport).toBe(true);
      expect(report.counts.created).toBe(6);
      expect(report.counts.conflicts).toBe(0);
      // The sources stay: the import is additive, so a reviewer can diff.
      expect(await readdir(join(root, "concepts", "L1-core"))).toEqual(before);
      expect(report.backupDir).not.toBeNull();
      expect((await stat(join(root, report.backupDir!))).isDirectory()).toBe(true);

      const status = await storeStatus({ root });
      expect(status.authority).toBe("structured");
      expect(status.counts.concepts).toBe(6);
      expect(status.counts.relations).toBe(11);
    } finally {
      await cleanup();
    }
  });

  test("a second import is refused until reimport is stated", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      // A silent merge of two authorities is the failure mode this guards.
      await expect(importKnowledgeBase({ root })).rejects.toBeInstanceOf(ImportRefusedError);

      const again = await importKnowledgeBase({ root }, { reimport: true });
      expect(again.firstImport).toBe(false);
      expect(again.counts.unchanged).toBe(6);
      expect(again.backupDir).toBeNull();
    } finally {
      await cleanup();
    }
  });

  test("re-importing an edited file updates the concept but not a changed authority", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });

      const file = join(root, "concepts", "L1-core", "review-risk.md");
      const raw = await readFile(file, "utf-8");
      await writeFile(file, raw.replace("可审查的风险要说出", "可审查的风险必须说出"));

      const report = await importKnowledgeBase({ root }, { reimport: true });
      expect(report.counts.updated).toBe(1);
      expect(report.counts.conflicts).toBe(0);

      const read = db(root);
      try {
        expect(conceptDocument(read, "review-risk")?.document).toContain("必须说出");
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });

  test("a concept that changed in both places is a reported conflict, not an overwrite", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });

      // Change the authority through a review...
      const read = db(root);
      let claimId = "";
      try {
        const document = conceptDocument(read, "review-risk")!;
        const proposal = submitProposal(read, {
          kind: "claim-create",
          subject: "review-risk",
          expectedRevision: document.conceptRevision,
          reason: "add an observed risk signal",
          author: "agent-under-test",
          payload: claim("观察信号：p99 连续 3 个窗口超过 450ms"),
        });
        const reviewed = reviewProposal(read, proposal.id, "approve", "reviewer-under-test");
        claimId = reviewed.applied[0]!;
      } finally {
        read.close();
      }
      expect(claimId.startsWith("clm_")).toBe(true);

      // ...and the file as well.
      const file = join(root, "concepts", "L1-core", "review-risk.md");
      await writeFile(file, `${await readFile(file, "utf-8")}\n新增一段。\n`);

      const report = await importKnowledgeBase({ root }, { reimport: true });
      expect(report.counts.conflicts).toBe(1);
      expect(report.concepts.find((outcome) => outcome.result === "conflict")?.detail).toContain("structured store");

      const after = db(root);
      try {
        // The reviewed claim survived: a conflicting re-import never drops it.
        expect(conceptDocument(after, "review-risk")?.claims.map((item) => item.id)).toContain(claimId);
      } finally {
        after.close();
      }
    } finally {
      await cleanup();
    }
  });

  test("the store is not created by a read", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      expect((await storeStatus({ root })).authority).toBe("markdown");
      expect(await readdir(root)).not.toContain("knowledge.db");
    } finally {
      await cleanup();
    }
  });
});

describe("structured reads", () => {
  test("graph and document survive the migration unchanged", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        const graph = buildGraphFromStore(read);
        expect(graph.nodes).toHaveLength(6);
        expect(graph.edges).toHaveLength(11);

        const document = conceptDocument(read, "review-tension")!;
        expect(document.document).toContain("id: review-tension");
        expect(document.document).toContain("contradicts");
        expect(document.revision).toMatch(/^[a-f0-9]{64}$/);
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });

  test("activation keeps the requires/load-next contract", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        const activation = activateFromStore(read, "review-risk");
        expect(activation.node.id).toBe("review-risk");
        expect(activation.requires).toContain("review-decision");
        expect(activation.requires).toContain("review-constraint");
        expect(activation.neighbors.length).toBeGreaterThan(0);
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });
});

describe("claims and evidence", () => {
  test("a claim only becomes authority after review, and carries its evidence", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        const before = conceptDocument(read, "review-premise")!;
        expect(before.claims).toEqual([]);

        const pending = submitProposal(read, {
          kind: "claim-create",
          subject: "review-premise",
          expectedRevision: before.conceptRevision,
          reason: "state the capacity premise explicitly",
          author: "external-agent",
          payload: claim("峰值 QPS 在 2027 年前不超过 3 倍当前值"),
        });
        // Unreviewed knowledge is not authoritative.
        expect(conceptDocument(read, "review-premise")!.claims).toEqual([]);

        const { applied } = reviewProposal(read, pending.id, "approve", "arch-reviewer");
        const claimId = applied[0]!;

        const after = conceptDocument(read, "review-premise")!;
        expect(after.claims.map((item) => item.id)).toEqual([claimId]);
        expect(after.document).toContain("峰值 QPS 在 2027 年前不超过 3 倍当前值");

        // Evidence attaches to the claim and is traceable back through it.
        const evidenceProposal = submitProposal(read, {
          kind: "evidence-attach",
          subject: "review-premise",
          expectedRevision: after.conceptRevision,
          reason: "cite the capacity review",
          author: "external-agent",
          payload: {
            kind: "text",
            title: "容量评审记录 2026-11-04",
            locator: "evidence/review-2026-11-04-capacity.md",
            excerpt: "SRE 指出：3 倍假设没有经过压测，也没有历史增长曲线支持。",
            capturedBy: "sre-lead",
            targets: [{ type: "claim", id: claimId }],
          },
        });
        const evidenceId = reviewProposal(read, evidenceProposal.id, "approve", "arch-reviewer").applied[0]!;

        const view = evidenceForTarget(read, "claim", claimId);
        expect(view.evidence.map((item) => item.id)).toEqual([evidenceId]);
        expect(view.concepts.map((concept) => concept.id)).toContain("review-premise");
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });

  test("an image asset is stored by content hash and rejected when the bytes disagree", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const png = await readFile(join(FIXTURE_KB, "assets", "latency-chart.png"));

      const seeded = db(root);
      let asset: Awaited<ReturnType<typeof uploadAsset>>;
      try {
        asset = await uploadAsset(seeded, root, new Uint8Array(png), "image/png");
      } finally {
        seeded.close();
      }
      expect(asset.sha256).toBe(createHash("sha256").update(png).digest("hex"));
      expect(asset.mediaType).toBe("image/png");

      // Same bytes, same address: no second copy.
      const again = db(root);
      try {
        expect((await uploadAsset(again, root, new Uint8Array(png))).sha256).toBe(asset.sha256);
        await expect(uploadAsset(again, root, new Uint8Array(png), "image/jpeg")).rejects.toThrow(/Declared/);
        await expect(uploadAsset(again, root, new Uint8Array([1, 2, 3]))).rejects.toThrow(/Unsupported/);
      } finally {
        again.close();
      }

      const read = db(root);
      try {
        const document = conceptDocument(read, "review-risk")!;
        const proposal = submitProposal(read, {
          kind: "evidence-attach",
          subject: "review-risk",
          expectedRevision: document.conceptRevision,
          reason: "attach the latency chart",
          author: "external-agent",
          payload: {
            kind: "image",
            title: "p99 延迟曲线",
            asset,
            capturedBy: "sre-lead",
            targets: [{ type: "concept", id: "review-risk" }],
          },
        });
        const evidenceId = reviewProposal(read, proposal.id, "approve", "arch-reviewer").applied[0]!;
        const item = evidenceForTarget(read, "concept", "review-risk").evidence.find((entry) => entry.id === evidenceId);
        expect(item?.asset?.sha256).toBe(asset.sha256);
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });

  test("evidence for a target that does not exist is refused", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        const document = conceptDocument(read, "review-risk")!;
        expect(() =>
          submitProposal(read, {
            kind: "evidence-attach",
            subject: "review-risk",
            expectedRevision: document.conceptRevision,
            reason: "dangling target",
            author: "external-agent",
            payload: {
              kind: "text",
              title: "orphan",
              excerpt: "text",
              capturedBy: "someone",
              targets: [{ type: "claim", id: "clm_doesnotexist" }],
            },
          }),
        ).toThrow();
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });
});

describe("review conflicts", () => {
  test("two proposals against the same base: the second is a conflict, not a lost update", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        const document = conceptDocument(read, "review-decision")!;
        const first = submitProposal(read, {
          kind: "claim-create",
          subject: "review-decision",
          expectedRevision: document.conceptRevision,
          reason: "first",
          author: "agent-a",
          payload: claim("We chose SQLite for structured authority", "constraint"),
        });
        const second = submitProposal(read, {
          kind: "claim-create",
          subject: "review-decision",
          expectedRevision: document.conceptRevision,
          reason: "second",
          author: "agent-b",
          payload: claim("We chose Markdown for structured authority", "constraint"),
        });

        reviewProposal(read, first.id, "approve", "reviewer");
        expect(() => reviewProposal(read, second.id, "approve", "reviewer")).toThrow(ConflictError);

        // The conflicted proposal is retained for resubmission, not deleted.
        const proposals = read
          .query<{ status: string }, [string]>("SELECT status FROM proposals WHERE id = ?")
          .all(second.id);
        expect(proposals[0]?.status).toBe("conflicted");
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });

  test("a proposal written against a stale revision is refused at submission", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        expect(() =>
          submitProposal(read, {
            kind: "claim-create",
            subject: "review-decision",
            expectedRevision: "0".repeat(64),
            reason: "stale",
            author: "agent-a",
            payload: claim("stale claim"),
          }),
        ).toThrow(ConflictError);
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });

  test("approving the same proposal twice is refused", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        const document = conceptDocument(read, "review-decision")!;
        const proposal = submitProposal(read, {
          kind: "claim-create",
          subject: "review-decision",
          expectedRevision: document.conceptRevision,
          reason: "once",
          author: "agent-a",
          payload: claim("only once"),
        });
        reviewProposal(read, proposal.id, "approve", "reviewer");
        expect(() => reviewProposal(read, proposal.id, "approve", "reviewer")).toThrow(/already approved/);
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });

  test("a rejected proposal leaves the authority untouched", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        const document = conceptDocument(read, "review-constraint")!;
        const proposal = submitProposal(read, {
          kind: "claim-create",
          subject: "review-constraint",
          expectedRevision: document.conceptRevision,
          reason: "bad idea",
          author: "agent-a",
          payload: claim("remove the latency budget", "constraint"),
        });
        reviewProposal(read, proposal.id, "reject", "reviewer", "not supported by evidence");
        expect(conceptDocument(read, "review-constraint")!.claims).toEqual([]);
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });

  test("a relation that would create a cycle is refused inside the transaction", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        const before = relationSetRevision(read, "review-risk");
        const proposal = submitProposal(read, {
          kind: "relation-assert",
          subject: "review-risk",
          expectedRevision: before,
          reason: "cycle test",
          author: "agent-a",
          payload: { from: "review-risk", type: "depends-on", target: "review-decision" },
        });
        // review-risk already depends-on review-decision, so this is a duplicate
        // and allowed; a real cycle needs the reverse edge.
        reviewProposal(read, proposal.id, "approve", "reviewer");

        const reverse = submitProposal(read, {
          kind: "relation-assert",
          subject: "review-decision",
          expectedRevision: relationSetRevision(read, "review-decision"),
          reason: "cycle test",
          author: "agent-a",
          payload: { from: "review-decision", type: "depends-on", target: "review-risk" },
        });
        expect(() => reviewProposal(read, reverse.id, "approve", "reviewer")).toThrow(/cycle/i);
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });
});

describe("hybrid retrieval", () => {
  /** A deterministic fixture embedding: term-presence vector, not a model. */
  const FIXTURE_MODEL = "fixture/deterministic-bow";
  const AXES = ["容量", "延迟", "约束", "风险", "决策"];

  function fixtureVector(text: string): number[] {
    return AXES.map((axis) => (text.includes(axis) ? 1 : 0));
  }

  async function withIndexedKb(run: (root: string, read: ReturnType<typeof openStore>) => Promise<void>): Promise<void> {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);

      // Seed claims from the review records, then embed everything the fixture
      // model can see. The vectors are fixtures: the revision must match.
      for (const conceptId of ["review-premise", "review-constraint", "review-tension"]) {
        const document = conceptDocument(read, conceptId)!;
        const text =
          conceptId === "review-premise"
            ? "容量假设：3 倍流量下数据库连接池仍是瓶颈"
            : conceptId === "review-constraint"
              ? "约束：p99 延迟必须低于 500ms"
              : "延迟与容量共同构成风险，张力在于二者不同向";
        const proposal = submitProposal(read, {
          kind: "claim-create",
          subject: conceptId,
          expectedRevision: document.conceptRevision,
          reason: "fixture claim from review record",
          author: "fixture",
          payload: claim(text, conceptId === "review-tension" ? "risk" : "constraint"),
        });
        reviewProposal(read, proposal.id, "approve", "fixture-reviewer");
      }

      const subjects: Array<{ type: "concept" | "claim"; id: string; text: string }> = [];
      for (const concept of read.query<{ id: string; body: string; atom: string }, []>("SELECT id, body, atom FROM concepts").all()) {
        subjects.push({
          type: "concept",
          id: concept.id,
          text: embeddingTextForConcept({ atom: concept.atom, body: concept.body }),
        });
      }
      for (const row of read.query<{ id: string; text: string }, []>("SELECT id, text FROM claims").all()) {
        subjects.push({ type: "claim", id: row.id, text: embeddingTextForClaim({ text: row.text }) });
      }

      upsertVectors(read, {
        vectors: subjects.map((subject) => ({
          subjectType: subject.type,
          subjectId: subject.id,
          model: FIXTURE_MODEL,
          provider: "fixture",
          revision: embeddingRevision(subject.text),
          values: fixtureVector(subject.text),
        })),
      });

      await run(root, read);
      read.close();
    } finally {
      await cleanup();
    }
  }

  test("lexical hits are explained by their matched terms", async () => {
    await withIndexedKb(async (_root, read) => {
      const result = hybridSearch(read, { query: "约束", limit: 5 });
      expect(result.mode).toBe("lexical");
      expect(result.results[0]?.conceptId).toBe("review-constraint");
      const signal = result.results[0]?.signals.find((entry) => entry.axis === "lexical");
      expect(signal?.axis).toBe("lexical");
      if (signal?.axis === "lexical") expect(signal.terms.length).toBeGreaterThan(0);
    });
  });

  test("a vector-only match is still found, and associations stay separate from asserted relations", async () => {
    await withIndexedKb(async (_root, read) => {
      // "SLA" is in review-constraint's triggers but not in its indexed text, so a
      // vector that names the concept is the only way it surfaces here.
      const query = new Array(AXES.length).fill(0);
      query[AXES.indexOf("约束")] = 1;

      const result = hybridSearch(read, {
        query: "unrelated words",
        limit: 5,
        vector: { model: FIXTURE_MODEL, values: query },
      });

      expect(result.mode).toBe("hybrid");
      expect(result.vector?.model).toBe(FIXTURE_MODEL);
      expect(result.vector?.excludedStale).toBe(0);
      const hit = result.results.find((candidate) => candidate.conceptId === "review-constraint");
      expect(hit).toBeDefined();
      if (hit !== undefined) {
        expect(hit.signals.some((signal) => signal.axis === "vector")).toBe(true);
        // The constraint depends on review-subject — asserted, not inferred.
        expect(hit.assertedRelations.some((relation) => relation.target === "review-subject")).toBe(true);
      }
    });
  });

  test("a stale vector is excluded and reported, never ranked", async () => {
    await withIndexedKb(async (_root, read) => {
      // Rewriting the concept body moves the embedded text, so the vector that
      // was computed from the old text describes a document that is gone.
      const live = conceptDocument(read, "review-constraint")!;
      const edited = live.document.replace(
        "约束与前提的区别是来源",
        "约束与前提的区别是来源与可协商性",
      );
      expect(edited).not.toBe(live.document);
      const proposal = submitProposal(read, {
        kind: "concept-markdown",
        subject: "review-constraint",
        expectedRevision: live.revision,
        reason: "clarify the constraint/premise distinction",
        author: "fixture",
        payload: { document: edited },
      });
      reviewProposal(read, proposal.id, "approve", "fixture-reviewer");

      const query = new Array(AXES.length).fill(0);
      query[AXES.indexOf("约束")] = 1;
      const result = hybridSearch(read, {
        query: "zzz",
        limit: 5,
        vector: { model: FIXTURE_MODEL, values: query },
      });
      expect(result.vector!.excludedStale).toBeGreaterThan(0);
      expect(result.notes.some((note) => note.includes("stale"))).toBe(true);
      expect(result.results.some((hit) => hit.kind === "concept" && hit.id === "review-constraint")).toBe(false);
    });
  });

  test("a vector for a different model or dimension does not participate", async () => {
    await withIndexedKb(async (_root, read) => {
      const result = hybridSearch(read, {
        query: "约束",
        limit: 5,
        vector: { model: "some/other-model", values: [1, 0, 0, 0, 0] },
      });
      expect(result.vector!.missing).toBeGreaterThan(0);
      expect(result.results.every((hit) => hit.signals.every((signal) => signal.axis !== "vector"))).toBe(true);

      expect(() =>
        hybridSearch(read, { query: "约束", vector: { model: FIXTURE_MODEL, values: [1, 0] } }),
      ).not.toThrow();
    });
  });

  test("an uploaded vector whose revision does not match the live text is refused", async () => {
    await withIndexedKb(async (_root, read) => {
      expect(() =>
        upsertVectors(read, {
          vectors: [
            {
              subjectType: "concept",
              subjectId: "review-risk",
              model: "fixture/model",
              revision: "f".repeat(64),
              values: [1, 0, 0, 0, 0],
            },
          ],
        }),
      ).toThrow(ConflictError);
    });
  });
});

describe("revision discovery", () => {
  /**
   * The endpoint hands out a baseline; submission re-derives it. These must be
   * the same string for every kind, or a client that follows the documentation
   * gets a guaranteed conflict.
   */
  test("every kind's advertised revision is accepted by its submission", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        const conceptId = "review-premise";
        const document = conceptDocument(read, conceptId)!;

        const cases: Array<{ kind: Parameters<typeof submitProposal>[1]["kind"]; subject: string; payload: Record<string, unknown> }> = [
          {
            kind: "claim-create",
            subject: conceptId,
            payload: { kind: "premise", text: "advertised revision", confidence: "confirmed" },
          },
          {
            kind: "relation-assert",
            subject: conceptId,
            payload: { from: conceptId, type: "enables", target: "review-risk", confidence: "hypothetical" },
          },
          {
            kind: "evidence-attach",
            subject: conceptId,
            payload: {
              kind: "text",
              title: "advertised revision",
              excerpt: "text",
              capturedBy: "fixture",
              targets: [{ type: "concept", id: conceptId }],
            },
          },
          {
            kind: "concept-markdown",
            subject: conceptId,
            payload: { document: document.document },
          },
        ];

        for (const testCase of cases) {
          const expected = expectedRevisionFor(read, testCase.kind, testCase.subject, testCase.payload);
          expect(() =>
            submitProposal(read, {
              kind: testCase.kind,
              subject: testCase.subject,
              expectedRevision: expected,
              reason: `revision probe: ${testCase.kind}`,
              author: "fixture",
              payload: testCase.payload,
            }),
          ).not.toThrow();
        }
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });

  test("a claim-create whose payload names another concept is refused", async () => {
    const { root, cleanup } = await fixtureKb();
    try {
      await importKnowledgeBase({ root });
      const read = db(root);
      try {
        const document = conceptDocument(read, "review-premise")!;
        expect(() =>
          submitProposal(read, {
            kind: "claim-create",
            subject: "review-premise",
            expectedRevision: document.conceptRevision,
            reason: "mismatched concept",
            author: "fixture",
            payload: { conceptId: "review-risk", kind: "premise", text: "x", confidence: "confirmed" },
          }),
        ).toThrow(/disagrees/);
      } finally {
        read.close();
      }
    } finally {
      await cleanup();
    }
  });
});

describe("kb isolation", () => {
  test("each KB has its own database and its own rows", async () => {
    const first = await fixtureKb();
    const second = await fixtureKb();
    try {
      await importKnowledgeBase({ root: first.root });
      const secondStore = join(second.root, "..", "other");
      await cp(join(second.root, "concepts"), join(secondStore, "concepts"), { recursive: true });
      await importKnowledgeBase({ root: secondStore });

      const a = db(first.root);
      const b = db(secondStore);
      try {
        a.run("UPDATE concepts SET atom = 'changed in A' WHERE id = 'review-risk'");
        expect(b.query("SELECT atom FROM concepts WHERE id = 'review-risk'").get()).not.toEqual({
          atom: "changed in A",
        });
      } finally {
        a.close();
        b.close();
      }
      expect(join(first.root, "knowledge.db")).not.toBe(join(secondStore, "knowledge.db"));
    } finally {
      await first.cleanup();
      await second.cleanup();
    }
  });
});
