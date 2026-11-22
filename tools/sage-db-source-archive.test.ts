import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { createHash } from "node:crypto";
import {
  cpSync,
  existsSync,
  chmodSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
  ARCHIVE_VERSION,
  ArchiveError,
  EXPORTED_SCHEMA_PROFILE,
  EXIT,
  KNOWN_META_KEYS,
  TABLE_ORDER,
  canonicalJson,
  exportSourceArchive,
  manifestContentDigest,
  parseCli,
  runCli,
  verifySourceArchive,
  type ArchiveManifest,
} from "./sage-db-source-archive.ts";

/**
 * The archive exporter, exercised only against temporary fixtures.
 *
 * Two rules this file keeps, because the tool's value depends on them:
 *
 * 1. The repository KBs are never exported — `fixtures/architecture-review` is
 *    copied to a temp directory first, and every database is built from scratch
 *    in that copy. There is no test that touches `kb/default`.
 * 2. Expected values come from the fixture itself (bytes read back off disk,
 *    rows read back through an independent SQLite connection), never from the
 *    exporter's own output. A test that compared the exporter to itself would
 *    pass while dropping a column.
 */

const REPO = resolve(import.meta.dir, "..");
const FIXTURE_KB = join(REPO, "fixtures", "architecture-review");

interface Sandbox {
  root: string;
  cleanup: () => void;
}

let sandboxes: Sandbox[] = [];

function sandbox(): Sandbox {
  const root = mkdtempSync(join(tmpdir(), "sage-archive-"));
  const box: Sandbox = {
    root,
    cleanup: () => rmSync(root, { recursive: true, force: true }),
  };
  sandboxes.push(box);
  return box;
}

afterEach(() => {
  for (const box of sandboxes) box.cleanup();
  sandboxes = [];
});

/** Copy the fixture KB into the sandbox and add a registry describing it. */
function fixtureSource(box: Sandbox): { source: string; registry: string } {
  const source = join(box.root, "kb");
  cpSync(FIXTURE_KB, source, { recursive: true });
  writeFileSync(join(source, "AI.md"), "# KB self-description\n\nFixture KB.\n", "utf8");
  writeFileSync(join(source, "frontier.md"), "# Frontier\n\n| concept | unresolved |\n|---|---|\n| review-risk | open |\n", "utf8");
  mkdirSync(join(source, "protocols"), { recursive: true });
  writeFileSync(join(source, "protocols", "exploration.md"), "# Exploration\n", "utf8");
  mkdirSync(join(source, "checkpoints"), { recursive: true });
  writeFileSync(
    join(source, "checkpoints", "review-risk-s1.json"),
    JSON.stringify({ concept: "review-risk", session: 1, established: ["a"] }, null, 2),
    "utf8",
  );
  mkdirSync(join(source, "proposals", "edges"), { recursive: true });
  writeFileSync(
    join(source, "proposals", "edges", "review-risk.json"),
    JSON.stringify({ kind: "edge", status: "pending" }, null, 2),
    "utf8",
  );
  const registry = join(box.root, "registry.json");
  writeFileSync(
    registry,
    JSON.stringify(
      {
        version: "1",
        knowledgeBases: [
          {
            id: "architecture-review",
            name: "Architecture Review",
            description: "fixture",
            path: "kb/architecture-review",
            domains: ["engineering"],
            created: "2026-11-16",
          },
        ],
      },
      null,
      2,
    ),
    "utf8",
  );
  return { source, registry };
}

function sha256File(file: string): string {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

const legacySchema = readFileSync(new URL("./fixtures/sqlite-v1.sql", import.meta.url), "utf8");

/** Open a fixture using the frozen schema from the retired SQLite service. */
function openLegacyFixture(root: string): Database {
  const db = new Database(join(root, "knowledge.db"), { create: true, readwrite: true });
  db.exec("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;");
  db.exec(legacySchema);
  return db;
}

/** Build an offline source fixture using the original SQLite layout. */
function buildDatabase(
  source: string,
  options: { imported: boolean; rows?: (db: Database) => void } = { imported: true },
): void {
  const db = openLegacyFixture(source);
  try {
    db.run("INSERT INTO meta (key, value) VALUES ('kb_root', ?)", [source]);
    db.run("INSERT INTO meta (key, value) VALUES ('schema_version', '1')");
    if (options.imported) {
      db.run("INSERT INTO meta (key, value) VALUES ('imported_at', ?)", ["2026-11-19T10:00:00.000Z"]);
    }
    options.rows?.(db);
    db.exec("PRAGMA wal_checkpoint(TRUNCATE)");
  } finally {
    db.close();
  }
  // Closing leaves an empty WAL/SHM pair; remove them so the fixtures start
  // from the simplest honest layout (a lone knowledge.db).
  for (const sidecar of ["knowledge.db-wal", "knowledge.db-shm"]) {
    const abs = join(source, sidecar);
    if (existsSync(abs)) rmSync(abs);
  }
}

/**
 * Caller ceilings used by tests. Deliberately roomy for fixtures but finite:
 * the API requires them, so a test that forgot them would not compile.
 */
const verifyLimits = {
  maxArchiveBytes: 64 * 1024 * 1024,
  maxRecords: 100_000,
  maxLineBytes: 4 * 1024 * 1024,
  maxEntries: 100_000,
};

const limits = {
  maxFiles: 1000,
  maxBytes: 50 * 1024 * 1024,
  maxRows: 10_000,
  maxEntries: 5000,
  maxDepth: 32,
};

interface ArchiveLayout {
  files: string[];
  rows: Array<Record<string, unknown>>;
}

function readArchive(outDir: string): ArchiveLayout {
  const files = readFileSync(join(outDir, "files.ndjson"), "utf8")
    .split("\n")
    .filter((line) => line !== "")
    .map((line) => JSON.parse(line) as Record<string, unknown>);
  const rowsPath = join(outDir, "rows.ndjson");
  const rows = existsSync(rowsPath)
    ? readFileSync(rowsPath, "utf8")
        .split("\n")
        .filter((line) => line !== "")
        .map((line) => JSON.parse(line) as Record<string, unknown>)
    : [];
  return { files: files.map((r) => String(r["path"])), rows };
}

function rowValues(rows: Array<Record<string, unknown>>, table: string): unknown[][] {
  return rows.filter((r) => r["table"] === table).map((r) => r["values"] as unknown[]);
}

describe("source and output path refusals", () => {
  test("an existing output directory is refused and left untouched", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "archive");
    mkdirSync(out);
    writeFileSync(join(out, "keep.txt"), "prior archive\n", "utf8");

    expect(() => exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits })).toThrow(
      /refusing to overwrite existing output/,
    );
    expect(readFileSync(join(out, "keep.txt"), "utf8")).toBe("prior archive\n");
  });

  test("an output nested in the source is refused before anything is written", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const nested = join(source, "archive-out");
    expect(() => exportSourceArchive({ sourceRoot: source, outDir: nested, kb: "architecture-review", registryPath: registry, limits })).toThrow(
      /inside the source root/,
    );
    expect(existsSync(nested)).toBe(false);
  });

  test("a source nested in the output is refused", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "outer");
    // Move the KB inside a would-be output directory: the containment check has
    // to compare real paths, not just refuse because `out` already exists.
    rmSync(source, { recursive: true, force: true });
    const nestedSource = join(out, "kb");
    cpSync(FIXTURE_KB, nestedSource, { recursive: true });
    expect(() =>
      exportSourceArchive({ sourceRoot: nestedSource, outDir: out, kb: "architecture-review", registryPath: registry, limits }),
    ).toThrow(/inside the output/);
  });

  test("a symlink anywhere in the source aborts the export", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    symlinkSync(join(source, "kb.json"), join(source, "kb-link.json"));
    expect(() => exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits })).toThrow(
      /refusing to export symlink kb-link.json/,
    );
    expect(existsSync(join(box.root, "out"))).toBe(false);
  });

  test("a FIFO is refused rather than silently skipped", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const fifo = join(source, "pipe");
    const made = Bun.spawnSync(["mkfifo", fifo]);
    if (made.exitCode !== 0) return; // platform without mkfifo; nothing to assert
    expect(() => exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits })).toThrow(
      /refusing to export non-regular file pipe/,
    );
  });
});

describe("registry is deployment context, source root is explicit", () => {
  test("the registry path is recorded but never resolved as the source", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const result = exportSourceArchive({
      sourceRoot: source,
      outDir: join(box.root, "out"),
      kb: "architecture-review",
      registryPath: registry,
      limits,
      createdAt: "2026-11-19T12:00:00.000Z",
    });
    expect(result.manifest.kb.entry.path).toBe("kb/architecture-review");
    expect(result.manifest.source.root_real).toBe(realpathSync(source));
    // The registry path is relative to the registry's deployment root; if the
    // tool had resolved it, the export would have pointed at a missing dir.
    expect(result.manifest.kb.registry.sha256).toBe(sha256File(registry));
  });

  test("an unknown kb id fails before the output exists", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    expect(() => exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "nope", registryPath: registry, limits })).toThrow(
      /registry has no knowledge base with id "nope"/,
    );
    expect(existsSync(join(box.root, "out"))).toBe(false);
  });
});

describe("markdown authority keeps every file byte-exact", () => {
  test("all relative files (AI, MANIFEST, frontier, protocols, checkpoints, proposals, assets) are exported", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    const result = exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits });

    expect(result.manifest.authority.kind).toBe("markdown");
    expect(result.manifest.authority.reason).toBe("no-database");

    const layout = readArchive(out);
    const onDisk: string[] = [];
    const walk = (dir: string, prefix: string): void => {
      for (const name of readdirSync(dir)) {
        const abs = join(dir, name);
        const rel = prefix === "" ? name : `${prefix}/${name}`;
        if (statSync(abs).isDirectory()) walk(abs, rel);
        else onDisk.push(rel);
      }
    };
    walk(source, "");
    onDisk.sort();
    expect(layout.files).toEqual(onDisk);
    expect(layout.files).toContain("AI.md");
    expect(layout.files).toContain("MANIFEST.md");
    expect(layout.files).toContain("frontier.md");
    expect(layout.files).toContain("protocols/exploration.md");
    expect(layout.files).toContain("checkpoints/review-risk-s1.json");
    expect(layout.files).toContain("proposals/edges/review-risk.json");
    expect(layout.files).toContain("assets/latency-chart.png");
    expect(layout.files).toContain("concepts/L2-support/review-tension.md");

    // Every recorded hash and blob is checked against disk, not against the
    // exporter's own manifest.
    const manifest = result.manifest;
    const fileRecords = readFileSync(join(out, "files.ndjson"), "utf8")
      .split("\n")
      .filter((line) => line !== "")
      .map((line) => JSON.parse(line) as Record<string, unknown>);
    for (const record of fileRecords) {
      const rel = String(record["path"]);
      const sourceBytes = readFileSync(join(source, rel));
      expect(record["sha256"]).toBe(createHash("sha256").update(sourceBytes).digest("hex"));
      expect(record["bytes"]).toBe(sourceBytes.length);
      const blob = readFileSync(join(out, String(record["blob"])));
      expect(blob.equals(sourceBytes)).toBe(true);
    }
    expect(manifest.counts.files).toBe(onDisk.length);
    expect(manifest.counts.rows).toBe(0);
  });

  test("the completed archive verifies and has no row stream", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits });
    const verified = verifySourceArchive(out, verifyLimits);
    expect(verified.authority).toBe("markdown");
    expect(verified.counts.rows).toBe(0);
    expect(existsSync(join(out, "rows.ndjson"))).toBe(false);
    expect(existsSync(join(out, "COMPLETE.json"))).toBe(true);
  });
});

describe("structured authority preserves exact row values", () => {
  test("bigints, blobs, Unicode, empty strings and NULLs survive every table", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, {
      imported: true,
      rows: (db) => {
        db.run(
          "INSERT INTO concepts (id, atom, layer, domain_json, confidence, source, triggers_json, body, source_path, revision, source_hash, source_revision, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
          [
            "c1",
            "atom with \u00e9 and \u4e2d\u6587",
            1,
            '["a","b"]',
            "confirmed",
            "personal",
            '["x"]',
            "body\u0000with nul",
            "concepts/L1-core/c1.md",
            "rev-1",
            null,
            "2026-11-19T00:00:00.000Z",
            "2026-11-19T00:00:00.000Z",
            "2026-11-19T00:00:00.000Z",
          ],
        );
        db.run(
          "INSERT INTO concepts (id, atom, layer, domain_json, confidence, source, triggers_json, body, source_path, revision, source_hash, source_revision, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
          ["c2", "", 3, "[]", "hypothetical", "domain", "[]", "", "", "rev-2", "h", "r", "t", "t"],
        );
        db.run(
          "INSERT INTO relations (from_id, type, target, confidence, position, created_at) VALUES (?,?,?,?,?,?)",
          ["c1", "depends-on", "c2", "contested", 9007199254740993n, "t"],
        );
        db.run(
          "INSERT INTO vectors (id, subject_type, subject_id, model, dimensions, revision, provider, values_blob, norm, created_at) VALUES (?,?,?,?,?,?,?,?,?,?)",
          ["v1", "concept", "c1", "m", 3, "r", null, new Uint8Array([0, 1, 0xff, 0x7f]), 0.1 + 0.2, "t"],
        );
        db.run(
          "INSERT INTO vectors (id, subject_type, subject_id, model, dimensions, revision, provider, values_blob, norm, created_at) VALUES (?,?,?,?,?,?,?,?,?,?)",
          ["v2", "concept", "c2", "m", 1, "r", "p", new Uint8Array(0), 1e308, "t"],
        );
        db.run(
          "INSERT INTO evidence (id, kind, title, locator, excerpt, asset_sha256, asset_media_type, asset_bytes, captured_at, captured_by, created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
          ["e1", "link", "t", null, null, null, null, null, "t", "t", "t"],
        );
        db.run(
          "INSERT INTO assets (sha256, media_type, byte_size, path, created_at) VALUES (?,?,?,?,?)",
          ["a".repeat(64), "image/png", 9007199254740995n, "assets/x", "t"],
        );
      },
    });

    const out = join(box.root, "out");
    const result = exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits });
    expect(result.manifest.authority.kind).toBe("structured");
    expect(result.manifest.authority.reason).toBe("imported");
    expect(result.manifest.authority.database?.imported_at).toBe("2026-11-19T10:00:00.000Z");
    expect(result.manifest.authority.database?.logical_snapshot_digest).toMatch(/^sha256:[0-9a-f]{64}$/);

    const layout = readArchive(out);
    // Zero-row tables emit no records, so enumeration is a declaration: the row
    // stream must name every registered table with its exact row count.
    const declared = result.manifest.streams.find((s) => s.path === "rows.ndjson")?.tables ?? [];
    expect(declared.map((entry) => entry.table).sort()).toEqual([...TABLE_ORDER].sort());
    const counted = declared.filter((entry) => entry.rows > 0).map((entry) => entry.table);
    const streamed = new Set(layout.rows.map((row) => String(row["table"])));
    expect([...streamed].sort()).toEqual(counted.sort());
    expect(declared.find((entry) => entry.table === "concepts")?.rows).toBe(2);

    const conceptRows = rowValues(layout.rows, "concepts");
    expect(conceptRows).toHaveLength(2);
    const c1 = conceptRows[0]!;
    expect(c1[0]).toEqual({ t: "text", v: "c1" });
    expect(c1[1]).toEqual({ t: "text", v: "atom with \u00e9 and \u4e2d\u6587" });
    expect(c1[2]).toEqual({ t: "integer", v: "1" });
    expect(c1[7]).toEqual({ t: "text", v: "body\u0000with nul" });
    expect(c1[10]).toEqual({ t: "null" });
    const c2 = conceptRows[1]!;
    expect(c2[1]).toEqual({ t: "text", v: "" });
    expect(c2[7]).toEqual({ t: "text", v: "" });

    // > 2^53 survives as an exact decimal, not a rounded double.
    const relation = rowValues(layout.rows, "relations")[0]!;
    expect(relation[4]).toEqual({ t: "integer", v: "9007199254740993" });

    // REAL values are captured bit-exactly, so 0.1+0.2 is not re-formatted.
    const vectors = rowValues(layout.rows, "vectors");
    const v1 = vectors[0]!;
    const norm = v1[8] as { t: string; bits: string };
    expect(norm.t).toBe("real");
    const view = new DataView(new ArrayBuffer(8));
    Buffer.from(norm.bits, "hex").forEach((byte, i) => view.setUint8(i, byte));
    expect(view.getFloat64(0)).toBe(0.1 + 0.2);
    expect(v1[7]).toEqual({ t: "blob", bytes_base64: Buffer.from([0, 1, 0xff, 0x7f]).toString("base64"), bytes: 4 });
    expect(v1[6]).toEqual({ t: "null" });

    const v2 = vectors[1]!;
    expect(v2[7]).toEqual({ t: "blob", bytes_base64: "", bytes: 0 });
    expect(v2[9]).toEqual({ t: "text", v: "t" });

    // A nullable column that is NULL everywhere is still recorded as null.
    const evidence = rowValues(layout.rows, "evidence")[0]!;
    expect(evidence[3]).toEqual({ t: "null" });
    expect(evidence[7]).toEqual({ t: "null" });

    // INTEGER beyond 2^53 in an ordinary column is exact too.
    expect(rowValues(layout.rows, "assets")[0]![2]).toEqual({ t: "integer", v: "9007199254740995" });
  });

  test("the raw database bytes are archived exactly once, as ordinary source files", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, { imported: true });
    const out = join(box.root, "out");
    const result = exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits });

    const retained = result.manifest.authority.database?.files ?? [];
    expect(retained.map((f) => f.path)).toEqual(["knowledge.db"]);
    expect(retained[0]!.sha256).toBe(sha256File(join(source, "knowledge.db")));

    // The bytes live in the blob the file stream already records; there is no
    // second `source-db/` copy to drift from it.
    expect(existsSync(join(out, "source-db"))).toBe(false);
    expect(readFileSync(join(out, "blobs", retained[0]!.sha256)).equals(readFileSync(join(source, "knowledge.db")))).toBe(true);
    expect(readArchive(out).files).toContain("knowledge.db");
  });
});

describe("authority is fail-closed", () => {
  test("a corrupt database is an error, not a markdown fallback", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    writeFileSync(join(source, "knowledge.db"), "this is not a sqlite database\n", "utf8");
    expect(() => exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits })).toThrow(
      ArchiveError,
    );
    expect(existsSync(join(box.root, "out", "COMPLETE.json"))).toBe(false);
  });

  test("an unsupported schema version is refused", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, { imported: true });
    const db = new Database(join(source, "knowledge.db"), { readwrite: true });
    db.exec("PRAGMA user_version = 2");
    db.close();
    try {
      exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits });
      throw new Error("expected refusal");
    } catch (err) {
      expect(String(err)).toContain("user_version=2");
      expect(String(err)).toContain("supported schema is 1");
    }
  });

  test("a table outside the profile is refused rather than exported unclassified", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, { imported: true });
    const db = new Database(join(source, "knowledge.db"), { readwrite: true });
    db.exec("CREATE TABLE surprise (id TEXT PRIMARY KEY)");
    db.close();
    expect(() => exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits })).toThrow(
      /tables outside schema version 1: surprise/,
    );
  });

  test("an extra column in a known table is refused", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, { imported: true });
    const db = new Database(join(source, "knowledge.db"), { readwrite: true });
    db.exec("ALTER TABLE claims ADD COLUMN surprise TEXT");
    db.close();
    expect(() => exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits })).toThrow(
      /table claims columns differ from schema version 1/,
    );
  });

  test("a database without meta.imported_at stays markdown but keeps its raw file", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, { imported: false });
    const out = join(box.root, "out");
    const result = exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits });
    expect(result.manifest.authority.kind).toBe("markdown");
    expect(result.manifest.authority.reason).toBe("database-not-imported");
    const dbBlock = result.manifest.authority.database!;
    expect(dbBlock.imported_at).toBeNull();
    expect(dbBlock.logical_snapshot_digest).toBeNull();
    expect(dbBlock.schema).not.toBeNull();
    // Its bytes are retained through the file stream, not a duplicate directory.
    expect(readArchive(out).files).toContain("knowledge.db");
    expect(existsSync(join(out, "source-db"))).toBe(false);
    expect(existsSync(join(out, "rows.ndjson"))).toBe(false);
    // The concept files are still the exported truth.
    expect(readArchive(out).files).toContain("concepts/L1-core/review-decision.md");
  });

  test("an unparseable imported_at is refused, not treated as not-imported", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, { imported: true });
    const db = new Database(join(source, "knowledge.db"), { readwrite: true });
    db.run("UPDATE meta SET value = 'not-a-time' WHERE key = 'imported_at'");
    db.close();
    expect(() => exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits })).toThrow(
      /is not a timestamp/,
    );
  });

  test("an unknown meta key is refused", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, { imported: true });
    const db = new Database(join(source, "knowledge.db"), { readwrite: true });
    db.run("INSERT INTO meta (key, value) VALUES ('future_key', 'x')");
    db.close();
    expect(() => exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits })).toThrow(
      /meta keys outside schema version 1: future_key/,
    );
  });
});

describe("budgets are checked against actual content", () => {
  test("maxFiles stops the export before an over-budget tree is read", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    expect(() =>
      exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits: { ...limits, maxFiles: 3 } }),
    ).toThrow(/maxFiles=3/);
    expect(existsSync(join(box.root, "out"))).toBe(false);
  });

  test("maxBytes is a real budget, not a post-hoc count", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    expect(() =>
      exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits: { ...limits, maxBytes: 512 } }),
    ).toThrow(/maxBytes=512/);
    expect(existsSync(join(box.root, "out"))).toBe(false);
  });

  test("maxEntries bounds directory work, not just files", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    // A tree of empty directories costs readdir work with zero files: a
    // file-count bound alone would never fire here.
    for (let i = 0; i < 20; i += 1) {
      mkdirSync(join(source, "empty", `d${i}`), { recursive: true });
    }
    expect(() =>
      exportSourceArchive({
        sourceRoot: source,
        outDir: join(box.root, "out"),
        kb: "architecture-review",
        registryPath: registry,
        limits: { ...limits, maxEntries: 15 },
      }),
    ).toThrow(/maxEntries=15/);
    expect(existsSync(join(box.root, "out"))).toBe(false);
  });

  test("maxDepth bounds nesting instead of relying on the filesystem", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    mkdirSync(join(source, "a", "b", "c"), { recursive: true });
    writeFileSync(join(source, "a", "b", "c", "deep.md"), "x", "utf8");
    expect(() =>
      exportSourceArchive({
        sourceRoot: source,
        outDir: join(box.root, "out"),
        kb: "architecture-review",
        registryPath: registry,
        limits: { ...limits, maxDepth: 2 },
      }),
    ).toThrow(/maxDepth=2/);
  });

  test("a file that grows between stat and read is refused, not buffered", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const target = join(source, "frontier.md");
    // Grow the file and re-stat behind the walker's back by re-running the
    // export with a hook that appends after the inventory is frozen.
    expect(() =>
      exportSourceArchive({
        sourceRoot: source,
        outDir: join(box.root, "out"),
        kb: "architecture-review",
        registryPath: registry,
        limits,
        hooks: {
          beforeFinalCheck: () => {
            const grown = readFileSync(target);
            writeFileSync(target, Buffer.concat([grown, Buffer.alloc(1024, 0x41)]));
          },
        },
      }),
    ).toThrow(/frontier\.md/);
    expect(existsSync(join(box.root, "out", "COMPLETE.json"))).toBe(false);
  });

  test("maxRows aborts a structured export mid-stream and leaves no marker", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, {
      imported: true,
      rows: (db) => {
        for (let i = 0; i < 20; i += 1) {
          db.run(
            "INSERT INTO concepts (id, atom, layer, domain_json, confidence, source, triggers_json, body, source_path, revision, source_hash, source_revision, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            [`c${i}`, "a", 1, "[]", "confirmed", "personal", "[]", "b", "p", "r", null, null, "t", "t"],
          );
        }
      },
    });
    const out = join(box.root, "out");
    expect(() =>
      exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits: { ...limits, maxRows: 5 } }),
    ).toThrow(/maxRows=5/);
    expect(existsSync(join(out, "COMPLETE.json"))).toBe(false);
    // Rows are exported before the manifest is assembled, so this failure leaves
    // neither a manifest nor a marker — only the honest partial directory.
    expect(existsSync(join(out, "archive.json"))).toBe(false);
    // The partial directory is kept on purpose: removing it would erase evidence
    // of what the failed run had already written.
    expect(existsSync(out)).toBe(true);
  });
});

describe("concurrent mutation never produces a complete archive", () => {
  test("a source file written during export fails the export", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    expect(() =>
      exportSourceArchive({
        sourceRoot: source,
        outDir: out,
        kb: "architecture-review",
        registryPath: registry,
        limits,
        hooks: {
          beforeFinalCheck: () => {
            writeFileSync(join(source, "frontier.md"), "# Frontier\n\nchanged mid-export\n", "utf8");
          },
        },
      }),
    ).toThrow(/source tree changed during export: frontier\.md modified/);
    expect(existsSync(join(out, "COMPLETE.json"))).toBe(false);
    const manifest = JSON.parse(readFileSync(join(out, "archive.json"), "utf8")) as ArchiveManifest;
    expect(manifest.complete).toBe(false);
  });

  test("a database write during export fails the export", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, { imported: true });
    const out = join(box.root, "out");
    expect(() =>
      exportSourceArchive({
        sourceRoot: source,
        outDir: out,
        kb: "architecture-review",
        registryPath: registry,
        limits,
        hooks: {
          beforeFinalCheck: () => {
            const db = new Database(join(source, "knowledge.db"), { readwrite: true });
            db.run("INSERT INTO meta (key, value) VALUES ('last_review_at', '2026-11-19T11:00:00.000Z')");
            db.close();
          },
        },
      }),
    ).toThrow(/source tree changed during export: knowledge\.db/);
    expect(existsSync(join(out, "COMPLETE.json"))).toBe(false);
  });

  test("a file added during export fails the export", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    expect(() =>
      exportSourceArchive({
        sourceRoot: source,
        outDir: out,
        kb: "architecture-review",
        registryPath: registry,
        limits,
        hooks: { beforeFinalCheck: () => writeFileSync(join(source, "new-file.md"), "x", "utf8") },
      }),
    ).toThrow(/new-file\.md added/);
  });
});

describe("verification rejects damaged and unknown input", () => {
  function goodArchive(): { out: string; outDir: string } {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, { imported: true });
    const outDir = join(box.root, "out");
    exportSourceArchive({ sourceRoot: source, outDir, kb: "architecture-review", registryPath: registry, limits });
    return { out: outDir, outDir };
  }

  test("a tampered blob is rejected", () => {
    const { outDir } = goodArchive();
    const blobs = readdirSync(join(outDir, "blobs"));
    const target = join(outDir, "blobs", blobs[0]!);
    const bytes = readFileSync(target);
    bytes[0] = bytes[0] === 0 ? 1 : 0;
    writeFileSync(target, bytes);
    expect(() => verifySourceArchive(outDir, verifyLimits)).toThrow(/blob .* (content hash mismatch|length mismatch)/);
  });

  test("a missing blob is rejected", () => {
    const { outDir } = goodArchive();
    rmSync(join(outDir, "blobs", readdirSync(join(outDir, "blobs"))[0]!));
    expect(() => verifySourceArchive(outDir, verifyLimits)).toThrow(/is missing/);
  });

  test("a tampered row stream is rejected", () => {
    const { outDir } = goodArchive();
    const rowsPath = join(outDir, "rows.ndjson");
    const text = readFileSync(rowsPath, "utf8");
    writeFileSync(rowsPath, text.replace('"v":"1"', '"v":"2"'));
    expect(() => verifySourceArchive(outDir, verifyLimits)).toThrow(/stream rows\.ndjson digest mismatch/);
  });

  test("a removed completion marker is rejected", () => {
    const { outDir } = goodArchive();
    rmSync(join(outDir, "COMPLETE.json"));
    expect(() => verifySourceArchive(outDir, verifyLimits)).toThrow(/COMPLETE\.json is missing/);
  });

  test("a manifest edited after export is rejected", () => {
    const { outDir } = goodArchive();
    const manifest = JSON.parse(readFileSync(join(outDir, "archive.json"), "utf8")) as ArchiveManifest;
    manifest.counts.files += 1;
    writeFileSync(join(outDir, "archive.json"), `${canonicalJson(manifest)}\n`, "utf8");
    expect(() => verifySourceArchive(outDir, verifyLimits)).toThrow(/content_digest mismatch/);
  });

  test("an unknown archive_version is rejected", () => {
    const { outDir } = goodArchive();
    const manifest = JSON.parse(readFileSync(join(outDir, "archive.json"), "utf8")) as ArchiveManifest;
    (manifest as unknown as Record<string, unknown>)["archive_version"] = "2";
    manifest.content_digest = manifestContentDigest(manifest);
    writeFileSync(join(outDir, "archive.json"), `${canonicalJson(manifest)}\n`, "utf8");
    expect(() => verifySourceArchive(outDir, verifyLimits)).toThrow(/unsupported archive_version/);
  });

  test("a half-written export (manifest says complete, no marker) is rejected", () => {
    const { outDir } = goodArchive();
    rmSync(join(outDir, "COMPLETE.json"));
    const manifest = JSON.parse(readFileSync(join(outDir, "archive.json"), "utf8")) as ArchiveManifest;
    expect(manifest.complete).toBe(true);
    expect(() => verifySourceArchive(outDir, verifyLimits)).toThrow(/COMPLETE\.json is missing/);
  });

  test("a symlink in the source is rejected by the exported inventory, not followed", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    symlinkSync(join(source, "kb.json"), join(source, "nested-link"));
    expect(() => exportSourceArchive({ sourceRoot: source, outDir: join(box.root, "out"), kb: "architecture-review", registryPath: registry, limits })).toThrow(
      /symlink/,
    );
  });
});

describe("CLI contract", () => {
  test("every export flag is required and no default path exists", () => {
    expect(() => parseCli([])).not.toThrow();
    const parsed = parseCli([]);
    expect(parsed.source).toBeUndefined();
    expect(parsed.out).toBeUndefined();
    expect(runCli([])).toBe(EXIT.usage);
    expect(runCli(["--source", "x"])).toBe(EXIT.usage);
  });

  test("limits must be positive integers", () => {
    expect(() => parseCli(["--max-files", "0"])).toThrow(/positive integer/);
    expect(() => parseCli(["--max-rows", "-1"])).toThrow(/positive integer/);
    expect(() => parseCli(["--max-bytes", "1.5"])).toThrow(/positive integer/);
  });

  test("an export then a verify round-trips through the CLI exit codes", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    const exportExit = runCli([
      "--source", source,
      "--out", out,
      "--kb", "architecture-review",
      "--registry", registry,
      "--max-files", "1000",
      "--max-bytes", String(50 * 1024 * 1024),
      "--max-rows", "10000",
      "--max-entries", "5000",
      "--max-depth", "32",
      "--created-at", "2026-11-19T12:00:00.000Z",
    ]);
    expect(exportExit).toBe(EXIT.ok);
    const verifyArgs = [
      "--max-archive-bytes", String(verifyLimits.maxArchiveBytes),
      "--max-records", String(verifyLimits.maxRecords),
      "--max-line-bytes", String(verifyLimits.maxLineBytes),
      "--verify-max-entries", String(verifyLimits.maxEntries),
    ];
    expect(runCli(["--verify", out, ...verifyArgs])).toBe(EXIT.ok);

    rmSync(join(out, "COMPLETE.json"));
    expect(runCli(["--verify", out, ...verifyArgs])).toBe(EXIT.verify);
  });
});

describe("legacy archive constants", () => {
  test("KNOWN_META_KEYS matches the keys written by the retired service", () => {
    // Keys written by packages/service: store/import.ts (kb_root, imported_at,
    // schema_version) and store/writes.ts (last_review_at).
    expect([...KNOWN_META_KEYS].sort()).toEqual(["imported_at", "kb_root", "last_review_at", "schema_version"]);
  });

  test("ARCHIVE_VERSION is a fixed string, not derived from the tool version", () => {
    expect(ARCHIVE_VERSION).toBe("1");
  });
});

describe("F1: a failed final verification never leaves a completion marker", () => {
  function exportArgs(source: string, registry: string, box: Sandbox, afterMarkerWrite: () => void) {
    return {
      sourceRoot: source,
      outDir: join(box.root, "out"),
      kb: "architecture-review",
      registryPath: registry,
      limits,
      hooks: { afterMarkerWrite },
    };
  }

  test("an error raised after the marker is written removes COMPLETE.json", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    expect(() =>
      exportSourceArchive(
        exportArgs(source, registry, box, () => {
          // Fail exactly in the window this finding describes.
          throw new Error("injected failure after marker write");
        }),
      ),
    ).toThrow(/injected failure after marker write/);
    expect(existsSync(join(out, "COMPLETE.json"))).toBe(false);
    // The manifest, if rewritten, must not still claim completeness.
    if (existsSync(join(out, "archive.json"))) {
      const manifest = JSON.parse(readFileSync(join(out, "archive.json"), "utf8")) as ArchiveManifest;
      expect(manifest.complete).toBe(false);
    }
    // Public verification rejects the directory, with the caller's ceilings.
    // Either failure mode is honest: no marker, or a manifest that no longer
    // claims completeness. What must never happen is a verifier accepting it.
    expect(() => verifySourceArchive(out, verifyLimits)).toThrow(
      /COMPLETE\.json is missing|manifest\.complete is not true/,
    );
  });

  test("tampering with a blob inside the post-marker window is caught and unmarked", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    let injected = false;
    expect(() =>
      exportSourceArchive({
        sourceRoot: source,
        outDir: out,
        kb: "architecture-review",
        registryPath: registry,
        limits,
        hooks: {
          afterMarkerWrite: () => {
            const first = readdirSync(join(out, "blobs"))[0]!;
            const target = join(out, "blobs", first);
            const bytes = readFileSync(target);
            bytes[0] = bytes[0] === 0 ? 1 : 0;
            writeFileSync(target, bytes);
            injected = true;
          },
        },
      }),
    ).toThrow(/blob .* content hash mismatch/);
    expect(injected).toBe(true);
    expect(existsSync(join(out, "COMPLETE.json"))).toBe(false);
    expect(() => verifySourceArchive(out, verifyLimits)).toThrow();
  });

  test("cleanup failure is reported rather than claiming the marker was removed", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    // Make the output directory read-only after the marker exists, so the unlink
    // must fail. The thrown error has to say cleanup was incomplete.
    let madeReadOnly = false;
    let error: unknown = null;
    try {
      exportSourceArchive({
        sourceRoot: source,
        outDir: out,
        kb: "architecture-review",
        registryPath: registry,
        limits,
        hooks: {
          afterMarkerWrite: () => {
            chmodSync(out, 0o500);
            madeReadOnly = true;
            throw new Error("injected failure with unwritable output");
          },
        },
      });
    } catch (err) {
      error = err;
    } finally {
      if (madeReadOnly) chmodSync(out, 0o700);
    }
    expect(error).not.toBeNull();
    expect(String(error)).toMatch(/cleanup was incomplete/);
    expect(String(error)).toMatch(/COMPLETE\.json/);
    // Honest outcome: the marker is genuinely still there, and the report says so.
    expect(existsSync(join(out, "COMPLETE.json"))).toBe(true);
  });
});

describe("F2: verification ceilings come from the caller, never the archive", () => {
  function goodArchive(): { outDir: string; box: Sandbox } {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const outDir = join(box.root, "out");
    exportSourceArchive({ sourceRoot: source, outDir, kb: "architecture-review", registryPath: registry, limits });
    return { outDir, box };
  }

  test("a self-consistent archive declaring enormous limits still fails a small caller ceiling", () => {
    const { outDir } = goodArchive();
    const manifest = JSON.parse(readFileSync(join(outDir, "archive.json"), "utf8")) as ArchiveManifest;
    // Rewrite the archive's own limits to something absurd, and re-sign so the
    // archive stays internally consistent. A verifier that trusted these would
    // now hash anything; the caller's ceiling must still bind.
    manifest.limits = { ...manifest.limits, max_bytes: 1e18, max_entries: 1e18, max_rows: 1e18 };
    manifest.content_digest = manifestContentDigest(manifest);
    writeFileSync(join(outDir, "archive.json"), `${canonicalJson(manifest)}\n`, "utf8");
    // The marker must agree with the new digest, else we test the wrong failure.
    const markerPath = join(outDir, "COMPLETE.json");
    const marker = JSON.parse(readFileSync(markerPath, "utf8")) as Record<string, unknown>;
    marker["content_digest"] = manifest.content_digest;
    writeFileSync(markerPath, `${canonicalJson(marker)}\n`, "utf8");

    expect(() => verifySourceArchive(outDir, { ...verifyLimits, maxArchiveBytes: 64 })).toThrow(
      /maxArchiveBytes=64/,
    );
  });

  test("an archive-controlled multi-gigabyte line fails the caller's line ceiling", () => {
    const { outDir } = goodArchive();
    // Append one oversized record line to the rows stream and re-sign the archive
    // coherently, so only the caller's maxLineBytes can catch it.
    const rowsPath = join(outDir, "files.ndjson");
    const huge = JSON.stringify({ record_type: "source_file", path: "x", bytes: 0, sha256: "0".repeat(64), blob: "blobs/x", mode: 0, mtime_ms: 0, ino: "0" });
    const padded = `${huge}${" ".repeat(4096)}`;
    writeFileSync(rowsPath, `${readFileSync(rowsPath, "utf8")}${padded}\n`, "utf8");
    const manifest = JSON.parse(readFileSync(join(outDir, "archive.json"), "utf8")) as ArchiveManifest;
    const filesStream = manifest.streams.find((s) => s.path === "files.ndjson")!;
    const bytes = readFileSync(rowsPath);
    filesStream.bytes = bytes.length;
    filesStream.records += 1;
    filesStream.sha256 = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
    filesStream.longest_line_bytes = Math.max(filesStream.longest_line_bytes, padded.length + 1);
    manifest.content_digest = manifestContentDigest(manifest);
    writeFileSync(join(outDir, "archive.json"), `${canonicalJson(manifest)}\n`, "utf8");
    const markerPath = join(outDir, "COMPLETE.json");
    const marker = JSON.parse(readFileSync(markerPath, "utf8")) as Record<string, unknown>;
    marker["content_digest"] = manifest.content_digest;
    writeFileSync(markerPath, `${canonicalJson(marker)}\n`, "utf8");

    expect(() => verifySourceArchive(outDir, { ...verifyLimits, maxLineBytes: 1024 })).toThrow(/maxLineBytes=1024/);
  });

  test("maxRecords and maxEntries refuse honestly", () => {
    const { outDir } = goodArchive();
    expect(() => verifySourceArchive(outDir, { ...verifyLimits, maxRecords: 2 })).toThrow(/maxRecords=2/);
    expect(() => verifySourceArchive(outDir, { ...verifyLimits, maxEntries: 2 })).toThrow(/maxEntries=2/);
  });

  test("verify without ceilings is a usage refusal, not a default", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits });
    expect(runCli(["--verify", out])).toBe(EXIT.usage);
    expect(() => parseCli(["--verify", out])).not.toThrow();
  });

  test("non-finite or zero ceilings are usage errors", () => {
    const { outDir } = goodArchive();
    expect(() => verifySourceArchive(outDir, { ...verifyLimits, maxArchiveBytes: 0 })).toThrow(/positive finite integer/);
    expect(() => verifySourceArchive(outDir, { ...verifyLimits, maxLineBytes: Number.POSITIVE_INFINITY })).toThrow(
      /positive finite integer/,
    );
    expect(() => verifySourceArchive(outDir, { ...verifyLimits, maxRecords: 1.5 })).toThrow(/positive finite integer/);
  });
});

describe("F4: inode provenance is exact 64-bit decimal text", () => {
  test("recorded ino is a decimal string equal to bigint lstat", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits });
    const records = readFileSync(join(out, "files.ndjson"), "utf8")
      .split("\n")
      .filter((line) => line !== "")
      .map((line) => JSON.parse(line) as Record<string, unknown>);
    expect(records.length).toBeGreaterThan(0);
    for (const record of records) {
      const rel = String(record["path"]);
      const exact = lstatSync(join(source, rel), { bigint: true }).ino.toString(10);
      expect(typeof record["ino"]).toBe("string");
      expect(record["ino"]).toBe(exact);
      // The recorded value must be the full 64-bit decimal, not a narrowed form:
      // for inodes above 2^53 a double round-trip would differ.
      expect(String(BigInt(String(record["ino"])))).toBe(exact);
    }
  });

  test("verification rejects a non-string ino in a file record", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    const out = join(box.root, "out");
    exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits });
    const filesPath = join(out, "files.ndjson");
    const lines = readFileSync(filesPath, "utf8").split("\n").filter((l) => l !== "");
    const first = JSON.parse(lines[0]!) as Record<string, unknown>;
    first["ino"] = 12345; // the old lossy form
    lines[0] = JSON.stringify(first);
    const body = `${lines.join("\n")}\n`;
    writeFileSync(filesPath, body, "utf8");
    const manifest = JSON.parse(readFileSync(join(out, "archive.json"), "utf8")) as ArchiveManifest;
    const filesStream = manifest.streams.find((s) => s.path === "files.ndjson")!;
    const bytes = readFileSync(filesPath);
    filesStream.bytes = bytes.length;
    filesStream.sha256 = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
    manifest.content_digest = manifestContentDigest(manifest);
    writeFileSync(join(out, "archive.json"), `${canonicalJson(manifest)}\n`, "utf8");
    const markerPath = join(out, "COMPLETE.json");
    const marker = JSON.parse(readFileSync(markerPath, "utf8")) as Record<string, unknown>;
    marker["content_digest"] = manifest.content_digest;
    writeFileSync(markerPath, `${canonicalJson(marker)}\n`, "utf8");
    expect(() => verifySourceArchive(out, verifyLimits)).toThrow(/ino/);
  });
});

describe("F3: file-data scope, not a filesystem snapshot", () => {
  test("empty directories are not records, but are still charged to maxEntries", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    mkdirSync(join(source, "empty-only"));
    const out = join(box.root, "out");
    exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits });
    const files = readArchive(out).files;
    expect(files).not.toContain("empty-only");
    expect(files.some((f) => f.startsWith("empty-only/"))).toBe(false);
    // Documented boundary: the archive is not promised to recreate the directory.
    expect(existsSync(join(out, "blobs"))).toBe(true);
  });
});

describe("schema profile matches the frozen SQLite v1 fixture", () => {
  test("EXPORTED_SCHEMA_PROFILE matches the original schema and user_version", () => {
    const box = sandbox();
    const kbRoot = join(box.root, "kb-live");
    mkdirSync(kbRoot, { recursive: true });
    const db = openLegacyFixture(kbRoot);
    try {
      const listed = db.query("SELECT name FROM pragma_table_list WHERE schema = 'main' AND type = 'table'").all() as Array<{ name: string }>;
      const names = listed
        .map((row) => row.name)
        .filter((name) => !name.startsWith("sqlite_"))
        .sort();
      expect(names).toEqual([...TABLE_ORDER].sort());
      for (const table of TABLE_ORDER) {
        const info = db.query(`PRAGMA table_info("${table}")`).all() as Array<{
          name: string;
          type: string;
          notnull: number;
          pk: number;
        }>;
        const live = info.map((c) => ({
          name: c.name,
          type: String(c.type).toUpperCase(),
          notnull: Number(c.notnull) === 1,
          pk: Number(c.pk),
        }));
        expect(live).toEqual(EXPORTED_SCHEMA_PROFILE[table] as unknown as typeof live);
      }
      const version = db.query<{ user_version: number }, []>("PRAGMA user_version").get();
      expect(Number(version?.user_version ?? 0)).toBe(1);
    } finally {
      db.close();
    }
  });
});

describe("the deployed database is never opened in place", () => {
  test("export leaves every source database file byte-identical", () => {
    const box = sandbox();
    const { source, registry } = fixtureSource(box);
    buildDatabase(source, {
      imported: true,
      rows: (db) => {
        db.run(
          "INSERT INTO concepts (id, atom, layer, domain_json, confidence, source, triggers_json, body, source_path, revision, source_hash, source_revision, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
          ["c1", "a", 1, "[]", "confirmed", "personal", "[]", "b", "p", "r", null, null, "t", "t"],
        );
      },
    });
    const before = new Map<string, string>();
    for (const name of readdirSync(source)) {
      if (name.startsWith("knowledge.db")) before.set(name, sha256File(join(source, name)));
    }
    expect(before.size).toBeGreaterThan(0);

    const out = join(box.root, "out");
    exportSourceArchive({ sourceRoot: source, outDir: out, kb: "architecture-review", registryPath: registry, limits });

    const after = new Map<string, string>();
    for (const name of readdirSync(source)) {
      if (name.startsWith("knowledge.db")) after.set(name, sha256File(join(source, name)));
    }
    // Same set of files, same bytes: the exporter copied, never opened them.
    expect([...after.keys()].sort()).toEqual([...before.keys()].sort());
    for (const [name, hash] of before) expect(after.get(name)).toBe(hash);
  });
});
