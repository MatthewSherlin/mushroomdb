# Spec: app-platform (Plan 18)

Date: 2026-08-27. Author: controller session on Matthew's standing direction
(SESSION-RESUME roadmap: Plan 18 "app-platform", approved 2026-08-22).
Matthew was asleep when this spec was written; decisions marked RULING are
provisional controller rulings, recorded in the plan-18 SDD ledger, and are
his to overturn on review.

## 1. Goal

Make mushroomdb the one-dependency backend for small internal graph apps
(linkt-KB class): per-query visibility control, nested document values,
one-call hybrid retrieval, declarative idempotent schema, and per-node
change history.

## 2. Features (normative)

### F1 — Map / nested values

- `Value::Map(BTreeMap<String, Value>)` APPENDED as the last variant of
  `Value` (bincode positional discriminants of existing variants must not
  change; old snapshots/WALs stay readable; files containing Map are not
  readable by older binaries — acceptable pre-1.0, document like RuleDef
  appended fields).
- BTreeMap, not HashMap: deterministic serialization (WAL replay identity,
  snapshot byte stability).
- Arbitrary nesting with List: `Map{... List[Map{...}]}` legal.
- JSON ingest: JSON objects map to `Value::Map` (today's behavior for
  objects — flattening or rejection — is replaced; document the change).
- Cypher: Map values are storable, returnable, and comparable for
  equality/inequality only (no ordering, no arithmetic). RULING: no
  nested dot-path accessor (`n.field.sub`) in this plan — YAGNI until an
  app needs it; apps read whole maps via `node_info`/JSON/RETURN.
- Columns: Map values take the existing Mixed/List spill path (slow path,
  documented). Fulltext: Map fields are not indexed (documented).
- V7 snapshot pack: Map rides the existing spill encoding; snapshot
  VERSION stays 7 IF AND ONLY IF the added variant does not change the
  byte encoding of non-Map data; otherwise bump and keep old decode.
  golden_v5/v6/v7 pins must stay green.

### F2 — Query-scoped node masks (ACL primitive)

- `NodeMask`: an explicit allow-set of node keys, resolved to dense ids at
  construction: `NodeMask::from_keys<I: IntoIterator<Item=impl AsRef<str>>>(db, keys)`.
  Unknown keys are ignored (mask is app-computed; absence = invisible anyway).
- `GraphDb::query_masked(&self, cypher: &str, params: &BTreeMap<String, Value>, mask: &NodeMask) -> Result<ResultSet>`:
  read-only Cypher evaluated as if nodes outside the mask (and every edge
  with either endpoint outside) do not exist. Write statements under a mask
  are rejected with a clear error.
- Implementation shape: `GraphView` (a concrete struct with pub fields)
  gains `mask: Option<&NodeMask>` + an inline `visible(u32) -> bool`;
  the executor consults `visible` at its three node-binding sites
  (label scan, key lookup, neighbor expansion). No per-operator
  scattering beyond those binding sites.
- RULING: masks apply to `query`-style reads only. Rules, views, fulltext
  search, subscriptions, and algo APIs are out of scope for this plan.
- Server: `POST /query` accepts optional `"mask": ["key", ...]`; MCP
  `query` tool gains the same optional argument. Absent mask = today's
  behavior, zero overhead.

### F3 — Hybrid search (RRF over fulltext + vector)

- `GraphDb::search_hybrid(&self, text_field: &str, query_text: &str,
  vector_field: &str, query_vec: &[f64], label: Option<&str>, k: usize)
  -> Vec<(String, f64)>` (plain Vec, matching `search`/`find_similar_vector`
  house style)
- Semantics: take top `4*k` fulltext hits for (`text_field`, `query_text`)
  and top `4*k` ANN/brute-force hits for (`vector_field`, `query_vec`,
  min=0.0); fuse with Reciprocal Rank Fusion, `score(d) = Σ 1/(60 + rank_i(d))`
  (rank 1-based; RRF_K = 60 fixed constant); return top `k` by fused score,
  ties broken by node key ascending (determinism).
- A node appearing in only one list still scores (that is the point of RRF).
- MCP tool `hybrid_search` with arguments `{query_text, text_field,
  vector, vector_field?, label?, k?}` (vector_field default "embedding",
  k default 10).

### F4 — Schema-as-code

- `Schema { fulltext: Vec<(String, String)>, rules: Vec<RuleDef>,
  views: Vec<ViewDef> }`, serde JSON round-trippable.
- `GraphDb::apply_schema(&mut self, schema: &Schema) -> Result<SchemaDiff>`
  where `SchemaDiff { created: Vec<String>, updated: Vec<String>,
  unchanged: Vec<String> }` (entries namespaced: "rule:NAME",
  "view:NAME", "fulltext:LABEL.FIELD").
- Idempotent: applying the same schema twice → second diff is all
  `unchanged`, zero WAL writes for unchanged items.
- Update semantics: a rule/view whose definition differs from the live one
  is replaced (delete + create; rules re-backfill — document the cost).
  RULING: no pruning — items live in the db but absent from the schema are
  left untouched (destructive prune waits for explicit demand).
- CLI: `mushroomdb schema apply <db-path> <schema.json>` printing the diff.

### F5 — node_history(key)

- `GraphDb::node_history(&self, key: &str) -> Result<Vec<HistoryEntry>>`
  where `HistoryEntry { commit: u64, change: HistoryChange }` and
  `HistoryChange` covers: NodeInserted{label}, PropSet{field, value},
  PropRemoved{field}, EdgeAdded{edge_type, other, direction},
  EdgeRemoved{edge_type, other, direction}, NodeDeleted.
- Source of truth: the CURRENT on-disk WAL, scanned on demand (no new
  in-memory state). Dense-id records are resolved by tracking Intern and
  key→id bindings incrementally during the scan.
- HORIZON (documented loudly): history reaches back to the last
  WAL-truncating snapshot, exactly like `open_at`. `keep_wal: true`
  snapshots preserve deeper history. This is the honest, zero-cost
  contract; a durable history log is out of scope.
- Derived (rule-created) edges are NOT in the WAL and therefore not in
  history (documented).

## 3. Gate G18

- All five features have committed tests exercising real behavior;
  `cargo test --workspace` green; fmt + clippy clean.
- Map: WAL replay identity + snapshot roundtrip tests with nested maps;
  golden pins green.
- Masks: a masked query returns exactly the visible subgraph; edges to
  hidden nodes invisible; write-under-mask rejected; no-mask path
  unchanged (existing query tests untouched).
- Hybrid: fixture where fulltext-only, vector-only, and both-lists nodes
  rank correctly per RRF; determinism pinned.
- Schema: apply → re-apply produces all-unchanged and writes zero WAL
  frames the second time (fsync/commit count asserted).
- History: insert/set/edge/delete sequence reproduced in order with
  correct commit indices; post-snapshot horizon behavior pinned.

## 3a. Maintenance rider (required by F1)

`bindings/python/src/lib.rs` matches `Value` exhaustively (`py_to_value`,
`value_to_py`) and constructs `RuleDef` — F1's Map variant and Phase 4's
`via_label`/`via_edge`/`via_dir` fields both break its compile. The plan
includes a compile-sync task: add the Map arms (Python dict ↔ Value::Map)
and the via fields (default None). Compile-checked only; wheel build/tests
remain a separate manual step (maturin venv).

## 4. Non-goals (this plan)

- Cypher dot-path access into maps; map mutation operators.
- Mask enforcement inside rules/views/subscriptions/fulltext.
- Schema pruning; schema for masks.
- Durable full-history log; history for derived edges.
- Auth/roles server-side (masks are the primitive; policy is the app's).
