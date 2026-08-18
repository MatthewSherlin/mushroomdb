import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import type { NeighborhoodOpts, QueryResult } from "./api";
import { GraphStore, edgeId } from "./store";
import { USER_ETYPE } from "./classify";
import type { ExpandApi } from "./expand";
import {
  NO_HARVEST_HINT,
  NO_RESULT_HINT,
  addHarvestedToCanvas,
  formatCell,
  formatTable,
  harvestDecision,
  harvestableKeys,
  queryErrorText,
  resultAfterRun,
} from "./query-result";

function result(columns: string[], rows: QueryResult["rows"]): QueryResult {
  return { columns, rows };
}

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, "query-result.ts"), "utf8");

describe("module contract", () => {
  it("is a pure module: no DOM, canvas, or cosmos imports", () => {
    expect(src).not.toMatch(
      /from\s+["']@cosmos\.gl|document\.|window\.|HTMLCanvas|getContext\(/,
    );
  });
});

describe("harvestableKeys", () => {
  it("harvests RETURN n node-variable columns as keys", () => {
    expect(harvestableKeys(result(["n"], [["p1"], ["p2"]]))).toEqual([
      "p1",
      "p2",
    ]);
  });

  it("uses the key column and skips dotted property columns", () => {
    expect(
      harvestableKeys(
        result(
          ["key", "label", "n.name"],
          [
            ["p1", "Person", "Ada"],
            ["o1", "Org", "Acme"],
          ],
        ),
      ),
    ).toEqual(["p1", "o1"]);
  });

  it("does not harvest a sibling label cell beside a node variable", () => {
    expect(harvestableKeys(result(["n", "label"], [["p1", "Org"]]))).toEqual([
      "p1",
    ]);
  });

  it("returns [] when every column is a dotted property projection", () => {
    expect(
      harvestableKeys(result(["n.name", "n.age"], [["Ada", 31]])),
    ).toEqual([]);
  });

  it("skips reserved depth columns and non-string cells", () => {
    expect(
      harvestableKeys(result(["depth", "n"], [[1, "p1"], [2, null]])),
    ).toEqual(["p1"]);
  });

  it("dedupes keys in first-seen order", () => {
    expect(
      harvestableKeys(result(["n"], [["p1"], ["p2"], ["p1"]])),
    ).toEqual(["p1", "p2"]);
  });
});

describe("harvestDecision", () => {
  it("blocks Add to canvas before any result", () => {
    expect(harvestDecision(undefined)).toEqual({
      keys: [],
      blocked: NO_RESULT_HINT,
    });
  });

  it("blocks Add to canvas when nothing is harvestable", () => {
    expect(harvestDecision(result(["n.name"], [["Ada"]]))).toEqual({
      keys: [],
      blocked: NO_HARVEST_HINT,
    });
  });

  it("enables Add to canvas when node keys exist", () => {
    expect(harvestDecision(result(["n"], [["p1"]]))).toEqual({
      keys: ["p1"],
      blocked: undefined,
    });
  });

  it("failed Run drops the previous result so Add to canvas disables", () => {
    const prior = result(["n"], [["p1"]]);
    expect(harvestDecision(prior).blocked).toBeUndefined();
    const after = resultAfterRun({ ok: false });
    expect(after).toBeUndefined();
    expect(harvestDecision(after)).toEqual({
      keys: [],
      blocked: NO_RESULT_HINT,
    });
  });
});

describe("formatCell / formatTable", () => {
  it("renders scalars, null, and nested lists for the mono table", () => {
    expect(formatCell("p1")).toBe("p1");
    expect(formatCell(3)).toBe("3");
    expect(formatCell(true)).toBe("true");
    expect(formatCell(null)).toBe("null");
    expect(formatCell(["a", 1])).toBe('["a",1]');
  });

  it("shapes columns and formatted rows", () => {
    expect(
      formatTable(result(["n", "n.age"], [["p1", 31], ["p2", null]])),
    ).toEqual({
      columns: ["n", "n.age"],
      rows: [
        ["p1", "31"],
        ["p2", "null"],
      ],
    });
  });
});

describe("queryErrorText", () => {
  it("surfaces the server stage-prefixed detail verbatim", () => {
    const err = new ApiError(400, { error: "parse: unexpected end of input" });
    expect(queryErrorText(err)).toBe("parse: unexpected end of input");
  });

  it("does not invent a stage prefix when the body has no error field", () => {
    const err = new ApiError(422, "Failed to deserialize");
    expect(queryErrorText(err)).toBe("HTTP 422");
  });
});

describe("addHarvestedToCanvas", () => {
  it("expands each harvested key via expandNode and does not invent a query", async () => {
    const store = new GraphStore();
    const calls: string[] = [];
    const api: ExpandApi = {
      query: async () => {
        calls.push("query");
        return { columns: ["n"], rows: [] };
      },
      neighborhood: async (key, opts: NeighborhoodOpts = {}) => {
        const dir = opts.dir ?? "both";
        const depth = opts.depth ?? 1;
        calls.push(`${key}|${depth}|${dir}`);
        if (key === "p1" && dir === "out") {
          return { columns: ["key", "label", "depth"], rows: [["j1", "Job", 1]] };
        }
        return { columns: ["key", "label", "depth"], rows: [] };
      },
      explain: async () => [],
    };

    const keys = await addHarvestedToCanvas(
      store,
      api,
      result(["n"], [["p1"]]),
    );

    expect(keys).toEqual(["p1"]);
    expect(calls.some((c) => c === "query")).toBe(false);
    expect(calls).toContain("p1|1|out");
    expect(calls).toContain("p1|1|in");
    expect(store.nodes.has("p1")).toBe(true);
    expect(store.edges.has(edgeId(USER_ETYPE, "p1", "j1"))).toBe(true);
  });

  it("binds key+label projections before expanding neighborhoods", async () => {
    const store = new GraphStore();
    const api: ExpandApi = {
      query: async () => ({ columns: ["n"], rows: [] }),
      neighborhood: async () => ({
        columns: ["key", "label", "depth"],
        rows: [],
      }),
      explain: async () => [],
    };

    await addHarvestedToCanvas(
      store,
      api,
      result(["key", "label"], [["p1", "Person"]]),
    );

    expect(store.nodes.get("p1")?.label).toBe("Person");
  });

  it("is a no-op when nothing is harvestable", async () => {
    const store = new GraphStore();
    let neighborhoods = 0;
    const api: ExpandApi = {
      query: async () => ({ columns: ["n"], rows: [] }),
      neighborhood: async () => {
        neighborhoods += 1;
        return { columns: ["key", "label", "depth"], rows: [] };
      },
      explain: async () => [],
    };

    const keys = await addHarvestedToCanvas(
      store,
      api,
      result(["n.name"], [["Ada"]]),
    );
    expect(keys).toEqual([]);
    expect(neighborhoods).toBe(0);
    expect(store.nodes.size).toBe(0);
  });
});
