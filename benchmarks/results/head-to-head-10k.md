# Head-to-head benchmark — mushroomdb vs. system A / system B / system C

## Machine / date / versions

- **Date:** 2026-08-20
- **Host:** mac.lan (Apple M4 Pro, 12 cores, arm64)
- **OS:** macOS 15.7.3
- **RAM:** 24 GiB
- **Python:** 3.12.12
- **Scale:** 10,000 nodes (seed=20260819, 70/20/10 Talent/Company/Job split)

| Engine | Version |
|---|---|
| mushroomdb | 0.1.0 (embedded Rust, Python bindings) |
| system A | 5.26.29 (image: `system A:5-community`; driver: `system A` 6.2.0) |
| system B | 0.11.3 (pip, embedded) |
| system C | latest / `c162cb9a6f76` (image: `system C/system C:latest`, pulled 2026-07-13; driver: `system A` 6.2.0 via bolt fallback) |

---

## Honesty notes

- **mushroomdb** numbers are **embedded Rust** (no network RTT, no serialization overhead).
  system B is also embedded — its numbers are directly comparable to mushroomdb's.
  system A and system C numbers go over bolt/localhost (~0.1–1 ms round-trip per query).
- **rule_derive** is mushroomdb-only — competitors have no auto-derivation equivalent.
  It is excluded from the cross-engine table. See `benchmarks/README.md` for the full explanation.
- **Sequential runs required** due to port conflict: system A and system C both default to
  `bolt://localhost:7687`. Run 1 (system A up) and Run 2 (system C up) were executed separately
  with the same dataset/seed. mushroomdb and system B results are drawn from Run 1.
  system C results are drawn from Run 2. See Provenance section below.
- **system C adapter schema note:** the system C adapter stores only the `key` field (not full
  node properties). Its `cypher_scan_filter` (`WHERE n.size_bucket = 3`) returns **0 rows**
  because `size_bucket` was never stored. Wall time is measured but is not semantically
  equivalent to the system A/mushroomdb scan (which return 1,400 matching rows).
- **mushroomdb cypher_two_hop error:** the harness query
  (`MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)-[:INDUSTRY_ALIGNMENT]->(t2:Talent) LIMIT 200`)
  triggered an engine error: *"intermediate result exceeds 1,000,000 rows"*.
  The query returned 0 rows. The rule_derive backfill generates a dense edge set;
  the query pattern without anchored variables causes a cartesian explosion before the LIMIT
  is applied. This is a known limitation — `wall_s` reflects the time to error, not a full result.

---

## Cross-engine comparison (wall time)

| workload | mushroomdb | system A | system B | system C |
|---|---|---|---|---|
| bulk_ingest | 8.761 s | 12.460 s | 1.21 min | 46.24 ms † |
| neighborhood_depth1 (p50) | 1.1 µs | 1.81 ms | 99.7 µs | 3.12 ms |
| neighborhood_depth1 (p95) | 14.0 µs | 12.81 ms | 476.5 µs | 3.90 ms |
| neighborhood_depth2 (p50) | 0.9 µs | 6.78 ms | 1.04 ms | 2.97 ms |
| cypher scan-filter-project | 5.11 ms | 84.81 ms | 1.78 ms | 11.56 ms † |
| cypher two-hop join | 1.273 s ‡ | 74.25 ms | 1.09 ms | 1.08 ms |

† system C adapter stores only `key` (not full props); bulk_ingest skips property serialization;
  cypher_scan_filter returns 0 rows (no `size_bucket` property stored). Not semantically
  equivalent to system A/mushroomdb results for those workloads.

‡ mushroomdb returned 0 rows with error: *intermediate result exceeds 1,000,000 rows*.
  Timing reflects time to error, not a complete result.

---

## mushroomdb — rule_derive (ours-only, excluded from cross-engine table)

> **Auto-derivation has no competitor equivalent.**
> Edges are derived automatically when rules are declared and on every subsequent
> ingest/update. Competitors require manual ETL / triggers. This workload is
> intentionally excluded from the cross-engine table.

- **Rules declared:** 2
- **Total backfill wall:** 20.728 s
  - `bench_industry_tc` (INDUSTRY_ALIGNMENT): 8.481 s
  - `bench_specialty_tc` (SPECIALTY_MATCH): 12.246 s

---

## Provenance / measurement notes

| Engine | Source run | Server state | Valid? |
|---|---|---|---|
| mushroomdb | Run 1 (system A up) | embedded, unaffected by bolt servers | YES |
| system A | Run 1 (system A up) | `bench-system A` (`system A:5-community`, `system A_AUTH=none`) on `:7687`; adapter auth `("system A","system A")` accepted | YES |
| system B | Run 1 (system A up) | embedded, unaffected by bolt servers | YES |
| system C (Run 1) | Run 1 (system A up) | system C adapter connected to system A on `:7687` (port conflict — confirmed by post-run node count: 20,000 nodes in system A after both adapters ingested) | **INVALID — excluded** |
| system A (Run 2) | Run 2 (system C up) | system A adapter connected to system C on `:7687` (same port conflict, reverse) | **INVALID — excluded** |
| system C | Run 2 (system C up) | `bench-system C` (`system C/system C:latest`) on `:7687`; adapter via system A driver bolt fallback | YES |
| system B (Run 2) | Run 2 (system C up) | same as Run 1 — consistent within 2% | consistent; Run 1 used |

**Port conflict detection method:** After Run 1 with system A up, queried system A directly:
`MATCH (n) RETURN labels(n)[0], count(n)` — result was 14,000 Talent / 4,000 Company / 2,000 Job
(20,000 total = 10,000 system A adapter + 10,000 system C adapter both wrote to system A).
This conclusively proves system C's Run 1 numbers measured system A, not system C.
