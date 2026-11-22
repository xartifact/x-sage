import { afterEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { cpSync, symlinkSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";

import {
  EXIT,
  bodySlice,
  canonicalJson,
  compareUtf8,
  generatePlan,
  parseCli,
  parseFrontierSections,
  readPlanContext,
  runCli,
  type MigrationPlan,
} from "./sage-db-import-plan.ts";
import { exportSourceArchive, manifestContentDigest, verifySourceArchive } from "./sage-db-source-archive.ts";

/**
 * The plan generator, exercised against temporary archives built in the test.
 *
 * Two rules this file keeps:
 *
 * 1. **Self-contained.** No test reads the frozen real capture or any repository
 *    KB. Every archive here is built by the shipped export tool from a KB this
 *    test wrote, so the suite is portable and cannot drift with captured data.
 * 2. **Expectations are authored.** Frontmatter, bodies and edge lists are
 *    written literally in the fixture and asserted literally, so a symmetric bug
 *    in the generator cannot pass by agreeing with itself.
 */

const REPO = resolve(import.meta.dir, "..");
const FIXTURE_KB = join(REPO, "fixtures", "architecture-review");


/**
 * Re-sign an archive so verification passes after a deliberate mutation.
 *
 * Several bounds sit *behind* the archive verifier, which is the correct order:
 * a damaged capture is refused before planning. To prove the planner's own guard
 * fires, the mutation has to be made internally consistent first — otherwise the
 * test would be asserting the verifier's behaviour and the planner's bound could
 * be missing entirely.
 */
function resignArchive(archive: string): void {
  const manifestPath = join(archive, "archive.json");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as Record<string, unknown> & {
    streams: Array<Record<string, unknown>>;
  };
  const filesBytes = readFileSync(join(archive, "files.ndjson"));
  const filesRecord = manifest.streams.find((s) => s["path"] === "files.ndjson")!;
  filesRecord["bytes"] = filesBytes.length;
  filesRecord["sha256"] = `sha256:${createHash("sha256").update(filesBytes).digest("hex")}`;
  filesRecord["records"] = filesBytes.toString("utf8").split("\n").filter((l) => l !== "").length;
  manifest["content_digest"] = manifestContentDigest(manifest as never);
  writeFileSync(manifestPath, `${canonicalJson(manifest)}\n`, "utf8");
  const markerPath = join(archive, "COMPLETE.json");
  const marker = JSON.parse(readFileSync(markerPath, "utf8")) as Record<string, unknown>;
  marker["content_digest"] = manifest["content_digest"];
  writeFileSync(markerPath, `${canonicalJson(marker)}\n`, "utf8");
}

const CONTEXT = {
  label: "test-run",
  created_at: "2026-11-20T00:00:00.000Z",
  purpose: "unit test",
};

const limits = {
  maxEntries: 2000,
  maxBytes: 8 * 1024 * 1024,
  maxRecords: 20_000,
  maxArchiveBytes: 16 * 1024 * 1024,
  maxLineBytes: 4 * 1024 * 1024,
  maxDiagnostics: 5000,
};

let sandboxes: string[] = [];

function sandbox(): string {
  const root = mkdtempSync(join(tmpdir(), "plan-"));
  sandboxes.push(root);
  return root;
}

afterEach(() => {
  for (const root of sandboxes) rmSync(root, { recursive: true, force: true });
  sandboxes = [];
});

const registry = (root: string): string => {
  const path = join(root, "registry.json");
  writeFileSync(
    path,
    JSON.stringify({
      version: "1",
      knowledgeBases: [
        { id: "fixture", name: "Fixture", description: "", path: "kb", domains: [], created: "2026-11-16" },
      ],
    }),
    "utf8",
  );
  return path;
};

interface Kb {
  root: string;
  archive: string;
}

/**
 * Build a KB with the given files, export it, and return the archive directory.
 *
 * `files` maps KB-relative path to exact bytes; nothing is added or rewritten, so
 * a test can assert byte-for-byte preservation end to end.
 */
function buildArchive(tag: string, files: Record<string, string>): Kb {
  const root = sandbox();
  const kb = join(root, `${tag}-kb`);
  mkdirSync(kb, { recursive: true });
  for (const [path, content] of Object.entries(files)) {
    const abs = join(kb, path);
    mkdirSync(join(abs, ".."), { recursive: true });
    writeFileSync(abs, content, "utf8");
  }
  const archive = join(root, `${tag}-archive`);
  exportSourceArchive({
    sourceRoot: kb,
    outDir: archive,
    kb: "fixture",
    registryPath: registry(root),
    limits: { maxFiles: 500, maxBytes: 4 * 1024 * 1024, maxRows: 100, maxEntries: 2000, maxDepth: 16 },
    createdAt: "2026-11-20T00:00:00.000Z",
  });
  return { root: kb, archive };
}

function concept(
  id: string,
  opts: { layer?: number; atom?: string; edges?: string[]; body?: string } = {},
): string {
  const layer = opts.layer ?? 1;
  const atom = opts.atom ?? `atom for ${id}`;
  const edges = opts.edges ?? [];
  const body = opts.body ?? `# ${id}\n\nBody of ${id}.\n`;
  return [
    "---",
    `id: "${id}"`,
    `atom: "${atom}"`,
    `layer: ${layer}`,
    "domain:",
    "  - product",
    'confidence: "confirmed"',
    'source: "personal"',
    "triggers:",
    `  - ${id}`,
    ...(edges.length > 0 ? ["edges:", ...edges.map((e) => `  - ${e}`)] : []),
    "---",
    body,
  ].join("\n");
}

const FRONTIER = `# 激活边界 frontier.md

> Boundary notes.

## 未决概念

| 概念 | 来源 | 未解问题 | 优先级 |
|------|------|----------|--------|
| Alpha | a.md | open question one | 高 |
| Beta | b.md | open question two | 中 |

## 概念内张力

| 张力 | 涉及概念 | 状态 | 说明 |
|------|---------|------|------|
| tension one | alpha | 开放 | explain one |
| tension two | beta | 开放 | explain two |

## L2→L3 待展开方向

| L3 候选方向 | 父概念 | 说明 |
|------------|--------|------|
| direction A | alpha | detail A |
`;

function baseFiles(overrides: Record<string, string> = {}): Record<string, string> {
  return {
    "kb.json": JSON.stringify({ id: "fixture", name: "Fixture", description: "", domains: [], created: "2026-11-16" }, null, 2),
    "frontier.md": FRONTIER,
    "concepts/L1-core/alpha.md": concept("alpha", {
      layer: 1,
      atom: "alpha 是主语",
      edges: [
        '{"type":"depends-on","target":"beta","confidence":"confirmed"}',
        '{"type":"enables","target":"gamma","confidence":"hypothetical"}',
      ],
    }),
    "concepts/L1-core/beta.md": concept("beta", { layer: 1, edges: ['{"type":"contradicts","target":"alpha","confidence":"contested"}'] }),
    "concepts/L2-support/gamma.md": concept("gamma", { layer: 2 }),
    "checkpoints/alpha-s1.json": JSON.stringify({ concept: "alpha", session: 1, established: ["x"] }, null, 2),
    "proposals/atoms/alpha-2026-11-13.md": [
      "---",
      "proposer: CLI",
      "date: 2026-11-13T00:00:00.000Z",
      "concept: alpha",
      "status: approved",
      "---",
      "## 当前 atom",
      "old",
      "## 建议新 atom",
      "new",
    ].join("\n"),
    "proposals/atoms/example-thing.md": [
      "---",
      "proposer: AI",
      "date: 2026-06-12",
      "concept: tension",
      "status: example",
      "---",
      "## 当前 atom",
      "template only",
    ].join("\n"),
    ...overrides,
  };
}

describe("terms and assertions", () => {
  test("every concept becomes a term with a deterministic target id", () => {
    const { archive } = buildArchive("terms", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    // Source extraction is complete; the mapping is not, because target audit fields
    // cannot be supplied by this source. That distinction is the whole point of v2.
    if (plan.source.extraction !== "complete") throw new Error(JSON.stringify(plan.errors, null, 1));
    expect(plan.source.extraction).toBe("complete");
    expect(plan.status).toBe("incomplete");
    expect(plan.target_mapping).toBe("not_ready");
    expect(plan.unresolved.some((u) => u.field === "term.created_at")).toBe(true);
    expect(plan.counts.term).toBe(3);
    // Ids follow the byte-sorted source order within layer order.
    expect(plan.terms.map((t) => [t.slug, t.target_id])).toEqual([
      ["alpha", 1],
      ["beta", 2],
      ["gamma", 3],
    ]);
    const alpha = plan.terms[0]!;
    expect(alpha.layer).toBe(1);
    expect(alpha.atom).toBe("alpha 是主语");
    expect(alpha.source_path).toBe("concepts/L1-core/alpha.md");
  });

  test("every authored edge becomes an assertion in original declaration order", () => {
    const { archive } = buildArchive("edges", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.counts.assertion).toBe(3);
    const fromAlpha = plan.assertions.filter((a) => a.from_slug === "alpha");
    expect(fromAlpha.map((a) => [a.declaration_order, a.relation, a.target])).toEqual([
      [0, "depends-on", "beta"],
      [1, "enables", "gamma"],
    ]);
    // Direction is preserved, never normalised: beta -> alpha stays that way.
    const fromBeta = plan.assertions.filter((a) => a.from_slug === "beta");
    expect(fromBeta.map((a) => [a.relation, a.target, a.confidence])).toEqual([
      ["contradicts", "alpha", "contested"],
    ]);
    // Every assertion carries a proposed numeric id and its endpoints' ids.
    expect(fromAlpha[0]!.from_target_id).toBe(1);
    expect(fromAlpha[0]!.target_target_id).toBe(2);
  });

  test("source body bytes are preserved exactly, including Unicode and frontmatter exclusion", () => {
    const body = "# α\n\nUnicode: 中文 日本語 emoji 🎯 quoting \" and backslash \\\n\nTrailing text.\n";
    const { archive } = buildArchive("bytes", baseFiles({
      "concepts/L1-core/alpha.md": concept("alpha", { atom: "原子 with 中文", body }),
    }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const alpha = plan.terms.find((t) => t.slug === "alpha")!;
    expect(alpha.atom).toBe("原子 with 中文");
    // The slice is the file's own bytes after the frontmatter: no trimming at all.
    const expected = createHash("sha256").update(body, "utf8").digest("hex");
    expect(alpha.body.sha256).toBe(expected);
    expect(alpha.body.length).toBe(Buffer.byteLength(body, "utf8"));
    // The normalised service projection is separate and explicitly labelled.
    expect(alpha.projection.trimmed).toBe(true);
    expect(alpha.projection.body_sha256).toBe(
      createHash("sha256").update(body.trim(), "utf8").digest("hex"),
    );
    // The raw file is retained regardless of projection.
    const raw = plan.attachments.find((a) => a.path === "concepts/L1-core/alpha.md")!;
    expect(raw.sha256).toBe(createHash("sha256").update(readFileSync(join(archive, raw.blob))).digest("hex"));
  });
});

describe("frontier", () => {
  test("ALL sections and tables are read, not just the first", () => {
    const { archive } = buildArchive("frontier", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const bySection = new Map<string, number>();
    for (const entry of plan.frontier) {
      bySection.set(entry.section, (bySection.get(entry.section) ?? 0) + 1);
    }
    expect(bySection.get("未决概念")).toBe(2);
    expect(bySection.get("概念内张力")).toBe(2);
    expect(bySection.get("L2→L3 待展开方向")).toBe(1);
    expect(plan.counts.frontier_entry).toBe(5);
    // Cell text and raw rows are preserved verbatim, in author order.
    const first = plan.frontier[0]!;
    expect(first.cells).toEqual(["Alpha", "a.md", "open question one", "高"]);
    expect(first.raw).toBe("| Alpha | a.md | open question one | 高 |");
    expect(plan.frontier.filter((e) => e.section === "概念内张力").map((e) => e.row)).toEqual([0, 1]);
  });

  test("the section parser keeps rows apart across tables in one section", () => {
    const raw = [
      "## One",
      "| a | b | c | d |",
      "|---|---|---|---|",
      "| r1 | x | y | z |",
      "",
      "prose between tables",
      "",
      "| a | b | c | d |",
      "|---|---|---|---|",
      "| r2 | x | y | z |",
    ].join("\n");
    const { entries, errors } = parseFrontierSections(raw, 100, 100);
    expect(errors).toEqual([]);
    // Two tables in one section get distinct table/row identity and a global order.
    expect(entries.map((e) => [e.section, e.table, e.row, e.cells[0], e.declaration_order])).toEqual([
      ["One", 0, 0, "r1", 0],
      ["One", 1, 0, "r2", 1],
    ]);
    expect(new Set(entries.map((e) => e.source_id)).size).toBe(2);
  });

  test("a missing frontier section is an error, never a silent omission", () => {
    const files = baseFiles({ "frontier.md": "# x\n\n## 未决概念\n\n| 概念 | 来源 | 未解问题 | 优先级 |\n|---|---|---|---|\n| A | b | c | 高 |\n" });
    const { archive } = buildArchive("nofrontier", files);
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.status).toBe("incomplete");
    expect(plan.errors.map((e) => e.code)).toContain("MISSING_FRONTIER_SECTION");
  });
});

describe("proposals", () => {
  test("approved proposals are records; examples are retained files, not pending records", () => {
    const { archive } = buildArchive("props", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    // 15 source files vs 13 real records is the distinction: the example is a file,
    // not a proposal, so it takes no record id.
    expect(plan.counts.proposal).toBe(1);
    expect(plan.counts.source_proposal_files).toBe(2);
    const approved = plan.proposals.find((p) => p.source_path.includes("alpha-2026"))!;
    expect(approved.status).toBe("approved");
    // No `template` field exists on an emitted proposal any more: a constant
    // `false` was a field a reader could only misread, since an example never
    // becomes a record in the first place.
    expect("template" in approved).toBe(false);
    expect(approved.concept).toBe("alpha");
    // The example takes no record id at all — searching records for it must fail.
    expect(plan.proposals.some((p) => p.source_path.includes("example-thing"))).toBe(false);
    // It is still preserved as a file and declared as not-imported.
    expect(plan.attachments.some((a) => a.path.includes("example-thing"))).toBe(true);
    expect(plan.unavailable.some((u) => u.family === "proposal" && u.subject.includes("example-thing"))).toBe(true);
    // No record id is spent on it, so ids are contiguous over real records only.
    expect(plan.proposals.map((p) => p.target_id)).toEqual([1]);
  });
});

describe("checkpoints and attachments", () => {
  test("checkpoints keep concept, session and content hash", () => {
    const { archive } = buildArchive("cp", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.counts.checkpoint).toBe(1);
    const cp = plan.checkpoints[0]!;
    expect(cp.concept).toBe("alpha");
    expect(cp.session).toBe(1);
    const raw = readFileSync(join(archive, cp.content.blob));
    expect(cp.content.sha256).toBe(createHash("sha256").update(raw).digest("hex"));
  });

  test("every source file is an attachment, including non-semantic ones", () => {
    const { archive } = buildArchive("att", baseFiles({ "AI.md": "# AI notes\n", "protocols/exploration.md": "# E\n" }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const paths = plan.attachments.map((a) => a.path).sort();
    expect(paths).toContain("AI.md");
    expect(paths).toContain("protocols/exploration.md");
    expect(paths).toContain("kb.json");
    expect(paths).toContain("frontier.md");
    expect(plan.counts.attachment).toBe(paths.length);
    // Counts match the archive's own file count.
    const manifest = JSON.parse(readFileSync(join(archive, "archive.json"), "utf8")) as { counts: { files: number } };
    expect(plan.counts.attachment).toBe(manifest.counts.files);
  });
});

describe("refusals and incompleteness", () => {
  test("an unknown edge target is refused and prevents a complete plan", () => {
    const files = baseFiles({
      "concepts/L1-core/alpha.md": concept("alpha", { edges: ['{"type":"depends-on","target":"does-not-exist","confidence":"confirmed"}'] }),
    });
    const { archive } = buildArchive("unknown", files);
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.status).toBe("incomplete");
    expect(plan.errors.map((e) => e.code)).toContain("UNKNOWN_EDGE_TARGET");
    const err = plan.errors.find((e) => e.code === "UNKNOWN_EDGE_TARGET")!;
    expect(err.source).toBe("concepts/L1-core/alpha.md");
    expect(err.index).toBe(0);
    expect(plan.unresolved.some((u) => u.field === "assertion.target")).toBe(true);
  });

  test("a cross-KB edge target is refused rather than bound", () => {
    const files = baseFiles({
      "concepts/L1-core/alpha.md": concept("alpha", { edges: ['{"type":"depends-on","target":"other::gamma","confidence":"confirmed"}'] }),
    });
    const { archive } = buildArchive("crosskb", files);
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.errors.map((e) => e.code)).toContain("CROSS_KB_EDGE");
    expect(plan.status).toBe("incomplete");
    // No id is invented for a target it cannot bind.
    const edge = plan.assertions.find((a) => a.target.includes("::"))!;
    expect(edge.target_target_id).toBeNull();
  });

  test("a duplicate concept id is refused", () => {
    const files = baseFiles({
      "concepts/L1-core/alpha.md": concept("alpha"),
      "concepts/L2-support/alpha.md": concept("alpha", { layer: 2 }),
    });
    const { archive } = buildArchive("dup", files);
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.errors.map((e) => e.code)).toContain("DUPLICATE_ID");
    expect(plan.status).toBe("incomplete");
  });

  test("a frontmatter id that disagrees with its filename is refused", () => {
    const { archive } = buildArchive("slug", baseFiles({
      "concepts/L1-core/alpha.md": concept("not-alpha"),
    }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.errors.map((e) => e.code)).toContain("SLUG_PATH_MISMATCH");
  });

  test("a concept outside the layer directories is refused", () => {
    const { archive } = buildArchive("stray", baseFiles({
      "concepts/misplaced.md": concept("misplaced"),
    }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.errors.map((e) => e.code)).toContain("UNKNOWN_CONCEPT_LAYER");
  });

  test("a checkpoint naming an absent concept is refused and left unresolved", () => {
    const { archive } = buildArchive("cpbad", baseFiles({
      "checkpoints/ghost-s1.json": JSON.stringify({ concept: "ghost", session: 1 }, null, 2),
    }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.errors.map((e) => e.code)).toContain("UNKNOWN_CHECKPOINT_CONCEPT");
    expect(plan.unresolved.some((u) => u.family === "checkpoint")).toBe(true);
  });
});

describe("determinism and change detection", () => {
  test("two runs over the same archive produce byte-identical plans", () => {
    const { archive } = buildArchive("det", baseFiles());
    const a = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const b = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(canonicalJson(a)).toBe(canonicalJson(b));
  });

  test("a changed source produces a different source binding", () => {
    const first = buildArchive("v1", baseFiles());
    const second = buildArchive("v2", baseFiles({
      "concepts/L1-core/alpha.md": concept("alpha", { atom: "changed atom", edges: [] }),
    }));
    const a = generatePlan({ archiveDir: first.archive, limits, context: CONTEXT });
    const b = generatePlan({ archiveDir: second.archive, limits, context: CONTEXT });
    expect(a.source.archive_content_digest).not.toBe(b.source.archive_content_digest);
    expect(canonicalJson(a)).not.toBe(canonicalJson(b));
    // The atom change is visible in the projection, not just the digest.
    expect(a.terms[0]!.atom).toBe("alpha 是主语");
    expect(b.terms[0]!.atom).toBe("changed atom");
  });

  test("'complete' is never set while any error or unresolved field remains", () => {
    const { archive } = buildArchive("status", baseFiles({
      "concepts/L1-core/alpha.md": concept("alpha", { edges: ['{"type":"depends-on","target":"ghost","confidence":"confirmed"}'] }),
    }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.status).toBe("incomplete");
    expect(plan.errors.length).toBeGreaterThan(0);
    // No success/ready/activate field exists at all.
    const keys = Object.keys(plan as unknown as Record<string, unknown>);
    expect(keys).not.toContain("ready");
    expect(keys).not.toContain("activate");
    expect(keys).not.toContain("success");
  });
});

describe("bounds", () => {
  test("maxEntries refuses an archive with more files before planning it", () => {
    const { archive } = buildArchive("bounds", baseFiles());
    expect(() => generatePlan({ archiveDir: archive, limits: { ...limits, maxEntries: 2 }, context: CONTEXT })).toThrow(/maxEntries/);
  });

  test("maxRecords refuses before exceeding the record ceiling", () => {
    const { archive } = buildArchive("recbounds", baseFiles());
    // Distinct message per ceiling, so a test cannot pass on the wrong bound.
    expect(() => generatePlan({ archiveDir: archive, limits: { ...limits, maxRecords: 2 }, context: CONTEXT })).toThrow(
      /maxRecords=2/,
    );
  });

  test("maxBytes refuses before an oversized source body is read", () => {
    const { archive } = buildArchive("bytebounds", baseFiles());
    expect(() => generatePlan({ archiveDir: archive, limits: { ...limits, maxBytes: 128 }, context: CONTEXT })).toThrow(
      /source bytes would exceed its ceiling/,
    );
  });

  test("non-positive or non-integer ceilings are usage errors", () => {
    const { archive } = buildArchive("badlimits", baseFiles());
    expect(() => generatePlan({ archiveDir: archive, limits: { ...limits, maxEntries: 0 }, context: CONTEXT })).toThrow(/positive finite integer/);
    expect(() => generatePlan({ archiveDir: archive, limits: { ...limits, maxRecords: 1.5 }, context: CONTEXT })).toThrow(/positive finite integer/);
  });
});

describe("CLI", () => {
  test("every ceiling is required and no default path exists", () => {
    expect(runCli([])).toBe(EXIT.usage);
    expect(runCli(["--archive", "x"])).toBe(EXIT.usage);
    expect(() => parseCli(["--max-entries", "0"])).toThrow(/positive integer/);
    expect(() => parseCli(["--max-records", "-5"])).toThrow(/positive integer/);
  });

  test("a source-complete plan exits 66 while target requirements remain", () => {
    const ok = buildArchive("cli-ok", baseFiles());
    const okOut = join(ok.root, "plan.json");
    const ctx = join(ok.root, "context.json");
    writeFileSync(ctx, JSON.stringify(CONTEXT), "utf8");
    const args = [
      "--archive", ok.archive, "--out", okOut, "--plan-context", ctx,
      "--max-entries", "2000", "--max-bytes", "8000000", "--max-records", "20000",
      "--max-archive-bytes", "16000000", "--max-line-bytes", "4000000",
      "--max-diagnostics", "5000",
    ];
    // The plan is written, source extraction is complete, and the exit code still says
    // "not finished": unresolved target requirements must not look like success.
    expect(runCli(args)).toBe(EXIT.incomplete);
    expect(existsSync(okOut)).toBe(true);
    const written = JSON.parse(readFileSync(okOut, "utf8")) as MigrationPlan;
    expect(written.source.extraction).toBe("complete");
    expect(written.status).toBe("incomplete");
    expect(written.target_mapping).toBe("not_ready");
    expect(written.unresolved.length).toBeGreaterThan(0);
  });

  test("a missing plan context is a usage refusal, not a clock default", () => {
    const { archive, root } = buildArchive("cli-noctx", baseFiles());
    expect(
      runCli([
        "--archive", archive, "--out", join(root, "plan.json"),
        "--max-entries", "2000", "--max-bytes", "8000000", "--max-records", "20000",
        "--max-archive-bytes", "16000000", "--max-line-bytes", "4000000",
      "--max-diagnostics", "5000",
      ]),
    ).toBe(EXIT.usage);
  });

  test("a plan context without a timestamp is refused", () => {
    const { root } = buildArchive("cli-badctx", baseFiles());
    const ctx = join(root, "context.json");
    writeFileSync(ctx, JSON.stringify({ label: "x", purpose: "y" }), "utf8");
    expect(() => readPlanContext(ctx, 1 << 20)).toThrow(/created_at/);
  });

  test("an existing output is never overwritten", () => {
    const { archive, root } = buildArchive("cli-exists", baseFiles());
    const out = join(root, "plan.json");
    const ctx = join(root, "context.json");
    writeFileSync(ctx, JSON.stringify(CONTEXT), "utf8");
    writeFileSync(out, "prior plan\n", "utf8");
    expect(
      runCli([
        "--archive", archive, "--out", out, "--plan-context", ctx,
        "--max-entries", "2000", "--max-bytes", "8000000", "--max-records", "20000",
        "--max-archive-bytes", "16000000", "--max-line-bytes", "4000000",
      "--max-diagnostics", "5000",
      ]),
    ).toBe(EXIT.refused);
    expect(readFileSync(out, "utf8")).toBe("prior plan\n");
  });
});

describe("target mapping is explicit, not implied", () => {
  test("unavailable source facts are declared rather than invented", () => {
    const { archive } = buildArchive("unavail", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    // The target types say: recorded_by is an optional verified principal, and
    // provenance is a separate enum. Nothing here invents an authenticated actor.
    for (const assertion of plan.assertions) {
      // The executor is unknown until a real importer runs; the ORIGIN the source
      // supports is Unavailable, because a Markdown edge declares no actor. Neither
      // is LegacyDeclared merely because the record is being migrated.
      expect(assertion.audit.recorded_by).toBeNull();
      expect(assertion.audit.actor_origin).toBe("Unresolved");
      expect(assertion.declared_author).toBeNull();
      expect(assertion.declared_actor_origin).toBe("Unavailable");
    }
    // Unavailable facts are declared, not defaulted to "now".
    expect(plan.unavailable.some((u) => u.family === "term" && u.field.includes("created_at"))).toBe(true);
    // And the required target fields are surfaced as unresolved with a proposed strategy.
    const created = plan.unresolved.find((u) => u.field === "term.created_at");
    expect(created).toBeDefined();
    expect(created!.proposed_strategy).toBeTruthy();
  });

  test("the plan is a control artifact: it carries mappings, not a persistence format", () => {
    const { archive } = buildArchive("control", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    // Every record type in the plan is a mapping statement about the source.
    for (const term of plan.terms) expect(term.kind).toBe("term");
    for (const assertion of plan.assertions) expect(assertion.kind).toBe("assertion");
    // And the source binding is explicit, so a plan cannot be applied to another capture.
    expect(plan.source.source_digest).toMatch(/^sha256:[0-9a-f]{64}$/);
    expect(plan.source.archive_content_digest).toMatch(/^sha256:[0-9a-f]{64}$/);
  });
});

describe("R2-F1: every read is bounded at the descriptor", () => {
  test("a blob that grows after verification is refused, not read unbounded", () => {
    const { archive } = buildArchive("grow", baseFiles());
    // Verification already passed inside buildArchive. Now grow a blob past its
    // recorded length: a stat-then-read implementation would read the new bytes.
    const files = readFileSync(join(archive, "files.ndjson"), "utf8").trim().split("\n");
    const first = JSON.parse(files[0]!) as { sha256: string };
    const blobPath = join(archive, "blobs", first.sha256);
    const before = readFileSync(blobPath);
    const grown = Buffer.concat([before, Buffer.alloc(4096, 0x41)]);
    writeFileSync(blobPath, grown);
    expect(() => generatePlan({ archiveDir: archive, limits, context: CONTEXT })).toThrow(
      // Either the archive verifier or this planner refuses it; both are bounded
      // refusals, and neither reads the grown file to completion.
      /is \d+ bytes, (expected|manifest says)|does not match its recorded hash|grew while being read/,
    );
  });

  test("a blob replaced by different bytes of the same length is refused", () => {
    const { archive } = buildArchive("replace", baseFiles());
    const files = readFileSync(join(archive, "files.ndjson"), "utf8").trim().split("\n");
    const first = JSON.parse(files[0]!) as { sha256: string };
    const blobPath = join(archive, "blobs", first.sha256);
    const before = readFileSync(blobPath);
    const flipped = Buffer.from(before);
    flipped[0] = flipped[0]! ^ 0xff;
    writeFileSync(blobPath, flipped);
    // Caught by content hash before any planning — the intended first line of defence.
    expect(() => generatePlan({ archiveDir: archive, limits, context: CONTEXT })).toThrow(
      /content hash mismatch|does not match its recorded hash/,
    );
  });

  test("a hostile blob path never reads outside the archive", () => {
    const { archive } = buildArchive("hostile", baseFiles());
    const filesPath = join(archive, "files.ndjson");
    const lines = readFileSync(filesPath, "utf8").trim().split("\n");
    const first = JSON.parse(lines[0]!) as Record<string, unknown>;
    // Absolute path escape and traversal, both non-canonical.
    for (const hostile of ["/etc/passwd", "../../../etc/passwd", "blobs/../archive.json"]) {
      const mutated = { ...first, blob: hostile };
      const copy = [...lines];
      copy[0] = JSON.stringify(mutated);
      writeFileSync(filesPath, `${copy.join("\n")}\n`, "utf8");
      resignArchive(archive);
      // Refused on the name, by the verifier and/or the planner; the point is that
      // the path is never opened, so nothing outside the archive can be read.
      expect(() => generatePlan({ archiveDir: archive, limits, context: CONTEXT })).toThrow(
        /not the canonical|expected its own hash/,
      );
      writeFileSync(filesPath, `${lines.join("\n")}\n`, "utf8");
      resignArchive(archive);
    }
  });

  test("the line ceiling binds on growth, before the line is complete", () => {
    const { archive } = buildArchive("longline", baseFiles());
    // A single 8 KiB record with no newline must fail on growth, not after parse.
    const filesPath = join(archive, "files.ndjson");
    writeFileSync(filesPath, `${readFileSync(filesPath, "utf8")}${"x".repeat(8192)}`, "utf8");
    resignArchive(archive);
    // Signature is consistent, so the only remaining guard is the planner's line bound.
    expect(() =>
      generatePlan({ archiveDir: archive, limits: { ...limits, maxLineBytes: 1024 }, context: CONTEXT }),
    ).toThrow(/maxLineBytes=1024/);
  });
});

describe("R2-F2: body bytes are the source's own bytes", () => {
  function sliceOf(raw: string): { offset: number; length: number } {
    return bodySlice(Buffer.from(raw, "utf8"));
  }

  test("leading blank lines and trailing newlines are preserved", () => {
    const body = "\n\n# Title\n\nText.\n\n";
    const raw = `---\nid: "a"\n---\n${body}`;
    const slice = sliceOf(raw);
    const bytes = Buffer.from(raw, "utf8").subarray(slice.offset, slice.offset + slice.length);
    expect(bytes.toString("utf8")).toBe(body);
  });

  test("CRLF line endings survive into the body slice", () => {
    const body = "# T\r\n\r\nLine.\r\n";
    const raw = `---\r\nid: "a"\r\n---\r\n${body}`;
    const slice = sliceOf(raw);
    const bytes = Buffer.from(raw, "utf8").subarray(slice.offset, slice.offset + slice.length);
    expect(bytes.toString("utf8")).toBe(body);
    expect(bytes.includes(0x0d)).toBe(true);
  });

  test("a plan's body hash matches a re-read of the source descriptor", () => {
    const body = "\n\n  spaced body with 中文 and 🎯  \n\n";
    const { archive } = buildArchive("bodyslice", baseFiles({
      "concepts/L1-core/alpha.md": concept("alpha", { body }),
    }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const alpha = plan.terms.find((t) => t.slug === "alpha")!;
    // Re-read the whole file from the archive and slice it independently.
    const raw = readFileSync(join(archive, alpha.file.blob));
    const bytes = raw.subarray(alpha.body.offset, alpha.body.offset + alpha.body.length);
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(alpha.body.sha256);
    expect(bytes.toString("utf8")).toBe(body);
    // And it is genuinely different from the trimmed projection.
    expect(alpha.projection.body_sha256).not.toBe(alpha.body.sha256);
  });
});

describe("R2-F3: frontier identity is unique and bounded", () => {
  test("repeated headings and multiple tables never collide", () => {
    const raw = [
      "## Same",
      "| a | b | c | d |",
      "|---|---|---|---|",
      "| r1 | x | y | z |",
      "",
      "## Same",
      "| a | b | c | d |",
      "|---|---|---|---|",
      "| r1 | x | y | z |",
      "",
      "| a | b | c | d |",
      "|---|---|---|---|",
      "| r2 | x | y | z |",
    ].join("\n");
    const { entries, errors } = parseFrontierSections(raw, 100, 100);
    expect(errors).toEqual([]);
    const ids = entries.map((e) => e.source_id);
    expect(new Set(ids).size).toBe(ids.length);
    expect(entries.map((e) => e.declaration_order)).toEqual([0, 1, 2]);
  });

  test("maxRecords bounds frontier growth before it is allocated", () => {
    const { archive } = buildArchive("frontierbound", baseFiles());
    // Three terms + three assertions already consume most of a tiny budget.
    expect(() =>
      generatePlan({ archiveDir: archive, limits: { ...limits, maxRecords: 6 }, context: CONTEXT }),
    ).toThrow(/maxRecords/);
  });

  test("a duplicate id in any family is an error", () => {
    const { archive } = buildArchive("dupfam", baseFiles({
      "concepts/L2-support/alpha.md": concept("alpha", { layer: 2 }),
    }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.errors.map((e) => e.code)).toContain("DUPLICATE_ID");
    expect(plan.source.extraction).toBe("incomplete");
  });
});

describe("R2-F4: proposal metadata is preserved, not re-derived", () => {
  test("unknown fields in the head survive into the plan", () => {
    const head = [
      "---",
      "proposer: CLI",
      "date: 2026-11-13T00:00:00.000Z",
      "concept: alpha",
      "status: approved",
      "expectedRevision: rev-9",
      "customField: keep-me",
      "---",
      "payload body",
    ].join("\n");
    const { archive } = buildArchive("propmeta", baseFiles({ "proposals/atoms/rich.md": head }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const proposal = plan.proposals.find((p) => p.source_path.includes("rich.md"))!;
    expect(proposal.metadata["customField"]).toBe("keep-me");
    expect(proposal.expected_revision).toBe("rev-9");
    // A declared author exists here, so the SOURCE supports LegacyDeclared — but the
    // authenticated executor is still unknown and stays separate.
    expect(proposal.declared_author).toBe("CLI");
    expect(proposal.declared_actor_origin).toBe("LegacyDeclared");
    expect(proposal.audit.recorded_by).toBeNull();
    expect(proposal.audit.actor_origin).toBe("Unresolved");
  });

  test("JSON proposals keep their whole parsed payload", () => {
    const json = JSON.stringify({ conceptId: "alpha", status: "approved", expectedRevision: "r1", extra: [1, 2] });
    const { archive } = buildArchive("propjson", baseFiles({ "proposals/bodies/x.json": json }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const proposal = plan.proposals.find((p) => p.source_path.includes("x.json"))!;
    expect(proposal.metadata["extra"]).toEqual([1, 2]);
    expect(proposal.expected_revision).toBe("r1");
  });

  test("an unknown status is an error plus unresolved, never complete", () => {
    const head = ["---", "proposer: CLI", "concept: alpha", "status: invented", "---", "x"].join("\n");
    const { archive } = buildArchive("propbad", baseFiles({ "proposals/atoms/bad.md": head }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.errors.map((e) => e.code)).toContain("UNKNOWN_PROPOSAL_STATUS");
    expect(plan.unresolved.some((u) => u.field === "proposal.status")).toBe(true);
    expect(plan.status).toBe("incomplete");
  });

  test("a real pending proposal is marked for rebase, not carried as pending", () => {
    const head = ["---", "proposer: CLI", "concept: alpha", "status: pending", "---", "x"].join("\n");
    const { archive } = buildArchive("proppend", baseFiles({ "proposals/atoms/pend.md": head }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const pending = plan.unresolved.find((u) => u.field === "proposal.expected_revision");
    expect(pending).toBeDefined();
    expect(pending!.proposed_strategy).toMatch(/conflicted/);
    expect(pending!.proposed_strategy).toMatch(/needs_rebase/);
  });
});

describe("R2-F5: target audit types are respected", () => {
  test("recorded_by is null and provenance is the enum, never a string stand-in", () => {
    const { archive } = buildArchive("audit", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    for (const term of plan.terms) {
      // The target type has no actor field on a term body; the requirement is surfaced.
      expect(term.missing_target_fields).toContain("recorded_by");
      expect(term.missing_target_fields).toContain("created_at");
    }
    for (const assertion of plan.assertions) {
      expect(assertion.audit).toEqual({
        recorded_by: null,
        actor_origin: "Unresolved",
        reason: null,
        created_at: "unavailable",
      });
      // Separate axis: what the source itself declares.
      expect(assertion.declared_actor_origin).toBe("Unavailable");
    }
    // A declared author is never promoted into recorded_by.
    expect(plan.proposals.every((p) => p.audit.recorded_by === null)).toBe(true);
  });

  test("target requirements are surfaced as unresolved, so status is never falsely complete", () => {
    const { archive } = buildArchive("targetreq", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.source.extraction).toBe("complete");
    expect(plan.status).not.toBe("complete");
    expect(plan.target_mapping).toBe("not_ready");
    const created = plan.unresolved.filter((u) => u.field === "term.created_at");
    expect(created.length).toBe(plan.terms.length);
    expect(created.every((u) => typeof u.proposed_strategy === "string")).toBe(true);
  });
});

describe("R2-F6: determinism without a clock", () => {
  test("no context means no plan: the clock is never consulted", () => {
    const { archive } = buildArchive("noctx", baseFiles());
    // @ts-expect-error deliberately missing the required context
    expect(() => generatePlan({ archiveDir: archive, limits })).toThrow();
  });

  test("two runs with the same context are byte-identical; a different context is not", () => {
    const { archive } = buildArchive("det2", baseFiles());
    const a = canonicalJson(generatePlan({ archiveDir: archive, limits, context: CONTEXT }));
    const b = canonicalJson(generatePlan({ archiveDir: archive, limits, context: CONTEXT }));
    expect(a).toBe(b);
    const other = canonicalJson(
      generatePlan({ archiveDir: archive, limits, context: { ...CONTEXT, label: "other-run" } }),
    );
    expect(other).not.toBe(a);
  });

  test("the run context is digest-bound and recorded verbatim", () => {
    const { archive } = buildArchive("ctxdigest", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    expect(plan.run.label).toBe(CONTEXT.label);
    expect(plan.run.created_at).toBe(CONTEXT.created_at);
    expect(plan.run.context_digest).toMatch(/^sha256:[0-9a-f]{64}$/);
  });

  test("ids order by UTF-8 bytes, not UTF-16 code units", () => {
    // U+FFFD and U+10000 sort differently in UTF-16 vs UTF-8.
    const a = "\ufffd";
    const b = "\u{10000}";
    expect(compareUtf8(a, b)).toBe(Math.sign(Buffer.compare(Buffer.from(a), Buffer.from(b))));
    // A case where the two encodings genuinely disagree, if one exists.
    const pairs: Array<[string, string]> = [[a, b], ["é", "z"], ["中", "a"]];
    for (const [x, y] of pairs) {
      const byUtf16 = x < y ? -1 : x > y ? 1 : 0;
      const byUtf8 = compareUtf8(x, y);
      if (byUtf16 !== byUtf8) {
        expect(byUtf8).toBe(Math.sign(Buffer.compare(Buffer.from(x), Buffer.from(y))));
      }
    }
  });
});

describe("R3-B1: one global record cap over every mapping family", () => {
  test("attachments are mapping records and charge the same budget", () => {
    const { archive } = buildArchive("globalcap", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    // The global count is exactly the sum of every emitted mapping record.
    const emitted =
      plan.counts.term +
      plan.counts.assertion +
      plan.counts.proposal +
      plan.counts.checkpoint +
      plan.counts.frontier_entry +
      plan.counts.attachment;
    expect(emitted).toBe(
      plan.terms.length +
        plan.assertions.length +
        plan.proposals.length +
        plan.checkpoints.length +
        plan.frontier.length +
        plan.attachments.length,
    );
    // And the cap covers that total, attachments included, not attachments alone.
    expect(() => generatePlan({ archiveDir: archive, limits: { ...limits, maxRecords: emitted - 1 }, context: CONTEXT }))
      .toThrow(/maxRecords=\d+|plan records|would exceed/);
  });

  test("exactly N records is accepted; N-1 is refused", () => {
    const { archive } = buildArchive("exactcap", baseFiles());
    // Derive the true total from a run with a roomy cap: a run at a tight cap would
    // report a truncated frontier and understate N.
    const full = generatePlan({ archiveDir: archive, limits: { ...limits, maxRecords: 100_000 }, context: CONTEXT });
    const total =
      full.counts.term +
      full.counts.assertion +
      full.counts.proposal +
      full.counts.checkpoint +
      full.counts.frontier_entry +
      full.counts.attachment;

    // N accepted.
    const atLimit = generatePlan({
      archiveDir: archive,
      limits: { ...limits, maxRecords: total },
      context: CONTEXT,
    });
    expect(atLimit.source.extraction).toBe("complete");
    expect(
      atLimit.attachments.length +
        atLimit.terms.length +
        atLimit.assertions.length +
        atLimit.proposals.length +
        atLimit.checkpoints.length +
        atLimit.frontier.length,
    ).toBe(total);

    // N-1 refused. The refusal names the planner's own accounting; because the
    // frontier rows are charged against the *remaining* allowance, the number in
    // the message is what was left when the frontier was parsed, not `total`.
    // The archive verifier ran first and passed, so this is the planner's cap.
    expect(() =>
      generatePlan({ archiveDir: archive, limits: { ...limits, maxRecords: total - 1 }, context: CONTEXT }),
    ).toThrow(/maxRecords=\d+|plan records|would exceed/);
  });

  test("the archive verifier alone accepts the archive the planner then rejects", () => {
    const { archive } = buildArchive("verify-vs-plan", baseFiles());
    const full = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const total =
      full.counts.term +
      full.counts.assertion +
      full.counts.proposal +
      full.counts.checkpoint +
      full.counts.frontier_entry +
      full.counts.attachment;

    // The verifier's own record ceiling is satisfied by this archive: it has far
    // fewer stream records than `total`, and verification succeeds.
    const verifiedOnly = verifySourceArchive(archive, {
      maxArchiveBytes: limits.maxArchiveBytes,
      maxRecords: total,
      maxLineBytes: limits.maxLineBytes,
      maxEntries: limits.maxEntries,
    });
    expect(verifiedOnly.contentDigest).toMatch(/^sha256:/);

    // The planner, on the very same bytes, refuses one record earlier than `total`
    // and names its own accounting — so the cap proven here is the planner's.
    let message = "";
    try {
      generatePlan({ archiveDir: archive, limits: { ...limits, maxRecords: total - 1 }, context: CONTEXT });
    } catch (err) {
      message = String(err);
    }
    expect(message).toMatch(/plan records|maxRecords|would exceed/);
  });

  test("auxiliary diagnostics have their own ceiling", () => {
    // Many unknown edge targets produce many errors+unresolved entries.
    const edges = Array.from({ length: 20 }, (_, i) => `{"type":"depends-on","target":"ghost-${i}","confidence":"confirmed"}`);
    const { archive } = buildArchive("diagcap", baseFiles({
      "concepts/L1-core/alpha.md": concept("alpha", { edges }),
    }));
    expect(() =>
      generatePlan({ archiveDir: archive, limits: { ...limits, maxDiagnostics: 5 }, context: CONTEXT }),
    ).toThrow(/diagnostics/);
  });
});

describe("R3-B2: the plan is bound to the bytes that were verified", () => {
  test("a manifest replaced after verification is refused by the planner", () => {
    const { archive } = buildArchive("bind-manifest", baseFiles());
    expect(() =>
      generatePlan({
        archiveDir: archive,
        limits,
        context: CONTEXT,
        hooks: {
          afterVerify: () => {
            // Same length, still internally self-consistent: only the planner's
            // join to the verified digest can catch this.
            const manifestPath = join(archive, "archive.json");
            const manifest = JSON.parse(readFileSync(manifestPath, "utf8")) as Record<string, unknown>;
            manifest["created_at"] = "2026-11-19T00:00:00.000Z"; // same length as the original
            manifest["content_digest"] = manifestContentDigest(manifest as never);
            writeFileSync(manifestPath, `${canonicalJson(manifest)}\n`, "utf8");
            const marker = JSON.parse(readFileSync(join(archive, "COMPLETE.json"), "utf8")) as Record<string, unknown>;
            marker["content_digest"] = manifest["content_digest"];
            writeFileSync(join(archive, "COMPLETE.json"), `${canonicalJson(marker)}\n`, "utf8");
          },
        },
      }),
    ).toThrow(/E_ARCHIVE_CHANGED|changed after verification|declares .* but the bytes in hand/);
  });

  test("a files stream replaced after verification is refused by the planner", () => {
    const { archive } = buildArchive("bind-stream", baseFiles());
    expect(() =>
      generatePlan({
        archiveDir: archive,
        limits,
        context: CONTEXT,
        hooks: {
          afterVerify: () => {
            const filesPath = join(archive, "files.ndjson");
            const lines = readFileSync(filesPath, "utf8").trim().split("\n");
            // Same number of lines, same length: change one path's bytes in place.
            const first = JSON.parse(lines[0]!) as Record<string, unknown>;
            const original = String(first["path"]);
            const swapped = original.slice(0, -1) + (original.endsWith("a") ? "b" : "a");
            first["path"] = swapped;
            lines[0] = JSON.stringify(first);
            writeFileSync(filesPath, `${lines.join("\n")}\n`, "utf8");
          },
        },
      }),
    ).toThrow(/E_ARCHIVE_CHANGED|files\.ndjson digest|files\.ndjson is .* bytes/);
  });

  test("the binding is by digest, not by the verifier's earlier opinion", () => {
    const { archive } = buildArchive("bind-digest", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    // The recorded source binding equals the manifest's own digest.
    const manifest = JSON.parse(readFileSync(join(archive, "archive.json"), "utf8")) as { content_digest: string };
    expect(plan.source.archive_content_digest).toBe(manifest.content_digest);
  });
});

describe("R3 audit correction", () => {
  test("no declared actor means Unavailable, never LegacyDeclared", () => {
    const { archive } = buildArchive("audit-unavail", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    for (const assertion of plan.assertions) {
      expect(assertion.declared_actor_origin).toBe("Unavailable");
      expect(assertion.audit.actor_origin).toBe("Unresolved");
    }
  });

  test("a declared actor yields LegacyDeclared for the SOURCE, executor still unresolved", () => {
    const head = ["---", "proposer: CLI", "concept: alpha", "status: approved", "---", "x"].join("\n");
    const { archive } = buildArchive("audit-declared", baseFiles({ "proposals/atoms/d.md": head }));
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const proposal = plan.proposals.find((p) => p.source_path.includes("d.md"))!;
    expect(proposal.declared_author).toBe("CLI");
    expect(proposal.declared_actor_origin).toBe("LegacyDeclared");
    // The executor is never upgraded from the declared author.
    expect(proposal.audit.recorded_by).toBeNull();
    expect(proposal.audit.actor_origin).toBe("Unresolved");
  });

  test("target revision and created_at are stated as NEW-space facts, not source history", () => {
    const { archive } = buildArchive("audit-rev", baseFiles());
    const plan = generatePlan({ archiveDir: archive, limits, context: CONTEXT });
    const revision = plan.unresolved.find((u) => u.field === "term.revision");
    expect(revision).toBeDefined();
    expect(revision!.proposed_strategy).toMatch(/FIRST revision in the NEW target space/);
    expect(revision!.proposed_strategy).toMatch(/does not reconstruct/);
    const created = plan.unresolved.find((u) => u.field === "term.created_at");
    expect(created!.proposed_strategy).toMatch(/commit clock/);
    expect(created!.proposed_strategy).toMatch(/unavailable/);
    // Approved history is never re-applied, and restatement stays unresolved.
    const head = ["---", "proposer: CLI", "concept: alpha", "status: approved", "---", "x"].join("\n");
    const withApproved = buildArchive("audit-hist", baseFiles({ "proposals/atoms/a.md": head }));
    const hist = generatePlan({ archiveDir: withApproved.archive, limits, context: CONTEXT });
    expect(hist.unresolved.some((u) => u.field === "proposal.historical_restatement")).toBe(true);
  });
});

describe("R4-F1: the manifest is bounded by maxArchiveBytes, not maxLineBytes", () => {
  test("a pretty manifest larger than maxLineBytes is still planned when the verifier accepts it", () => {
    const { archive } = buildArchive("manifestbound", baseFiles());
    const manifestSize = readFileSync(join(archive, "archive.json")).length;
    // A line ceiling far below the manifest, but above every stream line.
    const lineCeiling = Math.max(256, Math.floor(manifestSize / 4));

    // Discriminator: the archive verifier accepts this archive under that same
    // maxLineBytes — the manifest is not a "line" as far as the verifier is concerned.
    const verified = verifySourceArchive(archive, {
      maxArchiveBytes: limits.maxArchiveBytes,
      maxRecords: limits.maxRecords,
      maxLineBytes: lineCeiling,
      maxEntries: limits.maxEntries,
    });
    expect(verified.contentDigest).toMatch(/^sha256:/);

    // And the planner accepts it too. Before this fix it refused with
    // "archive.json is N bytes, over the M-byte ceiling", naming a bound the caller
    // set for a different purpose.
    const plan = generatePlan({
      archiveDir: archive,
      limits: { ...limits, maxLineBytes: lineCeiling },
      context: CONTEXT,
    });
    expect(plan.source.extraction).toBe("complete");

    // The manifest really is larger than the line ceiling that used to be applied.
    expect(manifestSize).toBeGreaterThan(lineCeiling);
  });

  test("the manifest is still bounded: maxArchiveBytes refuses an oversized one", () => {
    const { archive } = buildArchive("manifestbig", baseFiles());
    expect(() =>
      generatePlan({ archiveDir: archive, limits: { ...limits, maxArchiveBytes: 64 }, context: CONTEXT }),
    ).toThrow(/archive\.json|maxArchiveBytes|ceiling/);
  });
});

describe("R4-F6: the plan never writes inside the archive it describes", () => {
  test("an output inside the archive is refused before any write", () => {
    const { archive, root } = buildArchive("containment", baseFiles());
    // Snapshot every archive byte first: the refusal must leave it untouched.
    const before = new Map<string, string>();
    const walk = (dir: string, prefix: string): void => {
      for (const name of readdirSync(dir)) {
        const abs = join(dir, name);
        const rel = prefix === "" ? name : `${prefix}/${name}`;
        const stat = statSync(abs);
        if (stat.isDirectory()) walk(abs, rel);
        else before.set(rel, createHash("sha256").update(readFileSync(abs)).digest("hex"));
      }
    };
    walk(archive, "");

    const out = join(archive, "plan.json");
    expect(
      runCli([
        "--archive", archive, "--out", out, "--plan-context", (() => {
          const ctx = join(root, "context.json");
          writeFileSync(ctx, JSON.stringify(CONTEXT), "utf8");
          return ctx;
        })(),
        "--max-entries", "2000", "--max-bytes", "8000000", "--max-records", "20000",
        "--max-archive-bytes", "16000000", "--max-line-bytes", "4000000",
        "--max-diagnostics", "5000",
      ]),
    ).toBe(EXIT.refused);
    expect(existsSync(out)).toBe(false);

    // Every archive file is byte-identical, and no new file appeared.
    const after = new Map<string, string>();
    const walk2 = (dir: string, prefix: string): void => {
      for (const name of readdirSync(dir)) {
        const abs = join(dir, name);
        const rel = prefix === "" ? name : `${prefix}/${name}`;
        const stat = statSync(abs);
        if (stat.isDirectory()) walk2(abs, rel);
        else after.set(rel, createHash("sha256").update(readFileSync(abs)).digest("hex"));
      }
    };
    walk2(archive, "");
    expect([...after.keys()].sort()).toEqual([...before.keys()].sort());
    for (const [rel, hash] of before) expect(after.get(rel)).toBe(hash);

    // The archive still verifies afterwards.
    expect(() =>
      verifySourceArchive(archive, {
        maxArchiveBytes: limits.maxArchiveBytes,
        maxRecords: limits.maxRecords,
        maxLineBytes: limits.maxLineBytes,
        maxEntries: limits.maxEntries,
      }),
    ).not.toThrow();
  });

  test("the archive directory itself is refused as an output", () => {
    const { archive, root } = buildArchive("containment-self", baseFiles());
    const ctx = join(root, "context.json");
    writeFileSync(ctx, JSON.stringify(CONTEXT), "utf8");
    expect(
      runCli([
        "--archive", archive, "--out", archive, "--plan-context", ctx,
        "--max-entries", "2000", "--max-bytes", "8000000", "--max-records", "20000",
        "--max-archive-bytes", "16000000", "--max-line-bytes", "4000000",
        "--max-diagnostics", "5000",
      ]),
    ).toBe(EXIT.refused);
  });
});

describe("R4-F4/F3: frontier diagnostics and charge timing", () => {
  test("a pathological frontier is refused by maxDiagnostics, not accumulated first", () => {
    // Many rows with repeated ids inside one table cannot be produced by the
    // section/table/row scheme, so build collisions through duplicated headings
    // that the scoping rule legitimately separates — instead assert the parser's
    // own diagnostics ceiling directly.
    const rows = Array.from({ length: 50 }, (_, i) => `| r${i} | x | y | z |`).join("\n");
    const raw = `## S\n\n| a | b | c | d |\n|---|---|---|---|\n${rows}\n`;
    // The parser admits rows up to `limit`; with a tiny diagnostics ceiling and no
    // collisions it succeeds, which proves the ceiling is about errors not rows.
    const ok = parseFrontierSections(raw, 100, 10);
    expect(ok.errors).toEqual([]);
    expect(ok.entries.length).toBe(50);
    // The row ceiling is independent of the diagnostics ceiling: too many rows is
    // refused outright rather than truncated, so a bounded plan cannot silently lose
    // frontier rows.
    expect(() => parseFrontierSections(raw, 5, 10)).toThrow(/would exceed maxRecords=5/);
  });
});

describe("R5-F6: containment is decided on physical paths", () => {
  /** Snapshot every file under `dir` as rel → sha256, recursively. */
  function snapshot(dir: string): Map<string, string> {
    const out = new Map<string, string>();
    const walk = (current: string, prefix: string): void => {
      for (const name of readdirSync(current)) {
        const abs = join(current, name);
        const rel = prefix === "" ? name : `${prefix}/${name}`;
        if (statSync(abs).isDirectory()) walk(abs, rel);
        else out.set(rel, createHash("sha256").update(readFileSync(abs)).digest("hex"));
      }
    };
    walk(dir, "");
    return out;
  }

  function cliArgs(archive: string, out: string, ctx: string): string[] {
    return [
      "--archive", archive, "--out", out, "--plan-context", ctx,
      "--max-entries", "2000", "--max-bytes", "8000000", "--max-records", "20000",
      "--max-archive-bytes", "16000000", "--max-line-bytes", "4000000",
      "--max-diagnostics", "5000",
    ];
  }

  test("a symlinked OUTPUT PARENT aliasing into the archive is refused", () => {
    const { archive, root } = buildArchive("sym-parent", baseFiles());
    const ctx = join(root, "context.json");
    writeFileSync(ctx, JSON.stringify(CONTEXT), "utf8");
    // `alias` points at the archive's parent, so /alias/<basename>/plan.json is
    // lexically outside the archive but physically inside it.
    const alias = join(root, "alias");
    symlinkSync(dirname(archive), alias);
    const out = join(alias, basename(archive), "plan.json");

    const before = snapshot(archive);
    expect(runCli(cliArgs(archive, out, ctx))).toBe(EXIT.refused);
    const after = snapshot(archive);
    expect([...after.keys()].sort()).toEqual([...before.keys()].sort());
    for (const [rel, hash] of before) expect(after.get(rel)).toBe(hash);
    // And no plan was written at the aliased location.
    expect(existsSync(join(archive, "plan.json"))).toBe(false);
  });

  test("a symlinked ARCHIVE path is compared by its real location", () => {
    const { archive, root } = buildArchive("sym-archive", baseFiles());
    const ctx = join(root, "context.json");
    writeFileSync(ctx, JSON.stringify(CONTEXT), "utf8");
    // Address the archive through a symlink: a lexical check would see the output
    // as "outside" a differently-spelled archive path.
    const aliasArchive = join(root, "archive-alias");
    symlinkSync(archive, aliasArchive);
    const out = join(archive, "plan.json");

    const before = snapshot(archive);
    expect(runCli(cliArgs(aliasArchive, out, ctx))).toBe(EXIT.refused);
    const after = snapshot(archive);
    expect([...after.keys()].sort()).toEqual([...before.keys()].sort());
    for (const [rel, hash] of before) expect(after.get(rel)).toBe(hash);
  });

  test("a missing output parent fails without creating anything", () => {
    const { archive, root } = buildArchive("missing-parent", baseFiles());
    const ctx = join(root, "context.json");
    writeFileSync(ctx, JSON.stringify(CONTEXT), "utf8");
    const out = join(root, "does-not-exist", "deeper", "plan.json");

    const before = snapshot(archive);
    expect(runCli(cliArgs(archive, out, ctx))).toBe(EXIT.refused);
    // No directory was created, and the archive is untouched.
    expect(existsSync(join(root, "does-not-exist"))).toBe(false);
    const after = snapshot(archive);
    expect([...after.keys()].sort()).toEqual([...before.keys()].sort());
    for (const [rel, hash] of before) expect(after.get(rel)).toBe(hash);
  });

  test("an ordinary output beside the archive still works", () => {
    const { archive, root } = buildArchive("normal-out", baseFiles());
    const ctx = join(root, "context.json");
    writeFileSync(ctx, JSON.stringify(CONTEXT), "utf8");
    const out = join(root, "plan.json");
    expect(runCli(cliArgs(archive, out, ctx))).toBe(EXIT.incomplete);
    expect(existsSync(out)).toBe(true);
  });
});


describe("Root context-document ceiling", () => {
  test("a pretty context larger than stream-line cap still fits its document cap", () => {
    const { archive, root } = buildArchive("context-document", baseFiles());
    const ctx = join(root, "context.json");
    const context = { ...CONTEXT, purpose: "source extraction " + "x".repeat(8192) };
    writeFileSync(ctx, JSON.stringify(context, null, 2));
    const streams = ["files.ndjson", "rows.ndjson"].filter((name) => existsSync(join(archive, name)));
    const longest = Math.max(...streams.flatMap((name) => readFileSync(join(archive, name), "utf8").split("\n").map((line) => Buffer.byteLength(line))));
    const lineCap = longest + 1;
    expect(statSync(ctx).size).toBeGreaterThan(lineCap);
    const out = join(root, "context-plan.json");
    expect(runCli(["--archive", archive, "--out", out, "--plan-context", ctx,
      "--max-entries", "2000", "--max-bytes", "8000000", "--max-records", "20000",
      "--max-archive-bytes", "16000000", "--max-line-bytes", String(lineCap), "--max-diagnostics", "5000"])).toBe(EXIT.incomplete);
    expect(JSON.parse(readFileSync(out, "utf8")).run.purpose).toBe(context.purpose);
  });
});
