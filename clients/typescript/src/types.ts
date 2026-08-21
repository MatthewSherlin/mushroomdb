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
 *   Int(i64)   → number
 *   Float(f64) → number (NaN serializes as null per server behaviour)
 *   Str(String) → string
 *   Bool(bool) → boolean
 *   List(Vec<Value>) → CellValue[]
 *   null cell → null
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
  /** [node_key, degree] pairs, sorted by degree descending (ties: key asc). */
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
 * All variants except `lagged` carry `commit_seq: number` (u64 in Rust).
 * `lagged` means the subscriber's internal queue overflowed; the caller should
 * re-read graph state to recover consistency for lossless consumers.
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
