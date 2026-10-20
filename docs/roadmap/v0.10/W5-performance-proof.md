# W5 — Performance, bulk import, and published proof

> Part of the v0.10 spec — see [`../v0.10-spec.md`](../v0.10-spec.md). Where this document and the master spec disagree, the master spec wins; its §4 lists every override.

**Unit id prefix:** PF-  

---

## 1. Outcome

After 0.10 a general developer can:

- Run `MATCH (n:L {key: $k})-[:T]->(m) RETURN m` at 1M nodes and get a result in < 1 ms p50 — the most common interactive graph query shape, today 281 ms at 1M (ledger row 102).
- Import a CSV file from any ETL pipeline using standard header conventions (`:ID`, `:LABEL`, `:START_ID`, `:END_ID`, `:TYPE`, `name:int`) with `mushroomdb import nodes.csv relationships.csv` or `LOAD CSV FROM 'file:///...' AS row` in Cypher, at throughput meaningfully above the current 13k nodes/s.
- Point at a public performance page backed by reproducible scripts that shows ingest, open, query p50/p99, and RSS at 1M nodes — with a CI gate that would break if the page drifted from the data.
- Read a "your data is safe" page where every claim (crash safety, fsync, format compatibility, backup/restore) is enforced by a named CI job, and where the claim goes red if the job stops running.
- Know the query shapes that have been benchmarked, the shapes that haven't, and what the numbers mean on what hardware.

---

## 2. Current state

### Key-anchored traversal (row 102)

The query `MATCH (p:Person)-[:KNOWS]->(q) WHERE key(p) = $k RETURN key(q) AS dst` takes 18.7 ms at 100k, 80.3 ms at 300k, 281.6 ms at 1M — grows linearly with label cardinality (`docs/roadmap/v0.6.10-defects.md:3068`).

Root cause confirmed from code: the planner (`crates/core-query/src/cypher/plan.rs`) emits `ScanLabel(Person)` → `Expand(p→q via KNOWS)` → `Filter(key(p)=$k)`. The `fold_where_equalities` pass (`plan.rs:793-851`) explicitly refuses to fold a key-equality filter into a `ScanKey` when any `Expand` lies between the anchor scan and the filter (`plan.rs:849-850`). The executor therefore scans every `:Person` node (~400k at 1M scale), expands each node's edges, and only then applies the key filter.

The planner already handles `WHERE key(p) = $k` as a `ScanKey` when no `Expand` precedes the filter (`plan.rs:880-890`, `exec.rs:8262-8264`), and inline-prop forms `MATCH (p:Person {id: $k})-...` would hit the same issue. This is a post-Expand fold gap, not an index gap.

### Ingest throughput

`dogfood/results/scale-memory-1m.md`: 13,158 n/s at 1M (embedded, no rules, 10k-chunk `ingest_batch`, one WAL fsync per chunk). Each fsync is on the critical path. No CSV importer. No JSONL export. Arrow IPC exists for results (`crates/arrow-bridge`). CLI has `ingest-json` but not `ingest-csv`. No `LOAD CSV` Cypher clause.

### Scale coverage

1M nodes proven: 2.65 GiB peak RSS, 209 MiB snapshot. 10M nodes never attempted. GitHub standard runners max at 7 GiB usable; `ubuntu-latest-8-cores` (8 GiB) is tight; large self-hosted runners or a documented reference machine are needed for 10M.

### Existing bench infrastructure

- `crates/core-bench/benches/engine.rs`: criterion suite at 10k nodes, 8 query shapes (table in `docs/performance.md`)
- `benchmarks/ci/run.sh` + `benchmarks/ci/compare.py`: JSON output, 15% threshold, two-stage baseline (stored + base-commit fallback)
- `benchmarks/baselines/ci.json`: pinned baseline from ubuntu-latest, 2026-09-14
- Scale dogfood: `dogfood/scale_memory.py`, `dogfood/scale_run.py` — not CI gates; manual runs producing `.md` result files
- Recall gates in CI: `sim-harness` tests for ANN recall floor (`ci.yml:62-116`)
- Format-compat CI: `cargo test -p mushroomdb --test snapshot --test migrate` (`ci.yml:129`)
- Sim-harness crash simulation: crash at every WAL/snapshot offset, seeded deterministic scenarios

### Data-safety evidence gaps

Sim-harness crash simulation covers WAL/snapshot crash injection (described at `design.md:236-245`). Format-compat pins V5–V10 opening. But no published "data-safety page." Claims about fsync policy, backup/restore, and crash recovery are scattered across `docs/site/durability.md`, `docs/format-stability.md`, and `design.md` — not a single page with CI-enforced claims.

---

## 3. Design

### PF-01: Fix key-anchored traversal (row 102)

**Root cause:** `fold_where_equalities` (`plan.rs:812`) finds the `ScanLabel` anchor, then refuses to fold if an `Expand` appears before the `Filter` (`plan.rs:849-850`). For a `key(var) = lit/param` equality on the scan variable, the correct plan is `ScanKey(p, $k, Person)` → `LookupProps(p)` → `Expand(p→q via KNOWS)` — no scan at all.

**Fix design:** Add a second fold pass (or extend the existing one): after the Expand-gap check, detect the special case where the filter term is `key(scan_var) = lit/param` or `id(scan_var) = lit/param`. In that case, rewrite `ScanLabel(p:L)` to `ScanKey(p, value, L)` and remove the key-equality term from the Filter (keeping residual terms). This is safe regardless of whether other filters or Expand ops follow, because `ScanKey` is a superset filter of `ScanLabel` + key filter.

**Scope:** `crates/core-query/src/cypher/plan.rs` only. No executor changes needed — `ScanKey` already calls `db.node_by_key()` (O(1) IdMap lookup at `exec.rs:1326-1340`).

**Target:** p50 < 1 ms at 1M nodes for `MATCH (n:L)-[:T]->(m) WHERE key(n) = $k RETURN m` (same order as `node_info` p50 = 11 µs + `node_edges` p50 = 56 µs, with Expand overhead).

**W2 interaction:** W2's equality/range index work is about property indexes over scalar fields. Key-anchored traversal uses the IdMap key lookup, not a property index. W2's work does NOT subsume PF-01. The two are independent and can be built in parallel.

**Differential probes required:** same query result on: (a) WAL-only store vs snapshot-reopened store; (b) one batch vs separate commits; (c) `WHERE key(p) = $k` form vs `{id: $k}` inline-prop form; (d) bound param vs literal.

### PF-02: Standard query-shape benchmark set

**Query shapes** (run at 1M nodes, embedded Python binding, M4 Pro reference):

| Shape | Cypher | p50 target | p99 target |
|---|---|---|---|
| Point lookup by key | `MATCH (n) WHERE key(n) = $k RETURN n` | < 0.05 ms | < 0.5 ms |
| Point lookup by indexed property | `MATCH (n:L) WHERE n.field = $v RETURN n` | < 1 ms | < 10 ms |
| Key-anchored 1-hop | `MATCH (n:L)-[:T]->(m) WHERE key(n) = $k RETURN m` | < 1 ms | < 5 ms |
| Key-anchored 2-hop | `MATCH (a:L)-[:T]->(b)-[:T]->(c) WHERE key(a) = $k RETURN c` | < 5 ms | < 20 ms |
| Filtered expansion (full label) | `MATCH (n:L)-[:T]->(m) WHERE m.score > $v RETURN m LIMIT 100` | < 10 ms | < 50 ms |
| Aggregation | `MATCH (n:L) RETURN count(n)` | < 20 ms | < 50 ms |
| ORDER BY + LIMIT | `MATCH (n:L) RETURN n ORDER BY n.score DESC LIMIT 10` | < 50 ms | < 200 ms |
| Variable-length 1–3 hops | `MATCH (n)-[*1..3]->(m) WHERE key(n) = $k RETURN m LIMIT 100` | < 10 ms | < 100 ms |

Filtered expansion and ORDER BY targets assume no range index (W2 will add that; when range index exists, targets should tighten ×10). shortestPath deferred to W6/after-0.10 (not yet implemented in the executor).

**CI integration:** Extend `benchmarks/ci/run.sh` to run this suite using the Python binding (not criterion) at a pinned scale (100k nodes — fast enough for CI, representative of shape). Store p50 per shape in `benchmarks/baselines/query-shapes.json`. Gate: p50 must not regress > 20% vs stored baseline OR vs the base commit on the same runner (same two-stage logic as the existing bench gate). The 100k CI gate is a regression gate, not a published number; the 1M run is the published proof.

**Files touched:** `benchmarks/ci/run.sh`, new `benchmarks/ci/query_shapes.py`, `benchmarks/baselines/query-shapes.json`, `crates/core-bench/benches/engine.rs` (add Cypher shape benches at criterion level for the 10k dataset), `.github/workflows/ci.yml` (add step in the `bench` job).

### PF-03: Bulk CSV import

**CLI surface:**
```
mushroomdb import \
  --nodes nodes.csv \
  --relationships rels.csv \
  [--db <path>] \
  [--delimiter ,] \
  [--array-delimiter ;] \
  [--ignore-errors] \
  [--batch-size 50000]
```

**Header conventions (de facto standard, named generically):**
- Nodes: `:ID`, `:LABEL`, then property columns. Optional type suffix: `name:int`, `weight:float`, `active:boolean`, `tags:string[]`.
- Relationships: `:START_ID`, `:END_ID`, `:TYPE`, then property columns. `:START_ID` and `:END_ID` reference `:ID` values from the nodes file.
- Multiple labels in `:LABEL` (`Person;Employee`) are refused row by row with a clear error while storage holds one label per node (master spec §2 non-goals, §9 question 5).

**Cypher surface:**
```cypher
LOAD CSV WITH HEADERS FROM 'file:///data/nodes.csv' AS row
CREATE (n:Person {name: row.name, age: toInteger(row.age)})

LOAD CSV WITH HEADERS FROM 'file:///data/nodes.csv' AS row
RETURN count(row)
```
- `FROM 'file:///...'` is the only supported URI scheme on an embedded instance. The server must restrict to an allow-list directory (configured via `--csv-root-dir` env/flag). URL-scheme access (`http://`, `https://`) is out of scope for 0.10 — security risk on a server, not needed for the primary use case.
- `toInteger()`, `toFloat()`, `toString()`, `toBoolean()` coercions are required for typed imports.

**Internal bulk-import path:** Bypass per-commit WAL overhead. The bulk path:
1. Disables incremental rule evaluation during import.
2. Writes nodes and edges in batches of 50k, with one WAL fsync per batch (existing `ingest_batch` mechanism).
3. After all data is loaded, runs a single full rule backfill for each declared rule.
4. Writes one compacting snapshot after import completes.

**Throughput target:** ≥ 100k nodes/s for CSV import on the reference machine (vs 13k n/s for the current incremental path). The gap is achievable by eliminating per-batch rule evaluation and reducing fsync frequency (50k-node batches → 0.5 fsyncs/s at 100k n/s vs 13 fsyncs/s now).

**Format changes:** None to the on-disk format. The CSV parser is a new crate or a thin wrapper; the data flows through the existing `ingest_batch` + `batch_edges` path.

**New dependency:** `csv` crate (pure Rust, no unsafe, widely used). No-dependency alternative: hand-rolled RFC 4180 parser (~150 lines); prefer `csv` crate for correctness and Unicode handling. Owner approval required.

### PF-04: JSON/JSONL import parity and export

**Import:** Already exists for JSON via `ingest_batch` + `json_to_rows` (`crates/core-api/src/ingest.rs`). Gap: no streaming JSONL import for large files. Add `mushroomdb import --nodes-jsonl nodes.jsonl` — streams line by line using the same 50k-batch path as CSV.

**Export:**
```
mushroomdb export \
  --format csv|jsonl|cypher \
  --output <dir|file> \
  [--db <path>]
```
- CSV: one `nodes.csv` and one `relationships.csv` using the standard header conventions from PF-03.
- JSONL: one record per line, `{"type":"node","key":...}` / `{"type":"edge",...}`.
- Cypher dump: `CREATE (n:L {key: "k", ...})` statements followed by `MATCH ... CREATE ...` for edges. Idempotent re-import when run against an empty store. Suitable for migration between versions.

**No new dependencies** beyond what PF-03 adds.

### PF-05: Published benchmark suite and performance page

**Datasets:**
- *Synthetic memory-shaped* (already exists in `dogfood/scale_memory.py`): Person/Org/Project/Concept/Event, Zipf text, 1 FieldEqual rule. Run at 100k, 300k, 1M nodes. This is the primary published set.
- *Synthetic property graph* (new, modeled on LDBC SNB SF-1 schema but without the LDBC data files): Persons, Knows edges, Posts, Comments. 1M nodes / ~2M edges at SF-1 equivalent. Pure synthetic generation — no licence issues. Covers the standard query shapes from PF-02.
- *10M-node run*: conditional. At 2.65 GiB/1M, 10M would require ~26.5 GiB. This exceeds GitHub `ubuntu-latest-16-cores` (16 GiB) and requires either a 32 GiB self-hosted runner or a documented reference machine. **Owner decision required** (see §8b). Do not gate 10M in CI — describe reference machine, publish results manually.

**Published metrics per dataset:**
- Ingest wall time and throughput (n/s)
- Open time (WAL-only, snapshot)
- Snapshot write time and size on disk
- Peak RSS during ingest and at steady state
- Query shapes p50/p99 (8 shapes from PF-02)
- Concurrent readers under writes (W3 owns the concurrency bench; W5 references it here)

**Reproducibility:** `dogfood/scale_memory.py --scale 1000000 --seed <pinned>` produces a machine-readable `.md`. Add a JSON sidecar (`--out-json results.json`) for structured consumption. Pinned mushroomdb wheel version and Rust toolchain version are included in the output header (already done in `scale_memory.py:759`).

**Docs site performance page** (`docs/site/performance.md`): generated from result files by a script (`benchmarks/publish/render_perf_page.py`) that reads the JSON sidecars and writes the markdown. CI check: the rendered page's key numbers must match the current result files. Any manual edit that diverges from the data fails CI. This prevents drift between the page and the actual measurements.

**Where results live:** `dogfood/results/` (existing convention). The generate script is `benchmarks/publish/render_perf_page.py`. The CI check job is `docs-perf-check` in `.github/workflows/ci.yml`.

**Memory footprint reductions worth doing in 0.10 (S/M only):**

| Reduction | Mechanism | Size | Estimated savings at 1M |
|---|---|---|---|
| String interning for property keys | Intern `HashMap<String, Value>` keys across nodes of same label | S | 10–20% of property storage |
| Property column compaction | Compress sparse columns (fields present on < 10% of nodes) as `HashMap` instead of dense column | S | 5–15% RSS for heterogeneous schemas |

Both are purely internal (`crates/core-storage`). Format change: the snapshot section for properties may change layout; bump minor snapshot version, keep reader backward compat. Do not attempt if not measurable (add a before/after RSS gate to the unit's verification). Total ceiling gain at 1M: estimate 0.3–0.5 GiB, moving the ceiling from 10M@26.5 GiB to 10M@22–24 GiB (still needs 32 GiB machine).

### PF-06: Data-safety page with CI-enforced claims

**Page:** `docs/site/data-safety.md` (or `docs/site/durability.md` rewrite). Structure:

> **Claim 1: Writes survive a crash.**  
> *Evidence:* The sim-harness (`crates/sim-harness/`) crashes at every WAL byte offset using a deterministic fault-injecting IO layer. CI job: `recall-gates` (`ci.yml:62`), which includes crash scenarios. Gate: `sim-harness` tests must pass on every PR.

> **Claim 2: Old store files open.**  
> *Evidence:* Format versions V5–V10 tested in CI. CI job: `format-compat` (`ci.yml:121`). Gate: `cargo test -p mushroomdb --test snapshot --test migrate` must pass.

> **Claim 3: Every write is fsynced before the client returns.**  
> *Evidence:* `FsyncPolicy::Strict` is the default (`design.md:218`). Code: `crates/core-storage` WAL writer. CI gate: integration test that kills the process after a write returns and verifies the data survives on reopen. This test does NOT currently exist. PF-06 adds it.

> **Claim 4: Backup and restore preserves all data.**  
> *Evidence:* W4 owns backup/restore. PF-06 references W4's CI job here — the data-safety page should not go live until W4's backup round-trip CI test exists.

> **Claim 5: A snapshot written during a crash is safe.**  
> *Evidence:* Snapshot writes use `.bak` rename with atomic move (`format-stability.md`). Tested by sim-harness crash-during-snapshot scenarios. Gate: sim-harness.

**The "gate that cannot see what it guards" anti-pattern:** Every claim must have a CI job whose name is cited on the page and whose failure would make the claim false. Claims 1, 2, 5 already have gates. Claims 3 and 4 need new gates added in this workstream.

**New CI test for Claim 3 (fsync integrity):** A Rust integration test in `crates/core-api/tests/` that:
1. Opens a store, inserts a node, reads back the key returned by insert.
2. Calls `std::process::exit(0)` directly (not a normal drop path) after the write call returns.
3. Re-opens the store in a subprocess and verifies the node is present.
The test uses `assert_cmd` or a similar subprocess harness. On Darwin, `F_FULLFSYNC` ensures this. Add `#[ignore]` (slow) but include in `format-compat` CI job.

---

## 4. Interaction with differentiators

**Rules engine:** PF-01 (key-anchored traversal fix) is planner-only; rule evaluation is not on the Cypher executor path for simple traversal. The bulk-import path (PF-03) disables incremental rule firing and does a single backfill at the end — same pattern as `create_rule` backfill today. Rule-derived edges are NOT in the WAL by design; snapshot carries them. The data-safety page (PF-06) must note this: crash during bulk import before the final snapshot loses derived edges but not base data.

**Time travel / as-of:** Bulk import writes all records in a small number of WAL frames. `open_at` and `was_linked` will see the import as a range of commits. No special handling needed.

**Scoped visibility / RBAC masks:** `LOAD CSV` inside a masked session applies the mask to whatever `RETURN` or `CREATE` produces; no special handling. The CSV importer runs without a mask (server-only consideration: restrict CSV root directory, not mask).

**Snapshots + WAL replay:** The key-anchored fix (PF-01) is planner-only; WAL replay re-executes Cypher queries through the same planner, so the fix is automatically correct on replay. Differential probe required: verify that a Cypher query anchored by key returns the same result from WAL replay vs snapshot open.

**Full-text/vector:** Query benchmark (PF-02) does not cover full-text or vector shapes — those are covered by the existing recall gates. The data-safety page does not need to cover vector recall (that is a quality claim, not a safety claim).

**MCP tools:** No interaction.

---

## 5. Compatibility and migration

**PF-01 (planner fix):** Planner-only change. No format version bump. No API break. Old stores open unchanged. Queries that previously returned results (correctly, just slowly) continue to return the same results, faster. The only risk is queries where `key(n)` appeared in a filter that was reachable only because of the label scan's full coverage — but `ScanKey` is strictly more selective, so no extra results appear.

**PF-03 (CSV import):** New CLI subcommand. No format changes. No API break for existing users.

**PF-05 (string interning / property compaction):** If implemented, triggers a minor snapshot version bump (e.g., V10→V11). Old mushroomdb (0.9.x) cannot read the new snapshot and falls back to WAL replay — this must be loud (error message on `open()` if snapshot version is unrecognized). `format-compat` CI job must add a V11 golden fixture. The docs/format-stability.md entry must state: "A 0.10 snapshot cannot be opened by 0.9.x; WAL replay always works across versions."

**PF-06 (data-safety page):** Documentation only; no format or API changes. New integration test in `crates/core-api/tests/`.

---

## 6. Verification

### PF-01 (key-anchored traversal)

- **Unit test:** `where_key_eq_after_expand_uses_scan_key()` in `crates/core-query/src/cypher/exec.rs` — asserts `SCAN_KEY_COUNTER` increments and `FUSED_SCAN_FILTER_COUNTER` does not, for the pattern `MATCH (p:L)-[:T]->(q) WHERE key(p) = $k RETURN q` (existing counter infrastructure at `exec.rs:17-23`).
- **Integration test:** `crates/core-api/tests/cypher_key_traversal.rs` — inserts 10k nodes, queries with key anchor, asserts result equals `node_edges` result, asserts wall time < 10 ms (vs expected ~5 ms at 10k).
- **Differential probe:** same query result via: WAL replay, snapshot open, one-batch vs multi-batch ingest.
- **Scale gate:** extend `dogfood/scale_memory.py` query phase to time this shape separately, assert p50 < 5 ms at 1M. The CI bench suite adds this shape at 100k with 20% regression threshold.
- **Perf comparison vs v0.9.x:** run the 1M memory scale script with 0.9.x wheel and the 0.10 wheel; assert ratio > 50× improvement for the key-anchored hop.

### PF-02 (query benchmark)

- New CI step in `bench` job: `python benchmarks/ci/query_shapes.py --out results-shapes.json`; compare via `benchmarks/ci/compare.py results-shapes.json benchmarks/baselines/query-shapes.json --threshold 0.20`.
- For each new criterion bench added to `engine.rs`, verify it uses the same 10k dataset (`bench_db(10_000, 0xA5A5_5A5A_C0DE_4B1D)`) so it is comparable to existing benches.

### PF-03 (CSV import)

- Unit tests for header parsing edge cases: quoted fields, escaped delimiters, missing columns, unknown column types (should warn and treat as string).
- Integration test: round-trip `export --format csv` → `import --nodes / --relationships` → verify node/edge count and a sample of properties. Run with and without rules declared.
- Throughput assertion: `cargo test -p mushroomdb-cli --test csv_import -- --ignored --nocapture` must achieve > 50k nodes/s on the CI runner (relaxed from 100k target which assumes M4 Pro; CI runners are slower).
- `LOAD CSV` Cypher: add to `crates/core-query/` test suite, covering `WITH HEADERS` and without, `toInteger()`/`toFloat()` coercions, missing values as `null`.

### PF-05 (scale proof / memory footprint)

- `dogfood/scale_memory.py --scale 1000000` must complete with peak RSS < 2.8 GiB (current 2.65 GiB, headroom for string interning overhead).
- `benchmarks/publish/render_perf_page.py` must produce identical output to what is in `docs/site/performance.md`; CI job `docs-perf-check` fails if they diverge.
- If property compaction is implemented: before/after RSS gate (1M-node store, assert RSS reduction ≥ 5%).

### PF-06 (data-safety page)

- New test `crates/core-api/tests/fsync_integrity.rs` (runs via `format-compat` CI job, `--ignored` flag, `--release` profile).
- The data-safety page itself is checked by `check-claims.sh` (existing script, `docs/roadmap/v0.6.10-defects.md:56` notes blind spots — row 56 is a gate defect W6 owns; W5's page must be listed explicitly in the census).

---

## 7. Build units

| Unit ID | Description | Size | Files/crates | Depends on | Conflict hotspots |
|---|---|---|---|---|---|
| **PF-01** | Fix key-anchored traversal: extend `fold_where_equalities` to rewrite `ScanLabel→Expand→Filter(key=val)` as `ScanKey→Expand` | S | `crates/core-query/src/cypher/plan.rs`, `crates/core-api/tests/cypher_key_traversal.rs`, `dogfood/scale_memory.py` | none | `plan.rs` (W1 will also touch planner for new syntax) |
| **PF-02a** | Add 5 Cypher query shapes to criterion bench suite (`engine.rs`); update `docs/performance.md` baseline table | S | `crates/core-bench/benches/engine.rs`, `docs/performance.md` | PF-01 (shape 3 needs fix first to be meaningful) | `engine.rs` (add rows, no conflict with others) |
| **PF-02b** | Add Python query-shape CI gate: `benchmarks/ci/query_shapes.py`, baseline JSON, CI step | S | `benchmarks/ci/query_shapes.py`, `benchmarks/baselines/query-shapes.json`, `.github/workflows/ci.yml` | PF-02a, PF-01 | `.github/workflows/ci.yml` (high conflict — coordinate with all workstreams) |
| **PF-03a** | CSV node/relationship parser and `mushroomdb import` CLI subcommand | M | `crates/cli/src/import.rs` (new), `crates/cli/src/lib.rs`, `Cargo.toml` (new `csv` dep) | none | `crates/cli/src/lib.rs` (W3 may touch CLI) |
| **PF-03b** | `LOAD CSV [WITH HEADERS] FROM 'file://...' AS row` Cypher clause: lexer, parser, planner, executor | M | `crates/core-query/src/cypher/{lexer,ast,plan,exec}.rs` | W1 (Cypher infrastructure) | `plan.rs`, `exec.rs` (W1 touches all of these) |
| **PF-04** | JSONL streaming import + CSV/JSONL/Cypher-dump export | M | `crates/cli/src/export.rs` (new), `crates/core-api/src/ingest.rs` | PF-03a (shares CSV header logic) | `ingest.rs` (low conflict) |
| ~~PF-05a~~ | **Deferred to 0.11** (master §4.1): string interning changes the snapshot layout for every store and would end downgrade to 0.9.x | — | — | — | — |
| **PF-05b** | Performance page generator + CI drift check | S | `benchmarks/publish/render_perf_page.py`, `docs/site/performance.md`, `.github/workflows/ci.yml` | PF-01, PF-02 (results needed) | `.github/workflows/ci.yml` |
| **PF-06a** | Fsync integrity integration test | S | `crates/core-api/tests/fsync_integrity.rs`, `.github/workflows/ci.yml` | none | low |
| **PF-06b** | Data-safety page; link all existing CI jobs by name; cite W4 backup job | S | `docs/site/data-safety.md` (new) | W4 (backup/restore CI job must exist first) | none |
| **PF-07** | Row 62: measure the four engine caps (MAX_QUERY_TERMS, MAX_FULLTEXT_PAIRS, etc.) at 1M scale; replace first-guess constants with measured values or add telemetry | S | `crates/core-api/src/memory/`, `dogfood/scale_memory.py`, `docs/site/limits.md` | none | low |
| **PF-08** | Row 46: add ignored-test census to CI so scale assertions cannot be silently skipped | S | `.github/workflows/ci.yml`, test registry script | none | `.github/workflows/ci.yml` |

**Parallelism:** PF-01 can start immediately (no dependencies). PF-03a, PF-06a, PF-07, PF-08 are all independent of each other and of W1/W2/W3. PF-03b depends on W1 Cypher infrastructure. PF-05a can run in parallel with everything but needs review for snapshot format impact before merge.

---

## 8. Decisions

### (a) Builder's call

- **PF-01 fold approach:** Extend `fold_where_equalities` with a second pass rather than a separate function. Recommendation: separate function `fold_key_anchor_after_expand` called after the existing pass — cleaner to test in isolation.
- **CSV crate:** Use the `csv` crate (widely used, no unsafe, correct Unicode, handles CRLF). Hand-rolling is error-prone for quoted fields. Recommend: add the `csv` crate.
- **String interning scope:** Intern property keys across all nodes of a given label (not globally) to avoid cross-label key collisions. Intern table lives alongside the label's column store.
- **`LOAD CSV` URL restriction:** Compile-time: only `file://` URIs parse successfully. `http://` returns a `UnsupportedUri` error (not a runtime restriction — no network access in the embedded crate).
- **10M dogfood target:** Run the 10M scale script once on a 32 GiB reference machine (the owner's M4 Pro laptop, 24 GiB, will not fit — needs the 24-GiB figure). If the 1M RSS ceiling is at 2.65 GiB and string interning saves 15%, 10M would be ~22.5 GiB — fits a 32 GiB machine but not the owner's 24 GiB laptop. **Owner decision required** (see §8b).

### (b) Owner must decide

**OD-1: 10M-node published run.**  
Current RAM: 2.65 GiB @ 1M → ~26.5 GiB @ 10M without reduction, ~22.5 GiB with reduction. the owner's laptop has 24 GiB. Three options:
- **Recommended:** Publish 1M as the primary proof. Describe what 10M would require (32 GiB). Run 10M on a cloud instance (Hetzner AX162 or similar, 128 GiB, < $2/hr) once and publish the results as a one-time benchmark with stated hardware. CI gate stays at 1M.
- **Alternative:** Skip 10M entirely for 0.10; commit to 3M (fits the owner's laptop with reduction).
- **Alternative:** Run 10M in CI on a self-hosted 32 GiB runner (ongoing cost, setup required).

**OD-2: Property compaction / string interning (PF-05a).**  
These are S-size units but touch the snapshot format and require a minor version bump. If OD-1 decides 10M is needed, PF-05a becomes more important. If 1M is the ceiling, the gain is marginal. Recommendation: do string interning (S, low risk, measurable savings), skip property compaction for 0.10 (higher risk, less certain savings).

**OD-3: `LOAD CSV` FROM http://, https:// on the server.**  
HTTP CSV fetch introduces a network dependency and SSRF risk. Recommendation: out of scope for 0.10. If requested, restrict to an allowlist configured in `server.json`. Owner must decide if the server variant should support HTTP URLs at all in 0.10.

---

## 9. New dependencies

| Package | Purpose | No-dep alternative | Owner approval needed |
|---|---|---|---|
| `csv` (Rust crate) | RFC 4180 CSV parsing for PF-03 | Hand-rolled parser (~150 lines, risk of edge cases) | Yes |

No other new dependencies. The Python-side CSV import in PF-04 uses Python's stdlib `csv` module (no new pip dependency).

---

## 10. Out of scope / deferred

- **shortestPath** Cypher clause: not yet implemented in the executor; deferred to after-0.10 or W1 depending on Cypher scope.
- **Disk-paged storage / graph > RAM:** explicitly out of 0.10 per brief.
- **Concurrent-writer throughput bench:** owned by W3 (concurrency workstream); W5 references the result in the published metrics.
- **LongMemEval quality benchmark:** separate from performance proof; deferred to a future release (harness at `benchmarks/longmemeval/` exists but needs the benchmark data and a published baseline).
- **Parquet export:** not needed for the primary use case (CSV and JSONL cover ETL portability); deferred.
- **`LOAD CSV` HTTP/HTTPS on server:** OD-3 above; recommend deferring.
- **Row 44** (retired hook bodies still parse): belongs to W6 (CLI/tooling cleanup) not W5.
- **Row 51** (`same_as_pairs` swallows errors): belongs to W1 (correctness/engine), not W5.
- **Row 54** (all-features clippy red): quality debt, assign to W6 (general hygiene pass).
- **Row 63** (boolean literals, skill shadowing): W1 (Cypher) and W6 (tooling).
- **Row 64** (`remember` label disagreement): W1/memory API.
- **Row 65** (GIL held on writes): W3 (Python binding / concurrency) — requires making the read-then-write atomic before releasing GIL.
- **Row 66** (raw writes forge SAME_AS): owner decision, belongs to W1/memory API.
- **Row 67** (delete guard misfires): engine correctness, W1.
- **Row 68** (skill byte-cap test path): W6.
- **Row 69** (`delivery_for_store` returns wrong value): W6.
- **Row 70** (`query` CLI no stdin/file): W6.
- **Row 85** (`what_if_set_prop` double-counts): W1 engine correctness.
- **Row 88** (MCP schema incomplete): W4/MCP.
- **Row 90** (`count(true)`, algo positional args): W1 Cypher correctness.
- **Row 101** (PITR interrupted-archive refusal): W4 (backup/restore).
- **Row 114** (`reader().node_edges` derived:false): W3 (lock-free reader path).
- **Snapshot open > 1 s at 1M nodes** (current 1.18 s): noted in recon; the rkyv mmap path already loads lazily. No explicit optimisation unit proposed for 0.10 unless PF-05a's string interning also reduces snapshot size materially.

---

## Ledger row disposition summary (W5 scope)

| Row | Severity | Summary | W5 unit | Notes |
|---|---|---|---|---|
| 46 | M gate | Ignored-test census cannot fail CI | PF-08 | 23 scale tests run nowhere |
| 62 | L unmeasured | Four engine caps are first-guess constants | PF-07 | Measure or add telemetry |
| 102 | M perf | Key-anchored 1-hop scans label (281 ms @ 1M) | PF-01 | Core W5 fix |
| 54 | L CI gap | All-features clippy red | W6 | Not W5 |
| 51 | L correctness | `same_as_pairs` swallows errors | W1 | Not W5 |
| 67 | L correctness | Delete guard misfires | W1 | Not W5 |
| 85 | L correctness | `what_if_set_prop` double-counts | W1 | Not W5 |
| 90 | L correctness | `count(true)` unbound, algo positional | W1 | Not W5 |
| 114 | L reader | `reader().node_edges` derived:false | W3 | Not W5 |
