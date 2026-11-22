#!/usr/bin/env bun
/**
 * Deterministic, bounded source migration **plan** generator (plan format v2).
 *
 * Input is a frozen source archive produced and verified by
 * `sage-db-source-archive.ts`. Output is a migration **control artifact**: an
 * explicit statement of what would be imported, with stable source identities
 * mapped to proposed numeric target ids, and every unresolved target requirement
 * named rather than assumed.
 *
 * Not a record schema, not an importer, not a completion signal.
 *
 * ## Corrections carried in v2 (each was a real defect in v1)
 *
 * 1. **Every read is bounded at the descriptor.** v1 called `readFileSync().length`
 *    "streaming"; it allocated the whole file before any bound, then read it again,
 *    and the manifest/blob reads were unbounded `readFileSync` calls whose result
 *    could differ from the bytes verification saw. Here every read is
 *    `open` → `fstat` → exact-length loop → one-byte overflow probe, and blobs are
 *    only read from the canonical `blobs/<sha256>` path, re-hashed on the open fd.
 * 2. **Body bytes are the file's own bytes.** v1 stored `hash(parsed.molecule)`
 *    next to the whole-file blob, which is self-contradictory: the parser trims and
 *    normalises, so the hash did not describe any byte range of that blob. v2 slices
 *    the exact bytes after the frontmatter terminator — preserving leading/trailing
 *    whitespace, CRLF and Unicode — and binds `offset`+`length`+`body_sha256` to the
 *    raw file blob. The normalised service projection is kept separately and
 *    labelled as such.
 * 3. **Frontier identity is unique per table.** v1's `section#position` reset the
 *    counter on every new table, so two tables in one section produced duplicate
 *    ids. v2 uses `section/table/row`, a global `declaration_order`, and rejects any
 *    duplicate id in any family.
 * 4. **Proposals keep their real metadata.** v1 hand-rolled a scalar regex over the
 *    YAML head and discarded everything else, and it threw away known fields on the
 *    JSON variant. v2 parses with the same mature YAML parser `packages/core` uses,
 *    retains the whole head and payload, and turns unknown status / foreign concept /
 *    unexpected shape into errors plus `unresolved` entries.
 * 5. **Actor semantics match the target types.** `recorded_by` is an *optional
 *    verified principal*, not a provenance enum; the old files have no authenticated
 *    identity at all. v2 emits `recorded_by: null`, and `actor_origin` stays
 *    `Unresolved` until a real importer supplies a verified
 *    execution context, and the declared author/captured-by is kept in its own
 *    field. `LegacyDeclared` describes the source's own provenance only when it
 *    actually declared an actor; `Unavailable` is the honest value otherwise.
 *    Declared values are never relabelled as authentication.
 * 6. **Determinism does not depend on the clock.** v1 defaulted `created_at` to
 *    `new Date()`, so two runs differed. v2 requires an explicit plan context whose
 *    digest is recorded, and orders ids by UTF-8 byte comparison rather than JS
 *    UTF-16 code-unit comparison.
 *
 * ## Bound shapes, stated rather than implied
 *
 * Every read is `open` → `fstat` on that same descriptor → exact-length loop →
 * one-byte overflow probe. `files.ndjson` is **buffered whole** under
 * `maxArchiveBytes` and then scanned: bounded, but not a streaming line reader.
 * Record caps (including attachments) are charged before each push; auxiliary
 * diagnostics have their own ceiling. These are input-size bounds, not an
 * allocator or RSS proof. `compareUtf8` allocates two buffers per comparison.
 *
 * ## Bound to one capture
 *
 * The manifest and the file stream are re-read after verification and each read is
 * **joined** to the verified digests: a manifest or stream replaced by different
 * but internally valid bytes is refused as `E_ARCHIVE_CHANGED`, never mixed with
 * the earlier verification.
 *
 * CLI:
 *   bun tools/sage-db-import-plan.ts \
 *     --archive <archive-dir> --out <new-plan.json> --plan-context <json> \
 *     --max-entries N --max-bytes N --max-records N \
 *     --max-archive-bytes N --max-line-bytes N --max-diagnostics N
 */

import { createHash } from "node:crypto";
import {
  closeSync,
  existsSync,
  fstatSync,
  openSync,
  readSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { basename, dirname, join, resolve } from "node:path";

import { parseConceptFile } from "../packages/core/src/manifest/parse.ts";
import type { ConceptFile } from "../packages/core/src/types/concept.ts";
import {
  manifestContentDigest,
  verifySourceArchive,
  type ArchiveManifest,
} from "./sage-db-source-archive.ts";

export const PLAN_VERSION = "2" as const;
export const TOOL_NAME = "sage-db-import-plan";
export const TOOL_VERSION = 2;

export const EXIT = { ok: 0, usage: 64, refused: 65, incomplete: 66 } as const;

export class PlanError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "PlanError";
  }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Caller ceilings. Applied before growth; never derived from the input. */
export interface PlanLimits {
  /** Source files/entries the plan may describe. */
  maxEntries: number;
  /** Total source bytes the plan may read. */
  maxBytes: number;
  /**
   * Mapping records the plan may emit, across ALL families including attachments.
   * This is the global cap: an attachment is a mapping record, not free overhead.
   */
  maxRecords: number;
  /** Below: verification ceilings handed to the archive reader. */
  maxArchiveBytes: number;
  maxLineBytes: number;
  /**
   * Auxiliary diagnostic entries (`errors` + `unresolved` + `unavailable`) the plan
   * may accumulate. A separate, explicitly bounded policy: these are not mapping
   * records, and a source that generated one complaint per row would otherwise grow
   * without limit even though no record was emitted.
   */
  maxDiagnostics: number;
}

/**
 * Explicit run context. Required: the plan must not invent a timestamp, and the
 * digest binds this plan to one deliberate run.
 */
export interface PlanContext {
  /** Caller-supplied stable label, e.g. `migration-r2-2026-11-20`. */
  label: string;
  /** Caller-supplied ISO instant. Recorded, never defaulted from the clock. */
  created_at: string;
  /** Why this plan was produced; carried into the artifact verbatim. */
  purpose: string;
}

export interface PlanOptions {
  archiveDir: string;
  context: PlanContext;
  limits: PlanLimits;
  /**
   * Test-only injection point: runs immediately after the archive verifier returns
   * and before any byte is re-read for the plan.
   *
   * It exists to prove the *planner's* binding guard is what refuses a replaced
   * archive — the verifier has already passed at this point, so a test that
   * mutated earlier would only be asserting the verifier.
   */
  hooks?: { afterVerify?: (() => void) | undefined };
}

export type Family = "term" | "assertion" | "proposal" | "checkpoint" | "frontier_entry" | "attachment";

export interface SourceSlice {
  /** Byte offset inside the source file blob. */
  offset: number;
  /** Exact byte length of the slice. */
  length: number;
  /** SHA-256 of exactly those bytes. */
  sha256: string;
}

export interface Unresolved {
  family: Family;
  subject: string;
  field: string;
  reason: string;
  /**
   * The explicit strategy this plan proposes for a field the source cannot supply.
   * Present only where a decision is proposed; absent where none is possible yet.
   */
  proposed_strategy?: string;
}

export interface PlanError_ {
  source: string;
  index: number | null;
  code: string;
  message: string;
}

export interface Attachment {
  path: string;
  bytes: number;
  sha256: string;
  blob: string;
}

export interface TermMapping {
  kind: "term";
  source_id: string;
  source_path: string;
  target_id: number;
  slug: string;
  atom: string;
  layer: number;
  domain: string[];
  confidence: string;
  source: string;
  triggers: string[];
  /** Exact body bytes after the frontmatter terminator. */
  body: SourceSlice;
  /** The whole source file, for record-level provenance. */
  file: { bytes: number; sha256: string; blob: string };
  /** Service projection (trimmed/normalised). Never used as the byte authority. */
  projection: { body_sha256: string; trimmed: true };
  /** Target fields the source cannot supply. */
  missing_target_fields: string[];
}

export interface AssertionMapping {
  kind: "assertion";
  source_id: string;
  source_path: string;
  target_id: number;
  from_slug: string;
  from_target_id: number | null;
  relation: string;
  target: string;
  target_target_id: number | null;
  confidence: string;
  declaration_order: number;
  /** Target audit fields, typed exactly as the target schema does. */
  /**
   * Proposed target audit fields.
   *
   * `recorded_by` is the *authenticated import executor*, which does not exist
   * until a real importer runs; it is never the source's declared author.
   * `actor_origin` is left `Unresolved` because no verified execution context is
   * available yet: claiming `LegacyDeclared` merely because a record was migrated
   * was the R2 semantic error. The origin the SOURCE supports is reported
   * separately as `declared_actor_origin`.
   */
  audit: {
    recorded_by: null;
    actor_origin: "Unresolved";
    reason: null;
    created_at: "unavailable";
  };
  /** Declared author/actor as written in the source, if any; never the executor. */
  declared_author: string | null;
  /**
   * The origin the source itself supports: `LegacyDeclared` only when a declared
   * actor is actually present, otherwise `Unavailable`.
   */
  declared_actor_origin: "LegacyDeclared" | "Unavailable";
}

export interface ProposalMapping {
  kind: "proposal";
  source_id: string;
  source_path: string;
  target_id: number;
  status: string;
  /*
   * There is deliberately NO `template` flag on an emitted proposal. A file whose
   * status is `example` is documentation: it is listed under `attachments` and
   * `unavailable` and never becomes a proposal record, so no emitted proposal can
   * be a template and a constant `false` was a field a reader could only misread.
   */
  concept: string | null;
  /** The complete parsed head, all keys, verbatim. */
  metadata: Record<string, unknown>;
  payload: { bytes: number; sha256: string; blob: string };
  /**
   * Proposed target audit fields.
   *
   * `recorded_by` is the *authenticated import executor*, which does not exist
   * until a real importer runs; it is never the source's declared author.
   * `actor_origin` is left `Unresolved` because no verified execution context is
   * available yet: claiming `LegacyDeclared` merely because a record was migrated
   * was the R2 semantic error. The origin the SOURCE supports is reported
   * separately as `declared_actor_origin`.
   */
  audit: {
    recorded_by: null;
    actor_origin: "Unresolved";
    reason: null;
    created_at: "unavailable";
  };
  declared_author: string | null;
  /** The origin the source supports; see `AssertionMapping.declared_actor_origin`. */
  declared_actor_origin: "LegacyDeclared" | "Unavailable";
  /** Baseline in the source, when the source states one. */
  expected_revision: string | null;
}

export interface CheckpointMapping {
  kind: "checkpoint";
  source_id: string;
  source_path: string;
  target_id: number;
  concept: string;
  session: number;
  content: SourceSlice & { blob: string };
  metadata: Record<string, unknown>;
}

export interface FrontierEntryMapping {
  kind: "frontier_entry";
  /** `section/table/row`, unique even when a heading repeats. */
  source_id: string;
  source_path: string;
  section: string;
  table: number;
  row: number;
  /** Position across the whole document, author order. */
  declaration_order: number;
  target_id: number;
  cells: string[];
  raw: string;
}

export interface MigrationPlan {
  plan_version: typeof PLAN_VERSION;
  tool: { name: string; version: number };
  /** Run metadata: outside the canonical mapping body, digest-bound. */
  run: {
    label: string;
    created_at: string;
    purpose: string;
    context_digest: string;
  };
  source: {
    archive_dir: string;
    archive_content_digest: string;
    source_digest: string;
    authority: string;
    kb_id: string;
    files: number;
    bytes: number;
    extraction: "complete" | "incomplete";
  };
  limits: PlanLimits;
  counts: Record<Family, number> & { source_proposal_files: number };
  terms: TermMapping[];
  assertions: AssertionMapping[];
  proposals: ProposalMapping[];
  checkpoints: CheckpointMapping[];
  frontier: FrontierEntryMapping[];
  attachments: Attachment[];
  unavailable: Array<{ family: Family; subject: string; field: string; reason: string }>;
  unresolved: Unresolved[];
  errors: PlanError_[];
  status: "complete" | "incomplete";
  /** Whether the mapping is ready for an importer. Always false until reviewed. */
  target_mapping: "not_ready";
}

// ---------------------------------------------------------------------------
// Bounds and bounded IO
// ---------------------------------------------------------------------------

function assertLimits(limits: PlanLimits): void {
  for (const name of [
    "maxEntries",
    "maxBytes",
    "maxRecords",
    "maxArchiveBytes",
    "maxLineBytes",
    "maxDiagnostics",
  ] as const) {
    const value = limits[name];
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) {
      throw new PlanError("E_USAGE", `${name} must be a positive finite integer`);
    }
  }
}

/** Refuse before growing. Every count and every byte goes through one of these. */
class Budget {
  private used = 0;
  constructor(private readonly limit: number, private readonly what: string) {}
  spend(amount: number): void {
    if (this.used + amount > this.limit) {
      throw new PlanError("E_BUDGET", `${this.what} would exceed its ceiling: ${this.used}+${amount} > ${this.limit}`);
    }
    this.used += amount;
  }
  get total(): number {
    return this.used;
  }
}

function sha256Hex(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

/**
 * Read exactly `expected` bytes from a fresh descriptor, refusing growth.
 *
 * `stat(path).size` then `readFileSync` is not a bound: the file can grow in
 * between, and the read allocates whatever it finds. This opens, `fstat`s the
 * descriptor it will read (so a path swap cannot redirect it), reads exactly the
 * expected length, then probes one byte past the end.
 */
function readBoundedExact(abs: string, expected: number, ceiling: number, label: string): Buffer {
  if (expected > ceiling) {
    throw new PlanError("E_BUDGET", `${label} is ${expected} bytes, over the ${ceiling}-byte ceiling`);
  }
  let fd: number;
  try {
    fd = openSync(abs, "r");
  } catch (err) {
    throw new PlanError("E_ARCHIVE", `cannot open ${label}: ${(err as Error).message}`);
  }
  try {
    const stat = fstatSync(fd);
    if (!stat.isFile()) {
      throw new PlanError("E_ARCHIVE", `${label} is not a regular file`);
    }
    if (stat.size !== expected) {
      throw new PlanError("E_ARCHIVE", `${label} is ${stat.size} bytes, expected ${expected}`);
    }
    const buffer = Buffer.allocUnsafe(expected);
    let offset = 0;
    while (offset < expected) {
      const read = readSync(fd, buffer, offset, expected - offset, offset);
      if (read === 0) break;
      offset += read;
    }
    if (offset !== expected) {
      throw new PlanError("E_ARCHIVE", `${label} shrank while being read (${offset} of ${expected})`);
    }
    const probe = Buffer.allocUnsafe(1);
    if (readSync(fd, probe, 0, 1, expected) !== 0) {
      throw new PlanError("E_ARCHIVE", `${label} grew while being read; refusing an unbounded read`);
    }
    return buffer;
  } finally {
    closeSync(fd);
  }
}

/**
 * Read a file whose length the caller does not know, under an absolute ceiling.
 *
 * **One descriptor for both the size check and the read.** An earlier version
 * `fstat`ed one descriptor and then reopened the path inside `readBoundedExact`,
 * which still bounded the size but did not read the bytes it had measured. Here
 * the size comes from the descriptor that is then read to exactly that length, so
 * a path swapped between the two steps cannot produce a different file.
 */
function readBoundedUnknown(abs: string, ceiling: number, label: string): Buffer {
  let fd: number;
  try {
    fd = openSync(abs, "r");
  } catch (err) {
    throw new PlanError("E_ARCHIVE", `cannot open ${label}: ${(err as Error).message}`);
  }
  try {
    const stat = fstatSync(fd);
    if (!stat.isFile()) throw new PlanError("E_ARCHIVE", `${label} is not a regular file`);
    if (stat.size > ceiling) {
      throw new PlanError("E_BUDGET", `${label} is ${stat.size} bytes, over the ${ceiling}-byte ceiling`);
    }
    const expected = stat.size;
    const buffer = Buffer.allocUnsafe(expected);
    let offset = 0;
    while (offset < expected) {
      const read = readSync(fd, buffer, offset, expected - offset, offset);
      if (read === 0) break;
      offset += read;
    }
    if (offset !== expected) {
      throw new PlanError("E_ARCHIVE", `${label} shrank while being read (${offset} of ${expected})`);
    }
    const probe = Buffer.allocUnsafe(1);
    if (readSync(fd, probe, 0, 1, expected) !== 0) {
      throw new PlanError("E_ARCHIVE", `${label} grew while being read; refusing an unbounded read`);
    }
    return buffer;
  } finally {
    closeSync(fd);
  }
}

/**
 * Canonical blob path.
 *
 * A blob is addressable only as `blobs/<64 hex>`, and the digest must match the
 * record's `sha256`. A record naming `../x`, an absolute path or `blobs/other`
 * is refused *before* any open, so a hostile archive cannot make the planner read
 * outside the archive.
 */
function canonicalBlobPath(archiveDir: string, sha256: string): string {
  if (!/^[0-9a-f]{64}$/.test(sha256)) {
    throw new PlanError("E_ARCHIVE", `blob digest ${JSON.stringify(sha256)} is not 64 lowercase hex`);
  }
  return resolve(archiveDir, "blobs", sha256);
}

// ---------------------------------------------------------------------------
// Archive reading
// ---------------------------------------------------------------------------

interface ArchiveFileRecord {
  path: string;
  bytes: number;
  sha256: string;
  blob: string;
}

/**
 * Stream `files.ndjson` line by line under caller ceilings.
 *
 * Growth of the partial line is checked **before** each append, so one hostile
 * multi-gigabyte line fails on growth instead of being buffered. Entries and
 * lines are counted before any record is admitted.
 */
function readFileRecords(archiveDir: string, limits: PlanLimits): { records: ArchiveFileRecord[]; bytes: Buffer } {
  const abs = resolve(archiveDir, "files.ndjson");
  if (!existsSync(abs)) throw new PlanError("E_ARCHIVE", `${abs} is missing`);
  // NOTE: this buffers the whole stream under `maxArchiveBytes` and then scans it.
  // It is bounded, but it is not a streaming line reader; see the module comment.
  const whole = readBoundedUnknown(abs, limits.maxArchiveBytes, "files.ndjson");
  const records: ArchiveFileRecord[] = [];
  let lineIndex = 0;
  let start = 0;
  for (let i = 0; i <= whole.length; i += 1) {
    const atEnd = i === whole.length;
    if (!atEnd && whole[i] !== 0x0a) {
      // Bound the *partial* line as it grows, not after the newline is found.
      if (i - start + 1 > limits.maxLineBytes) {
        throw new PlanError("E_BUDGET", `record ${lineIndex} exceeds maxLineBytes=${limits.maxLineBytes}`);
      }
      continue;
    }
    const lineBytes = whole.subarray(start, i);
    if (lineBytes.length > 0) {
      if (records.length + 1 > limits.maxEntries) {
        throw new PlanError("E_BUDGET", `archive has more than maxEntries=${limits.maxEntries} files`);
      }
      let parsed: Record<string, unknown>;
      try {
        parsed = JSON.parse(lineBytes.toString("utf8")) as Record<string, unknown>;
      } catch (err) {
        throw new PlanError("E_ARCHIVE", `record ${lineIndex} is not JSON: ${(err as Error).message}`);
      }
      if (parsed["record_type"] !== "source_file") {
        throw new PlanError("E_ARCHIVE", `record ${lineIndex} is not a source_file record`);
      }
      for (const field of ["path", "sha256", "blob"] as const) {
        if (typeof parsed[field] !== "string") {
          throw new PlanError("E_ARCHIVE", `record ${lineIndex} has no string ${field}`);
        }
      }
      const bytes = parsed["bytes"];
      if (typeof bytes !== "number" || !Number.isSafeInteger(bytes) || bytes < 0) {
        throw new PlanError("E_ARCHIVE", `record ${lineIndex} has an invalid byte length`);
      }
      const sha256 = parsed["sha256"] as string;
      const blob = parsed["blob"] as string;
      // The recorded blob path must be the canonical one for its own digest.
      if (blob !== `blobs/${sha256}`) {
        throw new PlanError(
          "E_ARCHIVE",
          `record ${lineIndex} names blob ${JSON.stringify(blob)}, not the canonical blobs/${sha256}`,
        );
      }
      records.push({ path: parsed["path"] as string, bytes, sha256, blob });
      lineIndex += 1;
    }
    start = i + 1;
  }
  return { records, bytes: whole };
}

/** Read and re-hash one blob, refusing any mismatch or cost overrun. */
function readBlob(archiveDir: string, record: ArchiveFileRecord, budget: Budget, ceiling: number): Buffer {
  budget.spend(record.bytes);
  const abs = canonicalBlobPath(archiveDir, record.sha256);
  if (!existsSync(abs)) {
    throw new PlanError("E_ARCHIVE", `blob for ${record.path} is missing at ${abs}`);
  }
  const bytes = readBoundedExact(abs, record.bytes, ceiling, `blob ${record.path}`);
  if (sha256Hex(bytes) !== record.sha256) {
    throw new PlanError("E_ARCHIVE", `blob ${record.path} does not match its recorded hash`);
  }
  return bytes;
}

// ---------------------------------------------------------------------------
// UTF-8 ordering
// ---------------------------------------------------------------------------

/**
 * Compare two ids by **UTF-8 bytes**.
 *
 * JS `<` compares UTF-16 code units, which orders astral characters before some
 * BMP characters — so a future non-ASCII heading could sort differently from its
 * encoded form. Ids are byte strings here.
 */
export function compareUtf8(a: string, b: string): number {
  return Buffer.compare(Buffer.from(a, "utf8"), Buffer.from(b, "utf8"));
}

// ---------------------------------------------------------------------------
// Body slice: exact source bytes
// ---------------------------------------------------------------------------

/**
 * Locate the exact body bytes after the frontmatter terminator.
 *
 * Returns the byte range inside `raw`. Nothing is trimmed: leading blank lines,
 * trailing newlines, CRLF line endings and multi-byte characters all stay part of
 * the slice, because the target body blob must be reproducible from the source.
 */
export function bodySlice(raw: Buffer): { offset: number; length: number } {
  // Frontmatter opens at byte 0 with `---` followed by a line break.
  const open = raw.subarray(0, 3).toString("latin1");
  if (open !== "---") {
    throw new PlanError("E_PARSE", "concept file does not open with a frontmatter delimiter");
  }
  const firstBreak = raw.indexOf(0x0a, 3);
  if (firstBreak === -1) throw new PlanError("E_PARSE", "frontmatter has no line break");

  // The closing delimiter is a line whose content is exactly `---`.
  let lineStart = firstBreak + 1;
  for (;;) {
    if (lineStart > raw.length) throw new PlanError("E_PARSE", "frontmatter is unterminated");
    const lineEnd = raw.indexOf(0x0a, lineStart);
    const end = lineEnd === -1 ? raw.length : lineEnd;
    const line = raw.subarray(lineStart, end);
    const text = line.toString("latin1").replace(/\r$/, "");
    if (text === "---") {
      const offset = lineEnd === -1 ? raw.length : lineEnd + 1;
      return { offset, length: raw.length - offset };
    }
    if (lineEnd === -1) throw new PlanError("E_PARSE", "frontmatter is unterminated");
    lineStart = lineEnd + 1;
  }
}

// ---------------------------------------------------------------------------
// Frontier: every section, every table, unique ids
// ---------------------------------------------------------------------------

/**
 * Parse **all** tables of a frontier document.
 *
 * Ids are `section/table/row`: the table counter is per section, the row counter
 * is per table, so two tables in one section — or a heading that repeats — cannot
 * collide the way a single `section#position` counter does.
 */
/**
 * Parse every table of a frontier document.
 *
 * `limit` bounds admitted rows; `diagnosticsLimit` bounds this function's own
 * error array. The caller re-feeds those errors through its own budgeted counter,
 * but without a limit here a pathological frontier could build up to `limit`
 * entries in memory before any of them were charged — the one place where
 * "spend before push" was satisfied only at the second hop.
 */
export function parseFrontierSections(
  raw: string,
  limit: number,
  diagnosticsLimit: number,
): { entries: FrontierEntryMapping[]; errors: PlanError_[] } {
  const entries: FrontierEntryMapping[] = [];
  const errors: PlanError_[] = [];
  const seen = new Set<string>();
  let section = "(preamble)";
  let tableIndex = 0;
  /** Rows admitted since the current table's separator; -1 = no body rows yet. */
  let header: string[] | null = null;
  /** True once a separator has been seen for this table, i.e. the header is complete. */
  let separatorSeen = false;
  /** Tables already started in this section; the next header takes this index. */
  let tablesInSection = 0;
  /** Headings seen with this exact title; a repeat gets a distinct scope. */
  const headingCounts = new Map<string, number>();
  let row = 0;
  let declaration = 0;

  for (const line of raw.split("\n")) {
    if (line.startsWith("## ")) {
      {
        const title = line.slice(3).trim();
        const seen = (headingCounts.get(title) ?? 0) + 1;
        headingCounts.set(title, seen);
        // The scope distinguishes repeats, so ids stay unique even when an author
        // reuses a heading verbatim.
        section = seen === 1 ? title : `${title}#${seen}`;
      }
      tableIndex = 0;
      tablesInSection = 0;
      header = null;
      separatorSeen = false;
      row = 0;
      continue;
    }
    if (!line.startsWith("|")) {
      header = null;
      separatorSeen = false;
      continue;
    }
    const cells = line.split("|").slice(1, -1).map((c) => c.trim());
    if (cells.length === 0) continue;
    const isSeparator = cells.every((c) => /^-{2,}$/.test(c));
    if (isSeparator) {
      // A separator opens the body of the table whose header just preceded it.
      // Only the first separator of a table consumes a table index, so a second
      // table in the same section gets its own id space.
      if (!separatorSeen && header !== null) {
        separatorSeen = true;
        row = 0;
      }
      continue;
    }
    if (separatorSeen) {
      // Body row of the current table.
    } else {
      // Any pipe row before a separator begins a table. Every table in a section
      // takes the next index, so two tables can never share an id space.
      header = cells;
      separatorSeen = false;
      tableIndex = tablesInSection;
      tablesInSection += 1;
      row = 0;
      continue;
    }
    if (entries.length + 1 > limit) {
      // Charged before the push: maxRecords bounds frontier growth too.
      throw new PlanError("E_BUDGET", `frontier rows would exceed maxRecords=${limit}`);
    }
    const sourceId = `${section}/${tableIndex}/${row}`;
    if (seen.has(sourceId)) {
      if (errors.length >= diagnosticsLimit) {
        // Refuse rather than accumulate past the caller's diagnostic ceiling.
        throw new PlanError(
          "E_BUDGET",
          `frontier diagnostics would exceed maxDiagnostics=${diagnosticsLimit}`,
        );
      }
      errors.push({
        source: "frontier.md",
        index: declaration,
        code: "DUPLICATE_FRONTIER_ID",
        message: `frontier row id ${sourceId} appears twice`,
      });
    }
    seen.add(sourceId);
    entries.push({
      kind: "frontier_entry",
      source_id: sourceId,
      source_path: "frontier.md",
      section,
      table: tableIndex,
      row,
      declaration_order: declaration,
      target_id: declaration + 1,
      cells,
      raw: line,
    });
    declaration += 1;
    row += 1;
  }
  return { entries, errors };
}

// ---------------------------------------------------------------------------
// Proposals
// ---------------------------------------------------------------------------

/**
 * Parse a proposal head with the same mature YAML parser `packages/core` uses.
 *
 * `gray-matter` lives in `packages/core/node_modules`; resolving it through that
 * package's own `package.json` keeps this tool dependent on the real parser rather
 * than on a hand-written scalar scan that mangles quoted or duplicated keys.
 */
function yamlParser(): { parse: (raw: string) => { data: Record<string, unknown>; content: string } } {
  const require = createRequire(resolve(import.meta.dir, "..", "packages", "core", "package.json"));
  const matter = require("gray-matter") as {
    (raw: string): { data: Record<string, unknown>; content: string };
  };
  return { parse: (raw: string) => matter(raw) };
}

// ---------------------------------------------------------------------------
// Plan generation
// ---------------------------------------------------------------------------

const LAYER_DIRS = ["L0-axioms", "L1-core", "L2-support", "L3-instances"] as const;
/** Static status vocabulary: a literal table, not a runtime set. */
const KNOWN_PROPOSAL_STATUS: Record<string, true> = {
  approved: true,
  pending: true,
  rejected: true,
  conflicted: true,
  example: true,
};

export function generatePlan(options: PlanOptions): MigrationPlan {
  assertLimits(options.limits);
  const limits = options.limits;
  const archiveDir = resolve(options.archiveDir);
  const context = options.context;

  if (typeof context.label !== "string" || context.label.trim() === "") {
    throw new PlanError("E_USAGE", "plan context label must be non-empty");
  }
  if (typeof context.created_at !== "string" || Number.isNaN(Date.parse(context.created_at))) {
    throw new PlanError("E_USAGE", "plan context created_at must be a timestamp; it is never defaulted from the clock");
  }
  if (typeof context.purpose !== "string" || context.purpose.trim() === "") {
    throw new PlanError("E_USAGE", "plan context purpose must be non-empty");
  }
  const contextDigest = `sha256:${sha256Hex(Buffer.from(JSON.stringify({
    label: context.label,
    created_at: context.created_at,
    purpose: context.purpose,
  }), "utf8"))}`;

  // 1. The archive must verify under the caller's ceilings before any blob read.
  // TWO DIFFERENT COUNTS share the flag name, and this call site is where they meet:
  //   * the verifier's `maxRecords` counts STREAM RECORDS (lines of files.ndjson /
  //     rows.ndjson) — a property of the capture;
  //   * the planner's `recordBudget` counts MAPPING RECORDS (attachments + terms +
  //     assertions + proposals + checkpoints + frontier rows) — a property of the plan.
  // One caller flag feeds both so an operator states one budget, but the numbers are
  // not comparable and each refusal names which unit it hit.
  const streamRecordCeiling = limits.maxRecords;
  const verified = verifySourceArchive(archiveDir, {
    maxArchiveBytes: limits.maxArchiveBytes,
    maxRecords: streamRecordCeiling,
    maxLineBytes: limits.maxLineBytes,
    maxEntries: limits.maxEntries,
  });
  options.hooks?.afterVerify?.();

  // 2. Manifest. The bytes read here are NOT trusted because verification passed
  //    earlier: they are re-bound to it. The digest of the bytes in hand is
  //    recomputed with the archive tool's own public helper and compared both to
  //    the manifest's declared value and to the verified content digest, so a
  //    same-size replacement between verify and read is refused rather than
  //    producing a plan that mixes two different archives.
  // `maxLineBytes` is the verifier's per-LINE ceiling for NDJSON streams; it has
  // nothing to say about how large `archive.json` may be. `archive.json` is a
  // single pretty-printed document, so a caller can legitimately set a line bound
  // far below its size while the archive is perfectly admissible. Reusing one
  // caller limit under the other's name made the planner refuse archives the
  // verifier accepts; the manifest is bounded by `maxArchiveBytes` alone.
  const manifestBytes = readBoundedUnknown(
    resolve(archiveDir, "archive.json"),
    limits.maxArchiveBytes,
    "archive.json",
  );
  let manifest: ArchiveManifest;
  try {
    manifest = JSON.parse(manifestBytes.toString("utf8")) as ArchiveManifest;
  } catch (err) {
    throw new PlanError("E_ARCHIVE", `archive.json is not JSON: ${(err as Error).message}`);
  }
  const actualManifestDigest = manifestContentDigest(manifest);
  if (manifest.content_digest !== actualManifestDigest) {
    throw new PlanError(
      "E_ARCHIVE_CHANGED",
      `archive.json declares ${manifest.content_digest} but the bytes in hand digest to ${actualManifestDigest}`,
    );
  }
  if (actualManifestDigest !== verified.contentDigest) {
    throw new PlanError(
      "E_ARCHIVE_CHANGED",
      `archive.json changed after verification: verified ${verified.contentDigest}, read ${actualManifestDigest}`,
    );
  }

  const filesStream = readFileRecords(archiveDir, limits);
  const records = filesStream.records;

  // The bytes read for `files.ndjson` must be exactly the stream the manifest (and
  // therefore the earlier verification) described. Hash, length and record count
  // are all checked against the manifest in hand, so a same-length replacement
  // cannot smuggle different records past verification.
  const declaredFilesStream = manifest.streams.find((s) => s.path === "files.ndjson");
  if (declaredFilesStream === undefined) {
    throw new PlanError("E_ARCHIVE", "manifest declares no files.ndjson stream");
  }
  const actualStreamDigest = `sha256:${sha256Hex(filesStream.bytes)}`;
  if (actualStreamDigest !== declaredFilesStream.sha256) {
    throw new PlanError(
      "E_ARCHIVE_CHANGED",
      `files.ndjson digest ${actualStreamDigest} != declared ${declaredFilesStream.sha256}`,
    );
  }
  if (filesStream.bytes.length !== declaredFilesStream.bytes) {
    throw new PlanError(
      "E_ARCHIVE_CHANGED",
      `files.ndjson is ${filesStream.bytes.length} bytes, manifest says ${declaredFilesStream.bytes}`,
    );
  }
  if (records.length !== declaredFilesStream.records) {
    throw new PlanError(
      "E_ARCHIVE_CHANGED",
      `files.ndjson holds ${records.length} records, manifest says ${declaredFilesStream.records}`,
    );
  }
  const errors: PlanError_[] = [];
  const unresolved: Unresolved[] = [];
  const unavailable: MigrationPlan["unavailable"] = [];
  // One counter over all three auxiliary collections: refusing early keeps a
  // pathological source from accumulating diagnostics without bound.
  const diagnostics = new Budget(limits.maxDiagnostics, "diagnostics");
  const noteError = (entry: PlanError_): void => {
    diagnostics.spend(1);
    errors.push(entry);
  };
  const noteUnresolved = (entry: Unresolved): void => {
    diagnostics.spend(1);
    unresolved.push(entry);
  };
  const noteUnavailable = (entry: MigrationPlan["unavailable"][number]): void => {
    diagnostics.spend(1);
    unavailable.push(entry);
  };

  const recordBudget = new Budget(limits.maxRecords, "plan records");
  const byteBudget = new Budget(limits.maxBytes, "source bytes");

  const byPath = new Map<string, ArchiveFileRecord>();
  for (const record of records) {
    if (byPath.has(record.path)) {
      noteError({ source: record.path, index: null, code: "DUPLICATE_PATH", message: "path appears twice" });
      continue;
    }
    byPath.set(record.path, record);
  }

  // --- attachments: mapping records under the SAME global cap --------------
  // Each attachment is one mapping record in this plan, so it charges the same
  // budget as a term or an assertion. Checking only `attachments.length` against
  // maxRecords let 73 attachments plus 262 other records emit 335 under a cap of
  // 300 while every per-family check passed.
  const attachments: Attachment[] = [];
  for (const record of records) {
    recordBudget.spend(1);
    attachments.push({ path: record.path, bytes: record.bytes, sha256: record.sha256, blob: record.blob });
  }

  // --- terms ---------------------------------------------------------------
  const conceptRecords: ArchiveFileRecord[] = [];
  for (const layer of LAYER_DIRS) {
    const inLayer = records
      .filter((r) => r.path.startsWith(`concepts/${layer}/`) && r.path.endsWith(".md"))
      .sort((a, b) => compareUtf8(a.path, b.path));
    conceptRecords.push(...inLayer);
  }
  for (const stray of records.filter((r) => r.path.startsWith("concepts/") && !conceptRecords.includes(r))) {
    noteError({
      source: stray.path,
      index: null,
      code: "UNKNOWN_CONCEPT_LAYER",
      message: "concept file is outside the four registered layer directories",
    });
  }

  const terms: TermMapping[] = [];
  const slugToTarget = new Map<string, number>();
  const conceptParsed = new Map<string, ConceptFile>();

  conceptRecords.forEach((record, offset) => {
    const raw = readBlob(archiveDir, record, byteBudget, limits.maxBytes);
    let parsed: ConceptFile;
    try {
      parsed = parseConceptFile(raw.toString("utf8"), record.path);
    } catch (err) {
      noteError({ source: record.path, index: null, code: "INVALID_CONCEPT", message: (err as Error).message });
      return;
    }
    const slug = parsed.frontmatter.id;
    const fileSlug = record.path.split("/").pop()!.replace(/\.md$/, "");
    if (slug !== fileSlug) {
      noteError({
        source: record.path,
        index: null,
        code: "SLUG_PATH_MISMATCH",
        message: `frontmatter id ${slug} does not match file name ${fileSlug}`,
      });
    }
    if (slugToTarget.has(slug)) {
      noteError({
        source: record.path,
        index: null,
        code: "DUPLICATE_ID",
        message: `concept slug ${slug} appears more than once`,
      });
      return;
    }

    // Exact body bytes from the source file, not the trimmed projection.
    let slice: { offset: number; length: number };
    try {
      slice = bodySlice(raw);
    } catch (err) {
      noteError({ source: record.path, index: null, code: "INVALID_CONCEPT", message: (err as Error).message });
      return;
    }
    const bodyBytes = raw.subarray(slice.offset, slice.offset + slice.length);
    recordBudget.spend(1);

    const targetId = offset + 1;
    slugToTarget.set(slug, targetId);
    conceptParsed.set(slug, parsed);

    terms.push({
      kind: "term",
      source_id: slug,
      source_path: record.path,
      target_id: targetId,
      slug,
      atom: parsed.frontmatter.atom,
      layer: parsed.frontmatter.layer,
      domain: [...parsed.frontmatter.domain],
      confidence: parsed.frontmatter.confidence,
      source: parsed.frontmatter.source,
      triggers: [...parsed.frontmatter.triggers],
      body: { offset: slice.offset, length: slice.length, sha256: sha256Hex(bodyBytes) },
      file: { bytes: record.bytes, sha256: record.sha256, blob: record.blob },
      projection: { body_sha256: sha256Hex(Buffer.from(parsed.molecule, "utf8")), trimmed: true },
      missing_target_fields: ["created_at", "revision", "recorded_by", "actor_origin"],
    });

    noteUnavailable({
      family: "term",
      subject: slug,
      field: "created_at/updated_at/revision",
      reason: "source carries no timestamps or revision counter for concept records",
    });
    noteUnresolved({
      family: "term",
      subject: slug,
      field: "term.created_at",
      reason:
        "required target field; the source has no creation timestamp and none may be invented (no mtime, no zero, no back-dated value)",
      proposed_strategy:
        "created_at is the NEW import event's commit clock, supplied by the importer at commit time; the source's original creation time stays unavailable and is preserved as such in its own field",
    });
    noteUnresolved({
      family: "term",
      subject: slug,
      field: "term.revision",
      reason: "required target field; the source has no revision counter",
      proposed_strategy:
        "revision is the FIRST revision in the NEW target space (1), which records the import as the first edit there; it does not reconstruct any source-side revision history",
    });
    noteUnresolved({
      family: "term",
      subject: slug,
      field: "term.recorded_by/actor_origin",
      reason: "no authenticated principal exists in the source",
      proposed_strategy:
        "recorded_by is the authenticated import executor and stays unresolved until a real importer provides a verified context; the source's declared author (if any) is kept separately and its origin is LegacyDeclared only when such a value exists, otherwise Unavailable",
    });
  });

  // --- assertions: every authored edge, declaration order preserved --------
  const assertions: AssertionMapping[] = [];
  let assertionId = 0;
  for (const term of terms) {
    const parsed = conceptParsed.get(term.slug)!;
    parsed.frontmatter.edges.forEach((edge, order) => {
      assertionId += 1;
      recordBudget.spend(1);
      const crossKb = edge.target.includes("::");
      if (crossKb) {
        noteError({
          source: term.source_path,
          index: order,
          code: "CROSS_KB_EDGE",
          message: `edge target ${edge.target} names another knowledge base; the structured store cannot bind it`,
        });
      }
      const targetId = slugToTarget.get(edge.target) ?? null;
      if (!crossKb && targetId === null) {
        noteError({
          source: term.source_path,
          index: order,
          code: "UNKNOWN_EDGE_TARGET",
          message: `edge target ${edge.target} is not a concept in this source`,
        });
        noteUnresolved({
          family: "assertion",
          subject: `${term.slug}:${order}`,
          field: "assertion.target",
          reason: `target concept ${edge.target} is absent from the source capture`,
        });
      }
      assertions.push({
        kind: "assertion",
        source_id: `${term.slug}:${order}`,
        source_path: term.source_path,
        target_id: assertionId,
        from_slug: term.slug,
        from_target_id: term.target_id,
        relation: edge.type,
        target: edge.target,
        target_target_id: crossKb ? null : targetId,
        confidence: edge.confidence,
        declaration_order: order,
        audit: { recorded_by: null, actor_origin: "Unresolved", reason: null, created_at: "unavailable" },
        declared_author: null,
        // A Markdown edge declares no actor at all, so the honest origin is
        // Unavailable — not LegacyDeclared merely because it is being migrated.
        declared_actor_origin: "Unavailable",
      });
    });
  }
  if (assertions.length !== assertionId) {
    throw new PlanError("E_INTERNAL", "assertion numbering diverged from its count");
  }
  if (assertions.length > 0) {
    noteUnresolved({
      family: "assertion",
      subject: "(all)",
      field: "assertion.created_at",
      reason: "Markdown edges carry no timestamp",
      proposed_strategy:
        "created_at is the NEW import event's commit clock supplied by the importer; the original declaration time remains unavailable",
    });
  }

  // --- proposals -----------------------------------------------------------
  const proposalRecords = records
    .filter((r) => r.path.startsWith("proposals/"))
    .sort((a, b) => compareUtf8(a.path, b.path));
  const proposals: ProposalMapping[] = [];
  let proposalId = 0;
  const yaml = yamlParser();

  for (const record of proposalRecords) {
    const raw = readBlob(archiveDir, record, byteBudget, limits.maxBytes);
    // NOTE: the record budget is charged only for files that become proposal
    // RECORDS. An `example` is documentation: it is retained as a file and never
    // takes an id, so charging it here would make the global cap count records the
    // plan does not emit — and would make "maxRecords == exact emitted total"
    // unreachable by exactly the number of template files.
    let metadata: Record<string, unknown>;
    try {
      metadata = record.path.endsWith(".json")
        ? (JSON.parse(raw.toString("utf8")) as Record<string, unknown>)
        : yaml.parse(raw.toString("utf8")).data;
    } catch (err) {
      noteError({
        source: record.path,
        index: null,
        code: "INVALID_PROPOSAL",
        message: `head is unreadable: ${(err as Error).message}`,
      });
      continue;
    }

    const statusValue = metadata["status"];
    const status = typeof statusValue === "string" ? statusValue : "unknown";
    const conceptValue = metadata["concept"] ?? metadata["conceptId"];
    const concept = typeof conceptValue === "string" ? conceptValue : null;
    const proposer = metadata["proposer"];
    const declaredAuthor = typeof proposer === "string" ? proposer : null;
    const expectedRevisionValue = metadata["expectedRevision"] ?? metadata["expected_revision"];
    const expectedRevision = typeof expectedRevisionValue === "string" ? expectedRevisionValue : null;

    if (KNOWN_PROPOSAL_STATUS[status] !== true) {
      noteError({
        source: record.path,
        index: null,
        code: "UNKNOWN_PROPOSAL_STATUS",
        message: `status ${JSON.stringify(status)} is not one of approved/pending/rejected/conflicted/example`,
      });
      noteUnresolved({
        family: "proposal",
        subject: record.path,
        field: "proposal.status",
        reason: `unrecognised status ${JSON.stringify(status)} cannot be mapped to a target state`,
      });
    }
    // Examples are documentation: retained as files, never allocated a record id, and
    // their concept binding is illustrative. Checking it as a real reference would turn
    // a documentation file into a migration error.
    if (status === "example") {
      noteUnavailable({
        family: "proposal",
        subject: record.path,
        field: "proposal record",
        reason: "file is a documented example (status: example); preserved as an attachment, not imported as a reviewable proposal",
      });
      continue;
    }

    if (concept !== null && !slugToTarget.has(concept)) {
      noteError({
        source: record.path,
        index: null,
        code: "FOREIGN_PROPOSAL_CONCEPT",
        message: `proposal names concept ${concept}, which is not in this source`,
      });
      noteUnresolved({
        family: "proposal",
        subject: record.path,
        field: "proposal.subject",
        reason: `concept ${concept} is absent from the source capture`,
      });
    }

    recordBudget.spend(1);
    proposalId += 1;
    proposals.push({
      kind: "proposal",
      source_id: record.path,
      source_path: record.path,
      target_id: proposalId,
      status,
      concept,
      metadata,
      payload: { bytes: raw.length, sha256: sha256Hex(raw), blob: record.blob },
      audit: { recorded_by: null, actor_origin: "Unresolved", reason: null, created_at: "unavailable" },
      declared_author: declaredAuthor,
      declared_actor_origin: declaredAuthor === null ? "Unavailable" : "LegacyDeclared",
      expected_revision: expectedRevision,
    });

    if (status === "approved") {
      noteUnavailable({
        family: "proposal",
        subject: record.path,
        field: "reviewed_at/reviewer",
        reason: "approval exists only as a file status; no reviewer identity or review timestamp",
      });
      // An already-approved proposal is audit history, never re-applied.
      noteUnavailable({
        family: "proposal",
        subject: record.path,
        field: "proposal re-application",
        reason: "approved proposal is imported as history only; its change is not applied again at import",
      });
      noteUnresolved({
        family: "proposal",
        subject: record.path,
        field: "proposal.historical_restatement",
        reason:
          "restating a historically approved proposal in the target needs new target events with their own provenance; the old payload must never be re-applied",
        proposed_strategy:
          "leave unresolved until a canonical importer exists that can emit the restatement events explicitly",
      });
    }
    if (status === "pending") {
      noteUnresolved({
        family: "proposal",
        subject: record.path,
        field: "proposal.expected_revision",
        reason:
          "a pending proposal's baseline cannot be mapped to a sage-db revision vector, so it cannot be carried as pending",
        proposed_strategy: "import as status=conflicted with needs_rebase and preserve the original payload and baseline verbatim",
      });
    }
  }

  // --- checkpoints ---------------------------------------------------------
  const checkpointRecords = records
    .filter((r) => r.path.startsWith("checkpoints/") && r.path.endsWith(".json"))
    .sort((a, b) => compareUtf8(a.path, b.path));
  const checkpoints: CheckpointMapping[] = [];
  checkpointRecords.forEach((record, offset) => {
    const raw = readBlob(archiveDir, record, byteBudget, limits.maxBytes);
    let parsed: Record<string, unknown>;
    try {
      parsed = JSON.parse(raw.toString("utf8")) as Record<string, unknown>;
    } catch (err) {
      noteError({ source: record.path, index: null, code: "INVALID_CHECKPOINT", message: (err as Error).message });
      return;
    }
    const concept = parsed["concept"];
    const session = parsed["session"];
    if (typeof concept !== "string" || concept === "") {
      noteError({ source: record.path, index: null, code: "INVALID_CHECKPOINT", message: "missing concept" });
      return;
    }
    if (typeof session !== "number" || !Number.isInteger(session) || session < 1) {
      noteError({ source: record.path, index: null, code: "INVALID_CHECKPOINT", message: "missing or invalid session" });
      return;
    }
    if (!slugToTarget.has(concept)) {
      noteError({
        source: record.path,
        index: null,
        code: "UNKNOWN_CHECKPOINT_CONCEPT",
        message: `checkpoint names concept ${concept}, which is not in this source`,
      });
      noteUnresolved({
        family: "checkpoint",
        subject: record.path,
        field: "checkpoint.concept_id",
        reason: `concept ${concept} is absent from the source capture`,
      });
    }
    recordBudget.spend(1);
    checkpoints.push({
      kind: "checkpoint",
      source_id: record.path,
      source_path: record.path,
      target_id: offset + 1,
      concept,
      session,
      content: { offset: 0, length: raw.length, sha256: sha256Hex(raw), blob: record.blob },
      metadata: parsed,
    });
  });

  // --- frontier ------------------------------------------------------------
  let frontier: FrontierEntryMapping[] = [];
  const frontierRecord = byPath.get("frontier.md");
  if (frontierRecord === undefined) {
    noteError({ source: "frontier.md", index: null, code: "MISSING_FRONTIER", message: "archive has no frontier.md" });
  } else {
    const raw = readBlob(archiveDir, frontierRecord, byteBudget, limits.maxBytes);
    // Frontier rows are pre-bounded by the caller's remaining record allowance, and
    // charged once as an aggregate, immediately after the array is built:
    // `recordBudget.spend(frontier.length)` decrements the shared counter BEFORE any
    // later family runs, so a family mapped afterwards sees the reduced remainder and
    // its own per-push spend refuses on the same counter. Being the last family is
    // NOT load-bearing; what is load-bearing is that the aggregate charge happens
    // before the next family's first spend, and it does.
    //
    // The parser is bounded (it refuses past the remaining allowance rather than
    // growing unbounded), so the array it hands back is already inside the cap; the
    // aggregate charge then accounts for it exactly once.
    const remaining = limits.maxRecords - recordBudget.total;
    const parsedFrontier = parseFrontierSections(
      raw.toString("utf8"),
      remaining,
      limits.maxDiagnostics - diagnostics.total,
    );
    frontier = parsedFrontier.entries;
    recordBudget.spend(frontier.length);
    for (const parsedError of parsedFrontier.errors) noteError(parsedError);

    const sections = new Set(frontier.map((e) => e.section));
    for (const required of ["未决概念", "概念内张力", "L2→L3 待展开方向"]) {
      if (!sections.has(required)) {
        noteError({
          source: "frontier.md",
          index: null,
          code: "MISSING_FRONTIER_SECTION",
          message: `expected section ${required} is absent`,
        });
      }
    }
    // Target contract: a frontier entry is a record with its own required fields.
    noteUnresolved({
      family: "frontier_entry",
      subject: "(all)",
      field: "frontier_entry.id/status",
      reason: "target requires an id and a status per entry; the Markdown table carries neither",
      proposed_strategy:
        "assign ids from declaration_order and map the priority cell to status where present, otherwise leave open",
    });
  }

  // --- duplicate detection across all families -----------------------------
  const idsByFamily = new Map<Family, Set<string>>();
  const noteId = (family: Family, id: string, source: string): void => {
    let seen = idsByFamily.get(family);
    if (seen === undefined) {
      seen = new Set<string>();
      idsByFamily.set(family, seen);
    }
    if (seen.has(id)) {
      noteError({ source, index: null, code: "DUPLICATE_ID", message: `${family} id ${id} appears twice` });
    }
    seen.add(id);
  };
  for (const t of terms) noteId("term", t.source_id, t.source_path);
  for (const a of assertions) noteId("assertion", a.source_id, a.source_path);
  for (const p of proposals) noteId("proposal", p.source_id, p.source_path);
  for (const c of checkpoints) noteId("checkpoint", c.source_id, c.source_path);
  for (const f of frontier) noteId("frontier_entry", f.source_id, f.source_path);
  for (const a of attachments) noteId("attachment", a.path, a.path);

  // --- deterministic order -------------------------------------------------
  const sortBySource = <T extends { source_id: string }>(items: T[]): T[] =>
    items.sort((a, b) => compareUtf8(a.source_id, b.source_id));

  const ordered = {
    terms,
    assertions: sortBySource(assertions),
    proposals: sortBySource(proposals),
    checkpoints: sortBySource(checkpoints),
    frontier,
  };

  const counts: MigrationPlan["counts"] = {
    term: ordered.terms.length,
    assertion: ordered.assertions.length,
    proposal: ordered.proposals.length,
    checkpoint: ordered.checkpoints.length,
    frontier_entry: ordered.frontier.length,
    attachment: attachments.length,
    source_proposal_files: proposalRecords.length,
  };

  const emitted = counts.term + counts.assertion + counts.proposal + counts.checkpoint + counts.frontier_entry + counts.attachment;
  if (!Number.isSafeInteger(emitted) || recordBudget.total !== emitted) {
    throw new PlanError("E_INTERNAL", "mapping-record admission differs from emitted record count");
  }

  const extractionComplete = errors.length === 0;
  // The plan is "complete" only as a *source extraction*: every field it examined
  // from the source resolved. Target mapping readiness is a separate axis and is
  // never claimed here.
  const status = extractionComplete && unresolved.length === 0 ? "complete" : "incomplete";

  return {
    plan_version: PLAN_VERSION,
    tool: { name: TOOL_NAME, version: TOOL_VERSION },
    run: { ...context, context_digest: contextDigest },
    source: {
      archive_dir: archiveDir,
      archive_content_digest: verified.contentDigest,
      source_digest: manifest.source.snapshot.digest,
      authority: manifest.authority.kind,
      kb_id: manifest.kb.id,
      files: records.length,
      bytes: records.reduce((sum, r) => sum + r.bytes, 0),
      extraction: extractionComplete ? "complete" : "incomplete",
    },
    limits,
    counts,
    terms: ordered.terms,
    assertions: ordered.assertions,
    proposals: ordered.proposals,
    checkpoints: ordered.checkpoints,
    frontier: ordered.frontier,
    attachments,
    unavailable,
    unresolved,
    errors,
    status,
    target_mapping: "not_ready",
  };
}

/** Canonical JSON: sorted keys, stable order, so two runs are byte-identical. */
export function canonicalJson(value: unknown): string {
  if (value === null) return "null";
  switch (typeof value) {
    case "string":
      return JSON.stringify(value);
    case "number":
      if (!Number.isFinite(value)) throw new PlanError("E_CANONICAL", "non-finite number");
      return JSON.stringify(value);
    case "boolean":
      return value ? "true" : "false";
    case "object": {
      if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
      const obj = value as Record<string, unknown>;
      const parts: string[] = [];
      for (const key of Object.keys(obj).sort()) {
        const item = obj[key];
        if (item === undefined) throw new PlanError("E_CANONICAL", `undefined at ${key}`);
        parts.push(`${JSON.stringify(key)}:${canonicalJson(item)}`);
      }
      return `{${parts.join(",")}}`;
    }
    default:
      throw new PlanError("E_CANONICAL", `unsupported type ${typeof value}`);
  }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

interface CliOptions {
  archive?: string;
  out?: string;
  planContext?: string;
  maxEntries?: number;
  maxBytes?: number;
  maxRecords?: number;
  maxArchiveBytes?: number;
  maxLineBytes?: number;
  maxDiagnostics?: number;
}

function parsePositive(flag: string, raw: string | undefined): number {
  if (raw === undefined || !/^[0-9]+$/.test(raw)) {
    throw new PlanError("E_USAGE", `${flag} must be a positive integer`);
  }
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value <= 0) {
    throw new PlanError("E_USAGE", `${flag} must be a positive integer`);
  }
  return value;
}

export function usage(): string {
  return [
    "usage:",
    "  bun tools/sage-db-import-plan.ts --archive <archive-dir> --out <new-plan.json>",
    "      --plan-context <context.json>",
    "      --max-entries N --max-bytes N --max-records N",
    "      --max-archive-bytes N --max-line-bytes N --max-diagnostics N",
    "",
    "Reads a verified source archive and writes a deterministic migration plan.",
    "All six ceilings and an explicit plan context are required: the plan never",
    "chooses its own bounds and never invents a timestamp.",
  ].join("\n");
}

/** Read the required plan context file. Timestamps are caller-supplied, never clock-derived. */
export function readPlanContext(path: string, ceiling: number): PlanContext {
  const raw = readBoundedUnknown(resolve(path), ceiling, `plan context ${path}`);
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw.toString("utf8"));
  } catch (err) {
    throw new PlanError("E_USAGE", `plan context is not JSON: ${(err as Error).message}`);
  }
  const obj = parsed as Record<string, unknown>;
  if (typeof obj["label"] !== "string" || obj["label"].trim() === "") {
    throw new PlanError("E_USAGE", "plan context needs a non-empty label");
  }
  if (typeof obj["created_at"] !== "string" || Number.isNaN(Date.parse(obj["created_at"] as string))) {
    throw new PlanError("E_USAGE", "plan context needs a timestamp created_at; it is never defaulted");
  }
  if (typeof obj["purpose"] !== "string" || obj["purpose"].trim() === "") {
    throw new PlanError("E_USAGE", "plan context needs a non-empty purpose");
  }
  return { label: obj["label"] as string, created_at: obj["created_at"] as string, purpose: obj["purpose"] as string };
}

export function parseCli(argv: readonly string[]): CliOptions {
  if (argv.includes("--help") || argv.includes("-h")) {
    throw new PlanError("E_HELP", usage());
  }
  const opts: CliOptions = {};
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i]!;
    const next = (): string => {
      const value = argv[i + 1];
      if (value === undefined) throw new PlanError("E_USAGE", `${flag} requires a value`);
      i += 1;
      return value;
    };
    switch (flag) {
      case "--archive":
        opts.archive = next();
        break;
      case "--out":
        opts.out = next();
        break;
      case "--plan-context":
        opts.planContext = next();
        break;
      case "--max-entries":
        opts.maxEntries = parsePositive(flag, next());
        break;
      case "--max-bytes":
        opts.maxBytes = parsePositive(flag, next());
        break;
      case "--max-records":
        opts.maxRecords = parsePositive(flag, next());
        break;
      case "--max-archive-bytes":
        opts.maxArchiveBytes = parsePositive(flag, next());
        break;
      case "--max-line-bytes":
        opts.maxLineBytes = parsePositive(flag, next());
        break;
      case "--max-diagnostics":
        opts.maxDiagnostics = parsePositive(flag, next());
        break;
      default:
        throw new PlanError("E_USAGE", `unknown argument ${JSON.stringify(flag)}`);
    }
  }
  return opts;
}

export function runCli(argv: readonly string[]): number {
  let opts: CliOptions;
  try {
    opts = parseCli(argv);
  } catch (err) {
    if (err instanceof PlanError && err.code === "E_HELP") {
      process.stdout.write(`${usage()}\n`);
      return EXIT.ok;
    }
    process.stderr.write(`${(err as Error).message}\n\n${usage()}\n`);
    return EXIT.usage;
  }

  try {
    const { archive, out, planContext, maxEntries, maxBytes, maxRecords, maxArchiveBytes, maxLineBytes, maxDiagnostics } = opts;
    if (archive === undefined || out === undefined || planContext === undefined ||
        maxEntries === undefined || maxBytes === undefined || maxRecords === undefined ||
        maxArchiveBytes === undefined || maxLineBytes === undefined || maxDiagnostics === undefined) {
      throw new PlanError(
        "E_USAGE",
        "--archive, --out, --plan-context and all six ceilings (including --max-diagnostics) are required",
      );
    }
    // Containment BEFORE any write, on PHYSICAL paths.
    //
    // `resolve()` is purely lexical: it collapses `..` and makes a path absolute but
    // never consults the filesystem, so `--out /alias/arch/plan.json` where `alias`
    // is a symlink to the archive's real parent passes a lexical check while the
    // write lands inside the archive. That mutates the capture and invalidates the
    // plan's own binding, so the check must compare the paths the OS will actually
    // use: the archive's realpath, and the realpath of the output's existing parent
    // joined with the output's basename (the output itself may not exist yet).
    // The same physical path is then used for the `wx` write, so the validated path
    // and the written path cannot diverge.
    const archiveReal = realpathSync(archive);
    const outLexical = resolve(out);
    const outParent = dirname(outLexical);
    let outParentReal: string;
    try {
      outParentReal = realpathSync(outParent);
    } catch (err) {
      // A missing parent fails here, before any mkdir and before any source write.
      throw new PlanError(
        "E_OUTPUT_PARENT",
        `output parent ${outParent} is not resolvable: ${(err as Error).message}`,
      );
    }
    const outPhysical = join(outParentReal, basename(outLexical));
    const inside = (parent: string, child: string): boolean => {
      const withSep = parent.endsWith("/") ? parent : `${parent}/`;
      return child === parent || child.startsWith(withSep);
    };
    if (inside(archiveReal, outPhysical)) {
      throw new PlanError(
        "E_OUTPUT_INSIDE_ARCHIVE",
        `refusing to write the plan inside the archive it describes ` +
          `(physical output ${outPhysical} is within physical archive ${archiveReal})`,
      );
    }
    if (existsSync(outPhysical)) {
      throw new PlanError("E_OUTPUT_EXISTS", `refusing to overwrite existing plan ${outPhysical}`);
    }
    // Context is a JSON document, not one NDJSON stream line.
    const context = readPlanContext(planContext, maxArchiveBytes);
    const plan = generatePlan({
      archiveDir: archive,
      context,
      limits: { maxEntries, maxBytes, maxRecords, maxArchiveBytes, maxLineBytes, maxDiagnostics },
    });
    // The same physical path that survived the containment check: validating one
    // path and writing another is exactly the divergence this check exists to stop.
    writeFileSync(outPhysical, `${canonicalJson(plan)}\n`, { flag: "wx" });
    process.stdout.write(
      `plan ${outPhysical}\n` +
        `extraction=${plan.source.extraction} target_mapping=${plan.target_mapping}\n` +
        `terms=${plan.counts.term} assertions=${plan.counts.assertion} proposals=${plan.counts.proposal} ` +
        `(source files ${plan.counts.source_proposal_files}) checkpoints=${plan.counts.checkpoint} ` +
        `frontier=${plan.counts.frontier_entry} attachments=${plan.counts.attachment} ` +
        `errors=${plan.errors.length} unresolved=${plan.unresolved.length}\n`,
    );
    return plan.status === "complete" ? EXIT.ok : EXIT.incomplete;
  } catch (err) {
    if (err instanceof PlanError) {
      process.stderr.write(`${err.code}: ${err.message}\n`);
      return err.code === "E_USAGE" ? EXIT.usage : EXIT.refused;
    }
    process.stderr.write(`unexpected: ${(err as Error).stack ?? String(err)}\n`);
    return EXIT.refused;
  }
}

if (import.meta.main) {
  process.exit(runCli(process.argv.slice(2)));
}
