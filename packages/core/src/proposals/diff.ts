/**
 * Line diff for reviewing a body proposal.
 *
 * A unified diff is what a reviewer already knows how to read, and it keeps the
 * output proportional to the change instead of to the document. Implemented as
 * a plain LCS over lines: the alternative (pulling in a diff library) adds a
 * dependency to a package that only has gray-matter and zod, for ~40 lines of
 * algorithm.
 */

export interface DiffHunk {
  /** 1-based start line in the old body. */
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  /** Prefixed lines: " " context, "-" removed, "+" added. */
  lines: string[];
}

const CONTEXT = 3;

/** Longest common subsequence table over two line arrays. */
function lcsTable(a: string[], b: string[]): Uint32Array {
  const width = b.length + 1;
  const table = new Uint32Array((a.length + 1) * width);
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      table[i * width + j] =
        a[i] === b[j]
          ? table[(i + 1) * width + j + 1]! + 1
          : Math.max(table[(i + 1) * width + j]!, table[i * width + j + 1]!);
    }
  }
  return table;
}

type Op = { kind: " " | "-" | "+"; text: string; oldLine: number; newLine: number };

/** Walk both sides once, emitting the edit script in order. */
function editScript(a: string[], b: string[]): Op[] {
  const width = b.length + 1;
  const table = lcsTable(a, b);
  const ops: Op[] = [];
  let i = 0;
  let j = 0;

  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      ops.push({ kind: " ", text: a[i]!, oldLine: i + 1, newLine: j + 1 });
      i++;
      j++;
    } else if (table[(i + 1) * width + j]! >= table[i * width + j + 1]!) {
      ops.push({ kind: "-", text: a[i]!, oldLine: i + 1, newLine: j + 1 });
      i++;
    } else {
      ops.push({ kind: "+", text: b[j]!, oldLine: i + 1, newLine: j + 1 });
      j++;
    }
  }
  while (i < a.length) {
    ops.push({ kind: "-", text: a[i]!, oldLine: i + 1, newLine: j + 1 });
    i++;
  }
  while (j < b.length) {
    ops.push({ kind: "+", text: b[j]!, oldLine: i + 1, newLine: j + 1 });
    j++;
  }
  return ops;
}

function splitLines(text: string): string[] {
  if (text === "") return [];
  return text.replace(/\n$/, "").split("\n");
}

/**
 * Diff two bodies, grouped into hunks with `CONTEXT` unchanged lines around
 * each change. Unchanged stretches longer than that are collapsed, which is the
 * difference between a reviewable diff and a copy of the document.
 */
export function diffBodies(before: string, after: string): DiffHunk[] {
  const ops = editScript(splitLines(before), splitLines(after));

  const interesting = ops.map((op, index) => op.kind !== " " ? index : -1).filter((index) => index >= 0);
  if (interesting.length === 0) return [];

  // Merge change regions that are within 2*CONTEXT of each other.
  const ranges: Array<[number, number]> = [];
  for (const index of interesting) {
    const start = Math.max(0, index - CONTEXT);
    const end = Math.min(ops.length - 1, index + CONTEXT);
    const last = ranges[ranges.length - 1];
    if (last !== undefined && start <= last[1] + 1) {
      last[1] = Math.max(last[1], end);
    } else {
      ranges.push([start, end]);
    }
  }

  return ranges.map(([start, end]) => {
    const slice = ops.slice(start, end + 1);
    const oldLines = slice.filter((op) => op.kind !== "+");
    const newLines = slice.filter((op) => op.kind !== "-");
    return {
      oldStart: oldLines[0]?.oldLine ?? 1,
      oldLines: oldLines.length,
      newStart: newLines[0]?.newLine ?? 1,
      newLines: newLines.length,
      lines: slice.map((op) => `${op.kind}${op.text}`),
    };
  });
}

/** Render hunks as a unified diff with a conventional header. */
export function formatUnifiedDiff(hunks: DiffHunk[], fromLabel: string, toLabel: string): string {
  if (hunks.length === 0) return "No changes.\n";

  const out = [`--- ${fromLabel}`, `+++ ${toLabel}`];
  for (const hunk of hunks) {
    out.push(`@@ -${hunk.oldStart},${hunk.oldLines} +${hunk.newStart},${hunk.newLines} @@`);
    out.push(...hunk.lines);
  }
  return `${out.join("\n")}\n`;
}