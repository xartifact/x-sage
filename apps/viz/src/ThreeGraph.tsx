import { useLayoutEffect, useMemo, useRef, useState, type PointerEvent } from "react";
import GraphMapControls from "./GraphMapControls";
import { graphLabel, showGraphLabel, type GraphLabelMode, type GraphSize } from "./graph-camera";
import { ThreeGraphScene, type LayoutNode } from "./three-graph-scene";
import { conceptName, LAYERS, type GraphEdge, type GraphNode, type Layer } from "./types";

export interface ThreeGraphProps {
  nodes: LayoutNode[];
  edges: GraphEdge[];
  size: GraphSize;
  labelMode: GraphLabelMode;
  selectedNodeId: string | null;
  onNodeClick: (node: GraphNode | null) => void;
  onUnavailable: () => void;
}

const clickTolerance = 6;

export default function ThreeGraph({
  nodes, edges, size, labelMode, selectedNodeId, onNodeClick, onUnavailable,
}: ThreeGraphProps) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const scene = useRef<ThreeGraphScene>(null);
  const labels = useRef(new Map<string, HTMLButtonElement>());
  const layerLabels = useRef(new Map<Layer, HTMLSpanElement>());
  const pointers = useRef(new Set<number>());
  const click = useRef<{ id: number; x: number; y: number } | null>(null);
  const [hover, setHover] = useState<{ id: string; x: number; y: number; text: string } | null>(null);
  const [focusedNode, setFocusedNode] = useState<string | null>(null);
  const highlight = hover?.id ?? focusedNode ?? selectedNodeId;
  const neighbors = useMemo(() => {
    const ids = new Set<string>();
    if (!highlight) return ids;
    ids.add(highlight);
    for (const edge of edges) {
      if (edge.from === highlight) ids.add(edge.target);
      if (edge.target === highlight) ids.add(edge.from);
    }
    return ids;
  }, [edges, highlight]);

  useLayoutEffect(() => {
    const element = canvas.current;
    if (!element) return;
    let graph: ThreeGraphScene;
    function placeLabels() {
      for (const [id, label] of labels.current) {
        const point = graph.projectLabel(id);
        label.dataset.onScreen = String(!!point);
        if (point) {
          label.style.transform = "translate3d(" + point.x + "px," + point.y + "px,0)";
          label.style.setProperty("--sage-name-offset", point.offset + "px");
        }
      }
      for (const [layer, label] of layerLabels.current) {
        const point = graph.projectLayer(layer);
        label.hidden = !point;
        if (point) label.style.transform = "translate3d(" + point.x + "px," + point.y + "px,0) translate(-125%, -50%)";
      }
    }
    try {
      graph = new ThreeGraphScene(element, placeLabels);
    } catch {
      onUnavailable();
      return;
    }
    scene.current = graph;
    const syncTheme = () => graph.setSelectionColor(getComputedStyle(element).getPropertyValue("--xa-brand").trim());
    syncTheme();
    const appearance = new MutationObserver(syncTheme);
    appearance.observe(document.documentElement, { attributes: true, attributeFilter: ["class", "data-xartifact-palette"] });
    const lost = () => onUnavailable();
    element.addEventListener("webglcontextlost", lost);
    return () => {
      element.removeEventListener("webglcontextlost", lost);
      appearance.disconnect();
      scene.current = null;
      graph.dispose();
    };
  }, [onUnavailable]);

  useLayoutEffect(() => {
    scene.current?.updateLayout(nodes, edges, size, selectedNodeId, highlight, neighbors);
  }, [nodes, edges, size, selectedNodeId, highlight, neighbors]);

  function pick(event: PointerEvent<HTMLCanvasElement>) {
    const box = event.currentTarget.getBoundingClientRect();
    if (!box.width || !box.height) return null;
    return scene.current?.pick((event.clientX - box.left) / box.width,
      (event.clientY - box.top) / box.height) ?? null;
  }

  function moved(event: PointerEvent<HTMLCanvasElement>) {
    const start = click.current;
    return start?.id !== event.pointerId
      || Math.hypot(event.clientX - start.x, event.clientY - start.y) > clickTolerance;
  }

  function endPointer(event: PointerEvent<HTMLCanvasElement>) {
    if (!moved(event) && pointers.current.size === 1 && event.type === "pointerup") {
      onNodeClick(pick(event));
    }
    pointers.current.delete(event.pointerId);
    click.current = null;
  }

  return (
    <>
      <GraphMapControls hasNodes={nodes.length > 0}
        zoom={(factor) => scene.current?.zoom(factor)}
        fit={() => scene.current?.fit()}
        reset={() => scene.current?.fit(true)}
        focus={() => selectedNodeId && scene.current?.focus(selectedNodeId)}
        hasSelection={nodes.some((node) => node.id === selectedNodeId)}
      />
      <canvas
        ref={canvas}
        className="sage-webgl-canvas"
        tabIndex={0}
        aria-label="三维知识概念关系图"
        onPointerDown={(event) => {
          pointers.current.add(event.pointerId);
          click.current = pointers.current.size === 1 && event.button === 0
            ? { id: event.pointerId, x: event.clientX, y: event.clientY } : null;
          setHover(null);
        }}
        onPointerMove={(event) => {
          if (pointers.current.size) {
            if (moved(event)) click.current = null;
            return;
          }
          if (event.pointerType === "touch") return;
          const node = pick(event);
          setHover((previous) => {
            if (!node) return null;
            if (previous?.id === node.id) return previous;
            return { id: node.id, x: event.clientX, y: event.clientY, text: node.atom };
          });
        }}
        onPointerUp={endPointer}
        onPointerCancel={endPointer}
        onLostPointerCapture={endPointer}
        onPointerLeave={() => setHover(null)}
        onKeyDown={(event) => {
          if (event.key === "+" || event.key === "-") {
            event.preventDefault();
            scene.current?.zoom(event.key === "+" ? 0.8 : 1.25);
          }
          if (event.key === "Escape") onNodeClick(null);
        }}
      />
      <div className="sage-webgl-labels" role="group" aria-label="三维图谱概念">
        {LAYERS.map((layer) => (
          <span key={layer} aria-hidden="true" hidden className="sage-webgl-layer"
            ref={(element) => {
              if (element) layerLabels.current.set(layer, element);
              else layerLabels.current.delete(layer);
            }}>L{layer}</span>
        ))}
        {nodes.map((node) => {
          const selected = selectedNodeId === node.id;
          const visible = showGraphLabel(labelMode, node.layer, nodes.length <= 24,
            node.id === highlight || selected, !highlight || neighbors.has(node.id));
          return (
            <button key={node.id} type="button" className="sage-webgl-label"
              ref={(element) => {
                if (element) labels.current.set(node.id, element);
                else labels.current.delete(node.id);
              }}
              data-id={node.id}
              data-show-name={visible}
              aria-label={conceptName(node)}
              aria-pressed={selected}
              onClick={() => onNodeClick(node)}
              onFocus={() => {
                setFocusedNode(node.id);
                scene.current?.focus(node.id, false);
              }}
              onBlur={() => setFocusedNode(null)}
              onKeyDown={(event) => {
                if (event.key === "Enter" || event.key === " ") {
                  event.preventDefault();
                  onNodeClick(node);
                }
                if (event.key === "Escape") onNodeClick(null);
              }}>
              <span aria-hidden="true">{graphLabel(node)}</span>
            </button>
          );
        })}
      </div>
      {hover && (
        <div
          className="pointer-events-none fixed z-50 max-w-72 rounded-lg border bg-popover p-3 text-xs leading-relaxed text-popover-foreground shadow-lg"
          style={{ left: Math.max(8, Math.min(hover.x + 16, window.innerWidth - 300)), top: Math.max(8, hover.y - 8) }}
          lang="zh-CN"
        >
          {hover.text}
        </div>
      )}
    </>
  );
}
