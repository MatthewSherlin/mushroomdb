# Head-to-head benchmark — mushroomdb vs. Neo4j / KùzuDB / Memgraph

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
| neo4j | 5.26.29 (image: `neo4j:5-community`; driver: `neo4j` 6.2.0) |
| kuzu | 0.11.3 (pip, embedded) |
| memgraph | latest / `c162cb9a6f76` (image: `memgraph/memgraph:latest`, pulled 2026-07-13; driver: `neo4j` 6.2.0 via bolt fallback) |

---

## Honesty notes

- **mushroomdb** numbers are **embedded Rust** (no network RTT, no serialization overhead).
  KùzuDB is also embedded — its numbers are directly comparable to mushroomdb's.
  Neo4j and Memgraph numbers go over bolt/localhost (~0.1–1 ms round-trip per query).
- **rule_derive** is mushroomdb-only — competitors have no auto-derivation equivalent.
  It is excluded from the cross-engine table. See `benchmarks/README.md` for the full explanation.
- **Sequential runs required** due to port conflict: Neo4j and Memgraph both default to
  `bolt://localhost:7687`. Run 1 (neo4j up) and Run 2 (memgraph up) were executed separately
  with the same dataset/seed. mushroomdb and KùzuDB results are drawn from Run 1.
  Memgraph results are drawn from Run 2. See Provenance section below.
- **Memgraph adapter schema note:** the memgraph adapter stores only the `key` field (not full
  node properties). Its `cypher_scan_filter` (`WHERE n.size_bucket = 3`) returns **0 rows**
  because `size_bucket` was never stored. Wall time is measured but is not semantically
  equivalent to the neo4j/mushroomdb scan (which return 1,400 matching rows).
- **mushroomdb cypher_two_hop error:** the harness query
  (`MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)-[:INDUSTRY_ALIGNMENT]->(t2:Talent) LIMIT 200`)
  triggered an engine error: *"intermediate result exceeds 1,000,000 rows"*.
  The query returned 0 rows. The rule_derive backfill generates a dense edge set;
  the query pattern without anchored variables causes a cartesian explosion before the LIMIT
  is applied. This is a known limitation — `wall_s` reflects the time to error, not a full result.

---

## Cross-engine comparison (wall time)

| workload | mushroomdb | neo4j | kuzu | memgraph |
|---|---|---|---|---|
| bulk_ingest | 8.761 s | 12.460 s | 1.21 min | 46.24 ms † |
| neighborhood_depth1 (p50) | 1.1 µs | 1.81 ms | 99.7 µs | 3.12 ms |
| neighborhood_depth1 (p95) | 14.0 µs | 12.81 ms | 476.5 µs | 3.90 ms |
| neighborhood_depth2 (p50) | 0.9 µs | 6.78 ms | 1.04 ms | 2.97 ms |
| cypher scan-filter-project | 5.11 ms | 84.81 ms | 1.78 ms | 11.56 ms † |
| cypher two-hop join | 1.273 s ‡ | 74.25 ms | 1.09 ms | 1.08 ms |

† memgraph adapter stores only `key` (not full props); bulk_ingest skips property serialization;
  cypher_scan_filter returns 0 rows (no `size_bucket` property stored). Not semantically
  equivalent to neo4j/mushroomdb results for those workloads.

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
| mushroomdb | Run 1 (neo4j up) | embedded, unaffected by bolt servers | YES |
| neo4j | Run 1 (neo4j up) | `bench-neo4j` (`neo4j:5-community`, `NEO4J_AUTH=none`) on `:7687`; adapter auth `("neo4j","neo4j")` accepted | YES |
| kuzu | Run 1 (neo4j up) | embedded, unaffected by bolt servers | YES |
| memgraph (Run 1) | Run 1 (neo4j up) | memgraph adapter connected to neo4j on `:7687` (port conflict — confirmed by post-run node count: 20,000 nodes in neo4j after both adapters ingested) | **INVALID — excluded** |
| neo4j (Run 2) | Run 2 (memgraph up) | neo4j adapter connected to memgraph on `:7687` (same port conflict, reverse) | **INVALID — excluded** |
| memgraph | Run 2 (memgraph up) | `bench-memgraph` (`memgraph/memgraph:latest`) on `:7687`; adapter via neo4j driver bolt fallback | YES |
| kuzu (Run 2) | Run 2 (memgraph up) | same as Run 1 — consistent within 2% | consistent; Run 1 used |

**Port conflict detection method:** After Run 1 with neo4j up, queried neo4j directly:
`MATCH (n) RETURN labels(n)[0], count(n)` — result was 14,000 Talent / 4,000 Company / 2,000 Job
(20,000 total = 10,000 neo4j adapter + 10,000 memgraph adapter both wrote to neo4j).
This conclusively proves memgraph's Run 1 numbers measured neo4j, not memgraph.
