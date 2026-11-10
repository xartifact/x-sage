export type Layer = 0 | 1 | 2 | 3;
export type EdgeType =
  | "is-a"
  | "part-of"
  | "depends-on"
  | "enables"
  | "instantiates"
  | "contradicts"
  | "analogous-to";
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
  filePath: string;
  molecule: string;
}

export interface GraphEdge {
  from: string;
  type: EdgeType;
  target: string;
  confidence: Confidence;
}

export interface GraphEdgeInfo {
  /** True for the mirrored half of a symmetric relation (not author-written). */
  mirrored?: boolean;
}

export interface VizMeta {
  conceptCount: number;
  assertedEdgeCount: number;
  edgeTypes: EdgeType[];
  generatedAt: string;
  confidenceCounts: Record<Confidence, number>;
  layerCounts: Record<string, number>;
}

export interface GraphData {
  nodes: GraphNode[];
  edges: Array<GraphEdge & GraphEdgeInfo>;
  meta?: VizMeta;
}

/** Layer order used by filters, legends and counts. */
export const LAYERS: Layer[] = [0, 1, 2, 3];

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
  "is-a": "#06b6d4", // cyan
  "part-of": "#a855f7", // purple
  "depends-on": "#3b82f6", // blue
  enables: "#22c55e", // green
  instantiates: "#f59e0b", // amber
  contradicts: "#ef4444", // red
  "analogous-to": "#94a3b8", // slate
};

export const EDGE_LABELS: Record<EdgeType, string> = {
  "is-a": "是…的一种",
  "part-of": "组成",
  "depends-on": "依赖",
  enables: "启用",
  instantiates: "实例化",
  contradicts: "矛盾",
  "analogous-to": "类比",
};

/** Edge order used by filters, legends and counts. */
export const EDGE_TYPES: EdgeType[] = [
  "is-a",
  "part-of",
  "depends-on",
  "enables",
  "instantiates",
  "contradicts",
  "analogous-to",
];

export const NODE_RADIUS: Record<Layer, number> = {
  0: 24,
  1: 18,
  2: 16,
  3: 16,
};
