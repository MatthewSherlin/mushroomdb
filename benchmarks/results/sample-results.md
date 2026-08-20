# Comparative benchmark — mushroomdb

## Machine / date

- **Date:** 2026-08-20T07:45:17
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

| workload | mushroomdb | neo4j | kuzu | memgraph |
| --- | --- | --- | --- | --- |
| bulk_ingest | 31.211 s | not installed — skipped | not installed — skipped | not installed — skipped |
| neighborhood_depth1 (p50) | 1.1 µs | not installed — skipped | not installed — skipped | not installed — skipped |
| neighborhood_depth1 (p95) | 3.9 µs | not installed — skipped | not installed — skipped | not installed — skipped |
| neighborhood_depth2 (p50) | 0.8 µs | not installed — skipped | not installed — skipped | not installed — skipped |
| cypher scan-filter-project | 4.55 ms | not installed — skipped | not installed — skipped | not installed — skipped |
| cypher two-hop join | 1.155 s | not installed — skipped | not installed — skipped | not installed — skipped |

## mushroomdb — bulk ingest throughput

- **Nodes ingested:** 10,000
- **Wall time:** 31.211 s
- **Throughput:** 320 nodes/s
- **Chunk size:** 10k nodes / ingest_batch call

## mushroomdb — neighbourhood latencies

- **depth-1 (node_edges):** p50=1.1 µs p95=3.9 µs (n=20)
- **depth-2 (node_edges + neighbors + second-hop):** p50=0.8 µs p95=3.6 µs (n=20)

## mushroomdb — Cypher workloads

- **scan-filter-project** (`MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key`): rows=1400 wall=4.55 ms
- **two-hop join** (`...INDUSTRY_ALIGNMENT... LIMIT 200`): rows=0 wall=1.155 s

## mushroomdb — rule_derive (ours-only)

> **Auto-derivation has no competitor equivalent.**
> Edges are derived automatically when rules are declared and on every
> subsequent ingest/update. Competitors require manual ETL / triggers.
> This workload is intentionally excluded from the cross-engine table.
> See `benchmarks/README.md` for the full explanation.

- **Rules declared:** 2
- **Total backfill wall:** 20.532 s
  - `bench_industry_tc` (INDUSTRY_ALIGNMENT): 8.331 s
  - `bench_specialty_tc` (SPECIALTY_MATCH): 12.202 s

## Competitors

- **neo4j:** not installed — skipped  
  _Neo4j adapter: 'neo4j' Python driver not installed — install with 'pip install neo4j' to enable Neo4j benchmarks._
- **kuzu:** not installed — skipped  
  _'kuzu' not installed — pip install kuzu (No module named 'kuzu')_
- **memgraph:** not installed — skipped  
  _Memgraph adapter: no bolt driver installed — install with 'pip install mgclient' or 'pip install neo4j' to enable Memgraph benchmarks._

