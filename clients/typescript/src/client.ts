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
  type DegreeConfig,
  type DegreeReport,
  type Explanation,
  type IngestReport,
  type IngestRequest,
  type Neighborhood,
  type NodeInfo,
  type PageRankConfig,
  type PageRankReport,
  type QueryResult,
  type RuleDef,
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
  Explanation,
  IngestEdge,
  IngestOptions,
  IngestReport,
  IngestRequest,
  Neighborhood,
  NodeInfo,
  PageRankConfig,
  PageRankReport,
  PredicateKind,
  PredicateSummary,
  QueryResult,
  RuleDef,
  RulePredicate,
  RuleStats,
  RuleSuggestion,
  Stats,
  SuggestReport,
  WccConfig,
  WccReport,
} from "./types.js";
export { MushroomError };

/** Optional constructor flags for {@link MushroomClient}. */
export interface ClientOptions {
  /**
   * When set, sent as `Authorization: Bearer <token>` on every HTTP fetch.
   * Cookie auth is a browser/explorer concern and is not implemented here.
   */
  token?: string;
}

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
  private readonly token: string | undefined;

  /**
   * @param baseUrl  HTTP base URL of the mushroomdb server, e.g.
   *                 `"http://127.0.0.1:8080"`. Trailing slash is stripped.
   * @param opts     Optional `{ token }` — sent as `Authorization: Bearer`.
   */
  constructor(baseUrl: string, opts?: ClientOptions) {
    this.baseUrl = baseUrl.replace(/\/$/, "");
    this.wsBase = this.baseUrl.replace(/^http/, "ws");
    this.token = opts?.token;
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

  private authHeaders(): Record<string, string> {
    return this.token ? { Authorization: `Bearer ${this.token}` } : {};
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
        ...this.authHeaders(),
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
    // The server structs carry #[serde(default)], so sending only the fields
    // the caller explicitly set (or an empty body {}) is valid — the server
    // fills in its own defaults for any missing fields.
    return this.fetchJson<AlgoReport>(`/algo/${algo}`, {
      method: "POST",
      body: JSON.stringify(config ?? {}),
    });
  }

  /**
   * Explain rule-derived edges between two node keys.
   *
   * Wraps `GET /explain?a=&b=`.
   */
  async explain(a: string, b: string): Promise<Explanation[]> {
    const qs = new URLSearchParams({ a, b });
    return this.fetchJson<Explanation[]>(`/explain?${qs.toString()}`);
  }

  /**
   * Create a derivation rule.
   *
   * Wraps `POST /rules`. The server acquires the write lock, validates the
   * {@link RuleDef}, and backfills matching pairs.
   */
  async createRule(def: RuleDef): Promise<void> {
    await this.fetchJson("/rules", {
      method: "POST",
      body: JSON.stringify(def),
    });
  }

  /**
   * Rename a node's key.
   *
   * Wraps `POST /nodes/{key}/rename`. The dense id and all edges remain stable.
   *
   * @param oldKey The current key.
   * @param newKey The desired new key.
   * @throws {@link MushroomError} on `KeyNotFound` (old key) or `DuplicateKey` (new key taken).
   */
  async renameNode(oldKey: string, newKey: string): Promise<void> {
    await this.fetchJson(`/nodes/${encodeURIComponent(oldKey)}/rename`, {
      method: "POST",
      body: JSON.stringify({ new_key: newKey }),
    });
  }

  /**
   * Insert an edge, auto-creating any missing endpoint as a placeholder node.
   *
   * Wraps `POST /edges/upsert`. Each missing endpoint is created with
   * `placeholderLabel` and no properties.
   *
   * @param edgeType The edge type string.
   * @param src Source node key.
   * @param dst Destination node key.
   * @param placeholderLabel Label applied to any auto-created endpoint node.
   * @returns A report with `nodes_created` and `edge_inserted`.
   */
  async upsertEdge(
    edgeType: string,
    src: string,
    dst: string,
    placeholderLabel: string,
  ): Promise<{ nodes_created: number; edge_inserted: boolean }> {
    return this.fetchJson<{ nodes_created: number; edge_inserted: boolean }>(
      "/edges/upsert",
      {
        method: "POST",
        body: JSON.stringify({
          edge_type: edgeType,
          src_key: src,
          dst_key: dst,
          placeholder_label: placeholderLabel,
        }),
      },
    );
  }

  /**
   * Fetch a node by key.
   *
   * Wraps `GET /node/{key}`. Returns `null` when the server answers 404
   * (unknown key). Other HTTP errors throw {@link MushroomError}.
   */
  async node(key: string): Promise<NodeInfo | null> {
    const resp = await fetch(this.url(`/node/${encodeURIComponent(key)}`), {
      headers: {
        Accept: "application/json",
        ...this.authHeaders(),
      },
    });
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const body: any = await resp.json();
    if (resp.status === 404) {
      return null;
    }
    if (!resp.ok) {
      const detail: string =
        typeof body?.error === "string" ? body.error : `HTTP ${resp.status}`;
      throw new MushroomError(detail);
    }
    return body as NodeInfo;
  }

  /**
   * Depth-N neighborhood of a node.
   *
   * Wraps `GET /node/{key}/neighborhood`. Default depth is the server's
   * (1). Columns are `key`, `label`, `depth`.
   */
  async neighborhood(
    key: string,
    opts?: { depth?: number },
  ): Promise<Neighborhood> {
    const qs = new URLSearchParams();
    if (opts?.depth !== undefined) {
      qs.set("depth", String(opts.depth));
    }
    const query = qs.toString();
    const path = `/node/${encodeURIComponent(key)}/neighborhood${
      query ? `?${query}` : ""
    }`;
    return this.fetchJson<Neighborhood>(path);
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
    let url = this.wsUrl("/subscribe");
    if (this.token) {
      url += `?token=${encodeURIComponent(this.token)}`;
    }
    return wsSubscribe(url, opts, onEvent);
  }
}

export type { SubscribeHandle, SubscribeOptions, WsConstructor };
