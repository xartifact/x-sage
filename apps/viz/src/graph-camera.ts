import type { Layer } from "./types";

export interface GraphCamera {
  yaw: number;
  pitch: number;
}

export interface GraphSize {
  width: number;
  height: number;
}

export const defaultGraphCamera: GraphCamera = { yaw: -0.42, pitch: 0.38 };

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
  const eyeDistance = Math.max(width, height) * 1.6;

  return (x: number, y: number, layer: Layer) => {
    const horizontal = x - centerX;
    const groundDepth = y - centerY;
    const level = (layer - 1.5) * Math.min(108, height * 0.19);
    const across = horizontal * cosYaw - groundDepth * sinYaw;
    const rotatedDepth = horizontal * sinYaw + groundDepth * cosYaw;
    const vertical = level * cosPitch - rotatedDepth * sinPitch;
    const depth = level * sinPitch + rotatedDepth * cosPitch;
    const scale = eyeDistance / Math.max(eyeDistance * 0.35, eyeDistance + depth);
    return { x: centerX + across * scale, y: centerY + vertical * scale, depth, scale };
  };
}
