# Changelog

## v0.1.1 — unreleased

### MCP agent-memory tools

Three new MCP tools for agent workflows — available via `mushroomdb mcp <dir>`:

- **`upsert_entity`** — insert or update a node by key with no existence
  check required; creates the node if absent, updates properties if present.
- **`find_similar`** — return neighbors connected by a given edge type
  (default `SIMILAR`); designed for vector-rule recall in agent-memory
  workflows.
- **`explain_association`** — explain rule-derived associations between two
  node keys; returns rule name, edge type, and match score per link. Alias
  of `explain` with a semantically clearer name.

The MCP server now exposes eleven tools total. Tests: 15 stdio round-trip
unit tests in `crates/server/src/mcp.rs` covering all eleven tools.

See [`docs/site/mcp.md`](docs/site/mcp.md) for the Claude Desktop
configuration and the full agent-memory quickstart (store → link → recall →
explain).

### Performance (rule_derive backfill)

N=5 median measured 2026-08-24 on Apple M4 Pro, macOS 15.7.3, arm64,
release build:

| Rule | Time |
|---|---|
| `bench_industry_tc` (FieldEqual → INDUSTRY_ALIGNMENT) | ~856 ms |
| `bench_specialty_tc` (Overlap → SPECIALTY_MATCH) | ~2.041 s |
| **Total** | **2.878 s** |

Result: **within target**. Re-measured on v0.1.1: N=5 median 2.878 s vs 2.894 s
baseline — no residual regression at this scale. No code change was required.

### Snapshots

- **V6 snapshot format (compressed)** — `snapshot()` now writes a zstd-compressed
  (level 3) V6 container. Wire format: `GDB1` magic + 2-byte version header
  (uncompressed); body is a zstd stream of the existing V5 payload (CRC32 + bincode).
  Measured at 5k nodes: 62 KiB on disk, 16 ms write, 2 ms open. At 100k nodes
  (9 rules, ~10.5M derived edges): **1.1 GiB on disk** (−50% vs V5 ~2.2 GiB),
  **22.563 s write**, **8.880 s open** (V5 baseline: 25.09 s write, 8.71 s open).
  v0.1.0 V5 snapshots are read transparently — no migration required. Old binaries
  cannot read V6 files (forward-breaking for the snapshot file only; WAL format and
  Python/HTTP API are unchanged).
- **`snapshot_with(SnapshotOptions { keep_wal: bool })`** — new API that exposes
  snapshot options. `keep_wal: true` writes the V6 snapshot but preserves the WAL,
  keeping pre-snapshot commits reachable via `open_at`. Crash-safe: snapshot write
  is atomic; if it crashes the full WAL is intact and replay over the snapshot is
  idempotent. `snapshot()` is unchanged (keep_wal defaults to false).

### Cypher

- **`IS NULL` / `IS NOT NULL`** — postfix null-check predicate in `WHERE` and `WITH … WHERE`; composes with `AND`/`OR`/`NOT`; enables the anti-join idiom (`OPTIONAL MATCH … WHERE b IS NULL`).
- **General arithmetic (`+`, `-`, `*`, `/`)** — arithmetic expressions in `RETURN`, `WHERE` comparisons, `SET` RHS, and function arguments; operator precedence (`*`/`/` over `+`/`-`); parentheses; null propagation; saturating integer arithmetic; named error on division by zero.
- **`CREATE … RETURN`** and **`MERGE … RETURN`** — single-statement write-then-project; write commits to WAL before projection; returns created/matched node bindings and computed columns.

---

## v0.1.0 — 2026-08-21

First tagged release.

---

### Engine

- **Incremental linking rules** — declare a predicate once; every subsequent write
  evaluates it and fires or retracts the matching edges automatically. Six predicate
  kinds: `KeyMatch`, `FieldEqual`, `Overlap` (Jaccard), `NumericWithin`, `GeoRadius`,
  `VectorSimilar` (cosine). Predicates compose via `All(...)` (AND, score = min) and
  `Any(...)` (OR, score = max), nestable to depth 4.
- **Auto-FK detection** — fields ending in `_id` whose values match existing node keys
  get `KeyMatch` rules created automatically at ingest time.
- **Top-k per source** — `max_edges: Some(k)` limits the engine to the k highest-scoring
  destinations per source node; eviction and backfill fire automatically on every mutation.
- **Approximate vector mode** — `VectorSimilar` with `approximate: true` uses IVF-Flat
  candidate selection. Measured per-query recall ≥ 0.90 quiesced, ≥ 0.85 post-rebuild
  at 5k nodes / dim 1536. Exact backfill ~12 min, approximate ~17 s at that scale.
- **Crash-atomic write batches** — `write_batch(|b| { … })` commits any number of mixed
  ops in one WAL frame; on crash replay the frame is all-or-nothing.
- **Materialized views** — `create_view` maintains per-node derived properties
  (degree counts, neighbor-aggregate sum/avg/min/max/count) incrementally on every edge
  change. Values persist through WAL and snapshots; rebuild in O(nodes × degree) on open.
- **Rule suggestions** — `suggest_rules()` (and `GET /suggest`, `mushroomdb suggest`)
  profiles the data and returns a ranked list of candidate rules with estimated edge
  counts, example pairs, and rationale. No rule is ever applied automatically.
- **WAL + versioned snapshots** — CRC-checksummed WAL with per-commit fsync; versioned
  snapshots in a zero-copy archived format (V5). Open = snapshot + WAL replay. Derived
  edges are not WAL-logged; they are re-materialized from node data on open.

### Cypher

- `MATCH`, `WHERE`, `RETURN`, `ORDER BY`, `LIMIT`, `SKIP`
- `WITH` pipeline stages (projection, aliasing, HAVING-style WHERE, ORDER BY, LIMIT,
  re-entry MATCH)
- `OPTIONAL MATCH` (left-outer-join semantics; composes with WITH and aggregation)
- `UNWIND` list expansion
- `CREATE` (nodes and relationships), `MATCH … SET`, `MATCH … DELETE`,
  `MATCH … DETACH DELETE`, `MERGE` (single-key match-or-create)
- Aggregations: `COUNT(*)`, `COUNT(n)`, `SUM`, `AVG`, `MIN`, `MAX` — single and grouped
- Variable-length paths: `-[r:TYPE*min..max]->` and `shortestPath`; max hops capped at 10
- Query parameters: `$name` placeholders replaced at query time
- Scalar functions: `toLower`, `toUpper`, `size`, `coalesce`, `type(r)`, `abs`, `round`

### Durability

- Byte-offset crash sweep at every WAL byte verifies none-or-all atomicity and
  oracle-equivalence recovery across all predicate types, view definitions, and fulltext
  index state.
- Op-count crash sweep covers snapshot `write_atomic` and WAL-truncation `write_atomic`
  crash windows.
- Approximate (IVF-Flat) recall verified at every crash-recovery state: ≥ 0.85 floor.
- WAL replay identity for approximate rules: same rule + same data → same clusters → same
  derived edges after reopen.

### Unlocks

- **Time travel** — `open_at(&dir, commit)` replays the WAL to any past commit, giving a
  read-only view of the graph at that point. Scope: commits in the current WAL since the
  last snapshot.
- **Live subscriptions** — `subscribe_rule("name")` and `GET /subscribe` (WebSocket)
  stream `EdgeFired` / `EdgeRetracted` / `NodeInserted` / `NodeDeleted` events after each
  WAL fsync. Measured end-to-end latency: in-process p50 0.04 µs / p95 0.21 µs; over WS
  on localhost p50 61 µs / p95 88 µs.
- **Full-text search** — `enable_fulltext(label, field)` builds and maintains an inverted
  index. Queries support AND/OR boolean operators and prefix matching (`rust*`).
- **Graph algorithms** — PageRank, weakly-connected components (WCC), and degree
  centrality over the unified topology (manual + derived edges), accessible via the Rust
  API and the `mushroomdb algo` CLI subcommands.

### Clients

- **TypeScript client** (`clients/typescript`) — wraps the HTTP + WebSocket API with full
  TypeScript types. Install from the repository; npm publish pending after the first tag.
- **Python bindings** (`bindings/python`) — PyO3 thin wrapper over `core-api`. maturin
  build; PyPI publish pending after the first tag.
- **Docker image** — `ghcr.io/matthewsherlin/mushroomdb` (registry publish pending after
  the first tag). Local build: `docker build -t mushroomdb:local .`.
- **MCP server** — `mushroomdb mcp <dir>` starts a stdio JSON-RPC server exposing graph
  operations as MCP tools.

### Known limitations in v0.1.0

| Limitation | Detail |
|---|---|
| Memory-first storage | RAM-bound. Design target 10M nodes (~5–15 GB). mmap-backed storage is on the roadmap. |
| Two-hop Cypher joins at scale | Dense patterns producing >1,000,000 intermediate rows error without `LIMIT`. |
| Snapshot V4 → V5 migration | V4 snapshots are rejected by V5 code. Rebuild required: delete old snapshot directory, reopen from WAL (cold start), then call `snapshot()`. |
| Cold start without snapshot | WAL-only open re-fires all rules. At 100k nodes / 12 rules with IVF: 8.86 min (V4 baseline). Call `snapshot()` before close. |
| Approximate vector mode is opt-in | `approximate: true` must be set explicitly. Per-query recall ≥ 0.90 quiesced; review the trade-off for completeness-critical workloads. |
| No multi-statement transactions | Each API call and each Cypher write statement is its own WAL Batch frame. BEGIN/COMMIT interactive transactions are not supported. |
| Readers observe intermediate batch state | `write_batch` is crash-atomic but not isolated: readers may observe intermediate in-memory states while a committed batch is being applied. |
| Cypher write subset | SET RHS accepts literals or `$param` only; expression RHS (`n.x + 1`) is a named error. Combined MATCH…SET…RETURN is rejected. |
| `demo` refuses non-empty directories | `mushroomdb demo` exits 1 if the target directory is non-empty, including hidden files (`.DS_Store` counts). |

---

## Pre-1.0 breaking-format policy

Before v1.0.0, snapshot and WAL formats may change between minor versions
(`v0.x → v0.x+1`). When a format change is released:

- Old snapshots are rejected by the new code with a named error.
- A **full rebuild is required**: delete the database directory, reopen from a fresh
  ingest or re-run your write workload, and call `snapshot()`.

WAL records added in a minor version are backward-compatible within the same snapshot
series. Cross-series WAL replay (WAL from v0.x replayed by v0.x+1 code after a snapshot
format bump) is not guaranteed.

This policy does not apply after v1.0.0. Post-1.0 format compatibility follows semver.
