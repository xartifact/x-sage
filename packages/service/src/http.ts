/**
 * HTTP-shaped values shared by the dispatcher and its sub-routers.
 *
 * `ApiResponse` is transport-neutral: the server turns it into a Response and
 * the CLI writes its body to stdout. Keeping the error type and the small body
 * builders here (rather than in `api.ts`) lets the knowledge router throw the
 * same `ApiError` the main routing table maps to a status code, without a
 * circular import between the two modules.
 */

export interface ApiResponse {
  status: number;
  contentType: string;
  body: string;
  /**
   * Raw bytes for a response that is not text (an image asset).
   *
   * Separate from `body` because a JS string cannot carry arbitrary bytes: a
   * latin1-decoded PNG re-encoded as UTF-8 is corrupt, and base64 in the JSON
   * envelope would make an <img src> impossible.
   */
  bytes?: Uint8Array;
}

export const JSON_TYPE = "application/json; charset=utf-8";
export const TEXT_TYPE = "text/plain; charset=utf-8";
export const MARKDOWN_TYPE = "text/markdown; charset=utf-8";

/** A failure that carries the HTTP status it should become. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

export function json(status: number, value: unknown): ApiResponse {
  // Trailing newline keeps CLI stdout uniform: the same body serves a terminal
  // (which reads better ending on its own line) and an HTTP client.
  return { status, contentType: JSON_TYPE, body: JSON.stringify(value) + "\n" };
}

export function text(status: number, body: string, contentType = TEXT_TYPE): ApiResponse {
  return { status, contentType, body };
}

export function binary(status: number, bytes: Uint8Array, contentType: string): ApiResponse {
  return { status, contentType, body: "", bytes };
}
