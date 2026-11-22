const base = import.meta.env?.VITE_API_BASE ?? "";
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
