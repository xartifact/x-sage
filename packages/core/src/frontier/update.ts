import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

export interface FrontierEntry {
  concept: string;
  source: string;
  reason: string;
  priority: "high" | "medium" | "low";
}

/**
 * Read frontier entries from a frontier.md file.
 * Simple parser for the markdown table format.
 */
export async function readFrontier(kbPath: string): Promise<FrontierEntry[]> {
  const filePath = join(kbPath, "frontier.md");
  try {
    const raw = await readFile(filePath, "utf-8");
    return parseFrontierMarkdown(raw);
  } catch {
    return [];
  }
}

/**
 * Parse frontier.md markdown table into structured entries.
 */
export function parseFrontierMarkdown(raw: string): FrontierEntry[] {
  const entries: FrontierEntry[] = [];
  const lines = raw.split("\n");
  let inTable = false;

  for (const line of lines) {
    if (line.startsWith("| 概念") || line.startsWith("| concept")) {
      inTable = true;
      continue;
    }
    if (line.startsWith("|----") || line.startsWith("|---")) {
      continue;
    }
    if (inTable && line.startsWith("|")) {
      const cells = line.split("|").map((c) => c.trim()).filter(Boolean);
      if (cells.length >= 4) {
        const concept = cells[0]!;
        const source = cells[1]!;
        const reason = cells[2]!;
        const priority = cells[3]!;
        entries.push({
          concept,
          source,
          reason,
          priority: priority as "high" | "medium" | "low",
        });
      }
    } else if (inTable && !line.startsWith("|")) {
      inTable = false;
    }
  }

  return entries;
}

/**
 * Write frontier entries back to frontier.md.
 */
export async function writeFrontier(kbPath: string, entries: FrontierEntry[]): Promise<void> {
  const content = formatFrontierMarkdown(entries);
  await writeFile(join(kbPath, "frontier.md"), content, "utf-8");
}

/**
 * Format frontier entries into markdown.
 */
export function formatFrontierMarkdown(entries: FrontierEntry[]): string {
  const lines = [
    "# 激活边界 frontier.md",
    "",
    "> 记录已识别但未深入的节点和未解张力。每次会话结束后更新。",
    "",
    "## 未决概念",
    "",
    "| 概念 | 来源 | 未解问题 | 优先级 |",
    "|------|------|----------|--------|",
  ];

  for (const entry of entries) {
    lines.push(`| ${entry.concept} | ${entry.source} | ${entry.reason} | ${entry.priority} |`);
  }

  lines.push("");
  return lines.join("\n");
}

/**
 * Add a new entry to the frontier.
 */
export async function addFrontierEntry(kbPath: string, entry: FrontierEntry): Promise<void> {
  const existing = await readFrontier(kbPath);
  existing.push(entry);
  await writeFrontier(kbPath, existing);
}
