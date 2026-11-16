import { readFile, stat } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import type { KbContext } from "./kb.ts";

export class CatalogUnavailableError extends Error {}

export interface CatalogEntry {
  id: string;
  name: string;
  root: string;
}

/** The registry is operator-owned configuration, never a client-supplied path. */
export async function catalog(ctx: KbContext): Promise<CatalogEntry[]> {
  if (!ctx.registryPath)
    return [{ id: "default", name: "Default", root: ctx.root }];
  try {
    const raw: unknown = JSON.parse(await readFile(ctx.registryPath, "utf8"));
    if (
      !raw ||
      typeof raw !== "object" ||
      !("knowledgeBases" in raw) ||
      !Array.isArray(raw.knowledgeBases)
    )
      throw new Error();
    const ids = new Set<string>();
    const roots = new Set<string>();
    const entries: CatalogEntry[] = [];
    for (const item of raw.knowledgeBases) {
      if (
        !item ||
        typeof item !== "object" ||
        typeof item.id !== "string" ||
        !/^[a-z0-9][a-z0-9-]*$/.test(item.id) ||
        ids.has(item.id) ||
        typeof item.path !== "string" ||
        !item.path.trim() ||
        typeof item.name !== "string"
      )
        throw new Error();
      const root = resolve(dirname(ctx.registryPath), item.path);
      if (roots.has(root) || !(await stat(root)).isDirectory())
        throw new Error();
      ids.add(item.id);
      roots.add(root);
      entries.push({ id: item.id, name: item.name || item.id, root });
    }
    return entries;
  } catch {
    // Do not expose absolute operator paths or raw filesystem errors to clients.
    throw new CatalogUnavailableError(
      "Knowledge base registry is invalid or unavailable",
    );
  }
}
