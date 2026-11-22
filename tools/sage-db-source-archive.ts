#!/usr/bin/env bun
/**
 * Bounded, lossless legacy-source archive exporter (archive format v1).
 *
 * This is an **input tool** for the sage-db cutover, not an importer and not a
 * cutover claim. It freezes one deployed KB source tree — every regular file
 * plus, when the structured authority is active, every row of `knowledge.db` —
 * into a new directory that a later mapper can read. Nothing here maps records
 * to sage-db objects, so no "lossless target mapping" is implied by a successful
 * run.
 *
 * Scope of "complete" for files: this is an explicit **file-data** archive.
 * Directories are not records; an empty directory is not reproduced, and the
 * archive is not a filesystem snapshot. Directory work is still *charged*
 * against `maxEntries`/`maxDepth`, so a tree cannot be used to buy unbounded
 * traversal. Migration needs the file bytes and the record rows, not an empty
 * directory namespace.
 *
 * Guarantees (each one is enforced, not documented aspiration):
 *
 * - **Nothing silently skipped.** Every regular source file is a record in
 *   `files.ndjson`. A symlink, FIFO/socket/device, undecodable filename or
 *   unreadable entry aborts the export instead of being filtered out. (Empty
 *   directories are outside the file-data scope, not skipped content.)
 * - **Bounded before read.** `maxFiles` / `maxBytes` / `maxRows` / `maxEntries`
 *   / `maxDepth` are checked against `stat` sizes and actual rows before
 *   allocating or reading, so a runaway tree cannot be pulled into memory first.
 * - **Verification is bounded by the caller, not by the archive.** Every read,
 *   hash and line parse runs under `VerificationLimits` supplied by whoever
 *   verifies. The archive's own `limits` block is provenance; it can never raise
 *   those ceilings.
 * - **Fail-closed authority.** No database ⇒ Markdown authority. A readable
 *   database with a real `meta.imported_at` ⇒ structured authority. A corrupt,
 *   unreadable or unknown-schema database is an error; it never degrades to
 *   "just export the Markdown".
 * - **Frozen source.** The source inventory (path, size, mode, mtime, 64-bit
 *   inode as decimal text, SHA-256) is recomputed after export. Any change —
 *   including a bare `knowledge.db-wal` write from a live service — fails the
 *   export and no completion marker survives.
 * - **Honest partial output.** The manifest carries `complete:false` until the
 *   whole archive has been read back and re-hashed; `COMPLETE.json` is published
 *   last. If any step fails after the marker was written, the marker is removed
 *   (or the failure to remove it is reported) so a marker can never outlive a
 *   failed run. A directory without it is an unfinished export, not an archive.
 *
 * No repo modules are imported: this tool must run against a deployed KB from a
 * checkout that has never installed workspace dependencies. The schema profile
 * below is checked against `packages/service/src/store/schema.ts` by the test
 * suite, so drift fails a test instead of silently changing the archive.
 *
 * CLI:
 *   bun tools/sage-db-source-archive.ts \
 *     --source <kb-root> --out <new-dir> --kb <id> --registry <registry.json> \
 *     --max-files N --max-bytes N --max-rows N [--created-at ISO]
 *   bun tools/sage-db-source-archive.ts --verify <archive-dir> \
 *     --max-archive-bytes N --max-records N --max-line-bytes N --max-entries N
 */

import { Database } from "bun:sqlite";
import { createHash, randomBytes } from "node:crypto";
import {
  closeSync,
  existsSync,
  fsyncSync,
  lstatSync,
  mkdirSync,
  openSync,
  opendirSync,
  readFileSync,
  readdirSync,
  readSync,
  realpathSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
  writeSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve, sep } from "node:path";

export const ARCHIVE_VERSION = "1" as const;
export const TOOL_NAME = "sage-db-source-archive";
export const TOOL_VERSION = 1;

const FILES_STREAM = "files.ndjson";
const ROWS_STREAM = "rows.ndjson";
const MANIFEST_FILE = "archive.json";
const COMPLETE_FILE = "COMPLETE.json";
const BLOBS_DIR = "blobs";

/** Exit codes: distinct so a wrapper never has to parse stderr. */
export const EXIT = {
  ok: 0,
  usage: 64,
  refused: 65,
  verify: 66,
} as const;

export class ArchiveError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "ArchiveError";
  }
}

// ---------------------------------------------------------------------------
// Schema profile
// ---------------------------------------------------------------------------

export interface ColumnProfile {
  name: string;
  /** Declared type as SQLite reports it (`TEXT`, `INTEGER`, `BLOB`, `REAL`). */
  type: string;
  notnull: boolean;
  /** 1-based position inside the primary key, 0 when not part of it. */
  pk: number;
}

/**
 * Exact table/column policy of schema version 1.
 *
 * Mirrors the DDL in `packages/service/src/store/schema.ts`. Tables are listed in
 * DDL order so rows are emitted in a stable, reviewable sequence; each table's
 * primary key doubles as the deterministic row order. A table or column outside
 * this profile is refused rather than exported as an unclassified blob.
 */
export const EXPORTED_SCHEMA_PROFILE: Readonly<Record<string, readonly ColumnProfile[]>> = {
  meta: [
    { name: "key", type: "TEXT", notnull: true, pk: 1 },
    { name: "value", type: "TEXT", notnull: true, pk: 0 },
  ],
  concepts: [
    { name: "id", type: "TEXT", notnull: false, pk: 1 },
    { name: "atom", type: "TEXT", notnull: true, pk: 0 },
    { name: "layer", type: "INTEGER", notnull: true, pk: 0 },
    { name: "domain_json", type: "TEXT", notnull: true, pk: 0 },
    { name: "confidence", type: "TEXT", notnull: true, pk: 0 },
    { name: "source", type: "TEXT", notnull: true, pk: 0 },
    { name: "triggers_json", type: "TEXT", notnull: true, pk: 0 },
    { name: "body", type: "TEXT", notnull: true, pk: 0 },
    { name: "source_path", type: "TEXT", notnull: true, pk: 0 },
    { name: "revision", type: "TEXT", notnull: true, pk: 0 },
    { name: "source_hash", type: "TEXT", notnull: false, pk: 0 },
    { name: "source_revision", type: "TEXT", notnull: false, pk: 0 },
    { name: "created_at", type: "TEXT", notnull: true, pk: 0 },
    { name: "updated_at", type: "TEXT", notnull: true, pk: 0 },
  ],
  relations: [
    { name: "from_id", type: "TEXT", notnull: true, pk: 1 },
    { name: "type", type: "TEXT", notnull: true, pk: 2 },
    { name: "target", type: "TEXT", notnull: true, pk: 3 },
    { name: "confidence", type: "TEXT", notnull: true, pk: 0 },
    { name: "position", type: "INTEGER", notnull: true, pk: 0 },
    { name: "created_at", type: "TEXT", notnull: true, pk: 0 },
  ],
  claims: [
    { name: "id", type: "TEXT", notnull: false, pk: 1 },
    { name: "concept_id", type: "TEXT", notnull: true, pk: 0 },
    { name: "kind", type: "TEXT", notnull: true, pk: 0 },
    { name: "status", type: "TEXT", notnull: true, pk: 0 },
    { name: "text", type: "TEXT", notnull: true, pk: 0 },
    { name: "confidence", type: "TEXT", notnull: true, pk: 0 },
    { name: "revision", type: "TEXT", notnull: true, pk: 0 },
    { name: "created_at", type: "TEXT", notnull: true, pk: 0 },
    { name: "updated_at", type: "TEXT", notnull: true, pk: 0 },
  ],
  evidence: [
    { name: "id", type: "TEXT", notnull: false, pk: 1 },
    { name: "kind", type: "TEXT", notnull: true, pk: 0 },
    { name: "title", type: "TEXT", notnull: true, pk: 0 },
    { name: "locator", type: "TEXT", notnull: false, pk: 0 },
    { name: "excerpt", type: "TEXT", notnull: false, pk: 0 },
    { name: "asset_sha256", type: "TEXT", notnull: false, pk: 0 },
    { name: "asset_media_type", type: "TEXT", notnull: false, pk: 0 },
    { name: "asset_bytes", type: "INTEGER", notnull: false, pk: 0 },
    { name: "captured_at", type: "TEXT", notnull: true, pk: 0 },
    { name: "captured_by", type: "TEXT", notnull: true, pk: 0 },
    { name: "created_at", type: "TEXT", notnull: true, pk: 0 },
  ],
  evidence_targets: [
    { name: "evidence_id", type: "TEXT", notnull: true, pk: 1 },
    { name: "target_type", type: "TEXT", notnull: true, pk: 2 },
    { name: "target_id", type: "TEXT", notnull: true, pk: 3 },
  ],
  vectors: [
    { name: "id", type: "TEXT", notnull: false, pk: 1 },
    { name: "subject_type", type: "TEXT", notnull: true, pk: 0 },
    { name: "subject_id", type: "TEXT", notnull: true, pk: 0 },
    { name: "model", type: "TEXT", notnull: true, pk: 0 },
    { name: "dimensions", type: "INTEGER", notnull: true, pk: 0 },
    { name: "revision", type: "TEXT", notnull: true, pk: 0 },
    { name: "provider", type: "TEXT", notnull: false, pk: 0 },
    { name: "values_blob", type: "BLOB", notnull: true, pk: 0 },
    { name: "norm", type: "REAL", notnull: true, pk: 0 },
    { name: "created_at", type: "TEXT", notnull: true, pk: 0 },
  ],
  assets: [
    { name: "sha256", type: "TEXT", notnull: true, pk: 1 },
    { name: "media_type", type: "TEXT", notnull: true, pk: 0 },
    { name: "byte_size", type: "INTEGER", notnull: true, pk: 0 },
    { name: "path", type: "TEXT", notnull: true, pk: 0 },
    { name: "created_at", type: "TEXT", notnull: true, pk: 0 },
  ],
  proposals: [
    { name: "id", type: "TEXT", notnull: false, pk: 1 },
    { name: "kind", type: "TEXT", notnull: true, pk: 0 },
    { name: "subject", type: "TEXT", notnull: true, pk: 0 },
    { name: "expected_revision", type: "TEXT", notnull: true, pk: 0 },
    { name: "payload_json", type: "TEXT", notnull: true, pk: 0 },
    { name: "reason", type: "TEXT", notnull: true, pk: 0 },
    { name: "author", type: "TEXT", notnull: true, pk: 0 },
    { name: "status", type: "TEXT", notnull: true, pk: 0 },
    { name: "created_at", type: "TEXT", notnull: true, pk: 0 },
    { name: "reviewed_at", type: "TEXT", notnull: false, pk: 0 },
    { name: "reviewer", type: "TEXT", notnull: false, pk: 0 },
    { name: "review_note", type: "TEXT", notnull: false, pk: 0 },
  ],
};

/** DDL order; both processing order and the order tables appear in the manifest. */
export const TABLE_ORDER: readonly string[] = Object.freeze(Object.keys(EXPORTED_SCHEMA_PROFILE));

/** Meta keys written by the current service (`store/import.ts`, `store/writes.ts`). */
export const KNOWN_META_KEYS: readonly string[] = Object.freeze([
  "kb_root",
  "imported_at",
  "schema_version",
  "last_review_at",
]);

const DB_FILE = "knowledge.db";
const DB_SIDECARS = ["knowledge.db-wal", "knowledge.db-shm"] as const;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

export interface ExportLimits {
  maxFiles: number;
  maxBytes: number;
  maxRows: number;
  /**
   * Directory entries examined, including directories themselves. Without this
   * a tree of nested empty directories costs unbounded `readdir` work while
   * `maxFiles` (regular files only) never moves.
   */
  maxEntries: number;
  /** Directory nesting depth below the source root; the root is depth 0. */
  maxDepth: number;
}

/**
 * Caller-owned ceilings for verification.
 *
 * These are *not* the creation limits. They are what the verifying process is
 * willing to spend on an archive it may not trust, and they are applied before
 * any allocation: manifest size, each stream, each blob, each line, and the
 * cumulative record/byte totals. The archive's own `limits` block is provenance
 * recorded at export time and can never raise these.
 */
export interface VerificationLimits {
  /** Total bytes across manifest + streams + blobs. */
  maxArchiveBytes: number;
  /** Total records across every stream. */
  maxRecords: number;
  /** Longest single NDJSON line, in bytes. */
  maxLineBytes: number;
  /** Directory entries enumerated while checking blobs. */
  maxEntries: number;
}

export interface ExportOptions {
  sourceRoot: string;
  outDir: string;
  kb: string;
  registryPath: string;
  limits: ExportLimits;
  /** Fixed timestamp for reproducible fixtures; defaults to now. */
  createdAt?: string;
  /**
   * Test-only injection point: runs after the first manifest is written and
   * before the frozen-source recheck. Used to prove concurrent mutation fails.
   * It cannot weaken any check — it only gets a chance to change the source.
   */
  hooks?: {
    /** Runs after the frozen-source recheck and before the first read-back. */
    beforeFinalCheck?: (() => void) | undefined;
    /** Runs after `complete:true` is written and before the read-back. */
    beforeMarkerVerify?: ((ctx: { outDir: string; manifest: ArchiveManifest }) => void) | undefined;
    /** Runs after COMPLETE.json is published and before the final verification. */
    afterMarkerWrite?: ((ctx: { outDir: string; manifest: ArchiveManifest }) => void) | undefined;
  };
}

export type AuthorityKind = "markdown" | "structured";

export interface FileRecord {
  record_type: "source_file";
  /** POSIX-style path relative to the source root. */
  path: string;
  bytes: number;
  sha256: string;
  /** Relative path of the blob holding the exact bytes. */
  blob: string;
  mode: number;
  mtime_ms: number;
  /**
   * 64-bit inode as exact decimal text. `ino_t` exceeds 2^53 on ordinary
   * filesystems, so a JS number would round and let two distinct files compare
   * equal — the "same content, replaced file" signal this record exists for.
   */
  ino: string;
}

export interface EncodedValue {
  t: "null" | "integer" | "real" | "text" | "blob";
  v?: string;
  bits?: string;
  bytes_base64?: string;
  bytes?: number;
}

export interface RowRecord {
  record_type: "source_row";
  table: string;
  row_index: number;
  columns: string[];
  values: EncodedValue[];
}

export interface StreamRef {
  path: string;
  sha256: string;
  bytes: number;
  records: number;
  /**
   * Longest record line written to this stream. Recorded so an export can derive
   * its own read-back line ceiling from what it produced, instead of guessing.
   */
  longest_line_bytes: number;
  /**
   * Rows per table, for `rows.ndjson`. A table with zero rows has no records to
   * count, so enumeration cannot be inferred from the stream; this list is the
   * declaration that every registered table was actually read.
   */
  tables?: Array<{ table: string; rows: number }>;
}

export interface SchemaObject {
  type: string;
  name: string;
  tbl_name: string;
  sql: string | null;
}

export interface SchemaBlock {
  user_version: number;
  tables: Array<{ name: string; columns: ColumnProfile[] }>;
  objects: SchemaObject[];
}

export interface DatabaseBlock {
  /** Raw originals retained byte-for-byte (`knowledge.db` first). */
  files: Array<{ path: string; sha256: string; bytes: number }>;
  /** Digest of the retained raw file list; identifies the frozen snapshot. */
  source_digest: string;
  /** Size of the retained `knowledge.db-wal`; non-zero means uncheckpointed frames. */
  wal_bytes: number;
  imported_at: string | null;
  data_version: number | null;
  schema: SchemaBlock | null;
  /**
   * Identity of the *logical* snapshot actually read (schema + ordered rows),
   * which the raw WAL/SHM files alone cannot express.
   */
  logical_snapshot_digest: string | null;
}

export interface AuthorityBlock {
  kind: AuthorityKind;
  reason: "no-database" | "database-not-imported" | "imported";
  database: DatabaseBlock | null;
}

export interface RegistryEntry {
  id: string;
  name: string;
  description: string;
  path: string;
  domains: string[];
  created: string;
}

export interface ArchiveManifest {
  archive_version: typeof ARCHIVE_VERSION;
  tool: { name: string; version: number };
  created_at: string;
  kb: {
    id: string;
    /** Deployment context only: the declared path is recorded, never resolved. */
    registry: { path: string; sha256: string; version: string };
    entry: RegistryEntry;
  };
  source: {
    root: string;
    root_real: string;
    snapshot: { entries: number; bytes: number; digest: string };
  };
  authority: AuthorityBlock;
  limits: { max_files: number; max_bytes: number; max_rows: number; max_entries: number; max_depth: number };
  counts: {
    files: number;
    file_bytes: number;
    blobs: number;
    blob_bytes: number;
    rows: number;
    tables: number;
  };
  streams: StreamRef[];
  /** Digest of this manifest with `content_digest` and `complete` removed. */
  content_digest: string;
  complete: boolean;
}

/**
 * Verification limits plus the running totals the verifier observed, so an
 * exporter can derive an honest budget from its own written extents.
 */
export interface VerificationBudget extends VerificationLimits {
  observedArchiveBytes: number;
  observedRecords: number;
}

export interface ExportResult {
  outDir: string;
  manifest: ArchiveManifest;
  /**
   * Bytes this run actually wrote and knows by construction. The export's own
   * read-back derives its verification budget from these numbers rather than
   * from the manifest, which a verifier must not treat as policy.
   */
  written: { archiveBytes: number; records: number; maxLineBytes: number };
}

export interface VerifyResult {
  outDir: string;
  contentDigest: string;
  counts: ArchiveManifest["counts"];
  authority: AuthorityKind;
}

// ---------------------------------------------------------------------------
// Canonical serialisation and hashing
// ---------------------------------------------------------------------------

/**
 * Read at most `limit` bytes from an open descriptor, refusing a file that grew.
 *
 * `statSync().size` followed by `readFileSync` is not a bound: a file can grow
 * between the two calls and the read allocates whatever it finds. This reads
 * exactly the expected length, then probes for one more byte, so oversized
 * input is detected before it is buffered.
 */
function readExact(fd: number, expected: number, limit: number, label: string): Buffer {
  if (expected > limit) {
    throw new ArchiveError("E_BUDGET_BYTES", `${label} is ${expected} bytes, over the ${limit}-byte limit`);
  }
  const buffer = Buffer.allocUnsafe(expected);
  let offset = 0;
  while (offset < expected) {
    const read = readSync(fd, buffer, offset, expected - offset, offset);
    if (read === 0) break;
    offset += read;
  }
  if (offset !== expected) {
    throw new ArchiveError("E_SOURCE_CHANGED", `${label} shrank while being read (${offset} of ${expected} bytes)`);
  }
  const probe = Buffer.allocUnsafe(1);
  if (readSync(fd, probe, 0, 1, expected) !== 0) {
    throw new ArchiveError("E_SOURCE_CHANGED", `${label} grew while being read; refusing an unbounded read`);
  }
  return buffer;
}

/** Open for reading, fail closed, and run `fn` with the descriptor. */
function withReadFd<T>(file: string, label: string, fn: (fd: number) => T): T {
  let fd: number;
  try {
    fd = openSync(file, "r");
  } catch (err) {
    throw new ArchiveError("E_UNREADABLE", `cannot open ${label}: ${(err as Error).message}`);
  }
  try {
    return fn(fd);
  } finally {
    closeSync(fd);
  }
}

function sha256Hex(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

function digestOf(bytes: Uint8Array): string {
  return `sha256:${sha256Hex(bytes)}`;
}

/**
 * Canonical JSON: sorted object keys, no whitespace, arrays in given order.
 * The digest must be a function of content only, never of property insertion
 * order or formatting.
 */
export function canonicalJson(value: unknown): string {
  if (value === null) return "null";
  switch (typeof value) {
    case "string":
      return JSON.stringify(value);
    case "number":
      if (!Number.isFinite(value)) throw new ArchiveError("E_CANONICAL", "non-finite number in digest input");
      return JSON.stringify(value);
    case "boolean":
      return value ? "true" : "false";
    case "object": {
      if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
      const obj = value as Record<string, unknown>;
      const keys = Object.keys(obj).sort();
      const parts: string[] = [];
      for (const key of keys) {
        const item = obj[key];
        if (item === undefined) throw new ArchiveError("E_CANONICAL", `undefined value at key ${key}`);
        parts.push(`${JSON.stringify(key)}:${canonicalJson(item)}`);
      }
      return `{${parts.join(",")}}`;
    }
    default:
      throw new ArchiveError("E_CANONICAL", `unsupported digest input type ${typeof value}`);
  }
}

/** Manifest digest excludes its own digest field and the completion flag. */
export function manifestContentDigest(manifest: ArchiveManifest): string {
  const copy: Record<string, unknown> = { ...manifest };
  delete copy["content_digest"];
  delete copy["complete"];
  return digestOf(Buffer.from(canonicalJson(copy), "utf8"));
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

function isInside(parent: string, child: string): boolean {
  const p = parent.endsWith(sep) ? parent : parent + sep;
  return child === parent || child.startsWith(p);
}

function atomicWrite(dir: string, name: string, data: Uint8Array): void {
  const target = join(dir, name);
  const tmp = join(dir, `.${name}.${randomBytes(6).toString("hex")}.tmp`);
  const fd = openSync(tmp, "wx", 0o644);
  try {
    let offset = 0;
    while (offset < data.length) offset += writeSync(fd, data, offset, data.length - offset);
    fsyncSync(fd);
  } finally {
    closeSync(fd);
  }
  renameSync(tmp, target);
  syncDir(dir);
}

function syncDir(dir: string): void {
  let fd: number | null = null;
  try {
    fd = openSync(dir, "r");
    fsyncSync(fd);
  } catch {
    // Directory fsync is an extra durability step; platforms that refuse it
    // (some Windows configurations) still get atomic rename semantics.
  } finally {
    if (fd !== null) closeSync(fd);
  }
}

/** Incremental NDJSON writer: hashes and counts exactly the bytes written. */
class NdBuf {
  private readonly hash = createHash("sha256");
  private readonly fd: number;
  private buffered: string[] = [];
  private bufferedBytes = 0;
  private totalBytes = 0;
  private totalRecords = 0;
  private longestLine = 0;

  constructor(file: string) {
    this.fd = openSync(file, "wx", 0o644);
  }

  push(record: unknown): void {
    const line = `${canonicalJson(record)}\n`;
    this.totalRecords += 1;
    this.buffered.push(line);
    const lineBytes = Buffer.byteLength(line, "utf8");
    if (lineBytes > this.longestLine) this.longestLine = lineBytes;
    this.bufferedBytes += lineBytes;
    if (this.bufferedBytes >= 1 << 20) this.flush();
  }

  private flush(): void {
    if (this.buffered.length === 0) return;
    const chunk = Buffer.from(this.buffered.join(""), "utf8");
    this.buffered = [];
    this.bufferedBytes = 0;
    this.hash.update(chunk);
    this.totalBytes += chunk.length;
    let offset = 0;
    while (offset < chunk.length) offset += writeSync(this.fd, chunk, offset, chunk.length - offset);
  }

  close(): { sha256: string; bytes: number; records: number; longestLine: number } {
    this.flush();
    fsyncSync(this.fd);
    closeSync(this.fd);
    return {
      sha256: `sha256:${this.hash.digest("hex")}`,
      bytes: this.totalBytes,
      records: this.totalRecords,
      longestLine: this.longestLine,
    };
  }
}

/** Hash a file of known length without allocating it: one bounded chunk at a time. */
function hashFileExact(file: string, expected: number, label: string): string {
  return withReadFd(file, label, (fd) => {
    const size = statSync(file).size;
    if (size !== expected) {
      throw new ArchiveError("E_VERIFY_STREAM", `${label} is ${size} bytes, manifest says ${expected}`);
    }
    const hash = createHash("sha256");
    const chunk = Buffer.allocUnsafe(Math.min(1 << 20, Math.max(1, expected)));
    let offset = 0;
    while (offset < expected) {
      const read = readSync(fd, chunk, 0, Math.min(chunk.length, expected - offset), offset);
      if (read === 0) break;
      hash.update(chunk.subarray(0, read));
      offset += read;
    }
    if (offset !== expected) throw new ArchiveError("E_VERIFY_STREAM", `${label} shrank while being hashed`);
    const probe = Buffer.allocUnsafe(1);
    if (readSync(fd, probe, 0, 1, expected) !== 0) {
      throw new ArchiveError("E_VERIFY_STREAM", `${label} grew while being hashed`);
    }
    return `sha256:${hash.digest("hex")}`;
  });
}

/**
 * Read NDJSON line by line under a hard per-line ceiling.
 *
 * A partial line is accumulated into a buffer this function owns, and its length
 * is checked **before** every append. Without that, one archive-declared
 * multi-gigabyte line would be grown by repeated `Buffer.concat` until the
 * process died: the ceiling has to bind on growth, not on the final line.
 */
function readLinesSync(
  file: string,
  maxLineBytes: number,
  onLine: (line: string, index: number) => void,
): number {
  const fd = openSync(file, "r");
  const chunk = Buffer.allocUnsafe(Math.min(1 << 20, maxLineBytes + 1));
  let carry = Buffer.alloc(0);
  let index = 0;
  try {
    for (;;) {
      const read = readSync(fd, chunk, 0, chunk.length, null);
      if (read === 0) break;
      const incoming = chunk.subarray(0, read);
      let start = 0;
      for (;;) {
        const nl = incoming.indexOf(0x0a, start);
        if (nl === -1) break;
        const tail = incoming.subarray(start, nl);
        if (carry.length + tail.length > maxLineBytes) {
          throw new ArchiveError(
            "E_VERIFY_LINE",
            `${file}:${index} line exceeds maxLineBytes=${maxLineBytes} (${carry.length + tail.length} bytes so far)`,
          );
        }
        const line =
          carry.length === 0
            ? tail.toString("utf8")
            : Buffer.concat([carry, tail], carry.length + tail.length).toString("utf8");
        carry = Buffer.alloc(0);
        onLine(line, index++);
        start = nl + 1;
      }
      const rest = incoming.subarray(start);
      if (rest.length > 0) {
        if (carry.length + rest.length > maxLineBytes) {
          throw new ArchiveError(
            "E_VERIFY_LINE",
            `${file}:${index} line exceeds maxLineBytes=${maxLineBytes} (${carry.length + rest.length} bytes so far)`,
          );
        }
        carry = carry.length === 0 ? Buffer.from(rest) : Buffer.concat([carry, rest], carry.length + rest.length);
      }
    }
    if (carry.length > 0) onLine(carry.toString("utf8"), index++);
  } finally {
    closeSync(fd);
  }
  return index;
}

// ---------------------------------------------------------------------------
// Source inventory
// ---------------------------------------------------------------------------

interface InventoryEntry {
  path: string;
  bytes: number;
  sha256: string;
  mode: number;
  mtimeMs: number;
  /** Exact 64-bit inode as decimal text; never coerced to a double. */
  ino: string;
}

interface Inventory {
  entries: InventoryEntry[];
  digest: string;
  totalBytes: number;
}

/** Decode a filename exactly; a name that cannot round-trip is rejected. */
function checkName(name: string, parent: string): void {
  if (name === "" || name === "." || name === "..") {
    throw new ArchiveError("E_UNSAFE_PATH", `unsafe entry name ${JSON.stringify(name)} under ${parent}`);
  }
  if (name.includes("\u0000")) {
    throw new ArchiveError("E_UNSAFE_PATH", `NUL in entry name under ${parent}`);
  }
  if (Buffer.from(name, "utf8").toString("utf8") !== name) {
    throw new ArchiveError("E_UNSAFE_PATH", `filename is not valid UTF-8 under ${parent}`);
  }
}

/**
 * Walk the source with explicit work bounds.
 *
 * `opendir` entries are streamed, never collected: `readdirSync` would allocate
 * one array per directory whose size the exporter does not control. Every entry
 * counts against `maxEntries`, so a tree of nested empty directories fails on
 * work done rather than on files produced. Only the admitted entries are sorted,
 * and only for deterministic order.
 */
function walkSource(root: string, limits: ExportLimits): Inventory {
  const entries: InventoryEntry[] = [];
  let totalBytes = 0;
  let examined = 0;

  const visit = (dirAbs: string, relPrefix: string, depth: number): void => {
    if (depth > limits.maxDepth) {
      throw new ArchiveError(
        "E_BUDGET_DEPTH",
        `source nests deeper than maxDepth=${limits.maxDepth} at ${relPrefix === "" ? "." : relPrefix}`,
      );
    }
    const dir = opendirSync(dirAbs);
    try {
      for (;;) {
        const entry = dir.readSync();
        if (entry === null) break;
        examined += 1;
        if (examined > limits.maxEntries) {
          throw new ArchiveError(
            "E_BUDGET_ENTRIES",
            `source has more than maxEntries=${limits.maxEntries} directory entries (at ${relPrefix === "" ? "." : relPrefix})`,
          );
        }
        const name = entry.name;
        checkName(name, relPrefix === "" ? "." : relPrefix);
        const rel = relPrefix === "" ? name : `${relPrefix}/${name}`;
        if (entry.isSymbolicLink()) {
          throw new ArchiveError("E_SYMLINK", `refusing to export symlink ${rel}`);
        }
        if (entry.isDirectory()) {
          visit(join(dirAbs, name), rel, depth + 1);
          continue;
        }
        if (!entry.isFile()) {
          throw new ArchiveError("E_SPECIAL_FILE", `refusing to export non-regular file ${rel}`);
        }
        if (entries.length + 1 > limits.maxFiles) {
          throw new ArchiveError("E_BUDGET_FILES", `source has more than maxFiles=${limits.maxFiles} files`);
        }
        const abs = join(dirAbs, name);
        // `lstat` rather than the dirent: the dirent may be cached from before a
        // concurrent replacement, and the size below drives the read budget.
        // `bigint: true` keeps the 64-bit inode and size exact; the size is then
        // narrowed only after a safe-integer check.
        const st = lstatSync(abs, { bigint: true });
        if (!st.isFile()) {
          throw new ArchiveError("E_SOURCE_CHANGED", `${rel} stopped being a regular file while being walked`);
        }
        if (st.size > BigInt(Number.MAX_SAFE_INTEGER)) {
          throw new ArchiveError("E_BUDGET_BYTES", `${rel} reports a size beyond the safely checkable range`);
        }
        const size = Number(st.size);
        if (totalBytes + size > limits.maxBytes) {
          throw new ArchiveError(
            "E_BUDGET_BYTES",
            `source exceeds maxBytes=${limits.maxBytes} at ${rel} (${totalBytes + size} bytes)`,
          );
        }
        const bytes = withReadFd(abs, rel, (fd) => readExact(fd, size, limits.maxBytes - totalBytes, rel));
        totalBytes += bytes.length;
        entries.push({
          path: rel,
          bytes: bytes.length,
          sha256: sha256Hex(bytes),
          mode: Number(st.mode),
          mtimeMs: Number(st.mtimeMs),
          ino: st.ino.toString(10),
        });
      }
    } finally {
      dir.closeSync();
    }
  };

  visit(root, "", 0);
  entries.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  const digest = digestOf(
    Buffer.from(canonicalJson(entries.map((e) => [e.path, e.bytes, e.sha256, e.mode, e.mtimeMs, e.ino])), "utf8"),
  );
  return { entries, digest, totalBytes };
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

function readRegistry(registryPath: string, kbId: string, limit: number): ArchiveManifest["kb"] {
  const raw = withReadFd(registryPath, `registry ${registryPath}`, (fd) =>
    readExact(fd, statSync(registryPath).size, limit, `registry ${registryPath}`),
  );
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw.toString("utf8"));
  } catch (err) {
    throw new ArchiveError("E_REGISTRY", `registry is not JSON: ${(err as Error).message}`);
  }
  const obj = parsed as { version?: unknown; knowledgeBases?: unknown };
  if (typeof obj.version !== "string" || !Array.isArray(obj.knowledgeBases)) {
    throw new ArchiveError("E_REGISTRY", "registry must have {version, knowledgeBases[]}");
  }
  const matches = obj.knowledgeBases.filter((item): item is Record<string, unknown> => {
    return typeof item === "object" && item !== null && (item as Record<string, unknown>)["id"] === kbId;
  });
  if (matches.length === 0) {
    throw new ArchiveError("E_REGISTRY", `registry has no knowledge base with id ${JSON.stringify(kbId)}`);
  }
  if (matches.length > 1) {
    throw new ArchiveError("E_REGISTRY", `registry lists id ${JSON.stringify(kbId)} more than once`);
  }
  const entry = matches[0]!;
  const str = (key: string): string => {
    const value = entry[key];
    if (typeof value !== "string") throw new ArchiveError("E_REGISTRY", `registry entry ${kbId}.${key} must be a string`);
    return value;
  };
  const domainsRaw = entry["domains"];
  if (!Array.isArray(domainsRaw) || domainsRaw.some((d) => typeof d !== "string")) {
    throw new ArchiveError("E_REGISTRY", `registry entry ${kbId}.domains must be string[]`);
  }
  return {
    id: kbId,
    registry: { path: registryPath, sha256: sha256Hex(raw), version: obj.version },
    entry: {
      id: kbId,
      name: str("name"),
      description: str("description"),
      // Deployment context: recorded verbatim, never used to resolve the source.
      path: str("path"),
      domains: domainsRaw as string[],
      created: str("created"),
    },
  };
}

// ---------------------------------------------------------------------------
// Database inspection and row export (read-only, fail-closed)
// ---------------------------------------------------------------------------

/**
 * The deployed database is never opened in place.
 *
 * `knowledge.db`, `-wal` and `-shm` are live coordination files owned by the
 * process running the service. An exporter must not open or touch them: the
 * writer may be committing at any moment, and those files are not ours to
 * modify. Instead the bytes are copied into a private scratch directory under
 * the OS temp directory, and SQLite is only ever opened on **that** copy, where
 * it is free to create or rebuild its own `-shm`/`-wal` coordination files.
 *
 * Copy order is `knowledge.db`, then `-wal`, then `-shm`, so a copy taken while
 * a writer commits leans toward "database file older, WAL newer" — a state
 * SQLite replays — rather than the reverse. That ordering reduces the damage of
 * a torn copy; it does not make one correct.
 *
 * Concurrency is therefore rejected by evidence about the **source bytes**, not
 * by anything SQLite reports: the database files are re-read and compared to the
 * snapshot the rows came from, and the whole source tree is re-walked and
 * compared to the frozen inventory. A copy taken while the service commits is
 * detected there, never silently accepted.
 * `docs/design/migration.md` §1/§3 already require writes to stop for a real
 * export — this tool refuses to pretend otherwise.
 *
 * (An earlier version of this comment claimed that a read-only open rewrites
 * `-shm` and that close checkpoints `-wal` back into the database. That claim
 * was withdrawn: the only probes behind it had a read-write writer in the same
 * process, so they could not attribute the observed changes to a reader. It is
 * not repeated here, and no behaviour depends on it.)
 */
interface DatabaseSnapshot {
  /** Retained files, `knowledge.db` first. */
  files: Array<{ path: string; sha256: string; bytes: number }>;
  sourceDigest: string;
  /** Size of the retained `knowledge.db-wal`; non-zero means uncheckpointed frames. */
  walBytes: number;
}

const DB_SCRATCH_PREFIX = "sage-db-archive-scratch-";

interface RowsResult {
  stream: StreamRef;
  rows: number;
  tables: number;
  dataVersion: number;
  logicalSnapshotDigest: string;
  tableCounts: Array<[string, number]>;
}

interface DatabaseRead {
  authority: AuthorityBlock;
  rows: RowsResult | null;
  snapshot: DatabaseSnapshot;
}

function sqlQuote(identifier: string): string {
  return `"${identifier.replace(/"/g, '""')}"`;
}

/**
 * Open the private scratch copy for reading.
 *
 * `readonly: true` is not usable here: a WAL database whose `-shm` was not
 * copied alongside it refuses to open read-only ("unable to open database
 * file"), and inventing a `-shm` would mean guessing at shared-memory state.
 * The copy is disposable and never the source, so it is opened read-write and
 * immediately constrained with `query_only`, which makes every write attempt a
 * SQLite error. `safeIntegers` makes every INTEGER arrive as a bigint, so no
 * value is truncated to a double.
 */
function openScratch(file: string): Database {
  let db: Database;
  try {
    db = new Database(file, { readonly: false, create: false, safeIntegers: true });
  } catch (err) {
    throw new ArchiveError("E_DB_UNREADABLE", `cannot open the database snapshot: ${(err as Error).message}`);
  }
  try {
    db.exec("PRAGMA query_only = ON");
  } catch (err) {
    db.close();
    throw new ArchiveError("E_DB_UNREADABLE", `cannot set query_only: ${(err as Error).message}`);
  }
  return db;
}

/**
 * Collect schema-introspection rows with an explicit ceiling.
 *
 * `PRAGMA table_info(...)` cannot be wrapped in a counting subquery, so the
 * statement is iterated instead of `.all()`-d and the array grows only up to
 * `limit`: allocation is controlled by the same budget as the rest of the walk,
 * never by what a hostile or broken database reports.
 */
function collectRows<T>(stmt: ReturnType<Database["query"]>, limit: number, what: string): T[] {
  const rows: T[] = [];
  for (const row of stmt.iterate() as Iterable<T>) {
    if (rows.length >= limit) {
      throw new ArchiveError("E_DB_UNSUPPORTED", `${what} has more than ${limit} rows, over the introspection limit`);
    }
    rows.push(row);
  }
  return rows;
}

function pragmaRows<T>(db: Database, sql: string, limit: number, what: string): T[] {
  try {
    return collectRows<T>(db.query(sql), limit, what);
  } catch (err) {
    if (err instanceof ArchiveError) throw err;
    throw new ArchiveError("E_DB_UNSUPPORTED", `requires schema introspection: ${(err as Error).message}`);
  }
}

function readSchema(db: Database, introspectionLimit: number): SchemaBlock {
  let userVersion: number;
  try {
    userVersion = Number(db.query<{ user_version: number }, []>("PRAGMA user_version").get()?.user_version ?? 0);
  } catch (err) {
    throw new ArchiveError("E_DB_UNREADABLE", `cannot read user_version: ${(err as Error).message}`);
  }
  if (userVersion !== 1) {
    throw new ArchiveError("E_DB_UNSUPPORTED", `${DB_FILE} has user_version=${userVersion}, supported schema is 1`);
  }

  interface TableListRow {
    schema: string;
    name: string;
    type: string;
  }
  const listed = pragmaRows<TableListRow>(db, "SELECT * FROM pragma_table_list", introspectionLimit, "pragma_table_list");
  const actualTables = listed
    .filter((row) => row.schema === "main" && row.type === "table" && !String(row.name).startsWith("sqlite_"))
    .map((row) => row.name)
    .sort();

  const unknown = actualTables.filter((name) => !(name in EXPORTED_SCHEMA_PROFILE));
  if (unknown.length > 0) {
    throw new ArchiveError("E_DB_UNSUPPORTED", `${DB_FILE} has tables outside schema version 1: ${unknown.join(", ")}`);
  }
  const missing = [...TABLE_ORDER].filter((name) => !actualTables.includes(name));
  if (missing.length > 0) {
    throw new ArchiveError("E_DB_UNSUPPORTED", `${DB_FILE} is missing tables: ${missing.join(", ")}`);
  }

  const describe = (cols: readonly ColumnProfile[]): string =>
    cols.map((c) => `${c.name}:${c.type}${c.notnull ? "!" : ""}${c.pk ? `#${c.pk}` : ""}`).join(",");
  const tables: SchemaBlock["tables"] = [];
  for (const table of TABLE_ORDER) {
    interface TableInfoRow {
      cid: number;
      name: string;
      type: string;
      notnull: number;
      dflt_value: string | null;
      pk: number;
    }
    const info = pragmaRows<TableInfoRow>(db, `PRAGMA table_info(${sqlQuote(table)})`, introspectionLimit, `table ${table}`);
    // safeIntegers is on, so pragma integers arrive as bigint: compare through
    // Number instead of `=== 1`, which is always false for bigint.
    const actual: ColumnProfile[] = info.map((c) => ({
      name: c.name,
      type: String(c.type).toUpperCase(),
      notnull: Number(c.notnull) === 1,
      pk: Number(c.pk),
    }));
    if (describe(actual) !== describe(EXPORTED_SCHEMA_PROFILE[table]!)) {
      throw new ArchiveError(
        "E_DB_UNSUPPORTED",
        `${DB_FILE} table ${table} columns differ from schema version 1: actual [${describe(actual)}]`,
      );
    }
    tables.push({ name: table, columns: actual });
  }

  interface MasterRow {
    type: string;
    name: string;
    tbl_name: string;
    sql: string | null;
  }
  const master = pragmaRows<MasterRow>(
    db,
    "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name",
    introspectionLimit,
    "sqlite_master",
  );
  for (const obj of master) {
    if (obj.type !== "table" && obj.type !== "index") {
      throw new ArchiveError("E_DB_UNSUPPORTED", `${DB_FILE} has unsupported schema object ${obj.type} ${obj.name}`);
    }
  }
  return {
    user_version: userVersion,
    tables,
    objects: master.map((o) => ({ type: o.type, name: o.name, tbl_name: o.tbl_name, sql: o.sql })),
  };
}

function realBits(value: number): string {
  const view = new DataView(new ArrayBuffer(8));
  view.setFloat64(0, value);
  let out = "";
  for (let i = 0; i < 8; i += 1) out += view.getUint8(i).toString(16).padStart(2, "0");
  return out;
}

export function encodeValue(value: unknown, table: string, column: string): EncodedValue {
  if (value === null || value === undefined) return { t: "null" };
  if (typeof value === "bigint") return { t: "integer", v: value.toString(10) };
  // A REAL is the only JS number SQLite returns once safeIntegers is on.
  if (typeof value === "number") return { t: "real", bits: realBits(value) };
  if (typeof value === "string") {
    if (Buffer.from(value, "utf8").toString("utf8") !== value) {
      throw new ArchiveError("E_DB_UNSUPPORTED", `text value in ${table}.${column} is not valid UTF-8`);
    }
    return { t: "text", v: value };
  }
  if (value instanceof Uint8Array) {
    const bytes = Buffer.from(value);
    return { t: "blob", bytes_base64: bytes.toString("base64"), bytes: bytes.length };
  }
  throw new ArchiveError("E_DB_UNSUPPORTED", `unsupported SQLite value type ${typeof value} in ${table}.${column}`);
}

/** Copy `knowledge.db` and its companions into `dir`, verified against the inventory. */
function copyDatabaseFiles(source: string, dir: string, inventory: Inventory, names: readonly string[]): DatabaseSnapshot["files"] {
  const copied: DatabaseSnapshot["files"] = [];
  for (const name of names) {
    const entry = inventory.entries.find((e) => e.path === name);
    if (entry === undefined) continue;
    const abs = join(source, name);
    const bytes = withReadFd(abs, name, (fd) => readExact(fd, entry.bytes, entry.bytes, name));
    if (sha256Hex(bytes) !== entry.sha256) {
      throw new ArchiveError("E_SOURCE_CHANGED", `${name} changed while the database was copied`);
    }
    writeFileSync(join(dir, name), bytes, { flag: "wx" });
    copied.push({ path: name, sha256: entry.sha256, bytes: entry.bytes });
  }
  return copied;
}

/**
 * Establish authority from actual database state and, when structured, stream
 * every registered row out of one consistent read transaction.
 *
 * No database ⇒ Markdown. Readable, known-schema database ⇒ structured only if
 * `meta.imported_at` is a real timestamp. Anything that prevents reading the
 * database is an error: "corrupt" must never look like "never imported",
 * because that would silently export stale concept files as current truth.
 */
function readDatabase(
  source: string,
  outDir: string,
  inventory: Inventory,
  limits: ExportLimits,
): DatabaseRead {
  const emptySnapshot: DatabaseSnapshot = { files: [], sourceDigest: "", walBytes: 0 };
  if (inventory.entries.every((e) => e.path !== DB_FILE)) {
    if (existsSync(join(source, DB_FILE))) {
      throw new ArchiveError("E_SOURCE_CHANGED", `${DB_FILE} exists but was not part of the inventory`);
    }
    return {
      authority: { kind: "markdown", reason: "no-database", database: null },
      rows: null,
      snapshot: emptySnapshot,
    };
  }

  const names = [DB_FILE, ...DB_SIDECARS];
  // Schema objects are bounded by the same entry budget as the file tree: both
  // are "how much catalog can this export be asked to walk".
  const introspectionLimit = limits.maxEntries;
  const scratch = join(tmpdir(), `${DB_SCRATCH_PREFIX}${randomBytes(8).toString("hex")}`);
  mkdirSync(scratch, { recursive: false });
  let scratchCopy: DatabaseSnapshot["files"];
  let authority: AuthorityBlock;
  let rows: RowsResult | null = null;
  let schema: SchemaBlock | null = null;
  let importedAt: string | null = null;
  let dataVersion: number | null = null;

  try {
    scratchCopy = copyDatabaseFiles(source, scratch, inventory, names);
    const db = openScratch(join(scratch, DB_FILE));
    try {
      let quick: Record<string, unknown> | null;
      try {
        quick = db.query("PRAGMA quick_check").get() as Record<string, unknown> | null;
      } catch (err) {
        throw new ArchiveError("E_DB_CORRUPT", `${DB_FILE} failed integrity check: ${(err as Error).message}`);
      }
      if (quick?.["quick_check"] !== "ok") {
        throw new ArchiveError("E_DB_CORRUPT", `${DB_FILE} failed integrity check: ${String(quick?.["quick_check"])}`);
      }

      try {
        db.exec("BEGIN");
      } catch (err) {
        throw new ArchiveError("E_DB_UNREADABLE", `cannot open a read transaction: ${(err as Error).message}`);
      }
      try {
        // Static consistency guard, nothing more. This connection is the only one
        // on the scratch copy and it is `query_only`, so nothing inside this
        // process can move `data_version`. It would only differ if some other
        // connection were added to the scratch later; real detection of a live
        // writer is the source byte comparison and the frozen source re-walk.
        const startVersion = Number(db.query<{ data_version: number }, []>("PRAGMA data_version").get()?.data_version ?? 0);
        schema = readSchema(db, introspectionLimit);

        interface MetaRow {
          key: string;
          value: string;
        }
        let metaRows: MetaRow[];
        try {
          // `meta` is keyed by a fixed vocabulary, so the row count is bounded
          // by the same entry budget as the source tree rather than unbounded.
          metaRows = pragmaRows<MetaRow>(db, "SELECT key, value FROM meta", limits.maxEntries, "meta");
        } catch (err) {
          throw new ArchiveError("E_DB_UNSUPPORTED", `${DB_FILE} meta is unreadable: ${(err as Error).message}`);
        }
        const unknownKeys = metaRows.map((r) => r.key).filter((k) => !KNOWN_META_KEYS.includes(k));
        if (unknownKeys.length > 0) {
          throw new ArchiveError(
            "E_DB_UNSUPPORTED",
            `${DB_FILE} has meta keys outside schema version 1: ${unknownKeys.join(", ")}`,
          );
        }
        const meta = new Map(metaRows.map((r) => [r.key, r.value]));
        const schemaVersion = meta.get("schema_version");
        if (schemaVersion !== undefined && schemaVersion !== "1") {
          throw new ArchiveError("E_DB_UNSUPPORTED", `${DB_FILE} meta.schema_version=${schemaVersion}, supported is 1`);
        }

        importedAt = meta.get("imported_at") ?? null;
        if (importedAt !== null && (importedAt.trim() === "" || Number.isNaN(Date.parse(importedAt)))) {
          throw new ArchiveError(
            "E_DB_UNSUPPORTED",
            `${DB_FILE} meta.imported_at=${JSON.stringify(importedAt)} is not a timestamp`,
          );
        }

        if (importedAt !== null) {
          // --- rows: every registered table, one transaction, bounded ---------
          const writer = new NdBuf(join(outDir, ROWS_STREAM));
          let totalRows = 0;
          const tableCounts: Array<[string, number]> = [];
          for (const table of TABLE_ORDER) {
            const profile = EXPORTED_SCHEMA_PROFILE[table]!;
            const columns = profile.map((c) => c.name);
            const orderBy = profile
              .filter((c) => c.pk > 0)
              .sort((a, b) => a.pk - b.pk)
              .map((c) => sqlQuote(c.name))
              .join(", ");
            const sql = `SELECT ${columns.map(sqlQuote).join(", ")} FROM ${sqlQuote(table)} ORDER BY ${orderBy}`;
            const stmt = db.query(sql);
            let index = 0;
            for (const raw of stmt.iterate() as Iterable<Record<string, unknown>>) {
              if (totalRows + 1 > limits.maxRows) {
                throw new ArchiveError(
                  "E_BUDGET_ROWS",
                  `source has more than maxRows=${limits.maxRows} rows (at ${table})`,
                );
              }
              const values = columns.map((column) => encodeValue(raw[column], table, column));
              writer.push({ record_type: "source_row", table, row_index: index, columns, values } satisfies RowRecord);
              index += 1;
              totalRows += 1;
            }
            tableCounts.push([table, index]);
          }
          const stream = writer.close();

          const endVersion = Number(db.query<{ data_version: number }, []>("PRAGMA data_version").get()?.data_version ?? 0);
          if (endVersion !== startVersion) {
            // Not concurrency evidence against the deployed KB: it means a second
            // connection touched our own scratch, i.e. a bug in this tool.
            throw new ArchiveError(
              "E_SOURCE_CHANGED",
              `scratch database changed underneath the row read (data_version ${startVersion} → ${endVersion})`,
            );
          }
          db.exec("COMMIT");

          dataVersion = startVersion;
          rows = {
            stream: {
              path: ROWS_STREAM,
              ...stream,
              longest_line_bytes: stream.longestLine,
              tables: tableCounts.map(([table, count]) => ({ table, rows: count })),
            },
            rows: totalRows,
            tables: tableCounts.length,
            tableCounts,
            dataVersion: startVersion,
            logicalSnapshotDigest: digestOf(
              Buffer.from(
                canonicalJson({ schema, rows: { sha256: stream.sha256, records: stream.records, tables: tableCounts } }),
                "utf8",
              ),
            ),
          };
          authority = { kind: "structured", reason: "imported", database: null };
        } else {
          // A database that exists but never finished an import is Markdown
          // authority again; its bytes are retained but no rows are claimed.
          db.exec("COMMIT");
          authority = { kind: "markdown", reason: "database-not-imported", database: null };
        }
      } catch (err) {
        try {
          db.exec("ROLLBACK");
        } catch {
          // Transaction already ended (e.g. failed BEGIN); the original error wins.
        }
        throw err;
      }
    } finally {
      db.close();
    }
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }

  // The originals are already part of the archive: `knowledge.db` and every
  // companion are ordinary files in the inventory, so `files.ndjson` records
  // them and `blobs/` holds their exact bytes. Re-copying them into a second
  // directory would duplicate the database and create two places to check.
  // Instead the database bytes are re-read now and compared to the snapshot the
  // rows came from, which is the property that actually matters.
  const recheck: DatabaseSnapshot["files"] = [];
  for (const name of names) {
    const entry = inventory.entries.find((e) => e.path === name);
    if (entry === undefined) continue;
    const abs = join(source, name);
    const bytes = withReadFd(abs, name, (fd) => readExact(fd, entry.bytes, entry.bytes, name));
    if (sha256Hex(bytes) !== entry.sha256) {
      throw new ArchiveError("E_SOURCE_CHANGED", `${name} changed between the snapshot read and the export`);
    }
    recheck.push({ path: name, sha256: entry.sha256, bytes: entry.bytes });
  }
  if (canonicalJson(recheck) !== canonicalJson(scratchCopy)) {
    throw new ArchiveError("E_SOURCE_CHANGED", `${DB_FILE} changed between the snapshot read and the export`);
  }
  const files = recheck;
  const wal = files.find((f) => f.path === "knowledge.db-wal");

  const databaseBlock: DatabaseBlock = {
    files,
    source_digest: digestOf(Buffer.from(canonicalJson(files.map((f) => [f.path, f.sha256, f.bytes])), "utf8")),
    wal_bytes: wal?.bytes ?? 0,
    imported_at: importedAt,
    data_version: dataVersion,
    schema,
    logical_snapshot_digest: rows?.logicalSnapshotDigest ?? null,
  };
  authority.database = databaseBlock;

  return {
    authority,
    rows,
    snapshot: {
      files,
      sourceDigest: databaseBlock.source_digest,
      walBytes: databaseBlock.wal_bytes,
    },
  };
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

function assertLimits(limits: ExportLimits): void {
  const check = (name: string, value: unknown): number => {
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) {
      throw new ArchiveError("E_USAGE", `${name} must be a positive finite integer`);
    }
    return value;
  };
  check("maxFiles", limits.maxFiles);
  check("maxBytes", limits.maxBytes);
  check("maxRows", limits.maxRows);
  check("maxEntries", limits.maxEntries);
  // Depth 0 means "the source root only", so zero is meaningful here even
  // though every other bound must be at least one.
  if (typeof limits.maxDepth !== "number" || !Number.isSafeInteger(limits.maxDepth) || limits.maxDepth < 0) {
    throw new ArchiveError("E_USAGE", "maxDepth must be a non-negative finite integer");
  }
}

function resolvePaths(sourceRoot: string, outDir: string): { source: string; out: string } {
  let sourceReal: string;
  try {
    sourceReal = realpathSync(sourceRoot);
  } catch (err) {
    throw new ArchiveError("E_USAGE", `source root ${sourceRoot} is not readable: ${(err as Error).message}`);
  }
  if (!statSync(sourceReal).isDirectory()) {
    throw new ArchiveError("E_USAGE", `source root ${sourceReal} is not a directory`);
  }

  const outAbs = resolve(outDir);
  const parent = dirname(outAbs);
  let parentReal: string;
  try {
    parentReal = realpathSync(parent);
  } catch (err) {
    throw new ArchiveError("E_USAGE", `output parent ${parent} is not readable: ${(err as Error).message}`);
  }
  const outReal = join(parentReal, outAbs.slice(parent.length).replace(/^[/\\]+/, ""));

  // Containment first: an output that already exists may contain the source, in
  // which case "exists" is a symptom and "you would export into yourself" is the
  // actual problem the operator needs to read.
  if (isInside(sourceReal, outReal) || isInside(sourceReal, outAbs)) {
    throw new ArchiveError("E_NESTED_OUTPUT", `output ${outAbs} is inside the source root ${sourceReal}`);
  }
  if (isInside(outReal, sourceReal)) {
    throw new ArchiveError("E_NESTED_OUTPUT", `source root ${sourceReal} is inside the output ${outAbs}`);
  }
  if (existsSync(outAbs)) {
    throw new ArchiveError("E_OUTPUT_EXISTS", `refusing to overwrite existing output ${outAbs}`);
  }
  return { source: sourceReal, out: outAbs };
}

export function exportSourceArchive(options: ExportOptions): ExportResult {
  assertLimits(options.limits);
  if (typeof options.kb !== "string" || options.kb.trim() === "") {
    throw new ArchiveError("E_USAGE", "--kb must be a non-empty knowledge base id");
  }
  const paths = resolvePaths(options.sourceRoot, options.outDir);
  const createdAt = options.createdAt ?? new Date().toISOString();
  if (Number.isNaN(Date.parse(createdAt))) {
    throw new ArchiveError("E_USAGE", `--created-at ${JSON.stringify(createdAt)} is not a timestamp`);
  }

  // Deployment context first: a bad registry must fail before any output exists.
  const kbBlock = readRegistry(options.registryPath, options.kb, options.limits.maxBytes);

  // Frozen inventory before the output directory exists: a budget overrun
  // leaves no half-archive behind.
  const inventory = walkSource(paths.source, options.limits);

  mkdirSync(paths.out, { recursive: false });
  mkdirSync(join(paths.out, BLOBS_DIR));

  // Set once COMPLETE.json exists. The catch below retracts the marker whenever
  // this is true, so no failure path can leave a "finished" archive behind.
  let markerPublished = false;
  // Declared outside `try`: the failure path must be able to rewrite it as
  // `complete:false` even when the error happened after it was published.
  let manifest: ArchiveManifest | null = null;

  try {
    // The database is read from a private scratch copy of the frozen originals;
    // the deployed file itself is never opened.
    const database = readDatabase(paths.source, paths.out, inventory, options.limits);
    const authority = database.authority;

    // --- files + blobs -----------------------------------------------------
    const filesWriter = new NdBuf(join(paths.out, FILES_STREAM));
    const blobs = new Map<string, number>();
    let fileBytes = 0;
    let budgetUsed = 0;
    for (const entry of inventory.entries) {
      if (budgetUsed + entry.bytes > options.limits.maxBytes) {
        throw new ArchiveError("E_BUDGET_BYTES", `export exceeded maxBytes=${options.limits.maxBytes} at ${entry.path}`);
      }
      const abs = join(paths.source, entry.path);
      const bytes = withReadFd(abs, entry.path, (fd) =>
        readExact(fd, entry.bytes, options.limits.maxBytes - budgetUsed, entry.path),
      );
      budgetUsed += bytes.length;
      if (sha256Hex(bytes) !== entry.sha256) {
        throw new ArchiveError("E_SOURCE_CHANGED", `${entry.path} changed between inventory and export`);
      }
      const blobRel = `${BLOBS_DIR}/${entry.sha256}`;
      if (!blobs.has(entry.sha256)) {
        writeFileSync(join(paths.out, blobRel), bytes, { flag: "wx" });
        blobs.set(entry.sha256, bytes.length);
      }
      const record: FileRecord = {
        record_type: "source_file",
        path: entry.path,
        bytes: entry.bytes,
        sha256: entry.sha256,
        blob: blobRel,
        mode: entry.mode,
        mtime_ms: entry.mtimeMs,
        ino: entry.ino,
      };
      filesWriter.push(record);
      fileBytes += entry.bytes;
    }
    const filesStream = filesWriter.close();

    let blobBytes = 0;
    for (const size of blobs.values()) blobBytes += size;

    // --- rows (structured authority only) ----------------------------------
    const rowsFile: StreamRef | null = database.rows?.stream ?? null;
    const rows = database.rows?.rows ?? 0;
    const tableCount = database.rows?.tables ?? 0;
    if (authority.kind === "structured" && tableCount !== TABLE_ORDER.length) {
      throw new ArchiveError(
        "E_VERIFY_STREAM",
        `enumerated ${tableCount} of ${TABLE_ORDER.length} registered tables; refusing a partial schema export`,
      );
    }

    const streams: StreamRef[] = [{ path: FILES_STREAM, ...filesStream, longest_line_bytes: filesStream.longestLine }];
    if (rowsFile !== null) streams.push(rowsFile);

    manifest = {
      archive_version: ARCHIVE_VERSION,
      tool: { name: TOOL_NAME, version: TOOL_VERSION },
      created_at: createdAt,
      kb: kbBlock,
      source: {
        root: options.sourceRoot,
        root_real: paths.source,
        snapshot: { entries: inventory.entries.length, bytes: inventory.totalBytes, digest: inventory.digest },
      },
      authority,
      limits: {
        max_files: options.limits.maxFiles,
        max_bytes: options.limits.maxBytes,
        max_rows: options.limits.maxRows,
        max_entries: options.limits.maxEntries,
        max_depth: options.limits.maxDepth,
      },
      counts: {
        files: inventory.entries.length,
        file_bytes: fileBytes,
        blobs: blobs.size,
        blob_bytes: blobBytes,
        rows,
        tables: tableCount,
      },
      streams,
      content_digest: "",
      complete: false,
    };
    manifest.content_digest = manifestContentDigest(manifest);
    writeManifest(paths.out, manifest);

    // --- frozen source recheck ---------------------------------------------
    options.hooks?.beforeFinalCheck?.();
    const recheck = walkSource(paths.source, options.limits);
    if (recheck.digest !== inventory.digest) {
      const changed = diffInventory(inventory.entries, recheck.entries);
      throw new ArchiveError(
        "E_SOURCE_CHANGED",
        `source tree changed during export: ${changed.length > 0 ? changed.join(", ") : recheck.digest}`,
      );
    }

    // --- read-back verification, then publish the completion marker ---------
    // The marker is the only thing that says "this archive is finished", so it is
    // published last and only after a full read-back. Every failure below this
    // point must retract it: a marker that outlives a failed run is worse than no
    // marker, because consumers would trust a directory the tool itself rejects.
    if (manifest === null) {
      throw new ArchiveError("E_VERIFY_MANIFEST", "manifest was not assembled");
    }
    const finalManifest = manifest;
    const ownBudget = deriveOwnVerificationLimits({
      manifest: finalManifest,
      filesStream,
      rowsStream: rowsFile,
      blobBytes: blobBytes,
      // The manifest is rewritten once more with `complete:true`, and the marker
      // is written too; both are bounded by what this run already produced.
      markerBytes: 4096,
    });
    options.hooks?.beforeMarkerVerify?.({ outDir: paths.out, manifest: finalManifest });

    verifyInternal(paths.out, { requireComplete: false, limits: ownBudget });
    finalManifest.complete = true;
    writeManifest(paths.out, finalManifest);
    markerPublished = true;

    const marker = {
      archive_version: ARCHIVE_VERSION,
      complete: true,
      content_digest: finalManifest.content_digest,
      source_digest: inventory.digest,
      verified_at: new Date().toISOString(),
      counts: finalManifest.counts,
      streams: finalManifest.streams.map((s) => ({ path: s.path, sha256: s.sha256, records: s.records })),
    };
    atomicWrite(paths.out, COMPLETE_FILE, Buffer.from(`${canonicalJson(marker)}\n`, "utf8"));

    options.hooks?.afterMarkerWrite?.({ outDir: paths.out, manifest: finalManifest });

    // The final gate: with the marker present, verification must succeed on the
    // archive as a consumer will read it. Hooks above let a test fail exactly here.
    verifyInternal(paths.out, { requireComplete: true, limits: ownBudget });
    return {
      outDir: paths.out,
      manifest: finalManifest,
      written: {
        archiveBytes: ownBudget.observedArchiveBytes,
        records: ownBudget.observedRecords,
        maxLineBytes: ownBudget.maxLineBytes,
      },
    };
  } catch (err) {
    // The directory stays (deleting it would erase evidence of what was written),
    // but no completion marker may survive. If cleanup itself fails, say so: never
    // report the marker as removed when the unlink did not happen.
    if (markerPublished) {
      let cleanupFailure: string | null = null;
      try {
        rmSync(join(paths.out, COMPLETE_FILE), { force: true });
        if (existsSync(join(paths.out, COMPLETE_FILE))) {
          cleanupFailure = `${COMPLETE_FILE} still exists after removal`;
        }
      } catch (cleanupErr) {
        cleanupFailure = `${COMPLETE_FILE} could not be removed: ${(cleanupErr as Error).message}`;
      }
      try {
        if (manifest !== null) {
          manifest.complete = false;
          manifest.content_digest = manifestContentDigest(manifest);
          writeManifest(paths.out, manifest);
        }
      } catch (manifestErr) {
        const detail =
          cleanupFailure === null
            ? `manifest could not be reset to complete:false: ${(manifestErr as Error).message}`
            : `${cleanupFailure}; additionally ${(manifestErr as Error).message}`;
        throw new ArchiveError(
          "E_INCOMPLETE",
          `export failed and cleanup was incomplete (${detail}). Original error: ${(err as Error).message}`,
        );
      }
      if (cleanupFailure !== null) {
        throw new ArchiveError(
          "E_INCOMPLETE",
          `export failed and cleanup was incomplete (${cleanupFailure}). Original error: ${(err as Error).message}`,
        );
      }
    }
    throw err;
  }
}

/**
 * Derive the export's own verification ceiling from what it actually wrote.
 *
 * This is deliberately *not* `manifest.limits`: a verifier must not take its
 * budget from the artefact it is checking. The numbers here are known by
 * construction — each stream reports its own byte and record totals, and blobs
 * were counted as they were written — with a fixed small allowance for the
 * manifest and marker re-writes. Checked arithmetic: any overflow is refused
 * rather than wrapped.
 */
function deriveOwnVerificationLimits(input: {
  manifest: ArchiveManifest;
  filesStream: { bytes: number; records: number; longestLine: number };
  rowsStream: StreamRef | null;
  blobBytes: number;
  markerBytes: number;
}): VerificationBudget {
  const add = (a: number, b: number, what: string): number => {
    const sum = a + b;
    if (!Number.isSafeInteger(sum)) {
      throw new ArchiveError("E_USAGE", `own verification budget overflow while adding ${what}`);
    }
    return sum;
  };
  const rowsBytes = input.rowsStream?.bytes ?? 0;
  const rowsRecords = input.rowsStream?.records ?? 0;
  // The manifest is serialised twice (once before, once with complete:true) and
  // both copies are read back; bounding by twice the first serialisation plus the
  // marker allowance is generous and still derived from our own output.
  const manifestBytes = add(
    Buffer.byteLength(`${canonicalJson(input.manifest)}\n`, "utf8"),
    Buffer.byteLength(`${canonicalJson({ ...input.manifest, complete: true })}\n`, "utf8"),
    "manifest",
  );
  const archiveBytes = add(
    add(add(input.filesStream.bytes, rowsBytes, "streams"), input.blobBytes, "blobs"),
    add(manifestBytes, input.markerBytes, "manifest+marker"),
    "archive",
  );
  return {
    maxArchiveBytes: archiveBytes,
    maxRecords: add(input.filesStream.records, rowsRecords, "records"),
    maxLineBytes: Math.max(input.filesStream.longestLine, input.rowsStream?.longest_line_bytes ?? 0, 1),
    maxEntries: add(input.filesStream.records, 1, "entries"),
    observedArchiveBytes: archiveBytes,
    observedRecords: add(input.filesStream.records, rowsRecords, "records"),
  };
}

function diffInventory(before: InventoryEntry[], after: InventoryEntry[]): string[] {
  const beforeByPath = new Map(before.map((e) => [e.path, e]));
  const afterByPath = new Map(after.map((e) => [e.path, e]));
  const changed: string[] = [];
  for (const [path, entry] of beforeByPath) {
    const other = afterByPath.get(path);
    if (other === undefined) changed.push(`${path} removed`);
    else if (other.sha256 !== entry.sha256 || other.bytes !== entry.bytes || other.mtimeMs !== entry.mtimeMs) {
      changed.push(`${path} modified`);
    }
  }
  for (const path of afterByPath.keys()) if (!beforeByPath.has(path)) changed.push(`${path} added`);
  return changed;
}

function writeManifest(outDir: string, manifest: ArchiveManifest): void {
  atomicWrite(outDir, MANIFEST_FILE, Buffer.from(`${canonicalJson(manifest)}\n`, "utf8"));
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

type JsonObject = Record<string, unknown>;

function asObject(value: unknown, what: string): JsonObject {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new ArchiveError("E_VERIFY_MANIFEST", `${what} must be an object`);
  }
  return value as JsonObject;
}

function requireString(obj: JsonObject, key: string, what: string): string {
  const value = obj[key];
  if (typeof value !== "string") throw new ArchiveError("E_VERIFY_MANIFEST", `${what}.${key} must be a string`);
  return value;
}

function requireNumber(obj: JsonObject, key: string, what: string): number {
  const value = obj[key];
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new ArchiveError("E_VERIFY_MANIFEST", `${what}.${key} must be a number`);
  }
  return value;
}

function decodeValue(encoded: unknown, where: string): EncodedValue {
  const obj = asObject(encoded, where);
  const kind = requireString(obj, "t", where);
  switch (kind) {
    case "null":
      return { t: "null" };
    case "integer": {
      const v = requireString(obj, "v", where);
      if (!/^-?(0|[1-9][0-9]*)$/.test(v) || v === "-0") {
        throw new ArchiveError("E_VERIFY_STREAM", `${where} integer is not canonical: ${JSON.stringify(v)}`);
      }
      BigInt(v); // throws on overflow beyond the signed 64-bit range
      const asNumber = BigInt(v);
      if (asNumber > 9223372036854775807n || asNumber < -9223372036854775808n) {
        throw new ArchiveError("E_VERIFY_STREAM", `${where} integer exceeds signed 64-bit range`);
      }
      return { t: "integer", v };
    }
    case "real": {
      const bits = requireString(obj, "bits", where);
      if (!/^[0-9a-f]{16}$/.test(bits)) throw new ArchiveError("E_VERIFY_STREAM", `${where} real bits malformed`);
      return { t: "real", bits };
    }
    case "text": {
      const v = requireString(obj, "v", where);
      if (Buffer.from(v, "utf8").toString("utf8") !== v) {
        throw new ArchiveError("E_VERIFY_STREAM", `${where} text is not valid UTF-8`);
      }
      return { t: "text", v };
    }
    case "blob": {
      const b64 = requireString(obj, "bytes_base64", where);
      const bytes = Buffer.from(b64, "base64");
      if (bytes.toString("base64") !== b64) {
        throw new ArchiveError("E_VERIFY_STREAM", `${where} blob base64 is not canonical`);
      }
      const declared = requireNumber(obj, "bytes", where);
      if (bytes.length !== declared) throw new ArchiveError("E_VERIFY_STREAM", `${where} blob length mismatch`);
      return { t: "blob", bytes_base64: b64, bytes: declared };
    }
    default:
      throw new ArchiveError("E_VERIFY_STREAM", `${where} has unknown value type ${JSON.stringify(kind)}`);
  }
}

interface InternalVerifyOptions {
  requireComplete: boolean;
  /** Caller-owned ceilings. Never read from the archive being verified. */
  limits: VerificationBudget;
}

function assertVerificationLimits(limits: VerificationLimits): void {
  const check = (name: keyof VerificationLimits): number => {
    const value = limits[name];
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) {
      throw new ArchiveError("E_USAGE", `${name} must be a positive finite integer`);
    }
    return value;
  };
  check("maxArchiveBytes");
  check("maxRecords");
  check("maxLineBytes");
  check("maxEntries");
}

function verifyInternal(outDir: string, options: InternalVerifyOptions): VerifyResult {
  assertVerificationLimits(options.limits);
  const manifestPath = join(outDir, MANIFEST_FILE);
  if (!existsSync(manifestPath)) {
    throw new ArchiveError("E_VERIFY_MANIFEST", `${outDir} has no ${MANIFEST_FILE}`);
  }
  // Bound the manifest before parsing it: a multi-gigabyte `archive.json` would
  // otherwise be fully allocated before any check could run.
  const manifestSize = statSync(manifestPath).size;
  if (manifestSize > options.limits.maxArchiveBytes) {
    throw new ArchiveError(
      "E_VERIFY_BUDGET",
      `${MANIFEST_FILE} is ${manifestSize} bytes, over maxArchiveBytes=${options.limits.maxArchiveBytes}`,
    );
  }
  const manifestRaw = readFileSync(manifestPath, "utf8");
  let parsed: unknown;
  try {
    parsed = JSON.parse(manifestRaw);
  } catch (err) {
    throw new ArchiveError("E_VERIFY_MANIFEST", `${MANIFEST_FILE} is not JSON: ${(err as Error).message}`);
  }
  const manifest = parsed as ArchiveManifest;
  const root = asObject(parsed, "manifest");

  // Cumulative accounting against the caller's ceilings. Every byte and record
  // counted below reduces what remains for the next stream/blob.
  let remainingBytes = options.limits.maxArchiveBytes - manifestSize;
  let remainingRecords = options.limits.maxRecords;
  const spendBytes = (amount: number, what: string): void => {
    if (amount > remainingBytes) {
      throw new ArchiveError(
        "E_VERIFY_BUDGET",
        `${what} needs ${amount} bytes, only ${remainingBytes} of maxArchiveBytes=${options.limits.maxArchiveBytes} remain`,
      );
    }
    remainingBytes -= amount;
  };
  const spendRecords = (amount: number, what: string): void => {
    if (amount > remainingRecords) {
      throw new ArchiveError(
        "E_VERIFY_BUDGET",
        `${what} needs ${amount} records, only ${remainingRecords} of maxRecords=${options.limits.maxRecords} remain`,
      );
    }
    remainingRecords -= amount;
  };

  if (root["archive_version"] !== ARCHIVE_VERSION) {
    throw new ArchiveError(
      "E_VERIFY_MANIFEST",
      `unsupported archive_version ${JSON.stringify(root["archive_version"])}; this tool reads ${ARCHIVE_VERSION}`,
    );
  }
  for (const key of ["tool", "created_at", "kb", "source", "authority", "limits", "counts", "streams"]) {
    if (!(key in root)) throw new ArchiveError("E_VERIFY_MANIFEST", `manifest is missing ${key}`);
  }

  const declaredDigest = requireString(root, "content_digest", "manifest");
  const recomputedDigest = manifestContentDigest(manifest);
  if (recomputedDigest !== declaredDigest) {
    throw new ArchiveError(
      "E_VERIFY_DIGEST",
      `manifest content_digest mismatch (declared ${declaredDigest}, recomputed ${recomputedDigest})`,
    );
  }

  const authorityObj = asObject(root["authority"], "manifest.authority");
  const authorityKind = requireString(authorityObj, "kind", "manifest.authority");
  if (authorityKind !== "markdown" && authorityKind !== "structured") {
    throw new ArchiveError("E_VERIFY_MANIFEST", `unknown authority kind ${JSON.stringify(authorityKind)}`);
  }

  const counts = asObject(root["counts"], "manifest.counts");
  const expectedCounts = {
    files: requireNumber(counts, "files", "manifest.counts"),
    file_bytes: requireNumber(counts, "file_bytes", "manifest.counts"),
    blobs: requireNumber(counts, "blobs", "manifest.counts"),
    blob_bytes: requireNumber(counts, "blob_bytes", "manifest.counts"),
    rows: requireNumber(counts, "rows", "manifest.counts"),
    tables: requireNumber(counts, "tables", "manifest.counts"),
  };

  const schemaTables: string[] = [];
  const columnOrder = new Map<string, string[]>();
  if (authorityKind === "structured") {
    const database = asObject(authorityObj["database"], "manifest.authority.database");
    const schema = asObject(database["schema"], "manifest.authority.database.schema");
    const tables = schema["tables"];
    if (!Array.isArray(tables)) throw new ArchiveError("E_VERIFY_MANIFEST", "manifest schema.tables must be an array");
    for (const table of tables) {
      const t = asObject(table, "manifest schema table");
      const name = requireString(t, "name", "manifest schema table");
      if (schemaTables.includes(name)) throw new ArchiveError("E_VERIFY_MANIFEST", `duplicate table ${name}`);
      const columns = t["columns"];
      if (!Array.isArray(columns)) throw new ArchiveError("E_VERIFY_MANIFEST", `table ${name} columns must be an array`);
      const names: string[] = [];
      for (const column of columns) {
        const c = asObject(column, `table ${name} column`);
        names.push(requireString(c, "name", `table ${name} column`));
      }
      schemaTables.push(name);
      columnOrder.set(name, names);
    }
    if (dbLogicalDigest(database) === null) {
      throw new ArchiveError("E_VERIFY_MANIFEST", "structured authority has no logical_snapshot_digest");
    }
  }

  // Streams: re-hash bytes, re-count records, validate every record shape.
  const streamsRaw = root["streams"];
  if (!Array.isArray(streamsRaw) || streamsRaw.length === 0) {
    throw new ArchiveError("E_VERIFY_MANIFEST", "manifest.streams must be a non-empty array");
  }
  let fileRecords = 0;
  let fileBytes = 0;
  let rowRecords = 0;
  const referencedBlobs = new Map<string, number>();
  const fileRecordsByPath = new Map<string, { sha256: string; bytes: number }>();
  const rowTablesSeen: string[] = [];
  const perTableCounts = new Map<string, number>();

  for (const streamRaw of streamsRaw) {
    const stream = asObject(streamRaw, "manifest stream");
    const streamPath = requireString(stream, "path", "manifest stream");
    if (streamPath !== FILES_STREAM && streamPath !== ROWS_STREAM) {
      throw new ArchiveError("E_VERIFY_MANIFEST", `unknown stream ${JSON.stringify(streamPath)}`);
    }
    const declaredSha = requireString(stream, "sha256", "manifest stream");
    const declaredBytes = requireNumber(stream, "bytes", "manifest stream");
    const declaredRecords = requireNumber(stream, "records", "manifest stream");
    const abs = join(outDir, streamPath);
    if (!existsSync(abs)) throw new ArchiveError("E_VERIFY_STREAM", `stream ${streamPath} is missing`);
    // The declared length is checked against the caller's remaining budget before
    // a single byte is read; the manifest's own claim cannot widen it.
    spendBytes(declaredBytes, `stream ${streamPath}`);
    spendRecords(declaredRecords, `stream ${streamPath}`);
    // Streams are hashed in bounded chunks, never read whole: a 4 GiB NDJSON
    // written by some other tool must not become a 4 GiB allocation here.
    const actualSha = hashFileExact(abs, declaredBytes, `stream ${streamPath}`);
    if (actualSha !== declaredSha) {
      throw new ArchiveError("E_VERIFY_STREAM", `stream ${streamPath} digest mismatch`);
    }
    let seen = 0;
    const perTableRowIndex = new Map<string, number>();
    readLinesSync(abs, options.limits.maxLineBytes, (line, lineIndex) => {
      if (line === "") throw new ArchiveError("E_VERIFY_STREAM", `stream ${streamPath}:${lineIndex} is empty`);
      let record: unknown;
      try {
        record = JSON.parse(line);
      } catch (err) {
        throw new ArchiveError("E_VERIFY_STREAM", `stream ${streamPath}:${lineIndex} is not JSON: ${(err as Error).message}`);
      }
      const obj = asObject(record, `stream ${streamPath}:${lineIndex}`);
      const type = requireString(obj, "record_type", `stream ${streamPath}:${lineIndex}`);
      if (streamPath === FILES_STREAM) {
        if (type !== "source_file") {
          throw new ArchiveError("E_VERIFY_STREAM", `stream ${streamPath}:${lineIndex} has record ${type}`);
        }
        const recordPath = requireString(obj, "path", `stream ${streamPath}:${lineIndex}`);
        if (recordPath.startsWith("/") || recordPath.split("/").includes("..")) {
          throw new ArchiveError("E_VERIFY_STREAM", `unsafe recorded path ${JSON.stringify(recordPath)}`);
        }
        // Provenance fields are part of the record contract: `ino` is exact
        // 64-bit decimal text, so a numeric (or malformed) value means the writer
        // used a lossy form and the archive is not what this format describes.
        const ino = requireString(obj, "ino", `stream ${streamPath}:${lineIndex}`);
        if (!/^(0|[1-9][0-9]*)$/.test(ino)) {
          throw new ArchiveError("E_VERIFY_STREAM", `file ${recordPath} has a non-decimal ino: ${JSON.stringify(ino)}`);
        }
        requireNumber(obj, "mode", `stream ${streamPath}:${lineIndex}`);
        requireNumber(obj, "mtime_ms", `stream ${streamPath}:${lineIndex}`);
        const sha = requireString(obj, "sha256", `stream ${streamPath}:${lineIndex}`);
        if (!/^[0-9a-f]{64}$/.test(sha)) throw new ArchiveError("E_VERIFY_STREAM", `bad sha256 for ${recordPath}`);
        const bytes = requireNumber(obj, "bytes", `stream ${streamPath}:${lineIndex}`);
        const blob = requireString(obj, "blob", `stream ${streamPath}:${lineIndex}`);
        if (blob !== `${BLOBS_DIR}/${sha}`) {
          throw new ArchiveError("E_VERIFY_STREAM", `file ${recordPath} points at ${blob}, expected its own hash`);
        }
        const existing = referencedBlobs.get(sha);
        if (existing !== undefined && existing !== bytes) {
          throw new ArchiveError("E_VERIFY_STREAM", `blob ${sha} referenced with two lengths`);
        }
        referencedBlobs.set(sha, bytes);
        fileRecordsByPath.set(recordPath, { sha256: sha, bytes });
        fileRecords += 1;
        fileBytes += bytes;
      } else {
        if (type !== "source_row") {
          throw new ArchiveError("E_VERIFY_STREAM", `stream ${streamPath}:${lineIndex} has record ${type}`);
        }
        const table = requireString(obj, "table", `stream ${streamPath}:${lineIndex}`);
        if (!schemaTables.includes(table)) {
          throw new ArchiveError("E_VERIFY_STREAM", `row references unknown table ${JSON.stringify(table)}`);
        }
        const expectedColumns = columnOrder.get(table)!;
        const columns = obj["columns"];
        if (!Array.isArray(columns) || columns.length !== expectedColumns.length) {
          throw new ArchiveError("E_VERIFY_STREAM", `row in ${table} has wrong column count`);
        }
        const columnNames = columns.map((c, i) => {
          if (typeof c !== "string") throw new ArchiveError("E_VERIFY_STREAM", `row in ${table} has non-string column`);
          if (c !== expectedColumns[i]) {
            throw new ArchiveError("E_VERIFY_STREAM", `row in ${table} column ${i} is ${c}, expected ${expectedColumns[i]}`);
          }
          return c;
        });
        const values = obj["values"];
        if (!Array.isArray(values) || values.length !== columnNames.length) {
          throw new ArchiveError("E_VERIFY_STREAM", `row in ${table} has wrong value count`);
        }
        values.forEach((value, i) => {
          decodeValue(value, `stream ${streamPath}:${lineIndex} ${table}.${columnNames[i]}`);
        });
        const rowIndex = requireNumber(obj, "row_index", `stream ${streamPath}:${lineIndex}`);
        const expectedIndex = perTableRowIndex.get(table) ?? 0;
        if (rowIndex !== expectedIndex) {
          throw new ArchiveError(
            "E_VERIFY_STREAM",
            `row order broken in ${table}: expected row_index ${expectedIndex}, found ${rowIndex}`,
          );
        }
        perTableRowIndex.set(table, expectedIndex + 1);
        if (!rowTablesSeen.includes(table)) rowTablesSeen.push(table);
        perTableCounts.set(table, (perTableCounts.get(table) ?? 0) + 1);
        rowRecords += 1;
      }
      seen += 1;
    });
    if (seen !== declaredRecords) {
      throw new ArchiveError("E_VERIFY_STREAM", `stream ${streamPath} has ${seen} records, manifest says ${declaredRecords}`);
    }
  }

  // Blobs: every referenced blob must exist with the declared length and hash.
  const blobsDir = join(outDir, BLOBS_DIR);
  if (!existsSync(blobsDir)) throw new ArchiveError("E_VERIFY_BLOB", `${BLOBS_DIR}/ is missing`);
  // Ceilings come from the caller. `manifest.limits` is provenance written at
  // export time and is never consulted here: an archive cannot raise its own
  // verification budget, which is the whole point of F2.
  for (const [sha, bytes] of referencedBlobs) {
    const abs = join(blobsDir, sha);
    if (!existsSync(abs)) throw new ArchiveError("E_VERIFY_BLOB", `blob ${sha} is missing`);
    spendBytes(bytes, `blob ${sha}`);
    const actual = hashFileExact(abs, bytes, `blob ${sha}`);
    if (actual !== `sha256:${sha}`) throw new ArchiveError("E_VERIFY_BLOB", `blob ${sha} content hash mismatch`);
  }
  let listedCount = 0;
  const dir = opendirSync(blobsDir);
  try {
    for (;;) {
      const entry = dir.readSync();
      if (entry === null) break;
      listedCount += 1;
      if (listedCount > options.limits.maxEntries) {
        throw new ArchiveError(
          "E_VERIFY_BUDGET",
          `${BLOBS_DIR}/ holds more than maxEntries=${options.limits.maxEntries} entries`,
        );
      }
      const name = entry.name;
      if (!referencedBlobs.has(name)) {
        throw new ArchiveError("E_VERIFY_BLOB", `unreferenced blob ${name} present; archive is not self-consistent`);
      }
    }
  } finally {
    dir.closeSync();
  }

  if (fileRecords !== expectedCounts.files) {
    throw new ArchiveError("E_VERIFY_COUNTS", `file count ${fileRecords} != manifest ${expectedCounts.files}`);
  }
  if (fileBytes !== expectedCounts.file_bytes) {
    throw new ArchiveError("E_VERIFY_COUNTS", `file bytes ${fileBytes} != manifest ${expectedCounts.file_bytes}`);
  }
  if (referencedBlobs.size !== expectedCounts.blobs) {
    throw new ArchiveError("E_VERIFY_COUNTS", `blob count ${referencedBlobs.size} != manifest ${expectedCounts.blobs}`);
  }
  if (rowRecords !== expectedCounts.rows) {
    throw new ArchiveError("E_VERIFY_COUNTS", `row count ${rowRecords} != manifest ${expectedCounts.rows}`);
  }
  if (authorityKind === "markdown" && rowRecords !== 0) {
    throw new ArchiveError("E_VERIFY_COUNTS", "markdown authority must not carry row records");
  }
  if (authorityKind === "structured") {
    const rowsStream = streamsRaw
      .map((s) => asObject(s, "manifest stream"))
      .find((s) => s["path"] === ROWS_STREAM);
    if (rowsStream === undefined) throw new ArchiveError("E_VERIFY_MANIFEST", "structured authority has no row stream");
    const declared = rowsStream["tables"];
    if (!Array.isArray(declared)) {
      throw new ArchiveError("E_VERIFY_MANIFEST", "row stream must declare the per-table row counts it enumerated");
    }
    const declaredByName = new Map<string, number>();
    for (const entryRaw of declared) {
      const entry = asObject(entryRaw, "row stream table entry");
      const name = requireString(entry, "table", "row stream table entry");
      if (declaredByName.has(name)) throw new ArchiveError("E_VERIFY_MANIFEST", `table ${name} declared twice`);
      declaredByName.set(name, requireNumber(entry, "rows", "row stream table entry"));
    }
    for (const table of schemaTables) {
      if (!declaredByName.has(table)) {
        throw new ArchiveError("E_VERIFY_STREAM", `table ${table} is registered but not declared as enumerated`);
      }
    }
    for (const [table, declaredRows] of declaredByName) {
      if (!schemaTables.includes(table)) {
        throw new ArchiveError("E_VERIFY_STREAM", `row stream declares unknown table ${table}`);
      }
      const actualRows = perTableCounts.get(table) ?? 0;
      if (declaredRows !== actualRows) {
        throw new ArchiveError(
          "E_VERIFY_STREAM",
          `table ${table} declared ${declaredRows} rows, stream holds ${actualRows}`,
        );
      }
    }
    for (const table of rowTablesSeen) {
      if (!declaredByName.has(table)) {
        throw new ArchiveError("E_VERIFY_STREAM", `row stream contains undeclared table ${table}`);
      }
    }
  } else if (streamsRaw.some((s) => asObject(s, "manifest stream")["path"] === ROWS_STREAM)) {
    throw new ArchiveError("E_VERIFY_MANIFEST", "markdown authority must not carry a row stream");
  }

  // Database originals live in the ordinary file stream, so their bytes are
  // already re-hashed above. What still needs checking is that the database block
  // agrees with that stream: the same names, the same hashes, and an authority
  // reason consistent with what was actually retained.
  if (authorityObj["database"] !== null && authorityObj["database"] !== undefined) {
    const database = asObject(authorityObj["database"], "manifest.authority.database");
    const files = database["files"];
    if (!Array.isArray(files)) throw new ArchiveError("E_VERIFY_MANIFEST", "database.files must be an array");
    const names: string[] = [];
    for (const fileRaw of files) {
      const file = asObject(fileRaw, "database.files entry");
      const name = requireString(file, "path", "database.files entry");
      if (name.includes("/") || name.includes("..")) {
        throw new ArchiveError("E_VERIFY_MANIFEST", `unsafe database path ${JSON.stringify(name)}`);
      }
      const sha = requireString(file, "sha256", "database.files entry");
      const bytes = requireNumber(file, "bytes", "database.files entry");
      const recorded = fileRecordsByPath.get(name);
      if (recorded === undefined) {
        throw new ArchiveError("E_VERIFY_MANIFEST", `database file ${name} has no source_file record`);
      }
      if (recorded.sha256 !== sha || recorded.bytes !== bytes) {
        throw new ArchiveError("E_VERIFY_MANIFEST", `database file ${name} disagrees with its source_file record`);
      }
      names.push(name);
    }
    if (names.includes(DB_FILE) !== (authorityObj["reason"] !== "no-database")) {
      throw new ArchiveError("E_VERIFY_MANIFEST", "database files contradict the authority reason");
    }
    const declaredWal = requireNumber(database, "wal_bytes", "manifest.authority.database");
    const walRecord = fileRecordsByPath.get("knowledge.db-wal");
    const walBytes = walRecord === undefined ? 0 : walRecord.bytes;
    if (walBytes !== declaredWal) {
      throw new ArchiveError("E_VERIFY_MANIFEST", "database.wal_bytes does not match the retained WAL");
    }
  }

  if (options.requireComplete) {
    if (root["complete"] !== true) {
      throw new ArchiveError("E_INCOMPLETE", "manifest.complete is not true");
    }
    const markerPath = join(outDir, COMPLETE_FILE);
    if (!existsSync(markerPath)) {
      throw new ArchiveError("E_INCOMPLETE", `${COMPLETE_FILE} is missing; the export never finished`);
    }
    const marker = asObject(JSON.parse(readFileSync(markerPath, "utf8")), COMPLETE_FILE);
    if (marker["complete"] !== true) throw new ArchiveError("E_INCOMPLETE", `${COMPLETE_FILE}.complete is not true`);
    if (marker["archive_version"] !== ARCHIVE_VERSION) {
      throw new ArchiveError("E_VERIFY_MANIFEST", `${COMPLETE_FILE} archive_version mismatch`);
    }
    if (requireString(marker, "content_digest", COMPLETE_FILE) !== declaredDigest) {
      throw new ArchiveError("E_VERIFY_MANIFEST", `${COMPLETE_FILE} content_digest does not match the manifest`);
    }
    const source = asObject(root["source"], "manifest.source");
    const snapshot = asObject(source["snapshot"], "manifest.source.snapshot");
    if (requireString(marker, "source_digest", COMPLETE_FILE) !== requireString(snapshot, "digest", "manifest.source.snapshot")) {
      throw new ArchiveError("E_VERIFY_MANIFEST", `${COMPLETE_FILE} source_digest does not match the manifest`);
    }
  } else if (root["complete"] === true) {
    throw new ArchiveError("E_VERIFY_MANIFEST", "manifest claims complete before verification");
  }

  return {
    outDir,
    contentDigest: declaredDigest,
    counts: expectedCounts,
    authority: authorityKind,
  };
}

function dbLogicalDigest(database: JsonObject): string | null {
  const value = database["logical_snapshot_digest"];
  if (value === null || value === undefined) return null;
  if (typeof value !== "string") throw new ArchiveError("E_VERIFY_MANIFEST", "logical_snapshot_digest must be a string");
  return value;
}

/**
 * Read-only verification of a finished archive.
 *
 * `limits` is required and belongs to the caller. There is no default: a verifier
 * that cannot state its own ceilings would have to trust the archive's, which is
 * exactly the property this API exists to prevent.
 */
export function verifySourceArchive(outDir: string, limits: VerificationLimits): VerifyResult {
  assertVerificationLimits(limits);
  return verifyInternal(resolve(outDir), {
    requireComplete: true,
    limits: { ...limits, observedArchiveBytes: 0, observedRecords: 0 },
  });
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

interface CliOptions {
  mode: "export" | "verify";
  source?: string;
  out?: string;
  kb?: string;
  registry?: string;
  maxFiles?: number;
  maxBytes?: number;
  maxRows?: number;
  maxEntries?: number;
  maxDepth?: number;
  createdAt?: string;
  verifyDir?: string;
  maxArchiveBytes?: number;
  maxRecords?: number;
  maxLineBytes?: number;
  verifyMaxEntries?: number;
}

function parsePositiveInt(flag: string, raw: string | undefined): number {
  if (raw === undefined) throw new ArchiveError("E_USAGE", `${flag} requires a value`);
  if (!/^[0-9]+$/.test(raw)) throw new ArchiveError("E_USAGE", `${flag} must be a positive integer`);
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new ArchiveError("E_USAGE", `${flag} must be a positive integer`);
  }
  return value;
}

/** Depth is a level count, so 0 ("root only") is legal where sizes are not. */
function parseNonNegativeInt(flag: string, raw: string | undefined): number {
  if (raw === undefined || !/^[0-9]+$/.test(raw)) {
    throw new ArchiveError("E_USAGE", `${flag} must be a non-negative integer`);
  }
  const value = Number(raw);
  if (!Number.isSafeInteger(value)) throw new ArchiveError("E_USAGE", `${flag} must be a non-negative integer`);
  return value;
}

export function parseCli(argv: readonly string[]): CliOptions {
  if (argv.includes("--help") || argv.includes("-h")) {
    throw new ArchiveError("E_HELP", usage());
  }
  const opts: CliOptions = { mode: "export" };
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i]!;
    const next = (): string => {
      const value = argv[i + 1];
      if (value === undefined || value.startsWith("--")) {
        throw new ArchiveError("E_USAGE", `${flag} requires a value`);
      }
      i += 1;
      return value;
    };
    switch (flag) {
      case "--source":
        opts.source = next();
        break;
      case "--out":
        opts.out = next();
        break;
      case "--kb":
        opts.kb = next();
        break;
      case "--registry":
        opts.registry = next();
        break;
      case "--max-files":
        opts.maxFiles = parsePositiveInt(flag, next());
        break;
      case "--max-bytes":
        opts.maxBytes = parsePositiveInt(flag, next());
        break;
      case "--max-rows":
        opts.maxRows = parsePositiveInt(flag, next());
        break;
      case "--max-entries":
        opts.maxEntries = parsePositiveInt(flag, next());
        break;
      case "--max-depth":
        opts.maxDepth = parseNonNegativeInt(flag, next());
        break;
      case "--created-at":
        opts.createdAt = next();
        break;
      case "--verify":
        opts.mode = "verify";
        opts.verifyDir = next();
        break;
      case "--max-archive-bytes":
        opts.maxArchiveBytes = parsePositiveInt(flag, next());
        break;
      case "--max-records":
        opts.maxRecords = parsePositiveInt(flag, next());
        break;
      case "--max-line-bytes":
        opts.maxLineBytes = parsePositiveInt(flag, next());
        break;
      case "--verify-max-entries":
        opts.verifyMaxEntries = parsePositiveInt(flag, next());
        break;
      default:
        throw new ArchiveError("E_USAGE", `unknown argument ${JSON.stringify(flag)}`);
    }
  }
  return opts;
}

export function usage(): string {
  return [
    "usage:",
    "  bun tools/sage-db-source-archive.ts --source <kb-root> --out <new-dir> --kb <id>",
    "      --registry <registry.json> --max-files N --max-bytes N --max-rows N",
    "      --max-entries N --max-depth N [--created-at ISO]",
    "  bun tools/sage-db-source-archive.ts --verify <archive-dir>",
    "      --max-archive-bytes N --max-records N --max-line-bytes N --verify-max-entries N",
    "",
    "Every path and limit is explicit: there is no production default. The source",
    "root is exported byte-for-byte; `knowledge.db` rows are exported only when the",
    "database is readable, schema version 1, and meta.imported_at is set.",
    "",
    "Verification takes its ceilings from the caller, never from the archive being",
    "verified: an archive cannot raise the budget spent checking it.",
  ].join("\n");
}

export function runCli(argv: readonly string[]): number {
  let opts: CliOptions;
  try {
    opts = parseCli(argv);
  } catch (err) {
    if (err instanceof ArchiveError && err.code === "E_HELP") {
      process.stdout.write(`${usage()}\n`);
      return EXIT.ok;
    }
    process.stderr.write(`${(err as Error).message}\n\n${usage()}\n`);
    return EXIT.usage;
  }

  try {
    if (opts.mode === "verify") {
      const dir = opts.verifyDir;
      if (dir === undefined) throw new ArchiveError("E_USAGE", "--verify requires a directory");
      const { maxArchiveBytes, maxRecords, maxLineBytes, verifyMaxEntries } = opts;
      if (maxArchiveBytes === undefined || maxRecords === undefined || maxLineBytes === undefined ||
          verifyMaxEntries === undefined) {
        throw new ArchiveError(
          "E_USAGE",
          "--verify requires explicit ceilings: --max-archive-bytes, --max-records, " +
            "--max-line-bytes and --verify-max-entries",
        );
      }
      const result = verifySourceArchive(dir, {
        maxArchiveBytes,
        maxRecords,
        maxLineBytes,
        maxEntries: verifyMaxEntries,
      });
      process.stdout.write(
        `verified ${resolve(dir)}\n` +
          `authority=${result.authority} files=${result.counts.files} rows=${result.counts.rows} ` +
          `blobs=${result.counts.blobs} digest=${result.contentDigest}\n`,
      );
      return EXIT.ok;
    }

    const { source, out, kb, registry, maxFiles, maxBytes, maxRows, maxEntries, maxDepth } = opts;
    if (source === undefined || out === undefined || kb === undefined || registry === undefined ||
        maxFiles === undefined || maxBytes === undefined || maxRows === undefined ||
        maxEntries === undefined || maxDepth === undefined) {
      throw new ArchiveError(
        "E_USAGE",
        "--source, --out, --kb, --registry and all five bounds " +
          "(--max-files, --max-bytes, --max-rows, --max-entries, --max-depth) are required",
      );
    }
    const result = exportSourceArchive({
      sourceRoot: source,
      outDir: out,
      kb,
      registryPath: registry,
      limits: { maxFiles, maxBytes, maxRows, maxEntries, maxDepth },
      ...(opts.createdAt === undefined ? {} : { createdAt: opts.createdAt }),
    });
    const m = result.manifest;
    process.stdout.write(
      `exported ${result.outDir}\n` +
        `authority=${m.authority.kind}(${m.authority.reason}) files=${m.counts.files} ` +
        `file_bytes=${m.counts.file_bytes} rows=${m.counts.rows} blobs=${m.counts.blobs} ` +
        `digest=${m.content_digest}\n`,
    );
    return EXIT.ok;
  } catch (err) {
    if (err instanceof ArchiveError) {
      process.stderr.write(`${err.code}: ${err.message}\n`);
      if (err.code === "E_USAGE") return EXIT.usage;
      // A failed verification is its own outcome: a wrapper that retries export
      // on any non-zero code must not treat "archive is damaged" as "retry".
      return opts.mode === "verify" ? EXIT.verify : EXIT.refused;
    }
    process.stderr.write(`unexpected: ${(err as Error).stack ?? String(err)}\n`);
    return EXIT.refused;
  }
}

if (import.meta.main) {
  process.exit(runCli(process.argv.slice(2)));
}
