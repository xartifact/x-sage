import type { GraphEdge, ManifestRow } from "@x-sage/core";
import type { Activation } from "./query.ts";
import type { EdgeHit } from "./graph.ts";
import type { PathResult } from "./query.ts";
import type { VizPayload } from "./viz.ts";
import type { ConceptValidationReport } from "./validate.ts";
import type { ProposalLists, MergeOutcome } from "./write.ts";
import type { Checkpoint } from "@x-sage/core";
import type { GraphValidationReport } from "./graph.ts";
import type { FrontierEntry } from "@x-sage/core/frontier";

/**
 * Text renderers.
 *
 * All AI-facing output is produced here, on the server side. The CLI is a
 * transport: it sends a request and writes the response body to stdout
 * verbatim, so a client machine never needs the graph or the schema.
 *
 * Every function returns the exact bytes that should appear on stdout,
 * including the trailing newline. `console.log` used to add that newline; now
 * it is part of the string so file responses (HTTP) and terminal responses
 * (CLI) are byte-identical.
 */

const NL = "\n";

/**
 * One concept in the AI-oriented transcript format.
 *
 * `EDGES` lists only the edges this concept itself declares; `REQUIRES` is the
 * transitive prerequisite set and `LOAD-NEXT` the asserted `enables` targets.
 * Both come precomputed in the DTO because the client has no graph.
 *
 * Note `SOURCE` prints `domain[0]`, not the concept's `source` field: that is
 * the shape consumers already parse, and changing it would silently alter what
 * the model reads.
 */
export function renderConceptAi(activation: Activation): string {
  const { node, directEdges, triggers, molecule, requires, loadNext } = activation;

  const edgeStr = directEdges
    .filter((edge: GraphEdge) => edge.from === node.id)
    .map((edge: GraphEdge) => `${edge.type}→${edge.target}`)
    .join(", ");

  return [
    `[CONCEPT: ${node.id}]`,
    `ATOM: ${node.atom}`,
    `LAYER: ${node.layer} | SOURCE: ${node.domain[0] ?? "unknown"} | CONFIDENCE: ${node.confidence}`,
    `EDGES: ${edgeStr || "(none)"}`,
    `REQUIRES: [${requires.join(", ")}]`,
    `TRIGGERS: [${triggers.join(", ")}]`,
    `LOAD-NEXT: [${loadNext.join(", ")}]`,
    "---",
    molecule,
  ].join(NL);
}

/** Compact neighbour block: metadata only, no molecule. */
export function renderNeighborAtom(activation: Activation, index: number): string {
  const neighbor = activation.neighbors[index];
  if (neighbor === undefined) return "";

  const { node, requires, loadNext } = neighbor;
  const edgeStr = activation.directEdges
    .filter((edge: GraphEdge) => edge.from === node.id)
    .map((edge: GraphEdge) => `${edge.type}→${edge.target}`)
    .join(", ");

  return [
    `[CONCEPT: ${node.id}]`,
    `ATOM: ${node.atom}`,
    `LAYER: ${node.layer} | SOURCE: ${node.domain[0] ?? "unknown"} | CONFIDENCE: ${node.confidence}`,
    `EDGES: ${edgeStr || "(none)"}`,
    `REQUIRES: [${requires.join(", ")}]`,
    `LOAD-NEXT: [${loadNext.join(", ")}]`,
  ].join(NL);
}

/** The complete `activate` transcript: header line, concept block, neighbours. */
export function renderActivation(activation: Activation): string {
  const parts: string[] = [];
  if (activation.strategy !== null) {
    parts.push(`[Strategy: ${activation.strategy}]`);
  }
  parts.push(renderConceptAi(activation));

  if (activation.neighbors.length > 0) {
    const label =
      activation.strategy === null
        ? `${activation.depth}-hop neighbors`
        : `${activation.strategy} activation (${activation.visited.length} concepts)`;
    parts.push(`\n--- ${label} ---\n`);
    for (let i = 0; i < activation.neighbors.length; i++) {
      parts.push(renderNeighborAtom(activation, i));
      parts.push("");
    }
  }

  return parts.join(NL) + NL;
}

/** Grouped edge listing, as printed by `edges`. */
export function renderEdges(conceptId: string, hits: EdgeHit[]): string {
  if (hits.length === 0) {
    return `No matching edges for "${conceptId}".${NL}`;
  }

  const groups = new Map<string, EdgeHit[]>();
  for (const hit of hits) {
    const mark = hit.direction === "in" ? "←" : hit.direction === "out" ? "→" : "↔";
    const key = `${mark} ${hit.type}`;
    const bucket = groups.get(key);
    if (bucket) bucket.push(hit);
    else groups.set(key, [hit]);
  }

  const width = Math.max(...hits.map((hit) => hit.other.id.length));
  const lines: string[] = [];
  for (const [label, group] of groups) {
    lines.push(`${label} (${group.length}):`);
    for (const hit of group) {
      const mark = hit.derived ? " (derived)" : "";
      lines.push(
        `  ${hit.other.id.padEnd(width)}  L${hit.other.layer}  ${hit.confidence.padEnd(12)} ${hit.other.atom}${mark}`,
      );
    }
  }
  return lines.join(NL) + NL;
}

/** Search results in the compact `id (layer, confidence) atom` form. */
export function renderSearchResults(keyword: string, rows: ManifestRow[]): string {
  if (rows.length === 0) {
    return `No concepts found matching "${keyword}".${NL}`;
  }

  const parts: string[] = [];
  for (const row of rows) {
    parts.push(`${row.id}  (L${row.layer}, ${row.confidence})  ${row.atom}`);
    parts.push(`  triggers: ${row.triggers.join(", ")}`);
    parts.push("");
  }
  return parts.join(NL) + NL;
}

/** Path result, one line per node. */
export function renderPath(result: PathResult): string {
  if (result.path.length === 0) {
    return `No path found between "${result.from}" and "${result.to}".${NL}`;
  }

  const lines = [`Path from ${result.from} to ${result.to} (${result.path.length} nodes):`];
  for (const [index, node] of result.nodes.entries()) {
    lines.push(`  ${index + 1}. ${node.id}: ${node.atom}`);
  }
  return lines.join(NL) + NL;
}

/** `viz stats` summary plus the meta block. */
export function renderVizStats(payload: VizPayload): string {
  const { meta } = payload;
  return (
    [
      `concepts: ${meta.conceptCount}`,
      `edges:    ${payload.edges.length} (${meta.assertedEdgeCount} asserted, ${payload.edges.length - meta.assertedEdgeCount} mirrored)`,
      `layers:   ${Object.entries(meta.layerCounts).map(([layer, count]) => `${layer}=${count}`).join(" ")}`,
      `types:    ${meta.edgeTypes.join(", ")}`,
      `generated: ${meta.generatedAt}`,
    ].join(NL) + NL
  );
}

/** Per-file validation report plus the optional graph section. */
export function renderValidationReport(report: ConceptValidationReport, kbRoot: string): string {
  const lines = [`Validating concepts in ${kbRoot}...`, ""];

  for (const file of report.files) {
    if (file.valid) {
      lines.push(`✓ ${file.filePath}`);
    } else {
      lines.push(`✗ ${file.filePath}`);
      lines.push(`  Error: ${file.error ?? "unknown"}`);
    }
  }
  lines.push("");
  lines.push(`${report.total - report.failed}/${report.total} concepts valid. ${report.failed} errors.`);

  if (report.graph !== undefined) {
    lines.push("");
    // The section already ends with a newline; splitting without dropping the
    // empty tail would add a second one.
    const section = renderGraphSection(report.graph);
    lines.push(...section.slice(0, -1).split(NL));
  }

  return lines.join(NL) + NL;
}

/**
 * `graph validate` output: the integrity section without the file list.
 *
 * On failure the header line is included so the reason is visible; on success
 * only the verdict is printed, matching what the command has always emitted.
 */
export function renderGraphStatus(report: GraphValidationReport): string {
  const body = renderGraphSection(report);
  const clean =
    report.valid && (report.staleness === null || (report.staleness.missing.length === 0 && report.staleness.extra.length === 0 && !report.staleness.nodeDrift));

  if (clean) return "✓ Graph integrity: no issues found.\n";
  // Strip the "--- Graph integrity ---" banner: the command name already says it.
  return body.replace(/^--- Graph integrity ---\n\n/, "");
}

/** Only the graph half of a validation report (`graph validate`). */
export function renderGraphSection(report: GraphValidationReport): string {
  const lines: string[] = ["--- Graph integrity ---", ""];
  const { staleness, issues } = report;

  if (staleness?.error !== undefined) {
    // The graph could not be rebuilt, so nothing below is trustworthy: the
    // issues were computed against a graph.json the sources no longer describe.
    lines.push(`✗ [unbuildable] ${staleness.error}`);
    lines.push("");
    lines.push("Graph checks skipped: concept files must parse first.");
    return lines.join(NL) + NL;
  }

  if (staleness === null || (staleness.missing.length === 0 && staleness.extra.length === 0 && !staleness.nodeDrift)) {
    lines.push("✓ graph.json is current.");
  } else {
    lines.push("✗ [stale-graph] graph.json does not match the concept files.");
    for (const key of staleness.missing) lines.push(`  missing from graph.json: ${key}`);
    for (const key of staleness.extra) lines.push(`  not in concept files:    ${key}`);
    if (staleness.nodeDrift) lines.push("  concept set differs");
  }

  if (issues.length === 0) {
    lines.push("✓ Graph integrity: no issues found.");
  } else {
    for (const issue of issues) {
      lines.push(`✗ [${issue.type}] ${issue.message}`);
      if (issue.details.from !== undefined) {
        lines.push(
          `  from: ${issue.details.from}, target: ${issue.details.target}, edgeType: ${issue.details.edgeType ?? "n/a"}`,
        );
      } else {
        lines.push(`  target: ${issue.details.target}`);
      }
    }
    lines.push("");
    lines.push(`${issues.length} graph issues found.`);
  }

  return lines.join(NL) + NL;
}

/** `proposals list` output. */
export function renderProposals(lists: ProposalLists): string {
  const lines: string[] = [];

  if (lists.edges.length > 0) {
    lines.push("Edge proposals:");
    for (const proposal of lists.edges) {
      lines.push(
        `  ${proposal.status.padEnd(8)} ${proposal.from} --${proposal.type}--> ${proposal.target} (${proposal.confidence})  [${proposal.file}]`,
      );
    }
  }

  if (lists.atoms.length > 0) {
    lines.push("Atom proposals:");
    for (const proposal of lists.atoms) {
      lines.push(
        `  ${proposal.status.padEnd(8)} ${proposal.conceptId}: ${proposal.proposedAtom}  [${proposal.file}]`,
      );
    }
  }

  if (lists.bodies.length > 0) {
    lines.push("Body proposals:");
    for (const proposal of lists.bodies) {
      // Length, not content: a body is the whole document and echoing it here
      // would bury the list it belongs to. `body show` prints the candidate.
      lines.push(
        `  ${proposal.status.padEnd(8)} ${proposal.conceptId}: ${proposal.proposedBody.length} chars, ${proposal.reason || "(no reason)"}  [${proposal.file}]`,
      );
    }
  }

  if (lists.edges.length === 0 && lists.atoms.length === 0 && lists.bodies.length === 0) {
    lines.push("No proposals found.");
  }

  return lines.join(NL) + NL;
}

/** Result of a `proposals merge` run. */
export function renderMergeOutcome(outcome: MergeOutcome): string {
  const lines: string[] = [];
  for (const file of outcome.merged) lines.push(`✓ Merged: ${file}`);
  for (const conflict of outcome.conflicts) lines.push(`! Conflict ${conflict.file}: ${conflict.error}`);
  for (const failure of outcome.failed) lines.push(`✗ Failed ${failure.file}: ${failure.error}`);

  if (outcome.merged.length === 0 && outcome.failed.length === 0 && outcome.conflicts.length === 0) {
    lines.push("No pending proposals to merge.");
    return lines.join(NL) + NL;
  }

  lines.push("");
  // A body-only merge cannot change graph.json or MANIFEST.md: both are built
  // from frontmatter. Only ask for a rebuild when something actually did.
  lines.push(
    outcome.derivedStale
      ? `Merged ${outcome.merged.length}. Run 'x-sage graph generate --manifest' to update graph.json and MANIFEST.md.`
      : `Merged ${outcome.merged.length}. graph.json and MANIFEST.md are unchanged (body-only).`,
  );
  if (outcome.conflicts.length > 0) {
    // The proposal is still reviewable; the caller has to re-read and resubmit.
    lines.push(
      `${outcome.conflicts.length} proposal(s) conflict with the current concept. Re-read it with 'x-sage concept body <id>' and submit again.`,
    );
  }
  return lines.join(NL) + NL;
}

/** Frontier entries: a JSON array, or a note when empty. */
export function renderFrontier(entries: FrontierEntry[]): string {
  return entries.length === 0 ? `No frontier entries.${NL}` : JSON.stringify(entries, null, 2) + NL;
}

/** Checkpoint bodies are JSON by contract. */
export function renderCheckpoint(checkpoint: Checkpoint): string {
  return JSON.stringify(checkpoint, null, 2) + NL;
}

/** JSON helper that always ends with a newline, for uniform stdout. */
export function renderJson(value: unknown): string {
  return JSON.stringify(value, null, 2) + NL;
}
