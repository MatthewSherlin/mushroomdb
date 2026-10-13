/**
 * TypeScript types for the mushroomdb HTTP + WebSocket API.
 *
 * Every type is annotated with the Rust struct / enum it mirrors.
 */

// ---------------------------------------------------------------------------
// Primitive value
// ---------------------------------------------------------------------------

/**
 * A single cell value in a query result row.
 *
 * Mirrors `core_storage::types::Value`:
 *   Int(i64)        → number
 *   Float(f64)      → number (NaN serializes as null per server behaviour)
 *   Str(String)     → string
 *   Bool(bool)      → boolean
 *   List(Vec<Value>) → CellValue[]
 *   null cell       → null
 *
 * **Precision warning (Int / i64):** JavaScript `number` (IEEE 754 double)
 * safely represents integers only up to ±2^53 − 1 (~9×10^15). Rust `i64`
 * reaches ±9.2×10^18. Integer node properties whose absolute value exceeds
 * 2^53 will be silently corrupted when parsed as JS numbers. For such values
 * use a string representation in the graph instead. A future release will
 * offer a BigInt-aware parsing mode.
 */
export type CellValue = number | string | boolean | null | CellValue[];

// ---------------------------------------------------------------------------
// Query result
// ---------------------------------------------------------------------------

/**
 * Wire shape of a successful `POST /query?format=json` response.
 *
 * Mirrors `crates/server/src/json.rs::result_set_json` output:
 *   { columns: string[], rows: (scalar|null)[][] }
 */
export interface QueryResult {
  columns: string[];
  rows: CellValue[][];
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/**
 * Per-rule statistics.
 *
 * Mirrors `core_api::db::RuleStats`.
 */
export interface RuleStats {
  name: string;
  edges: number;
  tripped: boolean;
  fires: number;
  approximate: boolean;
}

/**
 * Database-wide counters returned by `GET /stats`.
 *
 * Mirrors `core_api::db::Stats`.
 */
export interface Stats {
  nodes_live: number;
  nodes_tombstoned: number;
  edges: number;
  rules: RuleStats[];
}

// ---------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------

/**
 * Options for `POST /ingest`.
 *
 * Mirrors `core_api::IngestOptions` (serialised by `crates/server/src/http.rs::ingest_options`).
 */
export interface IngestOptions {
  /** Name of the field to use as the node key. Defaults to "key". */
  key_field?: string;
  /**
   * Auto foreign-key detection mode.
   *   false | "off"       → disabled
   *   { suffix: string }  → detect fields ending with `suffix` (e.g. "_id")
   */
  auto_fk?: false | "off" | { suffix: string };
}

/** An explicit edge to wire during ingest. */
export interface IngestEdge {
  edge_type: string;
  src: string;
  dst: string;
}

/** Body sent to `POST /ingest`. */
export interface IngestRequest {
  label: string;
  rows: Record<string, unknown>[];
  options?: IngestOptions;
  edges?: IngestEdge[];
}

/** Response from `POST /ingest`. Shape is opaque; use `ok` to check success. */
export type IngestReport = Record<string, unknown>;

// ---------------------------------------------------------------------------
// Node / explain / neighborhood / rules
// ---------------------------------------------------------------------------

/**
 * Wire `NodeInfo` from `GET /node/{key}`.
 *
 * `props` uses the same untagged Value JSON as `/query`. Unknown keys are
 * HTTP 404; the client `node()` method maps that to `null`.
 */
export interface NodeInfo {
  key: string;
  label: string;
  props: Record<string, CellValue>;
}

/**
 * Predicate kind on an {@link Explanation}, matching HTTP snake_case JSON.
 */
export type PredicateKind =
  | "key_match"
  | "field_equal"
  | "overlap"
  | "all"
  | "numeric_within"
  | "geo_radius"
  | "vector_similar";

/**
 * Wire `PredicateSummary`. Option fields are present-null, never omitted.
 */
export interface PredicateSummary {
  kind: PredicateKind;
  fields: string[];
  min: number | null;
  tolerance: number | null;
  km: number | null;
  parts: PredicateSummary[] | null;
}

/**
 * One rule-derived edge between two nodes from `GET /explain?a=&b=`.
 *
 * Mirrors `core_api::db::Explanation`.
 */
export interface Explanation {
  rule: string;
  edge_type: string;
  src_key: string;
  dst_key: string;
  weight: number | null;
  predicate: PredicateSummary;
}

/**
 * Wire neighborhood expansion from `GET /node/{key}/neighborhood`.
 *
 * Columns are `key`, `label`, `depth`.
 */
export interface Neighborhood {
  columns: string[];
  rows: CellValue[][];
}

/**
 * Internally-tagged `Predicate` JSON accepted by `POST /rules`.
 *
 * Mirrors `core_rules::Predicate`.
 */
export type RulePredicate =
  | { KeyMatch: { field: string } }
  | { FieldEqual: { field: string } }
  | { Overlap: { field: string; min: number } }
  | { NumericWithin: { field: string; tolerance: number } }
  | { GeoRadius: { field: string; km: number } }
  | { VectorSimilar: { field: string; min: number } }
  | { All: RulePredicate[] }
  | { Any: RulePredicate[] };

/**
 * Rule definition posted to `POST /rules`.
 *
 * Mirrors `core_rules::RuleDef`. Omit or `max_edges: null` → server fills
 * scored top-k 32, or 512 if the predicate is KeyMatch-rooted (one per element
 * a list-valued FK field can name). HTTP has no
 * uncapped hatch (Rust/Python explicit `None` still uses the 1_000_000
 * global first-N-by-id budget).
 */
export interface RuleDef {
  name: string;
  src_label: string;
  dst_label: string;
  predicate: RulePredicate;
  edge_type: string;
  weight_prop?: string | null;
  /** Per-source top-k. Omit/`null` fills 32 (scored) or 512 (KeyMatch-rooted). */
  max_edges?: number | null;
  approximate?: boolean;
}

// ---------------------------------------------------------------------------
// Suggest
// ---------------------------------------------------------------------------

/**
 * One rule suggestion returned by `GET /suggest`.
 *
 * Mirrors `core_rules::suggest::RuleSuggestion`.
 */
export interface RuleSuggestion {
  /** The proposed rule definition (not yet created in the database). */
  def: Record<string, unknown>;
  /** Estimated edge count if the rule were applied. */
  est_edges: number;
  /** Up to 3 example (src_key, dst_key, score) triples drawn from sample evaluation. */
  examples: [string, string, number][];
  /** Human-readable explanation of why this rule was suggested. */
  rationale: string;
}

/**
 * Response from `GET /suggest`.
 *
 * Mirrors `core_rules::suggest::SuggestReport`.
 */
export interface SuggestReport {
  suggestions: RuleSuggestion[];
  /**
   * true when the global time budget fired before all candidates were evaluated.
   * Partial results are still returned.
   */
  truncated: boolean;
}

// ---------------------------------------------------------------------------
// Algo
// ---------------------------------------------------------------------------

/**
 * Edge direction for graph algorithms.
 *
 * Mirrors `core_api::algo::AlgoDir` (serialises as lowercase string).
 */
export type AlgoDir = "out" | "in" | "both";

/**
 * Config for `POST /algo/pagerank`.
 *
 * Mirrors `core_api::algo::PageRankConfig`. All fields are optional — the
 * server applies defaults (damping=0.85, max_iters=50, tol=1e-6,
 * edge_type=null, direction="out", budget_ms=5000).
 */
export interface PageRankConfig {
  damping?: number;
  max_iters?: number;
  tol?: number;
  edge_type?: string | null;
  direction?: AlgoDir;
  budget_ms?: number;
}

/**
 * Result of `POST /algo/pagerank`.
 *
 * Mirrors `core_api::algo::PageRankReport`.
 */
export interface PageRankReport {
  /** [node_key, score] pairs, sorted by score descending (ties: key asc). */
  scores: [string, number][];
  /** true when the algorithm converged before max_iters and before any budget fired. */
  converged: boolean;
}

/**
 * Config for `POST /algo/wcc`.
 *
 * Mirrors `core_api::algo::WccConfig`. Defaults: edge_type=null, budget_ms=5000.
 */
export interface WccConfig {
  edge_type?: string | null;
  budget_ms?: number;
}

/**
 * Result of `POST /algo/wcc`.
 *
 * Mirrors `core_api::algo::WccReport`.
 */
export interface WccReport {
  /** [node_key, component_id] pairs. component_id is the smallest key in the component. */
  components: [string, string][];
  truncated: boolean;
}

/**
 * Config for `POST /algo/degree`.
 *
 * Mirrors `core_api::algo::DegreeConfig`. Defaults: edge_type=null, direction="both", budget_ms=5000.
 */
export interface DegreeConfig {
  edge_type?: string | null;
  direction?: AlgoDir;
  budget_ms?: number;
}

/**
 * Result of `POST /algo/degree`.
 *
 * Mirrors `core_api::algo::DegreeReport`.
 */
export interface DegreeReport {
  /**
   * [node_key, degree] pairs, sorted by degree descending (ties: key asc).
   *
   * **Precision warning:** Rust returns `u64` degrees. JS `number` (IEEE 754)
   * safely represents values up to 2^53 − 1. Nodes with degree above ~9×10^15
   * will silently lose precision. In practice graphs with that many edges per
   * node do not exist, but be aware of the type constraint.
   */
  scores: [string, number][];
  truncated: boolean;
}

/** Union of all algo configs. */
export type AlgoConfig =
  | { algo: "pagerank"; config?: PageRankConfig }
  | { algo: "wcc"; config?: WccConfig }
  | { algo: "degree"; config?: DegreeConfig };

/** Union of all algo results. */
export type AlgoReport = PageRankReport | WccReport | DegreeReport;

// ---------------------------------------------------------------------------
// WebSocket subscription events
// ---------------------------------------------------------------------------

/**
 * A post-commit event delivered over the `/subscribe` WebSocket.
 *
 * Mirrors `core_api::subscription::DbEvent` — serialised as internally-tagged
 * JSON with `"type"` as the discriminant.
 *
 * All variants except `lagged` carry `commit_seq: number` (Rust: `u64`).
 * `lagged` means the subscriber's internal queue overflowed; the caller should
 * re-read graph state to recover consistency for lossless consumers.
 *
 * **Precision warning (`commit_seq` / `missed`):** Both are Rust `u64` and map
 * to JS `number` (IEEE 754 double, safe up to 2^53 − 1 ≈ 9×10^15). A
 * long-lived server generating more than ~9×10^15 commits will silently
 * corrupt these values. A future release will represent u64 fields as
 * `string` or `bigint`. For now treat `commit_seq` as an opaque ordering key,
 * not an exact integer.
 */
export type DbEvent =
  | {
      type: "edge_fired";
      rule: string;
      src_key: string;
      dst_key: string;
      edge_type: string;
      weight?: number;
      commit_seq: number;
    }
  | {
      type: "edge_retracted";
      rule: string;
      src_key: string;
      dst_key: string;
      edge_type: string;
      commit_seq: number;
    }
  | {
      type: "node_inserted";
      label: string;
      key: string;
      commit_seq: number;
    }
  | {
      type: "node_deleted";
      key: string;
      commit_seq: number;
    }
  | {
      type: "edge_inserted";
      edge_type: string;
      src: string;
      dst: string;
      commit_seq: number;
    }
  | {
      type: "edge_deleted";
      edge_type: string;
      src: string;
      dst: string;
      commit_seq: number;
    }
  | {
      type: "prop_set";
      key: string;
      field: string;
      commit_seq: number;
    }
  | {
      type: "prop_removed";
      key: string;
      field: string;
      commit_seq: number;
    }
  | {
      /**
       * One or more events were dropped because the subscriber's queue was full
       * (capacity 65,536). The caller must re-read graph state to recover
       * consistency for lossless consumers.
       */
      type: "lagged";
      missed: number;
    };

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/**
 * Response from `GET /health`.
 *
 * Unauthenticated callers receive only `{ ok: true }`. A full-access token
 * also receives `nodes`, `edges` and `addr`.
 */
export interface HealthReport {
  ok: boolean;
  nodes?: number;
  edges?: number;
  addr?: string;
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/**
 * One entry in the slow-query ring buffer from `GET /metrics`.
 */
export interface SlowQueryEntry {
  ms: number;
  query: string;
  at_commit: number;
}

/**
 * Response from `GET /metrics`.
 *
 * Mirrors the JSON object emitted by `crates/server/src/http.rs::metrics`.
 * `wal_size_bytes` and `rss_bytes` are null when the value is unavailable
 * (non-Linux/macOS platforms or a filesystem error reading the WAL size).
 */
export interface MetricsReport {
  nodes_live: number;
  nodes_tombstoned: number;
  edges: number;
  commit_seq: number;
  wal_size_bytes: number | null;
  rss_bytes: number | null;
  uptime_s: number;
  slow_queries: {
    threshold_ms: number;
    count: number;
    last: SlowQueryEntry[];
  };
}

// ---------------------------------------------------------------------------
// Prop predicate (used by find_similar `where` and by role definitions)
// ---------------------------------------------------------------------------

/**
 * A property filter accepted by `POST /find_similar`'s `where` field and by
 * the server's role-definition API.
 *
 * Mirrors `core_api::roles::PropPredicate` (a flat struct serialised as a JSON
 * object with `field` required and exactly one of `eq` or `in`).
 *
 * The server also accepts the graph's own tagged Value form for the values:
 * `{"Str": "x"}`, `{"Int": 3}` and `{"Bool": true}` are equivalent to their
 * plain scalar counterparts `"x"`, `3` and `true`.
 */
export type PropPredicate =
  /** Match nodes whose `field` equals `eq`. */
  | { field: string; eq: CellValue }
  /** Match nodes whose `field` is one of the values in `in`. */
  | { field: string; in: CellValue[] };

// ---------------------------------------------------------------------------
// Find similar
// ---------------------------------------------------------------------------

/**
 * Options for `POST /find_similar`.
 *
 * `field` and `vector` are required. All other fields are optional and apply
 * server defaults when absent.
 */
export interface FindSimilarOptions {
  /** The vector property field to search. */
  field: string;
  /** Query vector — must be the same dimensionality as the stored vectors. */
  vector: number[];
  /** Maximum number of results (default: 10). */
  k?: number;
  /** Minimum cosine similarity threshold (default: 0.8). */
  min?: number;
  /** Restrict results to nodes of this label. */
  label?: string;
  /** Node-key allow-list — restrict to these nodes. */
  mask?: string[];
  /** Force exhaustive scan instead of ANN index. */
  exact?: boolean;
  /**
   * Property predicate applied after the vector search: only nodes whose
   * stored value for `field` matches are returned.
   *
   * The server accepts two forms — `eq` (single-value equality) and `in`
   * (set membership). Plain JSON scalars are accepted (`"published"`, `3`,
   * `true`) as well as the graph's own tagged form (`{"Str":"published"}`).
   *
   * @example `{ field: "status", eq: "published" }`
   * @example `{ field: "status", in: ["draft", "review"] }`
   */
  where?: PropPredicate;
}

/**
 * Response from `POST /find_similar`.
 *
 * `hits` is an array of `[node_key, similarity_score]` pairs sorted by score
 * descending. Scores are cosine similarities in [min, 1.0].
 */
export interface FindSimilarResult {
  hits: [string, number][];
}

// ---------------------------------------------------------------------------
// Node edges
// ---------------------------------------------------------------------------

/**
 * A node-key endpoint in a masked edge.
 *
 * When `nodeEdges` is called with `mask` **and** `stub_hidden: true`, an
 * endpoint that lies outside the mask is returned as a restricted stub
 * object rather than a plain string. Without `stub_hidden`, out-of-mask edges
 * are omitted entirely.
 *
 * ```ts
 * if (typeof edge.dst_key === 'string') {
 *   // visible endpoint
 * } else {
 *   // restricted stub — only the key is known
 *   console.log(edge.dst_key.key, edge.dst_key.restricted); // true
 * }
 * ```
 */
export type NodeKeyOrStub = string | { key: string; restricted: true };

/**
 * One edge entry from `GET /node/{key}/edges`.
 *
 * Mirrors `core_api::EdgeInfo` as serialised by
 * `crates/server/src/json.rs::node_edges_json` (unmasked call) or
 * `json.rs::masked_edges_json` (call with `mask`/`stub_hidden`).
 *
 * When the call is made **without** a mask, `src_key` and `dst_key` are
 * always plain strings. When `stub_hidden: true` is in use, a masked-out
 * endpoint is returned as `{ key: string; restricted: true }` — see
 * {@link NodeKeyOrStub}.
 */
export interface EdgeInfo {
  edge_type: string;
  src_key: NodeKeyOrStub;
  dst_key: NodeKeyOrStub;
  /** true when this edge was derived by a rule; false when it was asserted. */
  derived: boolean;
}

/**
 * Response from `GET /node/{key}/edges`.
 */
export interface NodeEdgesResult {
  edges: EdgeInfo[];
}

// ---------------------------------------------------------------------------
// Node history
// ---------------------------------------------------------------------------

/**
 * One change entry in a node's history.
 *
 * Mirrors `core_api::HistoryChange` via
 * `crates/server/src/json.rs::history_change_json`.
 * The `"type"` field is the discriminant.
 */
export type HistoryChange =
  | { type: "NodeInserted"; label: string }
  | { type: "PropSet"; field: string; value: CellValue }
  | { type: "PropRemoved"; field: string }
  | { type: "EdgeAdded"; edge_type: string; other: string; outgoing: boolean }
  | { type: "EdgeRemoved"; edge_type: string; other: string; outgoing: boolean }
  | { type: "NodeDeleted" };

/**
 * One commit entry in a node's history from `GET /node/{key}/history`.
 *
 * Mirrors `crates/server/src/json.rs::history_entry_json`.
 */
export interface HistoryEntry {
  commit: number;
  change: HistoryChange;
}

/**
 * Response from `GET /node/{key}/history`.
 *
 * `horizon` is the oldest commit history can reach; events before it were
 * pruned and are not in `history`.
 */
export interface NodeHistoryResult {
  key: string;
  history: HistoryEntry[];
  /** Total WAL commits in the store. */
  total_commits: number;
  /** Oldest commit visible in history. Events before this were pruned. */
  horizon: number;
}

// ---------------------------------------------------------------------------
// Edge history
// ---------------------------------------------------------------------------

/**
 * One event in the lifecycle of an edge from `GET /history/edge?a=&b=`.
 *
 * Mirrors `core_api::EdgeHistoryEvent` as serialised by
 * `crates/server/src/json.rs::edge_history_result_json`.
 */
export interface EdgeHistoryEntry {
  edge_type: string;
  commit: number;
  event: "Added" | "Retracted";
  /** Rule that derived this edge, or null for an asserted edge. */
  rule: string | null;
}

/**
 * Response from `GET /history/edge?a=&b=`.
 */
export interface EdgeHistoryResult {
  a: string;
  b: string;
  events: EdgeHistoryEntry[];
  /** Total WAL commits in the store. */
  total_commits: number;
  /** Oldest commit visible in history. Events before this were pruned. */
  horizon: number;
}

// ---------------------------------------------------------------------------
// Was linked
// ---------------------------------------------------------------------------

/**
 * Response from `GET /history/was_linked?a=&b=&edge_type=&at_commit=`.
 *
 * `linked` is true when any edge of `edge_type` existed between `a` and `b`
 * (in either direction) at the given commit.
 */
export interface WasLinkedResult {
  a: string;
  b: string;
  edge_type: string;
  at_commit: number;
  linked: boolean;
}

// ---------------------------------------------------------------------------
// Create node result
// ---------------------------------------------------------------------------

/**
 * Response from `POST /nodes`.
 *
 * `nodes` is 1 when the node was created. A duplicate key yields a 400 error
 * instead of returning 0 — `POST /nodes` is an insert, not an upsert.
 * `edges` is always 0 for this endpoint.
 */
export interface CreateNodeResult {
  ok: boolean;
  nodes: number;
  edges: number;
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

/**
 * Response from `POST /backup`.
 *
 * Mirrors `BackupReportJson` from `crates/server/src/http.rs`.
 * When `verified` is false the server answers 500 — the backup was written
 * but the post-copy verification failed; inspect `files` and `bytes`.
 */
export interface BackupReport {
  /** Names of the files copied into the backup directory. */
  files: string[];
  /** Total bytes written. */
  bytes: number;
  /** true when the copy was verified by re-reading the destination. */
  verified: boolean;
}

// ---------------------------------------------------------------------------
// Mutation events (GET /watch WebSocket)
// ---------------------------------------------------------------------------

/**
 * A mutation event delivered over the `/watch` WebSocket.
 *
 * Mirrors `core_api::MutationEvent` — serialised as externally-tagged
 * snake_case JSON (`{"node_inserted":{"label":"A","key":"k"}}`).
 *
 * The `lagged` variant (`{"lagged": N}`) is not produced by serde but by the
 * WebSocket handler; it means the broadcast channel overflowed by N events.
 * Re-read any state your consumer depends on to recover consistency.
 *
 * **Note:** The `/watch` stream is the raw mutation log, distinct from the
 * filtered post-commit subscription on `/subscribe`. Every write to the store
 * appears here; `/subscribe` lets callers filter by rule or write type.
 */
export type MutationEvent =
  | { node_inserted: { label: string; key: string } }
  | { prop_set: { key: string; field: string } }
  | { prop_removed: { key: string; field: string } }
  | { edge_inserted: { edge_type: string; src: string; dst: string } }
  | { edge_deleted: { edge_type: string; src: string; dst: string } }
  | { node_deleted: { key: string } }
  | { rule_created: { name: string } }
  | { rule_deleted: { name: string } }
  | { rule_rebuilt: { name: string } }
  | { batch_applied: { ops: number } }
  | { ingested: { label: string; inserted: number } }
  | { lagged: number };

// ---------------------------------------------------------------------------
// Subscribe message (sent by client → server)
// ---------------------------------------------------------------------------

/**
 * Subscribe message sent to the server after the WebSocket upgrade.
 *
 * Mirrors `crates/server/src/subscribe.rs::SubscribeMsg`.
 * All fields are optional. With no fields the client receives no events.
 */
export interface SubscribeMessage {
  /** Rule names to subscribe to. Receive EdgeFired/EdgeRetracted for each. */
  rules?: string[];
  /** If true, also receive node/property write events. */
  writes?: boolean;
}

// ---------------------------------------------------------------------------
// Client error
// ---------------------------------------------------------------------------

/**
 * Thrown by the client when the server returns an HTTP error or the
 * WebSocket handshake fails.
 */
export class MushroomError extends Error {
  constructor(public readonly detail: string) {
    super(detail);
    this.name = "MushroomError";
  }
}
