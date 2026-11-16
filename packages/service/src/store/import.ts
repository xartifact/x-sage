import { createHash } from "node:crypto";
import { copyFile, mkdir, readFile, readdir, stat, writeFile } from "node:fs/promises";
import { join, relative } from "node:path";
import type { Database } from "bun:sqlite";
import { parseConceptFile } from "@x-sage/core";
import {
  conceptRecordRevision,
  parseKnowledgeDocument,
  type ConceptRecord,
  type ProjectedClaim,
} from "@x-sage/core/knowledge";
import type { KbContext } from "../kb.ts";
import { refreshConceptRevision } from "./entities.ts";
import { LAYERS } from "../kb.ts";
import { openStore, readMeta, storeExists, transaction, writeMeta, knowledgeDbFile } from "./schema.ts";

/**
 * Explicit, recoverable Markdown → structured import.
 *
 * Three properties this module exists to guarantee:
 *
 * 1. **Nothing implicit.** A KB keeps reading its Markdown files until someone
 *    calls import. Reading never creates a database, so an operator who has not
 *    migrated sees no change at all.
 * 2. **Nothing destroyed.** The concept files stay where they are and a byte
 *    copy is written under `archive/markdown-<timestamp>/` before the first row
 *    exists. Reverting is copying that directory back.
 * 3. **Nothing overwritten silently.** A second import compares the file hash
 *    against the hash recorded at its import. A file that changed while the
 *    authority also moved on is a reported conflict, not a last-writer-wins.
 */

export interface ImportFileOutcome {
  conceptId: string;
  sourcePath: string;
  result: "created" | "updated" | "unchanged" | "conflict" | "invalid";
  detail?: string;
}

export interface ImportReport {
  /** True when this call created the database; false when it added to one. */
  firstImport: boolean;
  backupDir: string | null;
  concepts: ImportFileOutcome[];
  /** Concept files that exist in the database but no longer on disk. */
  missingSources: string[];
  counts: {
    created: number;
    updated: number;
    unchanged: number;
    conflicts: number;
    invalid: number;
  };
  importedAt: string;
}

export class ImportRefusedError extends Error {}
export class ImportConflictError extends Error {
  constructor(
    message: string,
    readonly conflicts: ImportFileOutcome[],
  ) {
    super(message);
  }
}

interface DiskConcept {
  conceptId: string;
  sourcePath: string;
  absolutePath: string;
  raw: string;
  hash: string;
}

async function readDiskConcepts(ctx: KbContext): Promise<{ files: DiskConcept[]; errors: Array<{ sourcePath: string; message: string }> }> {
  const files: DiskConcept[] = [];
  const errors: Array<{ sourcePath: string; message: string }> = [];

  for (const layer of LAYERS) {
    const dir = join(ctx.root, "concepts", layer);
    let entries: string[];
    try {
      entries = await readdir(dir);
    } catch {
      continue;
    }
    for (const name of entries.sort()) {
      if (!name.endsWith(".md")) continue;
      const absolutePath = join(dir, name);
      const sourcePath = relative(ctx.root, absolutePath);
      try {
        const raw = await readFile(absolutePath, "utf-8");
        const parsed = parseConceptFile(raw, sourcePath);
        files.push({
          conceptId: parsed.frontmatter.id,
          sourcePath,
          absolutePath,
          raw,
          hash: createHash("sha256").update(raw).digest("hex"),
        });
      } catch (err) {
        errors.push({ sourcePath, message: err instanceof Error ? err.message : String(err) });
      }
    }
  }

  return { files, errors };
}

/**
 * Copy the Markdown sources aside before the first import.
 *
 * A copy, not a move: the files remain in place so a reviewer can diff what was
 * imported against what was there, and so `git status` shows the migration as an
 * addition rather than a mass deletion.
 */
async function backupMarkdown(ctx: KbContext, stamp: string): Promise<string> {
  const dir = join(ctx.root, "archive", `markdown-${stamp}`);
  const conceptsTarget = join(dir, "concepts");
  await mkdir(conceptsTarget, { recursive: true });

  for (const layer of LAYERS) {
    const source = join(ctx.root, "concepts", layer);
    const entries = await readdir(source).catch(() => [] as string[]);
    if (entries.length === 0) continue;
    await mkdir(join(conceptsTarget, layer), { recursive: true });
    for (const name of entries) {
      if (!name.endsWith(".md")) continue;
      await copyFile(join(source, name), join(conceptsTarget, layer, name));
    }
  }

  for (const name of ["graph.json", "MANIFEST.md"]) {
    const source = join(ctx.root, name);
    const exists = await stat(source).then(() => true).catch(() => false);
    if (exists) await copyFile(source, join(dir, name));
  }

  await writeFile(
    join(dir, "README.md"),
    [
      "# Markdown snapshot before structured import",
      "",
      `Taken at ${stamp} by \`POST /api/knowledge/import\`.`,
      "",
      "The structured authority for this knowledge base is `knowledge.db`; these",
      "files are the pre-import state and are not read by the server any more.",
      "To revert, copy `concepts/` back over the KB's `concepts/` directory and",
      "delete `knowledge.db`.",
      "",
    ].join("\n"),
  );

  return relative(ctx.root, dir);
}

/**
 * Replace `graph.json` / `MANIFEST.md` with a note that the authority moved.
 *
 * Both are derived: regenerating them from Markdown after the import would
 * reintroduce the second truth this migration removes. The stub says where the
 * content went and how to come back.
 */
async function stubDerivedArtifacts(ctx: KbContext, archiveDir: string): Promise<void> {
  const notice = [
    "<!-- x-sage: generated artifact, replaced by a stub at import time -->",
    "",
    "# 本文件已停止使用（structured authority）",
    "",
    `知识库已于导入时切换到结构化权威（\`knowledge.db\`）。`,
    `导入前的原始内容保存在 \`${archiveDir}/\`。`,
    "",
    "本文件与 `graph.json` 不再由服务端读写：`GET /api/manifest` 与 `GET /api/graph`",
    "直接从数据库生成，`PUT` 会返回 409。回滚方式见 `docs/knowledge-store.md`。",
    "",
  ].join("\n");

  // Written even when the file was absent: a reader who opens either path after
  // the migration must learn that the authority moved, not find nothing.
  for (const name of ["MANIFEST.md", "graph.json"]) {
    await writeFile(join(ctx.root, name), notice);
  }
}

interface ConceptFileRow {
  source_hash: string | null;
  source_revision: string | null;
  revision: string;
}

function conceptRow(db: Database, id: string): ConceptFileRow | null {
  return db
    .query<ConceptFileRow, [string]>("SELECT source_hash, source_revision, revision FROM concepts WHERE id = ?")
    .get(id);
}

/**
 * Import (or re-import) one KB.
 *
 * `reimport` must be stated explicitly for an existing store: the first import
 * is the migration, every later one is a merge of two authorities, and only the
 * operator knows whether that is intended.
 */
export async function importKnowledgeBase(
  ctx: KbContext,
  options: { reimport?: boolean } = {},
): Promise<ImportReport> {
  const existed = storeExists(ctx.root);
  if (existed && options.reimport !== true) {
    throw new ImportRefusedError(
      "This knowledge base already has a structured store. Re-import merges Markdown into existing authority; pass reimport=true to state that intent.",
    );
  }

  const { files, errors } = await readDiskConcepts(ctx);
  const importedAt = new Date().toISOString();
  const stamp = importedAt.replace(/[:.]/g, "-");
  const backupDir = existed ? null : await backupMarkdown(ctx, stamp);

  // Derived artifacts are stale the moment the authority moves. Leaving the old
  // bytes in place makes an editor show pre-migration content forever, so both
  // are replaced by a pointer; the pre-import copies are in the archive.
  if (backupDir !== null) await stubDerivedArtifacts(ctx, backupDir);

  await mkdir(join(ctx.root, "assets"), { recursive: true });
  const db = openStore(ctx.root, { create: true });

  const outcomes: ImportFileOutcome[] = errors.map((error) => ({
    conceptId: "",
    sourcePath: error.sourcePath,
    result: "invalid",
    detail: error.message,
  }));

  try {
    transaction(db, () => {
      writeMeta(db, "kb_root", ctx.root);
      writeMeta(db, "imported_at", importedAt);
      writeMeta(db, "schema_version", String(1));

      const seen = new Set<string>();

      for (const file of files) {
        if (seen.has(file.conceptId)) {
          outcomes.push({
            conceptId: file.conceptId,
            sourcePath: file.sourcePath,
            result: "conflict",
            detail: `Duplicate concept id ${file.conceptId}; first definition wins`,
          });
          continue;
        }
        seen.add(file.conceptId);

        const parsed = parseKnowledgeDocument(file.raw);
        const claims: ProjectedClaim[] = parsed.claims;
        // The structured store is one database per KB, so a cross-KB target has
        // nowhere to resolve. Reported per file at migration time rather than as
        // a dangling edge on every later read.
        const crossKb = parsed.frontmatter.edges.find((edge) => edge.target.includes("::"));
        if (crossKb !== undefined) {
          outcomes.push({
            conceptId: file.conceptId,
            sourcePath: file.sourcePath,
            result: "invalid",
            detail: `Cross-KB relation target "${crossKb.target}" is not supported by the structured store (one database per knowledge base)`,
          });
          return;
        }
        const existing = conceptRow(db, file.conceptId);

        const record = {
          id: parsed.frontmatter.id,
          atom: parsed.frontmatter.atom,
          layer: parsed.frontmatter.layer,
          domain: parsed.frontmatter.domain,
          confidence: parsed.frontmatter.confidence,
          source: parsed.frontmatter.source,
          triggers: parsed.frontmatter.triggers,
          body: parsed.body === "" ? `# ${parsed.frontmatter.id}\n` : parsed.body,
          sourcePath: file.sourcePath,
        };
        const revision = conceptRecordRevision(record);

        if (existing === null) {
          insertConcept(db, record, revision, file.hash, importedAt);
          outcomes.push({ conceptId: file.conceptId, sourcePath: file.sourcePath, result: "created" });
        } else if (existing.source_hash === file.hash) {
          outcomes.push({ conceptId: file.conceptId, sourcePath: file.sourcePath, result: "unchanged" });
        } else if (existing.source_revision !== null && existing.revision !== existing.source_revision) {
          // The authority moved on after the last import *and* the file changed.
          // Applying the file would discard whatever the structured side gained.
          // Comparing against the revision recorded *at import* is what makes
          // this decidable; `source_hash` cannot answer it, because a reviewed
          // claim leaves the file untouched.
          outcomes.push({
            conceptId: file.conceptId,
            sourcePath: file.sourcePath,
            result: "conflict",
            detail: "Concept changed in both Markdown and the structured store; resolve by exporting and submitting a reviewed write-back",
          });
          continue;
        } else {
          updateConcept(db, record, revision, file.hash, importedAt);
          outcomes.push({ conceptId: file.conceptId, sourcePath: file.sourcePath, result: "updated" });
        }

        replaceRelations(db, file.conceptId, parsed.frontmatter.edges, importedAt);
        importClaims(db, file.conceptId, claims, importedAt);
        // Store the authority revision (record + accepted claims) and record the
        // same value as the import baseline: the pair is the conflict detector.
        refreshConceptRevision(db, file.conceptId, importedAt);
        db.run("UPDATE concepts SET source_revision = revision WHERE id = ?", [file.conceptId]);
      }

      // A concept whose file disappeared is reported, never deleted: deleting
      // authority because a file was renamed is unrecoverable.
      const onDisk = new Set(files.map((file) => file.conceptId));
      const stored = db
        .query<{ id: string }, []>("SELECT id FROM concepts ORDER BY id")
        .all()
        .map((row) => row.id);
      for (const id of stored) {
        if (!onDisk.has(id)) {
          outcomes.push({
            conceptId: id,
            sourcePath: "",
            result: "conflict",
            detail: "Stored concept has no Markdown source any more; it was kept in the authority",
          });
        }
      }
    });
  } finally {
    db.close();
  }

  const count = (result: ImportFileOutcome["result"]): number =>
    outcomes.filter((outcome) => outcome.result === result).length;

  return {
    firstImport: !existed,
    backupDir,
    concepts: outcomes,
    missingSources: outcomes.filter((outcome) => outcome.sourcePath === "").map((outcome) => outcome.conceptId),
    counts: {
      created: count("created"),
      updated: count("updated"),
      unchanged: count("unchanged"),
      conflicts: count("conflict"),
      invalid: count("invalid"),
    },
    importedAt,
  };
}

function insertConcept(
  db: Database,
  record: Omit<ConceptRecord, "revision" | "createdAt" | "updatedAt">,
  revision: string,
  sourceHash: string,
  now: string,
): void {
  db.run(
    `INSERT INTO concepts (id, atom, layer, domain_json, confidence, source, triggers_json, body, source_path, revision, source_hash, source_revision, created_at, updated_at)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    [
      record.id,
      record.atom,
      record.layer,
      JSON.stringify(record.domain),
      record.confidence,
      record.source,
      JSON.stringify(record.triggers),
      record.body,
      record.sourcePath,
      revision,
      sourceHash,
      revision,
      now,
      now,
    ],
  );
}

function updateConcept(
  db: Database,
  record: Omit<ConceptRecord, "revision" | "createdAt" | "updatedAt">,
  revision: string,
  sourceHash: string,
  now: string,
): void {
  db.run(
    `UPDATE concepts SET atom = ?, layer = ?, domain_json = ?, confidence = ?, source = ?, triggers_json = ?, body = ?, source_path = ?, revision = ?, source_hash = ?, source_revision = ?, updated_at = ?
     WHERE id = ?`,
    [
      record.atom,
      record.layer,
      JSON.stringify(record.domain),
      record.confidence,
      record.source,
      JSON.stringify(record.triggers),
      record.body,
      record.sourcePath,
      revision,
      sourceHash,
      revision,
      now,
      record.id,
    ],
  );
}

function replaceRelations(
  db: Database,
  conceptId: string,
  edges: ReadonlyArray<{ type: string; target: string; confidence: string }>,
  now: string,
): void {
  db.run("DELETE FROM relations WHERE from_id = ?", [conceptId]);
  edges.forEach((edge, position) => {
    db.run(
      "INSERT OR REPLACE INTO relations (from_id, type, target, confidence, position, created_at) VALUES (?, ?, ?, ?, ?, ?)",
      [conceptId, edge.type, edge.target, edge.confidence, position, now],
    );
  });
}

/**
 * Import claims carried in a document's managed block.
 *
 * Claim ids are global, so a duplicate id on a different concept is a conflict
 * that must be visible: silently re-homing it would make one concept's claim
 * disappear from the other's evidence trail.
 */
function importClaims(db: Database, conceptId: string, claims: readonly ProjectedClaim[], now: string): void {
  for (const claim of claims) {
    const owner = db
      .query<{ concept_id: string }, [string]>("SELECT concept_id FROM claims WHERE id = ?")
      .get(claim.id);
    if (owner !== null) {
      if (owner.concept_id === conceptId) continue;
      throw new ImportConflictError(`Claim ${claim.id} is declared under ${owner.concept_id} and ${conceptId}`, []);
    }
    db.run(
      `INSERT INTO claims (id, concept_id, kind, status, text, confidence, revision, created_at, updated_at)
       VALUES (?, ?, ?, 'accepted', ?, ?, '1', ?, ?)`,
      [claim.id, conceptId, claim.kind, claim.text, claim.confidence, now, now],
    );
  }
}

export interface StoreStatus {
  authority: "markdown" | "structured";
  database: string;
  importedAt: string | null;
  counts: {
    concepts: number;
    relations: number;
    claims: number;
    evidence: number;
    vectors: number;
    proposalsPending: number;
  };
}

/** Whether this KB is served from Markdown files or from the structured store. */
export async function storeStatus(ctx: KbContext): Promise<StoreStatus> {
  const structured = storeExists(ctx.root);
  const status: StoreStatus = {
    authority: structured ? "structured" : "markdown",
    database: knowledgeDbFile(ctx.root),
    importedAt: null,
    counts: { concepts: 0, relations: 0, claims: 0, evidence: 0, vectors: 0, proposalsPending: 0 },
  };
  if (!structured) return status;

  const db = openStore(ctx.root);
  try {
    status.importedAt = readMeta(db, "imported_at");
    status.counts = {
      concepts: count(db, "concepts"),
      relations: count(db, "relations"),
      claims: count(db, "claims"),
      evidence: count(db, "evidence"),
      vectors: count(db, "vectors"),
      proposalsPending: db
        .query<{ n: number }, []>("SELECT COUNT(*) AS n FROM proposals WHERE status = 'pending'")
        .get()!.n,
    };
  } finally {
    db.close();
  }
  return status;
}

function count(db: Database, table: "concepts" | "relations" | "claims" | "evidence" | "vectors"): number {
  return db.query<{ n: number }, []>(`SELECT COUNT(*) AS n FROM ${table}`).get()!.n;
}
