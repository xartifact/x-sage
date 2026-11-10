import { useState, useCallback } from "react";
import { useGraphData } from "./useGraphData";
import ConceptGraph from "./ConceptGraph";
import FilterControls from "./FilterControls";
import DetailPanel from "./DetailPanel";
import type { GraphNode, Layer, EdgeType } from "./types";
import { EDGE_TYPES, LAYERS } from "./types";
import "./styles.css";

export default function App() {
  const { data, loading, error, reload } = useGraphData();
  const [selectedNode, setSelectedNode] = useState<GraphNode | null>(null);
  // Every layer starts visible. The KB grows at L2/L3, so a default of
  // [0, 1] made newly added concepts look like they never reached the server.
  const [visibleLayers, setVisibleLayers] = useState<Set<Layer>>(() => new Set(LAYERS));
  const [visibleEdgeTypes, setVisibleEdgeTypes] = useState<Set<EdgeType>>(() => new Set(EDGE_TYPES));

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
      <div className="app-title">
        x-sage 知识图谱
        {data.meta ? <span className="app-count">{data.meta.conceptCount} 概念</span> : null}
      </div>

      <FilterControls
        visibleLayers={visibleLayers}
        visibleEdgeTypes={visibleEdgeTypes}
        layerCounts={data.meta?.layerCounts}
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
