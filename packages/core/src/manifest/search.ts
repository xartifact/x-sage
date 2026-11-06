import type { ManifestRow } from "../types/index.ts";

/**
 * Search manifest rows by keyword.
 *
 * Matches against:
 * - id (exact or substring, case-insensitive)
 * - atom (substring, case-insensitive)
 * - triggers (exact or substring match of any trigger, case-insensitive)
 *
 * Ranking (highest first):
 * 1. exact id match
 * 2. exact trigger match
 * 3. atom substring match
 * 4. trigger substring match
 */
export function searchManifest(rows: ManifestRow[], keyword: string): ManifestRow[] {
  const lowerKeyword = keyword.toLowerCase();

  type ScoredRow = { row: ManifestRow; score: number };
  const scored: ScoredRow[] = [];

  for (const row of rows) {
    let score = 0;
    let matched = false;

    const lowerId = row.id.toLowerCase();
    if (lowerId === lowerKeyword) {
      score = 4;
      matched = true;
    } else if (lowerId.includes(lowerKeyword)) {
      score = 1;
      matched = true;
    }

    const lowerAtom = row.atom.toLowerCase();
    if (lowerAtom.includes(lowerKeyword)) {
      score = Math.max(score, 2);
      matched = true;
    }

    for (const trigger of row.triggers) {
      const lowerTrigger = trigger.toLowerCase();
      if (lowerTrigger === lowerKeyword) {
        score = Math.max(score, 3);
        matched = true;
        break;
      }
      if (lowerTrigger.includes(lowerKeyword)) {
        score = Math.max(score, 1);
        matched = true;
      }
    }

    if (matched) {
      scored.push({ row, score });
    }
  }

  scored.sort((a, b) => b.score - a.score);
  return scored.map((s) => s.row);
}
