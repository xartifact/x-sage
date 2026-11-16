import { describe, expect, test } from "bun:test";
import { mkdtemp, cp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createApp } from "../src/app.ts";
import { embeddingRevision, embeddingTextForClaim, embeddingTextForConcept } from "@x-sage/core/knowledge";
import { conceptDocument, openStore, submitProposal, reviewProposal, upsertVectors } from "@x-sage/service/store";

/**
 * The architecture-review fixed case, end to end.
 *
 * This is the acceptance scenario written down in
 * `docs/architecture-review-case.md`: premises recorded as claims, a contested
 * tension, text and image evidence, and hybrid retrieval over externally
 * supplied (fixture) vectors. If the doc and the system disagree, this fails.
 */

const FIXTURE_KB = resolve(import.meta.dir, "../../../fixtures/architecture-review");

/** Fixture embedding: five axes, 0/1 presence. Not a model; the values are fixed. */
const AXES = ["容量", "延迟", "约束", "风险", "决策"];
const FIXTURE_MODEL = "fixture/deterministic-bow";

const FIXED_CLAIMS = [
  { concept: "review-premise", kind: "premise" as const, text: "峰值 QPS 在 2027 年前不超过 3 倍当前值", confidence: "hypothetical" as const },
  { concept: "review-constraint", kind: "constraint" as const, text: "p99 延迟必须低于 500ms", confidence: "confirmed" as const },
  { concept: "review-tension", kind: "risk" as const, text: "满足当前约束与前提失效后的风险不同向", confidence: "contested" as const },
];

function fixtureVector(text: string): number[] {
  return AXES.map((axis) => (text.includes(axis) ? 1 : 0));
}

describe("architecture review fixed case", () => {
  test("records premises with evidence and answers the review questions", async () => {
    const dir = await mkdtemp(join(tmpdir(), "x-sage-case-"));
    await cp(FIXTURE_KB, join(dir, "kb"), { recursive: true });
    const kbRoot = join(dir, "kb");
    const app = createApp({ kbRoot });
    try {
      const imported = await app.request("/api/knowledge/import", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ mode: "authority" }),
      });
      expect(imported.status).toBe(201);

      const db = openStore(kbRoot);
      const claimIds: Record<string, string> = {};
      try {
        // 1. The three review conclusions, each through a review.
        for (const claim of FIXED_CLAIMS) {
          const document = conceptDocument(db, claim.concept)!;
          const proposal = submitProposal(db, {
            kind: "claim-create",
            subject: claim.concept,
            expectedRevision: document.conceptRevision,
            reason: "architecture review conclusion",
            author: "review-agent",
            payload: { kind: claim.kind, text: claim.text, confidence: claim.confidence },
          });
          const reviewed = reviewProposal(db, proposal.id, "approve", "arch-reviewer");
          claimIds[claim.concept] = reviewed.applied[0]!;

          // Before approval the claim was not authoritative; after, it is.
          expect(conceptDocument(db, claim.concept)!.claims.map((item) => item.id)).toContain(
            reviewed.applied[0]!,
          );
        }
      } finally {
        db.close();
      }

      // 2. Evidence: a review record for the capacity premise, a document plus a
      //    chart for the latency conclusion. The image arrives through HTTP, so
      //    the upload → record → attach path is exercised as a client uses it.
      const png = await readFile(join(FIXTURE_KB, "assets", "latency-chart.png"));
      const upload = await app.request("/api/knowledge/assets", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ data: png.toString("base64") }),
      });
      expect(upload.status).toBe(201);
      const asset = (await upload.json()) as { sha256: string; mediaType: string; byteSize: number };
      expect(asset.mediaType).toBe("image/png");

      const seeded = openStore(kbRoot);
      try {
        const capacityDoc = conceptDocument(seeded, "review-premise")!;
        const textEvidence = submitProposal(seeded, {
          kind: "evidence-attach",
          subject: "review-premise",
          expectedRevision: capacityDoc.conceptRevision,
          reason: "cite the capacity review",
          author: "review-agent",
          payload: {
            kind: "text",
            title: "容量评审记录 2026-11-04",
            locator: "evidence/review-2026-11-04-capacity.md",
            excerpt: "SRE 指出：3 倍假设没有经过压测，也没有历史增长曲线支持。",
            capturedBy: "sre-lead",
            targets: [{ type: "claim", id: claimIds["review-premise"]! }],
          },
        });
        reviewProposal(seeded, textEvidence.id, "approve", "arch-reviewer");

        const tensionDoc = conceptDocument(seeded, "review-tension")!;
        const imageEvidence = submitProposal(seeded, {
          kind: "evidence-attach",
          subject: "review-tension",
          expectedRevision: tensionDoc.conceptRevision,
          reason: "attach the latency curve that shows the two directions",
          author: "review-agent",
          payload: {
            kind: "image",
            title: "p99 延迟曲线",
            asset,
            capturedBy: "sre-lead",
            targets: [{ type: "claim", id: claimIds["review-tension"]! }],
          },
        });
        reviewProposal(seeded, imageEvidence.id, "approve", "arch-reviewer");
      } finally {
        seeded.close();
      }

      // 3. Every premise is traceable to its source: claim → evidence → concept.
      const claimEvidence = await (
        await app.request(`/api/knowledge/claims/${claimIds["review-premise"]}/evidence`)
      ).json() as { evidence: Array<{ title: string; excerpt?: string }>; concepts: Array<{ id: string }> };
      expect(claimEvidence.evidence.map((item) => item.title)).toContain("容量评审记录 2026-11-04");
      expect(claimEvidence.evidence[0]?.excerpt).toContain("3 倍假设没有经过压测");
      expect(claimEvidence.concepts.map((concept) => concept.id)).toContain("review-premise");

      const tensionEvidence = await (
        await app.request("/api/knowledge/concepts/review-tension/evidence")
      ).json() as { evidence: Array<{ kind: string; asset?: { sha256: string } }> };
      expect(tensionEvidence.evidence.some((item) => item.kind === "image")).toBe(true);

      // 4. The contested relation survives as structure, not as an averaged score.
      const edges = await (
        await app.request("/api/concepts/review-tension/edges?type=contradicts")
      ).json() as { edges: Array<{ direction: string; other: { id: string } }> };
      expect(edges.edges.map((edge) => edge.other.id)).toContain("review-decision");
      expect(edges.edges[0]?.direction).toBe("both");

      // 5. Hybrid retrieval: lexical finds the word, vector finds the concept.
      const vectors = openStore(kbRoot);
      try {
        const subjects: Array<{ type: "concept" | "claim"; id: string; text: string }> = [];
        for (const row of vectors
          .query<{ id: string; atom: string; body: string }, []>("SELECT id, atom, body FROM concepts")
          .all()) {
          subjects.push({
            type: "concept",
            id: row.id,
            text: embeddingTextForConcept({ atom: row.atom, body: row.body }),
          });
        }
        for (const row of vectors.query<{ id: string; text: string }, []>("SELECT id, text FROM claims").all()) {
          subjects.push({ type: "claim", id: row.id, text: embeddingTextForClaim({ text: row.text }) });
        }
        upsertVectors(vectors, {
          vectors: subjects.map((subject) => ({
            subjectType: subject.type,
            subjectId: subject.id,
            model: FIXTURE_MODEL,
            provider: "fixture",
            revision: embeddingRevision(subject.text),
            values: fixtureVector(subject.text),
          })),
        });
      } finally {
        vectors.close();
      }

      const lexical = await (
        await app.request("/api/knowledge/search", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ query: "约束", limit: 5 }),
        })
      ).json() as {
        mode: string;
        results: Array<{ id: string; signals: Array<{ axis: string }>; assertedRelations: unknown[] }>;
      };
      expect(lexical.mode).toBe("lexical");
      expect(lexical.results[0]?.id).toBe("review-constraint");
      expect(lexical.results[0]?.signals[0]?.axis).toBe("lexical");

      const constrainedAxis = new Array(AXES.length).fill(0);
      constrainedAxis[AXES.indexOf("约束")] = 1;
      const hybrid = await (
        await app.request("/api/knowledge/search", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            query: "unrelated words",
            limit: 5,
            vector: { model: FIXTURE_MODEL, values: constrainedAxis },
          }),
        })
      ).json() as {
        mode: string;
        results: Array<{
          conceptId: string | null;
          signals: Array<{ axis: string }>;
          assertedRelations: Array<{ type: string; target: string }>;
          vectorAssociations: Array<{ cosine: number | null }>;
        }>;
      };
      expect(hybrid.mode).toBe("hybrid");

      const constraintHit = hybrid.results.find((hit) => hit.conceptId === "review-constraint");
      expect(constraintHit).toBeDefined();
      expect(constraintHit?.signals.some((signal) => signal.axis === "vector")).toBe(true);
      // Asserted relations stay in their own field; similarity never appears there.
      expect(constraintHit?.assertedRelations.some((relation) => relation.target === "review-subject")).toBe(true);
      for (const association of constraintHit?.vectorAssociations ?? []) {
        expect(association.cosine === null || typeof association.cosine === "number").toBe(true);
      }
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});
