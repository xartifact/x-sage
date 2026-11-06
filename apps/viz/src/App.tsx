import { useState, useCallback } from "react";
import { useGraphData } from "./useGraphData";
import ConceptGraph from "./ConceptGraph";
import FilterControls from "./FilterControls";
import DetailPanel from "./DetailPanel";
import type { GraphNode, Layer, EdgeType } from "./types";
import "./styles.css";

export default function App() {
  const { data, loading, error, reload } = useGraphData();
  const [selectedNode, setSelectedNode] = useState<GraphNode | null>(null);
  const [visibleLayers, setVisibleLayers] = useState<Set<Layer>>(new Set([0, 1]));
  const [visibleEdgeTypes, setVisibleEdgeTypes] = useState<Set<EdgeType>>(
    new Set(["is-a", "part-of", "depends-on", "enables", "instantiates", "contradicts", "analogous-to"])
  );

  const handleToggleLayer = useCallback((layer: Layer) => {
    setVisibleLayers((prev) => {
      const next = new Set(prev);
      if (next.has(layer)) {
        next.delete(layer);
      } else {
        next.add(layer);
      }
      return next;
    });
  }, []);

  const handleToggleEdgeType = useCallback((type: EdgeType) => {
    setVisibleEdgeTypes((prev) => {
      const next = new Set(prev);
      if (next.has(type)) {
        next.delete(type);
      } else {
        next.add(type);
      }
      return next;
    });
  }, []);

  const handleNodeClick = useCallback((node: GraphNode | null) => {
    setSelectedNode(node);
  }, []);

  const handleClosePanel = useCallback(() => {
    setSelectedNode(null);
  }, []);

  if (loading) {
    return (
      <div className="app-container">
        <div className="loading-overlay">
          <div className="spinner" />
          <div className="loading-text">加载知识图谱数据...</div>
        </div>
      </div>
    );
  }

  if (error || !data) {
    return (
      <div className="app-container">
        <div className="error-overlay">
          <div className="error-icon">⚠️</div>
          <div className="error-text">加载数据失败</div>
          <div className="error-detail">{error || "数据不可用"}</div>
          <p className="error-hint">
            数据由 API 提供。请确认后端已启动：<code>bun run server</code>
          </p>
          <button className="error-retry" onClick={reload}>
            重试
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="app-container">
      <div className="app-title">x-sage 知识图谱</div>

      <FilterControls
        visibleLayers={visibleLayers}
        visibleEdgeTypes={visibleEdgeTypes}
        onToggleLayer={handleToggleLayer}
        onToggleEdgeType={handleToggleEdgeType}
      />

      <ConceptGraph
        nodes={data.nodes}
        edges={data.edges}
        visibleLayers={visibleLayers}
        visibleEdgeTypes={visibleEdgeTypes}
        onNodeClick={handleNodeClick}
        selectedNodeId={selectedNode?.id ?? null}
      />

      <DetailPanel
        node={selectedNode}
        edges={data.edges}
        allNodes={data.nodes}
        onClose={handleClosePanel}
      />
    </div>
  );
}
