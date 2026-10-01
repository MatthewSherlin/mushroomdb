# Comparative benchmark — mushroomdb

## Machine / date

- **Date:** 2026-08-20T08:00:27
- **Host:** mac.lan
- **OS:** macOS-15.7.3-arm64-arm-64bit
- **CPU:** Apple M4 Pro (12 cores, arm64)
- **RAM:** 24.00 GiB
- **Python:** 3.12.12
- **Scale:** 10,000 nodes (seed=20260819, 70/20/10 Talent/Company/Job split)

## Honesty note

See `benchmarks/README.md` for the full honesty section.
Short version: mushroomdb numbers are **embedded Rust** (no network RTT);
competitor numbers are over bolt/localhost. `rule_derive` is ours-only —
competitors have no auto-derivation equivalent, so it is excluded from
the cross-engine table.

## Cross-engine comparison (wall time)

| workload | mushroomdb | system A | system B | system C |
| --- | --- | --- | --- | --- |
| bulk_ingest | 8.647 s | not installed — skipped | not installed — skipped | not installed — skipped |
| neighborhood_depth1 (p50) | 1.0 µs | not installed — skipped | not installed — skipped | not installed — skipped |
| neighborhood_depth1 (p95) | 3.5 µs | not installed — skipped | not installed — skipped | not installed — skipped |
| neighborhood_depth2 (p50) | 0.8 µs | not installed — skipped | not installed — skipped | not installed — skipped |
| cypher scan-filter-project | 4.61 ms | not installed — skipped | not installed — skipped | not installed — skipped |
| cypher two-hop join | 1.278 s | not installed — skipped | not installed — skipped | not installed — skipped | ¹

## mushroomdb — bulk ingest throughput

- **Nodes ingested:** 10,000
- **Wall time:** 8.647 s
- **Throughput:** 1.2k nodes/s
- **Chunk size:** 2k nodes / ingest_batch call (5 sequential chunks at 10k scale)

## mushroomdb — neighbourhood latencies

- **depth-1 (node_edges):** p50=1.0 µs p95=3.5 µs (n=20)
- **depth-2 (node_edges + neighbors + second-hop):** p50=0.8 µs p95=1.7 µs (n=20)

## mushroomdb — Cypher workloads

- **scan-filter-project** (`MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key`): rows=1400 wall=4.61 ms
- **two-hop join** (`MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)<-[:INDUSTRY_ALIGNMENT]-(t2:Talent) LIMIT 200`): rows=0 wall=1.278 s — query error: execute: intermediate result exceeds 1000000 rows — the executor has no LIMIT pushdown yet (roadmap item): the join materializes before LIMIT applies; add a LIMIT or constrain patterns with shared variables

## mushroomdb — rule_derive (ours-only)

> **Auto-derivation has no competitor equivalent.**
> Edges are derived automatically when rules are declared and on every
> subsequent ingest/update. Competitors require manual ETL / triggers.
> This workload is intentionally excluded from the cross-engine table.
> See `benchmarks/README.md` for the full explanation.

- **Rules declared:** 2
- **Total backfill wall:** 20.518 s
  - `bench_industry_tc` (INDUSTRY_ALIGNMENT): 8.326 s
  - `bench_specialty_tc` (SPECIALTY_MATCH): 12.192 s

¹ 0 rows at 10k: the full join exceeds the 1M intermediate-row budget before LIMIT (see detail below); direction and traversal proven at 200-node scale.

## Competitors

- **system A:** not installed — skipped  
  _system A adapter: 'system A' Python driver not installed — install with 'pip install system A' to enable system A benchmarks._
- **system B:** not installed — skipped  
  _'system B' not installed — pip install system B (No module named 'system B')_
- **system C:** not installed — skipped  
  _system C adapter: no bolt driver installed — install with 'pip install a named system' or 'pip install system A' to enable system C benchmarks._

