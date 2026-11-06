#!/usr/bin/env bun
import { resolve, join } from "node:path";
import { existsSync, statSync } from "node:fs";
import { createApp } from "./app.ts";

/**
 * Entry point for the HTTP server.
 *
 * Config comes from the environment so one build serves any KB:
 *   X_SAGE_KB       KB directory (default: <repo>/kb/default)
 *   PORT            listen port (default: 3001)
 *   HOST            bind address (default: 127.0.0.1 — not public by default)
 *   CORS_ORIGIN     allowed origin (default: *, fine for local dev)
 *   X_SAGE_WEB_DIST built viz assets to serve (default: apps/viz/dist if present)
 *   X_SAGE_WRITE_TOKEN  Bearer token required for POST/PUT (unset = open writes)
 *
 * In development the Vite dev server proxies /api here, so the browser sees one
 * origin. In production this process serves both the API and the built assets.
 */
const kbRoot = resolve(process.env.X_SAGE_KB ?? resolve(import.meta.dir, "../../../kb/default"));
const port = Number.parseInt(process.env.PORT ?? "3001", 10);
const hostname = process.env.HOST ?? "127.0.0.1";
const corsOrigin = process.env.CORS_ORIGIN;
const defaultDist = resolve(import.meta.dir, "../../viz/dist");
const webDist = resolve(process.env.X_SAGE_WEB_DIST ?? defaultDist);

if (!existsSync(kbRoot)) {
  console.error(`KB directory not found: ${kbRoot}`);
  process.exit(1);
}

const writeToken = process.env.X_SAGE_WRITE_TOKEN;

// A server reachable from another machine must not accept anonymous writes. The
// bind address is the only signal available here, so refusing to start is safer
// than silently exposing mutation endpoints.
if (writeToken === undefined && hostname !== "127.0.0.1" && hostname !== "localhost" && hostname !== "::1") {
  console.error(
    `Refusing to start: HOST=${hostname} accepts off-host connections but X_SAGE_WRITE_TOKEN is unset.\n` +
      "Set X_SAGE_WRITE_TOKEN=<secret> so POST/PUT require a Bearer token.",
  );
  process.exit(1);
}

const app = createApp({
  kbRoot,
  ...(corsOrigin === undefined ? {} : { corsOrigin }),
  ...(writeToken === undefined ? {} : { writeToken }),
});
const serveWeb = existsSync(webDist) && statSync(webDist).isDirectory();

const indexHtml = serveWeb ? Bun.file(join(webDist, "index.html")) : null;

console.log("x-sage server");
console.log(`  api:  ${"/api"} (manifest, concepts, edges, validate, proposals, checkpoints)`);
console.log(`  kb:   ${kbRoot}`);
console.log(`  http: http://${hostname}:${port}`);
console.log(`  web:  ${serveWeb ? webDist : "(not built — run 'bun run build' in apps/viz)"}`);
console.log(`  auth: ${writeToken === undefined ? "writes open (loopback only)" : "writes require Bearer token"}`);

export default {
  port,
  hostname,
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname.startsWith("/api/")) {
      return app.fetch(request);
    }

    if (!serveWeb) {
      return new Response("Web assets not built. API is available under /api/.", {
        status: 404,
        headers: { "content-type": "text/plain; charset=utf-8" },
      });
    }

    // Static assets: serve the exact file when it exists, else fall back to
    // index.html so client-side routes work.
    const candidate = join(webDist, url.pathname === "/" ? "index.html" : url.pathname);
    if (candidate.startsWith(webDist) && existsSync(candidate) && statSync(candidate).isFile()) {
      return new Response(Bun.file(candidate));
    }

    if (indexHtml === null) {
      return new Response("Not found", { status: 404 });
    }
    return new Response(indexHtml, { headers: { "content-type": "text/html; charset=utf-8" } });
  },
};
