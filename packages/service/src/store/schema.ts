import { Database } from "bun:sqlite";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

/**
 * SQLite layout for the structured authority.
 *
 * One file per knowledge base, so two KBs cannot address each other's rows: the
 * database path is derived from the KB root the registry resolved, never from a
 * client-supplied id.
 *
 * Timestamps are ISO strings (not SQLite `datetime`), matching every other
 * timestamp this project emits, so a JSON read-back needs no conversion.
 */
export const SCHEMA_VERSION = 1;

const DDL = `
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS concepts (
  id            TEXT PRIMARY KEY,
  atom          TEXT NOT NULL,
  layer         INTEGER NOT NULL CHECK (layer BETWEEN 0 AND 3),
  domain_json   TEXT NOT NULL,
  confidence    TEXT NOT NULL,
  source        TEXT NOT NULL,
  triggers_json TEXT NOT NULL,
  body          TEXT NOT NULL,
  source_path   TEXT NOT NULL,
  revision      TEXT NOT NULL,
  -- Hash of the Markdown file this row was imported from, and the concept
  -- revision at that moment. A re-import compares both: a changed file against
  -- an unchanged revision is an update, a changed file against a changed
  -- revision is a conflict for a human to resolve.
  source_hash   TEXT,
  source_revision TEXT,
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS relations (
  from_id    TEXT NOT NULL REFERENCES concepts(id) ON DELETE CASCADE,
  type       TEXT NOT NULL,
  target     TEXT NOT NULL,
  confidence TEXT NOT NULL,
  -- Declaration order within the source concept. Preserved so activation and
  -- edge output after migration is byte-identical to the file-backed order
  -- instead of silently re-sorted; a caller diffing two responses would
  -- otherwise see a cutover as a semantic change.
  position   INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  PRIMARY KEY (from_id, type, target)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS relations_target ON relations(target);

CREATE TABLE IF NOT EXISTS claims (
  id         TEXT PRIMARY KEY,
  concept_id TEXT NOT NULL REFERENCES concepts(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL,
  status     TEXT NOT NULL,
  text       TEXT NOT NULL,
  confidence TEXT NOT NULL,
  revision   TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS claims_concept ON claims(concept_id, status);

/*
  Evidence targets are a separate table because one document can support several
  claims, and one claim can be supported by several documents. The ID encoding
  (\`type|id\`) keeps the join cheap while preserving the composite identity.
*/
CREATE TABLE IF NOT EXISTS evidence (
  id              TEXT PRIMARY KEY,
  kind            TEXT NOT NULL,
  title           TEXT NOT NULL,
  locator         TEXT,
  excerpt         TEXT,
  asset_sha256    TEXT,
  asset_media_type TEXT,
  asset_bytes     INTEGER,
  captured_at     TEXT NOT NULL,
  captured_by     TEXT NOT NULL,
  created_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS evidence_targets (
  evidence_id TEXT NOT NULL REFERENCES evidence(id) ON DELETE CASCADE,
  target_type TEXT NOT NULL,
  target_id   TEXT NOT NULL,
  PRIMARY KEY (evidence_id, target_type, target_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS evidence_targets_target ON evidence_targets(target_type, target_id);

/*
  Vectors are a distinct record from relations on purpose: nothing here is
  asserted, and no graph query may join them. \`revision\` is the hash of the
  embedded text; retrieval compares it against the live text and drops rows that
  no longer describe what the text says.
*/
CREATE TABLE IF NOT EXISTS vectors (
  id           TEXT PRIMARY KEY,
  subject_type TEXT NOT NULL,
  subject_id   TEXT NOT NULL,
  model        TEXT NOT NULL,
  dimensions   INTEGER NOT NULL,
  revision     TEXT NOT NULL,
  provider     TEXT,
  values_blob  BLOB NOT NULL,
  norm         REAL NOT NULL,
  created_at   TEXT NOT NULL,
  UNIQUE (subject_type, subject_id, model)
);
CREATE INDEX IF NOT EXISTS vectors_model ON vectors(model, dimensions);

CREATE TABLE IF NOT EXISTS assets (
  sha256     TEXT PRIMARY KEY,
  media_type TEXT NOT NULL,
  byte_size  INTEGER NOT NULL,
  path       TEXT NOT NULL,
  created_at TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS proposals (
  id                TEXT PRIMARY KEY,
  kind              TEXT NOT NULL,
  subject           TEXT NOT NULL,
  expected_revision TEXT NOT NULL,
  payload_json      TEXT NOT NULL,
  reason            TEXT NOT NULL,
  author            TEXT NOT NULL,
  status            TEXT NOT NULL CHECK (status IN ('pending','approved','rejected','conflicted')),
  created_at        TEXT NOT NULL,
  reviewed_at       TEXT,
  reviewer          TEXT,
  review_note       TEXT
);
CREATE INDEX IF NOT EXISTS proposals_status ON proposals(status, created_at);
`;

/** Absolute path of the knowledge database for one KB root. */
export function knowledgeDbFile(kbRoot: string): string {
  return resolve(join(kbRoot, "knowledge.db"));
}

/** Content-addressed asset root for one KB. */
export function assetRoot(kbRoot: string): string {
  return resolve(join(kbRoot, "assets"));
}

export interface OpenStoreOptions {
  /**
   * Create the file and schema when absent.
   *
   * Reads must not create anything: a read against a KB that was never imported
   * has to keep using the Markdown files, not silently leave an empty database
   * that later looks like an authority with zero concepts.
   */
  create?: boolean;
}

/** A database that exists and has finished an explicit import. */
export function storeExists(kbRoot: string): boolean {
  const file = knowledgeDbFile(kbRoot);
  if (!existsSync(file)) return false;
  let db: Database | null = null;
  try {
    // Read-only is not usable on a WAL database without its -shm companion, and
    // the store is always opened read-write anyway; `create: false` still keeps
    // a missing file from being created.
    db = new Database(file, { readwrite: true, create: false });
    const row = db.query<{ value: string }, []>("SELECT value FROM meta WHERE key = 'imported_at'").get();
    return row !== null;
  } catch {
    return false;
  } finally {
    db?.close();
  }
}

export function openStore(kbRoot: string, options: OpenStoreOptions = {}): Database {
  const file = knowledgeDbFile(kbRoot);
  const create = options.create === true;
  if (!create && !existsSync(file)) {
    throw new Error(`No knowledge store at ${file}`);
  }
  const db = new Database(file, { create, readwrite: true });
  db.exec("PRAGMA journal_mode = WAL");
  db.exec("PRAGMA foreign_keys = ON");
  db.exec("PRAGMA busy_timeout = 5000");
  if (create) {
    db.exec(DDL);
    const current = db.query<{ user_version: number }, []>("PRAGMA user_version").get()?.user_version ?? 0;
    if (current < SCHEMA_VERSION) db.exec(`PRAGMA user_version = ${SCHEMA_VERSION}`);
  }
  return db;
}

/** Ensure the parent directory exists before SQLite creates the file. */
export function ensureStoreDir(kbRoot: string): string {
  return dirname(knowledgeDbFile(kbRoot));
}

/** Run `fn` inside a write transaction, rolling back on any throw. */
export function transaction<T>(db: Database, fn: () => T): T {
  db.exec("BEGIN IMMEDIATE");
  try {
    const result = fn();
    db.exec("COMMIT");
    return result;
  } catch (err) {
    try {
      db.exec("ROLLBACK");
    } catch {
      // A failed rollback means the transaction already ended; the original
      // error is the one the caller needs.
    }
    throw err;
  }
}

export function readMeta(db: Database, key: string): string | null {
  return db.query<{ value: string }, [string]>("SELECT value FROM meta WHERE key = ?").get(key)?.value ?? null;
}

export function writeMeta(db: Database, key: string, value: string): void {
  db.run("INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value", [key, value]);
}
