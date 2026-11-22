import { expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createApp } from "./app.ts";
import { connectNativeStartup, requireActiveCoverage } from "./native-startup.ts";

const SHA = "ab".repeat(32);
const KB = "77".repeat(16);

test("startup checks product coverage, release identity, and the exact scope of every credential", async () => {
  const directory = await mkdtemp(join(tmpdir(), "sage-native-startup-"));
  const read = "12".repeat(32);
  const concept = "34".repeat(32);
  const aux = "56".repeat(32);
  const propose = "78".repeat(32);
  const review = "90".repeat(32);
  const capabilities = new Map([
    [read, "read"], [concept, "write_concept"], [aux, "write_aux"], [propose, "propose"], [review, "review"],
  ]);
  let ready = true;
  let source = SHA;
  let broadAux = false;
  const observations: string[] = [];

  function json(body: unknown) {
    const data = JSON.stringify(body);
    return new Response(data, {
      headers: {
        "content-length": String(Buffer.byteLength(data)),
        "content-type": "application/json",
      },
    });
  }
  const db = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      const route = new URL(request.url).pathname;
      if (route === "/api/health") {
        return json({ engine: "sage-db", sourceIdentity: source, ok: true, readReady: true, writeReady: ready });
      }
      const token = request.headers.get("authorization")?.replace("Bearer ", "");
      if (!token || !capabilities.has(token)) return new Response(null, { status: 401 });
      if (route === "/api/knowledge-bases") {
        expect(token).toBe(read);
        return json({ knowledgeBases: [{ id: "default", name: "Authoritative DB name", isDefault: true }] });
      }
      expect(route).toBe("/sage.v1.SageCommandService/Handshake");
      const capability = capabilities.get(token)!;
      observations.push(capability);
      const { envelope } = await request.json() as { envelope: { kbUuid: string; protocolVersion: number } };
      expect(envelope.kbUuid).toBe(KB);
      expect(envelope.protocolVersion).toBe(1);
      return json({ sourceIdentity: source, capabilities: broadAux && capability === "write_aux"
        ? [capability, "review"] : [capability] });
    },
  });

  try {
    const paths = ["read", "concept", "aux"].map((name) => join(directory, name));
    await Promise.all([writeFile(paths[0]!, read), writeFile(paths[1]!, concept), writeFile(paths[2]!, aux)]);
    const env = {
      X_SAGE_DB_MODE: "rehearsal",
      X_SAGE_DB_URL: db.url.href,
      X_SAGE_DB_KB_UUID: KB,
      X_SAGE_DB_KB_SLUG: "default",
      X_SAGE_DB_SOURCE_IDENTITY: SHA,
      X_SAGE_DB_READ_TOKEN_FILE: paths[0]!,
      X_SAGE_DB_CONCEPT_TOKEN_FILE: paths[1]!,
      X_SAGE_DB_AUX_TOKEN_FILE: paths[2]!,
      X_SAGE_DB_PROJECTION_DIR: join(directory, "projection"),
    };
    await expect(connectNativeStartup({ X_SAGE_DB_URL: db.url.href }, "127.0.0.1", "public"))
      .rejects.toThrow("explicit rehearsal or active");
    await expect(connectNativeStartup(env, "0.0.0.0", "public")).rejects.toThrow("loopback");
    await expect(connectNativeStartup({ ...env, X_SAGE_DB_MODE: "active" }, "127.0.0.1", "public"))
      .rejects.toThrow("proposal submission");
    expect(observations).toEqual([]);
    source = "12".repeat(32);
    await expect(connectNativeStartup(env, "127.0.0.1", "public")).rejects.toThrow("expected source identity");
    source = SHA;
    broadAux = true;
    await expect(connectNativeStartup(env, "127.0.0.1", "public")).rejects.toThrow("scoped credential");
    broadAux = false;

    const native = (await connectNativeStartup(env, "127.0.0.1", "public"))!;
    expect(native.reader.kbName).toBe("Authoritative DB name");
    expect(native.reader.createConcept).toBeFunction();
    expect(native.reader.readActivation).toBeFunction();
    expect(native.reader.readGraphSnapshot).toBeFunction();
    expect(native.reader.publishArtifacts).toBeFunction();
    expect(native.reader.frontier?.read).toBeFunction();
    expect(native.reader.checkpoints?.inspect).toBeFunction();
    expect(new Set(observations)).toEqual(new Set(["read", "write_aux", "write_concept"]));

    const app = createApp({ kbRoot: directory, nativeTermReader: native.reader, nativeHealth: native.health });
    const healthyDb = await app.request("/api/health");
    expect(healthyDb.status).toBe(503);
    expect(await healthyDb.json()).toMatchObject({ databaseReady: true, mode: "rehearsal", ok: false });
    ready = false;
    const unavailableDb = await app.request("/api/health");
    expect(unavailableDb.status).toBe(503);
    expect(await unavailableDb.json()).toMatchObject({ databaseReady: false, ok: false });

    const complete = {
      ...native.reader,
      readProposalFiles: async () => ({
        lists: { edges: [], atoms: [], bodies: [] },
        readBody: async () => ({ body: "", revision: "fixture" }),
      }),
      submitProposal: {
        edge: async () => "edge.md",
        atom: async () => "atom.md",
        body: async () => "body.json",
      },
      proposalMerge: {
        list: async () => ({ edges: [], atoms: [], bodies: [] }),
        decide: async () => ({ kind: "applied" as const }),
      },
    };
    expect(() => requireActiveCoverage(complete)).not.toThrow();

    ready = true;
    const proposeFile = join(directory, "propose");
    const reviewFile = join(directory, "review");
    await Promise.all([writeFile(proposeFile, propose), writeFile(reviewFile, review)]);
    const active = {
      ...env,
      X_SAGE_DB_MODE: "active",
      X_SAGE_DB_PROPOSE_TOKEN_FILE: proposeFile,
      X_SAGE_DB_REVIEW_TOKEN_FILE: reviewFile,
      GIT_COMMIT_HASH: "abc1234",
      BUILD_REF: "test-active",
      X_SAGE_DB_EXPECTED_BUN_COMMIT: "abc1234",
      X_SAGE_DB_EXPECTED_BUN_REF: "test-active",
    };
    await expect(connectNativeStartup({ ...active, X_SAGE_DB_REVIEW_TOKEN_FILE: proposeFile }, "127.0.0.1", "public"))
      .rejects.toThrow("distinct credentials");
    capabilities.set(review, "propose");
    await expect(connectNativeStartup(active, "127.0.0.1", "public")).rejects.toThrow("scoped credential");
    capabilities.set(review, "review");
    const running = await connectNativeStartup(active, "127.0.0.1", "public");
    expect(running?.reader.submitProposal?.body).toBeFunction();
    expect(running?.reader.proposalMerge?.decide).toBeFunction();
    expect(await running?.health()).toMatchObject({ mode: "active", ok: true, databaseReady: true });
    expect(new Set(observations)).toEqual(new Set(["read", "write_aux", "write_concept", "propose", "review"]));

    const { readActivation: _omitted, ...partial } = native.reader;
    expect(() => requireActiveCoverage(partial)).toThrow("activation");
    const candidateWithoutActivation = createApp({ kbRoot: directory, nativeTermReader: partial });
    expect((await candidateWithoutActivation.request("/api/health")).status).toBe(503);
    expect((await candidateWithoutActivation.request("/api/concepts/decision")).status).toBe(503);
    expect((await createApp({ kbRoot: directory }).request("/api/health")).status).toBe(200);
  } finally {
    await db.stop(true);
    await rm(directory, { recursive: true, force: true });
  }
});
