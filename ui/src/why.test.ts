import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import type { Explanation, QueryResult } from "./api";
import { GraphStore, edgeId } from "./store";
import {
  HAND_LINE,
  buildWhyModel,
  ensureProvenance,
  formatKeyMatchLine,
  formatOverlapLine,
  formatScore,
  highlightedIdsForRule,
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
    ...over,
  };
}

describe("module contract", () => {
  it("is a pure module: no DOM, canvas, or cosmos imports", () => {
    expect(src).not.toMatch(
      /from\s+["']@cosmos\.gl|document\.|window\.|HTMLCanvas|getContext\(/,
    );
  });

  it("documents the exact-key MATCH prop fetch (no node-props endpoint)", () => {
    expect(src).toMatch(/MATCH \(n \{id: \$key\}\)/);
    expect(src).toMatch(/no node-props endpoint/i);
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
