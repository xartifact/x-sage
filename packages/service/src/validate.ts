import { validateGraph, type Graph } from "@x-sage/core";
import { loadConcepts } from "./concepts.ts";
import { validateGraphState, type GraphValidationReport } from "./graph.ts";
import type { KbContext } from "./kb.ts";

export interface ConceptValidationReport {
  /** One entry per concept file, in layer then filename order. */
  files: Array<{ filePath: string; valid: boolean; error?: string }>;
  total: number;
  failed: number;
  /** Present only when graph checks were requested. */
  graph?: GraphValidationReport;
  /** True when every file parses and, if requested, the graph is clean. */
  valid: boolean;
}

/**
 * Validate concept files, optionally including graph integrity.
 *
 * Unlike `loadConcepts`, this collects every failure instead of throwing on the
 * first one: a validation report that stops at the first bad file is useless.
 */
export async function validateConcepts(
  ctx: KbContext,
  options: { graph?: boolean } = {},
): Promise<ConceptValidationReport> {
  const { concepts, errors } = await loadConcepts(ctx, { tolerateErrors: true });

  const errorByPath = new Map(errors.map((error) => [error.filePath, error.message]));
  const filePaths = [
    ...concepts.map((concept) => concept.filePath),
    ...errors.map((error) => error.filePath),
  ].sort();

  const files = filePaths.map((filePath) => {
    const error = errorByPath.get(filePath);
    return error === undefined ? { filePath, valid: true } : { filePath, valid: false, error };
  });

  const report: ConceptValidationReport = {
    files,
    total: files.length,
    failed: errors.length,
    valid: errors.length === 0,
  };

  if (options.graph === true) {
    const graph = await validateGraphState(ctx);
    report.graph = graph;
    report.valid = report.valid && graph.valid;
  }

  return report;
}

/** Validate current database rows. There is no generated graph artifact to compare. */
export function validateGraphSnapshot(graph: Graph, includeGraph: boolean): ConceptValidationReport {
  const files = graph.nodes.map(({ filePath }) => filePath).sort().map((filePath) => ({ filePath, valid: true }));
  const report: ConceptValidationReport = { files, total: files.length, failed: 0, valid: true };
  if (includeGraph) {
    const { issues } = validateGraph(graph);
    report.graph = { issues, staleness: { missing: [], extra: [], nodeDrift: false }, valid: issues.length === 0 };
    report.valid = report.graph.valid;
  }
  return report;
}
