import type {
  Explanation,
  NeighborhoodOpts,
  QueryParam,
  QueryResult,
} from "./api";
import {
  EXPLAIN_CONCURRENCY,
  classifyBothDirs,
  concatResults,
  explainNeighbors,
  firstNodeKey,
  hopKeysAtDepth,
  mapPool,
  neighborKeys,
} from "./classify";
import type { GraphStore } from "./store";

export type ExpandApi = {
  query(
    cypher: string,
    params?: Record<string, QueryParam>,
  ): Promise<QueryResult>;
  neighborhood(key: string, opts?: NeighborhoodOpts): Promise<QueryResult>;
  explain(a: string, b: string): Promise<Explanation[]>;
};

/**
 * Fetch the neighborhood of `root` and merge typed edges for **that** node
 * only (`needsEdges(root)` — never a scan of every visible node).
 *
 * Depth 2 also expands each hop-1 neighbor at depth 1 so hop-1→hop-2 edges
 * exist on the canvas.
 */
export async function expandNode(
  store: GraphStore,
  api: ExpandApi,
  root: string,
  depth: 1 | 2,
): Promise<void> {
  if (depth === 2) {
    const deep = await api.neighborhood(root, { depth: 2, dir: "both" });
    store.fromNeighborhood(root, deep);
    await attachEdges(store, api, root);
    const hop1 = hopKeysAtDepth(deep, 1).filter((key) => key !== root);
    // mapPool is best-effort: a failed hop-1 expand does not abort siblings.
    await mapPool(hop1, EXPLAIN_CONCURRENCY, (key) =>
      expandNode(store, api, key, 1),
    );
    return;
  }

  const [outNb, inNb] = await Promise.all([
    api.neighborhood(root, { depth: 1, dir: "out" }),
    api.neighborhood(root, { depth: 1, dir: "in" }),
  ]);
  store.fromNeighborhood(root, concatResults(outNb, inNb));
  await attachEdges(store, api, root, outNb, inNb);
}

async function attachEdges(
  store: GraphStore,
  api: ExpandApi,
  root: string,
  outNb?: QueryResult,
  inNb?: QueryResult,
): Promise<void> {
  if (!store.needsEdges(root)) {
    return;
  }
  const outResult =
    outNb ?? (await api.neighborhood(root, { depth: 1, dir: "out" }));
  const inResult =
    inNb ?? (await api.neighborhood(root, { depth: 1, dir: "in" }));
  const outKeys = neighborKeys(outResult, root);
  const inKeys = neighborKeys(inResult, root);
  const unique = [...new Set([...outKeys, ...inKeys])];
  const explanations = await explainNeighbors(
    (a, b) => api.explain(a, b),
    root,
    unique,
  );
  const classified = classifyBothDirs({
    root,
    outKeys,
    inKeys,
    explanations,
  });
  store.mergeNeighborhoodWithEdges(root, classified.out, "out");
  store.mergeNeighborhoodWithEdges(root, classified.in, "in");
  for (const p of classified.provenance) {
    store.setProvenance(p.id, p.explanation);
  }
}

export async function loadDemoNeighborhood(
  store: GraphStore,
  api: ExpandApi,
): Promise<string> {
  const result = await api.query("MATCH (n) RETURN n LIMIT 1");
  const key = firstNodeKey(result);
  if (key === undefined) {
    throw new Error("no nodes");
  }
  await expandNode(store, api, key, 1);
  return key;
}
