import type {
  Explanation,
  NeighborhoodOpts,
  NodeEdges,
  NodeInfo,
  QueryParam,
  QueryResult,
} from "./api";
import { isAbsentEndpoint, isKeyNotFound } from "./api";
import {
  EXPLAIN_CONCURRENCY,
  classifyBothDirs,
  classifyFromEdges,
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
  nodeInfo(key: string): Promise<NodeInfo>;
  nodeEdges(key: string): Promise<NodeEdges>;
};

/**
 * Fetch the neighborhood of `root` and merge typed edges for **that** node
 * only (`needsEdges(root)` — never a scan of every visible node).
 *
 * Depth 2 also expands each hop-1 neighbor at depth 1 so hop-1→hop-2 edges
 * exist on the canvas. Missing labels/props (blank roots, query-add stubs)
 * resolve via `GET /node/{key}` after the merge, bounded by mapPool.
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
    await fillMissingNodeInfo(store, api, root);
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
  await fillMissingNodeInfo(store, api, root);
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
  try {
    const payload = await api.nodeEdges(root);
    applyClassified(store, root, classifyFromEdges(root, payload.edges));
    return;
  } catch (err: unknown) {
    if (isKeyNotFound(err)) {
      applyClassified(store, root, classifyFromEdges(root, []));
      return;
    }
    if (!isAbsentEndpoint(err)) {
      throw err;
    }
    // Legacy: pre-sweep servers have no /node/{key}/edges. User edges
    // collapse into USER_ETYPE ("related") via explain-composition.
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
  applyClassified(
    store,
    root,
    classifyBothDirs({
      root,
      outKeys,
      inKeys,
      explanations,
    }),
  );
}

function applyClassified(
  store: GraphStore,
  root: string,
  classified: ReturnType<typeof classifyFromEdges>,
): void {
  store.mergeNeighborhoodWithEdges(root, classified.out, "out");
  store.mergeNeighborhoodWithEdges(root, classified.in, "in");
  for (const p of classified.provenance) {
    if (p.explanation !== null) {
      store.setProvenance(p.id, p.explanation);
    } else {
      store.setDerived(p.id, p.derived);
    }
  }
}

async function fillMissingNodeInfo(
  store: GraphStore,
  api: ExpandApi,
  root: string,
): Promise<void> {
  const keys = new Set<string>([root]);
  for (const edge of store.edges.values()) {
    if (edge.src !== root && edge.dst !== root) {
      continue;
    }
    const other = edge.src === root ? edge.dst : edge.src;
    if (store.nodes.get(other)?.label === "") {
      keys.add(other);
    }
  }
  const { errors } = await mapPool([...keys], EXPLAIN_CONCURRENCY, async (key) => {
    const info = await api.nodeInfo(key);
    store.applyNodeInfo(key, info.label, info.props);
  });
  for (const e of errors) {
    if (isKeyNotFound(e.error) || isAbsentEndpoint(e.error)) {
      continue;
    }
    throw e.error;
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
