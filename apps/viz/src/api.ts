const base = import.meta.env?.VITE_API_BASE ?? "";

export interface BuildIdentity {
  version: string;
  commitHash: string;
}

export async function fetchBuildIdentity(signal: AbortSignal): Promise<BuildIdentity | null> {
  const url = new URL(`${base}/api/health`, window.location.origin);
  const response = await fetch(url, { signal, cache: "no-store" });
  // The health route also reports the running build while the database is unavailable.
  if (!response.ok && response.status !== 503) return null;
  const value: Partial<BuildIdentity> | null = await response.json();
  return value && typeof value.version === "string" && typeof value.commitHash === "string"
    ? { version: value.version, commitHash: value.commitHash }
    : null;
}

export class ApiError extends Error {
  constructor(message:string, readonly status:number, readonly payload:unknown){super(message);this.name='ApiError';}
}
export async function api(
  kb: string,
  path: string,
  method = "GET",
  body?: unknown,
  token = "",
  signal?: AbortSignal,
) {
  const route = path.startsWith("/sage.v1.SageCommandService/") ? path : `/api${path}`;
  const url = new URL(`${base}${route}`, window.location.origin);
  if (kb) url.searchParams.set("kb", kb);
  const res = await fetch(url, {
    method,
    signal: signal ?? null,
    cache: "no-store",
    headers: {
      "Content-Type": "application/json",
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const value = await res.json();
  if (!res.ok) throw new ApiError(value.error ?? JSON.stringify(value), res.status, value);
  return value;
}
