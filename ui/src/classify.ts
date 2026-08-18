/**
 * Edge attribution for the explorer.
 *
 * ## Endpoint composition (v1 — documented for the T3 report)
 *
 * The HTTP neighborhood is a node list only (`columns: key,label,depth`).
 * `grouped_by_edge_type` is a Rust API, not an HTTP route. `/stats` RuleStats
 * has `name/edges/tripped/fires` and **no** `edge_type`, so we cannot probe
 * `?edge_types=` from stats.
 *
 * Chosen composition (legal, no server changes):
 * 1. `GET /node/{key}/neighborhood?depth=1&dir=out` and `dir=in` (no
 *    `edge_types`) — the full 1-hop node sets, split by direction.
 * 2. `GET /explain?a={root}&b={neighbor}` once per unique neighbor — the only
 *    wire that names a derived etype and carries provenance.
 * 3. Neighbors with a matching-direction explanation become that
 *    `edge_type`. Neighbors with none become {@link USER_ETYPE} (`related`).
 *
 * TODO(plan-8 endpoint): a `/node/{key}/edges` (or neighborhood rows that
 * include etype) is required to name user edges. Until then they collapse
 * into the generic `related` relation.
 *
 * v1 root-label gap: neighborhood rows are neighbors only, so
 * `fromNeighborhood` leaves the queried root's label blank until a later
 * neighbor-expansion (or a watch `node_inserted`) supplies it. Blank-label
 * nodes paint muted and get no label chip. T7/Plan-8 can add a node-info
 * endpoint.
 */
import type { Explanation, QueryResult } from "./api";
import { edgeId } from "./store";

/** Generic bucket for user edges whose etype the HTTP surface cannot name. */
export const USER_ETYPE = "related";

export const EXPLAIN_CONCURRENCY = 8;

export type EdgeProvenance = {
  id: string;
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
  return { columns, rows: results.flatMap((r) => r.rows) };
}

export async function mapPool<T, R>(
  items: readonly T[],
  limit: number,
  fn: (item: T) => Promise<R>,
): Promise<R[]> {
  const results = new Array<R>(items.length);
  let next = 0;
  const worker = async (): Promise<void> => {
    for (;;) {
      const i = next;
      next += 1;
      if (i >= items.length) {
        return;
      }
      results[i] = await fn(items[i] as T);
    }
  };
  const n = Math.min(Math.max(limit, 1), items.length);
  await Promise.all(Array.from({ length: n }, () => worker()));
  return results;
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
  const values = await mapPool(unique, EXPLAIN_CONCURRENCY, (n) =>
    explain(root, n),
  );
  const map = new Map<string, Explanation[]>();
  for (let i = 0; i < unique.length; i++) {
    map.set(unique[i]!, values[i]!);
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
          explanation: e,
        });
      }
      continue;
    }
    const src = dir === "out" ? root : nbr;
    const dst = dir === "out" ? nbr : root;
    addNeighbor(perEtype, USER_ETYPE, nbr);
    provenance.push({ id: edgeId(USER_ETYPE, src, dst), explanation: null });
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
