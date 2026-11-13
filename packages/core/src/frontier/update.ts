import { readFile } from "node:fs/promises";
import { writeFileAtomic } from "../fs/atomic.ts";
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
  await writeFileAtomic(join(kbPath, "frontier.md"), content);
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

/** Row form of one entry, matching the table's column order. */
function formatRow(entry: FrontierEntry): string {
  return `| ${entry.concept} | ${entry.source} | ${entry.reason} | ${entry.priority} |`;
}

/**
 * Insert one row into the pending-concepts table, leaving every other byte alone.
 *
 * `writeFrontier` re-renders the whole file from the parsed table, which loses
 * the "概念内张力" and "L2→L3 待展开方向" sections entirely — that is why the CLI
 * refused to expose this operation at all. Inserting after the last row of the
 * target table keeps the prose, the other tables and their ordering intact.
 *
 * A frontier file with no pending-concepts table is left untouched and reported:
 * guessing where to create one would put the row in a section that means
 * something else.
 */
export async function addFrontierEntry(kbPath: string, entry: FrontierEntry): Promise<void> {
  const filePath = join(kbPath, "frontier.md");
  let raw: string;
  try {
    raw = await readFile(filePath, "utf-8");
  } catch {
    throw new Error(`frontier.md not found in ${kbPath}`);
  }

  const lines = raw.split("\n");
  const tableStart = lines.findIndex((line) => line.startsWith("| 概念"));
  if (tableStart === -1) {
    throw new Error("frontier.md has no 未决概念 table to insert into");
  }

  // Walk to the last row of this table; the first non-row line after the header
  // ends it (a blank line, a heading, or another table).
  let lastRow = tableStart;
  for (let index = tableStart + 1; index < lines.length; index++) {
    const line = lines[index]!;
    if (line.startsWith("|")) {
      lastRow = index;
      continue;
    }
    break;
  }

  const row = formatRow(entry);
  if (lines.slice(tableStart, lastRow + 1).some((line) => line === row)) {
    return; // already present; adding it twice is not a second pending item
  }

  lines.splice(lastRow + 1, 0, row);
  await writeFileAtomic(filePath, lines.join("\n"));
}
