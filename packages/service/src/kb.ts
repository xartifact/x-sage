import { join } from "node:path";

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
