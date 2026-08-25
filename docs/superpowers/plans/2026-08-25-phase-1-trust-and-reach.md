# Phase 1 — Trust and reach Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make mushroomdb safe to point at a network, honest in its docs, usable from Python/TS/CLI/MCP with one argument order, and safe at default rule settings — without a storage rewrite or full Cypher.

**Architecture:** Seven independently reviewable PRs (Tasks 1–7). No new crates. Pre-1.0 API breaks are allowed (Python `insert_node` argument order; JSON omit-`max_edges` now means 32/1 not uncapped). Storage, HNSW, 3-node rules, and Cypher `IN`/`DISTINCT` are out of scope.

**Tech Stack:** Existing workspace: Rust 1.92, axum, PyO3, TypeScript `fetch` client, Vite UI, SimFs DST.

**Spec:** `docs/superpowers/specs/2026-08-25-best-graph-db.md`

## Global Constraints

- Rust 1.92.0 (`rust-toolchain.toml`); `cargo fmt`, `clippy --all-targets -- -D warnings`, `cargo test --workspace` green after every task.
- No new runtime crates. `libc` is allowed only if Darwin `F_FULLFSYNC` cannot be called through `std` (it cannot — use `libc::fcntl` behind `#[cfg(target_os = "macos")]`).
- Generality Guarantee: no engine behavior keyed off `Person`/`Org`/etc. Demo and tests may use those labels.
- WAL discriminants: do not reorder `WalRecord` variants. `RebuildRule` already exists (discriminant 9).
- `max_edges: None` in Rust remains the 1_000_000 global first-N-by-id budget. Do not change `DEFAULT_MAX_EDGES`.
- Default scored top-k is `32`; KeyMatch top-k is `1` (`docs/superpowers/specs/2026-08-25-best-graph-db.md` §7).
- Loopback HTTP needs no token; non-loopback requires `--token` or `MUSHROOMDB_TOKEN`.
- `GraphDb` Drop does **not** snapshot.
- Do not start Phase 2–4 work in this plan.
- Named errors stay prefixed (`lex:` / `parse:` / `plan:` / `execute:`).

---

## File map (locked)

| File | Role this phase |
|---|---|
| `crates/core-storage/src/fs.rs` | dir fsync after rename; Darwin `F_FULLFSYNC` |
| `crates/core-storage/src/types.rs` | no change unless a new error is required for bind/token (that lives in CLI/server) |
| `crates/core-rules/src/def.rs` | `DEFAULT_SCORED_TOP_K`, `DEFAULT_KEYMATCH_TOP_K`; helper `default_max_edges(&Predicate) -> u64` |
| `crates/core-rules/src/suggest.rs` | never emit `None`; preview under cap |
| `crates/core-rules/src/index.rs` | L2-normalize before k-means |
| `crates/core-rules/src/engine.rs` | drift auto-rebuild inside apply |
| `crates/core-rules/src/views.rs` | Count = neighbors with property present |
| `crates/core-api/src/ingest.rs` | auto-FK `max_edges: Some(1)` |
| `crates/core-api/src/db.rs` | JSON/HTTP omit filling; shutdown not here |
| `crates/core-query/src/cypher/plan.rs` | `ScanKey`; right-to-left compile when dest bound |
| `crates/core-query/src/cypher/exec.rs` | execute `ScanKey`; test counter |
| `crates/server/src/http.rs` | bearer/query token; `/health` **not** in this phase (Phase 2) — but token exemption list must include a future `/health`. Exempt nothing in Phase 1 except we add `/health` as a stub 200 in Task 1 so Docker probes work. Spec §8: `GET /health` is exempt. **Add a minimal `/health` in Task 1.** |
| `crates/server/src/mcp.rs` | `query` → `query_write` when `is_write_query` |
| `crates/cli/src/lib.rs` | default `:8080`, `--token`, `query`, `snapshot`, shutdown snapshot |
| `crates/cli/src/main.rs` | dispatch new commands; Ctrl-C snapshot |
| `bindings/python/src/lib.rs` | `insert_node(label, key, props)`; omit `max_edges` fills default |
| `clients/typescript/src/{client,types}.ts` | `explain`, `createRule`, `node`, `neighborhood`; optional token |
| `Dockerfile` | document `MUSHROOMDB_TOKEN`; keep `0.0.0.0` (binary will refuse without token) |
| `docs/design.md`, `README.md`, `docs/site/*.md` | honesty + API parity |

---

### Task 1: Honesty + bind/token + `/health` stub + fsync

**Files:**
- Modify: `docs/design.md` §3 storage/execution/concurrency/bindings/testing rows and §10 open-time target
- Modify: `README.md` Architecture paragraph ("zero-copy archived format"), Known limitations, Roadmap
- Modify: `docs/site/index.md` (delete "no node/edge deletes"; fix `radius_km` → `km`; remove `dims` from VectorSimilar wire shape)
- Modify: `docs/site/query.md` opener ("read-only Cypher subset" → document writes)
- Modify: `docs/site/api.md` MCP tool count (11, not 8); `/watch` event shape
- Modify: `docs/site/rules.md` FieldEqual is any `ValueKey`, not string-only; GeoRadius field is `km`
- Modify: `SECURITY.md` non-loopback requires token
- Modify: `crates/core-storage/src/fs.rs`
- Modify: `crates/server/src/http.rs`, `crates/server/src/lib.rs` (pass token into router)
- Modify: `crates/cli/src/lib.rs` (`Command::Serve` gains `token: Option<String>`; `default_addr` port 8080)
- Modify: `crates/cli/src/main.rs`
- Modify: `Dockerfile` (comment + ENV)
- Test: `crates/core-storage/src/fs.rs` (unit); `crates/cli/src/lib.rs` parse tests; `crates/server/tests/http.rs`

**Interfaces:**
- Consumes: existing `RealFs`, `router(db)`
- Produces:
  - `RealFs::sync` uses `F_FULLFSYNC` on macOS
  - `write_atomic` fsyncs the parent directory after `rename`
  - `pub fn router_with_auth(db: SharedDb, token: Option<String>) -> Router` — existing `router(db)` calls `router_with_auth(db, None)`
  - `GET /health` → `{"ok": true}` JSON, no auth
  - `default_addr()` = `127.0.0.1:8080`
  - Serve refuses to start if `addr.ip()` is not loopback and `token` is `None`/empty

- [ ] **Step 1: Write the failing tests**

In `crates/cli/src/lib.rs` parse tests, add:

```rust
#[test]
fn serve_default_addr_is_loopback_8080() {
    match parse_args(&["serve", "/tmp/db"]).unwrap() {
        Command::Serve { addr, .. } => {
            assert_eq!(addr, "127.0.0.1:8080".parse().unwrap());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn serve_token_flag_and_non_loopback_without_token_is_parsed() {
    // parse succeeds; main() enforces the bind rule. Token is stored.
    match parse_args(&["serve", "/tmp/db", "--addr", "0.0.0.0:8080", "--token", "s3cret"]).unwrap() {
        Command::Serve { token, addr, .. } => {
            assert_eq!(token.as_deref(), Some("s3cret"));
            assert_eq!(addr.ip().to_string(), "0.0.0.0");
        }
        other => panic!("{other:?}"),
    }
}
```

In `crates/server/tests/http.rs`, add:

```rust
#[tokio::test]
async fn health_is_unauthenticated() {
    // boot with token Some("t"); GET /health must 200 without Authorization
}

#[tokio::test]
async fn query_without_bearer_is_401_when_token_configured() {
    // POST /query with no header → 401 {"error":"..."}
}

#[tokio::test]
async fn query_with_bearer_succeeds_when_token_configured() {
    // Authorization: Bearer t → 200
}
```

In `crates/core-storage/src/fs.rs` tests, add:

```rust
#[test]
fn write_atomic_replaces_and_still_readable() {
    // existing append_read_and_atomic_write already covers replace;
    // keep it; add a comment that dir-sync is best-effort observable
    // only via crash tests. Do not fake F_FULLFSYNC in SimFs.
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mushroomdb-cli --lib serve_default_addr_is_loopback_8080 -- --nocapture`

Expected: FAIL — default port is still 0.

Run: `cargo test -p mushroomdb-server --test http health_is_unauthenticated -- --nocapture`

Expected: FAIL — `/health` 404.

- [ ] **Step 3: Implement**

`fs.rs` `sync` / `write_atomic`:

```rust
fn full_sync(file: &File) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
        if rc == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        file.sync_all()
    }
}

fn sync_dir(dir: &std::path::Path) -> std::io::Result<()> {
    let d = File::open(dir)?;
    d.sync_all()
}
```

After `rename` in `write_atomic`, call `sync_dir(&self.dir)`. Use `full_sync` instead of `sync_all` on the tmp file and on `sync()`.

Add `libc` to `crates/core-storage/Cargo.toml` with default features off if possible: `libc = "0.2"`.

CLI: change `default_addr` to port 8080. Add `--token` / `--token=` parsing. Read `MUSHROOMDB_TOKEN` in `main.rs` if flag absent (`std::env::var("MUSHROOMDB_TOKEN").ok().filter(|s| !s.is_empty())`).

`main.rs` before `run_serve`:

```rust
if !addr.ip().is_loopback() && token.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
    return fail(
        "non-loopback --addr requires --token or MUSHROOMDB_TOKEN \
         (see SECURITY.md)"
    );
}
```

HTTP: store `token: Option<String>` on `AppState`. Middleware: if `token` is `Some`, skip check for `GET /health`; else require `Authorization: Bearer …` or `?token=`. WS upgrades on `/watch` and `/subscribe` must read `?token=`.

`GET /health` returns `{"ok":true}`.

Honesty edits: replace "zero-copy archived format" with "zstd-compressed bincode snapshot (V6); not mmap". Rewrite design.md locked-decision rows that claim Sortledton/mmap/epoch/vectorized/napi-rs/Neo4j differential to "deferred, see `docs/superpowers/specs/2026-08-25-best-graph-db.md`".

Dockerfile: keep `CMD ["serve", "/data", "--addr", "0.0.0.0:8080", "--demo-if-empty"]`. Add:

```
# Non-loopback bind requires a token. Set at run:
#   docker run -e MUSHROOMDB_TOKEN=… -p 8080:8080 …
ENV MUSHROOMDB_TOKEN=""
```

- [ ] **Step 4: Run tests to verify they pass**

Run:

```
cargo test -p mushroomdb-cli --lib
cargo test -p mushroomdb-server --test http
cargo test -p mushroomdb-storage
cargo fmt --check && cargo clippy --all-targets -- -D warnings
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add docs crates/core-storage crates/server crates/cli Dockerfile SECURITY.md README.md
git commit -m "fix: honesty pass, loopback default :8080, token on non-loopback, durable fsync"
```

---

### Task 2: Client and docs parity (Python order, TS methods, MCP writes)

**Files:**
- Modify: `bindings/python/src/lib.rs` (`insert_node` args)
- Modify: `bindings/python/tests/test_basic.py` and every `insert_node(` call
- Modify: `bindings/python/README.md`
- Modify: `clients/typescript/src/types.ts`, `clients/typescript/src/client.ts`
- Test: `clients/typescript/tests/integration.test.ts`
- Modify: `crates/server/src/mcp.rs` `tool_query`
- Test: `crates/server/src/mcp.rs` unit tests + `crates/server/tests/mcp.rs`
- Modify: `docs/site/mcp.md`, `docs/site/api.md`, `docs/site/query.md` (ASCII `toLower`/`size` note), `crates/core-query/src/cypher/exec.rs` unknown-fn list includes `textMatches`

**Interfaces:**
- Consumes: `GraphDb::insert_node(label, key, props)`, `is_write_query`, `query_write`
- Produces:
  - Python `insert_node(self, label: str, key: str, props: dict)`
  - `MushroomClient.explain(a: string, b: string): Promise<Explanation[]>`
  - `MushroomClient.createRule(def: RuleDef): Promise<void>`  // POST /rules
  - `MushroomClient.node(key: string): Promise<NodeInfo | null>`
  - `MushroomClient.neighborhood(key: string, opts?: { depth?: number }): Promise<Neighborhood>`
  - MCP `query` uses write lock + `query_write` when `is_write_query` is true
  - Unknown scalar function error string contains `textMatches`

- [ ] **Step 1: Write the failing tests**

Python (`bindings/python/tests/test_basic.py`):

```python
def test_insert_node_label_then_key(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Org", "org-01", {"founded_year": 2010})
    info = db.node_info("org-01")
    assert info["label"] == "Org"
    db.close()
```

Flip every existing `insert_node("org-01", "Org", …)` to `insert_node("Org", "org-01", …)` in the same change as the implementation so the suite does not have a red window in CI — but **first** run the new test against current wheels to see `label="org-01"` (wrong). When implementing, the new test is the pin.

MCP: in `crates/server/src/mcp.rs` tests:

```rust
#[test]
fn test_query_create_is_a_write() {
    // tools/call query cypher: "CREATE (n:L {id: 'k'}) RETURN n"
    // after call, stats.nodes_live == 1
}
```

TS: add types and a unit/integration test that `createRule` POSTs `/rules` (mock if the integration harness already boots a server — `clients/typescript/tests/integration.test.ts`).

Unknown-fn: existing query test that asks for `toLower` — add:

```rust
#[test]
fn unknown_function_lists_text_matches() {
    let err = db.query("MATCH (n) RETURN nosuch(n)", &Default::default()).unwrap_err();
    let s = err.to_string();
    assert!(s.contains("textMatches"), "{s}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mushroomdb-server --lib test_query_create_is_a_write -- --nocapture`

Expected: FAIL — CREATE through MCP `query` errors (read lock / not a write).

- [ ] **Step 3: Implement**

Python:

```rust
fn insert_node(&self, label: &str, key: &str, props: Bound<'_, PyDict>) -> PyResult<()> {
    let mapped = dict_to_props(&props)?;
    self.with_mut(|db| db.insert_node(label, key, mapped))
}
```

MCP `tool_query`:

```rust
let is_write = match core_api::is_write_query(cypher) {
    Ok(b) => b,
    Err(e) => return CallOutcome::ToolErr(e),
};
let rs = if is_write {
    let mut g = db.write();
    g.query_write(cypher, &params)
} else {
    let g = db.read();
    g.query(cypher, &params)
};
```

TS: add methods mirroring HTTP (`GET /explain?a=&b=`, `POST /rules`, `GET /node/{key}`, `GET /node/{key}/neighborhood`). Pass `Authorization` header when `MushroomClient` is constructed with `{ token?: string }`.

`exec.rs`: include `textMatches` in the supported-function list string used by the unknown-fn error.

Docs: Python README examples use `(label, key)`. MCP docs say `query` runs writes. `query.md` scalar functions: "ASCII casefold; `size(str)` is bytes."

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p mushroomdb-server --lib
cargo test -p mushroomdb-core-api --test query
cd bindings/python && .venv/bin/pytest
cd clients/typescript && npm test
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add bindings/python clients/typescript crates/server crates/core-query docs
git commit -m "fix: python insert_node(label,key), MCP writes, TS explain/rules/node"
```

---

### Task 3: Default top-k + suggest + auto-FK + View Count

**Files:**
- Modify: `crates/core-rules/src/def.rs`
- Modify: `crates/core-rules/src/suggest.rs` (every `RuleDef { … max_edges: None }`)
- Modify: `crates/core-rules/src/views.rs` (`AggFn::Count` arms at ~521 and ~617)
- Modify: `crates/core-api/src/ingest.rs` auto-FK `max_edges: Some(1)`
- Modify: `crates/cli/src/lib.rs` demo `create_rule` max_edges
- Modify: `crates/server/src/json.rs` (or `http.rs` create_rule) so omitted JSON `max_edges` fills the default
- Test: `crates/core-api/tests/suggest.rs`, `crates/core-api/tests/ingest.rs`, `crates/core-api/tests/views.rs`, `crates/core-rules/tests/` if present
- Modify: DST `scratch_view_value` path in `crates/core-rules/src/views.rs` `compute_view_value` (single function — tests will follow)

**Interfaces:**
- Consumes: `Predicate`, `RuleDef`
- Produces:

```rust
pub const DEFAULT_SCORED_TOP_K: u64 = 32;
pub const DEFAULT_KEYMATCH_TOP_K: u64 = 1;

pub fn default_max_edges(predicate: &Predicate) -> u64 {
    if is_keymatch_rooted(predicate) {
        DEFAULT_KEYMATCH_TOP_K
    } else {
        DEFAULT_SCORED_TOP_K
    }
}
```

`is_keymatch_rooted` matches `KeyMatch` or `All` whose first element is `KeyMatch` (same shape as `predicate_is_keymatch` in `engine.rs` — extract a shared fn in `def.rs` if that avoids duplication).

Suggest + auto-FK + demo + HTTP omit → `Some(default_max_edges(&pred))`.

View Count: `neighbors.iter().filter(|&&n| props.get(n, prop).is_some()).count()`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn suggest_never_emits_uncapped_max_edges() {
    // ingest a small two-label graph with overlapping tags
    let s = db.suggest_rules();
    assert!(!s.is_empty());
    for row in s {
        assert!(row.def.max_edges.is_some(), "{}", row.def.name);
    }
}

#[test]
fn auto_fk_keymatch_is_top_1() {
    // existing ingest fixture; after ingest, rule auto_fk_person_org_id
    let r = db.rules().into_iter().find(|r| r.name == "auto_fk_person_org_id").unwrap();
    assert_eq!(r.max_edges, Some(1));
}

#[test]
fn neighbor_count_skips_missing_prop() {
    // City c1 with two LIVES_IN people: one has `weight`, one does not
    // View NeighborAgg Count on `weight` → 1, not 2
    // Degree view on same etype → 2
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mushroomdb-core-api --test suggest -- --nocapture`

Expected: FAIL — `max_edges` is `None`.

Run: `cargo test -p mushroomdb-core-api --test views neighbor_count_skips_missing_prop -- --nocapture`

Expected: FAIL — Count == 2.

- [ ] **Step 3: Implement**

Replace every suggest `max_edges: None` with `Some(default_max_edges(&pred))`. Same for auto-FK (`Some(1)` is equivalent). Demo scored rules `Some(32)`, auto-FK demo rules already come from ingest.

HTTP `POST /rules`: if JSON `max_edges` is null or absent, set `Some(default_max_edges(&def.predicate))` **after** deserialize. Use `#[serde(default)]` on a wrapper or fill in the handler. Do **not** add `#[serde(default)]` on `RuleDef.max_edges` for bincode — missing positional field must keep failing for old bytes. HTTP is serde_json.

Python `create_rule`: if dict key `max_edges` is missing or `None`, fill `default_max_edges`. **Change from today:** today's Python test passes `"max_edges": None` meaning uncapped. Keep that: Python `None` → Rust `None` (escape hatch). Missing key → default. Update `test_round_trip_numeric_within` to either pass `32` or omit the key (omit → 32; Org-Org numeric will still fire both directions within 32).

View Count arms: count present props. `compute_view_value` / incremental path must agree.

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p mushroomdb-core-api --test suggest
cargo test -p mushroomdb-core-api --test ingest
cargo test -p mushroomdb-core-api --test views
cargo test -p mushroomdb-core-rules
cargo test -p mushroomdb-sim-harness --test oracle_equivalence -- --test-threads=1
```

Expected: PASS. Oracle Count fixtures that assumed Count==Degree must be updated to the spec (neighbors with property). If a fixture has no missing props, counts stay equal — only add a case with a missing prop.

- [ ] **Step 5: Commit**

```bash
git add crates/core-rules crates/core-api crates/cli crates/server bindings/python
git commit -m "fix: default top-k on suggest/auto-fk, view count skips missing props"
```

---

### Task 4: IVF cosine-normalize + drift auto-rebuild

**Files:**
- Modify: `crates/core-rules/src/index.rs` (`kmeans_fit` / fit call site)
- Modify: `crates/core-rules/src/engine.rs` (`on_node_changed` / `on_node_removed` after index update)
- Modify: `crates/core-api/src/db.rs` apply path if rebuild must be WAL-logged as `RebuildRule` inside the same Batch
- Test: `crates/core-rules` unit tests; `crates/core-api/tests/rules.rs`; crash recovery already checks recall floor

**Interfaces:**
- Consumes: `kmeans_fit`, `ivf_dst_drift`, `RebuildRule`
- Produces:
  - `pub const IVF_DRIFT_REBUILD: u64 = 256;`
  - Vectors passed to `kmeans_fit` are L2-normalized (skip zero vectors)
  - When dst-side drift exceeds 256 on an `approximate` rule, apply logs `WalRecord::RebuildRule { name }` as part of the **same** Batch as the triggering op when the caller used `log_then_apply` for a single op: rewrite that path to `Batch(vec![original, RebuildRule])` **or** append RebuildRule in `apply` only if it is already inside a Batch. Simplest correct approach: after `on_node_changed`, if drift > threshold, call the in-memory rebuild (same as `rebuild_rule` apply arm) **and** the caller `log_then_apply_with` prepends/appends `RebuildRule` by converting a single record into `Batch(vec![rec, RebuildRule])` **before** the WAL write. That requires peeking drift **before** WAL — drift is a consequence of apply. So: WAL the user op, apply, if drift tripped, WAL+apply `RebuildRule` as a second commit. Two fsyncs on the rare rebuild path. Document that. **Do not** invent a look-ahead.

  Spec said "prefer one Batch". Pre-WAL we do not know drift. **Lock for this task:** two commits (user op, then RebuildRule) on the rebuild path. Replay is still deterministic. Update spec note in the commit message.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn kmeans_centroids_are_unit_norm() {
    let vecs = vec![(0, vec![3.0, 0.0, 0.0]), (1, vec![0.0, 4.0, 0.0])];
    let cents = kmeans_fit(&vecs, 2, 1);
    for c in cents {
        let n = c.iter().map(|x| x * x).sum::<f64>().sqrt();
        assert!((n - 1.0).abs() < 1e-9, "{n}");
    }
}

#[test]
fn approximate_rule_rebuilds_after_drift_threshold() {
    // create approximate VectorSimilar rule on a few nodes
    // delete/remove_prop enough dst vectors to push drift > 256
    // (construct with a small test override)
}
```

Add a `#[cfg(test)]` override `with_ivf_drift_rebuild(1, || { … })` analogous to `with_max_intermediate_rows` so the test does not need 257 deletes.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mushroomdb-core-rules kmeans_centroids_are_unit_norm -- --nocapture`

Expected: FAIL — centroids inherit raw scale.

- [ ] **Step 3: Implement**

In `kmeans_fit` (or immediately before, at the IVF fit call site), normalize:

```rust
fn l2_normalize(xs: &[f64]) -> Option<Vec<f64>> {
    let n = xs.iter().map(|x| x * x).sum::<f64>().sqrt();
    if n == 0.0 { return None; }
    Some(xs.iter().map(|x| x / n).collect())
}
```

Skip pairs that fail normalize. If after skip `vecs` is empty, return `vec![]` (existing empty path).

Drift: after index maintenance in `on_node_changed`/`on_node_removed`, if `approximate && dst_drift > threshold`, set a flag `engine.take_rebuild_needed() -> Vec<String>`. `log_then_apply_with` after successful apply, for each name, `log_then_apply(WalRecord::RebuildRule { name })` — **nested log_then_apply is re-entrant WAL**. Avoid recursion: collect names, then for-loop `log_then_apply` after the outer function returns to `insert_node`/`set_prop`. Cleaner: `log_then_apply_with` at the end:

```rust
let rebuilds = self.engine.take_rebuild_needed();
// distribute events for `rec` first (existing)
for name in rebuilds {
    self.log_then_apply(WalRecord::RebuildRule { name })?;
}
```

Guard against RebuildRule itself triggering another rebuild (rebuild resets drift). Test that.

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p mushroomdb-core-rules
cargo test -p mushroomdb-core-api --test rules
cargo test -p mushroomdb-sim-harness --test crash_recovery -- --test-threads=1
```

Expected: PASS. Recall floors in crash_recovery still hold.

- [ ] **Step 5: Commit**

```bash
git add crates/core-rules crates/core-api
git commit -m "fix: cosine-normalize IVF centroids; auto rebuild_rule on drift"
```

---

### Task 5: `snapshot` CLI + serve shutdown snapshot

**Files:**
- Modify: `crates/cli/src/lib.rs` (`Command` variants `Query`, `Snapshot`)
- Modify: `crates/cli/src/main.rs`
- Modify: `crates/core-api/src/db.rs` only if a `try_snapshot_on_drop` is tempting — **do not add Drop snapshot**
- Test: `crates/cli/src/lib.rs` parse tests; a small integration test that `run_snapshot` writes `snapshot.bin`

**Interfaces:**
- Consumes: `GraphDb::snapshot`, `GraphDb::query`, `query_write`, `is_write_query`
- Produces:

```rust
Command::Snapshot {
    db_dir: PathBuf,
    keep_wal: bool, // --keep-wal
}
Command::Query {
    db_dir: PathBuf,
    cypher: String, // positional after dir, or --query
}
```

`mushroomdb serve` installs a Ctrl-C handler: on SIGINT/SIGTERM, `db.write().snapshot()`, then exit 0. If snapshot fails, print the error and still exit 1 after attempting to leave the process.

Default snapshot truncates WAL (existing `snapshot()`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn parse_snapshot_and_query() {
    match parse_args(&["snapshot", "/tmp/db"]).unwrap() {
        Command::Snapshot { keep_wal, .. } => assert!(!keep_wal),
        other => panic!("{other:?}"),
    }
    match parse_args(&["snapshot", "/tmp/db", "--keep-wal"]).unwrap() {
        Command::Snapshot { keep_wal, .. } => assert!(keep_wal),
        other => panic!("{other:?}"),
    }
    match parse_args(&["query", "/tmp/db", "MATCH (n) RETURN n LIMIT 1"]).unwrap() {
        Command::Query { cypher, .. } => assert!(cypher.contains("MATCH")),
        other => panic!("{other:?}"),
    }
}
```

Usage string must list `query` and `snapshot`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mushroomdb-cli --lib parse_snapshot_and_query -- --nocapture`

Expected: FAIL — `unknown command: snapshot`.

- [ ] **Step 3: Implement**

Parse `query <dir> <cypher…>`: join remaining args with spaces as the cypher string (so quotes are optional in argv). `is_write_query` then `query_write` vs `query`. Print columns and rows using the same formatter as `asof`.

`snapshot <dir> [--keep-wal]`: `GraphDb::open` then `snapshot()` or `snapshot_with(SnapshotOptions { keep_wal: true })`.

Serve: use `tokio::signal::ctrl_c()` in `run_serve` next to the server future; on signal, drop the server, `db.write().snapshot()?`, return Ok.

Print listening URL: after `ready` channel, `println!("listening on http://{addr}")` — already exists; confirm it shows `:8080`.

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p mushroomdb-cli --lib
cargo test -p mushroomdb-core-api --test snapshot
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/cli
git commit -m "feat: mushroomdb query/snapshot CLI; serve snapshots on shutdown"
```

---

### Task 6: Planner `ScanKey` + expand-from-bound

**Files:**
- Modify: `crates/core-query/src/cypher/plan.rs` (`PlanOp`, `compile_pattern`, `row_bound` match)
- Modify: `crates/core-query/src/cypher/exec.rs` (every `match plan[0]` / `PlanOp` exhaustive match: staged, pull, check_params, fused scan)
- Test: `crates/core-api/tests/query.rs` (or `crates/core-query` unit tests if they exist for `plan()`)
- Modify: `crates/core-query/src/cypher/exec.rs` add `SCAN_KEY_FIRES` atomic like `FUSED_SCAN_FIRES`

**Interfaces:**
- Consumes: `GraphView::node_id`, `Operand`, `NodePat.props`
- Produces:

```rust
PlanOp::ScanKey {
    var: String,
    key: Operand,
    label: Option<String>,
}
```

Planner helper:

```rust
fn id_lookup(props: &[(String, Operand)]) -> Option<&Operand> {
    if props.len() == 1 && props[0].0 == "id" {
        Some(&props[0].1)
    } else {
        None
    }
}
```

When compiling an unbound start node with `id_lookup(&pat.start.props)` → emit `ScanKey` instead of `ScanLabel`+`LookupProps`.

Expand-from-bound: before the current left-to-right loop, if `!bound.contains(start) && chain.last().map(|(_, dest)| dest.var.as_ref().map(|v| bound.contains(v)).unwrap_or(false)) == Some(true)` and `chain.len() == 1` (v1: single-rel patterns only), reverse: treat dest as start, invert `RelDir` (Right↔Left; Undirected stays), then Expand toward the old start.

`RelDir` invert:

```rust
fn invert(d: RelDir) -> RelDir {
    match d {
        RelDir::Right => RelDir::Left,
        RelDir::Left => RelDir::Right,
        RelDir::Either => RelDir::Either,
    }
}
```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn plan_id_map_is_scan_key() {
    let toks = lex("MATCH (n:Person {id: $k}) RETURN n").unwrap();
    let q = parse(&toks).unwrap();
    let ops = plan(&q).unwrap();
    assert!(matches!(ops[0], PlanOp::ScanKey { .. }), "{ops:?}");
}

#[test]
fn plan_expands_from_bound_key() {
    let cy = "MATCH (t:Talent {id: $tid}) MATCH (c:Company)-[i:INDUSTRY_ALIGNMENT]->(t) RETURN c";
    let ops = plan(&parse(&lex(cy).unwrap()).unwrap()).unwrap();
    // first: ScanKey t; then Expand from t, dir Left (inbound)
    assert!(matches!(&ops[0], PlanOp::ScanKey { var, .. } if var == "t"));
    match &ops[1] {
        PlanOp::Expand { from, dir, to, .. } => {
            assert_eq!(from, "t");
            assert_eq!(to, "c");
            assert_eq!(*dir, RelDir::Left);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn scan_key_exec_does_not_use_label_scan() {
    // 3 Person nodes; MATCH (n:Person {id: 'p2'}) RETURN n
    // SCAN_KEY_FIRES increments; result one row
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p mushroomdb-core-query plan_id_map_is_scan_key -- --nocapture`

Expected: FAIL — `ops[0]` is `ScanLabel`.

- [ ] **Step 3: Implement**

Add variant. Fix every exhaustive match (compiler will list them). Pull path: `ScanKey` resolves param/lit to string, `view.node_id`, label check, push one row. Missing key / wrong label → zero rows, not an error.

`row_bound`: `ScanKey` is compatible with pull LIMIT (like `ScanLabel`).

`check_params`: walk `ScanKey.key` for `Operand::Param`.

Do not change `{id: $k, name: 'x'}` — still ScanLabel+LookupProps.

- [ ] **Step 4: Run tests to verify they pass**

```
cargo test -p mushroomdb-core-query
cargo test -p mushroomdb-core-api --test query
cargo test -p mushroomdb-sim-harness --test query_equivalence -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core-query crates/core-api
git commit -m "feat: Cypher ScanKey point lookup and expand-from-bound"
```

---

### Task 7: G1 gate — docs leftover, demo, usage, full workspace

**Files:**
- Modify: `crates/cli/src/lib.rs` `usage()` (port 8080, query, snapshot, --token)
- Modify: `README.md` Quickstart (`serve` URL)
- Modify: `docs/site/quickstart.md`, `docs/site/suggest.md` (defaults)
- Modify: `docs/site/views.md` if Count examples need a missing-prop note
- Test: no new tests; run the gates

**Interfaces:**
- Consumes: Tasks 1–6
- Produces: G1 green

- [ ] **Step 1: Grep for stale claims**

```
rg -n "zero-copy|Sortledton|napi-rs|read-only Cypher|no node/edge deletes|127.0.0.1:0|vectorized batches" README.md docs crates/cli
```

Fix any remaining hits that are not historical (CHANGELOG/release-notes may keep past tense).

- [ ] **Step 2: Run the full gates**

```
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --workspace --examples
cargo test --workspace
cargo bench --no-run
```

Node: `cd ui && npm test -- --run && npm run typecheck`

Python: `cd bindings/python && .venv/bin/pytest`

TS: `cd clients/typescript && npm test`

Expected: all exit 0.

- [ ] **Step 3: Manual smoke (human or agent with a built binary)**

```
cargo build -p mushroomdb-cli --bin mushroomdb --features embed-ui --release
./target/release/mushroomdb demo /tmp/mdb-g1
./target/release/mushroomdb serve /tmp/mdb-g1
# prints listening on http://127.0.0.1:8080
# Ctrl-C → snapshot.bin exists; reopen is fast
./target/release/mushroomdb query /tmp/mdb-g1 "MATCH (p:Person {id: 'person-01'}) RETURN p"
```

- [ ] **Step 4: Commit docs polish**

```bash
git add README.md docs crates/cli
git commit -m "docs: G1 quickstart, usage, suggest defaults"
```

- [ ] **Step 5: Stop**

Do not start Phase 2. Report G1 status.

---

## Out of scope (do not implement here)

`IN`, `DISTINCT`, `MATCH SET RETURN`, MERGE ON CREATE, `FsyncPolicy`, HTTP `spawn_blocking` on write (only document), `All` index intersect, CSR/columns/mmap, HNSW, 3-node rules, `subscribe_query`, Unicode `toLower`, god-file splits unless the compiler forces a file split for `ScanKey`.
