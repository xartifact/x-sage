import { randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import { isAbsolute } from "node:path";
import type { NativeTermReader } from "@x-sage/service";
import { readBuildInfo } from "./build-info.ts";
import { connectNativeCheckpointStore } from "./native-checkpoint-store.ts";
import { connectNativeDerivedArtifacts } from "./native-derived-artifacts.ts";
import { connectNativeFrontierDocument } from "./native-frontier-document.ts";
import { connectNativeTermReader } from "./native-term-reader.ts";

type NativeMode = "rehearsal" | "active";
type Capability = "read" | "write_aux" | "write_concept" | "propose" | "review";

export interface NativeHealth {
  ok: boolean;
  authority: "sage-db";
  mode: NativeMode;
  databaseReady: boolean;
  databaseSourceIdentity: string;
}

export interface NativeStartup {
  reader: NativeTermReader;
  health(): Promise<NativeHealth>;
}

const MAX_PROBE_BODY = 16 * 1024;

function required(env: NodeJS.ProcessEnv, key: string): string {
  const value = env[key];
  if (!value?.length) throw new Error(`Native database startup requires ${key}`);
  return value;
}

function readToken(env: NodeJS.ProcessEnv, key: string): string {
  const path = required(env, key);
  if (!isAbsolute(path)) throw new Error(`${key} must be absolute`);
  const token = readFileSync(path, "utf8").trim();
  if (!/^[0-9a-f]{64}$/.test(token)) throw new Error(`${key} contains an invalid credential`);
  return token;
}

function record(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

async function probe(endpoint: URL, request?: RequestInit): Promise<Record<string, unknown>> {
  const response = await fetch(endpoint, {
    ...request,
    redirect: "error",
    signal: AbortSignal.timeout(4500),
  });
  const length = response.headers.get("content-length");
  if (!response.ok || length === null || !/^[0-9]+$/.test(length) || Number(length) > MAX_PROBE_BODY) {
    await response.body?.cancel();
    throw new Error("Native database startup probe refused");
  }
  const bytes = await response.arrayBuffer();
  if (bytes.byteLength !== Number(length)) throw new Error("Native database probe body does not match its length");
  const body = record(JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)));
  if (!body) throw new Error("Native database probe returned a malformed object");
  return body;
}

/** Complete product coverage is checked against the assembled adapter, never an environment label. */
export function requireActiveCoverage(reader: NativeTermReader): void {
  const missing = [];
  if (!reader.frontier) missing.push("Frontier");
  if (!reader.checkpoints) missing.push("checkpoint");
  if (!reader.publishArtifacts) missing.push("projection");
  if (!reader.createConcept) missing.push("concept create");
  if (!reader.readActivation) missing.push("activation");
  if (!reader.readGraphSnapshot) missing.push("graph reads");

  if (!reader.readProposalFiles) missing.push("proposal list/detail");
  if (typeof reader.submitProposal?.edge !== "function" ||
      typeof reader.submitProposal.atom !== "function" ||
      typeof reader.submitProposal.body !== "function") missing.push("proposal submission");
  if (typeof reader.proposalMerge?.list !== "function" ||
      typeof reader.proposalMerge.decide !== "function") missing.push("proposal merge");
  if (missing.length > 0) throw new Error(`Native database activation refused, missing ${missing.join(", ")}`);
}

/** Assemble the Bun product over one explicitly configured loopback database. */
export async function connectNativeStartup(
  env: NodeJS.ProcessEnv,
  hostname: string,
  publicWriteToken: string | undefined,
): Promise<NativeStartup | undefined> {
  const mode = env.X_SAGE_DB_MODE;
  if (mode === undefined && !Object.keys(env).some((name) => name.startsWith("X_SAGE_DB_"))) return undefined;
  if (mode !== "rehearsal" && mode !== "active") {
    throw new Error("Native database startup requires explicit rehearsal or active mode");
  }
  if (mode === "rehearsal" && !["127.0.0.1", "::1"].includes(hostname)) {
    throw new Error("Native database rehearsal requires a public listener on loopback");
  }
  if (!publicWriteToken) throw new Error("Native database mode requires the Bun public write token");
  const url = required(env, "X_SAGE_DB_URL");
  const kbUuid = required(env, "X_SAGE_DB_KB_UUID");
  const kbSlug = required(env, "X_SAGE_DB_KB_SLUG");
  const sourceIdentity = required(env, "X_SAGE_DB_SOURCE_IDENTITY");
  if (!/^[0-9a-f]{64}$/.test(sourceIdentity)) throw new Error("Native database source identity must be lowercase SHA-256");
  const read = readToken(env, "X_SAGE_DB_READ_TOKEN_FILE");
  const aux = readToken(env, "X_SAGE_DB_AUX_TOKEN_FILE");
  const concept = readToken(env, "X_SAGE_DB_CONCEPT_TOKEN_FILE");
  const proposalFiles = [env.X_SAGE_DB_PROPOSE_TOKEN_FILE, env.X_SAGE_DB_REVIEW_TOKEN_FILE];
  const proposalCredentials = proposalFiles.some(Boolean) ? {
    proposalProposeToken: readToken(env, "X_SAGE_DB_PROPOSE_TOKEN_FILE"),
    proposalReviewToken: readToken(env, "X_SAGE_DB_REVIEW_TOKEN_FILE"),
  } : undefined;
  const roles: Array<[string, Capability]> = [[read, "read"], [aux, "write_aux"], [concept, "write_concept"]];
  if (proposalCredentials) {
    roles.push([proposalCredentials.proposalProposeToken, "propose"],
      [proposalCredentials.proposalReviewToken, "review"]);
  }
  if (new Set([...roles.map(([token]) => token), publicWriteToken]).size !== roles.length + 1) {
    throw new Error("The public and private database roles require distinct credentials");
  }
  const reader = {
    ...connectNativeTermReader({
      url, kbUuid, kbSlug, token: read, writeConceptToken: concept, ...proposalCredentials,
    }),
    frontier: connectNativeFrontierDocument({ url, kbUuid, readToken: read, writeToken: aux }),
    checkpoints: connectNativeCheckpointStore({ url, kbUuid, readToken: read, writeToken: aux }),
    publishArtifacts: connectNativeDerivedArtifacts(required(env, "X_SAGE_DB_PROJECTION_DIR")),
  };

  if (mode === "active") {
    requireActiveCoverage(reader);
    if (!proposalCredentials) throw new Error("Native database activation requires proposal credentials");
    const build = readBuildInfo(env);
    if (build.commitHash === "unknown" || build.buildRef === "unknown" ||
        build.commitHash !== required(env, "X_SAGE_DB_EXPECTED_BUN_COMMIT") ||
        build.buildRef !== required(env, "X_SAGE_DB_EXPECTED_BUN_REF")) {
      throw new Error("Native database mode requires the intended deployed Bun build");
    }
  }

  const target = new URL(url);
  async function ready() {
    const body = await probe(new URL("/api/health", target));
    if (body.engine !== "sage-db" || body.ok !== true || body.readReady !== true ||
        body.writeReady !== true || body.sourceIdentity !== sourceIdentity) {
      throw new Error("Native database is not Ready at the expected source identity");
    }
  }

  async function handshake(token: string, expectedCapability: Capability) {
    const body = await probe(new URL("/sage.v1.SageCommandService/Handshake", target), {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: JSON.stringify({
        envelope: {
          protocolVersion: 1, kbUuid, requestId: randomUUID(), deadlineMs: 4000, readSet: [],
        },
      }),
    });
    if (body.sourceIdentity !== sourceIdentity || !Array.isArray(body.capabilities) ||
        body.capabilities.length !== 1 || body.capabilities[0] !== expectedCapability) {
      throw new Error("Native database identity or scoped credential capability disagrees");
    }
  }

  async function checkRoles() {
    await Promise.all(roles.map(([token, capability]) => handshake(token, capability)));
  }

  await ready();
  await checkRoles();
  const catalog = await probe(new URL("/api/knowledge-bases", target), {
    headers: { authorization: `Bearer ${read}` },
  });
  const entry = Array.isArray(catalog.knowledgeBases) && catalog.knowledgeBases.length === 1
    ? record(catalog.knowledgeBases[0])
    : undefined;
  if (entry?.id !== kbSlug || entry.isDefault !== true || typeof entry.name !== "string") {
    throw new Error("Native database startup found a different KB catalog");
  }

  return {
    reader: { ...reader, kbName: entry.name },
    async health() {
      let databaseReady = false;
      try {
        await ready();
        await checkRoles();
        databaseReady = true;
      } catch {
        // A failed lookup is observable as an unhealthy Bun service.
      }
      return {
        ok: mode === "active" && databaseReady,
        authority: "sage-db",
        mode,
        databaseReady,
        databaseSourceIdentity: sourceIdentity,
      };
    },
  };
}
