import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import type { Explanation, NeighborhoodOpts, QueryResult } from "./api";
import { GraphStore, edgeId } from "./store";
import { USER_ETYPE } from "./classify";
import { expandNode, loadDemoNeighborhood, type ExpandApi } from "./expand";

function nb(rows: Array<[string, string, number]>): QueryResult {
  return { columns: ["key", "label", "depth"], rows };
}

function fit(src: string, dst: string): Explanation {
  return {
    rule: "skill_fit",
    edge_type: "FIT",
    src_key: src,
    dst_key: dst,
    weight: 1,
    predicate: null,
  };
}

type Call = { op: "neighborhood" | "explain" | "query"; arg: string };

function mockApi(spec: {
  neighborhood: Record<string, QueryResult>;
  explain?: Record<string, Explanation[]>;
  query?: QueryResult;
}): { api: ExpandApi; calls: Call[] } {
  const calls: Call[] = [];
  const api: ExpandApi = {
    query: async () => {
      calls.push({ op: "query", arg: "MATCH (n) RETURN n LIMIT 1" });
      return spec.query ?? { columns: ["n"], rows: [] };
    },
    neighborhood: async (key, opts: NeighborhoodOpts = {}) => {
      const dir = opts.dir ?? "both";
      const depth = opts.depth ?? 1;
      const token = `${key}|${depth}|${dir}`;
      calls.push({ op: "neighborhood", arg: token });
      const result = spec.neighborhood[token];
      if (result === undefined) {
        throw new Error(`unexpected neighborhood ${token}`);
      }
      return result;
    },
    explain: async (a, b) => {
      const token = `${a}|${b}`;
      calls.push({ op: "explain", arg: token });
      return spec.explain?.[token] ?? [];
    },
  };
  return { api, calls };
}

const expandSrc = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "expand.ts"),
  "utf8",
);

describe("module contract", () => {
  it("is a pure module: no DOM, canvas, or cosmos imports", () => {
    expect(expandSrc).not.toMatch(
      /from\s+["']@cosmos\.gl|document\.|window\.|HTMLCanvas|getContext\(/,
    );
  });
});

describe("expandNode", () => {
  it("depth 1: loads nodes from out+in, classifies via explain, merges only the queried root", async () => {
    const store = new GraphStore();
    store.fromNeighborhood("ghost", nb([["zzz", "Org", 1]]));
    expect(store.needsEdges("ghost")).toBe(true);

    const { api, calls } = mockApi({
      neighborhood: {
        "a|1|out": nb([["b", "Person", 1]]),
        "a|1|in": nb([["c", "Org", 1]]),
      },
      explain: {
        "a|b": [fit("a", "b")],
        "a|c": [],
      },
    });

    await expandNode(store, api, "a", 1);

    expect(store.nodes.get("b")?.label).toBe("Person");
    expect(store.nodes.get("c")?.label).toBe("Org");
    expect(store.nodes.get("a")?.label).toBe("");
    expect(store.edges.get(edgeId("FIT", "a", "b"))?.derived).toBe(true);
    expect(store.edges.get(edgeId(USER_ETYPE, "c", "a"))?.derived).toBe(false);
    expect(store.needsEdges("a")).toBe(false);
    expect(store.needsEdges("ghost")).toBe(true);

    expect(calls.filter((c) => c.op === "neighborhood").map((c) => c.arg)).toEqual(
      ["a|1|out", "a|1|in"],
    );
    expect(calls.filter((c) => c.op === "explain").map((c) => c.arg).sort()).toEqual(
      ["a|b", "a|c"],
    );
    expect(calls.some((c) => c.arg.startsWith("ghost"))).toBe(false);
  });

  it("does not invent a neighborhood call for a never-merged visible node", async () => {
    const store = new GraphStore();
    const { api, calls } = mockApi({
      neighborhood: {
        "a|1|out": nb([]),
        "a|1|in": nb([]),
      },
    });
    await expandNode(store, api, "a", 1);
    expect(calls.every((c) => c.arg.startsWith("a|"))).toBe(true);
    expect(store.needsEdges("a")).toBe(false);
  });

  it("depth 2: pulls hop-2 nodes then expands each hop-1 neighbor for edges", async () => {
    const store = new GraphStore();
    const { api, calls } = mockApi({
      neighborhood: {
        "a|2|both": nb([
          ["b", "Person", 1],
          ["d", "Org", 2],
        ]),
        "a|1|out": nb([["b", "Person", 1]]),
        "a|1|in": nb([]),
        "b|1|out": nb([["d", "Org", 1]]),
        "b|1|in": nb([["a", "Person", 1]]),
      },
      explain: {
        "a|b": [fit("a", "b")],
        "b|d": [],
        "b|a": [fit("a", "b")],
      },
    });

    await expandNode(store, api, "a", 2);

    expect([...store.nodes.keys()].sort()).toEqual(["a", "b", "d"]);
    expect(store.nodes.get("d")?.label).toBe("Org");
    expect(store.edges.has(edgeId("FIT", "a", "b"))).toBe(true);
    expect(store.edges.has(edgeId(USER_ETYPE, "b", "d"))).toBe(true);
    expect(store.needsEdges("a")).toBe(false);
    expect(store.needsEdges("b")).toBe(false);

    const nbCalls = calls.filter((c) => c.op === "neighborhood").map((c) => c.arg);
    expect(nbCalls).toContain("a|2|both");
    expect(nbCalls).toContain("b|1|out");
    expect(nbCalls).toContain("b|1|in");
  });

  it("depth 2: expands hop-1 neighbors concurrently under the pool cap", async () => {
    const store = new GraphStore();
    const { api } = mockApi({
      neighborhood: {
        "a|2|both": nb([
          ["b", "Person", 1],
          ["c", "Org", 1],
          ["d", "Org", 2],
          ["e", "Org", 2],
        ]),
        "a|1|out": nb([
          ["b", "Person", 1],
          ["c", "Org", 1],
        ]),
        "a|1|in": nb([]),
        "b|1|out": nb([["d", "Org", 1]]),
        "b|1|in": nb([["a", "Person", 1]]),
        "c|1|out": nb([["e", "Org", 1]]),
        "c|1|in": nb([["a", "Person", 1]]),
      },
      explain: {
        "a|b": [fit("a", "b")],
        "a|c": [fit("a", "c")],
        "b|d": [],
        "b|a": [fit("a", "b")],
        "c|e": [],
        "c|a": [fit("a", "c")],
      },
    });

    let inflight = 0;
    let max = 0;
    const origNb = api.neighborhood;
    api.neighborhood = async (key, opts) => {
      if (key !== "a") {
        inflight += 1;
        max = Math.max(max, inflight);
        await new Promise((r) => setTimeout(r, 20));
        inflight -= 1;
      }
      return origNb(key, opts);
    };

    await expandNode(store, api, "a", 2);

    expect(max).toBeGreaterThan(2);
    expect(store.needsEdges("b")).toBe(false);
    expect(store.needsEdges("c")).toBe(false);
    expect(store.edges.has(edgeId(USER_ETYPE, "b", "d"))).toBe(true);
    expect(store.edges.has(edgeId(USER_ETYPE, "c", "e"))).toBe(true);
  });
});

describe("loadDemoNeighborhood", () => {
  it("runs MATCH (n) RETURN n LIMIT 1 then expands that key at depth 1", async () => {
    const store = new GraphStore();
    const { api, calls } = mockApi({
      query: { columns: ["n"], rows: [["person-01"]] },
      neighborhood: {
        "person-01|1|out": nb([["proj-01", "Project", 1]]),
        "person-01|1|in": nb([]),
      },
      explain: { "person-01|proj-01": [fit("person-01", "proj-01")] },
    });

    const key = await loadDemoNeighborhood(store, api);
    expect(key).toBe("person-01");
    expect(calls[0]).toEqual({
      op: "query",
      arg: "MATCH (n) RETURN n LIMIT 1",
    });
    expect(store.edges.has(edgeId("FIT", "person-01", "proj-01"))).toBe(true);
  });

  it("throws when the demo query returns no nodes", async () => {
    const store = new GraphStore();
    const { api } = mockApi({
      query: { columns: ["n"], rows: [] },
      neighborhood: {},
    });
    await expect(loadDemoNeighborhood(store, api)).rejects.toThrow(/no nodes/i);
  });
});
