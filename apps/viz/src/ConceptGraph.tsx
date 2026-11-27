import { lazy, Suspense, useRef, useEffect, useState, useCallback, useMemo, useId, type ComponentType } from "react";
import { Network } from "lucide-react";
import * as d3 from "d3";
import GraphMapControls from "./GraphMapControls";
import type { ThreeGraphProps } from "./ThreeGraph";
import { conceptName, LAYER_COLORS, LAYERS, NODE_RADIUS, EDGE_COLORS } from "./types";
import type { GraphNode, GraphEdge, EdgeType } from "./types";
import {
  defaultGraphCamera, graphLabel, graphProjection, panGraphView, rotateGraph, showGraphLabel,
  type GraphCamera, type GraphLabelMode, type GraphPoint, type GraphView, type GraphSize,
} from "./graph-camera";

interface SimulationNode extends GraphNode, d3.SimulationNodeDatum {
  id: string;
}

interface SimulationEdge extends d3.SimulationLinkDatum<SimulationNode> {
  from: string;
  type: EdgeType;
  confidence: string;
}

type ClientPoint = Pick<MouseEvent, "clientX" | "clientY">;

interface ConceptGraphProps {
  mode: "2d" | "3d";
  labelMode: GraphLabelMode;
  fallingNodes: ReadonlySet<string> | null;
  threeUnavailable: boolean;
  onThreeUnavailable: (unavailable: boolean) => void;
  nodes: GraphNode[];
  edges: GraphEdge[];
  onNodeClick: (node: GraphNode | null) => void;
  selectedNodeId: string | null;
}

function UnavailableThreeGraph({ onUnavailable }: ThreeGraphProps) {
  useEffect(onUnavailable, [onUnavailable]);
  return null;
}

const ThreeGraph = lazy<ComponentType<ThreeGraphProps>>(
  () => import("./ThreeGraph").catch(() => ({ default: UnavailableThreeGraph })),
);

function getNodeId(d: string | number | SimulationNode): string {
  if (typeof d === "string") return d;
  if (typeof d === "number") return String(d);
  return d.id;
}

function fittedView(points: GraphPoint[], padding: number): GraphView {
  let left = Infinity, top = Infinity, right = -Infinity, bottom = -Infinity;
  for (const point of points) {
    left = Math.min(left, point.x);
    top = Math.min(top, point.y);
    right = Math.max(right, point.x);
    bottom = Math.max(bottom, point.y);
  }
  return { x: left - padding, y: top - padding, width: right - left + 2 * padding, height: bottom - top + 2 * padding };
}

function layerPlanes(nodes: SimulationNode[], project: ReturnType<typeof graphProjection>, size: GraphSize) {
  if (!nodes.length) return [];
  // Use the graph's footprint so the grid stays under the nodes even in filtered views.
  const bounds = fittedView(nodes.map((node) => ({ x: node.x ?? 0, y: node.y ?? 0 })), 44);
  const { x, y } = bounds;
  const width = Math.max(bounds.width, size.width * 0.34);
  const height = Math.max(bounds.height, size.height * 0.34);
  const left = x - (width - bounds.width) / 2, top = y - (height - bounds.height) / 2;
  return LAYERS.filter((layer) => nodes.some((node) => node.layer === layer)).map((layer) => {
    const at = (u: number, v: number) => project(left + width * u, top + height * v, layer);
    return {
      layer,
      corners: [at(0, 0), at(1, 0), at(1, 1), at(0, 1)],
      grid: [0.25, 0.5, 0.75].flatMap((step) => [
        [at(step, 0), at(step, 1)], [at(0, step), at(1, step)],
      ] as const),
    };
  });
}

function fitGraph(nodes: SimulationNode[], dimensions: GraphSize, camera?: GraphCamera) {
  if (!nodes.length) return null;
  const project = camera && graphProjection(camera, dimensions);
  const points = nodes.map((node) => {
    const x = node.x ?? 0, y = node.y ?? 0;
    return project ? project(x, y, node.layer) : { x, y };
  });
  return fittedView(project ? [...points, ...layerPlanes(nodes, project, dimensions).flatMap((plane) => plane.corners)] : points, project ? 55 : 96);
}

export default function ConceptGraph({
  mode,
  labelMode,
  fallingNodes,
  threeUnavailable,
  onThreeUnavailable,
  nodes,
  edges,
  onNodeClick,
  selectedNodeId,
}: ConceptGraphProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const showThree = mode === "3d" && !threeUnavailable;
  const useSvgFallback = useCallback(() => onThreeUnavailable(true), [onThreeUnavailable]);
  const positions = useRef(new Map<string, { x: number; y: number }>());
  const viewport = useRef("");
  const [camera, setCamera] = useState(defaultGraphCamera);
  const drag = useRef<{
    id: number;
    point: ClientPoint;
    initial: GraphCamera;
    moved: boolean;
    node: SimulationNode | undefined;
  } | null>(null);
  const touchPoints = useRef(new Map<number, ClientPoint>());
  const pinchDistance = useRef<number | null>(null);
  const suppressClick = useRef(false);
  const [view, setView] = useState<GraphView | null>(null);
  const svgRef = useRef<SVGSVGElement>(null);
  const paintId = useId();
  const simulationRef = useRef<d3.Simulation<
    SimulationNode,
    SimulationEdge
  > | null>(null);
  const [simNodes, setSimNodes] = useState<SimulationNode[]>([]);
  const [simEdges, setSimEdges] = useState<SimulationEdge[]>([]);
  const [hoveredNodeId, setHoveredNodeId] = useState<string | null>(null);
  const [tooltip, setTooltip] = useState<{
    x: number;
    y: number;
    text: string;
  } | null>(null);
  const [dimensions, setDimensions] = useState({ width: 640, height: 480 });

  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    const observer = new ResizeObserver((entries) => {
      const box = entries[0]?.contentRect;
      if (box && box.width > 0 && box.height > 0)
        setDimensions({ width: box.width, height: box.height });
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  // Initialize / update simulation
  useEffect(() => {
    const liveIds = new Set(nodes.map((node) => node.id));
    for (const id of positions.current.keys())
      if (!liveIds.has(id)) positions.current.delete(id);
    if (nodes.length === 0) {
      setSimNodes([]);
      setSimEdges([]);
      setHoveredNodeId(null);
      setTooltip(null);
      return;
    }

    const width = dimensions.width;
    const height = dimensions.height;

    const simNodes: SimulationNode[] = nodes.map((n) => ({
      ...n,
      x:
        positions.current.get(n.id)?.x ??
        width / 2 + (Math.random() - 0.5) * 100,
      y:
        positions.current.get(n.id)?.y ??
        height / 2 + (Math.random() - 0.5) * 100,
    }));

    const simEdges: SimulationEdge[] = edges.map((e) => ({
      ...e,
      source: e.from,
      target: e.target,
    }));

    const simulation = d3
      .forceSimulation<SimulationNode>(simNodes)
      .force(
        "link",
        d3
          .forceLink<SimulationNode, SimulationEdge>(simEdges)
          .id((d) => d.id)
          .distance((d) => (d.type === "contradicts" ? 180 : 120))
          .strength(0.5),
      )
      .force("charge", d3.forceManyBody().strength(-400))
      .force("center", d3.forceCenter(width / 2, height / 2))
      .force(
        "collide",
        d3
          .forceCollide<SimulationNode>()
          .radius((d) => NODE_RADIUS[d.layer] + 12)
          .strength(0.8),
      )
      .force("x", d3.forceX(width / 2).strength(0.05))
      .force("y", d3.forceY(height / 2).strength(0.05))
      .alphaDecay(0.02)
      .velocityDecay(0.4);

    const publish = () => {
      for (const node of simNodes)
        positions.current.set(node.id, { x: node.x ?? 0, y: node.y ?? 0 });
      setSimNodes([...simNodes]);
      setSimEdges([...simEdges]);
    };
    // Background tabs can defer animation frames indefinitely. Publish a useful
    // initial layout without waiting for D3 timer delivery.
    simulation.tick(30);
    publish();
    simulation.on("tick", publish);

    simulationRef.current = simulation;

    return () => {
      simulation.stop();
    };
  }, [nodes, edges, dimensions]);

  useEffect(() => {
    const viewportKey = `${mode}:${dimensions.width}:${dimensions.height}`;
    if (showThree || !simNodes.length || viewport.current === viewportKey) return;
    setCamera(defaultGraphCamera);
    const initialCamera = mode === "3d" ? defaultGraphCamera : undefined;
    setView(initialCamera || dimensions.width <= 640 ? fitGraph(simNodes, dimensions, initialCamera) : null);
    viewport.current = viewportKey;
  }, [dimensions, mode, showThree, simNodes]);

  useEffect(() => {
    const dragged = drag.current?.node;
    if (dragged) {
      dragged.fx = null;
      dragged.fy = null;
      simulationRef.current?.alphaTarget(0);
    }
    drag.current = null;
    touchPoints.current.clear();
    pinchDistance.current = null;
    suppressClick.current = false;
    setHoveredNodeId(null);
    setTooltip(null);
  }, [mode]);

  const project = graphProjection(camera, dimensions);
  const planes = mode === "3d" && !showThree ? layerPlanes(simNodes, project, dimensions) : [];
  const plottedNodes = (showThree ? [] : simNodes).map((node) => {
    const x = node.x ?? 0, y = node.y ?? 0;
    return { node, ...(mode === "3d" ? project(x, y, node.layer) : { x, y, scale: 1, depth: 0 }) };
  });
  if (mode === "3d") plottedNodes.sort((a, b) => b.depth - a.depth);
  const plottedById = new Map(plottedNodes.map((point) => [point.node.id, point]));
  const selectedPoint = selectedNodeId ? plottedById.get(selectedNodeId) : undefined;
  const showAllLabels = mode === "3d" ? nodes.length <= 24 : dimensions.width > 460;

  function resetView() {
    setCamera(defaultGraphCamera);
    setView(fitGraph(simNodes, dimensions, mode === "3d" ? defaultGraphCamera : undefined));
  }

  function focusSelection() {
    if (!selectedPoint) return;
    const width = dimensions.width * 0.6, height = dimensions.height * 0.6;
    setView({ x: selectedPoint.x - width / 2, y: selectedPoint.y - height / 2, width, height });
  }

  const defaultView = useMemo(() => ({ x: 0, y: 0, ...dimensions }), [dimensions]);
  const currentView = view ?? defaultView;
  const screenDelta = useCallback((dx: number, dy: number) => {
    const matrix = svgRef.current?.getScreenCTM()?.inverse();
    // A delta transforms as a vector: omit matrix translation.
    return matrix ? { x: matrix.a * dx + matrix.c * dy, y: matrix.b * dx + matrix.d * dy } : { x: dx, y: dy };
  }, []);
  const pan = useCallback((dx: number, dy: number) => {
    const offset = screenDelta(dx, dy);
    setView((previous) => panGraphView(previous ?? defaultView, offset.x, offset.y));
  }, [defaultView, screenDelta]);
  const zoom = useCallback((factor: number, point?: ClientPoint) => {
    const svg = svgRef.current;
    let anchorX = 0.5;
    let anchorY = 0.5;
    if (svg && point) {
      const matrix = svg.getScreenCTM();
      const box = svg.viewBox.baseVal;
      if (matrix && box.width > 0 && box.height > 0) {
        const anchor = new DOMPoint(point.clientX, point.clientY).matrixTransform(matrix.inverse());
        anchorX = (anchor.x - box.x) / box.width;
        anchorY = (anchor.y - box.y) / box.height;
      }
    }
    setView((previous) => {
      const { x, y, width, height } = previous ?? defaultView;
      const nextWidth = Math.min(dimensions.width * 12, Math.max(dimensions.width / 6, width * factor));
      if (nextWidth === width) return previous;
      const nextHeight = height * (nextWidth / width);
      return { x: x + (width - nextWidth) * anchorX, y: y + (height - nextHeight) * anchorY,
        width: nextWidth, height: nextHeight };
    });
  }, [defaultView, dimensions]);

  function finishDrag() {
    const active = drag.current;
    if (active?.moved && active.node) {
      active.node.fx = null;
      active.node.fy = null;
      simulationRef.current?.alphaTarget(0);
    }
    drag.current = null;
  }

  function measurePinch() {
    const [first, second] = touchPoints.current.values();
    if (!first || !second) return null;
    return {
      distance: Math.hypot(first.clientX - second.clientX, first.clientY - second.clientY),
      anchor: { clientX: (first.clientX + second.clientX) / 2, clientY: (first.clientY + second.clientY) / 2 },
    };
  }

  function startPointer(event: React.PointerEvent<SVGSVGElement>) {
    if (!simNodes.length || event.button !== 0) return;
    const point = { clientX: event.clientX, clientY: event.clientY };
    if (event.pointerType === "touch") {
      touchPoints.current.set(event.pointerId, point);
      if (touchPoints.current.size > 1) {
        finishDrag();
        suppressClick.current = true;
        for (const id of touchPoints.current.keys()) event.currentTarget.setPointerCapture(id);
        if (touchPoints.current.size === 2) pinchDistance.current = measurePinch()?.distance ?? null;
        return;
      }
    }
    if (!event.isPrimary) return;
    const target = event.target instanceof Element && event.target.closest<SVGGElement>("[data-graph-node]");
    const node = mode === "2d" && target ? plottedById.get(target.dataset.graphNode ?? "")?.node : undefined;
    suppressClick.current = false;
    drag.current = { id: event.pointerId, point, initial: camera, moved: false, node };
  }

  function movePointer(event: React.PointerEvent<SVGSVGElement>) {
    const point = { clientX: event.clientX, clientY: event.clientY };
    if (touchPoints.current.has(event.pointerId)) {
      touchPoints.current.set(event.pointerId, point);
      const pinch = measurePinch();
      if (pinch) {
        if (pinch.distance > 0 && pinchDistance.current) zoom(pinchDistance.current / pinch.distance, pinch.anchor);
        pinchDistance.current = pinch.distance;
        return;
      }
    }
    const active = drag.current;
    if (!active || active.id !== event.pointerId) return;
    const dx = point.clientX - active.point.clientX, dy = point.clientY - active.point.clientY;
    if (!active.moved) {
      if (Math.hypot(dx, dy) < 4) return;
      active.moved = true;
      suppressClick.current = true;
      event.currentTarget.setPointerCapture(event.pointerId);
      if (active.node) simulationRef.current?.alphaTarget(0.2).restart();
    }
    if (mode === "3d") {
      setCamera(rotateGraph(active.initial, dx, dy));
      return;
    }
    if (active.node) {
      const offset = screenDelta(dx, dy);
      active.node.fx = (active.node.fx ?? active.node.x ?? 0) + offset.x;
      active.node.fy = (active.node.fy ?? active.node.y ?? 0) + offset.y;
    } else {
      pan(dx, dy);
    }
    active.point = point;
  }

  function endPointer(event: React.PointerEvent<SVGSVGElement>) {
    if (touchPoints.current.delete(event.pointerId)) pinchDistance.current = measurePinch()?.distance ?? null;
    if (drag.current?.id === event.pointerId) finishDrag();
  }

  const hasGraph = simNodes.length > 0;
  useEffect(() => {
    const svg = svgRef.current;
    if (!svg || !hasGraph) return;
    const doc = svg.ownerDocument;
    let pointer: ClientPoint | undefined;
    let gesture: { scale: number; anchor: ClientPoint | undefined } | undefined;
    const trackPointer = ({ clientX, clientY }: PointerEvent) => { pointer = { clientX, clientY }; };
    const clearPointer = () => { pointer = undefined; };

    const wheel = (event: WheelEvent) => {
      const { deltaMode, deltaX, deltaY } = event;
      if (!event.ctrlKey) {
        if (mode !== "2d" || event.metaKey || (!deltaX && !deltaY)) return;
        event.preventDefault();
        const unit = deltaMode === WheelEvent.DOM_DELTA_LINE ? 16
          : deltaMode === WheelEvent.DOM_DELTA_PAGE ? svg.clientHeight : 1;
        pan(-deltaX * unit, -deltaY * unit);
        return;
      }
      if (deltaY === 0) return;
      event.preventDefault();
      if (gesture || touchPoints.current.size > 1) return;
      const step = deltaMode === WheelEvent.DOM_DELTA_LINE ? 0.5
        : deltaMode === WheelEvent.DOM_DELTA_PAGE ? 10 : 0.02;
      zoom(2 ** (deltaY * step), event);
    };

    // WebKit can send gesture events to the document. Claim a gesture only
    // when it begins over the SVG, and keep using its initial anchor.
    const startGesture = (event: Event) => {
      gesture = undefined;
      const target = event.target;
      const onSvg = target instanceof Node && svg.contains(target);
      const documentTarget = target === doc || target === doc.body || target === doc.documentElement;
      if (!onSvg && !(documentTarget && pointer &&
        svg.contains(doc.elementFromPoint(pointer.clientX, pointer.clientY)))) return;
      event.preventDefault();
      gesture = { scale: 1, anchor: pointer };
    };
    const changeGesture = (event: Event) => {
      if (!gesture) return;
      event.preventDefault();
      if (touchPoints.current.size > 1) return;
      const { scale } = event as Event & { scale: number };
      if (!Number.isFinite(scale) || scale <= 0) return;
      zoom(gesture.scale / scale, gesture.anchor);
      gesture.scale = scale;
    };
    const endGesture = (event: Event) => {
      if (gesture) event.preventDefault();
      gesture = undefined;
    };

    svg.addEventListener("pointerenter", trackPointer);
    svg.addEventListener("pointermove", trackPointer);
    svg.addEventListener("pointerleave", clearPointer);
    svg.addEventListener("wheel", wheel, { passive: false });
    doc.addEventListener("gesturestart", startGesture, { passive: false });
    doc.addEventListener("gesturechange", changeGesture, { passive: false });
    doc.addEventListener("gestureend", endGesture, { passive: false });
    return () => {
      svg.removeEventListener("pointerenter", trackPointer);
      svg.removeEventListener("pointermove", trackPointer);
      svg.removeEventListener("pointerleave", clearPointer);
      svg.removeEventListener("wheel", wheel);
      doc.removeEventListener("gesturestart", startGesture);
      doc.removeEventListener("gesturechange", changeGesture);
      doc.removeEventListener("gestureend", endGesture);
    };
  }, [hasGraph, mode, pan, showThree, zoom]);

  const highlightedNodeId = hoveredNodeId ?? selectedNodeId;

  // Keep a selected concept and its connections in context after the pointer leaves.
  const neighborSet = useMemo(() => {
    if (showThree || !highlightedNodeId) return new Set<string>();
    const neighbors = new Set<string>([highlightedNodeId]);
    simEdges.forEach((e) => {
      const src = getNodeId(e.source);
      const tgt = getNodeId(e.target);
      if (src === highlightedNodeId) neighbors.add(tgt);
      if (tgt === highlightedNodeId) neighbors.add(src);
    });
    return neighbors;
  }, [highlightedNodeId, showThree, simEdges]);

  const edgeHighlighted = useMemo(() => {
    if (showThree || !highlightedNodeId) return new Set<number>();
    const highlighted = new Set<number>();
    simEdges.forEach((e, idx) => {
      const src = getNodeId(e.source);
      const tgt = getNodeId(e.target);
      if (src === highlightedNodeId || tgt === highlightedNodeId) {
        highlighted.add(idx);
      }
    });
    return highlighted;
  }, [highlightedNodeId, showThree, simEdges]);

  const handleSvgClick = useCallback(
    (e: React.MouseEvent) => {
      if (e.target === svgRef.current) {
        onNodeClick(null);
      }
    },
    [onNodeClick],
  );

  const handleNodeHover = useCallback(
    (node: SimulationNode, event: React.MouseEvent) => {
      setHoveredNodeId(node.id);
      setTooltip({
        x: Math.min(event.clientX + 16, window.innerWidth - 300),
        y: Math.max(8, event.clientY - 8),
        text: node.atom,
      });
    },
    [],
  );

  const handleNodeLeave = useCallback(() => {
    setHoveredNodeId(null);
    setTooltip(null);
  }, []);

  if (showThree && nodes.length) {
    return (
      <div ref={containerRef} className="sage-graph-canvas" data-mode={mode}>
        <Suspense fallback={<div className="sage-graph-status" role="status">正在加载三维图谱…</div>}>
          <ThreeGraph
            nodes={simNodes}
            edges={edges}
            size={dimensions}
            labelMode={labelMode}
            fallingNodes={fallingNodes}
            selectedNodeId={selectedNodeId}
            onNodeClick={onNodeClick}
            onUnavailable={useSvgFallback}
          />
        </Suspense>
      </div>
    );
  }

  return (
    <div
      ref={containerRef}
      className="sage-graph-canvas"
      data-mode={mode}
    >
      <GraphMapControls
        hasNodes={hasGraph}
        zoom={zoom}
        fit={() => setView(fitGraph(simNodes, dimensions, mode === "3d" ? camera : undefined))}
        focus={mode === "3d" ? focusSelection : undefined}
        hasSelection={!!selectedPoint}
        reset={resetView}
      />
      {mode === "3d" && threeUnavailable && (
        <span className="sage-graph-fallback" role="status">使用兼容渲染 · 物理模拟需 WebGL 2</span>
      )}
      {nodes.length === 0 && (
        <div className="pointer-events-none absolute inset-0 flex flex-col items-center justify-center gap-3 p-6 text-center text-muted-foreground">
          <Network className="size-8" />
          <p className="text-sm">当前视图没有概念</p>
          <p className="text-xs">调整领域、关键词或筛选条件。</p>
        </div>
      )}
      <svg
        ref={svgRef}
        className="graph-svg h-full w-full touch-none"
        aria-label={mode === "3d" ? "三维知识概念关系图" : "知识概念关系图"}
        tabIndex={0}
        viewBox={`${currentView.x} ${currentView.y} ${currentView.width} ${currentView.height}`}
        onPointerDown={startPointer}
        onPointerMove={movePointer}
        onPointerUp={endPointer}
        onPointerCancel={endPointer}
        onLostPointerCapture={(event) => {
          if (event.target === event.currentTarget) endPointer(event);
        }}
        onPointerLeave={() => {
          if (drag.current && !drag.current.moved && !touchPoints.current.size) finishDrag();
        }}
        onClickCapture={(event) => {
          if (event.detail > 0 && suppressClick.current) {
            suppressClick.current = false;
            event.stopPropagation();
          }
        }}
        onClick={handleSvgClick}
        onKeyDown={(event) => {
          if (!hasGraph) return;
          const dx = event.key === "ArrowLeft" ? -1 : event.key === "ArrowRight" ? 1 : 0;
          const dy = event.key === "ArrowUp" ? -1 : event.key === "ArrowDown" ? 1 : 0;
          if (dx || dy) {
            event.preventDefault();
            if (mode === "3d") setCamera((previous) => rotateGraph(previous, dx * 20, dy * 20));
            else pan(dx * 44, dy * 44);
          }
        }}
      >
        <defs>
          {/* Arrow markers for each edge type */}
          {Object.entries(EDGE_COLORS).map(([type, color]) => (
            <marker
              key={type}
              id={`${paintId}-arrow-${type}`}
              viewBox="0 -5 10 10"
              refX={9}
              refY={0}
              /* Symmetric relations render undirected; an arrowhead would imply
                 a direction the relation does not have. */
              markerWidth={
                type === "contradicts" || type === "analogous-to" ? 0 : 6
              }
              markerHeight={6}
              orient="auto"
            >
              <path d="M0,-5L10,0L0,5" fill={color} />
            </marker>
          ))}
          {mode === "3d" && LAYERS.map((layer) => (
            <radialGradient key={layer} id={`${paintId}-node-${layer}`} cx="33%" cy="27%" r="73%" color={LAYER_COLORS[layer]}>
              <stop offset="0" className="sage-node-light" />
              <stop offset="0.55" stopColor="currentColor" />
              <stop offset="1" className="sage-node-shade" />
            </radialGradient>
          ))}
        </defs>

        {planes.length > 0 && (
          <g aria-hidden="true" pointerEvents="none">
            {planes.length > 1 && planes[0]!.corners.map((from, index) => {
              const to = planes[planes.length - 1]!.corners[index]!;
              return <line key={index} className="sage-layer-guide" x1={from.x} y1={from.y} x2={to.x} y2={to.y} />;
            })}
            {planes.map(({ layer, corners, grid }) => {
              const label = corners.reduce((left, point) => point.x < left.x ? point : left);
              return (
                <g key={layer} color={LAYER_COLORS[layer]}>
                  <polygon className="sage-layer-plane" points={corners.map(({ x, y }) => `${x},${y}`).join(" ")} />
                  {grid.map(([from, to], index) => (
                    <line key={index} className="sage-layer-grid" x1={from.x} y1={from.y} x2={to.x} y2={to.y} />
                  ))}
                  <text className="sage-layer-index" x={label.x - 8} y={label.y + 4}>L{layer}</text>
                </g>
              );
            })}
          </g>
        )}

        {/* Edges */}
        <g pointerEvents="none">
          {simEdges.map((edge, idx) => ({ edge, idx })).sort((a, b) =>
            Number(edgeHighlighted.has(a.idx)) - Number(edgeHighlighted.has(b.idx)),
          ).map(({ edge, idx }) => {
            const srcCoords = plottedById.get(getNodeId(edge.source));
            const tgtCoords = plottedById.get(getNodeId(edge.target));
            if (!srcCoords || !tgtCoords) return null;

            const isHighlighted = edgeHighlighted.has(idx);
            const isDimmed = highlightedNodeId && !isHighlighted;
            const depthScale = Math.max(0.65, Math.min(1.25, (srcCoords.scale + tgtCoords.scale) / 2));
            const opacity = isDimmed ? 0.10 : isHighlighted ? 1 : 0.47 * depthScale;
            const strokeWidth = isHighlighted ? 2.25 : 1.25 * depthScale;
            const dx = tgtCoords.x - srcCoords.x;
            const dy = tgtCoords.y - srcCoords.y;
            const targetRadius = NODE_RADIUS[tgtCoords.node.layer] * 0.54 * tgtCoords.scale;
            const length = Math.hypot(dx, dy);
            const trim = length > targetRadius ? targetRadius / length : 0;

            return (
              <line
                key={`${edge.from}-${edge.type}-${getNodeId(edge.target)}-${idx}`}
                className="graph-edge"
                x1={srcCoords.x}
                y1={srcCoords.y}
                x2={tgtCoords.x - dx * trim}
                y2={tgtCoords.y - dy * trim}
                stroke={EDGE_COLORS[edge.type]}
                strokeWidth={strokeWidth}
                strokeOpacity={opacity}
                strokeDasharray={
                  edge.type === "contradicts"
                    ? "6,4"
                    : edge.type === "analogous-to"
                      ? "2,4"
                      : undefined
                }
                markerEnd={`url(#${paintId}-arrow-${edge.type})`}
              />
            );
          })}
        </g>

        {/* Nodes */}
        <g>
          {plottedNodes.map(({ node, x, y, scale }) => {
            const isHovered = hoveredNodeId === node.id;
            const isNeighbor = neighborSet.has(node.id);
            const isSelected = selectedNodeId === node.id;
            const isDimmed = highlightedNodeId && !isNeighbor;
            const opacity = isDimmed ? 0.3 : 1.0;
            const showLabel = showGraphLabel(mode === "2d" ? "auto" : labelMode,
              node.layer, showAllLabels, isHovered || isSelected, !isDimmed);
            const radius = NODE_RADIUS[node.layer] * 0.54 * scale;

            return (
              <g
                key={node.id}
                data-graph-node={node.id}
                transform={`translate(${x}, ${y})`}
                onMouseEnter={(e) => handleNodeHover(node, e)}
                onMouseLeave={handleNodeLeave}
                role="button"
                tabIndex={0}
                aria-label={conceptName(node)}
                aria-pressed={isSelected}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    onNodeClick(node);
                  }
                  if (e.key === "Escape") onNodeClick(null);
                }}
                onFocus={() => setHoveredNodeId(node.id)}
                onBlur={handleNodeLeave}
                onClick={(e) => {
                  e.stopPropagation();
                  onNodeClick(node);
                }}
              >
                <g className="sage-node-glyph" opacity={opacity}>
                  <circle className="sage-node-halo" r={radius + (isSelected ? 8 : 5)}
                    fill={LAYER_COLORS[node.layer]} opacity={isSelected ? 0.32 : isHovered ? 0.24 : 0.12} />
                  <circle className="node-circle" r={radius}
                    fill={mode === "3d" ? `url(#${paintId}-node-${node.layer})` : LAYER_COLORS[node.layer]} />
                </g>
                {showLabel && (
                  <text className="node-label" y={radius + 13} opacity={isDimmed ? 0.85 : 1}>
                    {graphLabel(node)}
                  </text>
                )}
              </g>
            );
          })}
        </g>
      </svg>

      {/* Tooltip */}
      {tooltip && (
        <div
          className="pointer-events-none fixed z-50 max-w-72 rounded-lg border bg-popover p-3 text-xs leading-relaxed text-popover-foreground shadow-lg"
          style={{ left: tooltip.x, top: tooltip.y }}
          lang="zh-CN"
        >
          {tooltip.text}
        </div>
      )}
    </div>
  );
}
