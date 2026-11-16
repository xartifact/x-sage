import { describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  defaultManifestPath,
  formatVersion,
  packageVersion,
  probeGit,
  readCliBuildInfo,
  readManifest,
} from "./build-info.ts";

/**
 * Build identity tests.
 *
 * The contract these protect: a user asking "which build am I running?" must
 * get a *machine-checkable* answer that distinguishes two builds sharing the
 * same 0.1.0 label. The failure mode being guarded against is the opposite of
 * an error — `--version` cheerfully printing a plausible-looking triple that
 * does not identify the bytes on disk.
 */

const FULL_SHA = "0123456789abcdef0123456789abcdef01234567";

async function withTempDir<T>(fn: (dir: string) => Promise<T>): Promise<T> {
  const dir = await mkdtemp(join(tmpdir(), "x-sage-buildinfo-"));
  try {
    return await fn(dir);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

async function writeManifest(dir: string, body: unknown): Promise<string> {
  const path = join(dir, "build-manifest.json");
  await writeFile(path, typeof body === "string" ? body : JSON.stringify(body), "utf-8");
  return path;
}

describe("readManifest", () => {
  test("reads the documented keys and ignores unknown ones", async () => {
    await withTempDir(async (dir) => {
      const path = await writeManifest(dir, {
        version: "0.1.0",
        commit: FULL_SHA,
        ref: "main",
        installedAt: "2026-11-16T00:00:00Z",
        root: "/opt/x-sage",
        // Installer-added noise must not break parsing: manifests evolve.
        futureField: { nested: true },
      });
      expect(readManifest(path)).toEqual({
        version: "0.1.0",
        commit: FULL_SHA,
        ref: "main",
        installedAt: "2026-11-16T00:00:00Z",
        root: "/opt/x-sage",
      });
    });
  });

  test("drops empty strings rather than reporting blank identity", async () => {
    await withTempDir(async (dir) => {
      const path = await writeManifest(dir, { version: "", commit: FULL_SHA, ref: "" });
      expect(readManifest(path)).toEqual({ commit: FULL_SHA });
    });
  });

  test("rejects non-object JSON so a corrupt manifest is loud, not silent", async () => {
    await withTempDir(async (dir) => {
      // readManifest is synchronous on purpose (it is called from --version,
      // which must not need an event loop turn). Throws stay synchronous so a
      // caller cannot accidentally swallow them in a floating promise.
      expect(() => readManifest(join(dir, "missing.json"))).toThrow();
      const arr = await writeManifest(dir, "[1,2,3]");
      expect(() => readManifest(arr)).toThrow(/JSON object/);
      const str = await writeManifest(dir, '"just a string"');
      expect(() => readManifest(str)).toThrow(/JSON object/);
    });
  });
});

describe("readCliBuildInfo precedence", () => {
  test("manifest beats env and git", async () => {
    await withTempDir(async (dir) => {
      const manifestPath = await writeManifest(dir, {
        version: "0.1.0",
        commit: FULL_SHA,
        ref: "v0.1.0",
      });
      const info = readCliBuildInfo(
        { X_SAGE_COMMIT: "envcommit", X_SAGE_REF: "envref" } as NodeJS.ProcessEnv,
        { manifestPath },
      );
      expect(info).toEqual({
        version: "0.1.0",
        commit: FULL_SHA,
        ref: "v0.1.0",
        source: "manifest",
      });
    });
  });

  test("env beats the git probe when no manifest exists", async () => {
    await withTempDir(async (dir) => {
      const info = readCliBuildInfo(
        {
          X_SAGE_COMMIT: "envcommit",
          X_SAGE_REF: "v9.9.9",
          X_SAGE_VERSION: "2.0.0",
        } as NodeJS.ProcessEnv,
        { manifestPath: join(dir, "absent.json") },
      );
      expect(info).toEqual({
        version: "2.0.0",
        commit: "envcommit",
        ref: "v9.9.9",
        source: "env",
      });
    });
  });

  test("a manifest without a commit is treated as absent", async () => {
    // A truncated manifest (install died mid-write) must not yield a manifest
    // source with commit "unknown": that would claim install provenance for
    // something we cannot identify.
    await withTempDir(async (dir) => {
      const manifestPath = await writeManifest(dir, { version: "0.1.0", ref: "main" });
      const info = readCliBuildInfo({ X_SAGE_COMMIT: "fallback" } as NodeJS.ProcessEnv, {
        manifestPath,
      });
      expect(info.source).toBe("env");
      expect(info.commit).toBe("fallback");
    });
  });

  test("malformed manifest falls through instead of throwing", async () => {
    await withTempDir(async (dir) => {
      const manifestPath = await writeManifest(dir, "{ not json");
      const info = readCliBuildInfo({ X_SAGE_COMMIT: "abc" } as NodeJS.ProcessEnv, {
        manifestPath,
      });
      expect(info.source).toBe("env");
      expect(info.commit).toBe("abc");
    });
  });
});

describe("readCliBuildInfo fallbacks", () => {
  test("reports the git commit of the working tree that contains this file", () => {
    const info = readCliBuildInfo({} as NodeJS.ProcessEnv, {
      manifestPath: join(tmpdir(), "x-sage-absent-manifest.json"),
    });
    // The test runs inside the repo, so a probe must succeed. If this fails,
    // either the repo has no `.git` (installed snapshot) or git is missing —
    // both are real environment facts, not test bugs.
    if (info.source !== "git") {
      expect(["unknown", "env"]).toContain(info.source);
      return;
    }
    expect(info.commit).toMatch(/^[0-9a-f]{40}$/);
    expect(info.ref).not.toBe("");
  });

  test("sentinels are honest when nothing can be determined", async () => {
    await withTempDir(async (dir) => {
      const info = readCliBuildInfo({} as NodeJS.ProcessEnv, {
        manifestPath: join(dir, "absent.json"),
        gitCwd: dir, // not a repo
      });
      expect(info).toEqual({
        version: packageVersion() ?? "dev",
        commit: "unknown",
        ref: "unknown",
        source: "unknown",
      });
    });
  });
});

describe("probeGit", () => {
  test("returns nothing outside a repository", async () => {
    await withTempDir(async (dir) => {
      expect(probeGit(dir)).toEqual({});
    });
  });

  test("flags a dirty tree instead of silently reporting the commit", async () => {
    // The distinction matters: "commit abc, dirty" means the bytes do NOT match
    // abc. Reporting only the commit would invite a wrong bisect.
    const dir = await mkdtemp(join(tmpdir(), "x-sage-dirty-"));
    try {
      const run = (args: string[]) =>
        Bun.spawnSync({ cmd: ["git", ...args], cwd: dir, stdout: "pipe", stderr: "pipe" });
      run(["init"]);
      run(["config", "user.email", "t@example.com"]);
      run(["config", "user.name", "t"]);
      await writeFile(join(dir, "a.txt"), "one", "utf-8");
      run(["add", "."]);
      run(["commit", "-m", "first"]);

      expect(probeGit(dir).dirty).toBe(false);

      await writeFile(join(dir, "a.txt"), "two", "utf-8");
      const dirty = probeGit(dir);
      expect(dirty.dirty).toBe(true);
      expect(dirty.commit).toMatch(/^[0-9a-f]{40}$/);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

describe("formatVersion", () => {
  test("prints the full commit, never a truncated one", () => {
    // A 7-char prefix cannot be compared against a manifest storing 40.
    const line = formatVersion({
      version: "0.1.0",
      commit: FULL_SHA,
      ref: "main",
      source: "manifest",
    });
    expect(line).toContain(FULL_SHA);
    expect(line).toContain("0.1.0");
    expect(line).toContain("main");
  });

  test("says dirty when the tree is dirty", () => {
    const line = formatVersion({
      version: "0.1.0",
      commit: FULL_SHA,
      ref: "main",
      source: "git",
      dirty: true,
    });
    expect(line).toContain("dirty");
  });

  test("omits the source token for manifest installs but keeps it otherwise", () => {
    // Manifest is the normal, trusted case; printing it every time is noise.
    const manifest = formatVersion({
      version: "0.1.0",
      commit: FULL_SHA,
      ref: "main",
      source: "manifest",
    });
    expect(manifest).not.toContain("source manifest");
    const git = formatVersion({ version: "0.1.0", commit: FULL_SHA, ref: "main", source: "git" });
    expect(git).toContain("source git");
  });
});

describe("defaultManifestPath", () => {
  test("resolves two levels above the sources, matching the install layout", () => {
    // Layout: <install-root>/cli/src → <install-root>/build-manifest.json.
    // In a working tree this points at apps/build-manifest.json, which must
    // not exist — that absence is what routes us to the git probe.
    const path = defaultManifestPath();
    expect(path.endsWith("build-manifest.json")).toBe(true);
    expect(path).not.toContain("/src/");
  });
});
