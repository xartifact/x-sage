import matter from "gray-matter";
import { ConceptFrontmatterSchema, type ConceptFile } from "../types/index.ts";

/**
 * Parse a concept markdown file into frontmatter + molecule.
 * Validates frontmatter against the Zod schema.
 */
export function parseConceptFile(raw: string, filePath: string): ConceptFile {
  const { data, content } = matter(raw);
  const frontmatter = ConceptFrontmatterSchema.parse(data);
  const molecule = content.trim();
  return { frontmatter, molecule, filePath };
}

/**
 * Serialise frontmatter + body back into a concept file.
 *
 * Delegates the YAML to gray-matter instead of hand-building lines. The hand-
 * rolled version quoted every string with `"` and never escaped it, so an atom
 * containing a double quote produced a file that no longer parsed — the next
 * `validate` reported the concept as broken and `/api/viz` failed outright.
 * gray-matter picks the quoting style per value and escapes what it quotes.
 */
export function formatConceptFile(concept: ConceptFile): string {
  const { frontmatter, molecule } = concept;
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

  // `stringify` appends one newline after the body; concept files end with one,
  // and the trailing blank line the empty body would otherwise leave is dropped.
  return matter.stringify(`${molecule}\n`, data);
}
