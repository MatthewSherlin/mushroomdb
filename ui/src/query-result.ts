/**
 * Console result shaping and Add-to-canvas harvest.
 *
 * Harvest matches {@link GraphStore.mergeQueryGraph}: a `key` column binds
 * keys (and optional `label`); otherwise every string cell in a non-dotted
 * column that is not `label`/`depth` is a key. Property projections
 * (`n.name`) are never keys. Add-to-canvas is disabled when harvest is empty
 * so the UI never silently adds nothing.
 */
import { ApiError, type JsonCell, type QueryResult } from "./api";
import { EXPLAIN_CONCURRENCY, mapPool } from "./classify";
import { expandNode, type ExpandApi } from "./expand";
import type { GraphStore } from "./store";

export const NO_RESULT_HINT = "Run a query first";

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

export async function addHarvestedToCanvas(
  store: GraphStore,
  api: ExpandApi,
  result: QueryResult,
): Promise<string[]> {
  const keys = harvestableKeys(result);
  if (keys.length === 0) {
    return [];
  }
  store.mergeQueryGraph(result.columns, result.rows);
  const { errors } = await mapPool(keys, EXPLAIN_CONCURRENCY, (key) =>
    expandNode(store, api, key, 1),
  );
  if (errors.length === 0) {
    return keys;
  }
  const failed = new Set(errors.map((e) => e.index));
  return keys.filter((_, i) => !failed.has(i));
}

function stringCell(cell: JsonCell | undefined): string | undefined {
  return typeof cell === "string" && cell !== "" ? cell : undefined;
}
