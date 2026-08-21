/**
 * HTTP client for the mushroomdb server.
 *
 * Uses the browser-standard `fetch` API (built into Node 18+).
 *
 * ```ts
 * import { MushroomClient } from 'mushroomdb-client';
 *
 * const client = new MushroomClient('http://127.0.0.1:8080');
 * const result = await client.query('MATCH (n:Person) RETURN n.name LIMIT 10');
 * console.log(result.columns, result.rows);
 * ```
 */

import {
  MushroomError,
  type AlgoReport,
  type CellValue,
  type DegreeConfig,
  type DegreeReport,
  type IngestReport,
  type IngestRequest,
  type PageRankConfig,
  type PageRankReport,
  type QueryResult,
  type Stats,
  type SuggestReport,
  type WccConfig,
  type WccReport,
} from "./types.js";
import {
  subscribe as wsSubscribe,
  type SubscribeHandle,
  type SubscribeOptions,
  type WsConstructor,
} from "./ws.js";

export type {
  AlgoDir,
  AlgoReport,
  CellValue,
  DegreeConfig,
  DegreeReport,
  IngestEdge,
  IngestOptions,
  IngestReport,
  IngestRequest,
  PageRankConfig,
  PageRankReport,
  QueryResult,
  RuleStats,
  RuleSuggestion,
  Stats,
  SuggestReport,
  WccConfig,
  WccReport,
} from "./types.js";
export { MushroomError };

/** Parameters for a Cypher query. Values must be JSON scalars. */
export type QueryParams = Record<string, string | number | boolean>;

/** Options accepted by {@link MushroomClient.query}. */
export interface QueryOptions {
  /** Bound parameters. Values must be JSON scalars (string | number | boolean). */
  params?: QueryParams;
}

/**
 * HTTP + WebSocket client for mushroomdb.
 *
 * All methods use the browser-standard `fetch` API and are therefore
 * compatible with both Node.js 18+ and modern browsers.
 *
 * **Node-only**: The `subscribe` method requires a WebSocket implementation.
 * In Node < 21, install the `ws` package and pass `wsConstructor` in the
 * subscribe options. See {@link SubscribeOptions}.
 */
export class MushroomClient {
  private readonly baseUrl: string;
  private readonly wsBase: string;

  /**
   * @param baseUrl  HTTP base URL of the mushroomdb server, e.g.
   *                 `"http://127.0.0.1:8080"`. Trailing slash is stripped.
   */
  constructor(baseUrl: string) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.wsBase = this.baseUrl.replace(/^http/, "ws");
  }

  // -------------------------------------------------------------------------
  // Internal helpers
  // -------------------------------------------------------------------------

  private url(path: string): string {
    return `${this.baseUrl}${path}`;
  }

  private wsUrl(path: string): string {
    return `${this.wsBase}${path}`;
  }

  /**
   * Execute a fetch and decode the JSON body.
   * Throws {@link MushroomError} on non-2xx responses.
   */
  private async fetchJson<T>(
    path: string,
    init?: RequestInit,
  ): Promise<T> {
    const resp = await fetch(this.url(path), {
      ...init,
      headers: {
        "Content-Type": "application/json",
        Accept: "application/json",
        ...(init?.headers ?? {}),
      },
    });
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const body: any = await resp.json();
    if (!resp.ok) {
      const detail: string =
        typeof body?.error === "string" ? body.error : `HTTP ${resp.status}`;
      throw new MushroomError(detail);
    }
    return body as T;
  }

  // -------------------------------------------------------------------------
  // HTTP endpoints
  // -------------------------------------------------------------------------

  /**
   * Run a Cypher query (read or write).
   *
   * The server auto-detects write statements (`CREATE`, `MERGE`, `SET`,
   * `DELETE`) and acquires the appropriate lock. Both read and write queries
   * go to `POST /query?format=json`.
   *
   * @param cypher Cypher query string.
   * @param opts   Optional bound parameters (JSON scalar values only).
   * @returns      Column names and a 2-D array of {@link CellValue} rows.
   */
  async query(cypher: string, opts?: QueryOptions): Promise<QueryResult> {
    return this.fetchJson<QueryResult>("/query?format=json", {
      method: "POST",
      body: JSON.stringify({
        cypher,
        ...(opts?.params ? { params: opts.params } : {}),
      }),
    });
  }

  /**
   * Ingest nodes (and optional edges) into the database.
   *
   * Wraps `POST /ingest`. The server acquires the write lock, applies the
   * rows to the WAL, and runs all rules incrementally.
   *
   * @param req  Ingest payload — `label`, `rows`, optional `options` and `edges`.
   * @returns    Server ingest report (opaque; check for absence of errors).
   */
  async ingest(req: IngestRequest): Promise<IngestReport> {
    return this.fetchJson<IngestReport>("/ingest", {
      method: "POST",
      body: JSON.stringify(req),
    });
  }

  /**
   * Get database-wide statistics.
   *
   * Wraps `GET /stats`. Returns live node/edge counts and per-rule stats.
   */
  async stats(): Promise<Stats> {
    return this.fetchJson<Stats>("/stats");
  }

  /**
   * Profile the database and return rule suggestions.
   *
   * Wraps `GET /suggest`. CPU-intensive — runs in the server's blocking
   * thread-pool with a 5-second global budget. The {@link SuggestReport}
   * includes a `truncated` flag when the budget fires early.
   *
   * Suggestions are not auto-applied; call `POST /rules` to create a rule.
   */
  async suggest(): Promise<SuggestReport> {
    return this.fetchJson<SuggestReport>("/suggest");
  }

  /**
   * Run a graph algorithm.
   *
   * Wraps `POST /algo/{pagerank|wcc|degree}`.
   *
   * @param algo   Algorithm name.
   * @param config Optional algorithm-specific configuration.
   * @returns      Algorithm report — see {@link PageRankReport}, {@link WccReport},
   *               {@link DegreeReport}.
   *
   * @example
   * ```ts
   * const pr = await client.algo('pagerank') as PageRankReport;
   * console.log(pr.scores.slice(0, 5));
   * ```
   */
  async algo(algo: "pagerank", config?: PageRankConfig): Promise<PageRankReport>;
  async algo(algo: "wcc", config?: WccConfig): Promise<WccReport>;
  async algo(algo: "degree", config?: DegreeConfig): Promise<DegreeReport>;
  async algo(
    algo: "pagerank" | "wcc" | "degree",
    config?: PageRankConfig | WccConfig | DegreeConfig,
  ): Promise<AlgoReport> {
    // Server structs use #[derive(Deserialize)] without #[serde(default)], so
    // every field is required in the JSON body. Merge caller config over the
    // server's own defaults so an empty call works out of the box.
    const defaults: Record<string, Record<string, unknown>> = {
      pagerank: { damping: 0.85, max_iters: 50, tol: 1e-6, edge_type: null, direction: "out", budget_ms: 5000 },
      wcc:      { edge_type: null, budget_ms: 5000 },
      degree:   { edge_type: null, direction: "both", budget_ms: 5000 },
    };
    const body = { ...defaults[algo]!, ...(config ?? {}) };
    return this.fetchJson<AlgoReport>(`/algo/${algo}`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  }

  // -------------------------------------------------------------------------
  // WebSocket
  // -------------------------------------------------------------------------

  /**
   * Subscribe to post-commit events over WebSocket (`GET /subscribe`).
   *
   * Returns a promise that resolves when the server acknowledges the
   * subscription (`{"subscribed":true}`). After that, `onEvent` is called
   * for each {@link DbEvent}, including {@link DbEvent.lagged} frames.
   *
   * **No auto-reconnect in v1.** When the connection drops, no further events
   * are delivered. Reconnect manually if required.
   *
   * **Always await `handle.close()`** when done — an open WebSocket keeps the
   * Node.js event loop alive and will cause test hangs.
   *
   * **Node.js < 21**: pass `wsConstructor` — see {@link SubscribeOptions}.
   *
   * @example
   * ```ts
   * import WS from 'ws';
   * const handle = await client.subscribe(
   *   { writes: true, wsConstructor: WS as WsConstructor },
   *   (ev) => console.log(ev),
   * );
   * // ... do work ...
   * await handle.close();
   * ```
   */
  subscribe(
    opts: SubscribeOptions,
    onEvent: (event: import("./types.js").DbEvent) => void,
  ): Promise<SubscribeHandle> {
    return wsSubscribe(this.wsUrl("/subscribe"), opts, onEvent);
  }
}

export type { SubscribeHandle, SubscribeOptions, WsConstructor };
