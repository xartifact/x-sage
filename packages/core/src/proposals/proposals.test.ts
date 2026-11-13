import { describe, expect, test } from "bun:test";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  mergeAtomProposal,
  mergeEdgeProposal,
  ProposalConflictError,
  proposeEdge,
  refineAtom,
} from "./index.ts";
import { conceptRevision, splitConceptDocument } from "../manifest/body.ts";
import { addFrontierEntry } from "../frontier/update.ts";
import { formatConceptFile, parseConceptFile } from "../manifest/parse.ts";

/**
 * Proposal write/merge contract.
 *
 * Every case here corresponds to a way the old implementation damaged the KB or
 * lied about the result: unescaped YAML, path traversal from a request body,
 * same-day proposal collisions, and merges that applied a non-pending proposal.
 */

const CONCEPT = `---
id: risk
atom: "风险是未来潜在损失的不确定性"
layer: 1
domain: [product]
confidence: confirmed
source: personal
triggers: [风险, risk]
edges:
  - type: part-of
    target: agent
    confidence: confirmed
---

# 风险 Risk

## 核心内容（molecule）

正文。

## 张力与开放问题

无。
`;

async function makeKb(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "x-sage-proposal-"));
  await writeFile(join(root, "kb.json"), JSON.stringify({ id: "t", name: "t" }));
  for (const layer of ["L0-axioms", "L1-core", "L2-support", "L3-instances"]) {
    await import("node:fs/promises").then((fs) => fs.mkdir(join(root, "concepts", layer), { recursive: true }));
  }
  await writeFile(join(root, "concepts", "L1-core", "risk.md"), CONCEPT);
  await writeFile(
    join(root, "concepts", "L0-axioms", "agent.md"),
    CONCEPT.replace(/^id: risk$/m, "id: agent").replace(/^layer: 1$/m, "layer: 0"),
  );
  return root;
}

describe("concept serialisation", () => {
  test("an atom containing quotes survives a round trip", async () => {
    // The old hand-rolled formatter wrapped every string in `"` without
    // escaping, so this atom produced a file no later parse would accept: the
    // concept showed as invalid and /api/viz failed for the whole KB.
    const hostile = {
      frontmatter: {
        id: "risk",
        atom: '风险是"下行"不确定性的暴露程度',
        layer: 1 as const,
        domain: ["product"],
        confidence: "confirmed" as const,
        source: "personal" as const,
        triggers: ["风险", "a: b", "#tag", "- dash"],
        edges: [],
      },
      molecule: "# 风险\n\n正文",
      filePath: "risk.md",
    };

    const serialised = formatConceptFile(hostile);
    const reparsed = parseConceptFile(serialised, "risk.md");
    expect(reparsed.frontmatter.atom).toBe(hostile.frontmatter.atom);
    expect(reparsed.frontmatter.triggers).toEqual(hostile.frontmatter.triggers);
  });

  test("splitting a document leaves every metadata byte untouched", () => {
    // Body edits must not reformat frontmatter: `domain: [product]` becoming a
    // block list turns an unrelated prose edit into a metadata diff.
    const { prefix, body } = splitConceptDocument(CONCEPT);
    expect(prefix.endsWith("---\n")).toBe(true);
    expect(prefix + body).toBe(CONCEPT);
    expect(prefix).toContain("domain: [product]");
  });

  test("a file without frontmatter is rejected", () => {
    expect(() => splitConceptDocument("# 只有正文\n")).toThrow(/frontmatter/);
    expect(() => splitConceptDocument("---\nid: x\n")).toThrow(/Unterminated/);
  });

  test("revision tracks atom, edge and body changes", () => {
    const base = parseConceptFile(CONCEPT, "risk.md");
    const original = conceptRevision(base.frontmatter, base.molecule);

    const newAtom = structuredClone(base.frontmatter);
    newAtom.atom = "另一个 atom";
    expect(conceptRevision(newAtom, base.molecule)).not.toBe(original);

    const newEdge = structuredClone(base.frontmatter);
    newEdge.edges.push({ type: "depends-on", target: "agent", confidence: "confirmed" });
    expect(conceptRevision(newEdge, base.molecule)).not.toBe(original);

    expect(conceptRevision(base.frontmatter, `${base.molecule}\n追加`)).not.toBe(original);
    expect(conceptRevision(base.frontmatter, base.molecule)).toBe(original);
  });
});

describe("proposal identity", () => {
  test("an id that is a path is refused", async () => {
    const root = await makeKb();
    try {
      // `../../../escape` used to become a filename and write outside the KB.
      await expect(
        refineAtom(join(root, "proposals"), {
          conceptId: "../../../escape",
          currentAtom: "a",
          proposedAtom: "b",
          reason: "r",
        }),
      ).rejects.toThrow(/Invalid concept id/);
      await expect(
        proposeEdge(join(root, "proposals"), {
          from: "../../../escape",
          type: "is-a",
          target: "agent",
          confidence: "confirmed",
        }),
      ).rejects.toThrow(/Invalid concept id/);
      expect(await readdir(root)).not.toContain("escape-2026-11-13.md");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("two proposals for one concept the same day do not overwrite each other", async () => {
    const root = await makeKb();
    try {
      const proposals = join(root, "proposals");
      const first = await refineAtom(proposals, {
        conceptId: "risk", currentAtom: "a", proposedAtom: "first candidate", reason: "r",
      });
      const second = await refineAtom(proposals, {
        conceptId: "risk", currentAtom: "a", proposedAtom: "second candidate", reason: "r",
      });

      expect(first).not.toBe(second);
      expect(await readFile(join(proposals, "atoms", first), "utf-8")).toContain("first candidate");
      expect(await readFile(join(proposals, "atoms", second), "utf-8")).toContain("second candidate");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("an over-long atom is refused at submission", async () => {
    const root = await makeKb();
    try {
      // Accepting it here only moved the failure to merge time, where it
      // rewrote the concept with an atom the schema rejects forever after.
      await expect(
        refineAtom(join(root, "proposals"), {
          conceptId: "risk", currentAtom: "a", proposedAtom: "x".repeat(201), reason: "r",
        }),
      ).rejects.toThrow(/at most 200/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

describe("merge safety", () => {
  test("a proposal that is not pending is not applied", async () => {
    const root = await makeKb();
    try {
      const path = join(root, "proposals", "atoms", "rejected.md");
      await import("node:fs/promises").then((fs) => fs.mkdir(join(root, "proposals", "atoms"), { recursive: true }));
      await writeFile(
        path,
        "---\nstatus: rejected\nconcept: risk\n---\n## 当前 atom\n风险是未来潜在损失的不确定性\n## 建议新 atom\nREJECTED BUT APPLIED\n",
      );

      await expect(mergeAtomProposal(root, "rejected.md")).rejects.toThrow(/expected "pending"/);
      expect(await readFile(join(root, "concepts", "L1-core", "risk.md"), "utf-8")).not.toContain("REJECTED");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a proposal filename cannot leave the proposals directory", async () => {
    const root = await makeKb();
    try {
      await expect(mergeAtomProposal(root, "../../concepts/L1-core/risk.md")).rejects.toThrow(
        /Invalid proposal filename/,
      );
      await expect(mergeEdgeProposal(root, "../atoms/x.md")).rejects.toThrow(/Invalid proposal filename/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("an atom proposal whose baseline moved is a conflict, not a silent overwrite", async () => {
    const root = await makeKb();
    try {
      const proposals = join(root, "proposals");
      const file = await refineAtom(proposals, {
        conceptId: "risk",
        currentAtom: "风险是未来潜在损失的不确定性",
        proposedAtom: "新 atom",
        reason: "r",
      });

      // Someone else edits the atom first.
      const conceptPath = join(root, "concepts", "L1-core", "risk.md");
      await writeFile(
        conceptPath,
        (await readFile(conceptPath, "utf-8")).replace(
          /^atom: .*$/m,
          'atom: "被另一个人改过的 atom"',
        ),
      );

      await expect(mergeAtomProposal(root, file)).rejects.toBeInstanceOf(ProposalConflictError);
      expect(await readFile(conceptPath, "utf-8")).toContain("被另一个人改过的 atom");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("an edge proposal for an unknown concept names the concept", async () => {
    const root = await makeKb();
    try {
      await import("node:fs/promises").then((fs) => fs.mkdir(join(root, "proposals", "edges"), { recursive: true }));
      await writeFile(
        join(root, "proposals", "edges", "ghost.md"),
        "---\nstatus: pending\n---\n## 新边提案\n\n- **from**: ghost\n- **type**: is-a\n- **target**: agent\n- **confidence**: confirmed\n",
      );
      await expect(mergeEdgeProposal(root, "ghost.md")).rejects.toThrow(/Concept not found: ghost/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});
describe("frontier insertion", () => {
  test("adds one row without touching the other tables", async () => {
    const root = await mkdtemp(join(tmpdir(), "x-sage-frontier-"));
    try {
      await writeFile(
        join(root, "frontier.md"),
        [
          "# 激活边界 frontier.md",
          "",
          "## 未决概念",
          "",
          "| 概念 | 来源 | 未解问题 | 优先级 |",
          "|------|------|----------|--------|",
          "| A | 来源A | 问题A | 高 |",
          "",
          "## 概念内张力",
          "",
          "| 张力 | 涉及概念 | 状态 | 说明 |",
          "|------|---------|------|------|",
          "| 甲 | a | 开放 | 说明甲 |",
          "",
          "## L2→L3 待展开方向",
          "",
          "| L3 候选方向 | 父概念 | 说明 |",
          "|------------|--------|------|",
          "| 乙 | b | 说明乙 |",
          "",
        ].join("\n"),
      );

      await addFrontierEntry(root, { concept: "B", source: "来源B", reason: "问题B", priority: "medium" });

      const after = await readFile(join(root, "frontier.md"), "utf-8");
      // The old implementation re-rendered the file from the parsed first table
      // and silently deleted both prose tables below it.
      expect(after).toContain("## 概念内张力");
      expect(after).toContain("| 甲 | a | 开放 | 说明甲 |");
      expect(after).toContain("## L2→L3 待展开方向");
      expect(after).toContain("| 乙 | b | 说明乙 |");
      // Inserted after the last row of the pending table, not at the end.
      expect(after.indexOf("| B | 来源B | 问题B | medium |")).toBeLessThan(after.indexOf("## 概念内张力"));
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a file without the pending table is reported instead of guessed at", async () => {
    const root = await mkdtemp(join(tmpdir(), "x-sage-frontier-"));
    try {
      await writeFile(join(root, "frontier.md"), "# 只有散文\n\n没有表格。\n");
      await expect(
        addFrontierEntry(root, { concept: "B", source: "s", reason: "r", priority: "low" }),
      ).rejects.toThrow(/no 未决概念 table/);
      expect(await readFile(join(root, "frontier.md"), "utf-8")).toBe("# 只有散文\n\n没有表格。\n");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});
