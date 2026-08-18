import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import type { QueryResult } from "./api";
import {
  COLOR,
  GraphStore,
  edgeId,
  type CosmosSnapshot,
  type Rgba,
} from "./store";
import {
  MUTED_PAPER,
  flattenColors,
  flattenLinks,
  formatHoverCard,
  nextPositions,
  paintExplorer,
} from "./paint";

const here = dirname(fileURLToPath(import.meta.url));
const paintSrc = readFileSync(join(here, "paint.ts"), "utf8");

describe("module contract", () => {
  it("is a pure module: no DOM, canvas, or cosmos imports", () => {
    expect(paintSrc).not.toMatch(
      /from\s+["']@cosmos\.gl|document\.|window\.|HTMLCanvas|getContext\(/,
    );
  });

  it("chrome CSS introduces no hues beyond the token variables", () => {
    const css = readFileSync(join(here, "style.css"), "utf8");
    expect(css.match(/#[0-9A-Fa-f]{3,8}/g) ?? []).toEqual([]);
  });
});

function neighborhood(
  rows: Array<[string, string, number]>,
): QueryResult {
  return { columns: ["key", "label", "depth"], rows };
}

function seeded(): GraphStore {
  const store = new GraphStore();
  store.fromNeighborhood("a", neighborhood([["b", "Person", 1]]));
  store.mergeNeighborhoodWithEdges("a", { KNOWS: ["b"], FIT: ["c"] });
  store.setProvenance(edgeId("FIT", "a", "c"), {
    rule: "skill_fit",
    edge_type: "FIT",
    src_key: "a",
    dst_key: "c",
    weight: 1,
    predicate: null,
  });
  return store;
}

describe("MUTED_PAPER", () => {
  it("is a paper/ink mix, not a new hue", () => {
    expect(MUTED_PAPER[0]).toBeGreaterThan(COLOR.ink[0]);
    expect(MUTED_PAPER[0]).toBeLessThan(COLOR.paper[0]);
    expect(MUTED_PAPER[3]).toBe(1);
    expect(MUTED_PAPER).not.toEqual(COLOR.paper);
  });
});

describe("paintExplorer", () => {
  it("paints blank-label nodes muted paper and leaves labeled nodes paper", () => {
    const store = seeded();
    const snap = store.toCosmos();
    const painted = paintExplorer(store, snap, new Set());
    // keys sort a, b, c — a and c are blank-label stubs
    expect(painted.pointKeys).toEqual(["a", "b", "c"]);
    expect(painted.pointColors[0]).toEqual(MUTED_PAPER);
    expect(painted.pointColors[1]).toEqual(COLOR.paper);
    expect(painted.pointColors[2]).toEqual(MUTED_PAPER);
  });

  it("keeps the selected node signal even when its label is blank", () => {
    const store = seeded();
    store.select({ kind: "node", id: "a" });
    const painted = paintExplorer(store, store.toCosmos(), new Set());
    expect(painted.pointColors[0]).toEqual(COLOR.signal);
  });

  it("keeps a glowing user edge structure, not gold", () => {
    const store = seeded();
    const knows = edgeId("KNOWS", "a", "b");
    const painted = paintExplorer(store, store.toCosmos(), new Set([knows]));
    expect(colorOf(painted, 0, 1)).toEqual(COLOR.structure);
    expect(colorOf(painted, 0, 2)).toEqual(COLOR.gold);
  });

  it("overrides a glowing derived edge to gold unless the edge is selected", () => {
    const store = seeded();
    const fit = edgeId("FIT", "a", "c");
    const idle = paintExplorer(store, store.toCosmos(), new Set([fit]));
    expect(colorOf(idle, 0, 2)).toEqual(COLOR.gold);

    store.select({ kind: "edge", id: fit });
    const selected = paintExplorer(store, store.toCosmos(), new Set([fit]));
    expect(colorOf(selected, 0, 2)).toEqual(COLOR.signal);
  });

  it("paints a highlighted rule's edges gold and dims the rest", () => {
    const store = seeded();
    const fit = edgeId("FIT", "a", "c");
    const painted = paintExplorer(
      store,
      store.toCosmos(),
      new Set(),
      new Set([fit]),
    );
    expect(colorOf(painted, 0, 2)).toEqual(COLOR.gold);
    expect(colorOf(painted, 0, 1)).not.toEqual(COLOR.gold);
    expect(colorOf(painted, 0, 1)).not.toEqual(COLOR.signal);
  });
});

describe("formatHoverCard", () => {
  it("is mono data: key, label when present, prop count", () => {
    expect(
      formatHoverCard({ key: "person-01", label: "Person", props: { a: 1, b: 2 } }),
    ).toEqual(["person-01", "Person", "2 props"]);
    expect(formatHoverCard({ key: "a", label: "" })).toEqual(["a", "0 props"]);
    expect(formatHoverCard({ key: "b", label: "Org", props: { x: 1 } })).toEqual(
      ["b", "Org", "1 prop"],
    );
  });
});

describe("flatten helpers", () => {
  it("packs rgba tuples and link pairs into cosmos float arrays", () => {
    const colors = flattenColors([COLOR.paper, COLOR.gold]);
    expect(colors).toBeInstanceOf(Float32Array);
    expect(colors.length).toBe(8);
    expect(colors[0]).toBeCloseTo(COLOR.paper[0]);
    expect(colors[4]).toBeCloseTo(COLOR.gold[0]);
    expect(Array.from(flattenLinks([[0, 1], [2, 3]]))).toEqual([0, 1, 2, 3]);
  });
});

describe("nextPositions", () => {
  it("keeps previous coordinates and places new keys on a ring", () => {
    const prev = new Map<string, readonly [number, number]>([["a", [10, 20]]]);
    const { positions, map } = nextPositions(["a", "b"], prev);
    expect(map.get("a")).toEqual([10, 20]);
    expect(positions[0]).toBe(10);
    expect(positions[1]).toBe(20);
    expect(map.has("b")).toBe(true);
    const bx = positions[2]!;
    const by = positions[3]!;
    expect(Number.isFinite(bx)).toBe(true);
    expect(Number.isFinite(by)).toBe(true);
    expect([bx, by]).not.toEqual([10, 20]);
  });
});

function colorOf(
  snap: CosmosSnapshot,
  src: number,
  dst: number,
): Rgba {
  const i = snap.links.findIndex(([s, d]) => s === src && d === dst);
  expect(i).toBeGreaterThanOrEqual(0);
  return snap.linkColors[i]!;
}
