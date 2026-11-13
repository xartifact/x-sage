import { readdir } from "node:fs/promises";
import { writeFileAtomic } from "@x-sage/core/fs";
import { withKbLock } from "@x-sage/core";
import {
  AlreadyMergedError,
  ProposalConflictError,
  ProposalTargetNotFoundError,
} from "@x-sage/core/proposals";
import {
  CheckpointSchema,
  readCheckpoint,
  writeCheckpoint,
  type Checkpoint,
} from "@x-sage/core";
import {
  addFrontierEntry as coreAddFrontierEntry,
  readFrontier,
  type FrontierEntry,
} from "@x-sage/core/frontier";
import {
  listAtomProposals,
  listBodyProposals,
  listEdgeProposals,
  mergeAtomProposal,
  mergeEdgeProposal,
  proposeEdge,
  mergeBodyProposal,
  proposeBody,
  readConceptBody,
  refineAtom,
  type AtomProposal,
  type BodyProposal,
  type BodyReadback,
  type EdgeProposal,
} from "@x-sage/core/proposals";
import { formatManifest, generateManifest } from "@x-sage/core/manifest";
import type { Confidence, EdgeType } from "@x-sage/core";
import { checkpointsDir, manifestFile, proposalsDir, type KbContext } from "./kb.ts";
import { findConceptFile } from "./concepts.ts";

/** Highest `-sN` suffix for a concept, or 0 when none exist. */
async function latestSession(ctx: KbContext, conceptId: string): Promise<number> {
  let latest = 0;
  try {
    const files = await readdir(checkpointsDir(ctx));
    const pattern = new RegExp(`^${conceptId}-s(\\d+)\\.json$`);
    for (const file of files) {
      const match = file.match(pattern);
      if (match) {
        const session = parseInt(match[1]!, 10);
        if (session > latest) latest = session;
      }
    }
  } catch {
    // checkpoints directory not created yet
  }
  return latest;
}

/** Create an empty checkpoint skeleton; the caller fills in the content. */
export async function createCheckpoint(
  ctx: KbContext,
  conceptId: string,
  session?: number,
): Promise<Checkpoint> {
  const effective = session ?? (await latestSession(ctx, conceptId)) + 1;
  const checkpoint: Checkpoint = {
    concept: conceptId,
    session: effective,
    date: new Date().toISOString().split("T")[0]!,
    established: [],
    exploredEdges: [],
    frontier: [],
    nextEntryPoint: "",
  };
  await writeCheckpoint(checkpointsDir(ctx), checkpoint);
  return checkpoint;
}

/** Read a checkpoint by session, or the latest when session is omitted. */
export async function getCheckpoint(
  ctx: KbContext,
  conceptId: string,
  session?: number,
): Promise<Checkpoint | null> {
  const target = session ?? (await latestSession(ctx, conceptId));
  if (target === 0) return null;
  return readCheckpoint(checkpointsDir(ctx), conceptId, target);
}

/** Persist a checkpoint as given, overwriting any file for that session. */
export async function saveCheckpoint(ctx: KbContext, checkpoint: Checkpoint): Promise<void> {
  await writeCheckpoint(checkpointsDir(ctx), CheckpointSchema.parse(checkpoint));
}

export async function listFrontier(ctx: KbContext): Promise<FrontierEntry[]> {
  return readFrontier(ctx.root);
}

/**
 * Append a frontier entry.
 *
 * Delegates to core, which re-serialises the whole file from the four-column
 * table it parsed — every other section in frontier.md is dropped. Callers that
 * need the prose sections preserved must edit the markdown directly.
 */
export async function appendFrontier(ctx: KbContext, entry: FrontierEntry): Promise<void> {
  await coreAddFrontierEntry(ctx.root, entry);
}

export interface ProposalLists {
  edges: EdgeProposal[];
  atoms: AtomProposal[];
  bodies: BodyProposal[];
}

export async function listProposals(ctx: KbContext): Promise<ProposalLists> {
  return {
    edges: await listEdgeProposals(proposalsDir(ctx)),
    atoms: await listAtomProposals(proposalsDir(ctx)),
    bodies: await listBodyProposals(proposalsDir(ctx)),
  };
}

export interface ProposeEdgeInput {
  from: string;
  type: EdgeType;
  target: string;
  confidence: Confidence;
}

/** Write an edge proposal file, returning its filename. */
export async function submitEdgeProposal(
  ctx: KbContext,
  input: ProposeEdgeInput,
): Promise<string> {
  return proposeEdge(proposalsDir(ctx), input);
}

export interface ProposeAtomInput {
  conceptId: string;
  proposedAtom: string;
  reason?: string;
}

/** Write an atom proposal file, returning its filename. */
export async function submitAtomProposal(
  ctx: KbContext,
  input: ProposeAtomInput,
): Promise<string> {
  const concept = await findConceptFile(ctx, input.conceptId);
  return refineAtom(proposalsDir(ctx), {
    conceptId: input.conceptId,
    currentAtom: concept.file.frontmatter.atom,
    proposedAtom: input.proposedAtom,
    reason: input.reason ?? "",
  });
}

/**
 * Result of merging, including the distinction a caller needs to choose an exit
 * code: `conflict` means the proposal was fine but its baseline moved, while
 * `failed` means it could not be applied at all.
 */
/** Read one concept body plus the revision a proposal must quote. */
export async function readBody(ctx: KbContext, conceptId: string): Promise<BodyReadback> {
  return readConceptBody(ctx.root, conceptId);
}

export interface ProposeBodyInput {
  conceptId: string;
  expectedRevision: string;
  proposedBody: string;
  reason: string;
}

/** Persist a body proposal, returning its filename. */
export async function submitBodyProposal(ctx: KbContext, input: ProposeBodyInput): Promise<string> {
  return proposeBody(proposalsDir(ctx), input);
}

export interface MergeOutcome {
  merged: string[];
  failed: Array<{ file: string; error: string }>;
  conflicts: Array<{ file: string; error: string }>;
  /**
   * Whether anything merged touched a frontmatter field.
   *
   * `graph.json` and `MANIFEST.md` are derived from frontmatter alone, so a
   * body-only merge leaves both byte-identical. Telling the caller to
   * regenerate anyway trains them to ignore the message.
   */
  derivedStale: boolean;
}

/**
 * Merge pending proposals back into concept files.
 *
 * `target` is a proposal filename or "all". Graph constraints are not re-checked
 * here, so callers must regenerate and validate afterwards.
 *
 * A named target that matches no proposal file is an error rather than an empty
 * success: the caller asked for a specific file and it was not there, which is
 * almost always a typo, and reporting "nothing to merge" hides it.
 */
export async function mergeProposals(ctx: KbContext, target: string): Promise<MergeOutcome> {
  // Two clients merging at once both read-modify-write the same concept file;
  // without serialisation the later write silently discards the earlier one.
  return withKbLock(ctx.root, () => mergeProposalsLocked(ctx, target));
}

async function mergeProposalsLocked(ctx: KbContext, target: string): Promise<MergeOutcome> {
  const outcome: MergeOutcome = { merged: [], failed: [], conflicts: [], derivedStale: false };
  const pending = await listProposals(ctx);

  const merge = async (
    file: string,
    run: (kbRoot: string, file: string) => Promise<void>,
    touchesFrontmatter: boolean,
  ): Promise<void> => {
    try {
      await run(ctx.root, file);
      outcome.merged.push(file);
      if (touchesFrontmatter) outcome.derivedStale = true;
    } catch (err) {
      // A retry after a partial failure must not be reported as an error.
      if (err instanceof AlreadyMergedError) {
        outcome.merged.push(file);
      } else if (err instanceof ProposalConflictError) {
        outcome.conflicts.push({ file, error: err.message });
      } else {
        outcome.failed.push({ file, error: err instanceof Error ? err.message : String(err) });
      }
    }
  };

  for (const proposal of pending.edges) {
    if (target !== "all" && proposal.file !== target) continue;
    if (target === "all" && proposal.status !== "pending") continue;
    await merge(proposal.file, mergeEdgeProposal, true);
  }

  for (const proposal of pending.atoms) {
    if (target !== "all" && proposal.file !== target) continue;
    if (target === "all" && proposal.status !== "pending") continue;
    await merge(proposal.file, mergeAtomProposal, true);
  }

  // Body proposals merge last. A batch that also updates an atom therefore sees
  // the post-atom revision and reports a conflict, instead of applying a body
  // written against the previous atom and silently reverting it.
  for (const proposal of pending.bodies) {
    if (target !== "all" && proposal.file !== target) continue;
    if (target === "all" && proposal.status !== "pending") continue;
    await merge(proposal.file, mergeBodyProposal, false);
  }

  if (target !== "all") {
    const known = [...pending.edges, ...pending.atoms, ...pending.bodies].some(
      (proposal) => proposal.file === target,
    );
    if (!known) throw new ProposalTargetNotFoundError(target);
  }

  return outcome;
}

/** Render the MANIFEST table without touching disk. */
export async function renderManifest(ctx: KbContext): Promise<string> {
  const rows = await generateManifest(ctx.root);
  return formatManifest(rows);
}

/** Regenerate `MANIFEST.md` and return the rendered markdown. */
export async function writeManifest(ctx: KbContext): Promise<{ markdown: string; count: number }> {
  const rows = await generateManifest(ctx.root);
  const markdown = formatManifest(rows);
  await writeFileAtomic(manifestFile(ctx), markdown);
  return { markdown, count: rows.length };
}

