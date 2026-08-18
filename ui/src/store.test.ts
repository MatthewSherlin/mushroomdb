import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import type { Explanation, QueryResult } from "./api";
import type { MutationEvent } from "./watch";
import { COLOR, GraphStore, edgeId, type CosmosSnapshot } from "./store";

const here = dirname(fileURLToPath(import.meta.url));
const tokens = readFileSync(join(here, "tokens.css"), "utf8");
const storeSrc = readFileSync(join(here, "store.ts"), "utf8");

function neighborhood(
  rows: Array<[string, string, number]>,
): QueryResult {
  return { columns: ["key", "label", "depth"], rows };
}

function fit(src: string, dst: string, weight = 0.5): Explanation {
  return {
    rule: "skill_fit",
    edge_type: "FIT",
    src_key: src,
    dst_key: dst,
    weight,
  };
}

function seedPair(store: GraphStore): void {
  store.fromNeighborhood("a", neighborhood([["b", "Person", 1]]));
  store.mergeNeighborhoodWithEdges("a", { KNOWS: ["b"] });
}

function assertEndpoints(store: GraphStore): void {
  for (const edge of store.edges.values()) {
    expect(store.nodes.has(edge.src), `missing src ${edge.src}`).toBe(true);
    expect(store.nodes.has(edge.dst), `missing dst ${edge.dst}`).toBe(true);
  }
}

describe("module contract", () => {
  it("is a pure module: no DOM, canvas, or cosmos imports", () => {
    expect(storeSrc).not.toMatch(
      /from\s+["']@cosmos\.gl|document\.|window\.|HTMLCanvas|getContext\(/,
    );
  });
});

describe("edgeId", () => {
  it("is etype|src|dst", () => {
    expect(edgeId("FIT", "p1", "j1")).toBe("FIT|p1|j1");
  });
});

describe("fromNeighborhood + mergeNeighborhoodWithEdges", () => {
  it("adds root and neighbor nodes from the ResultSet and does not invent edges", () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", neighborhood([["b", "Person", 1]]));

    expect([...store.nodes.keys()].sort()).toEqual(["a", "b"]);
    expect(store.nodes.get("b")).toEqual({ key: "b", label: "Person" });
    expect(store.nodes.get("a")?.key).toBe("a");
    expect(store.edges.size).toBe(0);
    expect(store.needsEdges("a")).toBe(true);
    expect(store.needsEdges("b")).toBe(true);
    expect(store.needsEdges("ghost")).toBe(false);
  });

  it("mergeNeighborhoodWithEdges adds directed edges and stub nodes, then clears needsEdges", () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", neighborhood([["b", "Person", 1]]));
    store.mergeNeighborhoodWithEdges("a", { KNOWS: ["b"], LIKES: ["c"] });

    expect(store.needsEdges("a")).toBe(false);
    expect(store.edges.get(edgeId("KNOWS", "a", "b"))).toMatchObject({
      etype: "KNOWS",
      src: "a",
      dst: "b",
    });
    expect(store.edges.get(edgeId("LIKES", "a", "c"))).toMatchObject({
      etype: "LIKES",
      src: "a",
      dst: "c",
    });
    expect(store.nodes.get("c")).toEqual({ key: "c", label: "" });
    expect(store.nodes.get("b")?.label).toBe("Person");
    assertEndpoints(store);
  });

  it("dir:'in' stores etype|neighbor|root", () => {
    const store = new GraphStore();
    store.fromNeighborhood("b", neighborhood([["a", "Person", 1]]));
    store.mergeNeighborhoodWithEdges("b", { KNOWS: ["a"] }, "in");

    expect(store.edges.has(edgeId("KNOWS", "a", "b"))).toBe(true);
    expect(store.edges.has(edgeId("KNOWS", "b", "a"))).toBe(false);
    assertEndpoints(store);
  });

  it("is idempotent: rematch does not duplicate nodes or edges or wipe provenance", () => {
    const store = new GraphStore();
    store.fromNeighborhood(
      "a",
      neighborhood([
        ["b", "Person", 1],
        ["c", "Org", 1],
      ]),
    );
    store.mergeNeighborhoodWithEdges("a", { KNOWS: ["b"], WORKS_AT: ["c"] });
    store.setProvenance(edgeId("KNOWS", "a", "b"), fit("a", "b"));

    const snap = {
      nodes: [...store.nodes.entries()],
      edges: [...store.edges.entries()],
    };

    store.fromNeighborhood(
      "a",
      neighborhood([
        ["c", "Org", 1],
        ["b", "Person", 1],
      ]),
    );
    store.mergeNeighborhoodWithEdges("a", { WORKS_AT: ["c"], KNOWS: ["b"] });

    expect([...store.nodes.keys()].sort()).toEqual(
      snap.nodes.map(([k]) => k).sort(),
    );
    expect([...store.edges.keys()].sort()).toEqual(
      snap.edges.map(([k]) => k).sort(),
    );
    expect(store.edges.get(edgeId("KNOWS", "a", "b"))?.derived).toBe(true);
    expect(store.edges.get(edgeId("KNOWS", "a", "b"))?.explanation).toEqual(
      fit("a", "b"),
    );
    expect(store.nodes.size).toBe(3);
    expect(store.edges.size).toBe(2);
    assertEndpoints(store);
  });

  it("fromNeighborhood after a completed edge merge marks needsEdges again", () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", neighborhood([["b", "Person", 1]]));
    store.mergeNeighborhoodWithEdges("a", { KNOWS: ["b"] });
    expect(store.needsEdges("a")).toBe(false);
    store.fromNeighborhood("a", neighborhood([["b", "Person", 1]]));
    expect(store.needsEdges("a")).toBe(true);
  });

  it("does not clobber an existing label with an empty stub label", () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", neighborhood([["b", "Person", 1]]));
    store.mergeNeighborhoodWithEdges("x", { KNOWS: ["b"] });
    expect(store.nodes.get("b")?.label).toBe("Person");
  });

  it("clears the root's dirty mark on fromNeighborhood", () => {
    const store = new GraphStore();
    store.markDirty("a");
    store.fromNeighborhood("a", neighborhood([]));
    expect(store.dirty.has("a")).toBe(false);
  });
});

describe("mergeQueryGraph", () => {
  it("treats string cells in node columns as keys (RETURN n → key string)", () => {
    const store = new GraphStore();
    store.mergeQueryGraph(["n"], [["p1"], ["p2"]]);
    expect([...store.nodes.keys()].sort()).toEqual(["p1", "p2"]);
    expect(store.nodes.get("p1")).toEqual({ key: "p1", label: "" });
    expect(store.edges.size).toBe(0);
  });

  it("binds key+label columns and skips dotted property columns", () => {
    const store = new GraphStore();
    store.mergeQueryGraph(
      ["key", "label", "n.name"],
      [
        ["p1", "Person", "Ada"],
        ["o1", "Org", "Acme"],
      ],
    );
    expect([...store.nodes.keys()].sort()).toEqual(["o1", "p1"]);
    expect(store.nodes.get("p1")?.label).toBe("Person");
    expect(store.nodes.has("Ada")).toBe(false);
  });

  it("is idempotent and keeps existing labels when a later merge has none", () => {
    const store = new GraphStore();
    store.mergeQueryGraph(["key", "label"], [["p1", "Person"]]);
    store.mergeQueryGraph(["n"], [["p1"]]);
    expect(store.nodes.get("p1")?.label).toBe("Person");
    expect(store.nodes.size).toBe(1);
  });

  it("does not harvest a label cell as a node key beside a node variable", () => {
    // Heuristic: a `key` column binds `label`; otherwise every string cell
    // in a non-dotted column is a key. T4 must project `key`+`label` or
    // only node variables — a sibling `label` next to `n` is not a key.
    // Limit: this does not bind `Org` as p1's label (no `key` column).
    const store = new GraphStore();
    store.mergeQueryGraph(["n", "label"], [["p1", "Org"]]);
    expect(store.nodes.has("p1")).toBe(true);
    expect(store.nodes.has("Org")).toBe(false);
  });
});

describe("endpoint invariant", () => {
  it("holds after every public merge path", () => {
    const store = new GraphStore();
    store.mergeQueryGraph(["n"], [["z"]]);
    store.fromNeighborhood(
      "a",
      neighborhood([
        ["b", "Person", 1],
        ["c", "Person", 2],
      ]),
    );
    store.mergeNeighborhoodWithEdges("a", { KNOWS: ["b", "ghost"] }, "out");
    store.mergeNeighborhoodWithEdges("c", { KNOWS: ["a"] }, "in");
    store.apply({
      edge_inserted: { edge_type: "LIKES", src: "a", dst: "b" },
    });
    assertEndpoints(store);
    expect(store.nodes.has("ghost")).toBe(true);
    expect(store.nodes.has("z")).toBe(true);
  });
});

describe("provenance", () => {
  it("setProvenance marks derived and caches; null clears", () => {
    const store = new GraphStore();
    seedPair(store);
    const id = edgeId("KNOWS", "a", "b");
    expect(store.edges.get(id)?.derived).toBeUndefined();

    store.setProvenance(id, fit("a", "b"));
    expect(store.edges.get(id)?.derived).toBe(true);
    expect(store.edges.get(id)?.explanation).toEqual(fit("a", "b"));
    expect(store.derivedEdges().map((e) => edgeId(e.etype, e.src, e.dst))).toEqual(
      [id],
    );
    expect(store.derivedEdges("skill_fit")).toHaveLength(1);
    expect(store.derivedEdges("other")).toHaveLength(0);

    store.setProvenance(id, null);
    expect(store.edges.get(id)?.derived).toBe(false);
    expect(store.edges.get(id)?.explanation).toBeUndefined();
    expect(store.derivedEdges()).toEqual([]);
  });

  it("setProvenance on an unknown edge is a no-op", () => {
    const store = new GraphStore();
    store.setProvenance(edgeId("FIT", "x", "y"), fit("x", "y"));
    expect(store.edges.size).toBe(0);
  });
});

describe("watch apply", () => {
  type Case = {
    name: string;
    setup: (s: GraphStore) => void;
    event: MutationEvent;
    check: (s: GraphStore) => void;
  };

  const cases: Case[] = [
    {
      name: "node_inserted ignored when not dirty-interest and no edge refs it",
      setup: seedPair,
      event: { node_inserted: { label: "Person", key: "c" } },
      check: (s) => {
        expect(s.nodes.has("c")).toBe(false);
      },
    },
    {
      name: "node_inserted added when key is dirty-interest",
      setup: (s) => {
        s.markDirty("c");
      },
      event: { node_inserted: { label: "Person", key: "c" } },
      check: (s) => {
        expect(s.nodes.get("c")).toEqual({ key: "c", label: "Person" });
        expect(s.dirty.has("c")).toBe(true);
      },
    },
    {
      name: "node_inserted added when an existing edge references the key",
      setup: (s) => {
        seedPair(s);
        s.nodes.delete("b");
      },
      event: { node_inserted: { label: "Person", key: "b" } },
      check: (s) => {
        expect(s.nodes.get("b")?.label).toBe("Person");
      },
    },
    {
      name: "node_inserted updates the label of an already-visible node",
      setup: seedPair,
      event: { node_inserted: { label: "Human", key: "b" } },
      check: (s) => {
        expect(s.nodes.get("b")?.label).toBe("Human");
      },
    },
    {
      name: "prop_set on a visible node marks dirty",
      setup: seedPair,
      event: { prop_set: { key: "b", field: "skills" } },
      check: (s) => {
        expect([...s.dirty]).toEqual(["b"]);
      },
    },
    {
      name: "prop_removed on a visible node marks dirty",
      setup: seedPair,
      event: { prop_removed: { key: "a", field: "skills" } },
      check: (s) => {
        expect([...s.dirty]).toEqual(["a"]);
      },
    },
    {
      name: "prop_set on an invisible node is ignored",
      setup: seedPair,
      event: { prop_set: { key: "ghost", field: "x" } },
      check: (s) => {
        expect(s.dirty.size).toBe(0);
        expect(s.nodes.has("ghost")).toBe(false);
      },
    },
    {
      name: "edge_inserted with both endpoints visible adds an unexplained edge",
      setup: (s) => {
        s.mergeQueryGraph(["n"], [["a"], ["b"]]);
      },
      event: {
        edge_inserted: { edge_type: "FIT", src: "a", dst: "b" },
      },
      check: (s) => {
        const e = s.edges.get(edgeId("FIT", "a", "b"));
        expect(e).toEqual({ etype: "FIT", src: "a", dst: "b" });
        expect(e?.derived).toBeUndefined();
        expect(e?.explanation).toBeUndefined();
        assertEndpoints(s);
      },
    },
    {
      name: "edge_inserted ignored unless both endpoints are visible",
      setup: (s) => {
        s.mergeQueryGraph(["n"], [["a"]]);
      },
      event: {
        edge_inserted: { edge_type: "KNOWS", src: "a", dst: "z" },
      },
      check: (s) => {
        expect(s.edges.size).toBe(0);
        expect(s.nodes.has("z")).toBe(false);
      },
    },
    {
      name: "edge_inserted does not overwrite cached provenance",
      setup: (s) => {
        seedPair(s);
        s.setProvenance(edgeId("KNOWS", "a", "b"), fit("a", "b"));
      },
      event: {
        edge_inserted: { edge_type: "KNOWS", src: "a", dst: "b" },
      },
      check: (s) => {
        expect(s.edges.get(edgeId("KNOWS", "a", "b"))?.derived).toBe(true);
      },
    },
    {
      name: "edge_deleted with both endpoints visible removes the edge",
      setup: seedPair,
      event: {
        edge_deleted: { edge_type: "KNOWS", src: "a", dst: "b" },
      },
      check: (s) => {
        expect(s.edges.size).toBe(0);
        expect(s.nodes.has("a")).toBe(true);
        expect(s.nodes.has("b")).toBe(true);
      },
    },
    {
      name: "edge_deleted no-ops when an endpoint is already gone",
      setup: (s) => {
        seedPair(s);
        // Legal path: node_deleted drops b and the incident edge. The
        // subsequent edge_deleted then hits the both-endpoints-visible
        // guard with a consistent graph (no dangling edge).
        s.apply({ node_deleted: { key: "b" } });
      },
      event: {
        edge_deleted: { edge_type: "KNOWS", src: "a", dst: "b" },
      },
      check: (s) => {
        expect(s.nodes.has("b")).toBe(false);
        expect(s.edges.has(edgeId("KNOWS", "a", "b"))).toBe(false);
        expect(s.nodes.has("a")).toBe(true);
        expect(s.edges.size).toBe(0);
        assertEndpoints(s);
      },
    },
    {
      name: "node_deleted removes the node, its incident edges, and drops it from dirty",
      setup: (s) => {
        seedPair(s);
        s.mergeNeighborhoodWithEdges("a", { LIKES: ["c"] });
        s.markDirty("b");
      },
      event: { node_deleted: { key: "b" } },
      check: (s) => {
        expect(s.nodes.has("b")).toBe(false);
        expect(s.edges.has(edgeId("KNOWS", "a", "b"))).toBe(false);
        expect(s.edges.has(edgeId("LIKES", "a", "c"))).toBe(true);
        expect(s.dirty.has("b")).toBe(false);
        assertEndpoints(s);
      },
    },
    {
      name: "rule_created marks every visible node dirty",
      setup: seedPair,
      event: { rule_created: { name: "skill_fit" } },
      check: (s) => {
        expect([...s.dirty].sort()).toEqual(["a", "b"]);
      },
    },
    {
      name: "rule_deleted marks every visible node dirty and keeps derived cache",
      setup: (s) => {
        seedPair(s);
        s.setProvenance(edgeId("KNOWS", "a", "b"), fit("a", "b"));
      },
      event: { rule_deleted: { name: "skill_fit" } },
      check: (s) => {
        expect([...s.dirty].sort()).toEqual(["a", "b"]);
        expect(s.edges.get(edgeId("KNOWS", "a", "b"))?.derived).toBe(true);
      },
    },
    {
      name: "rule_rebuilt marks every visible node dirty",
      setup: seedPair,
      event: { rule_rebuilt: { name: "skill_fit" } },
      check: (s) => {
        expect([...s.dirty].sort()).toEqual(["a", "b"]);
      },
    },
    {
      name: "batch_applied is a no-op (inner events already applied)",
      setup: seedPair,
      event: { batch_applied: { ops: 4 } },
      check: (s) => {
        expect(s.nodes.size).toBe(2);
        expect(s.edges.size).toBe(1);
        expect(s.dirty.size).toBe(0);
      },
    },
    {
      name: "ingested is a no-op (inner events already applied)",
      setup: seedPair,
      event: { ingested: { label: "Person", inserted: 3 } },
      check: (s) => {
        expect(s.nodes.size).toBe(2);
        expect(s.edges.size).toBe(1);
        expect(s.dirty.size).toBe(0);
      },
    },
  ];

  it.each(cases)("$name", ({ setup, event, check }) => {
    const store = new GraphStore();
    setup(store);
    store.apply(event);
    check(store);
  });
});

describe("selection cleanup", () => {
  it("node_deleted clears a selection of that node", () => {
    const store = new GraphStore();
    seedPair(store);
    store.select({ kind: "node", id: "b" });
    store.apply({ node_deleted: { key: "b" } });
    expect(store.selection).toBeNull();
  });

  it("node_deleted clears a selection of an incident edge", () => {
    const store = new GraphStore();
    seedPair(store);
    store.select({ kind: "edge", id: edgeId("KNOWS", "a", "b") });
    store.apply({ node_deleted: { key: "a" } });
    expect(store.selection).toBeNull();
    expect(store.edges.size).toBe(0);
  });

  it("node_deleted leaves an unrelated selection alone", () => {
    const store = new GraphStore();
    seedPair(store);
    store.mergeNeighborhoodWithEdges("a", { LIKES: ["c"] });
    store.select({ kind: "node", id: "c" });
    store.apply({ node_deleted: { key: "b" } });
    expect(store.selection).toEqual({ kind: "node", id: "c" });
  });

  it("edge_deleted clears a selection of that edge", () => {
    const store = new GraphStore();
    seedPair(store);
    store.select({ kind: "edge", id: edgeId("KNOWS", "a", "b") });
    store.apply({
      edge_deleted: { edge_type: "KNOWS", src: "a", dst: "b" },
    });
    expect(store.selection).toBeNull();
  });
});

describe("lagged", () => {
  it("marks every visible node key dirty", () => {
    const store = new GraphStore();
    store.fromNeighborhood(
      "m",
      neighborhood([
        ["z", "Org", 1],
        ["a", "Person", 1],
      ]),
    );
    store.markDirty("a");
    store.lagged();
    expect([...store.dirty].sort()).toEqual(["a", "m", "z"]);
  });
});

describe("derived lifecycle", () => {
  it("edge added → provenance set → rule_deleted marks dirty", () => {
    const store = new GraphStore();
    store.mergeQueryGraph(["n"], [["p1"], ["j1"]]);
    store.apply({
      edge_inserted: { edge_type: "FIT", src: "p1", dst: "j1" },
    });
    const id = edgeId("FIT", "p1", "j1");
    expect(store.edges.get(id)?.derived).toBeUndefined();

    store.setProvenance(id, fit("p1", "j1", 1));
    expect(store.derivedEdges("skill_fit")).toHaveLength(1);

    store.apply({ rule_deleted: { name: "skill_fit" } });
    expect([...store.dirty].sort()).toEqual(["j1", "p1"]);
    expect(store.edges.get(id)?.derived).toBe(true);
  });
});

describe("toCosmos", () => {
  function seeded(): GraphStore {
    const store = new GraphStore();
    store.mergeQueryGraph(["n"], [["z"], ["a"], ["m"]]);
    store.mergeNeighborhoodWithEdges("a", { KNOWS: ["z"], FIT: ["m"] });
    store.setProvenance(edgeId("FIT", "a", "m"), fit("a", "m"));
    return store;
  }

  it("orders points and links deterministically by sorted keys / edgeIds", () => {
    const a = seeded().toCosmos();
    const b = seeded().toCosmos();
    expect(a).toEqual(b);
    expect(a.pointKeys).toEqual(["a", "m", "z"]);
    expect(a.links).toEqual([
      [0, 1],
      [0, 2],
    ]);
  });

  it("assigns gold to derived, structure to user, signal to a selected node's edges", () => {
    const store = seeded();
    const idle = store.toCosmos();
    expect(linkColor(idle, 0, 1)).toEqual(COLOR.gold);
    expect(linkColor(idle, 0, 2)).toEqual(COLOR.structure);
    expect(idle.pointColors).toEqual([COLOR.paper, COLOR.paper, COLOR.paper]);

    store.select({ kind: "node", id: "a" });
    const focused = store.toCosmos();
    expect(focused.pointColors[0]).toEqual(COLOR.signal);
    expect(focused.pointColors[1]).toEqual(COLOR.paper);
    expect(linkColor(focused, 0, 1)).toEqual(COLOR.signal);
    expect(linkColor(focused, 0, 2)).toEqual(COLOR.signal);

    store.select({ kind: "edge", id: edgeId("FIT", "a", "m") });
    const edgeSel = store.toCosmos();
    expect(linkColor(edgeSel, 0, 1)).toEqual(COLOR.signal);
    expect(linkColor(edgeSel, 0, 2)).toEqual(COLOR.structure);
    expect(edgeSel.pointColors).toEqual([COLOR.paper, COLOR.paper, COLOR.paper]);
  });

  it("uses the brief token hex values as 0–1 RGBA", () => {
    expect(COLOR.gold).toEqual(hexRgba("#E8A33D"));
    expect(COLOR.structure).toEqual(hexRgba("#55627A"));
    expect(COLOR.signal).toEqual(hexRgba("#6FC3B8"));
    expect(COLOR.paper).toEqual(hexRgba("#E8E6E1"));
    expect(tokens).toContain("--gold: #E8A33D;");
    expect(tokens).toContain("--structure: #55627A;");
    expect(tokens).toContain("--signal: #6FC3B8;");
    expect(tokens).toContain("--paper: #E8E6E1;");
  });
});

describe("setNodeProps + clearDirty", () => {
  it("stores props on a visible node and clearDirty drops marks", () => {
    const store = new GraphStore();
    seedPair(store);
    store.setNodeProps("b", { skills: ["s1"] });
    expect(store.nodes.get("b")?.props).toEqual({ skills: ["s1"] });
    store.fromNeighborhood("a", neighborhood([["b", "Person", 1]]));
    expect(store.nodes.get("b")?.props).toEqual({ skills: ["s1"] });
    store.markDirty("a");
    store.markDirty("b");
    store.clearDirty(["a"]);
    expect([...store.dirty]).toEqual(["b"]);
    store.clearDirty();
    expect(store.dirty.size).toBe(0);
  });
});

function linkColor(
  snap: CosmosSnapshot,
  src: number,
  dst: number,
): readonly [number, number, number, number] {
  const i = snap.links.findIndex(([s, d]) => s === src && d === dst);
  expect(i).toBeGreaterThanOrEqual(0);
  return snap.linkColors[i]!;
}

function hexRgba(hex: string): [number, number, number, number] {
  const n = Number.parseInt(hex.slice(1), 16);
  return [
    ((n >> 16) & 255) / 255,
    ((n >> 8) & 255) / 255,
    (n & 255) / 255,
    1,
  ];
}
