import { randomUUID } from "node:crypto";
import type { NativeTermReader } from "@x-sage/service";

const DOCUMENT_BYTES = 16 * 1024 * 1024;
const READ_PREFIX = 25;
const WRITE_PREFIX = 56;
const PATH = "/db/v1/frontier-document";
const OCTETS = "application/octet-stream";

function unavailable(): Error {
  return new Error("x-sage-db Frontier document is unavailable");
}

/** Connect the Bun Frontier routes to one configured native store on loopback. */
export function connectNativeFrontierDocument(options: {
  url: string;
  kbUuid: string;
  readToken: string;
  writeToken: string;
}): NonNullable<NativeTermReader["frontier"]> {
  const origin = new URL(options.url);
  if (
    origin.protocol !== "http:" ||
    !["127.0.0.1", "[::1]"].includes(origin.hostname) ||
    origin.pathname !== "/" ||
    origin.username || origin.password || origin.search || origin.hash
  ) {
    throw new Error("x-sage-db Frontier requires a private loopback origin");
  }
  if (!/^[0-9a-f]{32}$/.test(options.kbUuid) ||
      !/^[0-9a-f]{64}$/.test(options.readToken) || !/^[0-9a-f]{64}$/.test(options.writeToken)) {
    throw new Error("Invalid x-sage-db Frontier identity or credential");
  }
  const target = new URL(PATH, origin);
  const kb = Buffer.from(options.kbUuid, "hex");
  const decoder = new TextDecoder("utf-8", { ignoreBOM: true });
  const encoder = new TextEncoder();

  return {
    async read() {
      const response = await fetch(target, {
        headers: { authorization: `Bearer ${options.readToken}` },
        redirect: "error",
        signal: AbortSignal.timeout(5000),
      });
      const length = response.headers.get("content-length");
      const expected = length && /^[0-9]+$/.test(length) ? Number(length) : 0;
      if (response.status !== 200 || response.headers.get("content-type") !== OCTETS ||
          expected < READ_PREFIX || expected > READ_PREFIX + DOCUMENT_BYTES) {
        await response.body?.cancel();
        throw unavailable();
      }
      const body = await response.arrayBuffer();
      const bytes = new Uint8Array(body);
      if (body.byteLength !== expected || !kb.equals(bytes.subarray(0, 16))) throw unavailable();
      const revision = new DataView(body).getBigUint64(16, true);
      const present = bytes[24];
      if (revision === 0n || (present !== 0 && present !== 1) ||
          (present === 0 && expected !== READ_PREFIX)) {
        throw unavailable();
      }
      return { revision, raw: present === 0 ? null : decoder.decode(bytes.subarray(READ_PREFIX)) };
    },

    async replace(expected, document) {
      const content = encoder.encode(document);
      if (content.byteLength > DOCUMENT_BYTES) throw unavailable();
      const requestId = randomUUID().replaceAll("-", "");
      const frame = new Uint8Array(WRITE_PREFIX + content.byteLength);
      frame.set(kb);
      new DataView(frame.buffer).setBigUint64(16, expected, true);
      frame.set(encoder.encode(requestId), 24);
      frame.set(content, WRITE_PREFIX);
      const send = () => fetch(target, {
        method: "PUT",
        headers: {
          authorization: `Bearer ${options.writeToken}`,
          "content-type": OCTETS,
        },
        body: frame,
        redirect: "error",
        signal: AbortSignal.timeout(5000),
      });
      // A broken response is ambiguous. Probe the same id and bytes before
      // allowing the caller to interpret a new CAS result.
      const response = await send().catch(() => send());
      if (response.status === 204) return true;
      await response.body?.cancel();
      if (response.status === 412) return false;
      throw unavailable();
    },
  };
}
