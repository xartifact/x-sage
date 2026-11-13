import { mkdir, readdir, readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { randomBytes } from "node:crypto";
import matter from "gray-matter";
import {
  conceptRevision,
  isBlankBody,
  MAX_BODY_CHARS,
  normalizeBody,
  splitConceptDocument,
} from "../manifest/body.ts";
import { parseConceptFile } from "../manifest/parse.ts";
import { ConceptFrontmatterSchema, ConceptIdSchema, type ConceptFrontmatter } from "../types/index.ts";
import { writeFileAtomic } from "../fs/atomic.ts";
import { joinConceptDocument } from "../manifest/body.ts";
import { resolveConceptPath } from "./paths.ts";
import { AlreadyMergedError, ProposalConflictError } from "./errors.ts";

/**
 * Body proposals are JSON files, so the `.md`-oriented guard in `paths.ts` does
 * not apply. Same intent: the name arrives from a request body and is joined
 * onto a directory.
 */
/** `proposals/` is `<kb>/proposals`, so the KB root is one level up. */
function kbPathOf(proposalsDir: string): string {
  return dirname(proposalsDir);
}

function assertBodyProposalFilename(filename: string): string {
  if (!/^[A-Za-z0-9._-]+\.json$/.test(filename) || filename.startsWith(".") || filename.includes("..")) {
    throw new Error(`Invalid body proposal filename: ${JSON.stringify(filename)}`);
  }
  return filename;
}

/**
 * A body proposal: a candidate replacement for everything after the frontmatter.
 *
 * Stored as JSON rather than "frontmatter + prose": a body that opens with a
 * horizontal rule (`---`) is valid Markdown, and a gray-matter envelope would
 * parse it as metadata, so the candidate would come back truncated.
 */
export interface BodyProposal {
  /** Filename under `proposals/bodies/`; also the merge target. */
  file: string;
  conceptId: string;
  /** Revision the proposer read; merging refuses if the concept moved on. */
  expectedRevision: string;
  proposedBody: string;
  reason: string;
  date: string;
  status: "pending" | "approved";
}

export interface BodyReadback {
  conceptId: string;
  /** Everything after the frontmatter, verbatim. */
  body: string;
  revision: string;
  frontmatter: ConceptFrontmatter;
  /** The atom, surfaced because most body edits are correcting it. */
  atom: string;
}

/**
 * Read one concept body plus its revision.
 *
 * Deliberately not built on `activate`: that assembles the semantic closure and
 * every neighbour — a whole graph build — only to hand back one file's text, and
 * its `markdown` field is the entire document including frontmatter.
 */
export async function readConceptBody(kbPath: string, conceptId: string): Promise<BodyReadback> {
  const filePath = await resolveConceptPath(kbPath, conceptId);
  if (filePath === null) throw new Error(`Concept not found: ${conceptId}`);

  const raw = await readFile(filePath, "utf-8");
  const { prefix, body } = splitConceptDocument(raw);
  const frontmatter = ConceptFrontmatterSchema.parse(matter(prefix).data);

  return {
    conceptId: frontmatter.id,
    body,
    revision: conceptRevision(frontmatter, body),
    frontmatter,
    atom: frontmatter.atom,
  };
}

/**
 * Validate a candidate body against the limits a merge will enforce.
 *
 * The check that matters is that the reassembled document still parses as a
 * concept: that is the format baseline, and it catches a body that would break
 * the file for every later reader. No additional shape is imposed — a body may
 * open with a code block, a list or a horizontal rule.
 */
export function assertUsableBody(body: string, prefix: string): void {
  if (isBlankBody(body)) throw new Error("Proposed body is empty");
  if (body.length > MAX_BODY_CHARS) {
    throw new Error(`Proposed body is ${body.length} characters, limit is ${MAX_BODY_CHARS}`);
  }

  const candidate = `${prefix}${body}`;
  const reparsed = parseConceptFile(candidate, "candidate.md");
  if (reparsed.molecule === "") {
    throw new Error("Proposed body has no content after the frontmatter");
  }
}

/**
 * Persist a body proposal.
 *
 * The candidate is stored as JSON under `proposals/bodies/` with the revision it
 * was written against, so merging can detect that the concept moved on and the
 * proposal stays reviewable instead of being applied blindly.
 */
export async function proposeBody(
  proposalsDir: string,
  params: {
    conceptId: string;
    expectedRevision: string;
    proposedBody: string;
    reason: string;
  },
): Promise<string> {
  const parsed = ConceptIdSchema.safeParse(params.conceptId);
  if (!parsed.success) {
    throw new Error(`Invalid concept id: ${JSON.stringify(params.conceptId)}. Must be a lowercase slug [a-z0-9-]`);
  }
  if (params.reason.trim() === "") throw new Error("A body proposal needs a reason");

  // Check the baseline on the way in as well as on the way out. A proposal
  // written against text that is already gone would be reviewed (wasting the
  // reviewer's time) and then refused at merge; refusing now says so while the
  // author still has the file open.
  const live = await readConceptBody(kbPathOf(proposalsDir), params.conceptId);
  if (live.revision !== params.expectedRevision) {
    throw new ProposalConflictError(
      `Concept ${params.conceptId} is at revision ${live.revision.slice(0, 12)}, not ${params.expectedRevision.slice(0, 12)}. Re-read it and resubmit.`,
      "",
    );
  }

  const candidate = normalizeBody(params.proposedBody);
  const raw = await readFile((await resolveConceptPath(kbPathOf(proposalsDir), params.conceptId))!, "utf-8");
  assertUsableBody(candidate, splitConceptDocument(raw).prefix);

  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  const suffix = randomBytes(3).toString("hex");
  const filename = `${stamp}-${params.conceptId}-body-${suffix}.json`;
  const dir = join(proposalsDir, "bodies");
  await mkdir(dir, { recursive: true });

  const proposal: BodyProposal = {
    file: filename,
    conceptId: params.conceptId,
    expectedRevision: params.expectedRevision,
    // Stored normalised: a merge applies exactly these bytes, and a read-back
    // then matches the candidate the reviewer was shown.
    proposedBody: candidate,
    reason: params.reason,
    date: stamp,
    status: "pending",
  };

  await writeFileAtomic(join(dir, filename), `${JSON.stringify(proposal, null, 2)}\n`);
  return filename;
}

/** Read one body proposal, refusing names that leave the directory. */
export async function readBodyProposal(proposalsDir: string, filename: string): Promise<BodyProposal> {
  assertBodyProposalFilename(filename);
  const raw = await readFile(join(proposalsDir, "bodies", filename), "utf-8");
  const parsed = JSON.parse(raw) as Omit<BodyProposal, "file">;
  if (typeof parsed.conceptId !== "string" || typeof parsed.proposedBody !== "string") {
    throw new Error(`Body proposal ${filename} is malformed`);
  }
  // `file` is the identity on disk, never a value the JSON can disagree with.
  return { ...parsed, file: filename };
}

/** `proposals/bodies` listing, in filename order. */
export async function listBodyProposals(proposalsDir: string): Promise<BodyProposal[]> {
  const dir = join(proposalsDir, "bodies");
  let entries: string[];
  try {
    entries = await readdir(dir);
  } catch {
    return [];
  }

  const out: BodyProposal[] = [];
  for (const file of entries.sort()) {
    if (!file.endsWith(".json")) continue;
    try {
      out.push(await readBodyProposal(proposalsDir, file));
    } catch {
      // A corrupted proposal must not hide the others from `proposals list`.
      out.push({
        file, conceptId: "<unreadable>", expectedRevision: "", proposedBody: "",
        reason: file, date: "", status: "pending",
      });
    }
  }
  return out;
}

/**
 * Merge one body proposal into its concept.
 *
 * Order matters and is deliberate:
 * 1. already applied? The body on disk equals the candidate → mark approved and
 *    return. A crash between the concept write and the proposal write leaves
 *    exactly this state, and the revision has already moved by then.
 * 2. revision mismatch → conflict; the proposal is untouched and reviewable.
 * 3. otherwise validate, write the concept, then mark the proposal approved.
 *
 * The write replaces only the bytes after the frontmatter, so metadata is never
 * round-tripped through the parser.
 */
export async function mergeBodyProposal(kbPath: string, filename: string): Promise<void> {
  const proposalsDir = join(kbPath, "proposals");
  const proposalPath = join(proposalsDir, "bodies", filename);
  assertBodyProposalFilename(filename);

  const proposal = await readBodyProposal(proposalsDir, filename);
  if (proposal.status === "approved") throw new AlreadyMergedError(filename);

  const conceptPath = await resolveConceptPath(kbPath, proposal.conceptId);
  if (conceptPath === null) throw new Error(`Concept not found: ${proposal.conceptId}`);

  const raw = await readFile(conceptPath, "utf-8");
  const { prefix, body } = splitConceptDocument(raw);
  const frontmatter = ConceptFrontmatterSchema.parse(matter(prefix).data);
  const candidate = normalizeBody(proposal.proposedBody);

  if (normalizeBody(body) === candidate) {
    await writeFileAtomic(
      proposalPath,
      `${JSON.stringify({ ...proposal, status: "approved" }, null, 2)}\n`,
    );
    throw new AlreadyMergedError(filename);
  }

  const current = conceptRevision(frontmatter, body);
  if (proposal.expectedRevision !== current) {
    throw new ProposalConflictError(
      `Body proposal ${filename} was written against revision ${proposal.expectedRevision.slice(0, 12)}, current is ${current.slice(0, 12)}`,
      filename,
    );
  }

  assertUsableBody(candidate, prefix);
  await writeFileAtomic(conceptPath, joinConceptDocument(prefix, candidate));
  await writeFileAtomic(
    proposalPath,
    `${JSON.stringify({ ...proposal, status: "approved" }, null, 2)}\n`,
  );
}
