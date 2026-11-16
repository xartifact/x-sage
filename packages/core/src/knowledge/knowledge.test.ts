import { describe, expect, test } from "bun:test";
import {
  bm25,
  buildKnowledgeDocument,
  claimRevision,
  conceptRecordRevision,
  cosine,
  graphFromRecords,
  hasCjk,
  manifestFromRecords,
  parseKnowledgeDocument,
  projectionRevision,
  reciprocalRankFusion,
  termFrequencies,
  tokenize,
  truncate,
} from "./index.ts";
import type { ConceptRecord, RelationRecord } from "./index.ts";

/**
 * Pure-function tests for the structured knowledge layer.
 *
 * No database and no filesystem: these assert the properties the store and the
 * API depend on — deterministic tokenisation, dimension-safe cosine, stable
 * fusion, and a Markdown projection that round-trips a body byte for byte.
 */

const baseRecord = {
  id: "decision",
  atom: "决策是在张力与约束下的不可逆选择。",
  layer: 1 as const,
  domain: ["product"],
  confidence: "confirmed" as const,
  source: "synthesis" as const,
  triggers: ["决策", "decision"],
  body: "# 决策\n\n正文第一段。\n",
  sourcePath: "concepts/L1-core/decision.md",
};

describe("tokenize", () => {
  test("splits CJK into bigrams so a query word matches a longer sentence", () => {
    // A word-splitter leaves 结构化知识 as one token; 「知识」 would never match.
    expect(tokenize("结构化知识")).toContain("知识");
    expect(tokenize("结构化知识")).toContain("结构");
  });

  test("keeps single CJK characters and drops stopword bigrams", () => {
    expect(tokenize("值")).toEqual(["值"]);
    // 的 in 的的 is a stopword bigram only as an exact pair; 「我的」 is not.
    expect(tokenize("a of the")).toEqual([]);
  });

  test("lowercases latin tokens and drops one-character words", () => {
    expect(tokenize("Vector STORE model")).toEqual(["vector", "store", "model"]);
  });

  test("is deterministic", () => {
    expect(tokenize("架构审查 evidence")).toEqual(tokenize("架构审查 evidence"));
  });

  test("detects CJK for search-hint purposes", () => {
    expect(hasCjk("架构")).toBe(true);
    expect(hasCjk("architecture")).toBe(false);
  });
});

describe("bm25", () => {
  const documents = [
    { id: "a", text: "架构审查需要收集前提和冲突证据" },
    { id: "b", text: "架构审查的输出是决策记录" },
    { id: "c", text: "向量检索是语义关联，不是断言关系" },
  ];

  test("ranks the document that contains the rare query term first", () => {
    const scores = bm25(documents, "冲突证据");
    expect(scores[0]?.id).toBe("a");
    expect(scores[0]?.matched.length).toBeGreaterThan(0);
  });

  test("returns nothing for a query with no indexed terms", () => {
    expect(bm25(documents, "")).toEqual([]);
    expect(bm25([], "架构")).toEqual([]);
  });

  test("reports matched terms with frequencies as the match basis", () => {
    const [first] = bm25(documents, "审查");
    expect(first?.matched.every((entry) => entry.tf >= 1)).toBe(true);
  });

  test("honours a limit", () => {
    expect(bm25(documents, "架构审查", { limit: 1 })).toHaveLength(1);
  });

  test("term frequencies are sorted by term", () => {
    const terms = termFrequencies("b b a").map((entry) => entry.term);
    expect(terms).toEqual([...terms].sort());
  });
});

describe("cosine", () => {
  test("is 1 for parallel vectors and 0 for orthogonal ones", () => {
    expect(cosine([1, 0, 0], [2, 0, 0])).toBeCloseTo(1);
    expect(cosine([1, 0, 0], [0, 1, 0])).toBeCloseTo(0);
  });

  test("refuses a dimension mismatch instead of padding", () => {
    // Padding would silently compare a 768-dim embedding against a 1536-dim one.
    expect(() => cosine([1, 2], [1, 2, 3])).toThrow(/dimension mismatch/i);
  });

  test("returns 0 for a zero vector rather than NaN", () => {
    expect(cosine([0, 0], [1, 1])).toBe(0);
  });

  test("rejects non-finite components", () => {
    expect(() => cosine([Number.NaN], [1])).toThrow(/non-finite/i);
  });
});

describe("reciprocalRankFusion", () => {
  test("fuses by rank, so incomparable scores never get added", () => {
    const fused = reciprocalRankFusion([
      { name: "lexical", entries: [{ id: "a", score: 12.7 }, { id: "b", score: 1.2 }] },
      { name: "vector", entries: [{ id: "b", score: 0.83 }, { id: "a", score: 0.4 }] },
    ]);
    // a is 1st lexical / 2nd vector, b is 2nd / 1st: equal fused score, id order wins.
    expect(fused.map((entry) => entry.id)).toEqual(["a", "b"]);
    expect(fused[0]?.ranks).toEqual({ lexical: 1, vector: 2 });
  });

  test("a document found by both axes outranks one found by a single axis", () => {
    const fused = reciprocalRankFusion([
      { name: "lexical", entries: [{ id: "both", score: 5 }, { id: "lex-only", score: 4 }] },
      { name: "vector", entries: [{ id: "both", score: 0.9 }] },
    ]);
    expect(fused[0]?.id).toBe("both");
  });

  test("is stable for identical scores", () => {
    const once = reciprocalRankFusion([{ name: "x", entries: [{ id: "b", score: 1 }, { id: "a", score: 1 }] }]);
    const twice = reciprocalRankFusion([{ name: "x", entries: [{ id: "b", score: 1 }, { id: "a", score: 1 }] }]);
    expect(once).toEqual(twice);
  });
});

describe("markdown projection", () => {
  const claim = {
    id: "clm_aaaaaa",
    kind: "premise" as const,
    confidence: "confirmed" as const,
    text: "架构审查必须先声明前提 :: 包括约束",
  };
  const evidence = {
    id: "evd_bbbbbb",
    kind: "text" as const,
    title: "评审会议记录",
    locator: "docs/review.md",
  };

  test("round-trips frontmatter, body and claims", () => {
    const document = buildKnowledgeDocument({
      frontmatter: {
        id: "decision",
        atom: baseRecord.atom,
        layer: 1,
        domain: ["product"],
        confidence: "confirmed",
        source: "synthesis",
        triggers: ["决策"],
        edges: [],
      },
      body: baseRecord.body,
      claims: [claim],
      evidence: [evidence],
    });

    const parsed = parseKnowledgeDocument(document);
    expect(parsed.frontmatter.id).toBe("decision");
    expect(parsed.body).toBe(baseRecord.body);
    expect(parsed.claims).toHaveLength(1);
    // The text may itself contain the separator; it is reassembled, not split off.
    expect(parsed.claims[0]?.text).toBe("架构审查必须先声明前提 :: 包括约束");
    expect(parsed.evidence).toEqual([evidence]);
    expect(parsed.hasClaimsBlock).toBe(true);
  });

  test("a plain concept file imports with no claims", () => {
    const parsed = parseKnowledgeDocument(`---\nid: risk\natom: 风险\nlayer: 1\ndomain: [product]\nconfidence: confirmed\nsource: personal\ntriggers: [风险]\n---\n\n# 风险\n`);
    expect(parsed.hasClaimsBlock).toBe(false);
    expect(parsed.claims).toEqual([]);
    expect(parsed.body).toBe("# 风险\n");
  });

  test("refuses an unterminated managed block", () => {
    expect(() =>
      parseKnowledgeDocument(`---\nid: risk\natom: 风险\nlayer: 1\ndomain: [product]\nconfidence: confirmed\nsource: personal\ntriggers: [风险]\n---\n<!-- x-sage:claims:begin -->\n- clm_aaaaaa :: premise :: confirmed :: x\n`),
    ).toThrow(/unterminated claims block/);
  });

  test("projection revision changes with the document", () => {
    const a = projectionRevision("one");
    expect(a).toMatch(/^[a-f0-9]{64}$/);
    expect(projectionRevision("two")).not.toBe(a);
  });
});

describe("record derivations", () => {
  const concept: ConceptRecord = {
    ...baseRecord,
    revision: "a".repeat(64),
    createdAt: "2026-11-16T00:00:00.000Z",
    updatedAt: "2026-11-16T00:00:00.000Z",
  };
  const relations: RelationRecord[] = [
    { from: "decision", type: "depends-on", target: "goal", confidence: "confirmed", createdAt: "2026-11-16T00:00:00.000Z" },
  ];

  test("builds the asserted graph the pure algorithms already consume", () => {
    const graph = graphFromRecords([concept], relations);
    expect(graph.nodes.map((node) => node.id)).toEqual(["decision"]);
    expect(graph.nodes[0]?.filePath).toBe("concepts/L1-core/decision.md");
    expect(graph.edges).toEqual([
      { from: "decision", type: "depends-on", target: "goal", confidence: "confirmed" },
    ]);
  });

  test("manifest rows are sorted by layer then id", () => {
    const rows = manifestFromRecords([
      { ...concept, id: "risk", layer: 2 },
      { ...concept, id: "agent", layer: 0 },
      { ...concept, id: "decision", layer: 1 },
    ]);
    expect(rows.map((row) => row.id)).toEqual(["agent", "decision", "risk"]);
  });

  test("revision ignores timestamps and tracks content", () => {
    const first = conceptRecordRevision(baseRecord);
    expect(first).toMatch(/^[a-f0-9]{64}$/);
    expect(conceptRecordRevision({ ...baseRecord, body: "# 改过\n" })).not.toBe(first);
    // A relation change is not a concept-content change: edges live elsewhere.
    expect(conceptRecordRevision(baseRecord)).toBe(first);
  });

  test("claim revision is content-addressed", () => {
    expect(claimRevision("a", "premise", "confirmed")).toBe(claimRevision("a", "premise", "confirmed"));
    expect(claimRevision("a", "premise", "confirmed")).not.toBe(claimRevision("b", "premise", "confirmed"));
  });
});

describe("truncate", () => {
  test("returns short text unchanged and marks a cut", () => {
    expect(truncate("short", 50)).toBe("short");
    expect(truncate("a".repeat(60), 10).endsWith("…")).toBe(true);
  });
});

describe("projection is a lossless interchange format", () => {
  const frontmatter = {
    id: "decision",
    atom: "决策",
    layer: 1 as const,
    domain: ["product"],
    confidence: "confirmed" as const,
    source: "synthesis" as const,
    triggers: ["决策"],
    edges: [],
  };

  test("claim text keeps newlines and the separator", () => {
    const text = "第一行\n第二行 :: 带 :: 多个分隔符";
    const document = buildKnowledgeDocument({
      frontmatter,
      body: "# 决策\n",
      claims: [{ id: "clm_aaaaaa", kind: "premise", confidence: "confirmed", text }],
      evidence: [],
    });
    // One claim is one line: a raw newline would split it into two entries.
    expect(document.split("\n").filter((line) => line.startsWith("- clm_"))).toHaveLength(1);
    expect(parseKnowledgeDocument(document).claims[0]?.text).toBe(text);
  });

  test("a literal backslash-n is not decoded as a newline", () => {
    const text = "字面量 \\n 不是换行";
    const document = buildKnowledgeDocument({
      frontmatter,
      body: "# 决策\n",
      claims: [{ id: "clm_aaaaaa", kind: "premise", confidence: "confirmed", text }],
      evidence: [],
    });
    expect(parseKnowledgeDocument(document).claims[0]?.text).toBe(text);
  });

  test("an evidence title containing the separator round-trips", () => {
    const evidence = {
      id: "evd_bbbbbb",
      kind: "text" as const,
      title: "ADR :: 001",
      locator: "docs/adr/001.md",
    };
    const document = buildKnowledgeDocument({
      frontmatter,
      body: "# 决策\n",
      claims: [],
      evidence: [evidence],
    });
    expect(parseKnowledgeDocument(document).evidence).toEqual([evidence]);
  });

  test("a body carrying a managed-block sentinel is refused, not silently split", () => {
    expect(() =>
      buildKnowledgeDocument({
        frontmatter,
        body: "# 决策\n\n<!-- x-sage:claims:begin -->\n嵌入的示例\n",
        claims: [],
        evidence: [],
      }),
    ).toThrow(/managed-block marker/);
  });
});
