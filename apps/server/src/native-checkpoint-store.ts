import { randomBytes } from "node:crypto";
import { CheckpointSchema, type Checkpoint } from "@x-sage/core";
import type { NativeCheckpointStore } from "../../../packages/service/src/api.ts";

const PATH = "/db/v1/checkpoint-document";
const MAX_SLUG_BYTES = 8 * 1024;
const MAX_DOCUMENT_BYTES = 256 * 1024;

function unavailable(): Error {
  return new Error("x-sage-db checkpoint store unavailable");
}

/** One private checkpoint connection for the configured KB and two scoped credentials. */
export function connectNativeCheckpointStore(options: {
  url: string;
  kbUuid: string;
  readToken: string;
  writeToken: string;
}): NativeCheckpointStore {
  const origin = new URL(options.url);
  if (origin.protocol !== "http:" || !["127.0.0.1", "[::1]"].includes(origin.hostname)
      || origin.pathname !== "/" || origin.username || origin.password || origin.search || origin.hash) {
    throw new Error("Checkpoint store requires a private loopback origin");
  }
  if (!/^[0-9a-f]{32}$/.test(options.kbUuid) || !/^[0-9a-f]{64}$/.test(options.readToken)
      || !/^[0-9a-f]{64}$/.test(options.writeToken) || options.readToken === options.writeToken) {
    throw new Error("Invalid checkpoint identity or scoped credential");
  }

  const kb = Buffer.from(options.kbUuid, "hex");
  const target = new URL(PATH, origin);
  function slot(concept: string, session: number | undefined) {
    const slug = Buffer.from(concept, "utf8");
    if (!slug.length || slug.length > MAX_SLUG_BYTES || !/^[a-z0-9][a-z0-9-]*$/.test(concept)
        || (session !== undefined && (!Number.isSafeInteger(session) || session < 1))) {
      throw unavailable();
    }
    const prefix = Buffer.alloc(26 + slug.length);
    kb.copy(prefix);
    prefix.writeBigUInt64LE(BigInt(session ?? 0), 16);
    prefix.writeUInt16LE(slug.length, 24);
    slug.copy(prefix, 26);
    return prefix;
  }

  function send(method: "POST" | "PUT", body: Uint8Array<ArrayBuffer>, token: string, signal: AbortSignal) {
    return fetch(target, {
      method,
      headers: { "authorization": `Bearer ${token}`, "content-type": "application/octet-stream" },
      body,
      redirect: "error",
      signal,
    });
  }

  return {
    async inspect(concept, session, withContent) {
      const prefix = slot(concept, session);
      const response = await send(
        "POST", Buffer.concat([prefix, Buffer.of(Number(withContent))]), options.readToken, AbortSignal.timeout(5000),
      );
      const length = response.headers.get("content-length");
      if (!response.ok || !length || !/^[0-9]+$/.test(length)
          || Number(length) < 32 || Number(length) > 32 + MAX_DOCUMENT_BYTES) {
        await response.body?.cancel();
        throw unavailable();
      }
      const bytes = Buffer.from(await response.arrayBuffer());
      if (bytes.length !== Number(length) || !kb.equals(bytes.subarray(0, 16))) throw unavailable();
      const version = bytes.readBigUInt64LE(16);
      const ordinal = bytes.readBigUInt64LE(24);
      if (version === 0n || ordinal > BigInt(Number.MAX_SAFE_INTEGER)
          || (session !== undefined && ordinal !== 0n && ordinal !== BigInt(session))) throw unavailable();
      if (!withContent || ordinal === 0n) {
        if (bytes.length !== 32) throw unavailable();
        return { version: version.toString(), ...(ordinal === 0n ? {} : { session: Number(ordinal) }) };
      }
      const value = CheckpointSchema.parse(JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes.subarray(32))));
      if (value.concept !== concept || value.session !== Number(ordinal)) throw unavailable();
      return { version: version.toString(), session: value.session, checkpoint: value };
    },

    async upsert(version, checkpoint: Checkpoint) {
      if (!/^[1-9][0-9]*$/.test(version) || BigInt(version) > 0xffffffffffffffffn) throw unavailable();
      const state = Buffer.from(JSON.stringify(checkpoint));
      if (state.length > MAX_DOCUMENT_BYTES) throw unavailable();
      const expected = Buffer.alloc(8);
      expected.writeBigUInt64LE(BigInt(version));
      const identity = Buffer.from(randomBytes(16).toString("hex"));
      const frame = Buffer.concat([slot(checkpoint.concept, checkpoint.session), expected, identity, state]);
      const signal = AbortSignal.timeout(5000);
      for (let attempt = 0; attempt < 2; attempt++) {
        const response = await send("PUT", frame, options.writeToken, signal).catch(() => null);
        if (response === null) continue;
        await response.body?.cancel();
        if (response.status === 204) return "committed";
        if (response.status === 412) return "stale";
        if (response.status !== 503) break;
      }
      throw unavailable();
    },
  };
}
