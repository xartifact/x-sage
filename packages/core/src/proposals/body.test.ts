import { describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  mergeBodyProposal,
  proposeBody,
  readConceptBody,
  AlreadyMergedError,
  ProposalConflictError,
} from "./index.ts";
import { diffBodies, formatUnifiedDiff } from "./diff.ts";

/**
 * Body proposal contract.
 *
 * The properties under test are the ones a reviewer depends on: the applied
 * body is byte-identical to the candidate, frontmatter is never touched, a
 * stale baseline is a conflict rather than a silent overwrite, and a crash
 * between the two writes is recoverable.
 */

const FRONTMATTER = `---
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
`;

const BODY = `
# 风险 Risk

## 核心内容（molecule）

原始正文。

## 张力与开放问题

无。
`;

async function makeKb(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "x-sage-body-"));
  await mkdir(join(root, "concepts", "L1-core"), { recursive: true });
  await mkdir(join(root, "proposals", "bodies"), { recursive: true });
  await writeFile(join(root, "concepts", "L1-core", "risk.md"), FRONTMATTER + BODY);
  return root;
}

describe("reading a body", () => {
  test("returns the body verbatim and a revision that changes with content", async () => {
    const root = await makeKb();
    try {
      const first = await readConceptBody(root, "risk");
      expect(first.body).toBe(BODY);
      expect(first.atom).toBe("风险是未来潜在损失的不确定性");

      const again = await readConceptBody(root, "risk");
      expect(again.revision).toBe(first.revision);

      await writeFile(
        join(root, "concepts", "L1-core", "risk.md"),
        FRONTMATTER + BODY.replace("原始正文", "改过的正文"),
      );
      expect((await readConceptBody(root, "risk")).revision).not.toBe(first.revision);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("an id that is not a slug is refused before it becomes a path", async () => {
    const root = await makeKb();
    try {
      await expect(readConceptBody(root, "../../etc/passwd")).rejects.toThrow(/Invalid concept id/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("an unknown concept is reported by id", async () => {
    const root = await makeKb();
    try {
      await expect(readConceptBody(root, "nope")).rejects.toThrow(/Concept not found: nope/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

describe("applying a body", () => {
  test("writes the candidate exactly and leaves every frontmatter byte alone", async () => {
    const root = await makeKb();
    try {
      const before = await readFile(join(root, "concepts", "L1-core", "risk.md"), "utf-8");
      const { revision } = await readConceptBody(root, "risk");

      const candidate = `
# 风险 Risk

## 核心内容（molecule）

更正：风险是"下行"暴露。

- 列表项
- 另一项

\`\`\`ts
const x = "代码块";
\`\`\`

## 张力与开放问题

保留原意。
`;
      const file = await proposeBody(join(root, "proposals"), {
        conceptId: "risk",
        expectedRevision: revision,
        proposedBody: candidate,
        reason: "按澄清更正正文",
      });
      await mergeBodyProposal(root, file);

      const after = await readFile(join(root, "concepts", "L1-core", "risk.md"), "utf-8");
      // Frontmatter is a byte prefix, not a re-serialisation.
      expect(after.startsWith(FRONTMATTER)).toBe(true);
      expect(before.startsWith(FRONTMATTER)).toBe(true);
      expect(after.slice(FRONTMATTER.length)).toBe(candidate);
      expect((await readConceptBody(root, "risk")).body).toBe(candidate);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a stale revision is a conflict and leaves the concept untouched", async () => {
    const root = await makeKb();
    try {
      const { revision } = await readConceptBody(root, "risk");
      const file = await proposeBody(join(root, "proposals"), {
        conceptId: "risk",
        expectedRevision: revision,
        proposedBody: `${BODY}\n追加一行。\n`,
        reason: "r",
      });

      // Someone else changes the concept between proposal and merge.
      await writeFile(
        join(root, "concepts", "L1-core", "risk.md"),
        FRONTMATTER + BODY.replace("原始正文", "并发修改"),
      );

      await expect(mergeBodyProposal(root, file)).rejects.toBeInstanceOf(ProposalConflictError);
      expect(await readFile(join(root, "concepts", "L1-core", "risk.md"), "utf-8")).toContain("并发修改");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a proposal written against an outdated revision is refused at submission", async () => {
    const root = await makeKb();
    try {
      await expect(
        proposeBody(join(root, "proposals"), {
          conceptId: "risk",
          expectedRevision: "0".repeat(64),
          proposedBody: BODY,
          reason: "r",
        }),
      ).rejects.toBeInstanceOf(ProposalConflictError);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("two clients on one revision: the first wins, the second conflicts", async () => {
    const root = await makeKb();
    try {
      const { revision } = await readConceptBody(root, "risk");
      const proposals = join(root, "proposals");
      const first = await proposeBody(proposals, {
        conceptId: "risk", expectedRevision: revision, proposedBody: `${BODY}\n第一份。\n`, reason: "a",
      });
      const second = await proposeBody(proposals, {
        conceptId: "risk", expectedRevision: revision, proposedBody: `${BODY}\n第二份。\n`, reason: "b",
      });

      await mergeBodyProposal(root, first);
      await expect(mergeBodyProposal(root, second)).rejects.toBeInstanceOf(ProposalConflictError);
      expect((await readConceptBody(root, "risk")).body).toContain("第一份");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a retry after the concept write but before the status write still succeeds", async () => {
    const root = await makeKb();
    try {
      const { revision } = await readConceptBody(root, "risk");
      const candidate = `${BODY}\n新内容。\n`;
      const file = await proposeBody(join(root, "proposals"), {
        conceptId: "risk", expectedRevision: revision, proposedBody: candidate, reason: "r",
      });

      // Simulate a crash between the two writes: the concept already carries the
      // candidate, the proposal still says pending, and the revision has moved.
      const proposalPath = join(root, "proposals", "bodies", file);
      const proposal = JSON.parse(await readFile(proposalPath, "utf-8"));
      await writeFile(join(root, "concepts", "L1-core", "risk.md"), FRONTMATTER + candidate);

      await expect(mergeBodyProposal(root, file)).rejects.toBeInstanceOf(AlreadyMergedError);
      const after = JSON.parse(await readFile(proposalPath, "utf-8"));
      expect(after.status).toBe("approved");
      expect(proposal.expectedRevision).not.toBe((await readConceptBody(root, "risk")).revision);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a merged proposal is not applied twice", async () => {
    const root = await makeKb();
    try {
      const { revision } = await readConceptBody(root, "risk");
      const file = await proposeBody(join(root, "proposals"), {
        conceptId: "risk", expectedRevision: revision, proposedBody: `${BODY}\n一次性。\n`, reason: "r",
      });
      await mergeBodyProposal(root, file);
      await expect(mergeBodyProposal(root, file)).rejects.toBeInstanceOf(AlreadyMergedError);
      const body = (await readConceptBody(root, "risk")).body;
      expect(body.match(/一次性。/g)).toHaveLength(1);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("an empty, oversized or unparseable body is refused", async () => {
    const root = await makeKb();
    try {
      const { revision } = await readConceptBody(root, "risk");
      const proposals = join(root, "proposals");

      await expect(
        proposeBody(proposals, { conceptId: "risk", expectedRevision: revision, proposedBody: "   \n", reason: "r" }),
      ).rejects.toThrow(/empty/);

      await expect(
        proposeBody(proposals, {
          conceptId: "risk", expectedRevision: revision, proposedBody: "x".repeat(200 * 1024), reason: "r",
        }),
      ).rejects.toThrow(/limit/);

      // No heading and no content beyond the frontmatter.
      await expect(
        proposeBody(proposals, {
          conceptId: "risk", expectedRevision: revision, proposedBody: "\n\n", reason: "r",
        }),
      ).rejects.toThrow(/empty/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a body whose leading rule would confuse a YAML envelope is stored intact", async () => {
    const root = await makeKb();
    try {
      const { revision } = await readConceptBody(root, "risk");
      // A JSON envelope keeps this verbatim; a gray-matter envelope would parse
      // `---` as the end of metadata and truncate the candidate.
      const candidate = `---\n\n分隔线开头的正文。\n`;
      const file = await proposeBody(join(root, "proposals"), {
        conceptId: "risk", expectedRevision: revision, proposedBody: candidate, reason: "r",
      });
      await mergeBodyProposal(root, file);
      expect((await readConceptBody(root, "risk")).body).toBe(candidate);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("the stored candidate is what a reviewer diffs and what a merge writes", async () => {
    const root = await makeKb();
    try {
      const { revision } = await readConceptBody(root, "risk");
      // Neither the file a reviewer writes nor a terminal paste ends with
      // exactly one newline reliably. Normalising only at write time made the
      // read-back differ from the submitted candidate by one byte.
      const submitted = `${BODY.replace("原始正文", "改过的正文")}\n\n\n`;
      const file = await proposeBody(join(root, "proposals"), {
        conceptId: "risk", expectedRevision: revision, proposedBody: submitted, reason: "r",
      });

      const stored = JSON.parse(await readFile(join(root, "proposals", "bodies", file), "utf-8"));
      expect(stored.proposedBody).toBe(`${submitted.trimEnd()}\n`);

      await mergeBodyProposal(root, file);
      expect((await readConceptBody(root, "risk")).body).toBe(stored.proposedBody);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("a proposal needs a reason", async () => {
    const root = await makeKb();
    try {
      const { revision } = await readConceptBody(root, "risk");
      await expect(
        proposeBody(join(root, "proposals"), {
          conceptId: "risk", expectedRevision: revision, proposedBody: BODY, reason: "  ",
        }),
      ).rejects.toThrow(/reason/);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

describe("diff", () => {
  test("reports only changed regions, not the whole document", () => {
    const before = Array.from({ length: 60 }, (_, index) => `line ${index}`).join("\n") + "\n";
    const after = before.replace("line 30", "LINE 30");
    const hunks = diffBodies(before, after);
    expect(hunks).toHaveLength(1);
    // Three context lines each side (three before what was removed, three after
    // what was added) plus the replacement itself.
    expect(hunks[0]!.lines).toHaveLength(8);
    expect(hunks[0]!.lines).toContain("-line 30");
    expect(hunks[0]!.lines).toContain("+LINE 30");
  });

  test("identical bodies produce no hunks", () => {
    expect(diffBodies(BODY, BODY)).toEqual([]);
    expect(formatUnifiedDiff([], "a", "b")).toBe("No changes.\n");
  });

  test("a full rewrite is one hunk covering both sides", () => {
    const hunks = diffBodies("# A\n\nold\n", "# A\n\nnew\n");
    expect(hunks).toHaveLength(1);
    const text = formatUnifiedDiff(hunks, "current", "proposed");
    expect(text).toContain("--- current");
    expect(text).toContain("+new");
    expect(text).toContain("-old");
  });
});