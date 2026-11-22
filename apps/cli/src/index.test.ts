import { describe, expect, test } from "bun:test";
import { resolve, join } from "node:path";
import { mkdtemp, cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { createApp } from "../../server/src/app.ts";

/**
 * CLI tests.
 *
 * The CLI is a REST client, so the contract it publishes is exactly three
 * things: exit code, stdout bytes, and stderr text. Each case spawns the real
 * binary because `main()` runs on import.
 *
 * Every case goes through a real HTTP server: the CLI has no local mode, and
 * the point of these tests is that a client with no filesystem access still
 * gets fully rendered output.
 */

const CLI = resolve(import.meta.dir, "index.ts");
const REAL_KB = resolve(import.meta.dir, "../../../kb/default");

interface Run {
  stdout: string;
  stderr: string;
  exitCode: number;
}

/**
 * Isolated config dir for spawned CLI processes.
 *
 * The CLI now reads `$XDG_CONFIG_HOME/x-sage/config.toml`. Without this,
 * a developer's real config would leak into the suite — that is exactly how
 * the "no server configured" case started failing once config support landed.
 * Cases that want a config file write into this dir instead.
 */
const SANDBOX_CONFIG = join(tmpdir(), `x-sage-cli-cfg-${process.pid}`);

/**
 * A private KB copy for tests that mutate.
 *
 * Merges and proposals write to disk; pointing them at `REAL_KB` would edit the
 * repository and make the suite order-dependent.
 */
async function copyKb(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "x-sage-cli-kb-"));
  await cp(REAL_KB, dir, { recursive: true });
  return dir;
}

async function run(
  args: string[],
  env: Record<string, string> = {},
  stdin?: string,
): Promise<Run> {
  const proc = Bun.spawn(["bun", CLI, ...args], {
    cwd: process.cwd(),
    stdout: "pipe",
    stderr: "pipe",
    ...(stdin === undefined ? {} : { stdin: new TextEncoder().encode(stdin) }),
    env: {
      ...process.env,
      X_SAGE_SERVER: "",
      X_SAGE_KB: "",
      X_SAGE_TOKEN: "",
      XDG_CONFIG_HOME: SANDBOX_CONFIG,
      ...env,
    },
  });
  const [stdout, stderr] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ]);
  return { stdout, stderr, exitCode: await proc.exited };
}

/**
 * A real HTTP server on an ephemeral port.
 *
 * Remote-mode assertions must go over the wire: the point is that no kb data
 * reaches the client.
 */
function serve(writeToken?: string, kbRoot: string = REAL_KB) {
  const app = createApp({ kbRoot, ...(writeToken === undefined ? {} : { writeToken }) });
  const server = Bun.serve({ port: 0, fetch: (request) => app.fetch(request) });
  return {
    url: `http://127.0.0.1:${server.port}`,
    stop: () => server.stop(true),
  };
}

/**
 * Run one command against a live server.
 *
 * The CLI has no local mode, so every assertion needs a server; starting one
 * per case keeps the suite parallel-safe and proves the client never touches
 * the filesystem.
 */
/**
 * Detect a response that did not come from this server.
 *
 * Some sandboxes intercept a child process's outbound HTTP and answer 401 with
 * `{"error":{"code":"AUTH_REQUIRED"}}`. The dispatcher never emits that shape,
 * so seeing it means the request never reached the app; retrying is safe and
 * is the difference between a real regression and a network sandbox artifact.
 */
function isIntercepted(runResult: Run): boolean {
  return runResult.stderr.includes("AUTH_REQUIRED") || runResult.stdout.includes("AUTH_REQUIRED");
}

async function againstServer(
  args: string[],
  options: { writeToken?: string; stdin?: string; kbRoot?: string } = {},
): Promise<Run> {
  const attempts = 3;
  for (let attempt = 1; ; attempt++) {
    const server = serve(options.writeToken, options.kbRoot);
    try {
      const tokenArgs = options.writeToken === undefined ? [] : ["--token", options.writeToken];
      const result = await run(["--server", server.url, ...tokenArgs, ...args], {}, options.stdin);
      if (attempt >= attempts || !isIntercepted(result)) return result;
    } finally {
      server.stop();
    }
  }
}

describe("read commands", () => {
  test("activate prints the AI transcript with derived REQUIRES", async () => {
    const { stdout, exitCode } = await againstServer(["activate", "decision"]);
    expect(exitCode).toBe(0);
    expect(stdout).toContain("[CONCEPT: decision]");
    // context and value are only reachable transitively.
    expect(stdout).toMatch(/REQUIRES: \[.*context.*value.*\]/);
  });

  test("edges --dir in surfaces inbound relations", async () => {
    const { stdout, exitCode } = await againstServer(["edges", "goal", "--type", "contradicts", "--dir", "in"]);
    expect(exitCode).toBe(0);
    expect(stdout).toContain("↔ contradicts (5)");
    expect(stdout).toContain("latent-goal");
  });

  test("edges --derived adds closure edges that the default view omits", async () => {
    // decision asserts 5 prerequisites; `value` is only reachable transitively.
    const withoutDerived = await againstServer(["edges", "decision", "--type", "depends-on", "--dir", "out"]);
    const withDerived = await againstServer(["edges", "decision", "--type", "depends-on", "--dir", "out", "--derived"]);

    expect({ run: "withoutDerived", ...withoutDerived }).toMatchObject({ exitCode: 0, stderr: "" });
    expect({ run: "withDerived", ...withDerived }).toMatchObject({ exitCode: 0, stderr: "" });
    expect(withoutDerived.stdout).not.toContain("value");
    expect(withDerived.stdout).toContain("value");
    expect(withDerived.stdout).toContain("(derived)");
  });

  test("search finds concepts by trigger keyword", async () => {
    const { stdout, exitCode } = await againstServer(["search", "决策"]);
    expect(exitCode).toBe(0);
    expect(stdout).toContain("decision");
  });

  test("validate --graph exits 0 for a clean KB", async () => {
    const { stdout, exitCode } = await againstServer(["validate", "--graph"]);
    expect(exitCode).toBe(0);
    expect(stdout).toContain("Graph integrity: no issues found");
  });
});

describe("error handling", () => {
  test("an unknown concept exits 1 with a message on stderr", async () => {
    const { stderr, exitCode } = await againstServer(["activate", "nope"]);
    expect(exitCode).toBe(1);
    expect(stderr).toContain("Concept not found: nope");
  });

  test("an invalid edge type lists the valid values", async () => {
    const { stderr, exitCode } = await againstServer(["edges", "goal", "--type", "bogus"]);
    expect(exitCode).toBe(1);
    expect(stderr).toContain("Must be one of:");
  });

  test("frontier add appends one row and leaves the other sections alone", async () => {
    const kbRoot = await copyKb();
    try {
      const before = await readFile(join(kbRoot, "frontier.md"), "utf-8");
      const added = await againstServer(
        ["frontier", "add", "x-core", "--source", "矩阵规划", "--reason", "建仓后核验实际形态", "--priority", "medium"],
        { kbRoot },
      );
      expect(added.exitCode).toBe(0);

      const after = await readFile(join(kbRoot, "frontier.md"), "utf-8");
      // The two prose tables are the reason this command was disabled: a
      // whole-file re-render dropped them.
      expect(after).toContain("## 概念内张力");
      expect(after).toContain("## L2→L3 待展开方向");
      expect(after).toContain("| x-core | 矩阵规划 | 建仓后核验实际形态 | medium |");
      expect(after.length).toBeGreaterThan(before.length);

      // Adding the same row twice is not a second pending item.
      const again = await againstServer(
        ["frontier", "add", "x-core", "--source", "矩阵规划", "--reason", "建仓后核验实际形态", "--priority", "medium"],
        { kbRoot },
      );
      expect(again.exitCode).toBe(0);
      expect((await readFile(join(kbRoot, "frontier.md"), "utf-8")).match(/\| x-core \|/g)).toHaveLength(1);
    } finally {
      await rm(kbRoot, { recursive: true, force: true });
    }
  });

  test("frontier add without source or reason is a usage error", async () => {
    const { stderr, exitCode } = await againstServer(["frontier", "add", "x-core"]);
    expect(exitCode).toBe(1);
    expect(stderr).toContain("Usage: x-sage frontier add");
  });

  test("viz export requires an explicit path", async () => {
    const { stderr, exitCode } = await againstServer(["viz", "export"]);
    expect(exitCode).toBe(1);
    expect(stderr).toContain("Usage:");
  });

  test("no server configured is an error, not a silent bundled-KB fallback", async () => {
    // There is no local mode: without a target the client must not guess.
    const { stderr, exitCode } = await run(["activate", "decision"]);
    expect(exitCode).toBe(1);
    expect(stderr).toContain("No server configured");
  });
});

describe("config file", () => {
  test("a config file alone is enough to reach the server", async () => {
    // The point of the file: `x-sage activate x` with no flags and no exports.
    const server = serve();
    try {
      // configPath joins <XDG_CONFIG_HOME>/x-sage/config.toml, so the file must
      // live one level down — writing it at the root silently configures nothing.
      const dir = await mkdtemp(join(tmpdir(), "x-sage-cfgcli-"));
      await mkdir(join(dir, "x-sage"), { recursive: true });
      await writeFile(join(dir, "x-sage", "config.toml"), `server = "${server.url}"\n`);
      try {
        const { stdout, stderr, exitCode } = await run(["activate", "decision"], { XDG_CONFIG_HOME: dir });
        // Surface the child's stderr: a bare `expected 0, received 1` hides
        // whether this is a connection race or a config-resolution bug.
        expect({ exitCode, stderr }).toEqual({ exitCode: 0, stderr: "" });
        expect(stdout).toContain("[CONCEPT: decision]");
      } finally {
        await rm(dir, { recursive: true, force: true });
      }
    } finally {
      server.stop();
    }
  });

  test("flags override the config file", async () => {
    // A wrong server in the file must not beat an explicit --server.
    const server = serve();
    try {
      const dir = await mkdtemp(join(tmpdir(), "x-sage-cfgcli-"));
      await mkdir(join(dir, "x-sage"), { recursive: true });
      await writeFile(join(dir, "x-sage", "config.toml"), 'server = "http://127.0.0.1:1"\n');
      try {
        const { stdout, exitCode } = await run(
          ["--server", server.url, "activate", "decision"],
          { XDG_CONFIG_HOME: dir },
        );
        expect(exitCode).toBe(0);
        expect(stdout).toContain("[CONCEPT: decision]");
      } finally {
        await rm(dir, { recursive: true, force: true });
      }
    } finally {
      server.stop();
    }
  });
});

describe("validate against a broken KB", () => {
  test("reports every file and exits 1 without dumping a ZodError", async () => {
    const dir = await mkdtemp(join(tmpdir(), "x-sage-cli-"));
    try {
      await cp(REAL_KB, dir, { recursive: true });
      await writeFile(
        join(dir, "concepts", "L1-core", "broken.md"),
        "---\nid: broken\natom: x\nlayer: 9\n---\nbody\n",
      );

      const brokenServer = Bun.serve({ port: 0, fetch: (r) => createApp({ kbRoot: dir }).fetch(r) });
      try {
        const { stdout, exitCode } = await run([
          "--server", `http://127.0.0.1:${brokenServer.port}`, "validate", "--graph",
        ]);
        expect(exitCode).toBe(1);
        expect(stdout).toContain("✗ concepts/L1-core/broken.md");
        expect(stdout).toContain("22/23 concepts valid");
        // The unbuildable graph must be one line, not a serialized error object.
        expect(stdout).toContain("[unbuildable]");
        expect(stdout).not.toContain("unionErrors");
      } finally {
        brokenServer.stop(true);
      }
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

/**
 * The client speaks only HTTP: no local mode remains, so these cases start a
 * real server and assert the wire contract end to end.
 */
describe("wire contract", () => {
  const cases = [
    ["activate", "decision"],
    ["activate", "decision", "--strategy", "validation", "--depth", "1"],
    ["activate", "goal", "--format", "human"],
    ["edges", "goal", "--type", "contradicts", "--dir", "in"],
    ["edges", "decision", "--type", "depends-on", "--dir", "out"],
    ["edges", "decision", "--type", "depends-on", "--dir", "out", "--derived"],
    ["search", "决策"],
    ["search", "risk", "--format", "manifest"],
    ["path", "risk", "value"],
    ["validate"],
    ["validate", "--graph"],
    ["manifest"],
    ["proposals", "list"],
    ["frontier", "list"],
    ["graph", "validate"],
  ] as const;

  for (const args of cases) {
    test(`"${args.join(" ")}" succeeds against the server`, async () => {
      const result = await againstServer([...args]);
      // Include stderr so a failure names the cause instead of just "expected 0".
      expect({ exitCode: result.exitCode, stderr: result.stderr }).toEqual({
        exitCode: 0,
        stderr: "",
      });
      expect(result.stdout.length).toBeGreaterThan(0);
    });
  }

  test("errors arrive as text, not as a JSON object on stderr", async () => {
    const server = serve();
    try {
      const { stderr, exitCode } = await run(["--server", server.url, "activate", "nope"]);
      expect(exitCode).toBe(1);
      expect(stderr).toBe("Concept not found: nope\n");
    } finally {
      server.stop();
    }
  });

  test("an unreachable server fails with a readable message", async () => {
    const { stderr, exitCode } = await run(["--server", "http://127.0.0.1:1", "activate", "decision"]);
    expect(exitCode).toBe(1);
    expect(stderr).toContain("cannot reach");
  });

  test("writes require the token when the server is configured with one", async () => {
    const server = serve("s3cret");
    try {
      const denied = await run(["--server", server.url, "proposals", "merge", "nope.md"]);
      expect(denied.exitCode).toBe(1);
      expect(denied.stderr).toContain("Unauthorized");

      // 401 without the token, 404 with it: the named proposal does not exist.
      // A typo'd or missing filename must not read as "nothing to merge".
      const allowed = await run([
        "--server", server.url,
        "--token", "s3cret",
        "proposals", "merge", "nope.md",
      ]);
      expect(allowed.exitCode).toBe(1);
      expect(allowed.stderr).toContain("No such proposal");

      const all = await run([
        "--server", server.url,
        "--token", "s3cret",
        "proposals", "merge", "all",
      ]);
      expect(all.exitCode).toBe(0);
      expect(all.stdout).toContain("No pending proposals to merge");
    } finally {
      server.stop();
    }
  });

  test("reads still work without a token when writes are protected", async () => {
    const server = serve("s3cret");
    try {
      const { stdout, exitCode } = await run(["--server", server.url, "edges", "goal", "--dir", "in"]);
      expect(exitCode).toBe(0);
      expect(stdout).toContain("contradicts");
    } finally {
      server.stop();
    }
  });
});

describe("body write-back", () => {
  test("concept body reports the revision the proposal must quote", async () => {
    const { stdout, exitCode } = await againstServer(["concept", "body", "risk"]);
    expect(exitCode).toBe(0);
    const parsed = JSON.parse(stdout);
    expect(parsed.conceptId).toBe("risk");
    expect(parsed.revision).toMatch(/^[a-f0-9]{64}$/);
    // The body is everything after the frontmatter, so it cannot carry `id:`.
    expect(parsed.body).not.toContain("id: risk");
  });

  test("refine-body rejects a stale revision and accepts a current one", async () => {
    const kbRoot = await copyKb();
    try {
      const readback = await againstServer(["concept", "body", "risk"], { kbRoot });
      expect({ code: readback.exitCode, err: readback.stderr }).toEqual({ code: 0, err: "" });
      const live = JSON.parse(readback.stdout);

      const stale = await againstServer(
        ["refine-body", "risk", "--file", "-", "--expected-revision", "0".repeat(64), "--reason", "r"],
        { kbRoot, stdin: "# 风险\n\n候选。\n" },
      );
      expect(stale.exitCode).toBe(1);
      expect(stale.stderr).toContain("revision");

      const ok = await againstServer(
        ["refine-body", "risk", "--file", "-", "--expected-revision", live.revision, "--reason", "按澄清更正"],
        { kbRoot, stdin: "# 风险\n\n候选。\n" },
      );
      expect(ok.exitCode).toBe(0);
      expect(ok.stdout).toContain("proposals/bodies/");
    } finally {
      await rm(kbRoot, { recursive: true, force: true });
    }
  });

  test("refine-body reads stdin with --file -", async () => {
    const kbRoot = await copyKb();
    try {
      const readback = await againstServer(["concept", "body", "risk"], { kbRoot });
      expect({ code: readback.exitCode, err: readback.stderr }).toEqual({ code: 0, err: "" });
      const live = JSON.parse(readback.stdout);
      const candidate = "# 风险\n\n来自 stdin 的正文。\n";
      const { stdout, stderr, exitCode } = await againstServer(
        ["refine-body", "risk", "--file", "-", "--expected-revision", live.revision, "--reason", "stdin"],
        { kbRoot, stdin: candidate },
      );
      expect({ exitCode, stderr }).toEqual({ exitCode: 0, stderr: "" });

      const file = stdout.match(/proposals\/bodies\/(\S+\.json)/)?.[1];
      expect(file).toBeDefined();
      const shown = await againstServer(["proposals", "show", file!, "--json"], { kbRoot });
      expect(JSON.parse(shown.stdout).proposal.proposedBody).toBe(candidate);
    } finally {
      await rm(kbRoot, { recursive: true, force: true });
    }
  });

  test("proposals show prints a diff, then merge applies it", async () => {
    const kbRoot = await copyKb();
    try {
      const readback = await againstServer(["concept", "body", "risk"], { kbRoot });
      expect({ code: readback.exitCode, err: readback.stderr }).toEqual({ code: 0, err: "" });
      const live = JSON.parse(readback.stdout);
      const candidate = `${live.body}\n追加的一段。\n`;
      const created = await againstServer(
        ["refine-body", "risk", "--file", "-", "--expected-revision", live.revision, "--reason", "r"],
        { kbRoot, stdin: candidate },
      );
      const file = created.stdout.match(/proposals\/bodies\/(\S+\.json)/)?.[1];
      expect(file).toBeDefined();

      const shown = await againstServer(["proposals", "show", file!], { kbRoot });
      expect(shown.exitCode).toBe(0);
      expect(shown.stdout).toContain("+追加的一段。");

      const merged = await againstServer(["proposals", "merge", file!], { kbRoot });
      expect(merged.exitCode).toBe(0);
      expect(merged.stdout).toContain("graph.json and MANIFEST.md are unchanged");

      const after = await againstServer(["concept", "body", "risk", "--format", "markdown"], { kbRoot });
      expect(after.stdout).toBe(candidate);
    } finally {
      await rm(kbRoot, { recursive: true, force: true });
    }
  });

  test("an empty --file is refused before any request", async () => {
    const dir = await mkdtemp(join(tmpdir(), "x-sage-body-"));
    const file = join(dir, "empty.md");
    await writeFile(file, "   \n");
    try {
      const { stderr, exitCode } = await againstServer([
        "refine-body", "risk", "--file", file, "--expected-revision", "x", "--reason", "r",
      ]);
      expect(exitCode).toBe(1);
      expect(stderr).toContain("empty");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

/**
 * `--version` is a local command: no server, no config, no network.
 *
 * The point of these cases is that build identity must be answerable even when
 * everything else is broken — that is precisely when a user asks "what am I
 * even running?". A version probe that needs a reachable server would be
 * useless in the situation it is most needed.
 */
describe("--version", () => {
  test("prints a build triple without any server or config", async () => {
    const result = await run(["--version"], { XDG_CONFIG_HOME: join(tmpdir(), "x-sage-no-cfg") });
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("x-sage CLI");
    expect(result.stdout).toContain("commit");
    expect(result.stdout).toContain("ref");
    expect(result.stderr).toBe("");
  });

  test("version is an alias for --version", async () => {
    const viaFlag = await run(["--version"]);
    const viaWord = await run(["version"]);
    expect(viaWord.exitCode).toBe(0);
    expect(viaWord.stdout).toBe(viaFlag.stdout);
  });

  test("works when the server is unreachable — it must not dial out", async () => {
    // Port 9 (discard) is closed everywhere; if --version tried a request it
    // would surface a connection error instead of the triple.
    const result = await run(["--version", "--server", "http://127.0.0.1:9"]);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toContain("commit");
  });

  test("reports the installed manifest identity when one is present", async () => {
    const dir = await mkdtemp(join(tmpdir(), "x-sage-manifest-"));
    try {
      const commit = "a".repeat(40);
      await writeFile(
        join(dir, "build-manifest.json"),
        JSON.stringify({ version: "0.1.0", commit, ref: "v0.1.0" }),
      );
      const result = await run(["--version"], { X_SAGE_BUILD_MANIFEST: join(dir, "build-manifest.json") });
      expect(result.exitCode).toBe(0);
      expect(result.stdout).toContain(commit);
      expect(result.stdout).toContain("v0.1.0");
      // A manifest is the trusted install case, so the source token is omitted.
      expect(result.stdout).not.toContain("source manifest");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

/**
 * `knowledge` subcommands.
 *
 * The structured surface is the second authority, so these assert the same
 * client contract: one REST request per invocation, server-rendered text, exit
 * code from the status. A real fixture KB keeps the expected claims stable.
 */
describe("public sage-db compatibility", () => {
  test("covers the catalog, scoped metadata, graphs, concept creation and JSON proposals", async () => {
    const requests: Array<{ method: string; target: string; body: string }> = [];
    const server = Bun.serve({
      port: 0,
      async fetch(request) {
        const url = new URL(request.url);
        requests.push({ method: request.method, target: url.pathname + url.search, body: await request.text() });
        return Response.json({ nodes: [], edges: [] }, { status: request.method === "POST" ? 201 : 200 });
      },
    });
    const output = await mkdtemp(join(tmpdir(), "x-sage-graph-export-"));
    try {
      const args = ["--server", server.url.href];
      const document = { frontmatter: { id: "fixture" }, body: "# Fixture" };
      expect((await run([...args, "kb", "list"])).exitCode).toBe(0);
      expect((await run([...args, "--kb", "sample", "meta"])).exitCode).toBe(0);
      expect((await run([...args, "graph", "read", "--kb", "sample"])).exitCode).toBe(0);
      const file = join(output, "graph.json");
      expect((await run([...args, "graph", "export", file, "--semantic"])).exitCode).toBe(0);
      expect(JSON.parse(await readFile(file, "utf8"))).toEqual({ nodes: [], edges: [] });
      expect((await run([...args, "concept", "create", "-", "--kb", "sample"], {}, JSON.stringify(document))).exitCode).toBe(0);
      expect((await run([...args, "proposals", "list", "--json"])).exitCode).toBe(0);
      expect((await run([...args, "proposals", "merge", "one", "--json"])).exitCode).toBe(0);
      expect(requests).toEqual([
        { method: "GET", target: "/api/knowledge-bases", body: "" },
        { method: "GET", target: "/api/meta?kb=sample", body: "" },
        { method: "GET", target: "/api/graph?kb=sample", body: "" },
        { method: "GET", target: "/api/graph?semantic=true", body: "" },
        { method: "POST", target: "/api/concepts?kb=sample", body: JSON.stringify(document) },
        { method: "GET", target: "/api/proposals?format=json", body: "" },
        { method: "POST", target: "/api/proposals/merge?format=json", body: '{"file":"one"}' },
      ]);
    } finally {
      server.stop(true);
      await rm(output, { recursive: true, force: true });
    }
  });

  test("reports health and retires the old commands locally", async () => {
    const requests: string[] = [];
    let ready = true;
    const server = Bun.serve({
      port: 0,
      fetch(request) {
        const path = new URL(request.url).pathname;
        requests.push(path);
        expect(path).toBe("/api/health");
        return Response.json({ ok: ready, authority: "sage-db", databaseReady: ready }, { status: ready ? 200 : 503 });
      },
    });
    try {
      const args = ["--server", server.url.href];
      const status = await run([...args, "status"]);
      expect(status.exitCode).toBe(0);
      expect(JSON.parse(status.stdout)).toMatchObject({ authority: "sage-db", databaseReady: true });
      ready = false;
      const unready = await run([...args, "status"]);
      expect(unready.exitCode).toBe(1);
      expect(JSON.parse(unready.stderr)).toMatchObject({ authority: "sage-db", databaseReady: false });
      const retired = await run([...args, "knowledge", "import"]);
      expect(retired.exitCode).toBe(1);
      expect(retired.stderr).toContain("have been retired");
      expect(requests).toEqual(["/api/health", "/api/health"]);
    } finally {
      server.stop(true);
    }
  });

  test("explains unavailable surfaces and keeps an HTML write outcome inconclusive", async () => {
    const requests: string[] = [];
    const server = Bun.serve({
      port: 0,
      fetch(request) {
        const path = new URL(request.url).pathname;
        requests.push(path);
        if (path.startsWith("/sage.v1.")) {
          return new Response("<html>Workbench</html>", { headers: { "content-type": "text/html" } });
        }
        return new Response("Operation is not connected to x-sage-db\n", { status: 503 });
      },
    });
    try {
      const args = ["--server", server.url.href];
      const checkpoint = await run([...args, "native", "checkpoint", "agent"]);
      const handshake = await run([...args, "native", "handshake", "-"], {}, "{}");
      const commit = await run([...args, "native", "commit", "-"], {}, "{}");
      expect(checkpoint.exitCode).toBe(1);
      expect(checkpoint.stderr).toContain("x-sage checkpoint read");
      expect(handshake.exitCode).toBe(1);
      expect(handshake.stderr).toContain("private Rust endpoint");
      expect(commit.exitCode).toBe(2);
      expect(commit.stderr).toContain("original request id");
      expect(requests).toEqual([
        "/api/knowledge/checkpoints/agent",
        "/sage.v1.SageCommandService/Handshake", "/sage.v1.SageCommandService/CommitCommand",
      ]);
    } finally {
      server.stop(true);
    }
  });
});

describe("native commands", () => {
  test("sends each private read and extra write to its registered path and media type", async () => {
    const seen: Array<[string, string, string | null, string]> = [];
    const server = Bun.serve({
      port: 0,
      async fetch(request) {
        const url = new URL(request.url);
        seen.push([request.method, url.pathname + url.search, request.headers.get("content-type"), await request.text()]);
        if (request.headers.get("accept") === "application/octet-stream") {
          return new Response("fixture bytes", { headers: { "content-type": "application/octet-stream" } });
        }
        if (request.method === "PUT") return new Response(null, { status: 204 });
        return Response.json({ outcome: { committed: { recorded: {} } } });
      },
    });
    try {
      const args = ["--server", server.url.href, "native"];
      const rpc = [
        ["read-term-text", "ReadTermText"], ["read-term-document", "ReadTermDocument"],
        ["read-term-activation", "ReadTermActivation"], ["read-graph-snapshot", "ReadGraphSnapshot"],
        ["read-proposal-files", "ReadProposalFiles"], ["submit-large-body", "SubmitLargeBodyProposal"],
        ["commit-direct-concept", "CommitDirectConcept"],
      ] as const;
      for (const [command] of rpc) expect((await run([...args, command, "-"], {}, "{}")).exitCode).toBe(0);
      expect((await run([...args, "revision", "agent", "--revision", "5"])).exitCode).toBe(0);
      expect((await run([...args, "proposals", "prp_fixture", "--preview"])).exitCode).toBe(0);
      expect((await run([...args, "frontier"])).exitCode).toBe(0);
      expect((await run([...args, "frontier-document", "read"])).stdout).toBe("fixture bytes");
      expect((await run([...args, "checkpoint-document", "read", "-"], {}, "inspect")).stdout).toBe("fixture bytes");
      expect((await run([...args, "frontier-document", "write", "-"], {}, "frontier")).exitCode).toBe(0);
      expect((await run([...args, "checkpoint-document", "write", "-"], {}, "checkpoint")).exitCode).toBe(0);
      expect(seen).toEqual([
        ...rpc.map<[string, string, string, string]>(([, path], i) => [
          "POST", "/sage.v1.SageCommandService/" + path,
          i < 5 ? "application/json" : "application/octet-stream", "{}",
        ]),
        ["GET", "/api/revisions?id=agent&revision=5", null, ""],
        ["GET", "/api/knowledge/proposals/prp_fixture?preview=true", null, ""],
        ["GET", "/api/knowledge/frontier", null, ""],
        ["GET", "/db/v1/frontier-document", null, ""],
        ["POST", "/db/v1/checkpoint-document", "application/octet-stream", "inspect"],
        ["PUT", "/db/v1/frontier-document", "application/octet-stream", "frontier"],
        ["PUT", "/db/v1/checkpoint-document", "application/octet-stream", "checkpoint"],
      ]);
    } finally {
      server.stop(true);
    }
  });

  test("forwards literal request bytes with auth over exactly one RPC", async () => {
    const raw = '{\n "envelope":{ "requestId":"q","requestId":"q2" },\n "payload":{}\n}\n';
    let requests = 0;
    const server = Bun.serve({
      port: 0,
      async fetch(request) {
        requests++;
        expect(new URL(request.url).pathname).toBe("/sage.v1.SageCommandService/SubmitProposal");
        expect(request.method).toBe("POST");
        expect(request.headers.get("authorization")).toBe("Bearer author");
        expect(request.headers.get("content-type")).toBe("application/json");
        expect(await request.text()).toBe(raw);
        return Response.json({ error: "duplicate field" }, { status: 400 });
      },
    });
    try {
      const result = await run(["--server", server.url.href, "--token", "author", "native", "submit", "-"], {}, raw);
      expect(requests).toBe(1);
      expect(result.exitCode).toBe(1);
      expect(result.stdout).toBe("");
      expect(JSON.parse(result.stderr)).toEqual({ error: "duplicate field" });
    } finally {
      server.stop(true);
    }
  });

  test("uses exit 2 for an unconfirmed write or an incomplete outcome lookup", async () => {
    let requests = 0;
    const server = Bun.serve({
      port: 0,
      fetch(request) {
        requests++;
        const path = new URL(request.url).pathname;
        if (path.endsWith("/CommitCommand")) return Response.json({ outcome: { unknown: { requestId: "r" } } });
        expect(path).toBe("/sage.v1.SageCommandService/GetRequestOutcome");
        return Response.json({ incomplete: { reason: "unavailable" } });
      },
    });
    try {
      const args = ["--server", server.url.href, "native"];
      const request = '{"envelope":{"requestId":"r"}}';
      const commit = await run([...args, "commit", "-"], {}, request);
      const outcome = await run([...args, "outcome", "-"], {}, request);
      expect(commit.exitCode).toBe(2);
      expect(outcome.exitCode).toBe(2);
      expect(JSON.parse(commit.stdout).outcome.unknown.requestId).toBe("r");
      expect(JSON.parse(outcome.stdout).incomplete.reason).toBe("unavailable");
      expect(requests).toBe(2);
    } finally {
      server.stop(true);
    }
  });

  test("reads one native checkpoint by explicit session", async () => {
    let requests = 0;
    const server = Bun.serve({
      port: 0,
      fetch(request) {
        requests++;
        const url = new URL(request.url);
        expect(request.method).toBe("GET");
        expect(url.pathname).toBe("/api/knowledge/checkpoints/agent");
        expect(url.searchParams.get("session")).toBe("2");
        return Response.json({ session: 2, content: "saved" });
      },
    });
    try {
      const result = await run(["--server", server.url.href, "native", "checkpoint", "agent", "2"]);
      expect(result.exitCode).toBe(0);
      expect(JSON.parse(result.stdout)).toEqual({ session: 2, content: "saved" });
      expect(requests).toBe(1);
    } finally {
      server.stop(true);
    }
  });
});
