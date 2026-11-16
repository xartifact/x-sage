import { createHash } from "node:crypto";
import matter from "gray-matter";
import { ConceptFrontmatterSchema, type ConceptFrontmatter, type Confidence } from "../types/index.ts";
import type { ClaimKind, Evidence, EvidenceKind } from "./types.ts";

/**
 * Markdown as interchange, not as authority.
 *
 * The structured record is serialised to a concept document so a human can read
 * and edit it with ordinary tools, and parsed back so an edit is reviewable
 * against the revision it was based on.
 *
 * Layout, with the human body kept byte-identical between export and import:
 *
 * ```
 * ---
 * id: decision
 * ...the existing concept frontmatter...
 * ---
 * <human body, exactly as stored>
 *
 * <!-- x-sage:claims:begin -->
 * ## 断言 · claims (managed by x-sage; text after the last `::` is yours to edit)
 * - clm_ab12cd :: premise :: confirmed :: 决策是行动的起点。
 * <!-- x-sage:claims:end -->
 *
 * <!-- x-sage:evidence:begin -->
 * - evd_9f8e7d :: text :: 会议记录 2026-11-02 :: concepts/decision.md
 * <!-- x-sage:evidence:end -->
 * ```
 *
 * The claims block is machine-managed: ids anchor the edit, the trailing text is
 * what a reviewer changes. The evidence block is a read-only projection — an
 * image's bytes and hash cannot be edited in Markdown, so evidence changes go
 * through the structured API where the asset is validated.
 */

const CLAIMS_BEGIN = "<!-- x-sage:claims:begin -->";
const CLAIMS_END = "<!-- x-sage:claims:end -->";
const EVIDENCE_BEGIN = "<!-- x-sage:evidence:begin -->";
const EVIDENCE_END = "<!-- x-sage:evidence:end -->";

const CLAIM_HEADER = "## 断言 · claims (managed by x-sage; text after the last `::` is yours to edit)";
const EVIDENCE_HEADER = "## 证据 · evidence (read-only projection; change it through the structured API)";

/**
 * Normalise the human body so export and import agree byte for byte.
 *
 * Only the boundary is touched: leading blank lines (which the document layout
 * introduces) are dropped and the text ends with exactly one newline. Interior
 * whitespace, indentation and code blocks are preserved verbatim.
 */
export function normalizeProjectionBody(body: string): string {
  const trimmed = body.replace(/^[ \t]*\r?\n/, "").replace(/\s+$/, "");
  return trimmed === "" ? "" : `${trimmed}\n`;
}

export interface ProjectedClaim {
  id: string;
  kind: ClaimKind;
  confidence: Confidence;
  text: string;
}

export interface ProjectedEvidence {
  id: string;
  kind: EvidenceKind;
  title: string;
  locator: string | null;
}

export interface KnowledgeDocument {
  frontmatter: ConceptFrontmatter;
  body: string;
  claims: ProjectedClaim[];
  evidence: ProjectedEvidence[];
}

/**
 * Escape a claim text so one line carries it exactly.
 *
 * A claim is one line in the projection because the id/kind/confidence prefix is
 * what makes it editable; a raw newline would split one claim into two entries.
 * Backslash is escaped first so the encoding is reversible: a text containing the
 * literal characters `\n` does not decode as a newline.
 */
function escapeClaimText(text: string): string {
  return text
    .replaceAll("\\", "\\\\")
    .replace(/\r/g, "\\r")
    .replace(/\n/g, "\\n");
}

function unescapeClaimText(text: string): string {
  let out = "";
  for (let i = 0; i < text.length; i++) {
    const char = text[i]!;
    if (char !== "\\" || i + 1 >= text.length) {
      out += char;
      continue;
    }
    const next = text[i + 1]!;
    if (next === "n") {
      out += "\n";
      i++;
    } else if (next === "r") {
      out += "\r";
      i++;
    } else if (next === "\\") {
      out += "\\";
      i++;
    } else {
      out += char;
    }
  }
  return out;
}

/**
 * One line per claim: text is last so it may contain the `::` separator
 * (reassembly is from the right), and newlines are escaped so one claim stays
 * one line.
 */
function claimLine(claim: ProjectedClaim): string {
  return `- ${claim.id} :: ${claim.kind} :: ${claim.confidence} :: ${escapeClaimText(claim.text)}`;
}

/**
 * Escape a `::` inside a non-terminal field.
 *
 * Claim text is last, so it may contain the separator and is reassembled. Title
 * and locator are positional: an unescaped separator inside a title would shift
 * the locator into the title on read-back.
 */
function escapeSeparator(value: string): string {
  return value.replace(/::/g, "\\::");
}

function unescapeSeparator(value: string): string {
  return value.replace(/\\::/g, "::");
}

function evidenceLine(evidence: ProjectedEvidence): string {
  return `- ${evidence.id} :: ${evidence.kind} :: ${escapeSeparator(evidence.title)} :: ${escapeSeparator(evidence.locator ?? "")}`;
}

/**
 * Render a concept record as an editable Markdown document.
 *
 * Throws when the human body already contains a managed-block sentinel: the
 * sentinels are how the projection is parsed back, so a body carrying one would
 * be split in the wrong place and a later import would silently drop prose.
 */
export function buildKnowledgeDocument(document: KnowledgeDocument): string {
  const { frontmatter, body, claims, evidence } = document;
  for (const sentinel of [CLAIMS_BEGIN, CLAIMS_END, EVIDENCE_BEGIN, EVIDENCE_END]) {
    if (body.includes(sentinel)) {
      throw new Error(`Concept body must not contain the managed-block marker ${sentinel}`);
    }
  }
  const data: Record<string, unknown> = {
    id: frontmatter.id,
    atom: frontmatter.atom,
    layer: frontmatter.layer,
    domain: frontmatter.domain,
    confidence: frontmatter.confidence,
    source: frontmatter.source,
    triggers: frontmatter.triggers,
  };
  if (frontmatter.edges.length > 0) data.edges = frontmatter.edges;

  const sections: string[] = [];
  const humanBody = normalizeProjectionBody(body).replace(/\n$/, "");
  if (humanBody !== "") sections.push(humanBody);

  sections.push(
    [CLAIMS_BEGIN, CLAIM_HEADER, ...claims.map(claimLine), CLAIMS_END].join("\n"),
  );
  if (evidence.length > 0) {
    sections.push(
      [EVIDENCE_BEGIN, EVIDENCE_HEADER, ...evidence.map(evidenceLine), EVIDENCE_END].join("\n"),
    );
  }

  return matter.stringify(`\n${sections.join("\n\n")}\n`, data);
}

export interface ParsedKnowledgeDocument {
  frontmatter: ConceptFrontmatter;
  /** Human body with the managed blocks removed and trailing whitespace normalised. */
  body: string;
  claims: ProjectedClaim[];
  /** Read-only projection; an edit here is reported, never applied. */
  evidence: ProjectedEvidence[];
  /** True when the document carried the managed claims block. */
  hasClaimsBlock: boolean;
}

/**
 * Parse a knowledge document back into its parts.
 *
 * A document without the claims block is still valid: a concept authored as a
 * plain Markdown file imports with zero claims, which is exactly how the
 * existing KB migrates.
 */
export function parseKnowledgeDocument(raw: string): ParsedKnowledgeDocument {
  const { data, content } = matter(raw);
  const frontmatter = ConceptFrontmatterSchema.parse(data);

  const claims: ProjectedClaim[] = [];
  const evidence: ProjectedEvidence[] = [];
  let body = content;
  let hasClaimsBlock = false;

  const claimsStart = body.indexOf(CLAIMS_BEGIN);
  if (claimsStart !== -1) {
    const claimsEnd = body.indexOf(CLAIMS_END, claimsStart);
    if (claimsEnd === -1) throw new Error("Knowledge document has an unterminated claims block");
    hasClaimsBlock = true;
    const block = body.slice(claimsStart + CLAIMS_BEGIN.length, claimsEnd);
    for (const line of block.split("\n")) {
      const parsed = parseClaimLine(line);
      if (parsed !== null) claims.push(parsed);
    }
    body = `${body.slice(0, claimsStart)}${body.slice(claimsEnd + CLAIMS_END.length)}`;
  }

  const evidenceStart = body.indexOf(EVIDENCE_BEGIN);
  if (evidenceStart !== -1) {
    const evidenceEnd = body.indexOf(EVIDENCE_END, evidenceStart);
    if (evidenceEnd === -1) throw new Error("Knowledge document has an unterminated evidence block");
    const block = body.slice(evidenceStart + EVIDENCE_BEGIN.length, evidenceEnd);
    for (const line of block.split("\n")) {
      const parsed = parseEvidenceLine(line);
      if (parsed !== null) evidence.push(parsed);
    }
    body = `${body.slice(0, evidenceStart)}${body.slice(evidenceEnd + EVIDENCE_END.length)}`;
  }

  return {
    frontmatter,
    body: normalizeProjectionBody(body),
    claims,
    evidence,
    hasClaimsBlock,
  };
}

function parseClaimLine(line: string): ProjectedClaim | null {
  const trimmed = line.trim();
  if (!trimmed.startsWith("- ")) return null;
  const parts = trimmed.slice(2).split(" :: ");
  if (parts.length < 4) return null;
  const [id, kind, confidence, ...rest] = parts;
  // The separator inside the text is preserved by rejoining, so only the
  // trailing whitespace of the whole field is trimmed.
  const text = unescapeClaimText(rest.join(" :: ")).trim();
  if (!id || !kind || !confidence || text === "") return null;
  return {
    id,
    kind: kind as ClaimKind,
    confidence: confidence as Confidence,
    text,
  };
}

function parseEvidenceLine(line: string): ProjectedEvidence | null {
  const trimmed = line.trim();
  if (!trimmed.startsWith("- ")) return null;
  const parts = trimmed.slice(2).split(" :: ");
  if (parts.length < 3) return null;
  const [id, kind, ...rest] = parts;
  const rawTitle = rest.shift() ?? "";
  const rawLocator = rest.join(" :: ").trim();
  if (!id || !kind || rawTitle === "") return null;
  return {
    id,
    kind: kind as EvidenceKind,
    title: unescapeSeparator(rawTitle),
    locator: rawLocator === "" ? null : unescapeSeparator(rawLocator),
  };
}

/**
 * Revision of the Markdown projection.
 *
 * Used as the review baseline for a write-back that arrived as a document: the
 * exporter hands this value out, the editor returns it, and a change underneath
 * is a reported conflict instead of a lost edit.
 */
export function projectionRevision(raw: string): string {
  return createHash("sha256").update(raw).digest("hex");
}

/** Evidence rows arrive as full records; a projection only needs the reference. */
export function projectEvidence(evidence: readonly Evidence[]): ProjectedEvidence[] {
  return evidence.map((item) => ({
    id: item.id,
    kind: item.kind,
    title: item.title,
    locator: item.locator ?? null,
  }));
}
