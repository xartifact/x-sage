import { mkdir, open, unlink, stat } from "node:fs/promises";
import { dirname, join } from "node:path";

/**
 * A cross-process mutex built on an exclusive lock file.
 *
 * The KB is a directory of files, and two clients merging proposals at the same
 * time would both read-modify-write the same concept file; the later write wins
 * and silently discards the other. A lock file created with `O_EXCL` is atomic
 * on every filesystem we care about, so exactly one holder proceeds.
 *
 * Staleness: a crashed process never releases its lock, so a lock older than
 * `staleAfterMs` is reclaimed. This is a heuristic — the retry is safe because
 * the operations under the lock are themselves idempotent (see merge.ts).
 */
export interface LockOptions {
  /** Give up after this long. Defaults to 10s. */
  timeoutMs?: number;
  /** Treat a lock file older than this as abandoned. Defaults to 30s. */
  staleAfterMs?: number;
  /** Poll interval. Defaults to 50ms. */
  retryMs?: number;
}

export class LockTimeoutError extends Error {
  constructor(lockPath: string, timeoutMs: number) {
    super(`Timed out after ${timeoutMs}ms waiting for lock: ${lockPath}`);
  }
}

async function tryAcquire(lockPath: string, staleAfterMs: number): Promise<boolean> {
  try {
    const handle = await open(lockPath, "wx");
    await handle.writeFile(`${process.pid}\n${Date.now()}\n`, "utf-8");
    await handle.close();
    return true;
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code !== "EEXIST") throw err;

    // Someone holds it — or a previous holder died without cleaning up.
    try {
      const info = await stat(lockPath);
      if (Date.now() - info.mtimeMs > staleAfterMs) {
        await unlink(lockPath).catch(() => {});
      }
    } catch {
      // Vanished between open and stat; the next attempt wins.
    }
    return false;
  }
}

/**
 * Run `fn` while holding the KB write lock.
 *
 * Serialises mutations only; reads never take the lock.
 */
export async function withKbLock<T>(
  kbPath: string,
  fn: () => Promise<T>,
  options: LockOptions = {},
): Promise<T> {
  const { timeoutMs = 10_000, staleAfterMs = 30_000, retryMs = 50 } = options;
  const lockPath = join(kbPath, ".write.lock");

  await mkdir(dirname(lockPath), { recursive: true });

  const deadline = Date.now() + timeoutMs;
  while (true) {
    if (await tryAcquire(lockPath, staleAfterMs)) break;
    if (Date.now() >= deadline) throw new LockTimeoutError(lockPath, timeoutMs);
    const { promise, resolve } = Promise.withResolvers<void>();
    setTimeout(resolve, retryMs);
    await promise;
  }

  try {
    return await fn();
  } finally {
    await unlink(lockPath).catch(() => {});
  }
}
