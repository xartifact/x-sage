import { ProposalTargetNotFoundError } from "@x-sage/core/proposals";
import type { MergeOutcome } from "./write.ts";

const DIRECTORIES = ["edges", "atoms", "bodies"] as const;
type ProposalDirectory = typeof DIRECTORIES[number];

export interface ProposalMergeFile {
  directory: ProposalDirectory;
  file: string;
}

interface QueueEntry {
  readonly file: string;
  readonly status: string;
}

export type ProposalMergeDecision =
  | { kind: "applied" }
  | { kind: "alreadyApplied" }
  | { kind: "conflict"; message: string }
  | { kind: "failed"; message: string }
  | { kind: "unknown" };

/** List is selection only. Each decision resolves the effective filename on the current database basis. */
export interface ProposalMergePort {
  list(): Promise<{ readonly [D in ProposalDirectory]: readonly QueueEntry[] }>;
  /** Apply one proposal with its original per-kind policy, or report its definitive disposition. */
  decide(target: ProposalMergeFile): Promise<ProposalMergeDecision>;
}

export class ProposalMergeUnresolvedError extends Error {
  constructor(file: string, readonly partial: MergeOutcome) {
    super(`Database outcome unknown for proposal ${file}; resolve this decision before continuing`);
  }
}

/** Execute one legacy batch. A known refusal never undoes earlier approvals. */
export async function mergeProposalFiles(port: ProposalMergePort, target: string): Promise<MergeOutcome> {
  const outcome: MergeOutcome = { merged: [], failed: [], conflicts: [], derivedStale: false };
  const queue = await port.list();
  let matched = false;
  for (const directory of DIRECTORIES) {
    for (const { file, status } of queue[directory]) {
      if (target === "all" ? status !== "pending" : target !== file) continue;
      matched = true;
      // Await each decision so a body observes any earlier atom or edge change.
      let decision: ProposalMergeDecision;
      try {
        decision = await port.decide({ directory, file });
      } catch {
        throw new ProposalMergeUnresolvedError(file, outcome);
      }
      switch (decision.kind) {
        case "applied":
          outcome.merged.push(file);
          if (directory !== "bodies") outcome.derivedStale = true;
          break;
        case "alreadyApplied":
          outcome.merged.push(file);
          break;
        case "conflict":
          outcome.conflicts.push({ file, error: decision.message });
          break;
        case "failed":
          outcome.failed.push({ file, error: decision.message });
          break;
        case "unknown":
          throw new ProposalMergeUnresolvedError(file, outcome);
      }
    }
  }
  if (target !== "all" && !matched) throw new ProposalTargetNotFoundError(target);
  return outcome;
}
