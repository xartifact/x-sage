import { ZodError } from "zod";

interface ZodIssueLike {
  path?: Array<string | number>;
  message?: string;
}

function renderIssues(issues: ZodIssueLike[]): string | null {
  if (issues.length === 0) return null;
  return issues
    .map((issue) => {
      const path = (issue.path ?? []).join(".");
      const message = issue.message ?? "invalid";
      return path === "" ? message : `${path}: ${message}`;
    })
    .join("; ");
}

/**
 * Render a Zod failure as one line, or return null when `err` is not a Zod error.
 *
 * The `instanceof` check lives here, next to the schemas, rather than at every
 * call site. A consumer bundling its own copy of zod fails `instanceof` against
 * this module's `ZodError` and silently degrades to dumping the whole error
 * object — which is what happened when the CLI declared zod 4 while core used
 * zod 3.
 *
 * Never throws: this runs inside catch blocks, where a second failure would mask
 * the original error. A null return means "not a Zod error, use err.message".
 */
export function formatZodError(err: unknown): string | null {
  try {
    if (err instanceof ZodError) {
      return renderIssues(err.issues);
    }

    // Fallback for a foreign zod copy: same shape, different class identity.
    // Narrowing reads only `issues`, and every issue is re-checked before use.
    if (typeof err !== "object" || err === null || !("issues" in err)) {
      return null;
    }
    const issues: unknown = err.issues;
    if (!Array.isArray(issues)) return null;
    const shaped = issues.filter(
      (issue): issue is ZodIssueLike => typeof issue === "object" && issue !== null,
    );
    return shaped.length === issues.length ? renderIssues(shaped) : null;
  } catch {
    return null;
  }
}
