import { useState, useCallback, useEffect } from "react";
import { useGraphData } from "./useGraphData";
import ConceptGraph from "./ConceptGraph";
import FilterControls from "./FilterControls";
import DetailPanel from "./DetailPanel";
import type { GraphNode, Layer, EdgeType } from "./types";
import { EDGE_TYPES, LAYERS } from "./types";
import "./styles.css";
import { api } from "./api";
import Workbench from "./Workbench";

export default function App() {
  const [entries, setEntries] = useState<
    Array<{ id: string; name: string; isDefault: boolean }>
  >([]);
  const [kb, setKb] = useState("");
  const [error, setError] = useState("");
  const [editing, setEditing] = useState(false);
  const [catalogAttempt, setCatalogAttempt] = useState(0);
  const [catalogLoading, setCatalogLoading] = useState(true);
  useEffect(() => {
    let active = true;
    setCatalogLoading(true);
    setError("");
    api("", "/knowledge-bases")
      .then((data) => {
        if (!active) return;
        setEntries(data.knowledgeBases);
        setKb(
          data.knowledgeBases.find((k: { isDefault: boolean }) => k.isDefault)
            ?.id ??
            data.knowledgeBases[0]?.id ??
            "",
        );
      })
      .catch((e) => {
        if (active) setError(String(e));
      })
      .finally(() => {
        if (active) setCatalogLoading(false);
      });
    return () => {
      active = false;
    };
  }, [catalogAttempt]);
  return (
    <>
      <div className="workspace-bar">
        <strong>x-sage</strong>
        <label>
          知识库{" "}
          <select
            aria-label="知识库"
            disabled={editing}
            value={kb}
            onChange={(e) => setKb(e.target.value)}
          >
            {entries.map((entry) => (
              <option key={entry.id} value={entry.id}>
                {entry.name}
              </option>
            ))}
          </select>
        </label>
        {editing && <span>关闭维护面板后可切换知识库</span>}
        {error && <span role="alert">{error}</span>}
        {!editing && (
          <button onClick={() => setCatalogAttempt((n) => n + 1)}>
            刷新库列表
          </button>
        )}
      </div>
      {!kb && (
        <p className="catalog-message">
          {catalogLoading
            ? "正在加载知识库…"
            : error
              ? "知识库列表加载失败，请重试。"
              : "尚未配置知识库，请在服务端注册后刷新列表。"}
        </p>
      )}
      {kb && (
        <Workspace key={kb} kb={kb} editing={editing} setEditing={setEditing} />
      )}
    </>
  );
}
function Workspace({
  kb,
  editing,
  setEditing,
}: {
  kb: string;
  editing: boolean;
  setEditing: (value: boolean) => void;
}) {
  const { data, loading, error, reload, updated } = useGraphData(kb);
  const [selectedNode, setSelectedNode] = useState<GraphNode | null>(null);
  // Every layer starts visible. The KB grows at L2/L3, so a default of
  // [0, 1] made newly added concepts look like they never reached the server.
  const [visibleLayers, setVisibleLayers] = useState<Set<Layer>>(
    () => new Set(LAYERS),
  );
  const [visibleEdgeTypes, setVisibleEdgeTypes] = useState<Set<EdgeType>>(
    () => new Set(EDGE_TYPES),
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

  if (!data) {
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
        <button onClick={reload}>刷新</button>{" "}
        <button
          onClick={() => {
            setSelectedNode(null);
            setEditing(true);
          }}
        >
          维护知识库
        </button>
        <span className="sync-status">
          {error ? `同步失败：${error}` : `已同步 ${updated} · 每 3 秒更新`}
        </span>
        {data.meta ? (
          <span className="app-count">{data.meta.conceptCount} 概念</span>
        ) : null}
      </div>

      {editing && (
        <Workbench
          kb={kb}
          ids={data.nodes.map((n) => n.id)}
          onChange={reload}
          onClose={() => setEditing(false)}
        />
      )}
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
        node={data.nodes.find((node) => node.id === selectedNode?.id) ?? null}
        edges={data.edges}
        allNodes={data.nodes}
        onClose={handleClosePanel}
      />
    </div>
  );
}
