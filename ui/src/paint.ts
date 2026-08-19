import type { GraphNode, GraphStore, CosmosSnapshot, Rgba } from "./store";
import { COLOR } from "./store";

/** Paper pulled toward ink — same two tokens, no new hue. */
export const MUTED_PAPER: Rgba = mixRgb(COLOR.paper, COLOR.ink, 0.55);

/** Degree → point size (px), bounded so hubs read without swallowing the canvas. */
export const POINT_SIZE_MIN = 3;
export const POINT_SIZE_MAX = 9;

/**
 * cosmos.gl force params. Ring initials (SPACE_CENTER/RING) plus gravity
 * keep the Plan-6 off-canvas launch from recurring; decay + friction settle
 * the demo before the layout wanders. Reduced-motion keeps simulation off.
 */
export const FORCE_LAYOUT = {
  simulationDecay: 8000,
  simulationGravity: 0.08,
  simulationCenter: 0.03,
  simulationRepulsion: 1.2,
  simulationLinkSpring: 0.35,
  simulationLinkDistance: 280,
  simulationFriction: 0.7,
  simulationCollision: 1.8,
  simulationLinkDistRandomVariationRange: [1, 1] as const,
} as const;

/**
 * Per-label fills derived only from the five tokens.
 * Person = paper, Org = structure tint, Project = gold tint, Skill = signal tint.
 * Unknown labels hash into the same six slots.
 */
export const LABEL_TINTS: readonly Rgba[] = [
  COLOR.paper,
  mixRgb(COLOR.paper, COLOR.structure, 0.7),
  mixRgb(COLOR.paper, COLOR.gold, 0.55),
  mixRgb(COLOR.paper, COLOR.signal, 0.6),
  mixRgb(COLOR.paper, COLOR.structure, 0.4),
  mixRgb(COLOR.paper, COLOR.ink, 0.35),
];

const KNOWN_LABEL_SLOT: Record<string, number> = {
  Person: 0,
  Org: 1,
  Project: 2,
  Skill: 3,
};

export type EdgeLegendRow = {
  etype: string;
  derived: boolean;
  count: number;
  color: Rgba;
};

export function simulationOn(reducedMotion: boolean): boolean {
  return !reducedMotion;
}

export function labelFill(label: string): Rgba {
  if (label === "") {
    return MUTED_PAPER;
  }
  const known = KNOWN_LABEL_SLOT[label];
  if (known !== undefined) {
    return LABEL_TINTS[known]!;
  }
  return LABEL_TINTS[hashLabel(label) % LABEL_TINTS.length]!;
}

export function pointSizes(
  store: GraphStore,
  keys: readonly string[],
): Float32Array {
  const degrees = nodeDegrees(store);
  const values = keys.map((key) => degrees.get(key) ?? 0);
  const min = values.length === 0 ? 0 : Math.min(...values);
  const max = values.length === 0 ? 0 : Math.max(...values);
  const out = new Float32Array(keys.length);
  const mid = (POINT_SIZE_MIN + POINT_SIZE_MAX) / 2;
  for (let i = 0; i < keys.length; i++) {
    if (max === min) {
      out[i] = mid;
    } else {
      const t = (values[i]! - min) / (max - min);
      out[i] = POINT_SIZE_MIN + t * (POINT_SIZE_MAX - POINT_SIZE_MIN);
    }
  }
  return out;
}

export function edgeLegend(store: GraphStore): EdgeLegendRow[] {
  const counts = new Map<string, { etype: string; derived: boolean; count: number }>();
  for (const id of visibleEdgeIds(store)) {
    const edge = store.edges.get(id);
    if (edge === undefined) {
      continue;
    }
    const derived = edge.derived === true;
    const key = `${edge.etype}\0${derived ? "1" : "0"}`;
    const prev = counts.get(key);
    if (prev === undefined) {
      counts.set(key, { etype: edge.etype, derived, count: 1 });
    } else {
      prev.count += 1;
    }
  }
  return [...counts.values()]
    .sort((a, b) => {
      const et = a.etype.localeCompare(b.etype);
      if (et !== 0) {
        return et;
      }
      return Number(a.derived) - Number(b.derived);
    })
    .map((row) => ({
      ...row,
      color: row.derived ? COLOR.gold : COLOR.structure,
    }));
}

/** cosmos default space is 4096²; park new points near the center so a missed fitView is still on-canvas. */
const SPACE_CENTER = 2048;
const RING = 220;

export function mixRgb(a: Rgba, b: Rgba, t: number): Rgba {
  return [
    a[0] * (1 - t) + b[0] * t,
    a[1] * (1 - t) + b[1] * t,
    a[2] * (1 - t) + b[2] * t,
    1,
  ];
}

export function visibleEdgeIds(store: GraphStore): string[] {
  const ids: string[] = [];
  for (const id of [...store.edges.keys()].sort()) {
    const edge = store.edges.get(id);
    if (edge === undefined) {
      continue;
    }
    if (store.nodes.has(edge.src) && store.nodes.has(edge.dst)) {
      ids.push(id);
    }
  }
  return ids;
}

export function paintExplorer(
  store: GraphStore,
  snap: CosmosSnapshot,
  glowing: ReadonlySet<string>,
  highlighted: ReadonlySet<string> = new Set(),
): CosmosSnapshot {
  const selectedNode =
    store.selection?.kind === "node" ? store.selection.id : undefined;
  const pointColors = snap.pointKeys.map((key, i) => {
    if (key === selectedNode) {
      return snap.pointColors[i]!;
    }
    const node = store.nodes.get(key);
    if (node === undefined || node.label === "") {
      return MUTED_PAPER;
    }
    return labelFill(node.label);
  });

  const edgeIds = visibleEdgeIds(store);
  const dim = mixRgb(COLOR.structure, COLOR.ink, 0.55);
  const linkColors = snap.linkColors.map((color, i) => {
    if (sameRgba(color, COLOR.signal)) {
      return color;
    }
    const id = edgeIds[i];
    if (id !== undefined && highlighted.has(id)) {
      return COLOR.gold;
    }
    if (
      id !== undefined &&
      glowing.has(id) &&
      store.edges.get(id)?.derived === true
    ) {
      return COLOR.gold;
    }
    if (id !== undefined && highlighted.size > 0) {
      return dim;
    }
    return color;
  });

  return {
    pointKeys: snap.pointKeys,
    pointColors,
    links: snap.links,
    linkColors,
  };
}

export function formatHoverCard(node: GraphNode): string[] {
  const lines = [node.key];
  if (node.label !== "") {
    lines.push(node.label);
  }
  const n = node.props === undefined ? 0 : Object.keys(node.props).length;
  lines.push(n === 1 ? "1 prop" : `${n} props`);
  return lines;
}

export function flattenColors(colors: readonly Rgba[]): Float32Array {
  const out = new Float32Array(colors.length * 4);
  for (let i = 0; i < colors.length; i++) {
    const c = colors[i]!;
    out[i * 4] = c[0];
    out[i * 4 + 1] = c[1];
    out[i * 4 + 2] = c[2];
    out[i * 4 + 3] = c[3];
  }
  return out;
}

export function flattenLinks(
  links: readonly (readonly [number, number])[],
): Float32Array {
  const out = new Float32Array(links.length * 2);
  for (let i = 0; i < links.length; i++) {
    out[i * 2] = links[i]![0];
    out[i * 2 + 1] = links[i]![1];
  }
  return out;
}

export function nextPositions(
  keys: readonly string[],
  previous: ReadonlyMap<string, readonly [number, number]>,
): { positions: Float32Array; map: Map<string, [number, number]> } {
  const map = new Map<string, [number, number]>();
  const positions = new Float32Array(keys.length * 2);
  const newcomers: number[] = [];
  for (let i = 0; i < keys.length; i++) {
    const key = keys[i]!;
    const prev = previous.get(key);
    if (prev !== undefined) {
      map.set(key, [prev[0], prev[1]]);
      positions[i * 2] = prev[0];
      positions[i * 2 + 1] = prev[1];
    } else {
      newcomers.push(i);
    }
  }
  const n = newcomers.length;
  for (let k = 0; k < n; k++) {
    const i = newcomers[k]!;
    const angle = (2 * Math.PI * k) / Math.max(n, 1) - Math.PI / 2;
    const x = SPACE_CENTER + Math.cos(angle) * RING;
    const y = SPACE_CENTER + Math.sin(angle) * RING;
    map.set(keys[i]!, [x, y]);
    positions[i * 2] = x;
    positions[i * 2 + 1] = y;
  }
  return { positions, map };
}

function nodeDegrees(store: GraphStore): Map<string, number> {
  const deg = new Map<string, number>();
  for (const key of store.nodes.keys()) {
    deg.set(key, 0);
  }
  for (const id of visibleEdgeIds(store)) {
    const edge = store.edges.get(id);
    if (edge === undefined) {
      continue;
    }
    deg.set(edge.src, (deg.get(edge.src) ?? 0) + 1);
    deg.set(edge.dst, (deg.get(edge.dst) ?? 0) + 1);
  }
  return deg;
}

function hashLabel(label: string): number {
  let h = 2166136261;
  for (let i = 0; i < label.length; i++) {
    h ^= label.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

function sameRgba(a: Rgba, b: Rgba): boolean {
  return a[0] === b[0] && a[1] === b[1] && a[2] === b[2] && a[3] === b[3];
}
