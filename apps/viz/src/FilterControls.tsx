import { Checkbox } from "@/components/ui/checkbox";
import { Separator } from "@/components/ui/separator";
import { cn } from "@/lib/utils";
import {
  EDGE_COLORS,
  EDGE_LABELS,
  EDGE_TYPES,
  LAYER_COLORS,
  LAYER_LABELS,
  LAYERS,
} from "./types";
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
    <section aria-label="图谱筛选" className="space-y-5 p-4">
      <div>
        <h2 className="text-sm font-semibold">图谱筛选</h2>
        <p className="mt-1 text-xs text-muted-foreground">
          按层级与关系组织当前视图
        </p>
      </div>
      <fieldset className="space-y-1">
        <legend className="mb-2 text-xs font-medium text-muted-foreground">
          概念层级
        </legend>
        {LAYERS.map((layer) => (
          <label
            key={layer}
            className={cn(
              "flex min-h-9 cursor-pointer items-center gap-2.5 rounded-md px-2 text-sm hover:bg-accent",
              !visibleLayers.has(layer) && "text-muted-foreground",
            )}
          >
            <Checkbox
              checked={visibleLayers.has(layer)}
              onCheckedChange={() => onToggleLayer(layer)}
              aria-label={LAYER_LABELS[layer]}
            />
            <span
              aria-hidden
              className="size-2 shrink-0 rounded-full"
              style={{ backgroundColor: LAYER_COLORS[layer] }}
            />
            <span>{LAYER_LABELS[layer]}</span>
            {layerCounts && (
              <span className="ml-auto text-xs tabular-nums text-muted-foreground">
                {layerCounts[`L${layer}`] ?? 0}
              </span>
            )}
          </label>
        ))}
      </fieldset>
      <Separator />
      <fieldset className="space-y-1">
        <legend className="mb-2 text-xs font-medium text-muted-foreground">
          关系类型
        </legend>
        {EDGE_TYPES.map((type) => (
          <label
            key={type}
            className={cn(
              "flex min-h-9 cursor-pointer items-center gap-2.5 rounded-md px-2 text-sm hover:bg-accent",
              !visibleEdgeTypes.has(type) && "text-muted-foreground",
            )}
          >
            <Checkbox
              checked={visibleEdgeTypes.has(type)}
              onCheckedChange={() => onToggleEdgeType(type)}
              aria-label={EDGE_LABELS[type]}
            />
            <span
              aria-hidden
              className="h-0.5 w-3 shrink-0"
              style={{ backgroundColor: EDGE_COLORS[type] }}
            />
            <span>{EDGE_LABELS[type]}</span>
          </label>
        ))}
      </fieldset>
    </section>
  );
}
