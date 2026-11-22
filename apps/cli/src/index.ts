#!/usr/bin/env bun
import { existsSync, writeSync } from "node:fs";
import { resolve } from "node:path";
import type { ApiMethod, ApiResponse } from "@x-sage/service";
import { configPath, resolveTarget } from "./config.ts";
import { formatVersion, readCliBuildInfo } from "./build-info.ts";

/**
 * x-sage CLI — a REST client.
 *
 * The server is the only component that reads the knowledge base. This file
 * translates argv into an HTTP request and writes the
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
function writeAll(stream: "out" | "err", text: string | ArrayBuffer): void {
  const fd = stream === "out" ? 1 : 2;
  const buffer = typeof text === "string" ? Buffer.from(text, "utf8") : Buffer.from(text);
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
  "read-term-text": "ReadTermText",
  "read-term-document": "ReadTermDocument",
  "read-term-activation": "ReadTermActivation",
  "read-graph-snapshot": "ReadGraphSnapshot",
  "read-proposal-files": "ReadProposalFiles",
  "submit-large-body": "SubmitLargeBodyProposal",
  "commit-direct-concept": "CommitDirectConcept",
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
  jsonFile?: string;
  exportPath?: string;
  absolutePath?: boolean;
  octetRequest?: boolean;
  octetResponse?: boolean;
  documentWrite?: boolean;
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
    case "status": {
      return { method: "GET", path: "/health", query: {} };
    }

    case "meta":
      return { method: "GET", path: "/meta", query: { identity: flags.identity === "true" ? "true" : undefined } };

    case "kb":
      if (positional[0] !== "list") emitJsonError("Usage: x-sage kb list");
      return { method: "GET", path: "/knowledge-bases", query: {} };

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

      if (subcmd === "frontier" || subcmd === "proposals") {
        const id = positional[1];
        return {
          method: "GET",
          path: "/knowledge/" + subcmd + (subcmd === "proposals" && id ? "/" + encodeURIComponent(id) : ""),
          query: { status: flags.status, preview: flags.preview },
        };
      }
      if (subcmd === "revision") {
        const note = "Usage: x-sage native revision <concept-id> [--revision <number>]";
        return {
          method: "GET", path: "/revisions",
          query: { id: requirePositional(positional[1], note), revision: flags.revision },
        };
      }
      if (subcmd === "frontier-document" || subcmd === "checkpoint-document") {
        const action = positional[1];
        if (action !== "read" && action !== "write") emitJsonError("Expected read or write");
        const write = action === "write";
        const frontier = subcmd === "frontier-document";
        const hasBody = write || !frontier;
        return {
          method: write ? "PUT" : frontier ? "GET" : "POST",
          path: "/db/v1/" + subcmd,
          absolutePath: true,
          query: {},
          octetResponse: !write,
          ...(write ? { documentWrite: true } : {}),
          ...(hasBody ? {
            octetRequest: true,
            requestFile: requirePositional(positional[2], "A binary request file or - is required"),
          } : {}),
        };
      }

      if (subcmd && Object.hasOwn(nativeMethods, subcmd)) {
        const method = subcmd as keyof typeof nativeMethods;
        return {
          method: "POST",
          path: nativeService + nativeMethods[method],
          query: {},
          nativeMethod: method,
          octetRequest: method === "submit-large-body" || method === "commit-direct-concept",
          requestFile: requirePositional(positional[1], `Usage: x-sage native ${method} <request-file|->`),
        };
      }
      emitUsageError(usage());
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
      if (subcmd === "read" || subcmd === "export") {
        const exportPath = subcmd === "export" ? requirePositional(positional[1], "Usage: x-sage graph export <path> [--semantic]") : undefined;
        return {
          method: "GET", path: "/graph",
          query: { semantic: flags.semantic === "true" ? "true" : undefined },
          ...(exportPath === undefined ? {} : { exportPath }),
        };
      }
      if (subcmd === "generate") {
        const manifest = flags.manifest === "true" ? { manifest: true } : {};
        return { method: "PUT", path: "/graph", query: { format: "text" }, body: manifest };
      }
      if (subcmd !== undefined && subcmd !== "validate") emitUsageError(usage());
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
        query: { format: flags.json === "true" ? "json" : "text" },
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
        query: { format: flags.json === "true" ? "json" : "text" },
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
      if (subcmd === "create") {
        const file = requirePositional(positional[1], "Usage: x-sage concept create <request.json|->");
        return { method: "POST", path: "/concepts", query: {}, jsonFile: file };
      }
      emitJsonError("Usage: x-sage concept body <id> | concept create <request.json|->");
    }

    case "knowledge":
      emitJsonError("The legacy knowledge commands have been retired. Use x-sage status for service status.");

    case "proposals": {
      const subcmd = positional[0];
      if (subcmd === "show") {
        const file = requirePositional(positional[1], "Usage: x-sage proposals show <proposal-file> [--format diff|json]");
        return {
          method: "GET",
          path: `/proposals/${encodeURIComponent(file)}`,
          query: { format: flags.json === "true" || flags.format === "json" ? "json" : "text" },
          reportToStdout: true,
        };
      }
      if (subcmd === "merge") {
        return {
          method: "POST",
          path: "/proposals/merge",
          query: { format: flags.json === "true" ? "json" : "text" },
          body: { file: positional[1] ?? "all" },
        };
      }
      if (subcmd !== undefined && subcmd !== "list") emitUsageError(usage());
      return { method: "GET", path: "/proposals", query: { format: flags.json === "true" ? "json" : "text" } };
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
      return { method: "GET", path: "/viz", query: { format: "json" }, exportPath: output };
    }

    default:
      // Unknown command is a usage error, not a successful invocation.
      emitUsageError(usage());
  }
}

function usage(): string {
  return `x-sage CLI  (renders server-side; the client holds no kb data)

Usage: x-sage <command> [args] [flags]

Connection:
  --server <url>                        Server base URL
  --token <token>                       Bearer token for an authenticated server
  --kb <id>                             Select a KB on the public REST server

  Resolution: flags > environment > config file.
    X_SAGE_SERVER / X_SAGE_TOKEN        environment
    ~/.config/x-sage/config.toml        server = "...", token = "..."
                                        (honours XDG_CONFIG_HOME)

Native Rust service (use its private endpoint and role credential):
  native checkpoint <concept-id> [session-number]
  native handshake|submit|review|commit|outcome <request.json|->
  native read-term-text|read-term-document|read-term-activation <request.json|->
  native read-graph-snapshot|read-proposal-files <request.json|->
  native submit-large-body|commit-direct-concept <request.bin|->
  native revision <concept-id> [--revision <number>]
  native frontier | proposals [id] [--status S] [--preview]
  native frontier-document read | write <request.bin|->
  native checkpoint-document read|write <request.bin|->

The client holds no KB data. Native RPC commands send the caller's request
bytes directly. Exit 2 means a native attempt or outcome was inconclusive;
look up the original request id.

Read:
  status                                Show service health and reported authority
  kb list | meta [--identity]            List KBs or show metadata
  manifest [--write]                    Render MANIFEST.md; --write persists it
  search <keyword> [--format manifest] [--json]
  activate <id> [--format ai|human] [--depth N] [--strategy S]
                                        S = exploration|validation|generation
  path <from-id> <to-id>
  edges <id> [--type T] [--dir in|out|both] [--derived] [--json]

Validate:
  validate [--graph] [--json]           Files; --graph adds integrity + staleness

Write-back (needs --token when the server requires one):
  concept body <id> [--format json|markdown]   Read the body + revision
  concept create <request.json|->             Submit frontmatter and body for creation
  propose-edge <from> <type> <target> [--confidence C]
  refine-atom <concept-id> --atom "text" [--reason "text"]
  refine-body <id> --file <path|-> --expected-revision <rev> --reason "why"
  proposals list | proposals show <file> | proposals merge [file|all] [--json]
  frontier list | frontier add <concept> --source "..." --reason "..." [--priority P]
  checkpoint create <id> [session] | read <id> [session] | save -   (save reads stdin)

Graph:
  graph read | export <path> [--semantic]
  graph generate [--manifest] | validate

Viz:
  viz stats | viz export <path>

Offline administration (use a source checkout):
  admin archive|plan|import|backup|restore [--workspace <path>] <tool arguments>

Version:
  --version | version                  Print CLI version + build commit (local, no server)
`;
}

/** Recognize the public adapter's explicit refusal in either negotiated format. */
function isDisconnectedRoute(response: Response, body: string): boolean {
  if (response.status !== 503) return false;
  const message = "Operation is not connected to x-sage-db";
  if (body.trim() === message) return true;
  try {
    return JSON.parse(body)?.error === message;
  } catch {
    return false;
  }
}

function readHealth(body: string): { ok: boolean; authority?: string; databaseReady?: boolean } | undefined {
  try {
    const status = JSON.parse(body);
    if (typeof status?.ok === "boolean") return status;
  } catch {
    return undefined;
  }
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
  const privateTransport = invocation.nativeMethod !== undefined || invocation.absolutePath === true;
  const url = new URL(`${base}${privateTransport ? "" : "/api"}${invocation.path}`);
  const rpcMutation = invocation.nativeMethod === "submit" || invocation.nativeMethod === "review" ||
    invocation.nativeMethod === "commit" || invocation.nativeMethod === "submit-large-body" ||
    invocation.nativeMethod === "commit-direct-concept";
  const mutations = rpcMutation || invocation.documentWrite === true;
  const unresolved = invocation.documentWrite
    ? "Inspect the private document before retrying."
    : "Look up the original request id.";
  for (const [key, value] of Object.entries(invocation.query)) {
    if (value !== undefined) url.searchParams.set(key, value);
  }

  const headers: Record<string, string> = { accept: invocation.octetResponse ? "application/octet-stream" : accept };
  if (token !== undefined) headers.authorization = `Bearer ${token}`;
  const outbound = invocation.requestBytes ??
    (invocation.body === undefined ? undefined : JSON.stringify(invocation.body));
  if (outbound !== undefined) headers["content-type"] = invocation.octetRequest ? "application/octet-stream" : "application/json";

  let response: Response;
  try {
    response = await fetch(url, {
      method: invocation.method,
      headers,
      ...(privateTransport ? { redirect: "error" as const } : {}),
      ...(outbound === undefined ? {} : { body: outbound }),
    });
  } catch (err) {
    const detail = err instanceof Error ? err.message : String(err);
    if (mutations) {
      writeAll("err", `Request result unconfirmed at ${url.origin}. ${unresolved}\n`);
      process.exit(2);
    }
    emitJsonError(`cannot reach ${url.origin}: ${detail}`);
  }

  if (invocation.octetResponse && response.ok) {
    if (response.headers.get("content-type") !== "application/octet-stream") {
      emitJsonError("Expected a binary response from the private Rust endpoint.");
    }
    writeAll("out", await response.arrayBuffer());
    process.exit(0);
  }

  let body: string;
  try {
    body = await response.text();
  } catch (error) {
    if (mutations) {
      writeAll("err", `Request response incomplete. ${unresolved}\n`);
      process.exit(2);
    }
    throw error;
  }

  if (invocation.path === "/health") {
    const health = readHealth(body);
    if (health === undefined && response.ok) emitJsonError("The service health endpoint did not return a status report.");
    if (health !== undefined && (!health.ok || (health.authority === "sage-db" && health.databaseReady !== true))) {
      emit({ status: response.ok ? 503 : response.status, contentType: "application/json", body });
    }
  }

  if ((invocation.path.startsWith("/knowledge/") || invocation.path === "/revisions") && isDisconnectedRoute(response, body)) {
    if (invocation.path.startsWith("/knowledge/checkpoints/")) {
      emitJsonError("Native checkpoint reads use the private Rust endpoint. At the public endpoint, use x-sage checkpoint read <id> [session].");
    }
    emitJsonError("This native read uses the private Rust endpoint and its role credential.");
  }

  if (privateTransport && response.headers.get("content-type")?.toLowerCase().startsWith("text/html")) {
    if (mutations && (response.ok || response.status === 408 || response.status >= 500)) {
      writeAll("err", `Native response is HTML. Check the private Rust endpoint. ${unresolved}\n`);
      process.exit(2);
    }
    emitJsonError("Native RPC received HTML. Set --server to the private Rust endpoint and use its role credential.");
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
    if ((rpcMutation && !reply?.outcome?.committed?.recorded && !reply?.outcome?.alreadyCommitted?.recorded) ||
        (invocation.nativeMethod === "outcome" && !reply?.found?.recorded && !reply?.absent)) {
      writeAll("out", body);
      process.exit(2);
    }
  } else if (mutations && (response.status === 408 || response.status >= 500)) {
    writeAll("err", body);
    process.exit(2);
  }
  if (invocation.documentWrite && response.ok && response.status !== 204) {
    writeAll("err", `Unexpected native document acknowledgement. ${unresolved}\n`);
    process.exit(2);
  }

  const exportPath = invocation.exportPath;
  if (exportPath !== undefined && response.ok) {
    await Bun.write(exportPath, body);
    writeAll("out", `Exported ${invocation.path.slice(1)} payload → ${exportPath}\n`);
    process.exit(0);
  }

  emit({ status: response.status, contentType: response.headers.get("content-type") ?? "", body }, stream);
}

async function runAdmin(command: string | undefined, args: string[]): Promise<void> {
  if (!command || !["archive", "plan", "import", "backup", "restore"].includes(command)) {
    emitUsageError(usage());
  }
  const workspaceFlag = args.indexOf("--workspace");
  const workspace = workspaceFlag < 0
    ? resolve(import.meta.dir, "../../..")
    : resolve(requirePositional(args[workspaceFlag + 1], "--workspace needs a source checkout path"));
  if (workspaceFlag >= 0) args.splice(workspaceFlag, 2);
  const manifest = resolve(workspace, "sage-db/Cargo.toml");
  if (!existsSync(manifest)) emitJsonError("Provide --workspace <x-sage checkout> for offline administration.");
  let program: string[];
  if (command === "archive" || command === "plan") {
    const tool = command === "archive" ? "tools/sage-db-source-archive.ts" : "tools/sage-db-import-plan.ts";
    program = [process.execPath, resolve(workspace, tool)];
  } else {
    const toolchain = await Bun.file(resolve(workspace, "sage-db/rust-toolchain.toml")).text();
    const channel = /^channel\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"/m.exec(toolchain)?.[1];
    if (!channel) emitJsonError("Could not read the checkout's pinned Rust toolchain.");
    program = ["cargo", "+" + channel, "run", "--quiet", "--locked", "--manifest-path", manifest,
      "-p", "sage-db-migrate", "--bin", "sage-db-admin", "--", command];
  }
  const child = Bun.spawn([...program, ...args], {
    stdin: "inherit", stdout: "inherit", stderr: "inherit",
  });
  process.exitCode = await child.exited;
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

  const kbFlag = allArgs[0] === "admin" || !allArgs.includes("--kb")
    ? undefined
    : requirePositional(takeFlag("kb"), "Provide a knowledge-base id after --kb.");
  const command = allArgs[0];
  if (command === undefined) emitUsageError(usage());
  if (command === "--help" || command === "-h" || command === "help") {
    writeAll("out", usage());
    return;
  }
  if (command === "admin") {
    await runAdmin(allArgs[1], allArgs.slice(2));
    return;
  }


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
  if (kbFlag !== undefined) {
    if (command === "native") emitJsonError("Native requests select their KB through the private endpoint and request envelope.");
    invocation.query.kb = kbFlag;
  }
  if (invocation.jsonFile !== undefined) {
    const file = invocation.jsonFile;
    const body = file === "-" ? await new Response(Bun.stdin.stream()).text() : await Bun.file(file).text();
    try {
      invocation.body = JSON.parse(body);
    } catch {
      emitJsonError("concept create requires a JSON object with frontmatter and body.");
    }
  }

  if (invocation.requestFile !== undefined) {
    const file = invocation.requestFile;
    invocation.requestBytes = file === "-"
      ? await new Response(Bun.stdin.stream()).arrayBuffer()
      : await Bun.file(file).arrayBuffer();
    if (invocation.requestBytes.byteLength === 0) emitJsonError("Native request is empty");
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
