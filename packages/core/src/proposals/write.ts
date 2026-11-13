import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { writeFileAtomic } from "../fs/atomic.ts";
import { ConceptIdSchema, EdgeSchema, type Confidence, type EdgeType } from "../types/index.ts";

/**
 * A filename that cannot collide with another proposal made the same day.
 *
 * The old `<concept>-<date>.md` scheme overwrote an earlier proposal from the
 * same day: two `refine-atom risk` calls both returned 201 and only the second
 * survived, so the first review never happened. The timestamp plus a random
 * suffix removes that race without a listing-then-claim loop.
 */
function proposalName(subject: string): string {
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const suffix = randomBytes(3).toString("hex");
  return `${stamp}-${subject}-${suffix}.md`;
}

/** Reject an id that would escape the proposals directory as a filename fragment. */
function assertIdFragment(id: string): string {
  const parsed = ConceptIdSchema.safeParse(id);
  if (!parsed.success) {
    throw new Error(`Invalid concept id: ${JSON.stringify(id)}. Must be a lowercase slug [a-z0-9-]`);
  }
  return parsed.data;
}

export async function proposeEdge(
  proposalsDir: string,
  params: {
    from: string;
    type: EdgeType;
    target: string;
    confidence: Confidence;
  },
): Promise<string> {
  const from = assertIdFragment(params.from);
  const target = assertIdFragment(params.target);

  // A malformed edge is rejected here rather than at merge time: the proposal
  // is the review artifact, and an edge nobody can apply is not a proposal.
  const edge = EdgeSchema.parse({ type: params.type, target, confidence: params.confidence });

  const date = new Date().toISOString().split("T")[0]!;
  const filename = `${date}-${from}-${edge.type}-${edge.target}.md`;
  const edgesDir = join(proposalsDir, "edges");
  await mkdir(edgesDir, { recursive: true });

  const content = `---
proposer: CLI
date: ${date}
status: pending
---
## 新边提案

- **from**: ${from}
- **type**: ${edge.type}
- **target**: ${edge.target}
- **confidence**: ${edge.confidence}

## 理由
(待填写)
`;

  await writeFileAtomic(join(edgesDir, filename), content);
  return filename;
}

export async function refineAtom(
  proposalsDir: string,
  params: {
    conceptId: string;
    currentAtom: string;
    proposedAtom: string;
    reason: string;
  },
): Promise<string> {
  const conceptId = assertIdFragment(params.conceptId);
  const proposed = params.proposedAtom.trim();

  if (proposed === "") throw new Error("proposedAtom must not be empty");
  if (proposed.length > 200) {
    // The schema caps `atom` at 200 characters. Accepting a longer one here
    // only moved the failure to merge time, where it rewrote the concept file
    // with an atom no later parse would accept.
    throw new Error(`proposedAtom must be at most 200 characters (got ${proposed.length})`);
  }

  const date = new Date().toISOString().split("T")[0]!;
  const filename = proposalName(conceptId);
  const atomsDir = join(proposalsDir, "atoms");
  await mkdir(atomsDir, { recursive: true });

  const content = `---
proposer: CLI
date: ${date}
concept: ${conceptId}
status: pending
---
## 当前 atom
${params.currentAtom}

## 建议新 atom
${proposed}

## 改进理由
${params.reason}
`;

  await writeFileAtomic(join(atomsDir, filename), content);
  return filename;
}