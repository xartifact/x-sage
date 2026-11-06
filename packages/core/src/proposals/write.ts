import { writeFile, mkdir } from "node:fs/promises";
import { join } from "node:path";
import type { EdgeType, Confidence } from "../types/index.ts";

export async function proposeEdge(
  proposalsDir: string,
  params: {
    from: string;
    type: EdgeType;
    target: string;
    confidence: Confidence;
  }
): Promise<string> {
  const date = new Date().toISOString().split("T")[0]!;
  const filename = `${date}-${params.from}-${params.type}-${params.target}.md`;
  const edgesDir = join(proposalsDir, "edges");
  const filePath = join(edgesDir, filename);

  await mkdir(edgesDir, { recursive: true });

  const content = `---
proposer: CLI
date: ${date}
status: pending
---
## 新边提案

- **from**: ${params.from}
- **type**: ${params.type}
- **target**: ${params.target}
- **confidence**: ${params.confidence}

## 理由
(待填写)
`;

  await writeFile(filePath, content, "utf-8");
  return filename;
}

export async function refineAtom(
  proposalsDir: string,
  params: {
    conceptId: string;
    currentAtom: string;
    proposedAtom: string;
    reason: string;
  }
): Promise<string> {
  const date = new Date().toISOString().split("T")[0]!;
  const filename = `${params.conceptId}-${date}.md`;
  const atomsDir = join(proposalsDir, "atoms");
  const filePath = join(atomsDir, filename);

  await mkdir(atomsDir, { recursive: true });

  const content = `---
proposer: CLI
date: ${date}
concept: ${params.conceptId}
status: pending
---
## 当前 atom
${params.currentAtom}

## 建议新 atom
${params.proposedAtom}

## 改进理由
${params.reason}
`;

  await writeFile(filePath, content, "utf-8");
  return filename;
}
