# Plan 18 — app-platform Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make mushroomdb the one-dependency backend for small internal graph apps: Map values, query-scoped node masks, hybrid RRF search, idempotent schema-as-code, and per-node history.

**Architecture:** Six sequential tasks on one branch. Task 1 (Map values) is the foundation and touches the Value blast radius; Tasks 2–5 are feature-independent of each other; Task 6 is a compile-sync of the out-of-workspace Python binding. All storage changes are appended (Value variant, no wire renumbering); snapshot VERSION stays 7 unless golden pins say otherwise.

**Tech Stack:** In-tree Rust only. **No new crates.** serde/bincode/zstd as today.

**Spec:** `docs/superpowers/specs/2026-08-27-app-platform.md`

## Global Constraints

- `cargo test --workspace --offline`, `cargo fmt --check`, `cargo clippy --workspace --offline --all-targets` (zero warnings) green after every task. Cargo bin: `$HOME/.rustup/toolchains/1.92.0-aarch64-apple-darwin/bin`.
- `Value::Map(BTreeMap<String, Value>)` is APPENDED (bincode discriminant 5, after `List`=4). Existing discriminants must not change; `golden_v5_pin`/`golden_v6_pin`/`golden_v7_pin` must stay green unmodified.
- No Cypher dot-path map accessor. Map compares for equality only. Fulltext does not index Map (existing `_ => vec![]` catch-all already gives this).
- Masks apply to read `query` only; write statements under a mask are rejected. No-mask path must be byte-identical behavior (existing tests untouched).
- RRF constant 60, candidate pools 4*k, rank 1-based, ties by key ascending.
- `apply_schema` idempotent: second apply of an identical schema performs zero WAL writes.
- `node_history` horizon = current WAL (since last truncating snapshot); derived edges excluded; both documented on the method.
- Conventional lowercase commits, no Co-Authored-By.

---

## File map

| File | Role |
|---|---|
| `crates/core-storage/src/types.rs` | Value::Map variant + ValueKey arm |
| `crates/core-storage/src/columns.rs` | Map handled by spill path in set/get/remove/pack/unpack/to_map/from_first/accepts |
| `crates/core-query/src/value_ops.rs` | Map equality; type rank; as_f64 → None |
| `crates/server/src/json.rs` | Value::Map ↔ JSON object |
| `crates/core-api/src/ingest.rs` (or json ingest seam) | JSON objects → Value::Map |
| `crates/core-query/src/view.rs` | `mask` field + `visible()` on GraphView |
| `crates/core-query/src/cypher/exec.rs` | mask checks at ScanLabel/ScanKey/Expand binding sites |
| `crates/core-api/src/mask.rs` | new: `NodeMask` |
| `crates/core-api/src/db.rs` | `query_masked`, `search_hybrid`, `apply_schema`, `node_history` |
| `crates/core-api/src/schema.rs` | new: `Schema`, `SchemaDiff` |
| `crates/core-api/src/history.rs` | new: `HistoryEntry`, `HistoryChange` |
| `crates/server/src/http.rs` | `/query` optional `mask`; routes unchanged otherwise |
| `crates/server/src/mcp.rs` | `query` mask arg; new `hybrid_search` tool |
| `crates/cli/src/lib.rs` | `schema apply` subcommand |
| `bindings/python/src/lib.rs` | Map arms + via_* RuleDef fields (compile-sync only) |

---

### Task 1: `Value::Map`

**Files:**
- Modify: `crates/core-storage/src/types.rs` (Value enum ~line 4; ValueKey::from_value)
- Modify: `crates/core-storage/src/columns.rs` (all exhaustive matches — set/get/remove/pack/unpack/to_map/from_first/accepts; Map routes to the Mixed/spill path exactly like List)
- Modify: `crates/core-query/src/value_ops.rs` (values_equal, cmp_type_rank, as_f64)
- Modify: `crates/server/src/json.rs` (value_to_json + the json→value direction)
- Modify: JSON ingest seam so JSON objects become `Value::Map` (find where `ingest_json`/`ingest_batch` convert `serde_json::Value::Object` today — if objects are currently rejected or stringified, replace with Map conversion and note the behavior change in the doc comment)
- Test: `crates/core-api/tests/mutations.rs` (or a new `map_values.rs` integration test), `crates/core-storage/src/columns.rs` unit tests, `crates/core-api/tests/snapshot.rs`

**Interfaces:**
- Produces: `Value::Map(std::collections::BTreeMap<String, Value>)` — appended LAST. Every later task may rely on Map round-tripping through set_prop/WAL/snapshot/JSON.

- [ ] **Step 1: Write failing tests**

Integration (`crates/core-api/tests/map_values.rs`, new file):

```rust
use core_api::{GraphDb, Value};
use std::collections::BTreeMap;

fn m(pairs: &[(&str, Value)]) -> Value {
    Value::Map(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn map_value_roundtrips_through_wal_and_snapshot() {
    let dir = tmp("map-roundtrip");
    let nested = m(&[
        ("city", Value::Str("berlin".into())),
        ("scores", Value::List(vec![Value::Int(1), m(&[("deep", Value::Bool(true))])])),
    ]);
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![("meta".into(), nested.clone())]).unwrap();
    }
    // WAL replay
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("a", "meta"), Some(&nested));
    drop(db);
    // snapshot (V7 pack) roundtrip
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.snapshot().unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("a", "meta"), Some(&nested));
}

#[test]
fn map_equality_in_cypher_where() {
    let dir = tmp("map-cypher-eq");
    let mut db = GraphDb::open(&dir).unwrap();
    let meta = m(&[("k", Value::Int(1))]);
    db.insert_node("N", "a", vec![("meta".into(), meta.clone())]).unwrap();
    db.insert_node("N", "b", vec![("meta".into(), m(&[("k", Value::Int(2))]))]).unwrap();
    let mut params = std::collections::BTreeMap::new();
    params.insert("m".to_string(), meta);
    let rs = db.query("MATCH (n:N) WHERE n.meta = $m RETURN n.id", &params).unwrap();
    assert_eq!(rs.rows.len(), 1);
}
```

(Adjust the RETURN column to whatever the existing tests use for node identity — read a neighboring query test first.)

- [ ] **Step 2: Run to fail** — `Value::Map` does not exist; compile errors across the exhaustive matches ARE the failing state. Record the compile-error list as RED evidence.

- [ ] **Step 3: Implement**

`types.rs`: append `Map(BTreeMap<String, Value>)` to `Value` (import `std::collections::BTreeMap`). `ValueKey::from_value`: give Map a stable key form — serialize deterministically, e.g. `ValueKey::Str(format!("{v:?}"))` is NOT acceptable; follow whatever ValueKey does for List (read it; mirror the same recursive strategy for Map with key-sorted iteration — BTreeMap already iterates sorted).

`columns.rs`: every match gains a `Value::Map(_)` arm routed exactly like `Value::List` (spill/Mixed path). `accepts`/`from_first`: Map never promotes a homogeneous column; it lives in spill like List.

`value_ops.rs`: `values_equal` — recursive structural equality (BTreeMap == handles it if Value: PartialEq — check whether Value derives PartialEq; if yes most arms come free); `cmp_type_rank` — Map ranks after List (append at the end, document "sorts after all other types"); `as_f64` → `None`.

`json.rs`: `value_to_json`: Map → JSON object (recurse). json→value: JSON object → Map (this may currently error or flatten — replace).

Ingest seam: JSON object properties become `Value::Map` recursively.

WAL/snapshot need NO format work: Value rides inside bincode-encoded records and the columns spill path — but verify `golden_v5_pin`/`golden_v6_pin`/`golden_v7_pin` still pass untouched (they contain no Map, and existing discriminants did not move).

- [ ] **Step 4: Run tests** — new tests green; full workspace green; goldens green; fmt+clippy clean.

- [ ] **Step 5: Commit** `feat: nested map values`

---

### Task 2: query-scoped node masks

**Files:**
- Create: `crates/core-api/src/mask.rs`
- Modify: `crates/core-api/src/lib.rs` (export NodeMask)
- Modify: `crates/core-query/src/view.rs` (GraphView.mask + visible())
- Modify: `crates/core-query/src/cypher/exec.rs` (three binding sites)
- Modify: `crates/core-api/src/db.rs` (`view_masked`, `query_masked`)
- Modify: `crates/server/src/http.rs` (`/query` optional `"mask"` array), `crates/server/src/mcp.rs` (query tool optional `mask`)
- Test: `crates/core-api/tests/mask.rs` (new), `crates/server/tests/http.rs`

**Interfaces:**
- Produces:

```rust
// crates/core-api/src/mask.rs
pub struct NodeMask { pub(crate) visible: std::collections::HashSet<u32> }
impl NodeMask {
    /// Resolve keys to dense ids; unknown keys are ignored.
    pub fn from_keys<'a, F: core_storage::fs::Fs>(
        db: &GraphDb<F>, keys: impl IntoIterator<Item = &'a str>) -> Self;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
// core-query view.rs
pub struct GraphView<'a> { /* existing fields */ pub mask: Option<&'a std::collections::HashSet<u32>> }
impl GraphView<'_> { #[inline] pub fn visible(&self, id: u32) -> bool }
// db.rs
pub fn query_masked(&self, cypher: &str, params: &BTreeMap<String, Value>, mask: &NodeMask) -> Result<ResultSet>
```

- Consumes: nothing from earlier tasks.

- [ ] **Step 1: Failing test** (`crates/core-api/tests/mask.rs`):

```rust
use core_api::{GraphDb, NodeMask, Value};
use std::collections::BTreeMap;

#[test]
fn masked_query_hides_nodes_and_their_edges() {
    let dir = /* tmp helper as in other tests */;
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "alice", vec![]).unwrap();
    db.insert_node("P", "bob", vec![]).unwrap();
    db.insert_node("P", "carol", vec![]).unwrap();
    db.insert_edge("KNOWS", "alice", "bob").unwrap();
    db.insert_edge("KNOWS", "alice", "carol").unwrap();
    let mask = NodeMask::from_keys(&db, ["alice", "bob"]);
    let p = BTreeMap::new();
    // label scan sees only masked nodes
    let rs = db.query_masked("MATCH (n:P) RETURN n.id", &p, &mask).unwrap();
    assert_eq!(rs.rows.len(), 2);
    // expansion cannot reach carol
    let rs = db.query_masked(
        "MATCH (a:P)-[r:KNOWS]->(b:P) RETURN b.id", &p, &mask).unwrap();
    assert_eq!(rs.rows.len(), 1); // only alice->bob
    // key lookup on hidden node binds nothing
    let rs = db.query_masked("MATCH (n {id: \"carol\"}) RETURN n.id", &p, &mask).unwrap();
    assert_eq!(rs.rows.len(), 0);
    // unmasked query unchanged
    let rs = db.query("MATCH (n:P) RETURN n.id", &p).unwrap();
    assert_eq!(rs.rows.len(), 3);
}

#[test]
fn masked_write_is_rejected() {
    // query_masked with a CREATE/SET statement -> Err with clear message
}
```

(Match the `{id: ...}` key-lookup syntax to what existing query tests use.)

- [ ] **Step 2: Run to fail** (NodeMask unresolved).

- [ ] **Step 3: Implement.** `visible(id)` = `self.mask.map_or(true, |m| m.contains(&id))`. Binding sites in exec.rs: (a) ScanLabel — filter `nodes_with_label` output; (b) ScanKey / `node_id` resolution — treat invisible as not-found; (c) Expand — filter neighbor ids before binding. `query_masked` parses; if the statement is a write form (the parse distinguishes read/write — `query` vs `query_write` split already exists) return `Err(GraphError::QueryError { .. })` with "masked queries are read-only". Construct `GraphView` with `mask: Some(&mask.visible)`; all existing `GraphView` construction sites set `mask: None`.
  HTTP: `/query` body gains optional `"mask": ["key", ...]`; when present build `NodeMask::from_keys` and route to `query_masked` (reject write cypher under mask with 400). MCP `query` tool: same optional `mask` array argument, schema updated.

- [ ] **Step 4: Full suite** — all pre-existing query tests must pass UNTOUCHED (no-mask behavioral identity).

- [ ] **Step 5: Commit** `feat: query-scoped node masks`

---

### Task 3: hybrid search (RRF)

**Files:**
- Modify: `crates/core-api/src/db.rs` (`search_hybrid`)
- Modify: `crates/server/src/mcp.rs` (new `hybrid_search` tool: tools_list entry + dispatch arm + handler)
- Test: `crates/core-api/tests/rules.rs` or new `crates/core-api/tests/hybrid.rs`; `crates/server/tests/mcp.rs`

**Interfaces:**
- Consumes: `search(&self, field, query) -> Vec<(String, usize)>` (db.rs:3265-ish); `find_similar_vector(&self, field, label, q, k, min) -> Vec<(String, f64)>` (db.rs:3583-ish).
- Produces:

```rust
pub fn search_hybrid(
    &self,
    text_field: &str, query_text: &str,
    vector_field: &str, query_vec: &[f64],
    label: Option<&str>, k: usize,
) -> Vec<(String, f64)>
```

- [ ] **Step 1: Failing test** — fixture with three nodes: `t_only` (matches text, no useful vector), `v_only` (vector-close, no text match), `both` (matches text AND vector-close). With k=3: `both` must rank first (two RRF contributions); all three present; scores strictly descending; equal-score tie broken by key ascending (add a fourth node engineered to tie if cheap, else pin determinism by asserting exact score values: `1/61+1/61`, `1/61`, etc. — compute the expected floats in the test).

- [ ] **Step 2: Run to fail.**

- [ ] **Step 3: Implement.** Text ranks: `search(text_field, query_text)` — already sorted; take first `4*k`; rank = position+1. Vector ranks: label required for the vector leg — when `label` is None, brute-force needs a universe: RULING baked into this plan: `label` is required whenever `query_vec` participates via brute-force; pass `label: Option<&str>` through to `find_similar_vector` with `min = 0.0`, `k = 4*k`; when label is None and no HNSW rule covers the field, return text-only ranking (document). Fuse: `score = Σ 1/(60 + rank)`; sort score DESC then key ASC; truncate k.
  MCP `hybrid_search` args: `{query_text: string, text_field: string, vector?: number[], vector_field?: string (default "embedding"), label?: string, k?: number (default 10)}`; omitted `vector` → text-only ranking through the same RRF path.

- [ ] **Step 4: Full suite.**

- [ ] **Step 5: Commit** `feat: hybrid search with reciprocal rank fusion`

---

### Task 4: schema-as-code

**Files:**
- Create: `crates/core-api/src/schema.rs`
- Modify: `crates/core-api/src/lib.rs` (export), `crates/core-api/src/db.rs` (`apply_schema`)
- Modify: `crates/cli/src/lib.rs` (`schema apply <db> <file>` subcommand, printing the diff)
- Test: `crates/core-api/tests/schema.rs` (new); CLI covered by unit test if the CLI has test precedent, else core-api only

**Interfaces:**
- Consumes: `create_rule/delete_rule/rules`, `create_view/delete_view/views`, `enable_fulltext/is_fulltext_enabled` (all on GraphDb; signatures in db.rs ~3018-3244).
- Produces:

```rust
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Schema {
    #[serde(default)] pub fulltext: Vec<(String, String)>, // (label, field)
    #[serde(default)] pub rules: Vec<RuleDef>,
    #[serde(default)] pub views: Vec<ViewDef>,
}
#[derive(Debug, PartialEq)]
pub struct SchemaDiff {
    pub created: Vec<String>, pub updated: Vec<String>, pub unchanged: Vec<String>,
}
pub fn apply_schema(&mut self, schema: &Schema) -> Result<SchemaDiff>
```

Entry naming: `"rule:NAME"`, `"view:NAME"`, `"fulltext:LABEL.FIELD"`.

- [ ] **Step 1: Failing test:**

```rust
#[test]
fn apply_schema_is_idempotent_and_diffs() {
    // schema with 1 fulltext pair, 1 rule, 1 view
    let d1 = db.apply_schema(&schema).unwrap();
    assert_eq!(d1.created.len(), 3); assert!(d1.unchanged.is_empty());
    let commits_before = /* WAL frame count via a reopen-free observable:
        use db.stats() commit counter if exposed, else fs wal length via
        wal_commit_count_at(dir) after a scoped drop — pick what exists */;
    let d2 = db.apply_schema(&schema).unwrap();
    assert_eq!(d2.unchanged.len(), 3); assert!(d2.created.is_empty() && d2.updated.is_empty());
    // zero WAL writes on second apply
    assert_eq!(commits_after, commits_before);
    // changed rule -> updated (delete+create), others unchanged
}
```

(Resolve the commit-count observable against reality first — `core_api::wal_commit_count_at(dir)` exists per the API survey; a scoped drop/reopen around it is acceptable in the test.)

- [ ] **Step 2: Run to fail.**

- [ ] **Step 3: Implement.** For each schema item: fulltext — `is_fulltext_enabled` ? unchanged : enable → created. Rule — find by name in `rules()`: absent → create (created); present and `PartialEq`-equal → unchanged (RuleDef needs `PartialEq` derive if missing); differing → `delete_rule` + `create_rule` (updated; doc-comment the re-backfill cost). View — same pattern via `views()`/`delete_view`/`create_view` (ViewDef PartialEq). Order: fulltext, then views, then rules (rules may backfill using fulltext state? they don't — order is cosmetic; pick and document). CLI subcommand: read JSON file → `serde_json::from_str::<Schema>` → open db → apply → print diff lines.

- [ ] **Step 4: Full suite.**

- [ ] **Step 5: Commit** `feat: idempotent apply_schema and cli schema apply`

---

### Task 5: node_history

**Files:**
- Create: `crates/core-api/src/history.rs`
- Modify: `crates/core-api/src/lib.rs` (export), `crates/core-api/src/db.rs` (`node_history`)
- Test: `crates/core-api/tests/history.rs` (new)

**Interfaces:**
- Consumes: `core_storage::wal::decode_all(bytes) -> (Vec<WalRecord>, usize)` (pub); WalRecord variants 0-17 incl. dense-id forms (InsertNodeId{label:u32,key,props:Vec<(u32,Value)>}, SetPropId{id,field:u32,value}, InsertEdgeId{etype,src,dst}, Intern{id,text}); `self.fs.read(FileId::Wal)`.
- Produces:

```rust
pub struct HistoryEntry { pub commit: u64, pub change: HistoryChange }
#[derive(Debug, PartialEq)]
pub enum HistoryChange {
    NodeInserted { label: String },
    PropSet { field: String, value: Value },
    PropRemoved { field: String },
    EdgeAdded { edge_type: String, other: String, outgoing: bool },
    EdgeRemoved { edge_type: String, other: String, outgoing: bool },
    NodeDeleted,
}
pub fn node_history(&self, key: &str) -> Result<Vec<HistoryEntry>>
```

- [ ] **Step 1: Failing test:** insert a → set prop → insert b → edge a->b → remove prop → delete edge → history("a") yields exactly [NodeInserted, PropSet, EdgeAdded{outgoing:true}, PropRemoved, EdgeRemoved] with strictly increasing commit indices; history("b") sees NodeInserted + EdgeAdded{outgoing:false}. Then `snapshot()` + one more mutation: history horizon = post-snapshot only (assert the old entries are gone and the doc'd horizon holds).

- [ ] **Step 2: Run to fail.**

- [ ] **Step 3: Implement.** Read WAL bytes via fs, `decode_all`. Scan frames with `commit = frame index` (Batch = one commit; recurse into inner records with the same commit index). Maintain scan-local tables: `interns: HashMap<u32, String>` (from Intern records), `ids: HashMap<u32, String>` + next-id counter reproducing `InsertNodeId` assignment order — BUT WAL-only scan cannot know pre-snapshot id bindings; therefore resolve dense ids as follows: seed `interns`/`ids` from the LIVE maps (`self.syms`, `self.ids` expose resolution — key_of/resolve are available inside core-api) and use live resolution for ids/interns not (re)bound during the scan; records referencing ids that resolve to other keys are skipped unless they match `key`. Matching: string-keyed records match on key fields; dense records match when the resolved key equals `key`. Edge records produce entries for BOTH endpoints (outgoing flag per side) — but node_history(key) only collects entries for `key`. Skip: rule/view/fulltext records, RebuildRule, Batch wrapper itself.
  Doc comment MUST carry the horizon paragraph and the derived-edges exclusion verbatim from the spec.

- [ ] **Step 4: Full suite.**

- [ ] **Step 5: Commit** `feat: node_history from wal scan`

---

### Task 6: python binding compile-sync

**Files:**
- Modify: `bindings/python/src/lib.rs` only.

**Interfaces:**
- Consumes: `Value::Map` (Task 1); `RuleDef.via_label/via_edge/via_dir` (already on main from phase 4).

- [ ] **Step 1:** In `py_to_value`: Python `dict` → `Value::Map` (recurse; keys must be str, else TypeError). In `value_to_py`: `Value::Map` → Python dict. In the RuleDef construction site(s): set `via_label: None, via_edge: None, via_dir: None` (no new Python API surface — compile-sync only, YAGNI).

- [ ] **Step 2:** `cd bindings/python && PATH=... cargo check --offline` — must compile clean. (Wheel build/maturin remains a separate manual step; note that in the report.)

- [ ] **Step 3: Commit** `fix: python binding compiles with map values and via fields`

---

## G18 gate

- Workspace suite green after every task; fmt + clippy clean.
- Map WAL+snapshot roundtrip with nesting; golden v5/v6/v7 pins green untouched.
- Mask test: hidden nodes/edges invisible via scan, lookup, and expand; write-under-mask rejected; no-mask tests untouched.
- Hybrid RRF fixture with exact expected scores; determinism pinned.
- apply_schema double-apply: all-unchanged + zero new WAL commits.
- node_history sequence + post-snapshot horizon pinned.
- `bindings/python` compiles (`cargo check`).
