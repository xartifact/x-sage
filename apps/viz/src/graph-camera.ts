import { conceptName, type GraphNode, type Layer } from "./types";
import { graphLayerGap, graphLayerHeight } from "./graph-gravity";

export interface GraphCamera {
  yaw: number;
  pitch: number;
}

export interface GraphSize {
  width: number;
  height: number;
}

export interface GraphPoint {
  x: number;
  y: number;
}

export interface GraphView extends GraphPoint, GraphSize {}

export type GraphLabelMode = "auto" | "show" | "hide";

export function graphLabelMode(value: unknown): GraphLabelMode {
  return value === "show" || value === "hide" ? value : "auto";
}

export function graphLabel(node: GraphNode): string {
  const title = conceptName(node).replace(/(?<=\p{Script=Han})\s+[A-Z].*$/u, "");
  return title.length > 14 ? title.slice(0, 13) + "…" : title;
}

export function showGraphLabel(
  mode: GraphLabelMode, layer: Layer, sparse: boolean, focused: boolean, inContext: boolean,
): boolean {
  return mode === "show" || (mode === "auto" && (focused || (inContext && (sparse || layer < 2))));
}

export function panGraphView(view: GraphView, dx: number, dy: number): GraphView {
  return { ...view, x: view.x - dx, y: view.y - dy };
}

export const defaultGraphCamera: GraphCamera = { yaw: -0.48, pitch: 0.52 };

export function rotateGraph(camera: GraphCamera, dx: number, dy: number): GraphCamera {
  return {
    yaw: camera.yaw + dx * 0.008,
    pitch: Math.max(-1.35, Math.min(1.35, camera.pitch - dy * 0.008)),
  };
}

export function graphProjection({ yaw, pitch }: GraphCamera, { width, height }: GraphSize) {
  const sinYaw = Math.sin(yaw), cosYaw = Math.cos(yaw);
  const sinPitch = Math.sin(pitch), cosPitch = Math.cos(pitch);
  const centerX = width / 2, centerY = height / 2;
  const eyeDistance = Math.max(width, height) * 1.25;
  const gap = graphLayerGap(height);

  return (x: number, y: number, layer: Layer) => {
    const horizontal = x - centerX;
    const groundDepth = y - centerY;
    const level = -graphLayerHeight(layer, gap);
    const across = horizontal * cosYaw - groundDepth * sinYaw;
    const rotatedDepth = horizontal * sinYaw + groundDepth * cosYaw;
    const vertical = level * cosPitch - rotatedDepth * sinPitch;
    const depth = level * sinPitch + rotatedDepth * cosPitch;
    const scale = eyeDistance / Math.max(eyeDistance * 0.35, eyeDistance + depth);
    return { x: centerX + across * scale, y: centerY + vertical * scale, depth, scale };
  };
}
