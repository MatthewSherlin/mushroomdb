# Phase 2 — App query surface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Cypher paste-from-Neo4j work for the common app patterns, make HTTP writes safe on tokio, and stop `All` predicates from scanning because `parts[0]` is `VectorSimilar`.

**Architecture:** Three PRs after Gate G1. No storage rewrite. No new crates.

**Tech Stack:** Same workspace. Phase 1 `ScanKey` / token / default top-k already exist.

**Spec:** `docs/superpowers/specs/2026-08-25-best-graph-db.md` §4 Gate G2, §5 C11/C13/C14/A3

## Global Constraints

- Do **not** start this plan until G1 is green (`docs/superpowers/plans/2026-08-25-phase-1-trust-and-reach.md` Task 7).
- No new runtime crates.
- Do not implement `UNION`, `collect()`, `COUNT(DISTINCT)`, `CASE`, subqueries, unbounded variable-length.
- `FsyncPolicy::Strict` remains the default for single `insert_node` / `set_prop`. Ingest and `write_batch` use `Batched`.
- HTTP read path may stay on the worker for µs neighborhoods; **writes** must `spawn_blocking`.
- `All` index intersect must be a superset of true matches (never miss); extra candidates are OK.

---

## File map

| File | Role |
|---|---|
| `crates/core-query/src/cypher/{ast,lexer,parser,plan,exec}.rs` | `IN`, `DISTINCT`, SET…RETURN, MERGE ON CREATE |
| `crates/core-api/src/db.rs` | MERGE ON CREATE SET; MATCH SET RETURN in `query_write` |
| `crates/core-storage/src/fs.rs` | optional `sync` skip when policy is Relaxed |
| `crates/core-api/src/db.rs` | `FsyncPolicy` on `GraphDb`; group commit |
| `crates/server/src/http.rs` | `spawn_blocking` for write `query`/`ingest`/`rules`; `/health` already from Phase 1 — extend body |
| `crates/core-rules/src/index.rs` | `CandidateSpec::Intersect` |
| `crates/core-rules/src/engine.rs` | `compute_desired` uses intersect |

---

### Task 1: `IN`, `DISTINCT`, `MATCH … SET … RETURN`, MERGE ON CREATE SET

**Files:**
- Modify: `crates/core-query/src/cypher/ast.rs` (`Expr::In { expr: Operand, list: Vec<Operand> }`; `Query.distinct: bool`; `WriteStatement` SET/MERGE return items)
- Modify: `parser.rs` — today SET…RETURN is a named error (`parser.rs` ~1190); MERGE ON CREATE is rejected (~1128)
- Modify: `plan.rs`, `exec.rs`
- Modify: `crates/core-api/src/db.rs` `query_write`
- Test: `crates/core-api/tests/query.rs`, `crates/core-api/tests/cypher_writes.rs`

**Interfaces:**
- Consumes: Phase 1 `ScanKey`
- Produces:
  - `WHERE n.city IN ['a','b']` and `WHERE n.city IN $cities` (`$cities` is `Value::List`)
  - `RETURN DISTINCT n.city`
  - `MATCH (n {id:$k}) SET n.x = 1 RETURN n`
  - `MERGE (n:L {id:$k}) ON CREATE SET n.born = 1 RETURN n`
  - `ON MATCH SET` in the same MERGE
  - Still named-error: `UNION`, `CASE`, `collect()`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn where_in_list_and_param() {
    // three Person cities; WHERE n.city IN ['Austin', $c] with c='Paris'
}

#[test]
fn return_distinct_cities() {
    // two nodes Austin → one row
}

#[test]
fn match_set_return_same_statement() {
    let rs = db.query_write("MATCH (n {id:'a'}) SET n.x = 2 RETURN n.x", &map).unwrap();
    assert_eq!(rs.rows[0][0], Some(Value::Int(2)));
}

#[test]
fn merge_on_create_set() {
    db.query_write("MERGE (n:L {id:'new'}) ON CREATE SET n.born = 1 RETURN n", &Default::default()).unwrap();
    assert_eq!(db.node_info("new").unwrap().props.get("born"), Some(&Value::Int(1)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mushroomdb-core-api --test cypher_writes match_set_return_same_statement -- --nocapture`

Expected: FAIL — parse named error "combined MATCH…SET…RETURN is rejected".

- [ ] **Step 3: Implement**

Parser: allow `IN` as a comparison form. `DISTINCT` after `RETURN`. `MATCH … SET … RETURN` as one `WriteStatement` with optional `RetItem`s — execute write, then project from post-write view (WAL already committed; Phase 1 query_write already commits before CREATE RETURN).

MERGE: parse `ON CREATE SET` / `ON MATCH SET` clause lists. `query_write` already branches insert vs skip; apply the corresponding SETs inside the same `write_batch`.

`DISTINCT`: after `Project`, hash rows (`BTreeSet<Vec<Option<ValueKey>>>` with the same numeric unify as grouping). Cap distinct rows at `MAX_INTERMEDIATE_ROWS`.

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p mushroomdb-core-query
cargo test -p mushroomdb-core-api --test query
cargo test -p mushroomdb-core-api --test cypher_writes
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core-query crates/core-api docs/site/query.md
git commit -m "feat: Cypher IN, DISTINCT, MATCH SET RETURN, MERGE ON CREATE"
```

---

### Task 2: FsyncPolicy + ingest Batched + HTTP `spawn_blocking` + `/health` body

**Files:**
- Modify: `crates/core-api/src/db.rs` (`GraphDb.fsync: FsyncPolicy`)
- Modify: `crates/core-storage/src/fs.rs` only if `sync` needs to be skippable — prefer skipping the `self.fs.sync` call in `log_then_apply_with` based on policy
- Modify: `crates/core-api/src/ingest.rs` / `commit_logged_batch` to use Batched
- Modify: `crates/server/src/http.rs` write handlers
- Modify: `GET /health` body
- Modify: `crates/cli/src/lib.rs` (`Command::Serve` gains `snapshot_every: Option<Duration>`)
- Test: `crates/core-api/tests/batches.rs`, `crates/server/tests/http.rs`, `crates/cli/src/lib.rs` parse tests

**Interfaces:**
- Produces:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsyncPolicy {
    Strict,   // every log_then_apply_with calls fs.sync (today)
    Batched,  // sync only at Batch frame end; single-op path still Strict unless set
    Relaxed,  // never sync; snapshot() still syncs via write_atomic
}

impl GraphDb<F> {
    pub fn set_fsync_policy(&mut self, p: FsyncPolicy);
}
```

Ingest / `write_batch` / `query_write` already write one Batch frame — under Strict that is one sync (already). **Batched** means: consecutive single-op `insert_node` calls from `ingest` that currently fsync per node use one Batch (already true for `ingest_batch`). HTTP `/ingest` must use `ingest` batch, not a loop of `insert_node`. Verify and fix if `/ingest` loops.

HTTP: wrap `state.db.write()` / `query_write` / `ingest` / `create_rule` in `tokio::task::spawn_blocking`. Drop guards before `.await`. Reads stay inline (Phase 1 neighborhood is µs). `suggest` and `algo` already spawn_blocking.

Serve: `--snapshot-every <secs>` (omit = off). A tokio interval task takes the write lock, calls `snapshot()`, logs errors, continues. Default off so tests do not write surprise snapshots.

`/health` JSON:

```json
{"ok": true, "nodes": 12, "edges": 40, "addr": "127.0.0.1:8080"}
```

`nodes`/`edges` from `stats()` under a read lock, spawn_blocking optional.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn relaxed_policy_skips_sync_but_snapshot_is_durable() {
    // count SimFs sync calls if FsIntrospect exists; else skip this on RealFs
}

#[tokio::test]
async fn health_reports_counts() {
    // GET /health → ok + nodes
}
```

If SimFs does not count `sync`, add `sync_count` on `SimFs` (test-only introspect) — that is in-scope.

- [ ] **Step 2: Run tests to verify they fail**

Expected: `FsyncPolicy` missing; `/health` is `{"ok":true}` only.

- [ ] **Step 3: Implement** as specified.

- [ ] **Step 4: Run tests**

```
cargo test -p mushroomdb-core-api --test batches
cargo test -p mushroomdb-server --test http
cargo test -p mushroomdb-sim-harness --test crash_recovery -- --test-threads=1
```

Relaxed must **not** be used in DST crash sweeps (those stay Strict).

- [ ] **Step 5: Commit**

```bash
git commit -m "feat: fsync policy, spawn_blocking writes, health counts"
```

---

### Task 3: `All` candidate index intersect

**Files:**
- Modify: `crates/core-rules/src/index.rs` (`CandidateSpec`, `candidate_spec`)
- Modify: `crates/core-rules/src/engine.rs` `compute_desired`
- Test: `crates/core-rules/tests/predicate_fuzz.rs` (keep); new `crates/core-api/tests/rules.rs` case

**Interfaces:**

```rust
CandidateSpec::Intersect(Vec<CandidateSpec<'a>>)
```

`candidate_spec(All(parts))` → `Intersect(parts.iter().map(candidate_spec))` instead of `parts[0]`.

`SideIndex::candidates` for Intersect: compute each child set, intersect (`BTreeSet` intersection). Empty child → empty.

`ScanAll` in an Intersect: skip it as a child (it would be the universe); if **all** children are ScanAll, then ScanAll.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn all_vector_then_field_equal_does_not_scan_all() {
    // All(VectorSimilar, FieldEqual{industry})
    // create_rule exact (approximate false)
    // candidate_spec is Intersect([ScanAll, Scalar])
    // a node with matching industry but cosine below min does not get an edge
    // a node with cosine above min but different industry does not get an edge
    // both match → edge
}
```

Pin via `candidate_spec` unit test:

```rust
let p = Predicate::All(vec![
    Predicate::VectorSimilar { field: "e".into(), min: 0.8 },
    Predicate::FieldEqual { field: "industry".into() },
]);
match candidate_spec(&p) {
    CandidateSpec::Intersect(v) => assert_eq!(v.len(), 2),
    other => panic!("{other:?}"),
}
```

- [ ] **Step 2: Run to fail** — today `All` delegates to `parts[0]` → `ScanAll`.

- [ ] **Step 3: Implement** Intersect as specified. `Any` stays Union.

- [ ] **Step 4: Tests**

```
cargo test -p mushroomdb-core-rules
cargo test -p mushroomdb-core-api --test rules
cargo test -p mushroomdb-sim-harness --test oracle_equivalence -- --test-threads=1
```

- [ ] **Step 5: Commit**

```bash
git commit -m "feat: All predicates intersect indexes instead of using parts[0]"
```

---

## G2 gate

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

Do not start Phase 3 until this is green.
