export type Layer = 0 | 1 | 2 | 3;
export type EdgeType = "enables" | "depends-on" | "contradicts" | "part-of";
export type Confidence = "confirmed" | "hypothetical" | "contested";
export type Source = "personal" | "domain" | "synthesis";
export type Domain = "product" | "philosophy" | "personal-thought" | string;

export interface GraphNode {
  id: string;
  atom: string;
  layer: Layer;
  domain: Domain[];
  confidence: Confidence;
  source: Source;
  triggers: string[];
  requires: string[];
  enables: string[];
  filePath: string;
  molecule: string;
}

export interface GraphEdge {
  from: string;
  type: EdgeType;
  target: string;
  confidence: Confidence;
}

export interface GraphData {
  nodes: GraphNode[];
  edges: GraphEdge[];
}

export const LAYER_COLORS: Record<Layer, string> = {
  0: "#6366f1", // indigo — axioms
  1: "#3b82f6", // blue — core
  2: "#10b981", // emerald — support
  3: "#f59e0b", // amber — instances
};

export const LAYER_LABELS: Record<Layer, string> = {
  0: "L0 公理",
  1: "L1 核心",
  2: "L2 支撑",
  3: "L3 实例",
};

export const EDGE_COLORS: Record<EdgeType, string> = {
  enables: "#22c55e", // green
  "depends-on": "#3b82f6", // blue
  contradicts: "#ef4444", // red
  "part-of": "#a855f7", // purple
};

export const EDGE_LABELS: Record<EdgeType, string> = {
  enables: "启用",
  "depends-on": "依赖",
  contradicts: "矛盾",
  "part-of": "组成",
};

export const NODE_RADIUS: Record<Layer, number> = {
  0: 24,
  1: 18,
  2: 16,
  3: 16,
};
