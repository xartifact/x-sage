import type { Database } from "bun:sqlite";
import type { KbContext } from "./kb.ts";
import { openStore, storeExists } from "./store/schema.ts";

/**
 * Which record is authoritative for a KB.
 *
 * `markdown` — the pre-structured behaviour: concept files are the truth.
 * `structured` — `knowledge.db` is the truth; Markdown is an export.
 *
 * The switch is the existence of a database that has completed an explicit
 * import. Nothing flips it implicitly: reading a KB never creates one, and the
 * import endpoint is the only writer that does. Deleting `knowledge.db` returns
 * the KB to file authority, which is the documented rollback.
 */
export type Authority = "markdown" | "structured";

export function authorityOf(ctx: KbContext): Authority {
  return storeExists(ctx.root) ? "structured" : "markdown";
}

export function isStructured(ctx: KbContext): boolean {
  return authorityOf(ctx) === "structured";
}

/**
 * Open the store for one unit of work.
 *
 * The connection is opened per call and closed in `finally`: a long-lived global
 * would keep a deleted database file alive after a rollback and would serve one
 * KB's rows to another if a context were ever reused.
 */
export function withStore<T>(ctx: KbContext, fn: (db: Database) => T): T {
  const db = openStore(ctx.root);
  try {
    return fn(db);
  } finally {
    db.close();
  }
}

export class MarkdownAuthorityError extends Error {
  constructor(message: string) {
    super(message);
  }
}

/**
 * Refuse a write that targets the Markdown files of a structured KB.
 *
 * The message names the replacement endpoint: an operator who hits this has a
 * working client written against the old API, and "conflict" alone would leave
 * them guessing which of the new endpoints to call.
 */
export function refuseMarkdownWrite(action: string): never {
  throw new MarkdownAuthorityError(
    `${action} writes the Markdown files, but this knowledge base is served from the structured store. ` +
      "Submit the change as a reviewable proposal instead: POST /api/knowledge/proposals " +
      "(kind=concept-markdown for a document edit, kind=claim-create|claim-edit|evidence-attach|relation-assert otherwise).",
  );
}
