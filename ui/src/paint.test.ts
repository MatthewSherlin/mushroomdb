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
  FORCE_LAYOUT,
  LABEL_TINTS,
  MUTED_PAPER,
  POINT_SIZE_MAX,
  POINT_SIZE_MIN,
  edgeLegend,
  flattenColors,
  flattenLinks,
  formatHoverCard,
  labelFill,
  nextPositions,
  paintExplorer,
  pointSizes,
  sameKeys,
  simulationOn,
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

describe("labelFill", () => {
  it("uses only token mixes — no new hues — and keeps blanks muted", () => {
    expect(labelFill("")).toEqual(MUTED_PAPER);
    expect(LABEL_TINTS).toContainEqual(COLOR.paper);
    for (const tint of LABEL_TINTS) {
      expect(tint[3]).toBe(1);
      const channels = tint.slice(0, 3);
      const lo = Math.min(COLOR.ink[0], COLOR.paper[0], COLOR.structure[0], COLOR.gold[0], COLOR.signal[0]);
      const hi = Math.max(COLOR.ink[0], COLOR.paper[0], COLOR.structure[0], COLOR.gold[0], COLOR.signal[0]);
      expect(channels[0]!).toBeGreaterThanOrEqual(lo - 1e-6);
      expect(channels[0]!).toBeLessThanOrEqual(hi + 1e-6);
    }
  });

  it("gives Person, Org, and Project distinct fills so labels read without text", () => {
    const person = labelFill("Person");
    const org = labelFill("Org");
    const project = labelFill("Project");
    expect(person).not.toEqual(org);
    expect(person).not.toEqual(project);
    expect(org).not.toEqual(project);
    expect(person).not.toEqual(MUTED_PAPER);
    expect(labelFill("Person")).toEqual(person);
  });
});

describe("paintExplorer", () => {
  it("paints blank-label nodes muted paper and labeled nodes by label tint", () => {
    const store = seeded();
    const snap = store.toCosmos();
    const painted = paintExplorer(store, snap, new Set());
    // keys sort a, b, c — a and c are blank-label stubs; b is Person
    expect(painted.pointKeys).toEqual(["a", "b", "c"]);
    expect(painted.pointColors[0]).toEqual(MUTED_PAPER);
    expect(painted.pointColors[1]).toEqual(labelFill("Person"));
    expect(painted.pointColors[2]).toEqual(MUTED_PAPER);
  });

  it("keeps the selected node signal even when its label is blank", () => {
    const store = seeded();
    store.select({ kind: "node", id: "a" });
    const painted = paintExplorer(store, store.toCosmos(), new Set());
    expect(painted.pointColors[0]).toEqual(COLOR.signal);
  });

  it("keeps a selected labeled node signal, not its label tint", () => {
    const store = seeded();
    store.select({ kind: "node", id: "b" });
    const painted = paintExplorer(store, store.toCosmos(), new Set());
    expect(painted.pointColors[1]).toEqual(COLOR.signal);
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

describe("pointSizes", () => {
  it("maps degree onto a bounded 3–9px range; hubs are larger", () => {
    const store = new GraphStore();
    store.fromNeighborhood("hub", neighborhood([
      ["a", "Person", 1],
      ["b", "Person", 1],
      ["c", "Person", 1],
    ]));
    store.mergeNeighborhoodWithEdges("hub", { KNOWS: ["a", "b", "c"] });
    store.fromNeighborhood("leaf", neighborhood([]));
    const sized = pointSizes(store, ["a", "hub", "leaf"]);
    expect(POINT_SIZE_MIN).toBe(3);
    expect(POINT_SIZE_MAX).toBe(9);
    expect(sized[1]!).toBe(POINT_SIZE_MAX);
    expect(sized[2]!).toBe(POINT_SIZE_MIN);
    expect(sized[0]!).toBeGreaterThanOrEqual(POINT_SIZE_MIN);
    expect(sized[0]!).toBeLessThan(sized[1]!);
  });

  it("uses the mid size when every visible node has the same degree", () => {
    const equal = new GraphStore();
    equal.fromNeighborhood("a", neighborhood([["b", "Person", 1]]));
    equal.mergeNeighborhoodWithEdges("a", { KNOWS: ["b"] });
    const sizes = pointSizes(equal, ["a", "b"]);
    expect(sizes[0]).toBe((POINT_SIZE_MIN + POINT_SIZE_MAX) / 2);
    expect(sizes[1]).toBe((POINT_SIZE_MIN + POINT_SIZE_MAX) / 2);
  });
});

describe("edgeLegend", () => {
  it("lists visible etypes with gold/structure colors and counts from the store", () => {
    const store = seeded();
    const rows = edgeLegend(store);
    expect(rows).toEqual([
      { etype: "FIT", derived: true, count: 1, color: COLOR.gold },
      { etype: "KNOWS", derived: false, count: 1, color: COLOR.structure },
    ]);
    store.mergeNeighborhoodWithEdges("a", { KNOWS: ["d"], FIT: ["e"] });
    store.setProvenance(edgeId("FIT", "a", "e"), {
      rule: "skill_fit",
      edge_type: "FIT",
      src_key: "a",
      dst_key: "e",
      weight: 1,
      predicate: null,
    });
    expect(edgeLegend(store)).toEqual([
      { etype: "FIT", derived: true, count: 2, color: COLOR.gold },
      { etype: "KNOWS", derived: false, count: 2, color: COLOR.structure },
    ]);
  });

  it("splits the same etype when both user and derived instances are visible", () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", neighborhood([
      ["b", "Person", 1],
      ["c", "Person", 1],
    ]));
    store.mergeNeighborhoodWithEdges("a", { KNOWS: ["b", "c"] });
    store.setProvenance(edgeId("KNOWS", "a", "c"), {
      rule: "auto",
      edge_type: "KNOWS",
      src_key: "a",
      dst_key: "c",
      weight: 1,
      predicate: null,
    });
    expect(edgeLegend(store)).toEqual([
      { etype: "KNOWS", derived: false, count: 1, color: COLOR.structure },
      { etype: "KNOWS", derived: true, count: 1, color: COLOR.gold },
    ]);
  });
});

describe("sameKeys", () => {
  it("treats a reorder with the same membership as unchanged", () => {
    expect(sameKeys(["a", "b", "c"], ["c", "a", "b"])).toBe(true);
    expect(sameKeys(["a", "b"], ["a", "b"])).toBe(true);
    expect(sameKeys([], [])).toBe(true);
  });

  it("detects added, removed, or swapped membership", () => {
    expect(sameKeys(["a", "b"], ["a", "b", "c"])).toBe(false);
    expect(sameKeys(["a", "b", "c"], ["a", "b"])).toBe(false);
    expect(sameKeys(["a", "b"], ["a", "d"])).toBe(false);
    expect(sameKeys(["a"], [])).toBe(false);
  });
});

describe("force layout params", () => {
  it("turns simulation off under reduced motion and documents gravity/decay", () => {
    expect(simulationOn(true)).toBe(false);
    expect(simulationOn(false)).toBe(true);
    expect(FORCE_LAYOUT.simulationGravity).toBeGreaterThan(0);
    expect(FORCE_LAYOUT.simulationDecay).toBeGreaterThan(5000);
    expect(FORCE_LAYOUT.simulationCenter).toBeGreaterThan(0);
    expect(FORCE_LAYOUT.simulationLinkDistance).toBeGreaterThan(80);
    expect(FORCE_LAYOUT.simulationFriction).toBeGreaterThan(0);
    expect(FORCE_LAYOUT.simulationFriction).toBeLessThan(1);
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
