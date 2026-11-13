import { readFile } from "node:fs/promises";
import { join } from "node:path";
import matter from "gray-matter";
import { formatConceptFile, parseConceptFile } from "../manifest/parse.ts";
import { EdgeSchema } from "../types/index.ts";
import { writeFileAtomic } from "../fs/atomic.ts";
import { assertProposalFilename, resolveConceptPath } from "./paths.ts";
import { AlreadyMergedError, ProposalConflictError } from "./errors.ts";

/** A proposal that is not `pending` must not be applied. */
function assertPending(file: string, status: unknown): void {
  if (status === "approved") throw new AlreadyMergedError(file);
  if (status !== "pending") {
    throw new Error(`Proposal ${file} has status ${JSON.stringify(status)}, expected "pending"`);
  }
}

async function loadConcept(kbPath: string, conceptId: string): Promise<{
  filePath: string;
  raw: string;
}> {
  const filePath = await resolveConceptPath(kbPath, conceptId);
  if (filePath === null) throw new Error(`Concept not found: ${conceptId}`);
  return { filePath, raw: await readFile(filePath, "utf-8") };
}

export async function mergeEdgeProposal(kbPath: string, proposalFile: string): Promise<void> {
  const file = assertProposalFilename(proposalFile);
  const proposalPath = join(kbPath, "proposals", "edges", file);
  const raw = await readFile(proposalPath, "utf-8");
  const { data, content } = matter(raw);

  // Merging is not atomic across the two files it touches (concept + proposal).
  // If a crash lands between the writes, the retry sees `approved` and must not
  // append the edge a second time — that is how a duplicate edge used to appear.
  assertPending(file, data.status);

  const fromMatch = content.match(/- \*\*from\*\*: (.+)/);
  const typeMatch = content.match(/- \*\*type\*\*: (.+)/);
  const targetMatch = content.match(/- \*\*target\*\*: (.+)/);
  const confidenceMatch = content.match(/- \*\*confidence\*\*: (.+)/);

  const parsed = EdgeSchema.safeParse({
    type: typeMatch?.[1] ?? "",
    target: targetMatch?.[1] ?? "",
    confidence: confidenceMatch?.[1] ?? "confirmed",
  });
  if (!parsed.success) {
    // Writing an unparsed edge corrupts the concept file: the file is rewritten
    // before anyone notices, and every later parse of it fails.
    const detail = parsed.error.issues
      .map((issue) => `${issue.path.join(".")}: ${issue.message}`)
      .join("; ");
    throw new Error(`Edge proposal ${file} is malformed: ${detail}`);
  }

  const from = fromMatch?.[1]?.trim() ?? "";
  if (from === "") throw new Error(`Edge proposal ${file} has no "from" concept`);

  const { filePath, raw: conceptRaw } = await loadConcept(kbPath, from);
  const concept = parseConceptFile(conceptRaw, filePath);

  // An edge asserted twice is a duplicate, not a second relationship. Checking
  // here also makes a re-run after a partial failure harmless.
  const exists = concept.frontmatter.edges.some(
    (edge) => edge.type === parsed.data.type && edge.target === parsed.data.target,
  );
  if (!exists) {
    concept.frontmatter.edges.push(parsed.data);
    await writeFileAtomic(filePath, formatConceptFile(concept));
  }

  await writeFileAtomic(proposalPath, matter.stringify(content, { ...data, status: "approved" }));
}

/** The atom text between two headings of an atom proposal. */
function proposalAtom(content: string): string {
  const match = content.match(/## 建议新 atom\s*\n([\s\S]*?)(?=\n## |$)/);
  return match?.[1]?.trim() ?? "";
}

/** The atom the proposal was written against, used as its baseline. */
function proposalBaselineAtom(content: string): string {
  const match = content.match(/## 当前 atom\s*\n([\s\S]*?)(?=\n## |$)/);
  return match?.[1]?.trim() ?? "";
}

export async function mergeAtomProposal(kbPath: string, proposalFile: string): Promise<void> {
  const file = assertProposalFilename(proposalFile);
  const proposalPath = join(kbPath, "proposals", "atoms", file);
  const raw = await readFile(proposalPath, "utf-8");
  const { data, content } = matter(raw);

  assertPending(file, data.status);

  const conceptId = typeof data.concept === "string" ? data.concept : "";
  if (conceptId === "") throw new Error(`Atom proposal ${file} has no "concept"`);

  const proposedAtom = proposalAtom(content);
  if (proposedAtom === "") throw new Error(`Atom proposal ${file} has no proposed atom`);
  if (proposedAtom.length > 200) {
    throw new Error(`Atom proposal ${file} proposes ${proposedAtom.length} characters, limit is 200`);
  }

  const { filePath, raw: conceptRaw } = await loadConcept(kbPath, conceptId);
  const concept = parseConceptFile(conceptRaw, filePath);

  // The baseline is the atom the proposal quotes. If the live concept has moved
  // on, applying this proposal would silently discard whatever changed the atom
  // in between — the exact lost update the revision scheme exists to prevent.
  const baseline = proposalBaselineAtom(content);
  if (baseline !== "" && baseline !== concept.frontmatter.atom) {
    throw new ProposalConflictError(
      `Atom proposal ${file} was written against a different atom for ${conceptId}`,
      file,
    );
  }

  concept.frontmatter.atom = proposedAtom;
  await writeFileAtomic(filePath, formatConceptFile(concept));

  await writeFileAtomic(proposalPath, matter.stringify(content, { ...data, status: "approved" }));
}