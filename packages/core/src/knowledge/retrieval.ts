/**
 * Retrieval mathematics.
 *
 * Pure functions only: no database, no filesystem, no clock. The store hands in
 * rows and gets back scores, which is what makes the hybrid ranking testable
 * with fixed vectors instead of a live embedding service.
 *
 * Two axes are kept separate on purpose. Lexical (BM25) and vector (cosine)
 * scores are not comparable numbers — BM25 is unbounded, cosine is in [-1, 1] —
 * so they are fused by rank (reciprocal rank fusion), and the raw score of each
 * axis travels in the hit's `signals` so a consumer can see why something
 * matched instead of trusting one opaque number.
 */

/** Max characters of one document that contribute to the index. */
export const MAX_INDEX_CHARS = 20_000;

/** BM25 parameters; the textbook defaults, fixed so scores are reproducible. */
const K1 = 1.2;
const B = 0.75;

/** RRF damping constant. 60 is the value from the original paper. */
const RRF_K = 60;

/** Static stopword table: a literal lookup, not a mutable collection. */
const STOPWORDS: Record<string, true> = Object.fromEntries(
  [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "to", "was", "were", "with", "this", "these", "those", "we", "you",
    "的", "了", "和", "是", "在", "有", "与", "及", "或", "对", "为", "等", "中", "上", "下", "就",
  ].map((word) => [word, true as const]),
);


const CJK = /[\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff]/;
const LATIN = /[a-z0-9]+(?:[._-][a-z0-9]+)*/g;

/** A token and how often it occurred in the document. */
export interface TermFrequency {
  term: string;
  tf: number;
}

/**
 * Tokenise text into index terms.
 *
 * CJK has no spaces, so a word-oriented splitter turns a whole Chinese sentence
 * into one unusable token. Character bigrams are the standard cheap substitute:
 * 「结构化知识」becomes 结构/构化/化知/知识, which matches a query for 「知识」
 * without a segmenter or a dictionary.
 *
 * Kept deterministic and dependency-free: the same document always produces the
 * same term list, so a stored index can be compared across runs.
 */
export function tokenize(text: string): string[] {
  const limited = text.slice(0, MAX_INDEX_CHARS).toLowerCase();
  const tokens: string[] = [];

  for (const run of limited.match(/[\p{Script=Han}]+/gu) ?? []) {
    // Single-character runs still carry meaning (「值」); longer ones use bigrams.
    if (run.length === 1) {
      tokens.push(run);
      continue;
    }
    for (let i = 0; i + 1 < run.length; i++) {
      const bigram = run.slice(i, i + 2);
      if (STOPWORDS[bigram] === true) continue;
      tokens.push(bigram);
    }
  }

  for (const match of limited.match(LATIN) ?? []) {
    if (match.length < 2) continue;
    if (STOPWORDS[match] === true) continue;
    tokens.push(match);
  }

  return tokens;
}

/** Token counts, sorted by term so the output is stable. */
export function termFrequencies(text: string): TermFrequency[] {
  const counts = new Map<string, number>();
  for (const token of tokenize(text)) counts.set(token, (counts.get(token) ?? 0) + 1);
  return [...counts.entries()]
    .map(([term, tf]) => ({ term, tf }))
    .sort((a, b) => a.term.localeCompare(b.term));
}

export interface LexicalDocument {
  id: string;
  text: string;
}

export interface LexicalScore {
  id: string;
  score: number;
  /** The query terms that actually occurred, with the document's frequency. */
  matched: TermFrequency[];
}

/**
 * BM25 over an in-memory document set.
 *
 * Documents are supplied by the caller rather than read from disk, so the store
 * can decide the corpus (only accepted claims, only live evidence) and the same
 * function serves both a full index and a filtered subset.
 */
export function bm25(
  documents: readonly LexicalDocument[],
  query: string,
  options: { limit?: number } = {},
): LexicalScore[] {
  const queryTerms = [...new Set(tokenize(query))];
  if (queryTerms.length === 0 || documents.length === 0) return [];

  const frequencies = documents.map((document) => {
    const map = new Map<string, number>();
    let length = 0;
    for (const { term, tf } of termFrequencies(document.text)) {
      map.set(term, tf);
      length += tf;
    }
    return { id: document.id, map, length };
  });

  const averageLength =
    frequencies.reduce((sum, entry) => sum + entry.length, 0) / frequencies.length || 1;
  const documentFrequency = new Map<string, number>();
  for (const term of queryTerms) {
    let count = 0;
    for (const entry of frequencies) if (entry.map.has(term)) count++;
    documentFrequency.set(term, count);
  }

  const scores: LexicalScore[] = [];
  for (const entry of frequencies) {
    let score = 0;
    const matched: TermFrequency[] = [];
    for (const term of queryTerms) {
      const tf = entry.map.get(term);
      if (tf === undefined) continue;
      const df = documentFrequency.get(term) ?? 0;
      // BM25 idf with the +0.5 smoothing that keeps it positive for df = N.
      const idf = Math.log(1 + (documents.length - df + 0.5) / (df + 0.5));
      const denominator = tf + K1 * (1 - B + (B * entry.length) / averageLength);
      score += idf * ((tf * (K1 + 1)) / denominator);
      matched.push({ term, tf });
    }
    if (matched.length === 0) continue;
    scores.push({ id: entry.id, score, matched });
  }

  return scores
    .sort((a, b) => b.score - a.score || a.id.localeCompare(b.id))
    .slice(0, options.limit ?? scores.length);
}

/**
 * Cosine similarity between two vectors of equal, non-zero length.
 *
 * Throws on a dimension mismatch rather than padding: a 768-dimension vector is
 * not a 1536-dimension vector with zeros, and silently comparing them would
 * produce a ranking that looks valid and means nothing.
 */
export function cosine(a: readonly number[], b: readonly number[]): number {
  if (a.length !== b.length) {
    throw new Error(`Vector dimension mismatch: ${a.length} vs ${b.length}`);
  }
  if (a.length === 0) return 0;
  let dot = 0;
  let normA = 0;
  let normB = 0;
  for (let i = 0; i < a.length; i++) {
    const x = a[i]!;
    const y = b[i]!;
    if (!Number.isFinite(x) || !Number.isFinite(y)) {
      throw new Error("Vector contains a non-finite value");
    }
    dot += x * y;
    normA += x * x;
    normB += y * y;
  }
  if (normA === 0 || normB === 0) return 0;
  return dot / (Math.sqrt(normA) * Math.sqrt(normB));
}

export interface RankedEntry {
  id: string;
  score: number;
}

/**
 * Reciprocal rank fusion.
 *
 * Fuses axes by position, not by value: a lexical score of 12.7 and a cosine of
 * 0.83 have no common unit, but "first on the lexical axis" and "first on the
 * vector axis" are comparable. Ties break on the id so the result is stable
 * across runs even when scores are equal.
 */
export function reciprocalRankFusion(
  axes: Array<{ name: string; weight?: number; entries: readonly RankedEntry[] }>,
): Array<{ id: string; score: number; ranks: Record<string, number> }> {
  const fused = new Map<string, { score: number; ranks: Record<string, number> }>();

  for (const axis of axes) {
    const weight = axis.weight ?? 1;
    axis.entries.forEach((entry, index) => {
      const current = fused.get(entry.id) ?? { score: 0, ranks: {} };
      current.score += weight / (RRF_K + index + 1);
      current.ranks[axis.name] = index + 1;
      fused.set(entry.id, current);
    });
  }

  return [...fused.entries()]
    .map(([id, entry]) => ({ id, score: entry.score, ranks: entry.ranks }))
    .sort((a, b) => b.score - a.score || a.id.localeCompare(b.id));
}

/** Longest prefix of `text` that fits `max`, cut on a word boundary when one is near. */
export function truncate(text: string, max: number): string {
  if (text.length <= max) return text;
  const slice = text.slice(0, max);
  const space = slice.lastIndexOf(" ");
  return `${(space > max * 0.6 ? slice.slice(0, space) : slice).trimEnd()}…`;
}

/** Whether a string contains CJK, used to pick a segmenter-free search hint. */
export function hasCjk(text: string): boolean {
  return CJK.test(text);
}
