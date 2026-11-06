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
 * Format a ConceptFile back into markdown with frontmatter.
 */
export function formatConceptFile(concept: ConceptFile): string {
  const { frontmatter, molecule } = concept;
  const fm: Record<string, unknown> = {
    id: frontmatter.id,
    atom: frontmatter.atom,
    layer: frontmatter.layer,
    domain: frontmatter.domain,
    confidence: frontmatter.confidence,
    source: frontmatter.source,
    triggers: frontmatter.triggers,
  };
  if (frontmatter.edges.length > 0) fm.edges = frontmatter.edges;

  // YAML frontmatter stringification
  const lines = ["---"];
  for (const [key, value] of Object.entries(fm)) {
    if (Array.isArray(value)) {
      lines.push(`${key}:`);
      for (const item of value) {
        if (typeof item === "object" && item !== null) {
          lines.push(`  - ${JSON.stringify(item)}`);
        } else {
          lines.push(`  - ${String(item)}`);
        }
      }
    } else {
      lines.push(`${key}: ${typeof value === "string" ? `"${value}"` : value}`);
    }
  }
  lines.push("---");
  lines.push("");
  lines.push(molecule);
  return lines.join("\n");
}
