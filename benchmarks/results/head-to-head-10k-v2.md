# Head-to-head benchmark v2 — mushroomdb vs. Neo4j / KùzuDB / Memgraph

> **Supersedes** `benchmarks/results/head-to-head-10k.md` (v1, 2026-08-20).
> v1 had: mushroomdb two-hop ERROR (pull executor not yet landed), memgraph
> scan-filter semantically broken (stores key only), same-port contamination
> for the memgraph run.  All three are resolved here.

## Machine / date / versions

- **Date:** 2026-08-20
- **Host:** mac.lan (Apple M4 Pro, 12 cores, arm64)
- **OS:** macOS 15.7.3
- **RAM:** 24 GiB
- **Python:** 3.12.12
- **Scale:** 10,000 nodes (seed=20260819, 70/20/10 Talent/Company/Job split)

| Engine | Version |
|---|---|
| mushroomdb | 0.1.0 (embedded Rust, Python bindings; Plan-12 pull executor + V4 snapshot) |
| neo4j | 5-community (image: `neo4j:5-community`; driver: `neo4j` 6.2.0) |
| kuzu | 0.11.3 (pip, embedded) |
| memgraph | latest (image: `memgraph/memgraph:latest`; driver: `neo4j` 6.2.0 via bolt) |

---

## Honesty notes

- **mushroomdb** numbers are **embedded Rust** (no network RTT, no serialization overhead).
  KùzuDB is also embedded — its numbers are directly comparable to mushroomdb's.
  Neo4j and Memgraph numbers go over bolt/localhost (~0.1–1 ms round-trip per query).
- **rule_derive** is mushroomdb-only — competitors have no auto-derivation equivalent.
  It is excluded from the cross-engine table. See `benchmarks/README.md`.
- **Sequential runs required** (contamination guard): Neo4j and Memgraph both default to
  `bolt://localhost:7687`. They were run strictly sequentially:
  - Run A (neo4j only): dai-neo4j stopped; `bench-neo4j` (`neo4j:5-community`,
    `NEO4J_AUTH=none`) on `:7687`; ours and kuzu run simultaneously (embedded).
  - Run B (memgraph only): `bench-neo4j` stopped; `bench-memgraph`
    (`memgraph/memgraph:latest`) on `:7687`. Port free verified with `docker ps`
    before each run.
- **memgraph adapter fix (v2):** the memgraph adapter previously stored only the `key`
  field (`CREATE (n:{label} {key: row.key})`). Fixed to `SET n = row` which stores all
  fields (key + props). `cypher_scan_filter` now correctly returns 1,400 rows (same as
  neo4j/mushroomdb) instead of 0. `bulk_ingest` time increased from 46 ms to 19.9 s
  because full property serialization now occurs.
- **cypher_two_hop (I1 fix — fair comparison):** mushroomdb derives INDUSTRY_ALIGNMENT edges
  automatically via the `bench_industry_tc` rule (FieldEqual on `industry`). For a fair
  comparison, the 1,000,000 derived edges were exported and bulk-loaded into each competitor
  engine as ordinary edges (pre-materialization). All four engines then ran the same query:
  `MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)<-[:INDUSTRY_ALIGNMENT]-(t2:Talent)
  RETURN t.key, c.key, t2.key LIMIT 200` — all return **200 rows**.
  **Edge pre-materialization times (one-time cost):** neo4j 10.8 s, kuzu 0.17 s (COPY FROM CSV),
  memgraph 8.0 s. mushroomdb derives the edges in 0.924 s automatically on rule declaration.
  Competitors required manual ETL; mushroomdb's rule engine replaces this step entirely.
- **kuzu cypher_scan_filter (I2 fix):** KùzuDB adapter updated to store `size_bucket INT64`
  (full props). Scan-filter now uses `WHERE n.size_bucket = 3` — returns 1,400 rows
  (identical to neo4j/mushroomdb/memgraph). Previous workaround (`STARTS WITH 'talent'`,
  7,000 rows) is retired.
- **cold_start asymmetry:** mushroomdb `cold_start` measures `GraphDb::open()` + first
  `node_edges()` call (full process cost). Neo4j / Memgraph `cold_start` measures
  connect+first query with the server **already running** — their server boot cost is
  reported separately as `boot_to_ready`. KùzuDB (embedded) measures database open + query,
  directly comparable to mushroomdb.

---

## Cross-engine comparison (wall time)

| workload | mushroomdb | neo4j | kuzu | memgraph |
|---|---|---|---|---|
| bulk_ingest | 0.874 s | 13.227 s | 1.19 min | 19.924 s † |
| neighborhood_depth1 (p50) | 0.4 µs | 1.81 ms | 101 µs | 3.00 ms |
| neighborhood_depth1 (p95) | 2.2 µs | 14.47 ms | 405 µs | 6.71 ms |
| neighborhood_depth2 (p50) | 0.2 µs | 4.73 ms | 1.06 ms | 2.50 ms |
| cypher scan-filter-project (1.4k rows) | 2.20 ms | 87.36 ms | 0.37 ms ‡ | 12.56 ms |
| cypher two-hop join (200 rows) | **0.198 ms** | 5.68 ms ★ | 1.58 ms ★ | 2.17 ms ★ |
| cold_start (WAL-only / connect) | 3.24 s | 18.54 ms ▲ | 23.41 ms | 0.42 ms ▲ |
| cold_start (snapshot V4) | **1.01 s** | — | — | — |
| server boot-to-ready | n/a (embedded) | 6.6 s | n/a (embedded) | 4.3 s |

† memgraph `bulk_ingest`: v2 fix stores full props (`SET n = row`); time reflects real
  property serialization. v1 was 46 ms (key only — semantically incomplete).

‡ kuzu `cypher_scan_filter`: I2 fix — adapter now stores `size_bucket INT64`; uses
  `WHERE n.size_bucket = 3` returning 1,400 rows (was `STARTS WITH 'talent'` → 7,000 rows).
  0.37 ms is best-of-5; same predicate semantics as neo4j/mushroomdb/memgraph.

★ neo4j / kuzu / memgraph `cypher_two_hop` (I1 fix): all return **200 rows** after
  1,000,000 INDUSTRY_ALIGNMENT edges were bulk-loaded as ordinary edges (pre-materialization).
  One-time pre-mat cost: neo4j 10.8 s, kuzu 0.17 s (COPY FROM CSV), memgraph 8.0 s.
  mushroomdb derives the same edges automatically in 0.924 s on rule declaration — no manual
  ETL required. v1 entries showed 0 rows (empty scan); those have been superseded by this run.
  mushroomdb v1 row was ERROR; fixed (Plan-12 pull executor with LIMIT pushdown).

▲ neo4j / memgraph `cold_start`: server already running; measures connect + first query
  only. `boot-to-ready` row reports the actual container-start-to-first-query-answered time
  (neo4j: 6.6 s, memgraph: 4.3 s). mushroomdb and kuzu are embedded — there is no server;
  `cold_start` IS the full startup cost.

---

## mushroomdb — rule_derive (ours-only, excluded from cross-engine table)

> **Auto-derivation has no competitor equivalent.**
> Edges are derived automatically when rules are declared and on every subsequent
> ingest/update. Competitors require manual ETL / triggers. This workload is
> intentionally excluded from the cross-engine table.

- **Rules declared:** 2
- **Total backfill wall:** 3.076 s
  - `bench_industry_tc` (INDUSTRY_ALIGNMENT): 0.924 s
  - `bench_specialty_tc` (SPECIALTY_MATCH): 2.152 s

(v1: 20.728 s total — Plan-12 T1 streaming backfill reduced this ~7×)

---

## 100k cold-start — WAL-only vs. Snapshot V4

> Measured from a freshly rebuilt 100k-node dogfood database
> (`dogfood/results/scale-100000-db`, rebuilt 2026-08-20 via `dogfood/scale_run.py`).
> The old `scale-100000-db` was pre-Plan-11 bincode and unreadable; it was deleted
> and rebuilt from scratch.

| Path | Cold-start wall | Notes |
|---|---|---|
| mushroomdb WAL-only (100k) | **8.86 min** | WAL replay re-fires all 12 rules; IVF-Flat dominates (~8.37 min). Same bottleneck as Plan-11 7.91 min (non-semantic rules faster now via T1). |
| mushroomdb snapshot V4 (100k) | **11.15 s** | V4 snapshot loads derived edges + IVF centroids; no rule re-fire. **47.7× faster** than WAL-only. snapshot() write cost was 36.1 s (one-time, paid at graceful shutdown). |
| neo4j connect-only (10k scale) | 18.54 ms | Server already running; boot-to-ready = 6.6 s |
| kuzu open+query (10k) | 23.41 ms | Embedded; no rules to replay |
| memgraph connect-only (10k scale) | 0.42 ms | Server already running; boot-to-ready = 4.3 s |

**Key finding:** V4 snapshot (T4 Plan-12) reduces 100k cold-start from 8.86 min to 11 s —
a 47.7× improvement. The honest embedded-vs-server comparison:
- mushroomdb (embedded): 11 s from V4 snapshot, or 8.86 min from WAL-only
- neo4j (server): 6.6 s to boot the process; connect+query adds 18.5 ms after boot
- memgraph (server): 4.3 s to boot; connect+query adds 0.42 ms after boot
- kuzu (embedded, no rules): 23 ms open+query

mushroomdb V4 snapshot open (11 s) is slower than server boot (4–7 s) but eliminates
the 8.86-min rule-re-fire penalty entirely. Once booted, embedded mushroomdb has zero
network RTT vs bolt latency per query.

---

## Provenance / measurement notes

| Engine | Source | Server state | Valid? |
|---|---|---|---|
| mushroomdb | Run A (bench-neo4j up) | embedded, unaffected by bolt servers | YES |
| neo4j | Run A | `bench-neo4j` (`neo4j:5-community`, `NEO4J_AUTH=none`) on `:7687`; port 7687 verified free before start | YES |
| kuzu | Run A | embedded, unaffected by bolt servers | YES |
| memgraph | Run B | `bench-memgraph` (`memgraph/memgraph:latest`) on `:7687`; `bench-neo4j` stopped and removed before start; port 7687 verified free | YES |

**Contamination check:** `docker ps` run before each bolt server start. `bench-neo4j` stopped
and removed before memgraph start. No cross-contamination in v2 runs.
