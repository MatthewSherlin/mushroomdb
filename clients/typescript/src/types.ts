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
