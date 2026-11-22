-- Frozen SQLite v1 DDL from ffe2e85:packages/service/src/store/schema.ts.
-- Used only to exercise the offline source archive reader.
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
  (`type|id`) keeps the join cheap while preserving the composite identity.
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
  asserted, and no graph query may join them. `revision` is the hash of the
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
PRAGMA user_version = 1;
