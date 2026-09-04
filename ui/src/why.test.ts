import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import type { Explanation, PredicateSummary, QueryResult } from "./api";
import { ApiError } from "./api";
import { GraphStore, edgeId } from "./store";
import {
  HAND_LINE,
  RECOMPUTED_NOTE,
  THRESHOLD_NOTE,
  buildWhyModel,
  ensureProvenance,
  fieldsFromPredicate,
  formatKeyMatchLine,
  formatOverlapLine,
  formatScore,
  highlightedIdsForRule,
  loadNodeProps,
  markTokens,
  nodePropsQuery,
  overlapFromProps,
  propsFromResult,
  whyEdgeMissing,
} from "./why";

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, "why.ts"), "utf8");

function exp(over: Partial<Explanation> = {}): Explanation {
  return {
    rule: "skill_fit",
    edge_type: "FIT",
    src_key: "person-01",
    dst_key: "proj-01",
    weight: 1,
    predicate: null,
    ...over,
  };
}

function pred(
  over: Partial<PredicateSummary> & Pick<PredicateSummary, "kind">,
): PredicateSummary {
  return {
    fields: [],
    min: null,
    tolerance: null,
    km: null,
    parts: null,
    ...over,
  };
}

describe("module contract", () => {
  it("is a pure module: no DOM, canvas, or cosmos imports", () => {
    expect(src).not.toMatch(
      /from\s+["']@cosmos\.gl|document\.|window\.|HTMLCanvas|getContext\(/,
    );
  });

  it("documents node_info as the primary props path and MATCH as legacy only", () => {
    expect(src).toMatch(/GET \/node\/\{key\}/);
    expect(src).toMatch(/Legacy/);
    expect(src).toMatch(/MATCH \(n \{id: \$key\}\)/);
  });
});

describe("overlapFromProps / formatOverlapLine", () => {
  const srcProps = { skills: ["s01", "s02", "s03"], name: "Ada" };
  const dstProps = { skills: ["s02", "s03", "s04"], name: "Proj" };

  it("computes Jaccard from the two nodes' actual token lists", () => {
    const got = overlapFromProps(srcProps, dstProps, 0.5);
    expect(got).toEqual({
      field: "skills",
      src: ["s01", "s02", "s03"],
      dst: ["s02", "s03", "s04"],
      shared: ["s02", "s03"],
      union: ["s01", "s02", "s03", "s04"],
      score: 0.5,
    });
  });

  it("formats the brief's arithmetic line from those sets", () => {
    const got = overlapFromProps(srcProps, dstProps, 0.5)!;
    expect(formatOverlapLine(got)).toBe(
      "overlap(skills) = |{s02, s03}| / |{s01, s02, s03, s04}| = 0.5",
    );
  });

  it("picks the list field whose Jaccard matches the explanation weight", () => {
    const got = overlapFromProps(
      { skills: ["s01"], tags: ["a", "b"] },
      { skills: ["s01", "s02"], tags: ["a", "b"] },
      1,
    );
    expect(got?.field).toBe("tags");
    expect(got?.score).toBe(1);
  });

  it("returns undefined when neither side has a token list", () => {
    expect(overlapFromProps({ org_id: "org-07" }, { name: "Org" }, 1)).toBeUndefined();
  });

  it("returns undefined when both token lists are empty", () => {
    expect(overlapFromProps({ skills: [] }, { skills: [] }, 1)).toBeUndefined();
  });

  it("keeps unicode tokens in the intersection", () => {
    const got = overlapFromProps(
      { tags: ["東京", "α"] },
      { tags: ["東京", "β"] },
      0.333,
    );
    expect(got?.shared).toEqual(["東京"]);
    expect(got?.union).toEqual(["α", "β", "東京"]);
  });
});

describe("key_match line", () => {
  it("formats the matched field and destination key", () => {
    expect(
      formatKeyMatchLine({
        label: "Person",
        field: "org_id",
        value: "org-07",
      }),
    ).toBe('person.org_id = "org-07" → org-07');
    expect(
      formatKeyMatchLine({
        label: "",
        field: "org_id",
        value: "org-07",
      }),
    ).toBe('org_id = "org-07" → org-07');
    expect(
      formatKeyMatchLine({
        label: "File",
        field: "imports",
        value: "a.rs",
        inList: true,
      }),
    ).toBe('file.imports ∋ "a.rs" → a.rs');
  });
});

describe("markTokens", () => {
  it("flags shared tokens for gold highlighting", () => {
    expect(markTokens(["s01", "s02", "s03"], new Set(["s02", "s03"]))).toEqual([
      { token: "s01", shared: false },
      { token: "s02", shared: true },
      { token: "s03", shared: true },
    ]);
  });
});

describe("formatScore", () => {
  it("caps at 3 decimals and trims trailing zeros", () => {
    expect(formatScore(1)).toBe("1");
    expect(formatScore(0.5)).toBe("0.5");
    expect(formatScore(1 / 3)).toBe("0.333");
  });
});

describe("loadNodeProps", () => {
  it("loads from node_info and filters the return to summary.fields", async () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", {
      columns: ["key", "label", "depth"],
      rows: [["b", "Person", 1]],
    });
    const queries: string[] = [];
    const props = await loadNodeProps(
      store,
      {
        nodeInfo: async (key) => ({
          key,
          label: "Person",
          props: { skills: ["s01"], org_id: "org-07", extra: 1 },
        }),
        query: async (cypher) => {
          queries.push(cypher);
          return { columns: [], rows: [] };
        },
      },
      "b",
      ["skills", "org_id"],
    );
    expect(props).toEqual({ skills: ["s01"], org_id: "org-07" });
    expect(store.nodes.get("b")?.props).toEqual({
      skills: ["s01"],
      org_id: "org-07",
      extra: 1,
    });
    expect(store.nodes.get("b")?.label).toBe("Person");
    expect(queries).toEqual([]);
  });

  it("falls back to the exact-key MATCH when /node/{key} is absent", async () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", {
      columns: ["key", "label", "depth"],
      rows: [["b", "Person", 1]],
    });
    const queries: string[] = [];
    const props = await loadNodeProps(
      store,
      {
        nodeInfo: async () => {
          throw new ApiError(404, "Not Found");
        },
        query: async (cypher) => {
          queries.push(cypher);
          return {
            columns: ["skills"],
            rows: [[["s01"]]],
          };
        },
      },
      "b",
    );
    expect(props).toEqual({ skills: ["s01"] });
    expect(queries[0]).toContain("MATCH (n {id: $key})");
  });

  it("does not MATCH when the 404 is a key miss", async () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", {
      columns: ["key", "label", "depth"],
      rows: [["b", "Person", 1]],
    });
    let queried = false;
    const props = await loadNodeProps(
      store,
      {
        nodeInfo: async () => {
          throw new ApiError(404, { error: "node key not found: b" });
        },
        query: async () => {
          queried = true;
          return { columns: [], rows: [] };
        },
      },
      "b",
    );
    expect(props).toEqual({});
    expect(queried).toBe(false);
  });
});

describe("propsFromResult / nodePropsQuery", () => {
  it("builds an exact-key MATCH projecting watched fields", () => {
    const q = nodePropsQuery("person-01");
    expect(q.cypher).toContain("MATCH (n {id: $key})");
    expect(q.cypher).toContain("n.skills AS skills");
    expect(q.params).toEqual({ key: "person-01" });
  });

  it("takes the first row and drops null cells", () => {
    const result: QueryResult = {
      columns: ["skills", "org_id", "name"],
      rows: [[["s01", "s02"], "org-07", null]],
    };
    expect(propsFromResult(result)).toEqual({
      skills: ["s01", "s02"],
      org_id: "org-07",
    });
  });
});

describe("buildWhyModel", () => {
  it("builds overlap arithmetic for a derived FIT edge", () => {
    const model = buildWhyModel({
      edge: {
        etype: "FIT",
        src: "person-01",
        dst: "proj-02",
        derived: true,
        explanation: exp({
          dst_key: "proj-02",
          weight: 0.5,
        }),
      },
      src: {
        key: "person-01",
        label: "Person",
        props: { skills: ["s01", "s02", "s03"] },
      },
      dst: {
        key: "proj-02",
        label: "Project",
        props: { skills: ["s02", "s03", "s04"] },
      },
    });
    expect(model.kind).toBe("overlap");
    if (model.kind !== "overlap") {
      return;
    }
    expect(model.rule).toBe("skill_fit");
    expect(model.etype).toBe("FIT");
    expect(model.weight).toBe(0.5);
    expect(model.line).toBe(
      "overlap(skills) = |{s02, s03}| / |{s01, s02, s03, s04}| = 0.5",
    );
    expect(model.srcTokens.some((t) => t.token === "s02" && t.shared)).toBe(true);
    expect(model.srcTokens.some((t) => t.token === "s01" && !t.shared)).toBe(true);
  });

  it("builds a key_match line from the FK field equal to the dst key", () => {
    const model = buildWhyModel({
      edge: {
        etype: "ORG",
        src: "person-01",
        dst: "org-07",
        derived: true,
        explanation: exp({
          rule: "auto_fk_person_org_id",
          edge_type: "ORG",
          dst_key: "org-07",
          weight: null,
        }),
      },
      src: {
        key: "person-01",
        label: "Person",
        props: { org_id: "org-07", project_id: "proj-01" },
      },
      dst: { key: "org-07", label: "Org", props: { name: "Org 7" } },
    });
    expect(model).toMatchObject({
      kind: "key_match",
      rule: "auto_fk_person_org_id",
      etype: "ORG",
      line: 'person.org_id = "org-07" → org-07',
    });
  });

  it("builds a key_match line from a list-valued FK holding the dst key", () => {
    // The server reads a list KeyMatch field as a set of foreign keys. Without
    // list awareness this fell through to the field_equality branch and the
    // shared `region` prop was rendered as the reason for an IMPORTS edge.
    const model = buildWhyModel({
      edge: {
        etype: "IMPORTS",
        src: "main.rs",
        dst: "a.rs",
        derived: true,
        explanation: exp({
          rule: "imports",
          edge_type: "IMPORTS",
          src_key: "main.rs",
          dst_key: "a.rs",
          weight: null,
        }),
      },
      src: {
        key: "main.rs",
        label: "File",
        props: { imports: ["a.rs", "b.rs"], region: "emea" },
      },
      dst: { key: "a.rs", label: "Mod", props: { region: "emea" } },
    });
    expect(model).toMatchObject({
      kind: "key_match",
      rule: "imports",
      etype: "IMPORTS",
      line: 'file.imports ∋ "a.rs" → a.rs',
    });
  });

  it("builds a key_match line for a list field named by the predicate summary", () => {
    const model = buildWhyModel({
      edge: {
        etype: "IMPORTS",
        src: "main.rs",
        dst: "b.rs",
        derived: true,
        explanation: exp({
          rule: "imports",
          edge_type: "IMPORTS",
          src_key: "main.rs",
          dst_key: "b.rs",
          weight: 1,
          predicate: pred({ kind: "key_match", fields: ["imports"] }),
        }),
      },
      src: {
        key: "main.rs",
        label: "File",
        props: { imports: ["a.rs", "b.rs"] },
      },
      dst: { key: "b.rs", label: "Mod", props: {} },
    });
    expect(model).toMatchObject({
      kind: "key_match",
      rule: "imports",
      line: 'file.imports ∋ "b.rs" → b.rs',
    });
  });

  it("does not treat a null-weight FK edge as overlap when skills also intersect", () => {
    const model = buildWhyModel({
      edge: {
        etype: "ORG",
        src: "person-01",
        dst: "org-01",
        derived: true,
        explanation: exp({
          rule: "auto_fk_person_org_id",
          edge_type: "ORG",
          dst_key: "org-01",
          weight: null,
        }),
      },
      src: {
        key: "person-01",
        label: "Person",
        props: { org_id: "org-01", skills: ["s01", "s02", "s03"] },
      },
      dst: {
        key: "org-01",
        label: "Org",
        props: { skills: ["s01", "s02", "s03"] },
      },
    });
    expect(model.kind).toBe("key_match");
  });

  it("notes when the recomputed overlap disagrees with the server weight", () => {
    const model = buildWhyModel({
      edge: {
        etype: "FIT",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          src_key: "a",
          dst_key: "b",
          weight: 0.5,
        }),
      },
      src: { key: "a", label: "Person", props: { skills: ["s01", "s02", "s03", "s04"] } },
      dst: { key: "b", label: "Project", props: { skills: ["s01"] } },
    });
    expect(model.kind).toBe("overlap");
    if (model.kind !== "overlap") {
      return;
    }
    expect(model.weight).toBe(0.5);
    expect(model.line).toBe(
      "overlap(skills) = |{s01}| / |{s01, s02, s03, s04}| = 0.25 — recomputed from current props",
    );
  });

  it("renders FieldEqual as a shared-value line, not KeyMatch", () => {
    const model = buildWhyModel({
      edge: {
        etype: "SAME_REGION",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "same_region",
          edge_type: "SAME_REGION",
          src_key: "a",
          dst_key: "b",
          weight: null,
        }),
      },
      src: { key: "a", label: "Person", props: { region: "emea" } },
      dst: { key: "b", label: "Person", props: { region: "emea" } },
    });
    expect(model).toMatchObject({
      kind: "field_equal",
      rule: "same_region",
      field: "region",
      value: "emea",
      line: 'field_equal(region): "emea" = "emea"',
    });
  });

  it("gives derived edges an honest body when no arithmetic can be rebuilt", () => {
    const model = buildWhyModel({
      edge: {
        etype: "FIT",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({ src_key: "a", dst_key: "b", weight: null }),
      },
      src: { key: "a", label: "Person" },
      dst: { key: "b", label: "Project" },
    });
    expect(model).toMatchObject({
      kind: "derived",
      rule: "skill_fit",
      srcKey: "a",
      dstKey: "b",
      line: "Derived by rule skill_fit",
    });
  });

  it("does not invent arithmetic for a user edge", () => {
    const model = buildWhyModel({
      edge: { etype: "related", src: "a", dst: "b", derived: false },
      src: { key: "a", label: "Person", props: { skills: ["s01"] } },
      dst: { key: "b", label: "Job", props: { skills: ["s01"] } },
    });
    expect(model).toEqual({
      kind: "hand",
      etype: "related",
      src: "a",
      dst: "b",
      line: HAND_LINE,
    });
  });
});

describe("ensureProvenance / highlightedIdsForRule", () => {
  it("explains only edges with unknown derived and never name-guesses", async () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", {
      columns: ["key", "label", "depth"],
      rows: [
        ["b", "Person", 1],
        ["c", "Org", 1],
      ],
    });
    store.mergeNeighborhoodWithEdges("a", { FIT: ["b"], related: ["c"] });
    store.setProvenance(edgeId("FIT", "a", "b"), exp({ src_key: "a", dst_key: "b" }));

    const calls: Array<[string, string]> = [];
    await ensureProvenance(store, {
      explain: async (x, y) => {
        calls.push([x, y]);
        return [];
      },
    });

    expect(calls).toEqual([["a", "c"]]);
    expect(store.edges.get(edgeId("related", "a", "c"))?.derived).toBe(false);
    expect(highlightedIdsForRule(store, "skill_fit")).toEqual([
      edgeId("FIT", "a", "b"),
    ]);
  });

  it("explains derived edges that have no explanation yet (endpoint flag only)", async () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", {
      columns: ["key", "label", "depth"],
      rows: [["b", "Person", 1]],
    });
    store.mergeNeighborhoodWithEdges("a", { FIT: ["b"] });
    store.setDerived(edgeId("FIT", "a", "b"), true);

    const calls: Array<[string, string]> = [];
    await ensureProvenance(store, {
      explain: async (x, y) => {
        calls.push([x, y]);
        return [exp({ src_key: "a", dst_key: "b" })];
      },
    });
    expect(calls).toEqual([["a", "b"]]);
    expect(store.edges.get(edgeId("FIT", "a", "b"))?.explanation?.rule).toBe(
      "skill_fit",
    );
  });
});

describe("whyEdgeMissing", () => {
  it("is true only when the open edge id is no longer in the store", () => {
    const store = new GraphStore();
    store.fromNeighborhood("a", {
      columns: ["key", "label", "depth"],
      rows: [["b", "Person", 1]],
    });
    store.mergeNeighborhoodWithEdges("a", { FIT: ["b"] });
    const id = edgeId("FIT", "a", "b");
    expect(whyEdgeMissing(store, id)).toBe(false);
    expect(whyEdgeMissing(store, undefined)).toBe(false);
    store.apply({
      edge_deleted: { edge_type: "FIT", src: "a", dst: "b" },
    });
    expect(whyEdgeMissing(store, id)).toBe(true);
  });
});

describe("nodePropsQuery from summary.fields", () => {
  it("projects summary.fields instead of the fallback PROP_FIELDS list", () => {
    const q = nodePropsQuery("org-01", ["founded_year", "location"]);
    expect(q.cypher).toBe(
      "MATCH (n {id: $key}) RETURN n.founded_year AS founded_year, n.location AS location",
    );
    expect(q.cypher).not.toContain("skills");
    expect(q.params).toEqual({ key: "org-01" });
  });

  it("walks all.fields first-seen union for the per-edge fetch", () => {
    const summary = pred({
      kind: "all",
      fields: ["founded_year", "embedding"],
      parts: [
        pred({
          kind: "numeric_within",
          fields: ["founded_year"],
          tolerance: 2,
        }),
        pred({
          kind: "vector_similar",
          fields: ["embedding"],
          min: 0.9,
        }),
      ],
    });
    const q = nodePropsQuery("a", summary.fields);
    expect(q.cypher).toContain("n.founded_year AS founded_year");
    expect(q.cypher).toContain("n.embedding AS embedding");
    expect(q.cypher).not.toContain("n.skills");
  });

  it("falls back to PROP_FIELDS when no summary fields are supplied", () => {
    const q = nodePropsQuery("person-01");
    expect(q.cypher).toContain("n.skills AS skills");
    expect(q.cypher).toContain("n.org_id AS org_id");
  });

  it("unions part fields when kind all has empty top-level fields", () => {
    const summary = pred({
      kind: "all",
      fields: [],
      parts: [
        pred({
          kind: "numeric_within",
          fields: ["founded_year"],
          tolerance: 2,
        }),
        pred({
          kind: "geo_radius",
          fields: ["location"],
          km: 400,
        }),
      ],
    });
    const fields = fieldsFromPredicate(summary);
    expect(fields).toEqual(["founded_year", "location"]);
    const q = nodePropsQuery("org-01", fields);
    expect(q.cypher).toBe(
      "MATCH (n {id: $key}) RETURN n.founded_year AS founded_year, n.location AS location",
    );
  });
});

describe("buildWhyModel predicate summary dispatch", () => {
  const paris = [48.8566, 2.3522];
  const london = [51.5074, -0.1278];

  it("renders numeric_within arithmetic including ≤ tolerance at score 0", () => {
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "close_year",
          edge_type: "NEAR",
          src_key: "a",
          dst_key: "b",
          weight: 0,
          predicate: pred({
            kind: "numeric_within",
            fields: ["founded_year"],
            tolerance: 2,
          }),
        }),
      },
      src: { key: "a", label: "Org", props: { founded_year: 1998 } },
      dst: { key: "b", label: "Org", props: { founded_year: 2000 } },
    });
    expect(model).toMatchObject({
      kind: "numeric_within",
      rule: "close_year",
      weight: 0,
      field: "founded_year",
      line: "numeric_within(founded_year) = |1998 − 2000| = 2 ≤ 2",
    });
  });

  it("renders numeric > tolerance with a stale-threshold note", () => {
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "close_year",
          edge_type: "NEAR",
          src_key: "a",
          dst_key: "b",
          weight: 0,
          predicate: pred({
            kind: "numeric_within",
            fields: ["founded_year"],
            tolerance: 2,
          }),
        }),
      },
      src: { key: "a", label: "Org", props: { founded_year: 1998 } },
      dst: { key: "b", label: "Org", props: { founded_year: 2005 } },
    });
    expect(model.kind).toBe("numeric_within");
    if (model.kind !== "numeric_within") {
      return;
    }
    expect(model.line).toBe(
      `numeric_within(founded_year) = |1998 − 2005| = 7 > 2 — ${THRESHOLD_NOTE}`,
    );
    expect(model.line).not.toContain("≤");
  });

  it("notes when recomputed numeric score disagrees with the server weight", () => {
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "close_year",
          edge_type: "NEAR",
          src_key: "a",
          dst_key: "b",
          weight: 0.5,
          predicate: pred({
            kind: "numeric_within",
            fields: ["founded_year"],
            tolerance: 2,
          }),
        }),
      },
      src: { key: "a", label: "Org", props: { founded_year: 1998 } },
      dst: { key: "b", label: "Org", props: { founded_year: 2000 } },
    });
    expect(model.kind).toBe("numeric_within");
    if (model.kind !== "numeric_within") {
      return;
    }
    expect(model.line).toBe(
      `numeric_within(founded_year) = |1998 − 2000| = 2 ≤ 2 — ${RECOMPUTED_NOTE}`,
    );
  });

  it("falls back to the honest derived line when a numeric prop is missing", () => {
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "close_year",
          edge_type: "NEAR",
          src_key: "a",
          dst_key: "b",
          weight: 0,
          predicate: pred({
            kind: "numeric_within",
            fields: ["founded_year"],
            tolerance: 2,
          }),
        }),
      },
      src: { key: "a", label: "Org", props: { name: "Ada" } },
      dst: { key: "b", label: "Org", props: { founded_year: 2000 } },
    });
    expect(model).toMatchObject({
      kind: "derived",
      line: "Derived by rule close_year",
    });
  });

  it("renders geo_radius haversine to 1 decimal km", () => {
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "paris",
        dst: "london",
        derived: true,
        explanation: exp({
          rule: "nearby",
          edge_type: "NEAR",
          src_key: "paris",
          dst_key: "london",
          weight: 0.14110866279779188,
          predicate: pred({
            kind: "geo_radius",
            fields: ["location"],
            km: 400,
          }),
        }),
      },
      src: { key: "paris", label: "Office", props: { location: paris } },
      dst: { key: "london", label: "Office", props: { location: london } },
    });
    expect(model.kind).toBe("geo_radius");
    if (model.kind !== "geo_radius") {
      return;
    }
    expect(model.line).toBe("geo_radius(location) = 343.6 km ≤ 400 km");
    expect(model.line).not.toContain(RECOMPUTED_NOTE);
  });

  it("falls back to derived when either node lacks coordinates", () => {
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "paris",
        dst: "london",
        derived: true,
        explanation: exp({
          rule: "nearby",
          edge_type: "NEAR",
          src_key: "paris",
          dst_key: "london",
          weight: 0.141,
          predicate: pred({
            kind: "geo_radius",
            fields: ["location"],
            km: 400,
          }),
        }),
      },
      src: { key: "paris", label: "Office", props: { location: paris } },
      dst: { key: "london", label: "Office", props: { name: "London" } },
    });
    expect(model).toMatchObject({
      kind: "derived",
      line: "Derived by rule nearby",
    });
  });

  it("renders geo > radius with a stale-threshold note", () => {
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "paris",
        dst: "london",
        derived: true,
        explanation: exp({
          rule: "nearby",
          edge_type: "NEAR",
          src_key: "paris",
          dst_key: "london",
          weight: 0.141,
          predicate: pred({
            kind: "geo_radius",
            fields: ["location"],
            km: 300,
          }),
        }),
      },
      src: { key: "paris", label: "Office", props: { location: paris } },
      dst: { key: "london", label: "Office", props: { location: london } },
    });
    expect(model.kind).toBe("geo_radius");
    if (model.kind !== "geo_radius") {
      return;
    }
    expect(model.line).toBe(
      `geo_radius(location) = 343.6 km > 300 km — ${THRESHOLD_NOTE}`,
    );
    expect(model.line).not.toContain("≤");
  });

  it("notes when recomputed geo score disagrees with the server weight", () => {
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "paris",
        dst: "london",
        derived: true,
        explanation: exp({
          rule: "nearby",
          edge_type: "NEAR",
          src_key: "paris",
          dst_key: "london",
          weight: 0.5,
          predicate: pred({
            kind: "geo_radius",
            fields: ["location"],
            km: 400,
          }),
        }),
      },
      src: { key: "paris", label: "Office", props: { location: paris } },
      dst: { key: "london", label: "Office", props: { location: london } },
    });
    expect(model.kind).toBe("geo_radius");
    if (model.kind !== "geo_radius") {
      return;
    }
    expect(model.line).toBe(
      `geo_radius(location) = 343.6 km ≤ 400 km — ${RECOMPUTED_NOTE}`,
    );
  });

  it("renders vector_similar cosine with a dimension note", () => {
    const model = buildWhyModel({
      edge: {
        etype: "SIM",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "similar",
          edge_type: "SIM",
          src_key: "a",
          dst_key: "b",
          weight: 0.97,
          predicate: pred({
            kind: "vector_similar",
            fields: ["embedding"],
            min: 0.9,
          }),
        }),
      },
      src: { key: "a", label: "Doc", props: { embedding: [1, 0] } },
      dst: {
        key: "b",
        label: "Doc",
        props: { embedding: [0.97, Math.sqrt(1 - 0.97 ** 2)] },
      },
    });
    expect(model.kind).toBe("vector_similar");
    if (model.kind !== "vector_similar") {
      return;
    }
    expect(model.line).toBe(
      "vector_similar(embedding) = cos = 0.97 ≥ 0.9 · d=2",
    );
  });

  it("renders vector ≥ boundary when cosine equals min", () => {
    const model = buildWhyModel({
      edge: {
        etype: "SIM",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "similar",
          edge_type: "SIM",
          src_key: "a",
          dst_key: "b",
          weight: 0.9,
          predicate: pred({
            kind: "vector_similar",
            fields: ["embedding"],
            min: 0.9,
          }),
        }),
      },
      src: { key: "a", label: "Doc", props: { embedding: [1, 0] } },
      dst: {
        key: "b",
        label: "Doc",
        props: { embedding: [0.9, Math.sqrt(1 - 0.9 ** 2)] },
      },
    });
    expect(model.kind).toBe("vector_similar");
    if (model.kind !== "vector_similar") {
      return;
    }
    expect(model.line).toBe(
      "vector_similar(embedding) = cos = 0.9 ≥ 0.9 · d=2",
    );
  });

  it("echoes explanation weight when vector dims exceed 64", () => {
    const big = Array.from({ length: 65 }, () => 1);
    const model = buildWhyModel({
      edge: {
        etype: "SIM",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "similar",
          edge_type: "SIM",
          src_key: "a",
          dst_key: "b",
          weight: 0.97,
          predicate: pred({
            kind: "vector_similar",
            fields: ["embedding"],
            min: 0.9,
          }),
        }),
      },
      src: { key: "a", label: "Doc", props: { embedding: big } },
      dst: { key: "b", label: "Doc", props: { embedding: big } },
    });
    expect(model.kind).toBe("vector_similar");
    if (model.kind !== "vector_similar") {
      return;
    }
    expect(model.line).toBe(
      "vector_similar(embedding) = cos ≈ 0.97 ≥ 0.9",
    );
  });

  it("echoes explanation weight when either vector is missing", () => {
    const model = buildWhyModel({
      edge: {
        etype: "SIM",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "similar",
          edge_type: "SIM",
          src_key: "a",
          dst_key: "b",
          weight: 0.97,
          predicate: pred({
            kind: "vector_similar",
            fields: ["embedding"],
            min: 0.9,
          }),
        }),
      },
      src: { key: "a", label: "Doc", props: { embedding: [1, 0] } },
      dst: { key: "b", label: "Doc", props: { name: "b" } },
    });
    expect(model.kind).toBe("vector_similar");
    if (model.kind !== "vector_similar") {
      return;
    }
    expect(model.line).toBe(
      "vector_similar(embedding) = cos ≈ 0.97 ≥ 0.9",
    );
  });

  it("notes when recomputed cosine disagrees with the server weight", () => {
    const model = buildWhyModel({
      edge: {
        etype: "SIM",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "similar",
          edge_type: "SIM",
          src_key: "a",
          dst_key: "b",
          weight: 0.5,
          predicate: pred({
            kind: "vector_similar",
            fields: ["embedding"],
            min: 0.9,
          }),
        }),
      },
      src: { key: "a", label: "Doc", props: { embedding: [1, 0] } },
      dst: { key: "b", label: "Doc", props: { embedding: [1, 0] } },
    });
    expect(model.kind).toBe("vector_similar");
    if (model.kind !== "vector_similar") {
      return;
    }
    expect(model.line).toBe(
      `vector_similar(embedding) = cos = 1 ≥ 0.9 · d=2 — ${RECOMPUTED_NOTE}`,
    );
  });

  it("stacks all parts and adds a min-score disagreement note", () => {
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "both",
          edge_type: "NEAR",
          src_key: "a",
          dst_key: "b",
          weight: 1,
          predicate: pred({
            kind: "all",
            fields: ["founded_year", "embedding"],
            parts: [
              pred({
                kind: "numeric_within",
                fields: ["founded_year"],
                tolerance: 2,
              }),
              pred({
                kind: "vector_similar",
                fields: ["embedding"],
                min: 0.9,
              }),
            ],
          }),
        }),
      },
      src: {
        key: "a",
        label: "Doc",
        props: { founded_year: 1998, embedding: [1, 0] },
      },
      dst: {
        key: "b",
        label: "Doc",
        props: { founded_year: 2000, embedding: [1, 0] },
      },
    });
    expect(model.kind).toBe("all");
    if (model.kind !== "all") {
      return;
    }
    expect(model.lines).toEqual([
      "numeric_within(founded_year) = |1998 − 2000| = 2 ≤ 2",
      "vector_similar(embedding) = cos = 1 ≥ 0.9 · d=2",
      `min = 0 — ${RECOMPUTED_NOTE}`,
    ]);
  });

  it("renders an honest large-vector line for a dims>64 part inside all", () => {
    const big = Array.from({ length: 65 }, () => 1);
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "both",
          edge_type: "NEAR",
          src_key: "a",
          dst_key: "b",
          weight: 0,
          predicate: pred({
            kind: "all",
            fields: ["founded_year", "embedding"],
            parts: [
              pred({
                kind: "numeric_within",
                fields: ["founded_year"],
                tolerance: 2,
              }),
              pred({
                kind: "vector_similar",
                fields: ["embedding"],
                min: 0.9,
              }),
            ],
          }),
        }),
      },
      src: {
        key: "a",
        label: "Doc",
        props: { founded_year: 1998, embedding: big },
      },
      dst: {
        key: "b",
        label: "Doc",
        props: { founded_year: 2000, embedding: big },
      },
    });
    expect(model.kind).toBe("all");
    if (model.kind !== "all") {
      return;
    }
    expect(model.lines).toEqual([
      "numeric_within(founded_year) = |1998 − 2000| = 2 ≤ 2",
      "vector_similar(embedding) — cos not recomputed (large vector)",
    ]);
  });

  it("prefers the wire kind over inference when skills also overlap", () => {
    const model = buildWhyModel({
      edge: {
        etype: "NEAR",
        src: "a",
        dst: "b",
        derived: true,
        explanation: exp({
          rule: "close_year",
          edge_type: "NEAR",
          src_key: "a",
          dst_key: "b",
          weight: 0,
          predicate: pred({
            kind: "numeric_within",
            fields: ["founded_year"],
            tolerance: 2,
          }),
        }),
      },
      src: {
        key: "a",
        label: "Org",
        props: { founded_year: 1998, skills: ["s01"] },
      },
      dst: {
        key: "b",
        label: "Org",
        props: { founded_year: 2000, skills: ["s01"] },
      },
    });
    expect(model.kind).toBe("numeric_within");
  });
});
