import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import matter from "gray-matter";

export interface EdgeProposal {
  file: string;
  from: string;
  type: string;
  target: string;
  confidence: string;
  status: string;
  date: string;
}

export interface AtomProposal {
  file: string;
  conceptId: string;
  currentAtom: string;
  proposedAtom: string;
  status: string;
  date: string;
}

export async function listEdgeProposals(proposalsDir: string): Promise<EdgeProposal[]> {
  const edgesDir = join(proposalsDir, "edges");
  const proposals: EdgeProposal[] = [];

  let entries: string[];
  try {
    entries = await readdir(edgesDir);
  } catch {
    return [];
  }

  for (const file of entries) {
    if (!file.endsWith(".md")) continue;
    const filePath = join(edgesDir, file);
    const raw = await readFile(filePath, "utf-8");
    const { data, content } = matter(raw);

    const fromMatch = content.match(/- \*\*from\*\*: (.+)/);
    const typeMatch = content.match(/- \*\*type\*\*: (.+)/);
    const targetMatch = content.match(/- \*\*target\*\*: (.+)/);
    const confidenceMatch = content.match(/- \*\*confidence\*\*: (.+)/);

    const from = fromMatch && fromMatch.length > 1 ? fromMatch[1]! : "";
    const type = typeMatch && typeMatch.length > 1 ? typeMatch[1]! : "";
    const target = targetMatch && targetMatch.length > 1 ? targetMatch[1]! : "";
    const confidence = confidenceMatch && confidenceMatch.length > 1 ? confidenceMatch[1]! : "";

    proposals.push({
      file,
      from,
      type,
      target,
      confidence,
      status: typeof data.status === "string" ? data.status : "",
      date: typeof data.date === "string" ? data.date : "",
    });
  }

  return proposals;
}

export async function listAtomProposals(proposalsDir: string): Promise<AtomProposal[]> {
  const atomsDir = join(proposalsDir, "atoms");
  const proposals: AtomProposal[] = [];

  let entries: string[];
  try {
    entries = await readdir(atomsDir);
  } catch {
    return [];
  }

  for (const file of entries) {
    if (!file.endsWith(".md")) continue;
    const filePath = join(atomsDir, file);
    const raw = await readFile(filePath, "utf-8");
    const { data, content } = matter(raw);

    const currentAtomMatch = content.match(/## 当前 atom\s*\n(.+?)(?=\n## |$)/s);
    const proposedAtomMatch = content.match(/## 建议新 atom\s*\n(.+?)(?=\n## |$)/s);

    const currentAtom = currentAtomMatch && currentAtomMatch.length > 1 ? currentAtomMatch[1]!.trim() : "";
    const proposedAtom = proposedAtomMatch && proposedAtomMatch.length > 1 ? proposedAtomMatch[1]!.trim() : "";

    proposals.push({
      file,
      conceptId: typeof data.concept === "string" ? data.concept : "",
      currentAtom,
      proposedAtom,
      status: typeof data.status === "string" ? data.status : "",
      date: typeof data.date === "string" ? data.date : "",
    });
  }

  return proposals;
}
