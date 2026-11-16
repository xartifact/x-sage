import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readdir, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

/**
 * Installer behaviour.
 *
 * These run the real script against a real git remote and a real HTTP source,
 * because every failure mode here is a filesystem effect that a source-text
 * assertion cannot observe: whether a redirect followed a symlink, whether a
 * path was replaced or written through, whether an aborted switch left the
 * machine with a runnable client.
 *
 * Fixture: a throwaway git repo (so `git ls-remote` resolves a commit), served
 * over HTTP at `<base>/<sha>/...` to model GitHub raw.
 */

const REPO = resolve(import.meta.dir, "..");
const INSTALLER = join(REPO, "scripts/install-cli.sh");
const CLI_SOURCES = ["index.ts", "config.ts", "build-info.ts"];

interface Fixture {
  root: string;
  sha: string;
  base: string;
  repoUrl: string;
  apiUrl: string;
  stop: () => Promise<void>;
}

let fixture: Fixture;

async function sh(cwd: string, ...args: string[]): Promise<string> {
  const proc = Bun.spawn(args, { cwd, stdout: "pipe", stderr: "pipe" });
  const [out, err] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text()]);
  const code = await proc.exited;
  if (code !== 0) throw new Error(`${args.join(" ")} failed (${code}): ${err || out}`);
  return out;
}

beforeAll(async () => {
  const root = await mkdtemp(join(tmpdir(), "x-sage-installer-"));

  // Source tree laid out like the repository, so the installer's paths resolve.
  const tree = join(root, "tree");
  await mkdir(join(tree, "apps/cli/src"), { recursive: true });
  await mkdir(join(tree, "skills/x-sage-kb"), { recursive: true });
  for (const file of CLI_SOURCES) {
    await Bun.write(join(tree, "apps/cli/src", file), await Bun.file(join(REPO, "apps/cli/src", file)).text());
  }
  await Bun.write(join(tree, "apps/cli/package.json"), await Bun.file(join(REPO, "apps/cli/package.json")).text());
  await Bun.write(
    join(tree, "skills/x-sage-kb/SKILL.md"),
    await Bun.file(join(REPO, "skills/x-sage-kb/SKILL.md")).text(),
  );

  await sh(root, "git", "init", "-q", "-b", "main", tree);
  await sh(tree, "git", "add", "-A");
  await sh(tree, "git", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "fixture");
  const sha = (await sh(tree, "git", "rev-parse", "HEAD")).trim();

  // A bare remote gives `git ls-remote` something to answer with.
  const repoUrl = join(root, "remote.git");
  await sh(root, "git", "init", "-q", "--bare", repoUrl);
  await sh(tree, "git", "remote", "add", "origin", repoUrl);
  await sh(tree, "git", "push", "-q", "origin", "main");

  // Serve the tree at /<sha>/... the way raw.githubusercontent.com does.
  const server = Bun.serve({
    port: 0,
    async fetch(request) {
      const path = new URL(request.url).pathname.replace(/^\//, "");
      const file = Bun.file(join(root, "www", path));
      return (await file.exists()) ? new Response(file) : new Response("not found", { status: 404 });
    },
  });

  // Materialise the served copy (also proves the installer only needs HTTP).
  const www = join(root, "www");
  const copy = async (from: string, to: string) => {
    await mkdir(join(www, sha, to, "..").replace(/\/\.\.$/, ""), { recursive: true });
    await Bun.write(join(www, sha, to), await Bun.file(join(tree, from)).text());
  };
  for (const file of CLI_SOURCES) await copy(`apps/cli/src/${file}`, `apps/cli/src/${file}`);
  await copy("apps/cli/package.json", "apps/cli/package.json");
  await copy("skills/x-sage-kb/SKILL.md", "skills/x-sage-kb/SKILL.md");
  // API endpoint used only when git is unavailable; harmless to provide.
  await Bun.write(join(www, "api/commits/main"), JSON.stringify({ sha }));

  fixture = {
    root,
    sha,
    base: `http://127.0.0.1:${server.port}`,
    repoUrl,
    apiUrl: `http://127.0.0.1:${server.port}/api`,
    stop: async () => {
      server.stop(true);
    },
  };
});

afterAll(async () => {
  await fixture?.stop();
  if (fixture?.root !== undefined) await rm(fixture.root, { recursive: true, force: true });
});

/**
 * Publish an extra commit in the fixture repo and serve it, so a test can
 * distinguish "restored the previous build" from "installed the same build".
 *
 * Each commit gets its own package version and a marker file, making the two
 * installs observably different.
 */
async function addCommit(label: string, options: { marker?: string } = {}): Promise<string> {
  const tree = join(fixture.root, `tree-${label}`);
  await mkdir(join(tree, "apps/cli/src"), { recursive: true });
  await mkdir(join(tree, "skills/x-sage-kb"), { recursive: true });
  for (const file of CLI_SOURCES) {
    await Bun.write(join(tree, "apps/cli/src", file), await Bun.file(join(fixture.root, "tree/apps/cli/src", file)).text());
  }
  const pkg = JSON.parse(await Bun.file(join(fixture.root, "tree/apps/cli/package.json")).text());
  pkg.version = `9.9.${options.marker ?? label}`.replace(/[^0-9.]/g, "") || "9.9.9";
  await Bun.write(join(tree, "apps/cli/package.json"), JSON.stringify(pkg, null, 2));
  await Bun.write(
    join(tree, "skills/x-sage-kb/SKILL.md"),
    await Bun.file(join(fixture.root, "tree/skills/x-sage-kb/SKILL.md")).text(),
  );

  await sh(fixture.root, "git", "init", "-q", "-b", "main", tree);
  await sh(tree, "git", "add", "-A");
  await sh(tree, "git", "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", label);
  const sha = (await sh(tree, "git", "rev-parse", "HEAD")).trim();
  await sh(tree, "git", "remote", "add", "origin", fixture.repoUrl);
  await sh(tree, "git", "push", "-q", "origin", `HEAD:refs/heads/${label}`);

  // Serve this commit under its sha, like raw.githubusercontent.com.
  const copy = async (from: string, to: string) => {
    const dest = join(fixture.root, "www", sha, to);
    await mkdir(join(dest, ".."), { recursive: true });
    await Bun.write(dest, await Bun.file(join(tree, from)).text());
  };
  for (const file of CLI_SOURCES) await copy(`apps/cli/src/${file}`, `apps/cli/src/${file}`);
  await copy("apps/cli/package.json", "apps/cli/package.json");
  await copy("skills/x-sage-kb/SKILL.md", "skills/x-sage-kb/SKILL.md");
  return sha;
}

function installEnv(prefix: string, skillsDir: string, extra: Record<string, string> = {}) {
  return {
    ...process.env,
    X_SAGE_PREFIX: prefix,
    X_SAGE_SKILLS_DIR: skillsDir,
    X_SAGE_BASE: fixture.base,
    X_SAGE_API: fixture.apiUrl,
    X_SAGE_REPO_URL: fixture.repoUrl,
    X_SAGE_REF: "main",
    HOME: join(fixture.root, "home"),
    ...extra,
  };
}

async function runInstaller(env: Record<string, string>): Promise<{ code: number; output: string }> {
  const proc = Bun.spawn(["bash", INSTALLER], { stdout: "pipe", stderr: "pipe", env });
  const [out, err] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text()]);
  return { code: await proc.exited, output: `${out}${err}` };
}

async function freshPrefix(name: string): Promise<{ prefix: string; skills: string }> {
  const prefix = await mkdtemp(join(tmpdir(), `x-sage-${name}-`));
  return { prefix, skills: join(prefix, "skills") };
}

describe("installer behaviour", () => {
  test("a symlinked bin/x-sage is replaced, not written through", async () => {
    // On this machine ~/.bun/bin/x-sage points into a working tree. `> bin/x-sage`
    // would follow that link and overwrite the source file it points at.
    const { prefix, skills } = await freshPrefix("binlink");
    try {
      const protectedSource = join(prefix, "protected-source.ts");
      await writeFile(protectedSource, "PROTECTED SOURCE\n");
      await mkdir(join(prefix, "bin"), { recursive: true });
      await symlink(protectedSource, join(prefix, "bin/x-sage"));

      const result = await runInstaller(installEnv(prefix, skills));
      expect(result.code).toBe(0);

      expect(await readFile(protectedSource, "utf-8")).toBe("PROTECTED SOURCE\n");
      const entry = join(prefix, "bin/x-sage");
      expect(existsSync(entry)).toBe(true);
      expect((await Bun.file(entry).text()).startsWith("#!/usr/bin/env bash")).toBe(true);
    } finally {
      await rm(prefix, { recursive: true, force: true });
    }
  });

  test("a symlinked skill directory is moved aside, its target untouched", async () => {
    // Writing through the link would edit whichever repository the user pointed
    // it at — the same class of bug as the bin symlink.
    const { prefix, skills } = await freshPrefix("skilllink");
    try {
      const protectedSkillDir = join(prefix, "user-repo");
      await mkdir(protectedSkillDir, { recursive: true });
      await writeFile(join(protectedSkillDir, "SKILL.md"), "USER OWNED\n");
      await mkdir(skills, { recursive: true });
      await symlink(protectedSkillDir, join(skills, "x-sage-kb"));

      const result = await runInstaller(installEnv(prefix, skills));
      expect(result.code).toBe(0);

      expect(await readFile(join(protectedSkillDir, "SKILL.md"), "utf-8")).toBe("USER OWNED\n");

      const installed = await readFile(join(skills, "x-sage-kb/SKILL.md"), "utf-8");
      expect(installed).toContain("name: x-sage-kb");
      expect(installed).toContain("refine-body");
      // The old content must survive somewhere, named so a user can find it.
      const { readdir } = await import("node:fs/promises");
      const backups = (await readdir(skills)).filter((name) => name.startsWith("x-sage-kb.backup-"));
      expect(backups).toHaveLength(1);
      expect(await readFile(join(skills, backups[0]!, "SKILL.md"), "utf-8")).toBe("USER OWNED\n");
    } finally {
      await rm(prefix, { recursive: true, force: true });
    }
  });

  test("a failure after the switch restores the previous runnable CLI", async () => {
    // The switch is a symlink re-point, so it can be undone. This is the case a
    // string assertion cannot cover: the machine must still run the *previous*
    // build after a failed install.
    const { prefix, skills } = await freshPrefix("rollback");
    try {
      // Install v1 (the fixture commit), then make a different commit available
      // as v2 so "restored" cannot be satisfied by installing the same tree.
      const first = await runInstaller(installEnv(prefix, skills));
      expect(first.code).toBe(0);
      const entry = join(prefix, "bin/x-sage");
      const before = (await sh(prefix, entry, "--version")).trim();
      expect(before).toContain(`commit ${fixture.sha}`);

      const v2 = await addCommit("v2", { marker: "v2" });
      const upgraded = await runInstaller(installEnv(prefix, skills, { X_SAGE_REF: v2 }));
      expect(upgraded.code).toBe(0);
      expect((await sh(prefix, entry, "--version")).trim()).toContain(`commit ${v2}`);
      const currentAfterUpgrade = (await sh(prefix, "readlink", join(prefix, "lib/x-sage/current"))).trim();

      // Make the skill install fail after the link and wrapper have switched.
      const skillTarget = join(skills, "x-sage-kb");
      const backupBefore = (await readdir(skills)).filter((n) => n.includes(".backup-")).length;
      await chmod(skillTarget, 0o500);

      const third = await runInstaller(installEnv(prefix, skills, { X_SAGE_REF: v2 }));
      await chmod(skillTarget, 0o700).catch(() => {});
      expect(third.code).not.toBe(0);

      // Entry point survived and still reports the version installed *before*
      // the failed run — the rollback restored the link, it did not merely
      // leave a broken tree behind.
      expect(existsSync(entry)).toBe(true);
      expect((await sh(prefix, entry, "--version")).trim()).toContain(`commit ${v2}`);
      expect((await sh(prefix, "readlink", join(prefix, "lib/x-sage/current"))).trim()).toBe(currentAfterUpgrade);
      expect((await readdir(skills)).filter((n) => n.includes(".backup-")).length).toBe(backupBefore);
    } finally {
      await rm(prefix, { recursive: true, force: true });
    }
  }, 90_000);

  test("an unresolvable ref installs nothing and leaves the old CLI working", async () => {
    const { prefix, skills } = await freshPrefix("badref");
    try {
      const first = await runInstaller(installEnv(prefix, skills));
      expect(first.code).toBe(0);
      const entry = join(prefix, "bin/x-sage");
      const before = (await sh(prefix, entry, "--version")).trim();

      const second = await runInstaller(installEnv(prefix, skills, { X_SAGE_REF: "no-such-branch" }));
      expect(second.code).not.toBe(0);
      expect(second.output).toContain("nothing was installed");
      expect((await sh(prefix, entry, "--version")).trim()).toBe(before);
    } finally {
      await rm(prefix, { recursive: true, force: true });
    }
  }, 60_000);
});
