import { writeFile, rename, mkdir, unlink } from "node:fs/promises";
import { dirname, join } from "node:path";
import { randomBytes } from "node:crypto";

/**
 * Write a file atomically.
 *
 * A bare `writeFile` truncates the target before the new content exists, so a
 * crash or a full disk mid-write leaves a half-written concept file that no
 * longer parses — and the previous version is already gone. Writing to a
 * sibling temp file and renaming makes the swap atomic on POSIX: readers see
 * either the old file or the new one, never a partial one.
 *
 * The temp file is created in the *same directory* so the rename never crosses
 * a filesystem boundary (a cross-device rename is not atomic and fails on some
 * platforms).
 */
export async function writeFileAtomic(path: string, content: string): Promise<void> {
  const dir = dirname(path);
  await mkdir(dir, { recursive: true });

  const temp = join(dir, `.${randomBytes(6).toString("hex")}.tmp`);
  try {
    await writeFile(temp, content, "utf-8");
    await rename(temp, path);
  } catch (err) {
    // A leftover temp file would accumulate forever across failures.
    await unlink(temp).catch(() => {});
    throw err;
  }
}
