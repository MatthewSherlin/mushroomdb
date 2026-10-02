# Benchmark — mushroomdb

## Machine / date

- **Date:** 2026-10-01T16:06:43
- **Host:** an Apple M4 Pro laptop (the hardware is in the next lines)
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

## Annotations (added 2026-10-01, after the run)

Nothing above this heading was changed, with one exception: the **Host** line gave the
machine's network name and now describes the machine instead. These lines record what the run
did not.

- **Command:** `bindings/python/.venv/bin/python benchmarks/run.py --scale 10000 --out benchmarks/results/mushroomdb-10k-0.7.md`,
  from the repository root, straight after
  `(cd bindings/python && CARGO_NET_OFFLINE=true PIP_NO_INDEX=1 .venv/bin/maturin develop --release)`.
- **Load:** load not recorded; other sessions were active on this machine. The only reading is
  one taken about eight minutes after the run (16:14): load averages 3.66 / 4.66 / 5.22 over 1, 5
  and 15 minutes, on 12 cores. The 15-minute window covers the run and the release build before it.
- **Method:** one run, each workload timed once, no warmup and no repeats. The two-hop join is a
  single pass (`benchmarks/adapters/ours.py:164-167`); the edge count behind it was not recorded.

Earlier committed figures for the rows that moved:

| workload | this run | earlier | where |
| --- | --- | --- | --- |
| bulk_ingest | 1.076 s | 784 ms (2026-08-21, single shot); other single passes 797.75 ms (2026-08-24), 931.07 ms and 989.73 ms (2026-08-21) | `head-to-head-10k-v2.md:565`; `regression-v0.1.1-20260824.md`, `regression-v0.1-20260821.md` |
| cypher two-hop join | 421.0 µs, single pass | 261.6 µs, median of 10 after 3 warmups over 5,810,000 edges (2026-08-21); single passes 185.8 µs (2026-08-24), 206.7 µs and 325.6 µs (2026-08-21) | `head-to-head-10k-v2.md:343-352`; the two regression files |
| rule_derive (total) | 8.781 s | 2.849 s, 2.929 s, 3.149 s, 3.493 s, 3.514 s (2026-08-21 and 2026-08-24) | `regression-v0.1-20260821.md:36` and its note; `regression-v0.1.1-20260824.md:38` |

**Conclusion (added 2026-10-01, after an A/B against v0.6.12): no 0.7 regression.** See
`mushroomdb-10k-0.7-ab.md`. The released v0.6.12 takes 8.611 s for the same backfill and derives the
same 448,000 edges. The 2.85–3.51 s runs were a different workload: before v0.2.0 a rule without an
explicit `max_edges` stopped at a 1,000,000-edge global cap per rule, and since v0.2.0 it keeps the
top 32 per source when declared through the Python binding, as this harness does, so the backfill
evaluates every source. The two-hop figure above is a cold first
execution; the warm median on this tree is 192.1 µs. An earlier annotation here, made before the
A/B, said the backfill was under investigation as a possible regression; this replaces it.
