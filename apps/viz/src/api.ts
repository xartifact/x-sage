const base = import.meta.env.VITE_API_BASE ?? "";
export async function api(
  kb: string,
  path: string,
  method = "GET",
  body?: unknown,
  token = "",
  signal?: AbortSignal,
) {
  const url = new URL(`${base}/api${path}`, window.location.origin);
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
  if (!res.ok) throw new Error(value.error ?? JSON.stringify(value));
  return value;
}
