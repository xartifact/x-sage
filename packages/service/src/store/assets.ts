import { createHash, randomBytes } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { assetRoot } from "./schema.ts";
import { IMAGE_MEDIA_TYPES, MAX_EVIDENCE_BYTES } from "@x-sage/core/knowledge";
import type { EvidenceAsset } from "@x-sage/core/knowledge";

/**
 * Content-addressed asset storage.
 *
 * Bytes are stored under `<kb>/assets/<sha256>`, so the same screenshot attached
 * to two claims occupies one file and a hash mismatch on read is detectable.
 * Nothing here derives a path from a client string: the filename *is* the hash,
 * recomputed from the bytes that arrived.
 */

const MAGIC: Array<{ mediaType: (typeof IMAGE_MEDIA_TYPES)[number]; signature: number[] }> = [
  { mediaType: "image/png", signature: [0x89, 0x50, 0x4e, 0x47] },
  { mediaType: "image/jpeg", signature: [0xff, 0xd8, 0xff] },
  { mediaType: "image/webp", signature: [0x52, 0x49, 0x46, 0x46] },
  { mediaType: "image/gif", signature: [0x47, 0x49, 0x46, 0x38] },
];

/**
 * Identify an image from its first bytes.
 *
 * The declared content type is a claim by the uploader; the bytes are evidence.
 * Storing a mismatched pair would make `GET /assets/<sha>` serve a PNG labelled
 * as a PDF — a browser-decoded HTML payload with an image content type is the
 * classic stored-XSS shape.
 */
export function sniffImageMediaType(bytes: Uint8Array): (typeof IMAGE_MEDIA_TYPES)[number] | null {
  for (const candidate of MAGIC) {
    if (candidate.signature.every((byte, index) => bytes[index] === byte)) {
      // WebP needs the WEBP tag at offset 8; RIFF alone is also used by WAV.
      if (candidate.mediaType === "image/webp") {
        const tag = String.fromCharCode(...bytes.slice(8, 12));
        return tag === "WEBP" ? candidate.mediaType : null;
      }
      return candidate.mediaType;
    }
  }
  return null;
}

export class AssetTooLargeError extends Error {}
export class AssetTypeError extends Error {}

/** Store image bytes, returning their content address. */
export async function storeAsset(
  kbRoot: string,
  bytes: Uint8Array,
  declaredMediaType?: string,
): Promise<EvidenceAsset> {
  if (bytes.byteLength === 0) throw new AssetTypeError("Asset is empty");
  if (bytes.byteLength > MAX_EVIDENCE_BYTES) {
    throw new AssetTooLargeError(`Asset is ${bytes.byteLength} bytes, limit is ${MAX_EVIDENCE_BYTES}`);
  }

  const detected = sniffImageMediaType(bytes);
  if (detected === null) {
    throw new AssetTypeError(`Unsupported image type. Accepted: ${IMAGE_MEDIA_TYPES.join(", ")}`);
  }
  if (declaredMediaType !== undefined && declaredMediaType !== detected) {
    throw new AssetTypeError(`Declared ${declaredMediaType} but the bytes are ${detected}`);
  }

  const sha256 = createHash("sha256").update(bytes).digest("hex");
  const dir = assetRoot(kbRoot);
  const path = join(dir, sha256);
  await mkdir(dir, { recursive: true });

  // Content addressing makes the write idempotent: an existing file is either
  // the same bytes (do nothing) or a hash collision (verify, never overwrite).
  const existing = await readFile(path).catch(() => null);
  if (existing === null) {
    await writeFile(path, bytes, { flag: "wx" }).catch((err: unknown) => {
      // Two concurrent writers of the same bytes: the loser's EEXIST is expected.
      if (!isErrnoCode(err, "EEXIST")) throw err;
    });
  } else if (createHash("sha256").update(existing).digest("hex") !== sha256) {
    throw new Error(`Asset ${sha256} exists with different bytes`);
  }

  return { sha256, mediaType: detected, byteSize: bytes.byteLength };
}

/** Read an asset by its content address, verifying the hash still matches. */
export async function readAsset(kbRoot: string, sha256: string): Promise<{ bytes: Uint8Array; mediaType: string } | null> {
  if (!/^[a-f0-9]{64}$/.test(sha256)) throw new AssetTypeError("Invalid asset id");
  const path = join(assetRoot(kbRoot), sha256);
  const bytes = await readFile(path).catch(() => null);
  if (bytes === null) return null;
  if (createHash("sha256").update(bytes).digest("hex") !== sha256) {
    throw new Error(`Asset ${sha256} does not match its content address`);
  }
  const mediaType = sniffImageMediaType(bytes);
  if (mediaType === null) throw new AssetTypeError(`Asset ${sha256} is not a supported image`);
  return { bytes, mediaType };
}

/** Narrow an unknown error to a node errno code without asserting a shape. */
export function isErrnoCode(err: unknown, code: string): boolean {
  return err instanceof Error && "code" in err && err.code === code;
}

const HEX = "0123456789abcdef";

/** A prefixed random id, e.g. `clm_9f3a2b7c`. */
export function newId(prefix: "clm" | "evd" | "prp" | "vec"): string {
  const bytes = randomBytes(12);
  let suffix = "";
  for (const byte of bytes) suffix += HEX[byte & 0x0f]! + HEX[byte >> 4]!;
  return `${prefix}_${suffix}`;
}
