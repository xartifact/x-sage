import { createHash } from "node:crypto";
import type { ConceptFrontmatter } from "../types/index.ts";

/**
 * Split a concept file into its frontmatter block and its body.
 *
 * The split is positional, not parsed: `prefix` is the exact bytes through the
 * closing `---` line, so `prefix + body` reproduces the file byte for byte.
 * That is what lets a body edit leave every metadata byte untouched — parsing
 * and re-serialising instead would reflow `domain: [a, b]` into a block list and
 * drop the trailing newline, so an unrelated body edit would show up as a
 * frontmatter diff.
 *
 * Throws when the file has no terminated frontmatter: such a file is not a
 * concept, and treating its whole text as a body would let an edit overwrite
 * the metadata it never had.
 */
export function splitConceptDocument(raw: string): { prefix: string; body: string } {
  if (!/^---[ \t]*\r?\n/.test(raw)) {
    throw new Error("Concept file has no YAML frontmatter");
  }

  let cursor = raw.indexOf("\n") + 1;
  while (cursor < raw.length) {
    const lineEnd = raw.indexOf("\n", cursor);
    const stop = lineEnd === -1 ? raw.length : lineEnd;
    const line = raw.slice(cursor, stop).replace(/\r$/, "");

    if (line === "---") {
      const end = lineEnd === -1 ? raw.length : lineEnd + 1;
      return { prefix: raw.slice(0, end), body: raw.slice(end) };
    }
    if (lineEnd === -1) break;
    cursor = lineEnd + 1;
  }

  throw new Error("Unterminated YAML frontmatter");
}

/** Reassemble a concept file from a positional split. Inverse of `splitConceptDocument`. */
export function joinConceptDocument(prefix: string, body: string): string {
  return prefix + body;
}

/**
 * A stable identity for one concept version.
 *
 * Covers the validated frontmatter and the body, so an atom, edge or body edit
 * all invalidate a proposal based on the previous state. The frontmatter is
 * canonicalised through a fixed key order rather than hashed as raw bytes:
 * otherwise rewriting `domain: [a, b]` as a block list (which the merge path
 * does) would look like a semantic change and produce false conflicts.
 *
 * `mtime` is deliberately not used — a checkout or a backup restore changes it
 * without changing the content.
 */
export function conceptRevision(frontmatter: ConceptFrontmatter, body: string): string {
  const canonical = JSON.stringify({
    id: frontmatter.id,
    atom: frontmatter.atom,
    layer: frontmatter.layer,
    domain: frontmatter.domain,
    confidence: frontmatter.confidence,
    source: frontmatter.source,
    triggers: frontmatter.triggers,
    edges: frontmatter.edges.map((edge) => ({
      type: edge.type,
      target: edge.target,
      confidence: edge.confidence,
    })),
  });

  return createHash("sha256").update(canonical).update("\n").update(body).digest("hex");
}

/**
 * Longest body a proposal may carry.
 *
 * The HTTP layer caps a whole request at 256 KiB, and a body arrives inside a
 * JSON envelope with other fields, so this leaves headroom. Exceeding it is a
 * 413 rather than a truncation: a silently shortened concept is worse than a
 * rejected one.
 */
export const MAX_BODY_CHARS = 128 * 1024;

/** A body that is only whitespace carries no knowledge and must not be applied. */
export function isBlankBody(body: string): boolean {
  return body.trim() === "";
}

/**
 * Normalise a candidate body to exactly one trailing newline.
 *
 * Every concept file ends with one, and a candidate pasted from a terminal
 * brings either none or a handful of blank lines. Applying the normalisation at
 * one place keeps three things in agreement: what a proposal stores, what a
 * merge writes, and what `concept body` reads back. Doing it only at write time
 * made the read-back differ from the submitted file by exactly one byte, which
 * reads as "the server changed my text".
 */
export function normalizeBody(body: string): string {
  return `${body.replace(/\s+$/, "")}\n`;
}
