import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import { GraphStore, edgeId } from "./store";
import type { MutationEvent } from "./watch";
import type { ExpandApi } from "./expand";
import { USER_ETYPE } from "./classify";
import {
  TICKER_CAP,
  TickerBuffer,
  ResyncGate,
  applyLiveEvent,
  formatLaggedLine,
  formatTickerLine,
  glowBornDerived,
  nextDot,
  resyncNeighborhoods,
  resyncKeys,
  triggersResync,
  watchUrl,
} from "./live";

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, "live.ts"), "utf8");

function nb(rows: Array<[string, string, number]>) {
  return { columns: ["key", "label", "depth"] as string[], rows };
}

describe("module contract", () => {
  it("is a pure module: no DOM, canvas, or cosmos imports", () => {
    expect(src).not.toMatch(
      /from\s+["']@cosmos\.gl|document\.|window\.|HTMLCanvas|getContext\(/,
    );
  });

  it("documents the partial-endpoint edge_inserted decision", () => {
    expect(src).toMatch(/not fully expanded/i);
    expect(src).toMatch(/markDirty/);
  });
});

describe("TickerBuffer", () => {
  it("keeps the last N lines in arrival order", () => {
    const buf = new TickerBuffer(3);
    buf.push("a");
    buf.push("b");
    buf.push("c");
    buf.push("d");
    expect(buf.lines()).toEqual(["b", "c", "d"]);
    expect(buf.last()).toBe("d");
  });

  it("defaults to TICKER_CAP of 20", () => {
    const buf = new TickerBuffer();
    for (let i = 0; i < TICKER_CAP + 5; i++) {
      buf.push(String(i));
    }
    expect(buf.lines()).toHaveLength(20);
    expect(buf.lines()[0]).toBe("5");
    expect(buf.last()).toBe("24");
  });
});

describe("formatTickerLine", () => {
  const cases: Array<[MutationEvent, string]> = [
    [{ node_inserted: { label: "Person", key: "person-01" } }, "node inserted person-01"],
    [{ prop_set: { key: "person-01", field: "skills" } }, "prop set person-01.skills"],
    [{ prop_removed: { key: "person-01", field: "skills" } }, "prop removed person-01.skills"],
    [
      { edge_inserted: { edge_type: "FIT", src: "person-01", dst: "proj-01" } },
      "edge inserted FIT person-01 → proj-01",
    ],
    [
      { edge_deleted: { edge_type: "FIT", src: "person-01", dst: "proj-01" } },
      "edge deleted FIT person-01 → proj-01",
    ],
    [{ node_deleted: { key: "person-01" } }, "node deleted person-01"],
    [{ rule_created: { name: "skill_fit" } }, "rule created skill_fit"],
    [{ rule_deleted: { name: "skill_fit" } }, "rule deleted skill_fit"],
    [{ rule_rebuilt: { name: "skill_fit" } }, "rule rebuilt skill_fit"],
    [{ batch_applied: { ops: 3 } }, "batch applied 3"],
    [{ ingested: { label: "Person", inserted: 10 } }, "ingested Person 10"],
  ];

  it("is sentence case event name plus keys, no timestamp", () => {
    for (const [event, line] of cases) {
      expect(formatTickerLine(event)).toBe(line);
    }
    expect(formatLaggedLine(7)).toBe("lagged 7");
    expect(formatTickerLine(cases[0]![0])).not.toMatch(/\d{2}:\d{2}/);
  });
});

describe("nextDot", () => {
  it("flashes gold on an event unless reduced motion or not connected", () => {
    expect(nextDot("connected", "event", false)).toBe("flash");
    expect(nextDot("connected", "event", true)).toBe("connected");
    expect(nextDot("flash", "flash_end", false)).toBe("connected");
    expect(nextDot("idle", "event", false)).toBe("idle");
    expect(nextDot("connected", "reconnecting", false)).toBe("reconnecting");
    expect(nextDot("reconnecting", "connected", false)).toBe("connected");
    expect(nextDot("flash", "reconnecting", false)).toBe("reconnecting");
  });
});

describe("watchUrl", () => {
  it("uses ws or wss from the page location", () => {
    expect(watchUrl({ protocol: "http:", host: "127.0.0.1:5173" })).toBe(
      "ws://127.0.0.1:5173/watch",
    );
    expect(watchUrl({ protocol: "https:", host: "example.test" })).toBe(
      "wss://example.test/watch",
    );
  });

  it("forwards page ?token= onto /watch", () => {
    expect(
      watchUrl({
        protocol: "http:",
        host: "127.0.0.1:8080",
        search: "?token=s3cret",
      }),
    ).toBe("ws://127.0.0.1:8080/watch?token=s3cret");
    expect(
      watchUrl({
        protocol: "https:",
        host: "example.test",
        search: "?other=1",
      }),
    ).toBe("wss://example.test/watch");
  });
});

describe("applyLiveEvent", () => {
  it("adds an edge when both endpoints are visible and fully expanded", () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", nb([["b", "Person", 1]]));
    store.mergeNeighborhoodWithEdges("a", { KNOWS: ["b"] });
    expect(store.needsEdges("a")).toBe(false);
    expect(store.needsEdges("b")).toBe(true);
    store.fromNeighborhood("b", nb([["a", "Person", 1]]));
    store.mergeNeighborhoodWithEdges("b", { KNOWS: ["a"] });

    applyLiveEvent(store, {
      edge_inserted: { edge_type: "NOTE", src: "a", dst: "b" },
    });
    expect(store.edges.has(edgeId("NOTE", "a", "b"))).toBe(true);
  });

  it("marks a visible but not-fully-expanded node dirty and does not fabricate the edge", () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", nb([["b", "Person", 1]]));
    expect(store.needsEdges("a")).toBe(true);

    applyLiveEvent(store, {
      edge_inserted: { edge_type: "NOTE", src: "a", dst: "b" },
    });
    expect(store.edges.has(edgeId("NOTE", "a", "b"))).toBe(false);
    expect(store.dirty.has("a")).toBe(true);
    expect(store.dirty.has("b")).toBe(true);
  });

  it("marks only the visible endpoint dirty when the other is off-canvas", () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", nb([["b", "Person", 1]]));
    store.mergeNeighborhoodWithEdges("a", { KNOWS: ["b"] });

    applyLiveEvent(store, {
      edge_inserted: { edge_type: "NOTE", src: "a", dst: "ghost" },
    });
    expect(store.nodes.has("ghost")).toBe(false);
    expect(store.edges.has(edgeId("NOTE", "a", "ghost"))).toBe(false);
    expect(store.dirty.has("a")).toBe(true);
    expect(store.dirty.has("ghost")).toBe(false);
  });
});

describe("resyncKeys", () => {
  it("returns visible node keys in sorted order", () => {
    const store = new GraphStore();
    store.fromNeighborhood("c", nb([["a", "Person", 1], ["b", "Org", 1]]));
    expect(resyncKeys(store)).toEqual(["a", "b", "c"]);
  });
});

describe("triggersResync", () => {
  it("treats ingested, node_inserted, and edge_inserted as resync triggers", () => {
    expect(triggersResync({ ingested: { label: "Person", inserted: 1 } })).toBe(
      true,
    );
    expect(
      triggersResync({ node_inserted: { label: "Person", key: "p99" } }),
    ).toBe(true);
    expect(
      triggersResync({
        edge_inserted: { edge_type: "FIT", src: "a", dst: "b" },
      }),
    ).toBe(true);
    expect(triggersResync({ prop_set: { key: "a", field: "n" } })).toBe(false);
  });
});

describe("resyncNeighborhoods / glowBornDerived", () => {
  function apiWithNewFit(): ExpandApi {
    return {
      query: async () => ({ columns: ["n"], rows: [] }),
      neighborhood: async (key, opts = {}) => {
        const dir = opts.dir ?? "both";
        if (key === "a" && dir === "out") {
          return nb([["c", "Project", 1]]);
        }
        return nb([]);
      },
      explain: async (x, y) => {
        const pair = x === "a" && y === "c";
        if (!pair) {
          return [];
        }
        return [
          {
            rule: "skill_fit",
            edge_type: "FIT",
            src_key: "a",
            dst_key: "c",
            weight: 1,
            predicate: null,
          },
        ];
      },
      nodeInfo: async () => {
        throw new ApiError(404, "Not Found");
      },
      nodeEdges: async () => {
        throw new ApiError(404, "Not Found");
      },
    };
  }

  it("ingested-style resync surfaces a new derived edge and schedules glow", async () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", nb([["b", "Person", 1]]));
    store.mergeNeighborhoodWithEdges("a", { [USER_ETYPE]: ["b"] });
    store.setProvenance(edgeId(USER_ETYPE, "a", "b"), null);

    const event: MutationEvent = {
      ingested: { label: "Person", inserted: 1 },
    };
    expect(triggersResync(event)).toBe(true);

    const glow = await resyncNeighborhoods(store, apiWithNewFit(), false);
    expect(store.edges.get(edgeId("FIT", "a", "c"))?.derived).toBe(true);
    expect(glow).toEqual([edgeId("FIT", "a", "c")]);
  });

  it("skips glow when prefers-reduced-motion is on", async () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", nb([["b", "Person", 1]]));
    store.mergeNeighborhoodWithEdges("a", { [USER_ETYPE]: ["b"] });
    const glow = await resyncNeighborhoods(store, apiWithNewFit(), true);
    expect(store.edges.has(edgeId("FIT", "a", "c"))).toBe(true);
    expect(glow).toEqual([]);
    expect(
      glowBornDerived([edgeId("FIT", "a", "c")], [edgeId("FIT", "a", "c")], true),
    ).toEqual([]);
  });

  function apiWithTypedEdges(): { api: ExpandApi; explainCalls: { n: number } } {
    const explainCalls = { n: 0 };
    const api: ExpandApi = {
      query: async () => ({ columns: ["n"], rows: [] }),
      neighborhood: async (key, opts = {}) => {
        const dir = opts.dir ?? "both";
        if (key === "a" && (dir === "out" || dir === "both")) {
          return nb([["b", "Person", 1]]);
        }
        return nb([]);
      },
      explain: async () => {
        explainCalls.n += 1;
        return [];
      },
      nodeInfo: async () => {
        throw new ApiError(404, "Not Found");
      },
      nodeEdges: async (key) => {
        if (key === "a" || key === "b") {
          return {
            edges: [
              {
                edge_type: "WORKS_AT",
                src_key: "a",
                dst_key: "b",
                derived: false,
              },
              {
                edge_type: "FIT",
                src_key: "a",
                dst_key: "b",
                derived: true,
              },
            ],
          };
        }
        return { edges: [] };
      },
    };
    return { api, explainCalls };
  }

  it("new-server resync classifies from nodeEdges, not explain", async () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", nb([["b", "Person", 1]]));
    const { api, explainCalls } = apiWithTypedEdges();
    await resyncNeighborhoods(store, api, true);
    expect(store.edges.get(edgeId("WORKS_AT", "a", "b"))?.derived).toBe(false);
    expect(store.edges.get(edgeId("FIT", "a", "b"))?.derived).toBe(true);
    expect(store.edges.has(edgeId(USER_ETYPE, "a", "b"))).toBe(false);
    expect(explainCalls.n).toBe(0);
  });

  it("resync supersedes a live related ghost with the typed endpoint edge", async () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", nb([["b", "Person", 1]]));
    store.mergeNeighborhoodWithEdges("a", { [USER_ETYPE]: ["b"] });
    const ghost = edgeId(USER_ETYPE, "a", "b");
    store.setProvenance(ghost, null);
    store.select({ kind: "edge", id: ghost });

    const { api } = apiWithTypedEdges();
    await resyncNeighborhoods(store, api, true);

    expect(store.edges.has(ghost)).toBe(false);
    expect(store.edges.has(edgeId(USER_ETYPE, "b", "a"))).toBe(false);
    expect(store.edges.has(edgeId("WORKS_AT", "a", "b"))).toBe(true);
    expect(store.edges.has(edgeId("FIT", "a", "b"))).toBe(true);
    expect(store.selection).toBeNull();
  });
});

describe("ResyncGate", () => {
  it("coalesces overlapping requests into one queued run behind the active one", async () => {
    const gate = new ResyncGate();
    let inflight = 0;
    let max = 0;
    let runs = 0;
    const run = async (): Promise<void> => {
      runs += 1;
      inflight += 1;
      max = Math.max(max, inflight);
      await new Promise((r) => setTimeout(r, 25));
      inflight -= 1;
    };
    gate.request(run);
    gate.request(run);
    gate.request(run);
    await new Promise((r) => setTimeout(r, 80));
    expect(max).toBe(1);
    expect(runs).toBe(2);
  });
});
