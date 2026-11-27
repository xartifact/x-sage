import { LAYERS, type GraphData, type Layer } from "./types";

export function graphLayerGap(height: number) {
  return Math.min(128, height * 0.23);
}

/** The world Y axis points upward in both renderers. */
export function graphLayerHeight(layer: Layer, gap: number) {
  return (layer - (LAYERS.length - 1) / 2) * gap;
}

export function graphLandingHeight(gap: number) {
  return graphLayerHeight(0, gap) - Math.max(44, gap * 0.72);
}

/** Vertical motion under constant gravity, with an inelastic landing. */
export function fallingHeight(start: number, floor: number, elapsedMs: number) {
  const seconds = Math.max(0, elapsedMs) / 1000;
  return Math.max(floor, start - 520 * seconds * seconds);
}

/** Follow strictly descending, confirmed support links back to any L0 axiom. */
export function unsupportedGraphNodes({ nodes, edges }: GraphData) {
  const layers = new Map(nodes.map(({ id, layer }) => [id, layer]));
  const above = new Map<string, string[]>();
  for (const edge of edges) {
    if (edge.mirrored || edge.confidence !== "confirmed"
      || (edge.type !== "depends-on" && edge.type !== "is-a" && edge.type !== "instantiates")) continue;
    const from = layers.get(edge.from);
    const target = layers.get(edge.target);
    if (from === undefined || target === undefined || from <= target) continue;
    const dependents = above.get(edge.target);
    if (dependents) dependents.push(edge.from);
    else above.set(edge.target, [edge.from]);
  }
  const queue = nodes.filter(({ layer }) => layer === 0).map(({ id }) => id);
  const supported = new Set(queue);
  for (let i = 0; i < queue.length; i++) {
    for (const id of above.get(queue[i]!) ?? []) {
      if (supported.has(id)) continue;
      supported.add(id);
      queue.push(id);
    }
  }
  return new Set(nodes.filter(({ id }) => !supported.has(id)).map(({ id }) => id));
}
