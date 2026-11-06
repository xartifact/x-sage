import { useState, useEffect, useCallback } from "react";
import type { GraphData } from "./types";

interface UseGraphDataResult {
  data: GraphData | null;
  loading: boolean;
  error: string | null;
  reload: () => void;
}

/**
 * Base URL for the API.
 *
 * Empty in production (same origin, server serves the built assets). In `vite
 * dev` the proxy in vite.config.ts forwards /api to the server, so the browser
 * never needs to know the backend port.
 */
const API_BASE = import.meta.env.VITE_API_BASE ?? "";

function isGraphData(value: unknown): value is GraphData {
  if (typeof value !== "object" || value === null) return false;
  if (!("nodes" in value) || !Array.isArray(value.nodes)) return false;
  if (!("edges" in value) || !Array.isArray(value.edges)) return false;
  return true;
}

/**
 * Fetch the graph from the shared service layer over HTTP.
 *
 * The payload is built per request (~2.6 ms for this KB), so there is no
 * separately generated `graph-data.json` to fall out of date.
 */
export function useGraphData(): UseGraphDataResult {
  const [data, setData] = useState<GraphData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);

  const reload = useCallback(() => {
    setNonce((current) => current + 1);
  }, []);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);

    fetch(`${API_BASE}/api/viz`)
      .then(async (res) => {
        if (!res.ok) {
          const body: unknown = await res.json().catch(() => null);
          const detail =
            typeof body === "object" && body !== null && "error" in body && typeof body.error === "string"
              ? body.error
              : `HTTP ${res.status}`;
          throw new Error(detail);
        }
        return res.json() as Promise<unknown>;
      })
      .then((json) => {
        if (cancelled) return;
        if (!isGraphData(json)) {
          throw new Error("Invalid payload: missing nodes/edges arrays");
        }
        setData(json);
        setLoading(false);
      })
      .catch((err: unknown) => {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : String(err));
        setLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [nonce]);

  return { data, loading, error, reload };
}
