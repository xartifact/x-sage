#!/usr/bin/env bun
import { writeSync } from "node:fs";
import type { ApiMethod, ApiResponse } from "@x-sage/service";
import { configPath, resolveTarget } from "./config.ts";
import { formatVersion, readCliBuildInfo } from "./build-info.ts";

/**
 * x-sage CLI — a REST client.
 *
 * The server is the only component that reads the knowledge base. This file
 * translates argv into one HTTP request and writes the
 * response body to stdout; text is rendered server-side, so the client needs
 * no graph, no schema and no kb files.
 *
 * There is no local mode. Point it at a deployment with `--server` or
 * `X_SAGE_SERVER`, and supply `--token`/`X_SAGE_TOKEN` when the server
 * requires one for writes.
 */

/**
 * Write bytes to a stream and guarantee they are flushed before exit.
 *
 * This is not defensive noise: it fixed an intermittent truncation where a
 * response body larger than the pipe buffer arrived partially written while the
 * process reported success.
 *
 * `process.exit()` terminates the process without draining a pipe: when stdout
 * is a pipe (a test harness, `| head`, a CI log capture) a large body could be
 * truncated at the buffer boundary, so a caller saw a partial response and a
 * zero exit code — an intermittent, load-dependent failure. Writing
 * synchronously to the file descriptor leaves nothing buffered to lose, and
 * `process.exitCode` lets node flush naturally when the fd is a pipe.
 */
function writeAll(stream: "out" | "err", text: string): void {
  const fd = stream === "out" ? 1 : 2;
  const buffer = Buffer.from(text, "utf8");
  let offset = 0;
  while (offset < buffer.length) {
    try {
      offset += writeSync(fd, buffer, offset, buffer.length - offset);
    } catch (err) {
      // EAGAIN on a non-blocking pipe: the reader has not drained yet.
      if (err instanceof Error && "code" in err && (err.code === "EAGAIN" || err.code === "EWOULDBLOCK")) {
        continue;
      }
      // EPIPE: the reader closed (e.g. `| head`). Nothing left to do.
      return;
    }
  }
}

/** Write the response body out and exit with a matching status. */
function emit(response: ApiResponse, stream?: "out" | "err"): never {
  const failed = response.status >= 400;
  const target = stream ?? (failed ? "err" : "out");
  writeAll(target, response.body);
  process.exit(failed ? 1 : 0);
}

function emitJsonError(message: string): never {
  writeAll("err", `${message}\n`);
  process.exit(1);
}

/** Usage text is informational, but the invocation still failed. */
function emitUsageError(message: string): never {
  writeAll("out", `${message}\n`);
  process.exit(1);
}

/**
 * Content type from magic bytes, so `knowledge asset` sends what the file is
 * rather than what its extension claims. The server re-checks both anyway.
 */
function sniffMediaType(bytes: Uint8Array): string | undefined {
  const starts = (...signature: number[]): boolean => signature.every((byte, index) => bytes[index] === byte);
  if (starts(0x89, 0x50, 0x4e, 0x47)) return "image/png";
  if (starts(0xff, 0xd8, 0xff)) return "image/jpeg";
  if (starts(0x47, 0x49, 0x46, 0x38)) return "image/gif";
  if (starts(0x52, 0x49, 0x46, 0x46)) return "image/webp";
  return undefined;
}

/** Split argv into `--flag value`, bare `--flag`, and positionals. */
function parseArgs(args: string[]): {
  flags: Record<string, string>;
  positional: string[];
} {
  const flags: Record<string, string> = {};
  const positional: string[] = [];

  for (let i = 0; i < args.length; i++) {
    const arg = args[i]!;
    if (arg.startsWith("--")) {
      const key = arg.slice(2);
      const next = args[i + 1];
      if (next !== undefined && !next.startsWith("--")) {
        flags[key] = next;
        i++;
      } else {
        flags[key] = "true";
      }
    } else {
      positional.push(arg);
    }
  }

  return { flags, positional };
}

function requirePositional(value: string | undefined, usage: string): string {
  if (value === undefined || value === "") emitJsonError(usage);
  return value;
}

/** Map a CLI invocation onto the shared request shape. */
const nativeService = "/sage.v1.SageCommandService/";
const nativeMethods = {
  handshake: "Handshake",
  submit: "SubmitProposal",
  review: "ReviewProposal",
  commit: "CommitCommand",
  outcome: "GetRequestOutcome",
} as const;

interface Invocation {
  method: ApiMethod;
  path: string;
  query: Record<string, string | undefined>;
  body?: unknown;
  nativeMethod?: keyof typeof nativeMethods;
  requestFile?: string;
  requestBytes?: ArrayBuffer;
  /**
   * Print the body on stdout even when the status is an error.
   *
   * `validate` reports findings as its normal output and signals failure with
   * the exit code; sending the report to stderr would hide it from a pipe.
   */
  reportToStdout?: boolean;
  /**
   * Local path (or `-` for stdin) whose contents become `body.proposedBody`.
   *
   * The body is the user's own material, not KB data, so reading it locally
   * does not make this a local mode: nothing about the KB is inspected.
   */
  readBodyFrom?: string;
  /**
   * Local file (or `-`) whose contents become part of the request body.
   *
   * Same rule as `readBodyFrom`: the file is the caller's own material. The
   * variants differ only in how it is encoded — JSON parsed, Markdown verbatim,
   * or base64 for an image.
   */
  readPayloadFrom?: Record<string, string>;
}

/**
 * Translate `command args --flags` into an API request.
 *
 * This is the only piece of knowledge the CLI holds about the API: which
 * command maps to which path. Everything else — validation, wording,
 * formatting — lives in the dispatcher.
 */
function invoke(command: string, args: string[], stdinCheckpoint?: unknown): Invocation {
  const { flags, positional } = parseArgs(args);
  const q: Record<string, string | undefined> = {};

  // Text is the CLI's native output format; JSON stays available via --json so
  // pipelines that parse the response keep working.
  if (flags.json === "true") q.format = "json";

  switch (command) {
    case "manifest": {
      if (flags.write === "true") {
        return { method: "PUT", path: "/manifest", query: { format: "text" } };
      }
      return { method: "GET", path: "/manifest", query: { format: flags.json === "true" ? "json" : "text" } };
    }

    case "search": {
      const keyword = requirePositional(positional[0], "Usage: x-sage search <keyword> [--format manifest] [--json]");
      q.format = flags.format === "manifest" ? "manifest" : flags.json === "true" ? "json" : "text";
      return { method: "GET", path: "/search", query: { ...q, q: keyword } };
    }

    case "activate": {
      const conceptId = requirePositional(
        positional[0],
        "Usage: x-sage activate <concept-id> [--format ai|human] [--depth N] [--strategy exploration|validation|generation]",
      );
      q.format = flags.format === "human" ? "markdown" : flags.json === "true" ? "json" : "text";
      if (flags.depth !== undefined) q.depth = flags.depth;
      if (flags.strategy !== undefined) q.strategy = flags.strategy;
      return { method: "GET", path: `/concepts/${encodeURIComponent(conceptId)}`, query: q };
    }

    case "path": {
      const from = requirePositional(positional[0], "Usage: x-sage path <from-id> <to-id>");
      const to = requirePositional(positional[1], "Usage: x-sage path <from-id> <to-id>");
      if (flags.json !== "true") q.format = "text";
      return { method: "GET", path: "/path", query: { ...q, from, to } };
    }

    case "edges": {
      const conceptId = requirePositional(
        positional[0],
        "Usage: x-sage edges <concept-id> [--type T] [--dir in|out|both] [--derived] [--json]",
      );
      if (flags.json !== "true") q.format = "text";
      if (flags.type !== undefined) q.type = flags.type;
      if (flags.dir !== undefined) q.dir = flags.dir;
      if (flags.derived === "true") q.derived = "true";
      return { method: "GET", path: `/concepts/${encodeURIComponent(conceptId)}/edges`, query: q };
    }

    case "validate": {
      q.graph = flags.graph === "true" ? "true" : undefined;
      if (flags.json !== "true") q.format = "text";
      return { method: "GET", path: "/validate", query: q, reportToStdout: true };
    }

    case "frontier": {
      const subcmd = positional[0];
      if (subcmd === "add") {
        const usage =
          'Usage: x-sage frontier add <concept> --source "where this came from" --reason "what is unresolved" [--priority high|medium|low]';
        const concept = requirePositional(positional[1], usage);
        return {
          method: "POST",
          path: "/frontier",
          query: { format: flags.json === "true" ? "json" : "text" },
          body: {
            concept,
            source: requirePositional(flags.source, usage),
            reason: requirePositional(flags.reason, usage),
            ...(flags.priority === undefined ? {} : { priority: flags.priority }),
          },
        };
      }
      return {
        method: "GET",
        path: "/frontier",
        query: { format: flags.json === "true" ? "json" : "text" },
      };
    }

    case "native": {
      const subcmd = positional[0];
      if (subcmd === "checkpoint") {
        const id = requirePositional(positional[1], "Usage: x-sage native checkpoint <concept-id> [session-number]");
        return {
          method: "GET",
          path: `/knowledge/checkpoints/${encodeURIComponent(id)}`,
          query: { session: positional[2] },
        };
      }

      if (subcmd && Object.hasOwn(nativeMethods, subcmd)) {
        const method = subcmd as keyof typeof nativeMethods;
        return {
          method: "POST",
          path: nativeService + nativeMethods[method],
          query: {},
          nativeMethod: method,
          requestFile: requirePositional(positional[1], `Usage: x-sage native ${method} <request.json|->`),
        };
      }
      emitJsonError("Usage: x-sage native checkpoint <concept-id> [session] | native handshake|submit|review|commit|outcome <request.json|->");
    }

    case "checkpoint": {
      const subcmd = positional[0];
      const conceptId = positional[1];

      if (subcmd === "create") {
        const id = requirePositional(conceptId, "Usage: x-sage checkpoint create <concept-id> [session-number]");
        const session = positional[2] === undefined ? undefined : Number.parseInt(positional[2], 10);
        if (session !== undefined && (Number.isNaN(session) || session < 1)) {
          emitJsonError("Session number must be a positive integer");
        }
        return {
          method: "POST",
          path: `/checkpoints/${encodeURIComponent(id)}`,
          query: {},
          body: session === undefined ? {} : { sessionNumber: session },
        };
      }

      if (subcmd === "read") {
        const id = requirePositional(conceptId, "Usage: x-sage checkpoint read <concept-id> [session-number]");
        const session = positional[2];
        return {
          method: "GET",
          path: `/checkpoints/${encodeURIComponent(id)}`,
          query: session === undefined ? {} : { session },
        };
      }

      if (subcmd === "save") {
        const checkpoint = stdinCheckpoint;
        if (checkpoint === null || typeof checkpoint !== "object" || !("concept" in checkpoint)) {
          emitJsonError("Usage: x-sage checkpoint save - < checkpoint.json   (reads stdin)");
        }
        const concept = String((checkpoint as { concept: unknown }).concept);
        return {
          method: "PUT",
          path: `/checkpoints/${encodeURIComponent(concept)}`,
          query: {},
          body: checkpoint,
        };
      }

      emitJsonError(`Checkpoint subcommands:
  checkpoint create <concept-id> [session]   Create an empty checkpoint skeleton
  checkpoint read <concept-id> [session]     Read a checkpoint (defaults to latest)
  checkpoint save -                          Write a checkpoint read from stdin
`);
    }

    case "graph": {
      const subcmd = positional[0];
      if (subcmd === "generate") {
        const manifest = flags.manifest === "true" ? { manifest: true } : {};
        return { method: "PUT", path: "/graph", query: { format: "text" }, body: manifest };
      }
      return {
        method: "GET",
        path: "/validate",
        query: { graph: "true", format: "graph" },
        reportToStdout: true,
      };
    }

    case "propose-edge": {
      const from = requirePositional(positional[0], "Usage: x-sage propose-edge <from> <type> <target> [--confidence C]");
      const type = requirePositional(positional[1], "Usage: x-sage propose-edge <from> <type> <target> [--confidence C]");
      const target = requirePositional(positional[2], "Usage: x-sage propose-edge <from> <type> <target> [--confidence C]");
      return {
        method: "POST",
        path: "/proposals",
        query: { format: "text" },
        body: {
          kind: "edge",
          from,
          type,
          target,
          ...(flags.confidence === undefined ? {} : { confidence: flags.confidence }),
        },
      };
    }

    case "refine-atom": {
      const conceptId = requirePositional(
        positional[0],
        'Usage: x-sage refine-atom <concept-id> --atom "new atom text" [--reason "reason"]',
      );
      const proposedAtom = requirePositional(
        flags.atom,
        'Usage: x-sage refine-atom <concept-id> --atom "new atom text" [--reason "reason"]',
      );
      return {
        method: "POST",
        path: "/proposals",
        query: { format: "text" },
        body: {
          kind: "atom",
          conceptId,
          proposedAtom,
          ...(flags.reason === undefined ? {} : { reason: flags.reason }),
        },
      };
    }

    case "refine-body": {
      const conceptId = requirePositional(
        positional[0],
        'Usage: x-sage refine-body <concept-id> --file <path|-> --expected-revision <rev> --reason "why"',
      );
      const source = requirePositional(
        flags.file,
        'Usage: x-sage refine-body <concept-id> --file <path|-> --expected-revision <rev> --reason "why"',
      );
      const expectedRevision = requirePositional(
        flags["expected-revision"],
        'Usage: x-sage refine-body <concept-id> --file <path|-> --expected-revision <rev> --reason "why"',
      );
      const reason = requirePositional(
        flags.reason,
        'Usage: x-sage refine-body <concept-id> --file <path|-> --expected-revision <rev> --reason "why"',
      );
      return {
        method: "POST",
        path: "/proposals",
        query: { format: flags.json === "true" ? "json" : "text" },
        body: { kind: "body", conceptId, expectedRevision, reason, proposedBody: "" },
        // Filled in by main() before the request: the body is a local file.
        readBodyFrom: source,
      };
    }

    case "concept": {
      const subcmd = positional[0];
      if (subcmd === "body") {
        const conceptId = requirePositional(
          positional[1],
          "Usage: x-sage concept body <concept-id> [--format json|markdown] [--json]",
        );
        q.format =
          flags.json === "true" ? "json" : flags.format === "markdown" || flags.format === "human" ? "markdown" : "json";
        return { method: "GET", path: `/concepts/${encodeURIComponent(conceptId)}/body`, query: q };
      }
      emitJsonError("Usage: x-sage concept body <concept-id> [--format json|markdown]");
    }

    case "knowledge": {
      return knowledgeInvocation(args, flags, positional, q);
    }

    case "proposals": {
      const subcmd = positional[0];
      if (subcmd === "show") {
        const file = requirePositional(positional[1], "Usage: x-sage proposals show <proposal-file> [--format diff|json]");
        return {
          method: "GET",
          path: `/proposals/${encodeURIComponent(file)}`,
          query: { format: flags.json === "true" ? "json" : "text" },
          reportToStdout: true,
        };
      }
      if (subcmd === "merge") {
        return {
          method: "POST",
          path: "/proposals/merge",
          query: { format: "text" },
          body: { file: positional[1] ?? "all" },
        };
      }
      return { method: "GET", path: "/proposals", query: { format: "text" } };
    }

    case "viz": {
      const subcmd = positional[0];
      if (subcmd === "stats" || subcmd === undefined) {
        return { method: "GET", path: "/viz", query: { format: "text" } };
      }
      // The full payload is JSON; the CLI writes it wherever the caller asked.
      // No default path: `public/` no longer exists, and a stray artifact in the
      // repo would be a stale copy of the truth.
      const output = positional[1];
      if (output === undefined) {
        emitJsonError("Usage: x-sage viz export <path>   (the web UI reads /api/viz; no default path)");
      }
      return { method: "GET", path: "/viz", query: { format: "json", export: output } };
    }

    default:
      // Unknown command is a usage error, not a successful invocation.
      emitUsageError(usage());
  }
}

/**
 * `x-sage knowledge …` — the structured-authority surface.
 *
 * Every subcommand is one REST request; the CLI still reads no KB data. Where a
 * request needs a body built from local files (`propose --payload`, `document
 * edit`), the file is the caller's own material, not KB state.
 */
function knowledgeInvocation(
  args: string[],
  flags: Record<string, string>,
  positional: string[],
  q: Record<string, string | undefined>,
): Invocation {
  const sub = positional[0];
  const text = flags.json === "true" ? "json" : "text";

  if (sub === "status") {
    return { method: "GET", path: "/knowledge/status", query: { format: "json" } };
  }

  if (sub === "import") {
    return {
      method: "POST",
      path: "/knowledge/import",
      query: { format: text },
      body: { mode: "authority", ...(flags.reimport === "true" ? { reimport: true } : {}) },
    };
  }

  if (sub === "concepts") {
    return { method: "GET", path: "/knowledge/concepts", query: { format: "json" } };
  }

  if (sub === "document") {
    const conceptId = requirePositional(
      positional[1],
      "Usage: x-sage knowledge document <concept-id> [--format markdown|json]",
    );
    return {
      method: "GET",
      path: `/knowledge/concepts/${encodeURIComponent(conceptId)}/document`,
      query: {
        format: flags.json === "true" ? "json" : flags.format === "json" ? "json" : "markdown",
      },
    };
  }

  if (sub === "claims") {
    const conceptId = requirePositional(positional[1], "Usage: x-sage knowledge claims <concept-id>");
    return {
      method: "GET",
      path: `/knowledge/concepts/${encodeURIComponent(conceptId)}/claims`,
      query: { format: "json" },
    };
  }

  if (sub === "evidence") {
    const target = requirePositional(
      positional[1],
      "Usage: x-sage knowledge evidence <concept-id> | x-sage knowledge evidence claim:<claim-id>",
    );
    if (target.startsWith("claim:")) {
      const claimId = target.slice("claim:".length);
      return {
        method: "GET",
        path: `/knowledge/claims/${encodeURIComponent(claimId)}/evidence`,
        query: { format: "json" },
      };
    }
    return {
      method: "GET",
      path: `/knowledge/concepts/${encodeURIComponent(target)}/evidence`,
      query: { format: "json" },
    };
  }

  if (sub === "propose") {
    const kind = requirePositional(
      flags.kind,
      "Usage: x-sage knowledge propose --kind <claim-create|claim-edit|evidence-attach|relation-assert|concept-markdown> --subject <id> --expected-revision <rev> --reason <why> --author <id> (--payload <json-file|-> | --document <md-file|->)",
    );
    const subject = requirePositional(flags.subject, "Missing --subject");
    const expectedRevision = requirePositional(flags["expected-revision"], "Missing --expected-revision");
    const reason = requirePositional(flags.reason, "Missing --reason");
    const author = requirePositional(flags.author, "Missing --author");
    const payloadFile = flags.payload;
    const documentFile = flags.document;
    if (payloadFile === undefined && documentFile === undefined) {
      emitJsonError("Provide --payload <json-file|-> or --document <markdown-file|->");
    }
    return {
      method: "POST",
      path: "/knowledge/proposals",
      query: { format: text },
      body: { kind, subject, expectedRevision, reason, author, payload: {} },
      // Filled by main(): the payload is the caller's local file.
      readPayloadFrom: payloadFile === undefined ? { document: documentFile! } : { payload: payloadFile },
    };
  }

  if (sub === "revisions") {
    const kind = requirePositional(
      flags.kind,
      "Usage: x-sage knowledge revisions --kind <proposal-kind> --subject <concept-id>",
    );
    const subject = requirePositional(flags.subject, "Missing --subject");
    return {
      method: "GET",
      path: "/knowledge/revisions",
      query: { format: "json", kind, subject },
    };
  }

  if (sub === "proposals") {
    const status = flags.status;
    return {
      method: "GET",
      path: "/knowledge/proposals",
      query: { format: text, ...(status === undefined ? {} : { status }) },
    };
  }

  if (sub === "review") {
    const id = requirePositional(positional[1], "Usage: x-sage knowledge review <proposal-id> --decision approve|reject --reviewer <id> [--note <text>]");
    const decision = requirePositional(flags.decision, "Missing --decision approve|reject");
    if (decision !== "approve" && decision !== "reject") emitJsonError("--decision must be approve or reject");
    const reviewer = requirePositional(flags.reviewer, "Missing --reviewer");
    return {
      method: "POST",
      path: `/knowledge/proposals/${encodeURIComponent(id)}/review`,
      query: { format: text },
      body: { decision, reviewer, ...(flags.note === undefined ? {} : { note: flags.note }) },
    };
  }

  if (sub === "search") {
    const query = requirePositional(
      positional[1],
      "Usage: x-sage knowledge search <text> [--limit N] [--vector <json-file|->]",
    );
    return {
      method: "POST",
      path: "/knowledge/search",
      query: { format: "json" },
      body: { query, ...(flags.limit === undefined ? {} : { limit: Number.parseInt(flags.limit, 10) }) },
      ...(flags.vector === undefined ? {} : { readPayloadFrom: { vector: flags.vector } }),
    };
  }

  if (sub === "vectors") {
    const file = requirePositional(
      positional[1],
      "Usage: x-sage knowledge vectors <vectors.json|->   (one { vectors: [...] } document)",
    );
    return {
      method: "POST",
      path: "/knowledge/vectors",
      query: { format: "json" },
      body: {},
      readPayloadFrom: { vectors: file },
    };
  }

  if (sub === "asset") {
    const file = requirePositional(
      positional[1],
      "Usage: x-sage knowledge asset <image-file>   (uploads and prints the content address)",
    );
    return {
      method: "POST",
      path: "/knowledge/assets",
      query: { format: "json" },
      body: {},
      readPayloadFrom: { asset: file },
    };
  }

  emitJsonError(`Knowledge subcommands:
  knowledge status                      Authority and record counts
  knowledge import [--reimport]         Migrate Markdown into the structured store
  knowledge concepts                    List structured concepts
  knowledge document <id> [--format markdown|json]
  knowledge claims <id>                 Accepted claims with their evidence
  knowledge evidence <concept-id|claim:<claim-id>>
  knowledge propose --kind K --subject S --expected-revision R --reason W --author A
                    (--payload <json-file|-> | --document <md-file|->)
  knowledge revisions --kind K --subject S
  knowledge proposals [--status pending|approved|rejected|conflicted]
  knowledge review <proposal-id> --decision approve|reject --reviewer <id> [--note <text>]
  knowledge search <text> [--limit N] [--vector <json-file|->]
  knowledge vectors <file|->            Upload externally computed embeddings
  knowledge asset <image-file>          Store an image, print its content address
`);
}

function usage(): string {
  return `x-sage CLI  (renders server-side; the client holds no kb data)

Usage: x-sage <command> [args] [flags]

Connection:
  --server <url>                        Server base URL
  --token <token>                       Bearer token for an authenticated server

  Resolution: flags > environment > config file.
    X_SAGE_SERVER / X_SAGE_TOKEN        environment
    ~/.config/x-sage/config.toml        server = "...", token = "..."
                                        (honours XDG_CONFIG_HOME)

Native Rust service:
  native checkpoint <concept-id> [session-number]
  native handshake|submit|review|commit|outcome <request.json|->

Every command makes one HTTP request. The client holds no KB data. Native
commands send the caller's request bytes directly. Exit 2 means a native
attempt or outcome was inconclusive; look up the original request id.

Read:
  manifest [--write]                    Render MANIFEST.md; --write persists it
  search <keyword> [--format manifest] [--json]
  activate <id> [--format ai|human] [--depth N] [--strategy S]
                                        S = exploration|validation|generation
  path <from-id> <to-id>
  edges <id> [--type T] [--dir in|out|both] [--derived] [--json]

Validate:
  validate [--graph] [--json]           Files; --graph adds integrity + staleness

Structured knowledge (see docs/knowledge-store.md):
  knowledge status | import [--reimport] | concepts | document <id>
  knowledge claims <id> | evidence <concept-id|claim:id>
  knowledge propose --kind K --subject S --expected-revision R --reason W --author A
  knowledge proposals [--status S] | review <proposal-id> --decision approve|reject --reviewer <id>
  knowledge revisions --kind K --subject S    Baseline the server will accept for that proposal
  knowledge search <text> [--limit N] [--vector <file|->]
  knowledge vectors <file|-> | asset <image-file>

Write-back (needs --token when the server requires one):
  concept body <id> [--format json|markdown]   Read the body + revision
  propose-edge <from> <type> <target> [--confidence C]
  refine-atom <concept-id> --atom "text" [--reason "text"]
  refine-body <id> --file <path|-> --expected-revision <rev> --reason "why"
  proposals list | proposals show <file> | proposals merge [file|all]
  frontier list | frontier add <concept> --source "..." --reason "..." [--priority P]
  checkpoint create <id> [session] | read <id> [session] | save -   (save reads stdin)

Graph:
  graph generate [--manifest]           Rebuild graph.json (+ MANIFEST.md)

Viz:
  viz stats | viz export <path>

Meta:
  --version | version                  Print CLI version + build commit (local, no server)
`;
}

/** Run the invocation against a deployed server. */
async function runRemote(
  server: string,
  invocation: Invocation,
  accept: string,
  token: string | undefined,
): Promise<void> {
  const stream = invocation.reportToStdout === true ? "out" : undefined;
  const base = server.endsWith("/") ? server.slice(0, -1) : server;
  const url = new URL(`${base}${invocation.nativeMethod ? "" : "/api"}${invocation.path}`);
  const mutations = invocation.nativeMethod === "submit" ||
    invocation.nativeMethod === "review" || invocation.nativeMethod === "commit";
  for (const [key, value] of Object.entries(invocation.query)) {
    if (value !== undefined) url.searchParams.set(key, value);
  }

  const headers: Record<string, string> = { accept };
  if (token !== undefined) headers.authorization = `Bearer ${token}`;
  const outbound = invocation.requestBytes ??
    (invocation.body === undefined ? undefined : JSON.stringify(invocation.body));
  if (outbound !== undefined) headers["content-type"] = "application/json";

  let response: Response;
  try {
    response = await fetch(url, {
      method: invocation.method,
      headers,
      ...(invocation.nativeMethod ? { redirect: "error" as const } : {}),
      ...(outbound === undefined ? {} : { body: outbound }),
    });
  } catch (err) {
    const detail = err instanceof Error ? err.message : String(err);
    if (mutations) {
      writeAll("err", `Request result unconfirmed at ${url.origin}. Look up the original request id.\n`);
      process.exit(2);
    }
    emitJsonError(`cannot reach ${url.origin}: ${detail}`);
  }

  let body: string;
  try {
    body = await response.text();
  } catch (error) {
    if (mutations) {
      writeAll("err", "Request response incomplete. Look up the original request id.\n");
      process.exit(2);
    }
    throw error;
  }

  if (invocation.nativeMethod && response.ok) {
    let reply;
    try {
      reply = JSON.parse(body);
    } catch {
      if (mutations) {
        writeAll("err", "Request response unreadable. Look up the original request id.\n");
        process.exit(2);
      }
      emitJsonError("Native service response is not JSON");
    }
    if (reply?.error || reply?.outcome?.error) {
      writeAll("err", body);
      process.exit(1);
    }
    if ((mutations && !reply?.outcome?.committed?.recorded && !reply?.outcome?.alreadyCommitted?.recorded) ||
        (invocation.nativeMethod === "outcome" && !reply?.found?.recorded && !reply?.absent)) {
      writeAll("out", body);
      process.exit(2);
    }
  } else if (mutations && (response.status === 408 || response.status >= 500)) {
    writeAll("err", body);
    process.exit(2);
  }

  // `viz export` writes the payload to a file instead of stdout.
  const exportPath = invocation.query.export;
  if (exportPath !== undefined && response.ok) {
    await Bun.write(exportPath, body);
    writeAll("out", `Exported viz payload → ${exportPath}\n`);
    process.exit(0);
  }

  emit({ status: response.status, contentType: response.headers.get("content-type") ?? "", body }, stream);
}

async function main(): Promise<void> {
  const allArgs = process.argv.slice(2);

  // Global flags are stripped before the command is dispatched.
  const takeFlag = (name: string): string | undefined => {
    const index = allArgs.indexOf(`--${name}`);
    if (index === -1) return undefined;
    const value = allArgs[index + 1];
    allArgs.splice(index, value === undefined || value.startsWith("--") ? 1 : 2);
    return value;
  };

  const serverFlag = takeFlag("server");
  const tokenFlag = takeFlag("token");

  // `--version` / `version` answer locally: no server, no config, no network.
  // A version probe that needs configuration cannot diagnose a broken config.
  if (allArgs[0] === "--version" || allArgs[0] === "version") {
    writeAll("out", `${formatVersion(readCliBuildInfo())}\n`);
    process.exit(0);
  }

  const command = allArgs[0];
  if (command === undefined) emitUsageError(usage());

  // `checkpoint save -` streams a checkpoint in: a client cannot read a
  // server-side path, so stdin is the only transfer that works remotely.
  let stdinCheckpoint: unknown;
  if (command === "checkpoint" && allArgs[1] === "save" && allArgs[2] === "-") {
    const raw = await new Response(Bun.stdin.stream()).text();
    try {
      stdinCheckpoint = JSON.parse(raw);
    } catch {
      emitJsonError("checkpoint save: stdin is not valid JSON");
    }
  }

  const invocation = invoke(command, allArgs.slice(1), stdinCheckpoint);

  if (invocation.requestFile !== undefined) {
    const file = invocation.requestFile;
    invocation.requestBytes = file === "-"
      ? await new Response(Bun.stdin.stream()).arrayBuffer()
      : await Bun.file(file).arrayBuffer();
    if (invocation.requestBytes.byteLength === 0) emitJsonError("Native request is empty");
  }

  if (invocation.readPayloadFrom !== undefined) {
    for (const [target, source] of Object.entries(invocation.readPayloadFrom)) {
      const raw =
        source === "-"
          ? await new Response(Bun.stdin.stream()).arrayBuffer()
          : await Bun.file(source).arrayBuffer().catch((err: unknown) => {
              emitJsonError(`cannot read ${source}: ${err instanceof Error ? err.message : String(err)}`);
            });
      const bytes = new Uint8Array(raw);
      if (bytes.byteLength === 0) emitJsonError(`${source} is empty`);

      if (target === "asset") {
        const base64 = Buffer.from(bytes).toString("base64");
        const mediaType = sniffMediaType(bytes);
        invocation.body = {
          ...(invocation.body as Record<string, unknown>),
          data: base64,
          ...(mediaType === undefined ? {} : { mediaType }),
        };
        continue;
      }

      const text = Buffer.from(bytes).toString("utf8");
      if (target === "document") {
        invocation.body = { ...(invocation.body as Record<string, unknown>), payload: { document: text } };
        continue;
      }

      let parsed: unknown;
      try {
        parsed = JSON.parse(text);
      } catch {
        emitJsonError(`${source} is not valid JSON`);
      }
      invocation.body =
        target === "payload"
          ? { ...(invocation.body as Record<string, unknown>), payload: parsed }
          : target === "vector"
            ? { ...(invocation.body as Record<string, unknown>), vector: parsed }
            : target === "vectors"
              ? parsed
              : invocation.body;
    }
  }

  if (invocation.readBodyFrom !== undefined) {
    const source = invocation.readBodyFrom;
    const text =
      source === "-"
        ? await new Response(Bun.stdin.stream()).text()
        : await Bun.file(source).text().catch((err: unknown) => {
            emitJsonError(`cannot read --file ${source}: ${err instanceof Error ? err.message : String(err)}`);
          });
    if (text.trim() === "") emitJsonError(`--file ${source} is empty: a body proposal needs content`);
    invocation.body = { ...(invocation.body as Record<string, unknown>), proposedBody: text };
  }

  // Acceptance mirrors the requested format so a proxy or the dispatcher itself
  // can negotiate on content type without the CLI having to know the mapping.
  const accept =
    invocation.query.format === "text" || invocation.query.format === "manifest" || invocation.query.format === "graph"
      ? "text/plain"
      : invocation.query.format === "markdown"
        ? "text/markdown"
        : "application/json";

  // Flags beat environment variables, which beat the config file. A config file
  // exists so the common case needs no shell exports at all.
  const target = resolveTarget({
    ...(serverFlag === undefined ? {} : { server: serverFlag }),
    ...(tokenFlag === undefined ? {} : { token: tokenFlag }),
  });

  if (target.server === undefined) {
    emitJsonError(
      "No server configured.\n" +
        `  write ${configPath()} with:  server = "https://x-sage.example.com"\n` +
        "  or pass --server, or set X_SAGE_SERVER.",
    );
  }

  await runRemote(target.server, invocation, accept, target.token);
}

main().catch((err: unknown) => {
  writeAll("err", `${err instanceof Error ? err.message : String(err)}\n`);
  process.exit(1);
});
