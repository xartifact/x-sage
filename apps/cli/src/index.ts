#!/usr/bin/env bun
import type { ApiMethod, ApiResponse } from "@x-sage/service";
import { configPath, resolveTarget } from "./config.ts";

/**
 * x-sage CLI — a REST client.
 *
 * The server is the only component that reads the knowledge base. This file
 * translates argv into one HTTP request against `/api/*` and writes the
 * response body to stdout; text is rendered server-side, so the client needs
 * no graph, no schema and no kb files.
 *
 * There is no local mode. Point it at a deployment with `--server` or
 * `X_SAGE_SERVER`, and supply `--token`/`X_SAGE_TOKEN` when the server
 * requires one for writes.
 */

/**
 * Write the response body out and exit with a matching status.
 *
 * Failures go to stderr so `x-sage activate x > file` never puts an error
 * message into the file. Everything else is stdout, unmodified.
 */
function emit(response: ApiResponse, stream?: "out" | "err"): never {
  const failed = response.status >= 400;
  const target = stream ?? (failed ? "err" : "out");
  (target === "out" ? process.stdout : process.stderr).write(response.body);
  process.exit(failed ? 1 : 0);
}

function emitJsonError(message: string): never {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

/** Usage text is informational, but the invocation still failed. */
function emitUsageError(message: string): never {
  process.stdout.write(`${message}\n`);
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
interface Invocation {
  method: ApiMethod;
  path: string;
  query: Record<string, string | undefined>;
  body?: unknown;
  /**
   * Print the body on stdout even when the status is an error.
   *
   * `validate` reports findings as its normal output and signals failure with
   * the exit code; sending the report to stderr would hide it from a pipe.
   */
  reportToStdout?: boolean;
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
        emitJsonError(
          "frontier add is disabled: it rewrites frontier.md from one table and deletes the other sections.\n" +
            "Edit kb/<id>/frontier.md directly instead.",
        );
      }
      return {
        method: "GET",
        path: "/frontier",
        query: { format: flags.json === "true" ? "json" : "text" },
      };
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

    case "proposals": {
      const subcmd = positional[0];
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

function usage(): string {
  return `x-sage CLI  (renders server-side; the client holds no kb data)

Usage: x-sage <command> [args] [flags]

Connection:
  --server <url>                        Server base URL
  --token <token>                       Bearer token for write commands

  Resolution: flags > environment > config file.
    X_SAGE_SERVER / X_SAGE_TOKEN        environment
    ~/.config/x-sage/config.toml        server = "...", token = "..."
                                        (honours XDG_CONFIG_HOME)

Every command is one HTTP request against the server's REST API; the client
holds no kb data and never touches the filesystem.

Read:
  manifest [--write]                    Render MANIFEST.md; --write persists it
  search <keyword> [--format manifest] [--json]
  activate <id> [--format ai|human] [--depth N] [--strategy S]
                                        S = exploration|validation|generation
  path <from-id> <to-id>
  edges <id> [--type T] [--dir in|out|both] [--derived] [--json]

Validate:
  validate [--graph] [--json]           Files; --graph adds integrity + staleness

Write-back (needs --token when the server requires one):
  propose-edge <from> <type> <target> [--confidence C]
  refine-atom <concept-id> --atom "text" [--reason "text"]
  proposals list | proposals merge [file|all]
  frontier list
  checkpoint create <id> [session] | read <id> [session] | save -   (save reads stdin)

Graph:
  graph generate [--manifest]           Rebuild graph.json (+ MANIFEST.md)

Viz:
  viz stats | viz export <path>
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
  // Every path the dispatcher serves lives under /api on the wire.
  const base = server.endsWith("/") ? server.slice(0, -1) : server;
  const url = new URL(`${base}/api${invocation.path}`);
  for (const [key, value] of Object.entries(invocation.query)) {
    if (value !== undefined) url.searchParams.set(key, value);
  }

  const headers: Record<string, string> = { accept };
  if (token !== undefined) headers.authorization = `Bearer ${token}`;
  if (invocation.body !== undefined) headers["content-type"] = "application/json";

  let response: Response;
  try {
    response = await fetch(url, {
      method: invocation.method,
      headers,
      ...(invocation.body === undefined ? {} : { body: JSON.stringify(invocation.body) }),
    });
  } catch (err) {
    const detail = err instanceof Error ? err.message : String(err);
    emitJsonError(`cannot reach ${url.origin}: ${detail}`);
  }

  const body = await response.text();

  // `viz export` writes the payload to a file instead of stdout.
  const exportPath = invocation.query.export;
  if (exportPath !== undefined && response.ok) {
    await Bun.write(exportPath, body);
    process.stdout.write(`Exported viz payload → ${exportPath}\n`);
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
  process.stderr.write(`${err instanceof Error ? err.message : String(err)}\n`);
  process.exit(1);
});
