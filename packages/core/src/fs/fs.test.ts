import { describe, expect, test } from "bun:test";
import { mkdtemp, readFile, readdir, rm, utimes, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { writeFileAtomic } from "./atomic.ts";
import { withKbLock, LockTimeoutError } from "./lock.ts";

describe("writeFileAtomic", () => {
  test("writes the content", async () => {
    const root = await mkdtemp(join(tmpdir(), "x-sage-fs-"));
    try {
      const path = join(root, "a.txt");
      await writeFileAtomic(path, "hello");
      expect(await readFile(path, "utf-8")).toBe("hello");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("creates missing parent directories", async () => {
    const root = await mkdtemp(join(tmpdir(), "x-sage-fs-"));
    try {
      const path = join(root, "nested", "deep", "a.txt");
      await writeFileAtomic(path, "x");
      expect(await readFile(path, "utf-8")).toBe("x");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("leaves no temp files behind", async () => {
    // A leftover .tmp would accumulate on every write.
    const root = await mkdtemp(join(tmpdir(), "x-sage-fs-"));
    try {
      await writeFileAtomic(join(root, "a.txt"), "one");
      await writeFileAtomic(join(root, "a.txt"), "two");
      expect(await readdir(root)).toEqual(["a.txt"]);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("replaces the previous content", async () => {
    const root = await mkdtemp(join(tmpdir(), "x-sage-fs-"));
    try {
      const path = join(root, "a.txt");
      await writeFileAtomic(path, "old and long");
      await writeFileAtomic(path, "new");
      expect(await readFile(path, "utf-8")).toBe("new");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});

describe("withKbLock", () => {
  test("runs the callback and releases the lock", async () => {
    const root = await mkdtemp(join(tmpdir(), "x-sage-fs-"));
    try {
      const result = await withKbLock(root, async () => 42);
      expect(result).toBe(42);
      expect(await readdir(root)).not.toContain(".write.lock");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("serialises concurrent holders", async () => {
    // Measured: 20 unlocked read-modify-writes of one file land only 2-3 times,
    // because each writer reads the same value. With the lock all 20 apply, so
    // the assertion below fails loudly if serialisation regresses.
    const root = await mkdtemp(join(tmpdir(), "x-sage-fs-"));
    try {
      const path = join(root, "counter.txt");
      await writeFile(path, "0");

      const increment = async () => {
        const current = Number.parseInt(await readFile(path, "utf-8"), 10);
        await writeFile(path, String(current + 1));
      };

      await Promise.all(Array.from({ length: 20 }, () => withKbLock(root, increment)));

      expect(await readFile(path, "utf-8")).toBe("20");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("releases the lock when the callback throws", async () => {
    const root = await mkdtemp(join(tmpdir(), "x-sage-fs-"));
    try {
      await expect(
        withKbLock(root, async () => {
          throw new Error("boom");
        }),
      ).rejects.toThrow("boom");

      // The next acquisition must not wait for a timeout.
      await expect(withKbLock(root, async () => "ok", { timeoutMs: 500 })).resolves.toBe("ok");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("reclaims a stale lock left by a dead process", async () => {
    const root = await mkdtemp(join(tmpdir(), "x-sage-fs-"));
    try {
      const lock = join(root, ".write.lock");
      await writeFile(lock, "99999\n0\n");
      // Backdate it past the staleness threshold.
      const old = new Date(Date.now() - 60_000);
      await utimes(lock, old, old);

      await expect(
        withKbLock(root, async () => "recovered", { staleAfterMs: 1000, timeoutMs: 2000 }),
      ).resolves.toBe("recovered");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("times out when a live lock is never released", async () => {
    const root = await mkdtemp(join(tmpdir(), "x-sage-fs-"));
    try {
      await writeFile(join(root, ".write.lock"), "1\n1\n");
      await expect(
        withKbLock(root, async () => "never", {
          timeoutMs: 120,
          retryMs: 10,
          staleAfterMs: 60_000,
        }),
      ).rejects.toBeInstanceOf(LockTimeoutError);
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});
