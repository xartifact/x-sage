import { readFile } from "node:fs/promises";
import { writeFileAtomic } from "../fs/atomic.ts";
import { join } from "node:path";
import { CheckpointSchema, type Checkpoint } from "../types/index.ts";

/**
 * Read a checkpoint file for a concept.
 */
export async function readCheckpoint(checkpointsDir: string, conceptId: string, session: number): Promise<Checkpoint | null> {
  const filePath = join(checkpointsDir, `${conceptId}-s${session}.json`);
  try {
    const raw = await readFile(filePath, "utf-8");
    return CheckpointSchema.parse(JSON.parse(raw));
  } catch {
    return null;
  }
}

/**
 * Write a checkpoint file.
 */
export async function writeCheckpoint(checkpointsDir: string, checkpoint: Checkpoint): Promise<void> {
  const filePath = join(checkpointsDir, `${checkpoint.concept}-s${checkpoint.session}.json`);
  await writeFileAtomic(filePath, JSON.stringify(checkpoint, null, 2));
}
