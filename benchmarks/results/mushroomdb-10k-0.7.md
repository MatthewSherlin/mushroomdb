# Benchmark — mushroomdb

## Machine / date

- **Date:** 2026-10-01T16:06:43
- **Host:** Matthews-MBP.lan
- **OS:** macOS-15.7.3-arm64-arm-64bit
- **CPU:** Apple M4 Pro (12 cores, arm64)
- **RAM:** 24.00 GiB
- **Python:** 3.11.14
- **Scale:** 10,000 nodes (seed=20260819, 70/20/10 Talent/Company/Job split)
- **Tree:** 0.7.0 pre-release, commit 60c62cc, release build of the binding

## What these numbers are

Embedded: the engine runs in this process, so there is no network round-trip and
no serialization in any figure. One run on one machine; see `benchmarks/README.md`
for the workloads and how to reproduce it.

## Workloads (wall time)

| workload | mushroomdb |
| --- | --- |
| bulk_ingest | 1.076 s |
| neighborhood_depth1 (p50) | 0.4 µs |
| neighborhood_depth1 (p95) | 2.4 µs |
| neighborhood_depth2 (p50) | 0.2 µs |
| cypher scan-filter-project | 1.02 ms |
| cypher two-hop join | 421.0 µs |

## mushroomdb — bulk ingest throughput

- **Nodes ingested:** 10,000
- **Wall time:** 1.076 s
- **Throughput:** 9.3k nodes/s
- **Chunk size:** 2k nodes / ingest_batch call (5 sequential chunks at 10k scale)

## mushroomdb — neighbourhood latencies

- **depth-1 (node_edges):** p50=0.4 µs p95=2.4 µs (n=20)
- **depth-2 (node_edges + neighbors + second-hop):** p50=0.2 µs p95=0.8 µs (n=20)

## mushroomdb — Cypher workloads

- **scan-filter-project** (`MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key`): rows=1400 wall=1.02 ms
- **two-hop join** (`MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)<-[:INDUSTRY_ALIGNMENT]-(t2:Talent) LIMIT 200`): rows=200 wall=421.0 µs

## mushroomdb — rule_derive (ours-only)

Edges are derived when a rule is declared and on every later write that changes
what it reads. This is the cost of declaring the rules over the loaded graph.

- **Rules declared:** 2
- **Total backfill wall:** 8.781 s
  - `bench_industry_tc` (INDUSTRY_ALIGNMENT): 1.675 s
  - `bench_specialty_tc` (SPECIALTY_MATCH): 7.106 s

