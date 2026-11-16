/**
 * Build identity for the CLI.
 *
 * The server reports its own identity via `/api/health` (`readBuildInfo` in
 * apps/server). The client needs the same three facts for a different reason:
 * a user reporting a bug, or a script asserting "am I current", cannot tell two
 * 0.1.0 builds apart. A constant version string is therefore not a version — it
 * is a label that happens to look like one.
 *
 * Sources, in order of authority:
 *   1. `X_SAGE_BUILD_MANIFEST` — path to a manifest written by the installer.
 *      This is what a *snapshot install* carries: it describes the exact commit
 *      the snapshot was taken from, independent of whatever git state happens to
 *      exist near the installed files.
 *   2. Environment variables baked at install time
 *      (`X_SAGE_VERSION` / `X_SAGE_COMMIT` / `X_SAGE_REF`).
 *   3. Live git probe (developer working tree only) — best effort, and marked
 *      as `dirty` when the tree has uncommitted changes, because "which commit am
 *      I?" has no single answer then.
 *   4. Sentinels: `dev` / `unknown`, which are honest about not knowing.
 *
 * The manifest outranks a git probe on purpose: an installed snapshot lives
 * outside the repo, so a probe would either fail or (worse) read a *different*
 * repo that happens to be nearby.
 */
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";

export interface CliBuildInfo {
  version: string;
  commit: string;
  ref: string;
  /**
   * `manifest` when read from an installer manifest, `env` when from baked
   * env vars, `git` when probed from a working tree, `unknown` otherwise.
   * Surfaced in `--version` so a user can tell how much to trust the triple.
   */
  source: "manifest" | "env" | "git" | "unknown";
  /** Working tree had uncommitted changes at probe time (git source only). */
  dirty?: boolean;
}

/** Shape written by `scripts/install-cli.sh`. Unknown keys are ignored. */
export interface BuildManifest {
  version?: string;
  commit?: string;
  ref?: string;
  installedAt?: string;
  /** Absolute path of the snapshot root; informational only. */
  root?: string;
}

/** Read and validate a manifest file. Throws on unreadable/malformed input. */
export function readManifest(path: string): BuildManifest {
  const raw = readFileSync(path, "utf-8");
  const parsed: unknown = JSON.parse(raw);
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error(`${path}: manifest must be a JSON object`);
  }
  const record = parsed as Record<string, unknown>;
  const out: BuildManifest = {};
  for (const key of ["version", "commit", "ref", "installedAt", "root"] as const) {
    const value = record[key];
    if (typeof value === "string" && value !== "") out[key] = value;
  }
  return out;
}

/**
 * Locate the manifest that ships with an installed snapshot.
 *
 * Installed layout:   `<install-root>/cli/src` → `<install-root>/build-manifest.json`
 *                     (two levels up from `src`)
 * Working tree:       `<repo>/apps/cli/src`     → `<repo>/apps/build-manifest.json` (absent)
 *
 * A missing file is the signal we want in a working tree: it falls through to
 * the git probe rather than pretending to be an install.
 */
export function defaultManifestPath(): string {
  return resolve(dirname(import.meta.dir), "..", "build-manifest.json");
}

/**
 * Version declared by the CLI package itself.
 *
 * Kept separate from the commit: the version is a human-facing release label,
 * the commit is the machine-checkable identity. `--version` prints both because
 * either alone is ambiguous (many builds share 0.1.0; a commit is unreadable).
 */
export function packageVersion(): string | undefined {
  try {
    // import.meta.dir is `<pkg>/src`, so the package root is one level up.
    const raw = readFileSync(resolve(dirname(import.meta.dir), "package.json"), "utf-8");
    const parsed: unknown = JSON.parse(raw);
    if (parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)) {
      const version = (parsed as Record<string, unknown>).version;
      if (typeof version === "string" && version !== "") return version;
    }
  } catch {
    // No package.json next to the sources: fall through to sentinels.
  }
  return undefined;
}

/** Run `git` and return trimmed stdout, or undefined when unavailable. */
function git(args: string[], cwd: string): string | undefined {
  try {
    const proc = Bun.spawnSync({
      cmd: ["git", ...args],
      cwd,
      stdout: "pipe",
      stderr: "ignore",
    });
    if (proc.exitCode !== 0) return undefined;
    const text = proc.stdout.toString().trim();
    return text === "" ? undefined : text;
  } catch {
    return undefined;
  }
}

/**
 * Probe the repository that contains this file.
 *
 * Only meaningful for a working tree; an installed snapshot has no `.git`
 * above it and every probe returns undefined.
 */
export function probeGit(cwd: string = dirname(import.meta.dir)): {
  commit?: string;
  ref?: string;
  dirty?: boolean;
} {
  const commit = git(["rev-parse", "HEAD"], cwd);
  if (commit === undefined) return {};
  const symbolic = git(["rev-parse", "--abbrev-ref", "HEAD"], cwd);
  const status = git(["status", "--porcelain"], cwd);
  // Spread rather than assign undefined: `exactOptionalPropertyTypes` treats
  // an explicit `undefined` as different from an absent key, and the absent-key
  // form is what the rest of this module checks for.
  return {
    commit,
    ...(symbolic === undefined || symbolic === "HEAD" ? {} : { ref: symbolic }),
    dirty: status !== undefined,
  };
}

/**
 * Resolve the build identity.
 *
 * Ordering rationale is in the module comment; the one non-obvious case is a
 * dirty tree: we still report the commit, but `dirty: true` travels with it so
 * `--version` can say so instead of implying the bytes match that commit.
 */
export function readCliBuildInfo(
  env: NodeJS.ProcessEnv = process.env,
  options: { manifestPath?: string; gitCwd?: string } = {},
): CliBuildInfo {
  const manifestPath = options.manifestPath ?? env.X_SAGE_BUILD_MANIFEST ?? defaultManifestPath();
  const declaredVersion = env.X_SAGE_VERSION ?? packageVersion();

  try {
    const manifest = readManifest(manifestPath);
    if (manifest.commit !== undefined) {
      return {
        version: manifest.version ?? declaredVersion ?? "unknown",
        commit: manifest.commit,
        ref: manifest.ref ?? "unknown",
        source: "manifest",
      };
    }
  } catch {
    // Missing or malformed manifest: not an install, or a truncated one.
    // Fall through rather than fail — `--version` must always answer.
  }

  const envCommit = env.X_SAGE_COMMIT;
  if (envCommit !== undefined && envCommit !== "") {
    return {
      version: declaredVersion ?? "unknown",
      commit: envCommit,
      ref: env.X_SAGE_REF ?? "unknown",
      source: "env",
    };
  }

  const probed = probeGit(options.gitCwd);
  if (probed.commit !== undefined) {
    return {
      version: declaredVersion ?? "unknown",
      commit: probed.commit,
      ref: probed.ref ?? "unknown",
      source: "git",
      ...(probed.dirty === true ? { dirty: true } : {}),
    };
  }

  return {
    version: declaredVersion ?? "dev",
    commit: "unknown",
    ref: "unknown",
    source: "unknown",
  };
}

/**
 * One-line rendering for `--version`.
 *
 * Prints the full 40-char commit: truncating to 7 is conventional in logs but
 * invites "is this the same commit?" guessing when comparing against a manifest
 * that stores the full hash.
 */
export function formatVersion(info: CliBuildInfo): string {
  const parts = [`x-sage CLI ${info.version}`, `commit ${info.commit}`, `ref ${info.ref}`];
  if (info.dirty === true) parts.push("dirty");
  if (info.source !== "manifest") parts.push(`source ${info.source}`);
  return parts.join("  ");
}
