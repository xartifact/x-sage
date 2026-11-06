import { useRef, useEffect, useState, useCallback, useMemo } from "react";
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

function getNodeCoords(d: string | number | SimulationNode): { x: number; y: number } {
  if (typeof d === "string" || typeof d === "number") return { x: 0, y: 0 };
  return { x: d.x ?? 0, y: d.y ?? 0 };
}

function getChineseName(node: GraphNode): string {
  const match = node.molecule.match(/^#\s+(.+?)\s/);
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
  const svgRef = useRef<SVGSVGElement>(null);
  const simulationRef = useRef<d3.Simulation<SimulationNode, SimulationEdge> | null>(null);
  const [simNodes, setSimNodes] = useState<SimulationNode[]>([]);
  const [simEdges, setSimEdges] = useState<SimulationEdge[]>([]);
  const [hoveredNodeId, setHoveredNodeId] = useState<string | null>(null);
  const [tooltip, setTooltip] = useState<{ x: number; y: number; text: string } | null>(null);
  const [dimensions, setDimensions] = useState({ width: window.innerWidth, height: window.innerHeight });

  // Update dimensions on resize
  useEffect(() => {
    const handleResize = () => {
      setDimensions({ width: window.innerWidth, height: window.innerHeight });
    };
    window.addEventListener("resize", handleResize);
    return () => window.removeEventListener("resize", handleResize);
  }, []);

  // Filter visible nodes and edges
  const filteredNodes = useMemo(
    () => nodes.filter((n) => visibleLayers.has(n.layer)),
    [nodes, visibleLayers]
  );

  const filteredEdges = useMemo(
    () =>
      edges.filter(
        (e) =>
          visibleEdgeTypes.has(e.type) &&
          filteredNodes.some((n) => n.id === e.from) &&
          filteredNodes.some((n) => n.id === e.target)
      ),
    [edges, visibleEdgeTypes, filteredNodes]
  );

  // Initialize / update simulation
  useEffect(() => {
    if (filteredNodes.length === 0) return;

    const width = dimensions.width;
    const height = dimensions.height;

    const simNodes: SimulationNode[] = filteredNodes.map((n) => ({
      ...n,
      x: width / 2 + (Math.random() - 0.5) * 100,
      y: height / 2 + (Math.random() - 0.5) * 100,
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
          .strength(0.5)
      )
      .force("charge", d3.forceManyBody().strength(-400))
      .force("center", d3.forceCenter(width / 2, height / 2))
      .force("collide", d3.forceCollide<SimulationNode>().radius((d) => NODE_RADIUS[d.layer] + 12).strength(0.8))
      .force("x", d3.forceX(width / 2).strength(0.05))
      .force("y", d3.forceY(height / 2).strength(0.05))
      .alphaDecay(0.02)
      .velocityDecay(0.4);

    simulation.on("tick", () => {
      setSimNodes([...simNodes]);
      setSimEdges([...simEdges]);
    });

    simulationRef.current = simulation;

    return () => {
      simulation.stop();
    };
  }, [filteredNodes, filteredEdges, dimensions]);

  // Drag behavior
  const handleDragStart = useCallback(
    (event: React.MouseEvent | React.TouchEvent, node: SimulationNode) => {
      if (!simulationRef.current) return;
      event.preventDefault();
      simulationRef.current.alphaTarget(0.3).restart();
      node.fx = node.x;
      node.fy = node.y;
    },
    []
  );

  const handleDragMove = useCallback(
    (event: React.MouseEvent | React.TouchEvent, node: SimulationNode) => {
      if (!simulationRef.current) return;
      const svg = svgRef.current;
      if (!svg) return;
      const point = d3.pointer(event, svg);
      node.fx = point[0];
      node.fy = point[1];
    },
    []
  );

  const handleDragEnd = useCallback(
    (_event: React.MouseEvent | React.TouchEvent, node: SimulationNode) => {
      if (!simulationRef.current) return;
      simulationRef.current.alphaTarget(0);
      node.fx = null;
      node.fy = null;
    },
    []
  );

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
    [onNodeClick]
  );

  const handleNodeHover = useCallback(
    (node: SimulationNode, event: React.MouseEvent) => {
      setHoveredNodeId(node.id);
      setTooltip({
        x: event.clientX + 16,
        y: event.clientY - 8,
        text: node.atom,
      });
    },
    []
  );

  const handleNodeLeave = useCallback(() => {
    setHoveredNodeId(null);
    setTooltip(null);
  }, []);

  return (
    <>
      <svg
        ref={svgRef}
        className="graph-svg"
        viewBox={`0 0 ${dimensions.width} ${dimensions.height}`}
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
              markerWidth={type === "contradicts" || type === "analogous-to" ? 0 : 6}
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
                onMouseDown={(e) => handleDragStart(e, node)}
                onMouseMove={(e) => handleDragMove(e, node)}
                onMouseUp={(e) => handleDragEnd(e, node)}
                onTouchStart={(e) => handleDragStart(e, node)}
                onTouchMove={(e) => handleDragMove(e, node)}
                onTouchEnd={(e) => handleDragEnd(e, node)}
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
                  {getChineseName(node)}
                </text>
              </g>
            );
          })}
        </g>
      </svg>

      {/* Tooltip */}
      {tooltip && (
        <div
          className={`atom-tooltip ${tooltip ? "visible" : ""}`}
          style={{ left: tooltip.x, top: tooltip.y }}
          lang="zh-CN"
        >
          {tooltip.text}
        </div>
      )}
    </>
  );
}
