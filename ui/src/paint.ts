import type { GraphNode, GraphStore, CosmosSnapshot, Rgba } from "./store";
import { COLOR } from "./store";

/** Paper pulled toward ink — same two tokens, no new hue. */
export const MUTED_PAPER: Rgba = mixRgb(COLOR.paper, COLOR.ink, 0.55);

/** cosmos default space is 4096²; park new points near the center so a missed fitView is still on-canvas. */
const SPACE_CENTER = 2048;
const RING = 160;

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
    if (node !== undefined && node.label === "") {
      return MUTED_PAPER;
    }
    return snap.pointColors[i]!;
  });

  const edgeIds = visibleEdgeIds(store);
  const dim = mixRgb(COLOR.structure, COLOR.ink, 0.55);
  const linkColors = snap.linkColors.map((color, i) => {
    if (sameRgba(color, COLOR.signal)) {
      return color;
    }
    const id = edgeIds[i];
    if (id !== undefined && (glowing.has(id) || highlighted.has(id))) {
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

function sameRgba(a: Rgba, b: Rgba): boolean {
  return a[0] === b[0] && a[1] === b[1] && a[2] === b[2] && a[3] === b[3];
}
