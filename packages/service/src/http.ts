/**
 * HTTP-shaped values shared by the dispatcher and its sub-routers.
 *
 * `ApiResponse` is transport-neutral: the server turns it into a Response and
 * the CLI writes its body to stdout. The dispatcher maps `ApiError` to its
 * HTTP status.
 */

export interface ApiResponse {
  status: number;
  contentType: string;
  body: string;
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
