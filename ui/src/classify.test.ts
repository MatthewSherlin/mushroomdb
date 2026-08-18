import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import type { EdgeInfo, Explanation, QueryResult } from "./api";
import { edgeId } from "./store";
import {
  EXPLAIN_CONCURRENCY,
  USER_ETYPE,
  classifyBothDirs,
  classifyFromEdges,
  concatResults,
  explainNeighbors,
  firstNodeKey,
  hopKeysAtDepth,
  mapPool,
  neighborKeys,
} from "./classify";

function nb(rows: Array<[string, string, number]>): QueryResult {
  return { columns: ["key", "label", "depth"], rows };
}

function exp(
  edgeType: string,
  src: string,
  dst: string,
  rule = "skill_fit",
): Explanation {
  return {
    rule,
    edge_type: edgeType,
    src_key: src,
    dst_key: dst,
    weight: 0.5,
    predicate: null,
  };
}

const here = dirname(fileURLToPath(import.meta.url));
const classifySrc = readFileSync(join(here, "classify.ts"), "utf8");

describe("module contract", () => {
  it("is a pure module: no DOM, canvas, or cosmos imports", () => {
    expect(classifySrc).not.toMatch(
      /from\s+["']@cosmos\.gl|document\.|window\.|HTMLCanvas|getContext\(/,
    );
  });
});

describe("USER_ETYPE", () => {
  it("is the generic related bucket for unnamed user edges", () => {
    expect(USER_ETYPE).toBe("related");
  });
});

function edge(
  edgeType: string,
  src: string,
  dst: string,
  derived: boolean,
): EdgeInfo {
  return { edge_type: edgeType, src_key: src, dst_key: dst, derived };
}

describe("classifyFromEdges", () => {
  it("keeps real user etypes and derived flags; no related synthesis", () => {
    const got = classifyFromEdges("a", [
      edge("KNOWS", "a", "c", false),
      edge("FIT", "a", "b", true),
      edge("WORKS_AT", "d", "a", false),
    ]);
    expect(got.out).toEqual({ FIT: ["b"], KNOWS: ["c"] });
    expect(got.in).toEqual({ WORKS_AT: ["d"] });
    expect(got.out[USER_ETYPE]).toBeUndefined();
    expect(got.provenance).toEqual([
      { id: edgeId("FIT", "a", "b"), derived: true, explanation: null },
      { id: edgeId("KNOWS", "a", "c"), derived: false, explanation: null },
      { id: edgeId("WORKS_AT", "d", "a"), derived: false, explanation: null },
    ]);
  });

  it("keeps mixed user + derived etypes on the same pair", () => {
    const got = classifyFromEdges("a", [
      edge("KNOWS", "a", "b", false),
      edge("FIT", "a", "b", true),
    ]);
    expect(got.out).toEqual({ KNOWS: ["b"], FIT: ["b"] });
    expect(got.out[USER_ETYPE]).toBeUndefined();
    expect(got.provenance).toEqual([
      { id: edgeId("FIT", "a", "b"), derived: true, explanation: null },
      { id: edgeId("KNOWS", "a", "b"), derived: false, explanation: null },
    ]);
  });

  it("emits a self-loop once on the out side", () => {
    const got = classifyFromEdges("a", [edge("LOOP", "a", "a", false)]);
    expect(got.out).toEqual({ LOOP: ["a"] });
    expect(got.in).toEqual({});
    expect(got.provenance).toEqual([
      { id: edgeId("LOOP", "a", "a"), derived: false, explanation: null },
    ]);
  });
});

describe("neighborKeys", () => {
  it("reads the key column and skips the empty / non-string cells", () => {
    const result: QueryResult = {
      columns: ["key", "label", "depth"],
      rows: [
        ["b", "Person", 1],
        ["", "Org", 1],
        [null, "x", 1],
        ["c", "Org", 1],
      ],
    };
    expect(neighborKeys(result)).toEqual(["b", "c"]);
  });

  it("returns [] when there is no key column", () => {
    expect(neighborKeys({ columns: ["n"], rows: [["p1"]] })).toEqual([]);
  });

  it("drops the root if the neighborhood includes it", () => {
    expect(neighborKeys(nb([["a", "Person", 0], ["b", "Org", 1]]), "a")).toEqual(
      ["b"],
    );
  });
});

describe("hopKeysAtDepth", () => {
  it("returns keys at the requested depth", () => {
    const result = nb([
      ["b", "Person", 1],
      ["c", "Org", 2],
      ["d", "Person", 1],
    ]);
    expect(hopKeysAtDepth(result, 1)).toEqual(["b", "d"]);
    expect(hopKeysAtDepth(result, 2)).toEqual(["c"]);
  });
});

describe("firstNodeKey", () => {
  it("returns the first non-empty string cell (RETURN n LIMIT 1)", () => {
    expect(
      firstNodeKey({ columns: ["n"], rows: [["person-01"], ["person-02"]] }),
    ).toBe("person-01");
  });

  it("returns undefined on an empty result", () => {
    expect(firstNodeKey({ columns: ["n"], rows: [] })).toBeUndefined();
  });
});

describe("concatResults", () => {
  it("concatenates rows that share columns", () => {
    const out = concatResults(
      nb([["b", "Person", 1]]),
      nb([["c", "Org", 1]]),
    );
    expect(out.columns).toEqual(["key", "label", "depth"]);
    expect(out.rows).toEqual([
      ["b", "Person", 1],
      ["c", "Org", 1],
    ]);
  });

  it("throws when a later result's columns differ", () => {
    expect(() =>
      concatResults(
        { columns: ["key", "label", "depth"], rows: [["b", "Person", 1]] },
        { columns: ["key", "label"], rows: [["c", "Org"]] },
      ),
    ).toThrow(/column mismatch/i);
  });
});

describe("mapPool", () => {
  it("preserves order and caps in-flight work", async () => {
    let inflight = 0;
    let max = 0;
    const started: number[] = [];
    const out = await mapPool([1, 2, 3, 4, 5], 2, async (n) => {
      started.push(n);
      inflight += 1;
      max = Math.max(max, inflight);
      await new Promise((r) => setTimeout(r, 15));
      inflight -= 1;
      return n * 2;
    });
    expect(out.results).toEqual([2, 4, 6, 8, 10]);
    expect(out.errors).toEqual([]);
    expect(max).toBeLessThanOrEqual(2);
    expect(started).toEqual([1, 2, 3, 4, 5]);
  });

  it("keeps processing remaining items after a mid-batch rejection", async () => {
    const seen: number[] = [];
    const out = await mapPool([1, 2, 3, 4], 2, async (n) => {
      seen.push(n);
      await new Promise((r) => setTimeout(r, 10));
      if (n === 2) {
        throw new Error("boom");
      }
      return n * 10;
    });
    expect(seen.sort()).toEqual([1, 2, 3, 4]);
    expect(out.results).toEqual([10, undefined, 30, 40]);
    expect(out.errors).toHaveLength(1);
    expect(out.errors[0]?.index).toBe(1);
    expect(out.errors[0]?.item).toBe(2);
    expect((out.errors[0]?.error as Error).message).toBe("boom");
  });
});

describe("explainNeighbors", () => {
  it("explains each neighbor once and uses the concurrency cap", async () => {
    let inflight = 0;
    let max = 0;
    const calls: Array<[string, string]> = [];
    const map = await explainNeighbors(
      async (a, b) => {
        calls.push([a, b]);
        inflight += 1;
        max = Math.max(max, inflight);
        await new Promise((r) => setTimeout(r, 10));
        inflight -= 1;
        return [exp("FIT", a, b)];
      },
      "root",
      ["n1", "n2", "n3"],
    );
    expect(calls).toEqual([
      ["root", "n1"],
      ["root", "n2"],
      ["root", "n3"],
    ]);
    expect(map.get("n1")?.[0]?.edge_type).toBe("FIT");
    expect(max).toBeLessThanOrEqual(EXPLAIN_CONCURRENCY);
  });
});

describe("classifyBothDirs", () => {
  it("attributes derived etypes from explanations and buckets the rest as related", () => {
    const got = classifyBothDirs({
      root: "a",
      outKeys: ["b", "c"],
      inKeys: ["d"],
      explanations: new Map([
        ["b", [exp("FIT", "a", "b")]],
        ["c", []],
        ["d", [exp("WORKS_AT", "d", "a")]],
      ]),
    });

    expect(got.out).toEqual({ FIT: ["b"], [USER_ETYPE]: ["c"] });
    expect(got.in).toEqual({ WORKS_AT: ["d"] });
    expect(got.provenance).toEqual([
      {
        id: edgeId("FIT", "a", "b"),
        derived: true,
        explanation: exp("FIT", "a", "b"),
      },
      {
        id: edgeId("WORKS_AT", "d", "a"),
        derived: true,
        explanation: exp("WORKS_AT", "d", "a"),
      },
      {
        id: edgeId(USER_ETYPE, "a", "c"),
        derived: false,
        explanation: null,
      },
    ]);
  });

  it("ignores an explanation whose direction does not match the neighborhood dir", () => {
    const got = classifyBothDirs({
      root: "a",
      outKeys: ["b"],
      inKeys: [],
      explanations: new Map([["b", [exp("FIT", "b", "a")]]]),
    });
    expect(got.out).toEqual({ [USER_ETYPE]: ["b"] });
    expect(got.in).toEqual({});
  });

  it("keeps multiple derived etypes between the same pair and does not also emit related", () => {
    const got = classifyBothDirs({
      root: "a",
      outKeys: ["b"],
      inKeys: [],
      explanations: new Map([
        ["b", [exp("FIT", "a", "b"), exp("TEAM", "a", "b", "same_team")]],
      ]),
    });
    expect(got.out).toEqual({ FIT: ["b"], TEAM: ["b"] });
    expect(got.out[USER_ETYPE]).toBeUndefined();
  });

  it("classifies an in-neighbor whose explanation arrives as {src_key: nbr, dst_key: root}", () => {
    const got = classifyBothDirs({
      root: "a",
      outKeys: [],
      inKeys: ["d"],
      explanations: new Map([["d", [exp("WORKS_AT", "d", "a")]]]),
    });
    expect(got.in).toEqual({ WORKS_AT: ["d"] });
    expect(got.out).toEqual({});
    expect(got.provenance).toEqual([
      {
        id: edgeId("WORKS_AT", "d", "a"),
        derived: true,
        explanation: exp("WORKS_AT", "d", "a"),
      },
    ]);
  });

  it("classifies a mutual neighbor independently per direction", () => {
    const got = classifyBothDirs({
      root: "a",
      outKeys: ["b"],
      inKeys: ["b"],
      explanations: new Map([["b", [exp("FIT", "a", "b")]]]),
    });
    expect(got.out).toEqual({ FIT: ["b"] });
    expect(got.in).toEqual({ [USER_ETYPE]: ["b"] });
  });

  it("skips the root if it appears in a neighbor list", () => {
    const got = classifyBothDirs({
      root: "a",
      outKeys: ["a", "b"],
      inKeys: [],
      explanations: new Map([["b", []]]),
    });
    expect(got.out).toEqual({ [USER_ETYPE]: ["b"] });
  });
});
