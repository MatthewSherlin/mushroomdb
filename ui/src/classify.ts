/**
 * Edge attribution for the explorer.
 *
 * Primary path: `GET /node/{key}/edges` → {@link classifyFromEdges}. User
 * edges keep their real `edge_type`; `derived` comes from the wire. `/explain`
 * is not used for classification (why-panel arithmetic only).
 *
 * Legacy path (pre-sweep servers): {@link classifyBothDirs} still composes
 * neighborhood + `/explain`. Neighbors with no explanation become
 * {@link USER_ETYPE} (`related`). Keep that synthesis only for the
 * absent-endpoint fallback.
 */
import type { EdgeInfo, Explanation, QueryResult } from "./api";
import { edgeId } from "./store";

/** Legacy-only bucket for unnamed user edges on pre-sweep servers. */
export const USER_ETYPE = "related";

export const EXPLAIN_CONCURRENCY = 8;

export type EdgeProvenance = {
  id: string;
  derived: boolean;
  explanation: Explanation | null;
};

export type ClassifiedEdges = {
  out: Record<string, string[]>;
  in: Record<string, string[]>;
  provenance: EdgeProvenance[];
};

export function neighborKeys(result: QueryResult, root?: string): string[] {
  const keyIdx = result.columns.indexOf("key");
  if (keyIdx < 0) {
    return [];
  }
  const keys: string[] = [];
  for (const row of result.rows) {
    const key = row[keyIdx];
    if (typeof key === "string" && key !== "" && key !== root) {
      keys.push(key);
    }
  }
  return keys;
}

export function hopKeysAtDepth(result: QueryResult, depth: number): string[] {
  const keyIdx = result.columns.indexOf("key");
  const depthIdx = result.columns.indexOf("depth");
  if (keyIdx < 0 || depthIdx < 0) {
    return [];
  }
  const keys: string[] = [];
  for (const row of result.rows) {
    const key = row[keyIdx];
    if (typeof key !== "string" || key === "") {
      continue;
    }
    if (row[depthIdx] === depth) {
      keys.push(key);
    }
  }
  return keys;
}

export function firstNodeKey(result: QueryResult): string | undefined {
  for (const row of result.rows) {
    for (const cell of row) {
      if (typeof cell === "string" && cell !== "") {
        return cell;
      }
    }
  }
  return undefined;
}

export function concatResults(...results: QueryResult[]): QueryResult {
  const columns = results[0]?.columns ?? [];
  for (let i = 1; i < results.length; i++) {
    const next = results[i]!.columns;
    if (next.length !== columns.length || next.some((c, j) => c !== columns[j])) {
      throw new Error(
        `concatResults: column mismatch (${columns.join(",")} vs ${next.join(",")})`,
      );
    }
  }
  return { columns, rows: results.flatMap((r) => r.rows) };
}

export type MapPoolError<T> = {
  index: number;
  item: T;
  error: unknown;
};

/**
 * Bounded concurrent map. Each item is independently try/caught: one
 * rejection does not abort queued items or sibling workers.
 *
 * Failure surface: `{ results, errors }`.
 * - `results[i]` is the fulfilled value, or `undefined` if item `i` rejected
 *   (also `undefined` when `R` is `void` — inspect `errors` for those).
 * - `errors` is `{ index, item, error }[]` sorted by index.
 */
export type MapPoolResult<T, R> = {
  results: Array<R | undefined>;
  errors: MapPoolError<T>[];
};

export async function mapPool<T, R>(
  items: readonly T[],
  limit: number,
  fn: (item: T) => Promise<R>,
): Promise<MapPoolResult<T, R>> {
  const results = new Array<R | undefined>(items.length);
  const errors: MapPoolError<T>[] = [];
  let next = 0;
  const worker = async (): Promise<void> => {
    for (;;) {
      const i = next;
      next += 1;
      if (i >= items.length) {
        return;
      }
      try {
        results[i] = await fn(items[i] as T);
      } catch (error: unknown) {
        errors.push({ index: i, item: items[i] as T, error });
      }
    }
  };
  const n = Math.min(Math.max(limit, 1), items.length);
  await Promise.all(Array.from({ length: n }, () => worker()));
  errors.sort((a, b) => a.index - b.index);
  return { results, errors };
}

export async function explainNeighbors(
  explain: (a: string, b: string) => Promise<Explanation[]>,
  root: string,
  neighbors: readonly string[],
): Promise<Map<string, Explanation[]>> {
  const unique: string[] = [];
  const seen = new Set<string>();
  for (const key of neighbors) {
    if (key === "" || key === root || seen.has(key)) {
      continue;
    }
    seen.add(key);
    unique.push(key);
  }
  // /explain is bidirectional: the server matches
  // (src==a && dst==b) || (src==b && dst==a) at crates/core-api/src/db.rs:900.
  // explain(root, n) therefore returns in-edges {src_key:n, dst_key:root} as
  // well as out-edges. classifyDir does not flip or rewrite direction.
  const { results } = await mapPool(unique, EXPLAIN_CONCURRENCY, (n) =>
    explain(root, n),
  );
  const map = new Map<string, Explanation[]>();
  for (let i = 0; i < unique.length; i++) {
    map.set(unique[i]!, results[i] ?? []);
  }
  return map;
}

export function classifyBothDirs(args: {
  root: string;
  outKeys: readonly string[];
  inKeys: readonly string[];
  explanations: ReadonlyMap<string, readonly Explanation[]>;
}): ClassifiedEdges {
  const out = classifyDir(args.root, args.outKeys, args.explanations, "out");
  const inn = classifyDir(args.root, args.inKeys, args.explanations, "in");
  const provenance = [...out.provenance, ...inn.provenance];
  provenance.sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  return { out: out.perEtype, in: inn.perEtype, provenance };
}

export function classifyFromEdges(
  root: string,
  edges: readonly EdgeInfo[],
): ClassifiedEdges {
  const out: Record<string, string[]> = {};
  const inn: Record<string, string[]> = {};
  const provenance: EdgeProvenance[] = [];
  const seen = new Set<string>();
  for (const e of edges) {
    if (e.src_key !== root && e.dst_key !== root) {
      continue;
    }
    const id = edgeId(e.edge_type, e.src_key, e.dst_key);
    if (seen.has(id)) {
      continue;
    }
    seen.add(id);
    if (e.src_key === root) {
      addNeighbor(out, e.edge_type, e.dst_key);
    }
    if (e.dst_key === root && e.src_key !== root) {
      addNeighbor(inn, e.edge_type, e.src_key);
    }
    provenance.push({
      id,
      derived: e.derived,
      explanation: null,
    });
  }
  provenance.sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  return { out, in: inn, provenance };
}

function classifyDir(
  root: string,
  keys: readonly string[],
  explanations: ReadonlyMap<string, readonly Explanation[]>,
  dir: "in" | "out",
): { perEtype: Record<string, string[]>; provenance: EdgeProvenance[] } {
  const perEtype: Record<string, string[]> = {};
  const provenance: EdgeProvenance[] = [];
  for (const nbr of keys) {
    if (nbr === "" || nbr === root) {
      continue;
    }
    const matching = (explanations.get(nbr) ?? []).filter((e) =>
      dir === "out"
        ? e.src_key === root && e.dst_key === nbr
        : e.src_key === nbr && e.dst_key === root,
    );
    if (matching.length > 0) {
      for (const e of matching) {
        addNeighbor(perEtype, e.edge_type, nbr);
        provenance.push({
          id: edgeId(e.edge_type, e.src_key, e.dst_key),
          derived: true,
          explanation: e,
        });
      }
      continue;
    }
    const src = dir === "out" ? root : nbr;
    const dst = dir === "out" ? nbr : root;
    addNeighbor(perEtype, USER_ETYPE, nbr);
    provenance.push({
      id: edgeId(USER_ETYPE, src, dst),
      derived: false,
      explanation: null,
    });
  }
  return { perEtype, provenance };
}

function addNeighbor(
  perEtype: Record<string, string[]>,
  etype: string,
  nbr: string,
): void {
  const list = perEtype[etype];
  if (list === undefined) {
    perEtype[etype] = [nbr];
    return;
  }
  if (!list.includes(nbr)) {
    list.push(nbr);
  }
}
