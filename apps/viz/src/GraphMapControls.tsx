import { Focus, Maximize2, Minus, Plus, RotateCcw } from "lucide-react";
import { Button } from "@/components/ui/button";

interface MapControlsProps {
  hasNodes: boolean;
  zoom: (factor: number) => void;
  fit: () => void;
  reset: () => void;
  focus?: (() => void) | undefined;
  hasSelection?: boolean;
}

export default function GraphMapControls({ hasNodes, zoom, fit, reset, focus, hasSelection }: MapControlsProps) {
  return (
    <div className="sage-map-controls">
      <Button variant="ghost" size="icon" onClick={() => zoom(0.8)} disabled={!hasNodes}
        aria-label="放大图谱" title="放大图谱"><Plus className="size-4" /></Button>
      <Button variant="ghost" size="icon" onClick={() => zoom(1.25)} disabled={!hasNodes}
        aria-label="缩小图谱" title="缩小图谱"><Minus className="size-4" /></Button>
      <Button variant="ghost" size="icon" onClick={fit} disabled={!hasNodes}
        aria-label="适应全部节点" title="适应全部节点"><Maximize2 className="size-4" /></Button>
      {focus && (
        <Button variant="ghost" size="icon" onClick={focus} disabled={!hasSelection}
          aria-label="定位所选概念" title="定位所选概念"><Focus className="size-4" /></Button>
      )}
      <Button variant="ghost" size="icon" onClick={reset}
        aria-label="重置视图" title="重置视图"><RotateCcw className="size-4" /></Button>
    </div>
  );
}
