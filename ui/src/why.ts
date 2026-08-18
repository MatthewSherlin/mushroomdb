/**
 * Why-panel view-model and provenance helpers.
 *
 * There is no node-props endpoint. Props are fetched with an exact-key
 * MATCH on the ingest default key field (`id`):
 *   MATCH (n {id: $key}) RETURN n.skills AS skills, …
 * Cached on the store via `setNodeProps`. Reused when already present.
 *
 * Predicate type is not on the wire (`Explanation` has rule/etype/weight
 * only; `RuleStats` has no edge_type). Arithmetic is reconstructed from
 * the two nodes' actual props + the explanation weight — not from names.
 *
 * FieldEqual is derived the same way overlap learns its field: scan both
 * nodes' props. A shared scalar field with equal values (not a token list,
 * not an FK equal to the other key) is FieldEqual. There is no predicate
 * kind/field on `/stats` or `/explain`.
 */
import type { Explanation, JsonCell, QueryParam, QueryResult } from "./api";
import { EXPLAIN_CONCURRENCY, mapPool } from "./classify";
import { edgeId, type GraphEdge, type GraphNode, type GraphStore } from "./store";

export const HAND_LINE = "Created by hand";

export const RECOMPUTED_NOTE = "recomputed from current props";

const PROP_FIELDS = ["skills", "org_id", "project_id", "name", "tags"] as const;

const WEIGHT_EPS = 1e-9;

export type TokenMark = {
  token: string;
  shared: boolean;
};

export type OverlapSets = {
  field: string;
  src: string[];
  dst: string[];
  shared: string[];
  union: string[];
  score: number;
};

export type KeyMatchHit = {
  label: string;
  field: string;
  value: string;
};

export type WhyModel =
  | {
      kind: "hand";
      etype: string;
      src: string;
      dst: string;
      line: string;
    }
  | {
      kind: "overlap";
      rule: string;
      etype: string;
      weight: number | null;
      field: string;
      line: string;
      srcTokens: TokenMark[];
      dstTokens: TokenMark[];
      srcKey: string;
      dstKey: string;
    }
  | {
      kind: "key_match";
      rule: string;
      etype: string;
      weight: number | null;
      line: string;
      srcKey: string;
      dstKey: string;
    }
  | {
      kind: "field_equal";
      rule: string;
      etype: string;
      weight: number | null;
      field: string;
      value: string;
      line: string;
      srcKey: string;
      dstKey: string;
    }
  | {
      kind: "derived";
      rule: string;
      etype: string;
      weight: number | null;
      line: string;
      srcKey: string;
      dstKey: string;
    };

export type ExplainApi = {
  explain(a: string, b: string): Promise<Explanation[]>;
};

export type PropsApi = {
  query(
    cypher: string,
    params?: Record<string, QueryParam>,
  ): Promise<QueryResult>;
};

export async function loadNodeProps(
  store: GraphStore,
  api: PropsApi,
  key: string,
): Promise<Record<string, unknown>> {
  const node = store.nodes.get(key);
  if (node?.props !== undefined) {
    return node.props;
  }
  const { cypher, params } = nodePropsQuery(key);
  const result = await api.query(cypher, params);
  const props = propsFromResult(result);
  store.setNodeProps(key, props);
  return props;
}

export function nodePropsQuery(key: string): {
  cypher: string;
  params: Record<string, QueryParam>;
} {
  const ret = PROP_FIELDS.map((f) => `n.${f} AS ${f}`).join(", ");
  return {
    cypher: `MATCH (n {id: $key}) RETURN ${ret}`,
    params: { key },
  };
}

export function propsFromResult(result: QueryResult): Record<string, unknown> {
  const row = result.rows[0];
  if (row === undefined) {
    return {};
  }
  const props: Record<string, unknown> = {};
  for (let i = 0; i < result.columns.length; i++) {
    const col = result.columns[i];
    const cell = row[i];
    if (col === undefined || cell === null || cell === undefined) {
      continue;
    }
    props[col] = cellFromJson(cell);
  }
  return props;
}

export function formatScore(n: number): string {
  if (Number.isInteger(n)) {
    return String(n);
  }
  return n.toFixed(3).replace(/\.?0+$/, "");
}

export function markTokens(
  tokens: readonly string[],
  shared: ReadonlySet<string>,
): TokenMark[] {
  return tokens.map((token) => ({ token, shared: shared.has(token) }));
}

export function overlapFromProps(
  srcProps: Record<string, unknown>,
  dstProps: Record<string, unknown>,
  weight?: number | null,
): OverlapSets | undefined {
  const fields = new Set([...Object.keys(srcProps), ...Object.keys(dstProps)]);
  const candidates: OverlapSets[] = [];
  for (const field of [...fields].sort()) {
    const src = tokenList(srcProps[field]);
    const dst = tokenList(dstProps[field]);
    if (src === undefined || dst === undefined) {
      continue;
    }
    const srcSet = new Set(src);
    const dstSet = new Set(dst);
    const shared = [...srcSet].filter((t) => dstSet.has(t)).sort();
    const union = [...new Set([...src, ...dst])].sort();
    if (union.length === 0 || shared.length === 0) {
      continue;
    }
    const score = shared.length / union.length;
    candidates.push({
      field,
      src: [...src].sort(),
      dst: [...dst].sort(),
      shared,
      union,
      score,
    });
  }
  if (candidates.length === 0) {
    return undefined;
  }
  if (weight !== undefined && weight !== null) {
    let best = candidates[0]!;
    let bestDelta = Math.abs(best.score - weight);
    for (const c of candidates.slice(1)) {
      const delta = Math.abs(c.score - weight);
      if (delta < bestDelta) {
        best = c;
        bestDelta = delta;
      }
    }
    if (bestDelta <= WEIGHT_EPS || bestDelta < 0.05) {
      return best;
    }
  }
  return candidates.reduce((a, b) => (a.score >= b.score ? a : b));
}

export function formatOverlapLine(
  sets: OverlapSets,
  serverWeight?: number | null,
): string {
  const base = `overlap(${sets.field}) = |{${sets.shared.join(", ")}}| / |{${sets.union.join(", ")}}| = ${formatScore(sets.score)}`;
  if (
    serverWeight !== undefined &&
    serverWeight !== null &&
    Math.abs(sets.score - serverWeight) > WEIGHT_EPS
  ) {
    return `${base} — ${RECOMPUTED_NOTE}`;
  }
  return base;
}

export function keyMatchFromProps(
  src: GraphNode,
  dst: GraphNode,
): KeyMatchHit | undefined {
  const srcHit = fkField(src.props, dst.key);
  if (srcHit !== undefined) {
    return { label: src.label, field: srcHit, value: dst.key };
  }
  const dstHit = fkField(dst.props, src.key);
  if (dstHit !== undefined) {
    return { label: dst.label, field: dstHit, value: src.key };
  }
  return undefined;
}

export function formatKeyMatchLine(hit: KeyMatchHit): string {
  const head = hit.label !== "" ? `${hit.label.toLowerCase()}.` : "";
  return `${head}${hit.field} = "${hit.value}" → ${hit.value}`;
}

export type FieldEqualHit = {
  field: string;
  value: string;
};

export function fieldEqualFromProps(
  srcProps: Record<string, unknown>,
  dstProps: Record<string, unknown>,
): FieldEqualHit | undefined {
  for (const field of Object.keys(srcProps).sort()) {
    if (!(field in dstProps)) {
      continue;
    }
    if (field === "name" || field === "id") {
      continue;
    }
    if (tokenList(srcProps[field]) !== undefined || tokenList(dstProps[field]) !== undefined) {
      continue;
    }
    const value = scalarText(srcProps[field]);
    if (value === undefined || value !== scalarText(dstProps[field])) {
      continue;
    }
    return { field, value };
  }
  return undefined;
}

export function formatFieldEqualLine(hit: FieldEqualHit): string {
  return `field_equal(${hit.field}): "${hit.value}" = "${hit.value}"`;
}

export function whyEdgeMissing(
  store: GraphStore,
  edgeIdValue: string | undefined,
): boolean {
  return edgeIdValue !== undefined && !store.edges.has(edgeIdValue);
}

export function buildWhyModel(args: {
  edge: GraphEdge;
  src: GraphNode;
  dst: GraphNode;
}): WhyModel {
  const { edge, src, dst } = args;
  if (edge.derived !== true || edge.explanation === undefined) {
    return {
      kind: "hand",
      etype: edge.etype,
      src: src.key,
      dst: dst.key,
      line: HAND_LINE,
    };
  }
  const explanation = edge.explanation;
  const srcProps = src.props ?? {};
  const dstProps = dst.props ?? {};
  // Scored explanations (overlap) carry a weight. Key-match / auto-FK
  // explanations have weight null — prefer the FK field so a coincidental
  // token overlap (Person.skills ∩ Org.skills) is not invented as arithmetic.
  if (explanation.weight !== null) {
    const scored = overlapModel(edge, src, dst, explanation, srcProps, dstProps);
    if (scored !== undefined) {
      return scored;
    }
  }
  const km = keyMatchFromProps(src, dst);
  if (km !== undefined) {
    return {
      kind: "key_match",
      rule: explanation.rule,
      etype: edge.etype,
      weight: explanation.weight,
      line: formatKeyMatchLine(km),
      srcKey: src.key,
      dstKey: dst.key,
    };
  }
  const eq = fieldEqualFromProps(srcProps, dstProps);
  if (eq !== undefined) {
    return {
      kind: "field_equal",
      rule: explanation.rule,
      etype: edge.etype,
      weight: explanation.weight,
      field: eq.field,
      value: eq.value,
      line: formatFieldEqualLine(eq),
      srcKey: src.key,
      dstKey: dst.key,
    };
  }
  const overlap = overlapModel(edge, src, dst, explanation, srcProps, dstProps);
  if (overlap !== undefined) {
    return overlap;
  }
  return {
    kind: "derived",
    rule: explanation.rule,
    etype: edge.etype,
    weight: explanation.weight,
    line: `Derived by rule ${explanation.rule}`,
    srcKey: src.key,
    dstKey: dst.key,
  };
}

function overlapModel(
  edge: GraphEdge,
  src: GraphNode,
  dst: GraphNode,
  explanation: Explanation,
  srcProps: Record<string, unknown>,
  dstProps: Record<string, unknown>,
): Extract<WhyModel, { kind: "overlap" }> | undefined {
  const overlap = overlapFromProps(srcProps, dstProps, explanation.weight);
  if (overlap === undefined) {
    return undefined;
  }
  const shared = new Set(overlap.shared);
  return {
    kind: "overlap",
    rule: explanation.rule,
    etype: edge.etype,
    weight: explanation.weight,
    field: overlap.field,
    line: formatOverlapLine(overlap, explanation.weight),
    srcTokens: markTokens(overlap.src, shared),
    dstTokens: markTokens(overlap.dst, shared),
    srcKey: src.key,
    dstKey: dst.key,
  };
}

export async function ensureProvenance(
  store: GraphStore,
  api: ExplainApi,
): Promise<void> {
  const pending: Array<[string, string]> = [];
  const seen = new Set<string>();
  for (const edge of store.edges.values()) {
    // Intentional short-circuit: classified edges skip re-explain.
    // A new rule that would reclassify the pair is handled by
    // rule_created → markAllDirty → resync → expand, not this guard.
    if (edge.derived !== undefined) {
      continue;
    }
    const a = edge.src < edge.dst ? edge.src : edge.dst;
    const b = edge.src < edge.dst ? edge.dst : edge.src;
    const token = `${a}\0${b}`;
    if (seen.has(token)) {
      continue;
    }
    seen.add(token);
    pending.push([edge.src, edge.dst]);
  }
  if (pending.length === 0) {
    return;
  }
  const { results } = await mapPool(pending, EXPLAIN_CONCURRENCY, ([a, b]) =>
    api.explain(a, b),
  );
  for (let i = 0; i < pending.length; i++) {
    const pair = pending[i]!;
    applyExplanations(store, pair[0], pair[1], results[i] ?? []);
  }
}

export function highlightedIdsForRule(
  store: GraphStore,
  ruleName: string,
): string[] {
  return store.derivedEdges(ruleName).map((e) => edgeId(e.etype, e.src, e.dst));
}

function applyExplanations(
  store: GraphStore,
  a: string,
  b: string,
  explanations: readonly Explanation[],
): void {
  for (const edge of store.edges.values()) {
    const between =
      (edge.src === a && edge.dst === b) || (edge.src === b && edge.dst === a);
    if (!between) {
      continue;
    }
    const match = explanations.find((e) => e.edge_type === edge.etype);
    store.setProvenance(edgeId(edge.etype, edge.src, edge.dst), match ?? null);
  }
}

function tokenList(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) {
    return undefined;
  }
  const tokens: string[] = [];
  for (const item of value) {
    if (typeof item === "string" && item !== "") {
      tokens.push(item);
    } else if (typeof item === "number" && Number.isFinite(item)) {
      tokens.push(String(item));
    } else {
      return undefined;
    }
  }
  return tokens;
}

function fkField(
  props: Record<string, unknown> | undefined,
  target: string,
): string | undefined {
  if (props === undefined) {
    return undefined;
  }
  for (const field of Object.keys(props).sort()) {
    if (props[field] === target) {
      return field;
    }
  }
  return undefined;
}

function scalarText(value: unknown): string | undefined {
  if (typeof value === "string" && value !== "") {
    return value;
  }
  if (typeof value === "number" && Number.isFinite(value)) {
    return String(value);
  }
  if (typeof value === "boolean") {
    return String(value);
  }
  return undefined;
}

function cellFromJson(cell: JsonCell): unknown {
  return cell;
}
