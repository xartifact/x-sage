import { useRef, useEffect, useState, useCallback, useMemo } from "react";
import { Button } from "@/components/ui/button";
import { Maximize2, RotateCcw, Network } from "lucide-react";
import * as d3 from "d3";
import { LAYER_COLORS, NODE_RADIUS, EDGE_COLORS } from "./types";
import type { GraphNode, GraphEdge, Layer, EdgeType } from "./types";

interface SimulationNode extends GraphNode, d3.SimulationNodeDatum {
  id: string;
}

interface SimulationEdge extends d3.SimulationLinkDatum<SimulationNode> {
  from: string;
  type: EdgeType;
  target: string;
  confidence: string;
}

interface ConceptGraphProps {
  nodes: GraphNode[];
  edges: GraphEdge[];
  visibleLayers: Set<Layer>;
  visibleEdgeTypes: Set<EdgeType>;
  onNodeClick: (node: GraphNode | null) => void;
  selectedNodeId: string | null;
}

function getNodeId(d: string | number | SimulationNode): string {
  if (typeof d === "string") return d;
  if (typeof d === "number") return String(d);
  return d.id;
}

function getNodeCoords(d: string | number | SimulationNode): {
  x: number;
  y: number;
} {
  if (typeof d === "string" || typeof d === "number") return { x: 0, y: 0 };
  return { x: d.x ?? 0, y: d.y ?? 0 };
}

function fittedView(nodes: SimulationNode[]): string {
  const xs = nodes.map((node) => node.x ?? 0),
    ys = nodes.map((node) => node.y ?? 0);
  const padding = 96;
  const left = Math.min(...xs) - padding,
    top = Math.min(...ys) - padding;
  return [
    left,
    top,
    Math.max(...xs) - left + padding,
    Math.max(...ys) - top + padding,
  ].join(" ");
}

function getChineseName(node: GraphNode): string {
  const match = node.molecule.match(/^#\s+(.+)$/m);
  if (match && match[1]) return match[1];
  return node.id;
}

export default function ConceptGraph({
  nodes,
  edges,
  visibleLayers,
  visibleEdgeTypes,
  onNodeClick,
  selectedNodeId,
}: ConceptGraphProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const positions = useRef(new Map<string, { x: number; y: number }>());
  const viewport = useRef("");
  const dragging = useRef<string | null>(null);
  const [view, setView] = useState<string | null>(null);
  const svgRef = useRef<SVGSVGElement>(null);
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

  // Filter visible nodes and edges
  const filteredNodes = useMemo(
    () => nodes.filter((n) => visibleLayers.has(n.layer)),
    [nodes, visibleLayers],
  );

  const filteredEdges = useMemo(
    () =>
      edges.filter(
        (e) =>
          visibleEdgeTypes.has(e.type) &&
          filteredNodes.some((n) => n.id === e.from) &&
          filteredNodes.some((n) => n.id === e.target),
      ),
    [edges, visibleEdgeTypes, filteredNodes],
  );

  // Initialize / update simulation
  useEffect(() => {
    const liveIds = new Set(nodes.map((node) => node.id));
    for (const id of positions.current.keys())
      if (!liveIds.has(id)) positions.current.delete(id);
    if (filteredNodes.length === 0) {
      setSimNodes([]);
      setSimEdges([]);
      setHoveredNodeId(null);
      setTooltip(null);
      return;
    }

    const width = dimensions.width;
    const height = dimensions.height;

    const simNodes: SimulationNode[] = filteredNodes.map((n) => ({
      ...n,
      x:
        positions.current.get(n.id)?.x ??
        width / 2 + (Math.random() - 0.5) * 100,
      y:
        positions.current.get(n.id)?.y ??
        height / 2 + (Math.random() - 0.5) * 100,
    }));

    const simEdges: SimulationEdge[] = filteredEdges.map((e) => ({
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
    const viewportKey = `${width}:${height}`;
    if (viewport.current !== viewportKey) {
      // A narrow canvas starts with the entire graph visible. Resize/orientation
      // changes recalculate the fit; manual reset stays until the next resize.
      setView(width <= 640 ? fittedView(simNodes) : null);
      viewport.current = viewportKey;
    }
    simulation.on("tick", publish);

    simulationRef.current = simulation;

    return () => {
      simulation.stop();
    };
  }, [filteredNodes, filteredEdges, dimensions]);

  const startDrag = (
    event: React.PointerEvent<SVGGElement>,
    node: SimulationNode,
  ) => {
    if (event.button !== 0) return;
    dragging.current = node.id;
    event.currentTarget.setPointerCapture(event.pointerId);
    simulationRef.current?.alphaTarget(0.2).restart();
    node.fx = node.x;
    node.fy = node.y;
  };
  const moveDrag = (
    event: React.PointerEvent<SVGGElement>,
    node: SimulationNode,
  ) => {
    if (dragging.current !== node.id || !svgRef.current) return;
    const point = d3.pointer(event, svgRef.current);
    node.fx = point[0];
    node.fy = point[1];
  };
  const endDrag = (
    _event: React.PointerEvent<SVGGElement>,
    node: SimulationNode,
  ) => {
    dragging.current = null;
    node.fx = null;
    node.fy = null;
    simulationRef.current?.alphaTarget(0);
  };
  const fit = () => {
    if (!simNodes.length) return;
    setView(fittedView(simNodes));
  };

  // Compute neighbor sets for hover highlight
  const neighborSet = useMemo(() => {
    if (!hoveredNodeId) return new Set<string>();
    const neighbors = new Set<string>([hoveredNodeId]);
    simEdges.forEach((e) => {
      const src = getNodeId(e.source);
      const tgt = getNodeId(e.target);
      if (src === hoveredNodeId) neighbors.add(tgt);
      if (tgt === hoveredNodeId) neighbors.add(src);
    });
    return neighbors;
  }, [hoveredNodeId, simEdges]);

  const edgeHighlighted = useMemo(() => {
    if (!hoveredNodeId) return new Set<number>();
    const highlighted = new Set<number>();
    simEdges.forEach((e, idx) => {
      const src = getNodeId(e.source);
      const tgt = getNodeId(e.target);
      if (src === hoveredNodeId || tgt === hoveredNodeId) {
        highlighted.add(idx);
      }
    });
    return highlighted;
  }, [hoveredNodeId, simEdges]);

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

  return (
    <div
      ref={containerRef}
      className="relative h-full min-h-80 w-full overflow-hidden bg-muted/20"
    >
      <div className="absolute right-3 top-3 z-10 flex gap-1 rounded-lg border bg-background/95 p-1 shadow-sm">
        <Button
          variant="ghost"
          size="icon"
          onClick={fit}
          disabled={simNodes.length === 0}
          aria-label="适应全部节点"
          title="适应全部节点"
        >
          <Maximize2 className="size-4" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          onClick={() => setView(null)}
          aria-label="重置视图"
          title="重置视图"
        >
          <RotateCcw className="size-4" />
        </Button>
      </div>
      {filteredNodes.length === 0 && (
        <div className="pointer-events-none absolute inset-0 flex flex-col items-center justify-center gap-3 p-6 text-center text-muted-foreground">
          <Network className="size-8" />
          <p className="text-sm">当前视图没有概念</p>
          <p className="text-xs">调整筛选条件，或创建第一个概念。</p>
        </div>
      )}
      <svg
        ref={svgRef}
        className="graph-svg h-full w-full touch-none"
        aria-label="知识概念关系图"
        viewBox={view ?? `0 0 ${dimensions.width} ${dimensions.height}`}
        onClick={handleSvgClick}
      >
        <defs>
          {/* Arrow markers for each edge type */}
          {Object.entries(EDGE_COLORS).map(([type, color]) => (
            <marker
              key={type}
              id={`arrow-${type}`}
              viewBox="0 -5 10 10"
              refX={type === "contradicts" ? 28 : 22}
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
        </defs>

        {/* Edges */}
        <g>
          {simEdges.map((edge, idx) => {
            const srcCoords = getNodeCoords(edge.source);
            const tgtCoords = getNodeCoords(edge.target);

            const isHighlighted = edgeHighlighted.has(idx);
            const isDimmed = hoveredNodeId && !isHighlighted;
            const opacity = isDimmed ? 0.05 : isHighlighted ? 1.0 : 0.6;
            const strokeWidth = isHighlighted ? 2.5 : 1.5;

            return (
              <line
                key={`${edge.from}-${edge.type}-${edge.target}-${idx}`}
                className="graph-edge"
                x1={srcCoords.x}
                y1={srcCoords.y}
                x2={tgtCoords.x}
                y2={tgtCoords.y}
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
                markerEnd={`url(#arrow-${edge.type})`}
              />
            );
          })}
        </g>

        {/* Nodes */}
        <g>
          {simNodes.map((node) => {
            const isHovered = hoveredNodeId === node.id;
            const isNeighbor = neighborSet.has(node.id);
            const isSelected = selectedNodeId === node.id;
            const isDimmed = hoveredNodeId && !isNeighbor;
            const opacity = isDimmed ? 0.15 : 1.0;
            const radius = NODE_RADIUS[node.layer];
            const strokeWidth = isSelected ? 4 : isHovered ? 3 : 2;

            return (
              <g
                key={node.id}
                transform={`translate(${node.x ?? 0}, ${node.y ?? 0})`}
                opacity={opacity}
                style={{ transition: "opacity 150ms ease" }}
                onMouseEnter={(e) => handleNodeHover(node, e)}
                onMouseLeave={handleNodeLeave}
                role="button"
                tabIndex={0}
                aria-label={getChineseName(node)}
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
                onPointerDown={(e) => startDrag(e, node)}
                onPointerMove={(e) => moveDrag(e, node)}
                onPointerUp={(e) => endDrag(e, node)}
                onPointerCancel={(e) => endDrag(e, node)}
                onClick={(e) => {
                  e.stopPropagation();
                  onNodeClick(node);
                }}
              >
                <circle
                  className="node-circle"
                  r={radius}
                  fill={LAYER_COLORS[node.layer]}
                  stroke={isSelected ? "#ffffff" : "#ffffff"}
                  strokeWidth={strokeWidth}
                  style={{
                    filter: isSelected
                      ? "drop-shadow(0 0 8px rgba(255,255,255,0.6))"
                      : undefined,
                  }}
                />
                <text className="node-label" dy={1}>
                  {getChineseName(node).length > 12
                    ? getChineseName(node).slice(0, 12) + "…"
                    : getChineseName(node)}
                </text>
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
