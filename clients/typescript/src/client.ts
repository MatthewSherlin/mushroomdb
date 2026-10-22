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
  TransactionConflictError,
  TransactionExpiredError,
  type AlgoReport,
  type BackupReport,
  type CreateNodeResult,
  type DegreeConfig,
  type DegreeReport,
  type EdgeHistoryResult,
  type Explanation,
  type FindSimilarOptions,
  type FindSimilarResult,
  type HealthReport,
  type IngestReport,
  type IngestRequest,
  type MetricsReport,
  type Neighborhood,
  type NodeEdgesResult,
  type NodeHistoryResult,
  type NodeInfo,
  type PageRankConfig,
  type PageRankReport,
  type QueryResult,
  type RuleDef,
  type Stats,
  type SuggestReport,
  type TxBeginResponse,
  type TxCommitResponse,
  type WasLinkedResult,
  type WccConfig,
  type WccReport,
} from "./types.js";
import {
  subscribe as wsSubscribe,
  watch as wsWatch,
  type MutationEvent,
  type SubscribeHandle,
  type SubscribeOptions,
  type WatchHandle,
  type WatchOptions,
  type WsConstructor,
} from "./ws.js";

export type {
  AlgoDir,
  AlgoReport,
  BackupReport,
  CellValue,
  CreateNodeResult,
  DegreeConfig,
  DegreeReport,
  EdgeHistoryEntry,
  EdgeHistoryResult,
  EdgeInfo,
  Explanation,
  FindSimilarOptions,
  FindSimilarResult,
  HealthReport,
  HistoryChange,
  HistoryEntry,
  IngestEdge,
  IngestOptions,
  IngestReport,
  IngestRequest,
  MetricsReport,
  Neighborhood,
  NodeEdgesResult,
  NodeHistoryResult,
  NodeInfo,
  NodeKeyOrStub,
  PageRankConfig,
  PageRankReport,
  PredicateKind,
  PredicateSummary,
  PropPredicate,
  QueryResult,
  RuleDef,
  RulePredicate,
  RuleStats,
  RuleSuggestion,
  SlowQueryEntry,
  Stats,
  SuggestReport,
  WasLinkedResult,
  WccConfig,
  WccReport,
} from "./types.js";
export { MushroomError, TransactionConflictError, TransactionExpiredError };
export type { TxBeginResponse, TxCommitResponse };

/** Optional constructor flags for {@link MushroomClient}. */
export interface ClientOptions {
  /**
   * When set, sent as `Authorization: Bearer <token>` on every HTTP fetch.
   * Cookie auth is a browser/explorer concern and is not implemented here.
   */
  token?: string;
}

/** Parameters for a Cypher query. Values must be JSON scalars; `null` binds as a null. */
export type QueryParams = Record<string, string | number | boolean | null>;

/** Options accepted by {@link MushroomClient.query}. */
export interface QueryOptions {
  /** Bound parameters. Values must be JSON scalars (string | number | boolean | null); `null` binds as a null. */
  params?: QueryParams;
  /**
   * Time-travel: run the query against the graph as it existed at this WAL
   * commit index (read-only). Valid values are `0` to `commit_seq − 1`
   * inclusive; passing `commit_seq` itself yields a range error. Obtain a
   * commit index from the `commit` field of a `nodeHistory`/`edgeHistory`
   * entry, or compute `(await client.metrics()).commit_seq - 1` for the most
   * recent committed state.
   */
  as_of?: number;
  /**
   * Node-key allow-list: restrict visible nodes to these keys. The query sees
   * only nodes whose key is in this set. Makes the query read-only.
   */
  mask?: string[];
  /**
   * Namespace to restrict results to. Intersects with `mask` and any role.
   * Makes the query read-only.
   */
  namespace?: string;
}

/**
 * An open interactive transaction. Obtained from
 * {@link MushroomClient.beginTransaction} or passed to the callback of
 * {@link MushroomClient.transaction}.
 *
 * Statements run against a snapshot frozen at begin, with the transaction's own
 * writes visible. Nothing reaches the store until {@link TxHandle.commit}.
 */
export class TxHandle {
  /** Server-assigned transaction id. */
  readonly txId: string;
  /** Expiry time as the server reported it at begin. */
  readonly expiresAt: string | number;
  /**
   * open        usable.
   * committing  a commit request is in flight; every other call throws.
   * unknown     a commit failed at the network level: whether it applied is
   *             not known, so the handle refuses further use. Check the store.
   * finished    committed, rolled back, conflicted, or expired.
   * A rollback that fails leaves the state `open`, so it can be retried.
   */
  private state: "open" | "committing" | "unknown" | "finished" = "open";

  /** @internal Use {@link MushroomClient.beginTransaction}. */
  constructor(
    txId: string,
    expiresAt: string | number,
    private readonly call: <T>(suffix: string, init?: RequestInit) => Promise<T>,
  ) {
    this.txId = txId;
    this.expiresAt = expiresAt;
  }

  /** True once the transaction is committed, rolled back, conflicted or expired. */
  get isFinished(): boolean {
    return this.state === "finished";
  }

  private assertOpen(): void {
    if (this.state === "committing") {
      throw new MushroomError(`transaction ${this.txId} has a commit in flight`);
    }
    if (this.state === "unknown") {
      throw new MushroomError(
        `transaction ${this.txId}: the commit failed at the network level and its outcome is unknown; check the store before retrying`,
      );
    }
    if (this.state === "finished") {
      throw new MushroomError(`transaction ${this.txId} is already finished`);
    }
  }

  /**
   * Run one Cypher statement inside the transaction
   * (`POST /tx/{tx_id}/run`).
   *
   * @throws {@link TransactionExpiredError} when the server no longer knows the transaction.
   */
  async execute(cypher: string, params?: QueryParams): Promise<QueryResult> {
    this.assertOpen();
    return this.call<QueryResult>("/run", {
      method: "POST",
      body: JSON.stringify({ cypher, params: params ?? {} }),
    });
  }

  /**
   * Commit (`POST /tx/{tx_id}/commit`). Resolves to the bookmark: the commit
   * number of the one WAL frame the transaction wrote.
   *
   * A second `commit()` while one is in flight throws without a request. If the
   * request fails at the network level the outcome is unknown and the handle
   * refuses all further use.
   *
   * @throws {@link TransactionConflictError} when a node the transaction read or wrote changed since begin.
   * @throws {@link TransactionExpiredError} when the transaction expired before the commit.
   */
  async commit(): Promise<number> {
    this.assertOpen();
    this.state = "committing";
    try {
      const out = await this.call<TxCommitResponse>("/commit", { method: "POST" });
      this.state = "finished";
      return out.bookmark;
    } catch (err) {
      if (
        err instanceof TransactionConflictError ||
        err instanceof TransactionExpiredError
      ) {
        // The server ended the transaction.
        this.state = "finished";
      } else if (err instanceof MushroomError) {
        // The server answered with an error; the commit did not apply.
        this.state = "open";
      } else {
        this.state = "unknown";
      }
      throw err;
    }
  }

  /**
   * Discard everything the transaction buffered (`POST /tx/{tx_id}/rollback`).
   *
   * Deliberately a silent no-op when the transaction is already finished, and
   * when the server reports it expired: {@link MushroomClient.transaction}
   * relies on this. If the request fails the handle stays open, so the
   * rollback can be retried. Throws if a commit is in flight or its outcome is
   * unknown.
   */
  async rollback(): Promise<void> {
    if (this.state === "finished") return;
    this.assertOpen();
    try {
      await this.call<unknown>("/rollback", { method: "POST" });
      this.state = "finished";
    } catch (err) {
      if (err instanceof TransactionExpiredError) {
        this.state = "finished";
        return;
      }
      throw err;
    }
  }
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
   * Validate a single URL path segment (a node key, edge type, or property
   * field name that will be percent-encoded into a URL path).
   *
   * Rejects `.` and `..` because fetch normalises those path segments away
   * (`/node/./edges` becomes `/node/edges`, silently querying the wrong route).
   * Encoding them as `%2E` / `%2E%2E` does not help — fetch normalises those
   * too. Also catches lone-surrogate strings, which would throw a raw
   * `URIError` from `encodeURIComponent`; re-throws as {@link MushroomError}.
   *
   * @throws {@link MushroomError}
   */
  private checkPathSeg(seg: string): void {
    if (seg === "." || seg === "..") {
      throw new MushroomError(
        `"${seg}" is not a valid path segment — fetch normalises it away`,
      );
    }
    try {
      encodeURIComponent(seg);
    } catch {
      // encodeURIComponent throws URIError for lone Unicode surrogates.
      throw new MushroomError(
        "key or field name contains an invalid Unicode character (lone surrogate)",
      );
    }
  }

  /**
   * Execute a fetch and decode the JSON body.
   *
   * Reads the response as text first so that an empty or plain-text error body
   * (e.g. HTTP 404 with `content-length: 0`, or a 408/413/415 plain-text body)
   * produces `MushroomError("HTTP <status>")` instead of a raw `SyntaxError`.
   *
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
    const text = await resp.text();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    let body: any;
    try {
      body = text ? JSON.parse(text) : null;
    } catch {
      body = null;
    }
    if (!resp.ok) {
      const detail: string =
        typeof body?.error === "string" ? body.error : `HTTP ${resp.status}`;
      throw new MushroomError(detail, resp.status);
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
        ...(opts?.as_of !== undefined ? { as_of: opts.as_of } : {}),
        ...(opts?.mask ? { mask: opts.mask } : {}),
        ...(opts?.namespace ? { namespace: opts.namespace } : {}),
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
    this.checkPathSeg(oldKey);
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
    this.checkPathSeg(key);
    const resp = await fetch(this.url(`/node/${encodeURIComponent(key)}`), {
      headers: {
        Accept: "application/json",
        ...this.authHeaders(),
      },
    });
    if (resp.status === 404) {
      return null;
    }
    const text = await resp.text();
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    let body: any;
    try {
      body = text ? JSON.parse(text) : null;
    } catch {
      body = null;
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
    this.checkPathSeg(key);
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

  /**
   * Get server liveness.
   *
   * Wraps `GET /health`. An unauthenticated caller receives only `{ ok: true }`.
   * A full-access token also receives `nodes`, `edges` and `addr`.
   */
  async health(): Promise<HealthReport> {
    return this.fetchJson<HealthReport>("/health");
  }

  /**
   * Get server-wide runtime metrics.
   *
   * Wraps `GET /metrics`. Returns node/edge counts, WAL size, RSS, uptime
   * and a slow-query ring buffer. Requires a full-access token; role-bound
   * tokens receive 403.
   */
  async metrics(): Promise<MetricsReport> {
    return this.fetchJson<MetricsReport>("/metrics");
  }

  /**
   * Vector kNN search.
   *
   * Wraps `POST /find_similar`. Returns up to `k` nodes whose stored value for
   * `field` has cosine similarity ≥ `min` to the query `vector`. Results are
   * sorted by similarity descending.
   *
   * @param opts  Query options — `field` and `vector` are required.
   * @returns     `{ hits: [node_key, similarity][] }`
   */
  async findSimilar(opts: FindSimilarOptions): Promise<FindSimilarResult> {
    return this.fetchJson<FindSimilarResult>("/find_similar", {
      method: "POST",
      body: JSON.stringify(opts),
    });
  }

  /**
   * Create a new node.
   *
   * Wraps `POST /nodes`. This is an **insert**, not an upsert. If a node with
   * the same `key` already exists the server responds with 400
   * "duplicate node key: <key>" and this method throws {@link MushroomError}.
   *
   * @param label  Node label.
   * @param key    Node key — must be unique in the store.
   * @param props  Optional property map. Null values are silently dropped.
   * @param namespace  Optional namespace to assign the node to.
   */
  async createNode(
    label: string,
    key: string,
    props?: Record<string, import("./types.js").CellValue>,
    namespace?: string,
  ): Promise<CreateNodeResult> {
    return this.fetchJson<CreateNodeResult>("/nodes", {
      method: "POST",
      body: JSON.stringify({
        label,
        key,
        ...(props ? { props } : {}),
        ...(namespace ? { namespace } : {}),
      }),
    });
  }

  /**
   * Delete a node by key.
   *
   * Wraps `DELETE /node/{key}`. The dense id is freed; all edges attached to
   * this node are also removed. Throws {@link MushroomError} when the key does
   * not exist.
   *
   * @param key  The node key to delete.
   */
  async deleteNode(key: string): Promise<void> {
    this.checkPathSeg(key);
    await this.fetchJson(`/node/${encodeURIComponent(key)}`, { method: "DELETE" });
  }

  /**
   * List all edges attached to a node.
   *
   * Wraps `GET /node/{key}/edges`. Returns both asserted and derived edges in
   * both directions. Throws {@link MushroomError} (404) when the key does not
   * exist.
   *
   * @param key   The node key.
   * @param opts  Optional filters:
   *   - `mask` — node-key allow-list; only edges whose **other** endpoint is
   *     in the list are returned (`key` itself need not be). Must be non-empty
   *     (an empty array throws {@link MushroomError}). A key must not contain
   *     a comma, be empty, or have surrounding whitespace: the server splits
   *     on commas and trims (use a `query()` with `mask` for such keys).
   *   - `stub_hidden` — when `true` and `mask` is set, edges to masked-out
   *     endpoints are **included** rather than omitted; those endpoints are
   *     returned as `{ key: string; restricted: true }` stubs rather than
   *     plain key strings. Without a mask this option is silently ignored by
   *     the server. Note that `as_of` (time-travel) does not compose with
   *     `stub_hidden`; the server rejects that combination.
   */
  async nodeEdges(
    key: string,
    opts?: { mask?: string[]; stub_hidden?: boolean },
  ): Promise<NodeEdgesResult> {
    this.checkPathSeg(key);
    const qs = new URLSearchParams();
    if (opts?.mask !== undefined) {
      if (opts.mask.length === 0) {
        throw new MushroomError(
          "nodeEdges mask must not be empty — an empty allow-list is ambiguous; " +
          "omit mask to return all edges, or use query() with mask:[] to hide all nodes",
        );
      }
      for (const k of opts.mask) {
        if (k === "" || k !== k.trim()) {
          throw new MushroomError(
            `nodeEdges mask key ${JSON.stringify(k)} is empty or has surrounding ` +
            "whitespace — the server trims each key, so it would match nothing or " +
            "remove the mask altogether",
          );
        }
        if (k.includes(",")) {
          throw new MushroomError(
            `nodeEdges mask key "${k}" contains a comma — the server splits on commas ` +
            "and cannot express a key with one; use query() with mask for such keys",
          );
        }
      }
      qs.set("mask", opts.mask.join(","));
    }
    if (opts?.stub_hidden !== undefined) {
      qs.set("stub_hidden", String(opts.stub_hidden));
    }
    const query = qs.toString();
    const path = `/node/${encodeURIComponent(key)}/edges${query ? `?${query}` : ""}`;
    return this.fetchJson<NodeEdgesResult>(path);
  }

  /**
   * Get WAL change history for a node.
   *
   * Wraps `GET /node/{key}/history`. Returns the sequence of WAL entries that
   * touched this node, oldest first. Throws {@link MushroomError} (404) when
   * the key does not exist.
   *
   * @param key  The node key.
   */
  async nodeHistory(key: string): Promise<NodeHistoryResult> {
    this.checkPathSeg(key);
    return this.fetchJson<NodeHistoryResult>(
      `/node/${encodeURIComponent(key)}/history`,
    );
  }

  /**
   * Get edge lifecycle between two nodes.
   *
   * Wraps `GET /history/edge?a=&b=`. Returns every time an edge was added or
   * retracted between `a` and `b` (in either direction, any edge type).
   *
   * @param a  Key of the first node.
   * @param b  Key of the second node.
   */
  async edgeHistory(a: string, b: string): Promise<EdgeHistoryResult> {
    const qs = new URLSearchParams({ a, b });
    return this.fetchJson<EdgeHistoryResult>(`/history/edge?${qs.toString()}`);
  }

  /**
   * Point-in-time edge check.
   *
   * Wraps `GET /history/was_linked?a=&b=&edge_type=&at_commit=`. Returns
   * whether any edge of `edgeType` existed between `a` and `b` at the given
   * WAL commit index. Throws {@link MushroomError} (400) when `atCommit` is
   * outside the visible horizon.
   *
   * @param a         Key of the first node.
   * @param b         Key of the second node.
   * @param edgeType  The edge type to check.
   * @param atCommit  WAL commit index (integer) or an RFC 3339 date string
   *                  (e.g. `"2026-01-01T00:00:00Z"`); the server resolves the
   *                  date to the nearest commit at or before that time.
   */
  async wasLinked(
    a: string,
    b: string,
    edgeType: string,
    atCommit: number | string,
  ): Promise<WasLinkedResult> {
    const qs = new URLSearchParams({
      a,
      b,
      edge_type: edgeType,
      at_commit: String(atCommit),
    });
    return this.fetchJson<WasLinkedResult>(
      `/history/was_linked?${qs.toString()}`,
    );
  }

  /**
   * Create an explicit edge.
   *
   * Wraps `POST /edges`. Both endpoint nodes must already exist; use
   * {@link upsertEdge} to auto-create missing endpoints.
   *
   * @param type  Edge type string.
   * @param src   Source node key.
   * @param dst   Destination node key.
   */
  async createEdge(type: string, src: string, dst: string): Promise<void> {
    await this.fetchJson("/edges", {
      method: "POST",
      body: JSON.stringify({ type, src, dst }),
    });
  }

  /**
   * Delete an explicit edge.
   *
   * Wraps `DELETE /edges/{etype}/{src}/{dst}`. Returns silently when the edge
   * does not exist (idempotent — the server answers 200). Throws
   * {@link MushroomError} only when an endpoint node is missing
   * (400 "node key not found: <key>").
   *
   * @param etype  Edge type string.
   * @param src    Source node key.
   * @param dst    Destination node key.
   */
  async deleteEdge(etype: string, src: string, dst: string): Promise<void> {
    this.checkPathSeg(etype);
    this.checkPathSeg(src);
    this.checkPathSeg(dst);
    await this.fetchJson(
      `/edges/${encodeURIComponent(etype)}/${encodeURIComponent(src)}/${encodeURIComponent(dst)}`,
      { method: "DELETE" },
    );
  }

  /**
   * Set a property on a node.
   *
   * Wraps `PUT /node/{key}/prop/{field}`. Creates the property if absent,
   * replaces its value if present. The value must be a non-null scalar, array
   * or object; a null value is rejected with 400.
   *
   * @param key    Node key.
   * @param field  Property field name.
   * @param value  New property value (non-null).
   */
  async setNodeProp(
    key: string,
    field: string,
    value: import("./types.js").CellValue,
  ): Promise<void> {
    this.checkPathSeg(key);
    this.checkPathSeg(field);
    await this.fetchJson(
      `/node/${encodeURIComponent(key)}/prop/${encodeURIComponent(field)}`,
      {
        method: "PUT",
        body: JSON.stringify({ value }),
      },
    );
  }

  /**
   * Remove a property from a node.
   *
   * Wraps `DELETE /node/{key}/prop/{field}`. Succeeds even when the property
   * does not exist (the server is idempotent on prop removal).
   *
   * @param key    Node key.
   * @param field  Property field name to remove.
   */
  async removeNodeProp(key: string, field: string): Promise<void> {
    this.checkPathSeg(key);
    this.checkPathSeg(field);
    await this.fetchJson(
      `/node/${encodeURIComponent(key)}/prop/${encodeURIComponent(field)}`,
      { method: "DELETE" },
    );
  }

  /**
   * Trigger a consistent backup of the database.
   *
   * Wraps `POST /backup`. The server holds a read lock for the duration of
   * the file copies. `dest` is a path relative to the backup root
   * (`MUSHROOMDB_BACKUP_DIR`, or the server's working directory); `..`
   * segments are rejected. Requires a full-access token.
   *
   * When `verified` is false in the response, the server answered 500 and
   * throws {@link MushroomError}; the backup was written but the post-copy
   * verification failed.
   *
   * @param dest  Destination directory name or relative path.
   */
  async backup(dest: string): Promise<BackupReport> {
    return this.fetchJson<BackupReport>("/backup", {
      method: "POST",
      body: JSON.stringify({ dest }),
    });
  }

  // -------------------------------------------------------------------------
  // Transactions
  // -------------------------------------------------------------------------

  /**
   * Run one request against a transaction route and map its failures: HTTP 409
   * becomes {@link TransactionConflictError}. 404 and 410 become
   * {@link TransactionExpiredError} on `/run`, `/commit` and `/rollback`
   * (`expiredOnMissing`); on `/tx/begin` they mean the server has no
   * transaction routes.
   */
  private async txJson<T>(
    path: string,
    init?: RequestInit,
    expiredOnMissing = true,
  ): Promise<T> {
    try {
      return await this.fetchJson<T>(path, init);
    } catch (err) {
      if (err instanceof MushroomError) {
        if (err.status === 409) {
          throw new TransactionConflictError(err.detail, err.status);
        }
        if (expiredOnMissing && (err.status === 404 || err.status === 410)) {
          throw new TransactionExpiredError(err.detail, err.status);
        }
      }
      throw err;
    }
  }

  /**
   * Begin an interactive transaction (`POST /tx/begin`).
   *
   * Prefer {@link MushroomClient.transaction}, which commits or rolls back for
   * you. With this form, always end the handle with `commit()` or `rollback()`;
   * an abandoned transaction is rolled back by the server when it expires.
   *
   * @example
   * ```ts
   * const tx = await client.beginTransaction();
   * await tx.execute("CREATE (:Person {id: $id})", { id: "p1" });
   * const bookmark = await tx.commit();
   * ```
   */
  async beginTransaction(): Promise<TxHandle> {
    let begun: TxBeginResponse;
    try {
      begun = await this.txJson<TxBeginResponse>(
        "/tx/begin",
        { method: "POST" },
        false,
      );
    } catch (err) {
      if (err instanceof MushroomError && (err.status === 404 || err.status === 405)) {
        throw new MushroomError(
          "this server does not support transactions (POST /tx/begin not found)",
          err.status,
        );
      }
      throw err;
    }
    const base = `/tx/${encodeURIComponent(begun.tx_id)}`;
    return new TxHandle(
      begun.tx_id,
      begun.expires_at,
      <T>(suffix: string, init?: RequestInit): Promise<T> =>
        this.txJson<T>(`${base}${suffix}`, init),
    );
  }

  /**
   * Run `fn` inside a transaction. The transaction commits when `fn` resolves
   * and rolls back when it throws; the error is rethrown unchanged. If the
   * rollback itself fails, that failure is attached to the thrown error as a
   * `rollbackError` property (when the thrown value is an object) and never
   * replaces it. If `fn`
   * commits or rolls back itself, nothing more is done.
   *
   * A commit conflict throws {@link TransactionConflictError}; the callback is
   * not retried.
   *
   * @returns what `fn` returned.
   */
  async transaction<T>(fn: (tx: TxHandle) => Promise<T>): Promise<T> {
    const tx = await this.beginTransaction();
    let result: T;
    try {
      result = await fn(tx);
    } catch (err) {
      try {
        await tx.rollback();
      } catch (rollbackError) {
        // The callback's error is the one the caller needs; keep the other beside it.
        if (typeof err === "object" && err !== null) {
          Object.defineProperty(err, "rollbackError", {
            value: rollbackError,
            enumerable: false,
            configurable: true,
            writable: true,
          });
        }
      }
      throw err;
    }
    if (!tx.isFinished) {
      await tx.commit();
    }
    return result;
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

  /**
   * Subscribe to the raw mutation event stream over WebSocket (`GET /watch`).
   *
   * The server sends `{"subscribed":true}` immediately after the upgrade; no
   * client subscribe message is required. After that, {@link MutationEvent}
   * frames arrive for every write to the store.
   *
   * Unlike `/subscribe`, the `/watch` stream carries every mutation (nodes,
   * edges, rules, batches, ingests) without filtering.
   *
   * **No auto-reconnect in v1.** Reconnect manually when the connection drops.
   *
   * **Always await `handle.close()`** to avoid dangling handles in tests.
   *
   * **Node.js < 21**: pass `wsConstructor` — see {@link WatchOptions}.
   *
   * @example
   * ```ts
   * import WS from 'ws';
   * const handle = await client.watch(
   *   { wsConstructor: WS as WsConstructor },
   *   (ev) => console.log(ev),
   * );
   * await handle.close();
   * ```
   */
  watch(
    opts: WatchOptions,
    onEvent: (event: MutationEvent) => void,
  ): Promise<WatchHandle> {
    let url = this.wsUrl("/watch");
    if (this.token) {
      url += `?token=${encodeURIComponent(this.token)}`;
    }
    return wsWatch(url, opts, onEvent);
  }
}

export type {
  MutationEvent,
  SubscribeHandle,
  SubscribeOptions,
  WatchHandle,
  WatchOptions,
  WsConstructor,
};
