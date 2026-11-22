import { join } from "node:path";
import { lstat } from "node:fs/promises";
import { ApiError } from "./http.ts";

/** Layer directory names in presentation order, most abstract first. */
export const LAYERS = ["L0-axioms", "L1-core", "L2-support", "L3-instances"] as const;

/**
 * A single knowledge base on disk.
 *
 * Every service function takes this instead of resolving paths itself, so the
 * same code serves requests scoped to one KB. The dispatcher resolves an
 * optional registry selection before invoking any use case.
 */
export interface KbContext {
  /** Absolute path to the KB directory, e.g. `<repo>/kb/default`. */
  root: string;
  /** Operator-owned registry; clients select IDs, never filesystem paths. */
  registryPath?: string;
}

export function kb(root: string): KbContext {
  return { root };
}

/** Refuse to serve Markdown that may predate a retired SQLite authority. */
export async function requireMarkdownSource(ctx: KbContext): Promise<void> {
  for (const name of ["knowledge.db", "knowledge.db-wal", "knowledge.db-shm"]) {
    try {
      await lstat(join(ctx.root, name));
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") continue;
      throw new ApiError(503, "Could not verify the Markdown source");
    }
    throw new ApiError(503, "A legacy SQLite database is present. Migrate it offline before serving this knowledge base.");
  }
}

export function conceptsDir(ctx: KbContext): string {
  return join(ctx.root, "concepts");
}

export function layerDir(ctx: KbContext, layer: string): string {
  return join(ctx.root, "concepts", layer);
}

export function graphFile(ctx: KbContext): string {
  return join(ctx.root, "graph.json");
}

export function manifestFile(ctx: KbContext): string {
  return join(ctx.root, "MANIFEST.md");
}

export function frontierFile(ctx: KbContext): string {
  return join(ctx.root, "frontier.md");
}

export function proposalsDir(ctx: KbContext): string {
  return join(ctx.root, "proposals");
}

export function checkpointsDir(ctx: KbContext): string {
  return join(ctx.root, "checkpoints");
}
