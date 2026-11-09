import { readFile } from "node:fs/promises";
import { join } from "node:path";
import matter from "gray-matter";
import { parseConceptFile, formatConceptFile } from "../manifest/parse.ts";
import { EdgeSchema } from "../types/index.ts";
import { writeFileAtomic } from "../fs/atomic.ts";

async function findConceptFile(kbPath: string, conceptId: string): Promise<string> {
  const layers = ["L0-axioms", "L1-core", "L2-support", "L3-instances"];
  for (const layer of layers) {
    const filePath = join(kbPath, "concepts", layer, `${conceptId}.md`);
    try {
      await readFile(filePath, "utf-8");
      return filePath;
    } catch {
      // File doesn't exist in this layer, continue scanning
    }
  }
  throw new Error(`Concept file not found: ${conceptId}`);
}

/** Raised when a proposal was already merged; callers treat it as success. */
export class AlreadyMergedError extends Error {
  constructor(proposalFile: string) {
    super(`Proposal already merged: ${proposalFile}`);
  }
}

export async function mergeEdgeProposal(
  kbPath: string,
  proposalFile: string
): Promise<void> {
  const proposalPath = join(kbPath, "proposals", "edges", proposalFile);
  const raw = await readFile(proposalPath, "utf-8");
  const { data, content } = matter(raw);

  // Merging is not atomic across the two files it touches (concept + proposal).
  // If a crash lands between the writes, the retry sees `approved` and must not
  // append the edge a second time — that is how a duplicate edge used to appear.
  if (data.status === "approved") {
    throw new AlreadyMergedError(proposalFile);
  }

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
    throw new Error(`Edge proposal ${proposalFile} is malformed: ${detail}`);
  }

  const from = fromMatch?.[1]?.trim() ?? "";
  if (from === "") {
    throw new Error(`Edge proposal ${proposalFile} has no "from" concept`);
  }

  const conceptFilePath = await findConceptFile(kbPath, from);
  const conceptRaw = await readFile(conceptFilePath, "utf-8");
  const concept = parseConceptFile(conceptRaw, conceptFilePath);

  // An edge asserted twice is a duplicate, not a second relationship. Checking
  // here also makes a re-run after a partial failure harmless.
  const exists = concept.frontmatter.edges.some(
    (edge) => edge.type === parsed.data.type && edge.target === parsed.data.target,
  );
  if (!exists) {
    concept.frontmatter.edges.push(parsed.data);
    await writeFileAtomic(conceptFilePath, formatConceptFile(concept));
  }

  const approved = matter.stringify(content, { ...data, status: "approved" });
  await writeFileAtomic(proposalPath, approved);
}

export async function mergeAtomProposal(
  kbPath: string,
  proposalFile: string
): Promise<void> {
  const proposalPath = join(kbPath, "proposals", "atoms", proposalFile);
  const raw = await readFile(proposalPath, "utf-8");
  const { data, content } = matter(raw);

  if (data.status === "approved") {
    throw new AlreadyMergedError(proposalFile);
  }

  const conceptId = typeof data.concept === "string" ? data.concept : "";

  const proposedAtomMatch = content.match(/## 建议新 atom\s*\n(.+?)(?=\n## |$)/s);
  const proposedAtom = proposedAtomMatch && proposedAtomMatch.length > 1 ? proposedAtomMatch[1]!.trim() : "";

  const conceptFilePath = await findConceptFile(kbPath, conceptId);
  const conceptRaw = await readFile(conceptFilePath, "utf-8");
  const concept = parseConceptFile(conceptRaw, conceptFilePath);

  concept.frontmatter.atom = proposedAtom;
  await writeFileAtomic(conceptFilePath, formatConceptFile(concept));

  const approved = matter.stringify(content, { ...data, status: "approved" });
  await writeFileAtomic(proposalPath, approved);
}
