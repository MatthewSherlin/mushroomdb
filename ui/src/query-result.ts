/**
 * Console result shaping and Add-to-canvas harvest.
 *
 * Harvest matches {@link GraphStore.mergeQueryGraph}: a `key` column binds
 * keys (and optional `label`); otherwise every string cell in a non-dotted
 * column that is not `label`/`depth` is a key. Property projections
 * (`n.name`) are never keys. Add-to-canvas is disabled when harvest is empty
 * so the UI never silently adds nothing.
 */
import { ApiError, isAbsentEndpoint, isKeyNotFound, type JsonCell, type QueryResult } from "./api";
import { EXPLAIN_CONCURRENCY, mapPool } from "./classify";
import { expandNode, type ExpandApi } from "./expand";
import type { GraphStore } from "./store";

export const NO_RESULT_HINT = "Run a query first";

/**
 * Nodes with degree above this threshold are skipped during auto-expansion in
 * {@link addHarvestedToCanvas}. They are placed on the canvas un-expanded
 * (via mergeQueryGraph) so a single user click triggers expandNode directly.
 * markDirty is also applied so the live-resync path picks them up if a watch
 * event arrives before the user clicks.
 */
export const DENSE_HUB_DEGREE_THRESHOLD = 50;

export const NO_HARVEST_HINT =
  "No node keys in this result — return a node variable or project key and label";

export type HarvestDecision = {
  keys: string[];
  blocked: string | undefined;
};

export type FormattedTable = {
  columns: string[];
  rows: string[][];
};

export function harvestableKeys(result: QueryResult): string[] {
  const keys: string[] = [];
  const seen = new Set<string>();
  const add = (key: string): void => {
    if (seen.has(key)) {
      return;
    }
    seen.add(key);
    keys.push(key);
  };

  const keyIdx = result.columns.indexOf("key");
  if (keyIdx >= 0) {
    for (const row of result.rows) {
      const key = stringCell(row[keyIdx]);
      if (key !== undefined) {
        add(key);
      }
    }
    return keys;
  }

  for (const row of result.rows) {
    for (let i = 0; i < result.columns.length; i++) {
      const col = result.columns[i];
      if (
        col === undefined ||
        col.includes(".") ||
        col === "label" ||
        col === "depth"
      ) {
        continue;
      }
      const key = stringCell(row[i]);
      if (key !== undefined) {
        add(key);
      }
    }
  }
  return keys;
}

/** Failed Run discards the last result so Add-to-canvas cannot use stale keys. */
export function resultAfterRun(
  outcome: { ok: true; value: QueryResult } | { ok: false },
): QueryResult | undefined {
  return outcome.ok ? outcome.value : undefined;
}

/** Failed Run clears the result table, not only the harvest. */
export function tableAfterRun(
  outcome: { ok: true; value: QueryResult } | { ok: false },
): FormattedTable | undefined {
  const next = resultAfterRun(outcome);
  return next === undefined ? undefined : formatTable(next);
}

export function harvestDecision(
  result: QueryResult | undefined,
): HarvestDecision {
  if (result === undefined) {
    return { keys: [], blocked: NO_RESULT_HINT };
  }
  const keys = harvestableKeys(result);
  if (keys.length === 0) {
    return { keys, blocked: NO_HARVEST_HINT };
  }
  return { keys, blocked: undefined };
}

export function formatCell(cell: JsonCell): string {
  if (cell === null) {
    return "null";
  }
  if (typeof cell === "string") {
    return cell;
  }
  if (typeof cell === "number" || typeof cell === "boolean") {
    return String(cell);
  }
  return JSON.stringify(cell);
}

export function formatTable(result: QueryResult): FormattedTable {
  return {
    columns: result.columns,
    rows: result.rows.map((row) =>
      result.columns.map((_, i) => formatCell(row[i] ?? null)),
    ),
  };
}

export function queryErrorText(err: unknown): string {
  if (err instanceof ApiError) {
    return err.message;
  }
  if (err instanceof Error) {
    return err.message;
  }
  return String(err);
}

export type AddHarvestedOpts = {
  /** Called once after all keys are processed if any dense hubs were skipped. */
  onProgress?: (msg: string) => void;
};

export async function addHarvestedToCanvas(
  store: GraphStore,
  api: ExpandApi,
  result: QueryResult,
  opts?: AddHarvestedOpts,
): Promise<string[]> {
  const keys = harvestableKeys(result);
  if (keys.length === 0) {
    return [];
  }
  store.mergeQueryGraph(result.columns, result.rows);
  let skipped = 0;
  const { errors } = await mapPool(keys, EXPLAIN_CONCURRENCY, async (key) => {
    // Fetch degree first (cheap). Dense hubs (degree > DENSE_HUB_DEGREE_THRESHOLD)
    // are skipped here so Add never blocks >5 s for nodes with hundreds of edges.
    // The node is already on canvas via mergeQueryGraph; a user click calls
    // expandNode directly (see Explorer.onPointClick). markDirty additionally
    // registers it for the live-resync path in case a watch event arrives first.
    let degree: number | undefined;
    try {
      const ne = await api.nodeEdges(key);
      degree = ne.edges.length;
    } catch (err: unknown) {
      if (!isAbsentEndpoint(err) && !isKeyNotFound(err)) {
        throw err;
      }
      // Absent endpoint (old server) or key miss — fall through to full expand.
    }
    if (degree !== undefined && degree > DENSE_HUB_DEGREE_THRESHOLD) {
      // markDirty: if a live-watch event arrives before the user clicks, the
      // resync path will pick this node up. The primary expand path is the
      // user's click → Explorer.onPointClick → expandNode (unconditional).
      store.markDirty(key);
      skipped += 1;
      return;
    }
    await expandNode(store, api, key, 1);
  });
  if (skipped > 0) {
    opts?.onProgress?.(
      `${skipped} dense node${skipped === 1 ? "" : "s"} skipped (>${DENSE_HUB_DEGREE_THRESHOLD} edges) — click to expand`,
    );
  }
  if (errors.length === 0) {
    return keys;
  }
  const failed = new Set(errors.map((e) => e.index));
  return keys.filter((_, i) => !failed.has(i));
}

function stringCell(cell: JsonCell | undefined): string | undefined {
  return typeof cell === "string" && cell !== "" ? cell : undefined;
}
