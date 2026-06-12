import { useState, useEffect } from "react";
import type { GraphData, GraphNode, GraphEdge } from "./types";

interface UseGraphDataResult {
  data: GraphData | null;
  loading: boolean;
  error: string | null;
}

export function useGraphData(): UseGraphDataResult {
  const [data, setData] = useState<GraphData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    fetch("/graph-data.json")
      .then((res) => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        return res.json();
      })
      .then((json: unknown) => {
        // Validate structure
        const data = json as GraphData;
        if (!data.nodes || !Array.isArray(data.nodes)) {
          throw new Error("Invalid graph data: missing nodes array");
        }
        if (!data.edges || !Array.isArray(data.edges)) {
          throw new Error("Invalid graph data: missing edges array");
        }
        setData(data);
        setLoading(false);
      })
      .catch((err) => {
        setError(err instanceof Error ? err.message : String(err));
        setLoading(false);
      });
  }, []);

  return { data, loading, error };
}
