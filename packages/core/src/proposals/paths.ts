import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import matter from "gray-matter";
import { ConceptIdSchema } from "../types/index.ts";

/**
 * Resolve a concept id to its file, or null when it does not exist.
 *
 * The id is validated as a slug before it is used as a filename: an id straight
 * from a proposal or a request body is attacker-controlled, and `join` happily
 * walks out of the knowledge base with `../../`. A concept is also required to
 * live at `concepts/<layer>/<id>.md` with a frontmatter `id` equal to its
 * filename, so a stray `.md` file elsewhere in the tree is never mistaken for
 * one.
 *
 * Layer order fixes which file wins if a concept was moved; the KB forbids
 * duplicates, so the first match is the only match.
 */
const LAYERS = ["L0-axioms", "L1-core", "L2-support", "L3-instances"];

const ID_ERROR = (id: string): Error =>
  new Error(`Invalid concept id: ${JSON.stringify(id)}. Must be a lowercase slug [a-z0-9-]`);

export async function resolveConceptPath(kbPath: string, conceptId: string): Promise<string | null> {
  if (!ConceptIdSchema.safeParse(conceptId).success) throw ID_ERROR(conceptId);

  for (const layer of LAYERS) {
    const dir = join(kbPath, "concepts", layer);
    let entries: string[];
    try {
      entries = await readdir(dir);
    } catch {
      continue;
    }

    const file = `${conceptId}.md`;
    if (!entries.includes(file)) continue;

    const filePath = join(dir, file);
    const raw = await readFile(filePath, "utf-8");
    const { data } = matter(raw);
    // A file whose declared id disagrees with its name is a KB error, not a
    // redirect target: following it would let one concept overwrite another.
    if (data.id !== conceptId) {
      throw new Error(`Concept file ${layer}/${file} declares id ${JSON.stringify(data.id)}`);
    }
    return filePath;
  }

  return null;
}

/**
 * Validate a proposal filename before it is joined onto a proposals directory.
 *
 * Same reasoning as `resolveConceptPath`: the name arrives from a request body,
 * and `2026-11-13-../../escape-is-a-goal.md` normalises to a path outside the
 * directory it was meant for.
 */
export function assertProposalFilename(filename: string): string {
  if (!/^[A-Za-z0-9._-]+\.md$/.test(filename) || filename.startsWith(".") || filename.includes("..")) {
    throw new Error(`Invalid proposal filename: ${JSON.stringify(filename)}`);
  }
  return filename;
}
