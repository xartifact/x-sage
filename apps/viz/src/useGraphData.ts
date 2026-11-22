import { useState, useEffect, useCallback } from "react";
import type { GraphData } from "./types";
import { api } from "./api";
export function useGraphData(kb = "", token = "") {
  const [data, setData] = useState<GraphData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);
  const [updated, setUpdated] = useState("");
  const reload = useCallback(() => setNonce((n) => n + 1), []);
  useEffect(() => {
    let active = true;
    let busy = false;
    const controller = new AbortController();
    const refresh = async () => {
      if (busy) return;
      busy = true;
      try {
        const next = await api(
          kb,
          "/viz",
          "GET",
          undefined,
          token,
          controller.signal,
        );
        if (!Array.isArray(next.nodes) || !Array.isArray(next.edges))
          throw new Error("图谱数据格式无效");
        if (active) {
          setData((old) =>
            old &&
            JSON.stringify([old.nodes, old.edges]) ===
              JSON.stringify([next.nodes, next.edges])
              ? old
              : next,
          );
          setError(null);
          setUpdated(new Date().toLocaleTimeString());
        }
      } catch (e) {
        if (active) setError(String(e));
      } finally {
        busy = false;
      }
    };
    void refresh();
    const timer = setInterval(() => {
      if (!document.hidden) void refresh();
    }, 3000);
    window.addEventListener("focus", refresh);
    return () => {
      active = false;
      controller.abort();
      clearInterval(timer);
      window.removeEventListener("focus", refresh);
    };
  }, [kb, nonce, token]);
  return { data, loading: !data && !error, error, reload, updated };
}
