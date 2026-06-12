import { LAYER_COLORS, LAYER_LABELS, EDGE_COLORS, EDGE_LABELS } from "./types";
import type { Layer, EdgeType } from "./types";

interface FilterControlsProps {
  visibleLayers: Set<Layer>;
  visibleEdgeTypes: Set<EdgeType>;
  onToggleLayer: (layer: Layer) => void;
  onToggleEdgeType: (type: EdgeType) => void;
}

const LAYERS: Layer[] = [0, 1, 2, 3];
const EDGE_TYPES: EdgeType[] = ["enables", "depends-on", "contradicts", "part-of"];

export default function FilterControls({
  visibleLayers,
  visibleEdgeTypes,
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
