/**
 * Why-panel view-model and provenance helpers.
 *
 * There is no node-props endpoint. Props are fetched with an exact-key
 * MATCH on the ingest default key field (`id`):
 *   MATCH (n {id: $key}) RETURN n.skills AS skills, …
 * Cached on the store via `setNodeProps`. Reused when already present.
 *
 * `/explain` carries `Explanation.predicate`. `buildWhyModel` dispatches
 * on `predicate.kind` when present. The prop-comparison inference chain
 * is the fallback for old servers (predicate null) and summaries the
 * panel cannot render.
 *
 * Props fetch projects `summary.fields` (first-seen union for `all`).
 * `PROP_FIELDS` is the fallback list only.
 */
import type {
  Explanation,
  JsonCell,
  PredicateKind,
  PredicateSummary,
  QueryParam,
  QueryResult,
} from "./api";
import { EXPLAIN_CONCURRENCY, mapPool } from "./classify";
import { edgeId, type GraphEdge, type GraphNode, type GraphStore } from "./store";

export const HAND_LINE = "Created by hand";

export const RECOMPUTED_NOTE = "recomputed from current props";

export const THRESHOLD_NOTE = `${RECOMPUTED_NOTE} — no longer within threshold`;

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
    }
  | {
      kind: "numeric_within";
      rule: string;
      etype: string;
      weight: number | null;
      field: string;
      line: string;
      srcKey: string;
      dstKey: string;
    }
  | {
      kind: "geo_radius";
      rule: string;
      etype: string;
      weight: number | null;
      field: string;
      srcKey: string;
      dstKey: string;
      line: string;
    }
  | {
      kind: "vector_similar";
      rule: string;
      etype: string;
      weight: number | null;
      field: string;
      line: string;
      srcKey: string;
      dstKey: string;
    }
  | {
      kind: "all";
      rule: string;
      etype: string;
      weight: number | null;
      lines: string[];
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

export function fieldsFromPredicate(
  predicate: PredicateSummary | null | undefined,
): readonly string[] | undefined {
  if (predicate == null) {
    return undefined;
  }
  if (predicate.fields.length > 0) {
    return predicate.fields;
  }
  if (predicate.parts === null) {
    return undefined;
  }
  const out: string[] = [];
  for (const part of predicate.parts) {
    const inner = fieldsFromPredicate(part);
    if (inner === undefined) {
      continue;
    }
    for (const field of inner) {
      if (!out.includes(field)) {
        out.push(field);
      }
    }
  }
  return out.length > 0 ? out : undefined;
}

export async function loadNodeProps(
  store: GraphStore,
  api: PropsApi,
  key: string,
  fields?: readonly string[],
): Promise<Record<string, unknown>> {
  const node = store.nodes.get(key);
  const requested = fields !== undefined && fields.length > 0 ? fields : undefined;
  if (
    node?.props !== undefined &&
    (requested === undefined || requested.every((f) => f in node.props!))
  ) {
    return node.props;
  }
  const { cypher, params } = nodePropsQuery(key, requested);
  const result = await api.query(cypher, params);
  const fetched = propsFromResult(result);
  const props = { ...(node?.props ?? {}), ...fetched };
  store.setNodeProps(key, props);
  return props;
}

export function nodePropsQuery(
  key: string,
  fields?: readonly string[],
): {
  cypher: string;
  params: Record<string, QueryParam>;
} {
  const list = fields !== undefined && fields.length > 0 ? fields : PROP_FIELDS;
  const ret = list.map((f) => `n.${f} AS ${f}`).join(", ");
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
  const summary = explanation.predicate;
  if (summary != null) {
    const fromWire = modelFromSummary(
      edge,
      src,
      dst,
      explanation,
      summary,
      srcProps,
      dstProps,
    );
    if (fromWire !== undefined) {
      return fromWire;
    }
    if (isKnownKind(summary.kind)) {
      return derivedModel(edge, explanation, src, dst);
    }
  }
  // Fallback: old servers omit predicate; unknown summaries fall through.
  // Scored explanations (overlap) carry a weight. Key-match / auto-FK
  // explanations have weight null — prefer the FK field so a coincidental
  // token overlap (Person.skills ∩ Org.skills) is not invented as arithmetic.
  return inferWhyModel(edge, src, dst, explanation, srcProps, dstProps);
}

function inferWhyModel(
  edge: GraphEdge,
  src: GraphNode,
  dst: GraphNode,
  explanation: Explanation,
  srcProps: Record<string, unknown>,
  dstProps: Record<string, unknown>,
): WhyModel {
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
  return derivedModel(edge, explanation, src, dst);
}

function derivedModel(
  edge: GraphEdge,
  explanation: Explanation,
  src: GraphNode,
  dst: GraphNode,
): Extract<WhyModel, { kind: "derived" }> {
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

const KNOWN_KINDS: ReadonlySet<PredicateKind> = new Set([
  "key_match",
  "field_equal",
  "overlap",
  "all",
  "numeric_within",
  "geo_radius",
  "vector_similar",
]);

const EARTH_RADIUS_KM = 6371.0088;
const VECTOR_MAX_DIM = 64;
const MINUS = "\u2212";

type PartRender = {
  lines: string[];
  score: number | undefined;
};

function isKnownKind(kind: string): kind is PredicateKind {
  return KNOWN_KINDS.has(kind as PredicateKind);
}

function modelFromSummary(
  edge: GraphEdge,
  src: GraphNode,
  dst: GraphNode,
  explanation: Explanation,
  summary: PredicateSummary,
  srcProps: Record<string, unknown>,
  dstProps: Record<string, unknown>,
): WhyModel | undefined {
  switch (summary.kind) {
    case "key_match": {
      const hit = keyMatchFromField(summary.fields[0], src, dst);
      if (hit === undefined) {
        return undefined;
      }
      return {
        kind: "key_match",
        rule: explanation.rule,
        etype: edge.etype,
        weight: explanation.weight,
        line: formatKeyMatchLine(hit),
        srcKey: src.key,
        dstKey: dst.key,
      };
    }
    case "field_equal": {
      const hit = fieldEqualFromField(summary.fields[0], srcProps, dstProps);
      if (hit === undefined) {
        return undefined;
      }
      return {
        kind: "field_equal",
        rule: explanation.rule,
        etype: edge.etype,
        weight: explanation.weight,
        field: hit.field,
        value: hit.value,
        line: formatFieldEqualLine(hit),
        srcKey: src.key,
        dstKey: dst.key,
      };
    }
    case "overlap": {
      const field = summary.fields[0];
      if (field === undefined) {
        return undefined;
      }
      const sets = overlapFromProps(
        { [field]: srcProps[field] },
        { [field]: dstProps[field] },
        explanation.weight,
      );
      if (sets === undefined) {
        return undefined;
      }
      const shared = new Set(sets.shared);
      return {
        kind: "overlap",
        rule: explanation.rule,
        etype: edge.etype,
        weight: explanation.weight,
        field: sets.field,
        line: formatOverlapLine(sets, explanation.weight),
        srcTokens: markTokens(sets.src, shared),
        dstTokens: markTokens(sets.dst, shared),
        srcKey: src.key,
        dstKey: dst.key,
      };
    }
    case "numeric_within": {
      const rendered = numericRender(summary, srcProps, dstProps, explanation.weight);
      const field = summary.fields[0];
      if (rendered === undefined || field === undefined) {
        return undefined;
      }
      return {
        kind: "numeric_within",
        rule: explanation.rule,
        etype: edge.etype,
        weight: explanation.weight,
        field,
        line: rendered.lines[0]!,
        srcKey: src.key,
        dstKey: dst.key,
      };
    }
    case "geo_radius": {
      const rendered = geoRender(summary, srcProps, dstProps, explanation.weight);
      const field = summary.fields[0];
      if (rendered === undefined || field === undefined) {
        return undefined;
      }
      return {
        kind: "geo_radius",
        rule: explanation.rule,
        etype: edge.etype,
        weight: explanation.weight,
        field,
        line: rendered.lines[0]!,
        srcKey: src.key,
        dstKey: dst.key,
      };
    }
    case "vector_similar": {
      const rendered = vectorRender(
        summary,
        srcProps,
        dstProps,
        explanation.weight,
        explanation.weight,
      );
      const field = summary.fields[0];
      if (rendered === undefined || field === undefined) {
        return undefined;
      }
      return {
        kind: "vector_similar",
        rule: explanation.rule,
        etype: edge.etype,
        weight: explanation.weight,
        field,
        line: rendered.lines[0]!,
        srcKey: src.key,
        dstKey: dst.key,
      };
    }
    case "all": {
      const rendered = partLines(summary, src, dst, srcProps, dstProps);
      if (rendered === undefined) {
        return undefined;
      }
      const lines = [...rendered.lines];
      if (
        rendered.score !== undefined &&
        explanation.weight !== null &&
        Math.abs(rendered.score - explanation.weight) > WEIGHT_EPS
      ) {
        lines.push(`min = ${formatScore(rendered.score)} — ${RECOMPUTED_NOTE}`);
      }
      return {
        kind: "all",
        rule: explanation.rule,
        etype: edge.etype,
        weight: explanation.weight,
        lines,
        srcKey: src.key,
        dstKey: dst.key,
      };
    }
  }
}

function partLines(
  summary: PredicateSummary,
  src: GraphNode,
  dst: GraphNode,
  srcProps: Record<string, unknown>,
  dstProps: Record<string, unknown>,
): PartRender | undefined {
  switch (summary.kind) {
    case "all": {
      if (summary.parts === null || summary.parts.length === 0) {
        return undefined;
      }
      const lines: string[] = [];
      const scores: number[] = [];
      for (const part of summary.parts) {
        const inner = partLines(part, src, dst, srcProps, dstProps);
        if (inner === undefined) {
          continue;
        }
        lines.push(...inner.lines);
        if (inner.score !== undefined) {
          scores.push(inner.score);
        }
      }
      if (lines.length === 0) {
        return undefined;
      }
      const score =
        scores.length === summary.parts.length
          ? Math.min(...scores)
          : undefined;
      return { lines, score };
    }
    case "numeric_within":
      return numericRender(summary, srcProps, dstProps, null);
    case "geo_radius":
      return geoRender(summary, srcProps, dstProps, null);
    case "vector_similar":
      return vectorRender(summary, srcProps, dstProps, null, null);
    case "overlap": {
      const field = summary.fields[0];
      if (field === undefined) {
        return undefined;
      }
      const sets = overlapFromProps(
        { [field]: srcProps[field] },
        { [field]: dstProps[field] },
      );
      if (sets === undefined) {
        return undefined;
      }
      return { lines: [formatOverlapLine(sets)], score: sets.score };
    }
    case "key_match": {
      const hit = keyMatchFromField(summary.fields[0], src, dst);
      if (hit === undefined) {
        return undefined;
      }
      return { lines: [formatKeyMatchLine(hit)], score: 1 };
    }
    case "field_equal": {
      const hit = fieldEqualFromField(summary.fields[0], srcProps, dstProps);
      if (hit === undefined) {
        return undefined;
      }
      return { lines: [formatFieldEqualLine(hit)], score: 1 };
    }
  }
}

function numericRender(
  summary: PredicateSummary,
  srcProps: Record<string, unknown>,
  dstProps: Record<string, unknown>,
  serverWeight: number | null,
): PartRender | undefined {
  const field = summary.fields[0];
  const tolerance = summary.tolerance;
  if (
    field === undefined ||
    tolerance === null ||
    !Number.isFinite(tolerance) ||
    tolerance < 0
  ) {
    return undefined;
  }
  const a = asFiniteF64(srcProps[field]);
  const b = asFiniteF64(dstProps[field]);
  if (a === undefined || b === undefined) {
    return undefined;
  }
  const delta = Math.abs(a - b);
  const within = tolerance === 0 ? delta === 0 : delta <= tolerance;
  const op = within ? "≤" : ">";
  const line = `numeric_within(${field}) = |${displayNum(a)} ${MINUS} ${displayNum(b)}| = ${displayNum(delta)} ${op} ${displayNum(tolerance)}`;
  if (!within) {
    return { lines: [`${line} — ${THRESHOLD_NOTE}`], score: undefined };
  }
  const score = tolerance === 0 ? 1 : 1 - delta / tolerance;
  return { lines: [withNote(line, score, serverWeight)], score };
}

function geoRender(
  summary: PredicateSummary,
  srcProps: Record<string, unknown>,
  dstProps: Record<string, unknown>,
  serverWeight: number | null,
): PartRender | undefined {
  const field = summary.fields[0];
  const km = summary.km;
  if (field === undefined || km === null || !Number.isFinite(km) || km <= 0) {
    return undefined;
  }
  const a = asLatLon(srcProps[field]);
  const b = asLatLon(dstProps[field]);
  if (a === undefined || b === undefined) {
    return undefined;
  }
  const d = haversineKm(a[0], a[1], b[0], b[1]);
  if (!Number.isFinite(d)) {
    return undefined;
  }
  const within = d <= km;
  const op = within ? "≤" : ">";
  const line = `geo_radius(${field}) = ${d.toFixed(1)} km ${op} ${displayNum(km)} km`;
  if (!within) {
    return { lines: [`${line} — ${THRESHOLD_NOTE}`], score: undefined };
  }
  return { lines: [withNote(line, 1 - d / km, serverWeight)], score: 1 - d / km };
}

function vectorRender(
  summary: PredicateSummary,
  srcProps: Record<string, unknown>,
  dstProps: Record<string, unknown>,
  serverWeight: number | null,
  echoWeight: number | null,
): PartRender | undefined {
  const field = summary.fields[0];
  if (field === undefined) {
    return undefined;
  }
  const min = summary.min;
  const minClause = min !== null ? ` ≥ ${formatScore(min)}` : "";
  const a = asNumericList(srcProps[field]);
  const b = asNumericList(dstProps[field]);
  const canRecompute =
    a !== undefined &&
    b !== undefined &&
    a.length === b.length &&
    a.length > 0 &&
    a.length <= VECTOR_MAX_DIM;
  if (canRecompute) {
    const cos = cosine(a, b);
    if (cos !== undefined) {
      const line = `vector_similar(${field}) = cos = ${formatScore(cos)}${minClause} · d=${a.length}`;
      return { lines: [withNote(line, cos, serverWeight)], score: cos };
    }
  }
  if (echoWeight !== null) {
    return {
      lines: [`vector_similar(${field}) = cos ≈ ${formatScore(echoWeight)}${minClause}`],
      score: undefined,
    };
  }
  const large =
    (a !== undefined && a.length > VECTOR_MAX_DIM) ||
    (b !== undefined && b.length > VECTOR_MAX_DIM);
  if (large) {
    return {
      lines: [`vector_similar(${field}) — cos not recomputed (large vector)`],
      score: undefined,
    };
  }
  return undefined;
}

function keyMatchFromField(
  field: string | undefined,
  src: GraphNode,
  dst: GraphNode,
): KeyMatchHit | undefined {
  if (field === undefined) {
    return undefined;
  }
  if ((src.props ?? {})[field] === dst.key) {
    return { label: src.label, field, value: dst.key };
  }
  if ((dst.props ?? {})[field] === src.key) {
    return { label: dst.label, field, value: src.key };
  }
  return undefined;
}

function fieldEqualFromField(
  field: string | undefined,
  srcProps: Record<string, unknown>,
  dstProps: Record<string, unknown>,
): FieldEqualHit | undefined {
  if (field === undefined) {
    return undefined;
  }
  const value = scalarText(srcProps[field]);
  if (value === undefined || value !== scalarText(dstProps[field])) {
    return undefined;
  }
  return { field, value };
}

function withNote(
  line: string,
  score: number | undefined,
  serverWeight: number | null,
): string {
  if (
    serverWeight !== null &&
    score !== undefined &&
    Math.abs(score - serverWeight) > WEIGHT_EPS
  ) {
    return `${line} — ${RECOMPUTED_NOTE}`;
  }
  return line;
}

function displayNum(n: number): string {
  if (Number.isInteger(n)) {
    return String(n);
  }
  return formatScore(n);
}

function asFiniteF64(value: unknown): number | undefined {
  if (typeof value === "number" && Number.isFinite(value)) {
    return value;
  }
  return undefined;
}

function asLatLon(value: unknown): [number, number] | undefined {
  if (!Array.isArray(value) || value.length !== 2) {
    return undefined;
  }
  const lat = asFiniteF64(value[0]);
  const lon = asFiniteF64(value[1]);
  if (lat === undefined || lon === undefined) {
    return undefined;
  }
  if (lat < -90 || lat > 90 || lon < -180 || lon > 180) {
    return undefined;
  }
  return [lat, lon];
}

function asNumericList(value: unknown): number[] | undefined {
  if (!Array.isArray(value) || value.length === 0) {
    return undefined;
  }
  const out: number[] = [];
  for (const item of value) {
    const n = asFiniteF64(item);
    if (n === undefined) {
      return undefined;
    }
    out.push(n);
  }
  return out;
}

function haversineKm(
  lat1: number,
  lon1: number,
  lat2: number,
  lon2: number,
): number {
  const phi1 = toRad(lat1);
  const phi2 = toRad(lat2);
  const dphi = toRad(lat2 - lat1);
  const dlam = toRad(lon2 - lon1);
  const a = Math.min(
    1,
    Math.max(
      0,
      Math.sin(dphi / 2) ** 2 +
        Math.cos(phi1) * Math.cos(phi2) * Math.sin(dlam / 2) ** 2,
    ),
  );
  const c = 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a));
  return EARTH_RADIUS_KM * c;
}

function cosine(a: number[], b: number[]): number | undefined {
  let dot = 0;
  let na2 = 0;
  let nb2 = 0;
  for (let i = 0; i < a.length; i++) {
    const x = a[i]!;
    const y = b[i]!;
    dot += x * y;
    na2 += x * x;
    nb2 += y * y;
  }
  const na = Math.sqrt(na2);
  const nb = Math.sqrt(nb2);
  if (!(na > 0 && nb > 0)) {
    return undefined;
  }
  const cos = dot / (na * nb);
  return Number.isFinite(cos) ? Math.min(cos, 1) : undefined;
}

function toRad(deg: number): number {
  return (deg * Math.PI) / 180;
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
