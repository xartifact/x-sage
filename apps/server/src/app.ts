import { Hono } from "hono";
import { cors } from "hono/cors";
import { compress } from "hono/compress";
import { timingSafeEqual } from "node:crypto";
import { bodyLimit } from "hono/body-limit";
import { handleRequest, type ApiMethod, type ApiRequest } from "@x-sage/service";
import { kb, type KbContext, wantsText } from "@x-sage/service";
import { readBuildInfo } from "./build-info.ts";

export interface AppOptions {
  /** Absolute path to the KB directory. */
  kbRoot: string;
  /** Origin allowed to call the API; defaults to any, which suits local dev. */
  corsOrigin?: string;
  /**
   * Token required for every write (non-GET) request. Unset means writes are
   * open — acceptable only when the server is bound to loopback.
   */
  writeToken?: string;
}

/** Methods that mutate the KB. These require a token when one is configured. */
const WRITE_METHODS: Record<string, true> = { POST: true, PUT: true, DELETE: true, PATCH: true };

const isWrite = (method: string): boolean => WRITE_METHODS[method] === true;

/**
 * HTTP adapter over the shared dispatcher.
 *
 * There is no route table here on purpose: `handleRequest` in `@x-sage/service`
 * owns routing, validation and rendering, so the CLI and this server cannot
 * drift. This layer only does transport work — parse the request, apply
 * auth/CORS/limits, convert the dispatcher's `{status, contentType, body}` into
 * a Response.
 */
export function createApp(options: AppOptions) {
  const ctx: KbContext = kb(options.kbRoot);
  const app = new Hono();

  app.use("*", cors({ origin: options.corsOrigin ?? "*" }));

  // The viz payload is ~93 KB of JSON and every response is generated per
  // request, so compression is the difference between usable and not over a
  // remote link.
  app.use("*", compress());

  // Writes are unbounded without this: a 50 MB proposal body would be parsed
  // into memory before any validation runs.
  app.use(
    "/api/*",
    bodyLimit({
      maxSize: 256 * 1024,
      onError: (c) => c.json({ error: "Request body too large" }, 413),
    }),
  );

  if (options.writeToken !== undefined) {
    const token = options.writeToken;
    app.use("/api/*", async (c, next) => {
      if (!isWrite(c.req.method)) return next();

      const header = c.req.header("authorization") ?? "";
      const presented = header.startsWith("Bearer ") ? header.slice("Bearer ".length) : "";

      // Compare lengths first so `timingSafeEqual` cannot throw on a size
      // mismatch, then compare in constant time: a token check that leaks
      // prefix length is worth brute-forcing over a slow link.
      const expected = new TextEncoder().encode(token);
      const actual = new TextEncoder().encode(presented);
      const sameLength = expected.length === actual.length;
      const equal = sameLength && timingSafeEqual(expected, actual);

      if (!equal) {
        // The middleware runs before the dispatcher, so it must honour the
        // requested format itself: a CLI asking for text would otherwise print
        // a JSON object where a sentence belongs.
        const message = "Unauthorized: a Bearer token is required for write requests";
        const url = new URL(c.req.url);
        return wantsText(url.searchParams.get("format"), c.req.header("accept"))
          ? c.text(`${message}\n`, 401)
          : c.json({ error: message }, 401);
      }
      return next();
    });
  }

  app.get("/api/health", (c) => c.json({ ok: true, kb: options.kbRoot, ...readBuildInfo() }));

  // Every other /api route goes through the dispatcher. Registering one handler
  // for the whole prefix keeps the routing table in exactly one place.
  app.all("/api/*", async (c) => {
    const url = new URL(c.req.url);
    const path = url.pathname.slice("/api".length);

    let body: unknown;
    if (isWrite(c.req.method)) {
      // A body is optional for endpoints whose arguments are all in the path.
      body = await c.req.json().catch(() => undefined);
    }

    const query: Record<string, string | undefined> = {};
    url.searchParams.forEach((value, key) => {
      query[key] = value;
    });

    const request: ApiRequest = {
      method: c.req.method as ApiMethod,
      path,
      query,
      ...(body === undefined ? {} : { body }),
    };

    const response = await handleRequest(ctx, request, c.req.header("accept"));
    return new Response(response.body, {
      status: response.status,
      headers: { "content-type": response.contentType },
    });
  });

  return app;
}
