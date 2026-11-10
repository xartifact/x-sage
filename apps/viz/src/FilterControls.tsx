import { EDGE_COLORS, EDGE_LABELS, EDGE_TYPES, LAYER_COLORS, LAYER_LABELS, LAYERS } from "./types";
import type { Layer, EdgeType } from "./types";

interface FilterControlsProps {
  visibleLayers: Set<Layer>;
  visibleEdgeTypes: Set<EdgeType>;
  layerCounts?: Record<string, number> | undefined;
  onToggleLayer: (layer: Layer) => void;
  onToggleEdgeType: (type: EdgeType) => void;
}

export default function FilterControls({
  visibleLayers,
  visibleEdgeTypes,
  layerCounts,
  onToggleLayer,
  onToggleEdgeType,
}: FilterControlsProps) {
  return (
    <div className="filter-controls">
      <h3>筛选</h3>

      <div className="filter-group">
        <div className="filter-group-title">层级</div>
        {LAYERS.map((layer) => (
          <label key={layer} className="filter-item">
            <input
              type="checkbox"
              checked={visibleLayers.has(layer)}
              onChange={() => onToggleLayer(layer)}
            />
            <span
              className="filter-color-dot"
              style={{ backgroundColor: LAYER_COLORS[layer] }}
            />
            <span>{LAYER_LABELS[layer]}</span>
            {/* The count is the whole point: a hidden layer that silently holds
                most of the KB reads as "my new concepts never deployed". */}
            {layerCounts ? (
              <span className="filter-item-count">{layerCounts[`L${layer}`] ?? 0}</span>
            ) : null}
          </label>
        ))}
      </div>

      <div className="filter-group">
        <div className="filter-group-title">关系类型</div>
        {EDGE_TYPES.map((type) => (
          <label key={type} className="filter-item">
            <input
              type="checkbox"
              checked={visibleEdgeTypes.has(type)}
              onChange={() => onToggleEdgeType(type)}
            />
            <span
              className="filter-color-dot"
              style={{ backgroundColor: EDGE_COLORS[type] }}
            />
            <span>{EDGE_LABELS[type]}</span>
          </label>
        ))}
      </div>
    </div>
  );
}
