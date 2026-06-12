import { readdir, readFile } from "node:fs/promises";
import { join, relative } from "node:path";
import { parseConceptFile } from "./parse.ts";
import { ManifestRowSchema, type ManifestRow } from "../types/index.ts";

/**
 * Scan a KB's concepts directory and generate MANIFEST rows.
 */
export async function generateManifest(kbPath: string): Promise<ManifestRow[]> {
  const layers = ["L0-axioms", "L1-core", "L2-support", "L3-instances"];
  const rows: ManifestRow[] = [];

  for (const layer of layers) {
    const layerDir = join(kbPath, "concepts", layer);
    let entries: string[];
    try {
      entries = await readdir(layerDir);
    } catch {
      continue; // layer directory doesn't exist yet
    }

    for (const file of entries) {
      if (!file.endsWith(".md")) continue;
      const filePath = join(layerDir, file);
      const raw = await readFile(filePath, "utf-8");
      const concept = parseConceptFile(raw, relative(kbPath, filePath));
      const row = ManifestRowSchema.parse({
        id: concept.frontmatter.id,
        atom: concept.frontmatter.atom,
        layer: concept.frontmatter.layer,
        domain: concept.frontmatter.domain,
        confidence: concept.frontmatter.confidence,
        source: concept.frontmatter.source,
        triggers: concept.frontmatter.triggers,
      });
      rows.push(row);
    }
  }

  return rows;
}

/**
 * Format manifest rows into the standard MANIFEST.md table.
 */
export function formatManifest(rows: ManifestRow[]): string {
  const header = [
    "# MANIFEST — 全局原子索引",
    "",
    "> 每次 AI 会话开始时必须加载本文件。",
    "> 格式：每个概念一行紧凑描述 + 触发关键词。",
    "",
    "| id | atom | layer | source | confidence | triggers |",
    "|----|------|-------|--------|------------|----------|",
  ];

  const tableRows = rows.map((r) => {
    const triggers = r.triggers.join(", ");
    return `| ${r.id} | ${r.atom} | ${r.layer} | ${r.source} | ${r.confidence} | ${triggers} |`;
  });

  return [...header, ...tableRows, ""].join("\n");
}
