import { readdir, readFile } from "node:fs/promises";
import { join, relative } from "node:path";
import {
  parseConceptFile,
  type ConceptFile,
  formatZodError,
} from "@x-sage/core";
import { LAYERS, conceptsDir, layerDir, type KbContext } from "./kb.ts";

/** One concept file read from disk, in the order layers appear. */
export interface LoadedConcept {
  /** Path relative to the KB root, e.g. `concepts/L1-core/risk.md`. */
  filePath: string;
  layer: number;
  file: ConceptFile;
}

export interface LoadConceptsOptions {
  /**
   * Collect per-file failures instead of throwing. Off by default so a broken
   * file surfaces immediately; `validate` turns it on to report every file.
   */
  tolerateErrors?: boolean;
}

export interface LoadConceptsResult {
  concepts: LoadedConcept[];
  errors: Array<{ filePath: string; message: string }>;
}

/**
 * Read every concept file in the KB.
 *
 * This is the single file-walking routine: graph generation, manifest
 * generation, viz payloads and validation all consume it, so a new layer or a
 * changed parse rule only needs editing here.
 */
export async function loadConcepts(
  ctx: KbContext,
  options: LoadConceptsOptions = {},
): Promise<LoadConceptsResult> {
  const concepts: LoadedConcept[] = [];
  const errors: LoadConceptsResult["errors"] = [];

  for (const layer of LAYERS) {
    const dir = layerDir(ctx, layer);
    let entries: string[];
    try {
      entries = await readdir(dir);
    } catch {
      continue; // layer directory may not exist yet
    }

    for (const name of entries.sort()) {
      if (!name.endsWith(".md")) continue;
      const absolute = join(dir, name);
      const filePath = relative(ctx.root, absolute);

      try {
        const raw = await readFile(absolute, "utf-8");
        concepts.push({ filePath, layer: LAYERS.indexOf(layer), file: parseConceptFile(raw, filePath) });
      } catch (err) {
        const message = formatZodError(err) ?? (err instanceof Error ? err.message : "Unknown error");
        if (!options.tolerateErrors) {
          throw new Error(`${filePath}: ${message}`);
        }
        errors.push({ filePath, message });
      }
    }
  }

  return { concepts, errors };
}

/** Concepts belonging to one layer directory, for callers that filter. */
export async function loadConceptsInLayer(
  ctx: KbContext,
  layer: string,
): Promise<LoadedConcept[]> {
  const { concepts } = await loadConcepts(ctx);
  return concepts.filter((concept) => concept.filePath.startsWith(join("concepts", layer)));
}

/** Resolve a concept file by id, scanning layers. Throws when absent. */
export async function findConceptFile(ctx: KbContext, conceptId: string): Promise<LoadedConcept> {
  const { concepts } = await loadConcepts(ctx);
  const match = concepts.find((concept) => concept.file.frontmatter.id === conceptId);
  if (!match) {
    throw new Error(`Concept not found: ${conceptId}`);
  }
  return match;
}

/** Raw markdown of one concept file, for `--format human`. */
export async function readConceptRaw(ctx: KbContext, conceptId: string): Promise<string> {
  const concept = await findConceptFile(ctx, conceptId);
  return readFile(join(ctx.root, concept.filePath), "utf-8");
}

export { conceptsDir };
