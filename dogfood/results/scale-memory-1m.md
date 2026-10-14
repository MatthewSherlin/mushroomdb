# Scale run — memory-shaped workload (1M-node)

## Machine / date

- **Date:** 2026-10-14 (runs: 13:43, 13:46, 13:54 UTC−0)
- **OS:** macOS-15.7.3-arm64-arm-64bit-Mach-O
- **CPU:** Apple M4 Pro (12 cores, arm64)
- **RAM:** 24.00 GiB
- **Python:** 3.14.6
- **mushroomdb wheel:** 0.8.0
- **Seed:** 20261014
- **Scales:** 100,000 / 300,000 / 1,000,000 nodes

Node mix per scale: Person 40 %, Org 10 %, Project 20 %, Concept 20 %, Event 10 %.
Edges per entity: MEMBER_OF (person→org), WORKS_ON (1–2/person→project), KNOWS (ring),
RELATED_TO (ring), ATTENDED (every 5th person→event).  One FieldEqual rule:
`same_team` (Person→Person on `team_id`), `max_edges=None` (global 1M-edge cap).

Peak RSS is `resource.ru_maxrss` (process-lifetime, Darwin bytes).  Current RSS
is `ps -o rss=` after the phase.  Bindings are embedded Rust via
`mushroomdb.GraphDb` — not HTTP.  This is a memory-schema workload (Person, Org,
Project, Concept, Event nodes) with **synthetic Zipf-distributed text**; numbers
are not apples-to-apples with any production deployment (different hardware,
no network, synthetic text, no real embeddings).

---

## Phase timings (all scales)

| Phase | scale | status | wall | peak RSS | RSS after | notes |
|---|---|---|---|---|---|---|
| schema | 100,000 | ok | 128.70 ms | 24.42 MiB | 24.42 MiB | |
| schema | 300,000 | ok | 277.43 ms | 23.95 MiB | 24.05 MiB | |
| schema | 1,000,000 | ok | 208.79 ms | 24.58 MiB | 24.58 MiB | |
| ingest | 100,000 | ok | 5.968 s | 284.61 MiB | 284.61 MiB | nodes=100k edges=164,031 tput=16,756 n/s |
| ingest | 300,000 | ok | 20.558 s | 770.73 MiB | 770.73 MiB | nodes=300k edges=491,984 tput=14,593 n/s |
| ingest | 1,000,000 | ok | 1.27 min | 2.09 GiB | 2.09 GiB | nodes=1M edges=1,639,993 tput=13,158 n/s |
| rules | 100,000 | ok | 1.741 s | 908.58 MiB | 866.77 MiB | same_team max=None tripped=True derived=1,000,000 |
| rules | 300,000 | ok | 2.309 s | 1.22 GiB | 1.19 GiB | same_team max=None tripped=True derived=1,000,000 |
| rules | 1,000,000 | ok | 2.922 s | 2.31 GiB | 2.31 GiB | same_team max=None tripped=True derived=1,000,000 |
| remember | 100,000 | ok | 3.01 min | 1.01 GiB | 806.41 MiB | p50=13.91 ms p95=42.78 ms n=10,000 |
| remember | 300,000 | ok | 5.30 min | 1.36 GiB | 987.19 MiB | p50=21.29 ms p95=77.75 ms n=10,000 |
| remember | 1,000,000 | ok | 7.16 min | 2.65 GiB | 2.26 GiB | p50=22.16 ms p95=80.09 ms n=10,000 |
| reopen_wal | 100,000 | ok | 2.039 s | 1.01 GiB | 836.31 MiB | WAL replay; rule re-application |
| reopen_wal | 300,000 | ok | 3.923 s | 1.36 GiB | 793.03 MiB | WAL replay; rule re-application |
| reopen_wal | 1,000,000 | ok | 11.261 s | 2.65 GiB | 1.27 GiB | WAL replay; rule re-application |
| reopen_snap | 100,000 | ok | 2.313 s | 1.13 GiB | 791.91 MiB | snap_write=2.200 s open=112.50 ms size=41.61 MiB |
| reopen_snap | 300,000 | ok | 2.677 s | 1.36 GiB | 692.34 MiB | snap_write=2.321 s open=356.21 ms size=79.28 MiB |
| reopen_snap | 1,000,000 | ok | 6.824 s | 2.65 GiB | 1.21 GiB | snap_write=5.645 s open=1.179 s size=209.31 MiB |
| recall | 100,000 | ok | — | — | — | p50=7.01 ms p95=10.33 ms p99=10.78 ms |
| recall | 300,000 | ok | — | — | — | p50=21.33 ms p95=28.09 ms p99=29.21 ms |
| recall | 1,000,000 | ok | — | — | — | p50=66.09 ms p95=99.41 ms p99=107.73 ms |
| lookup | 100,000 | ok | — | — | — | node_info p50=3.9 µs p95=10.6 µs \| node_edges p50=45.0 µs p95=67.5 µs |
| lookup | 300,000 | ok | — | — | — | node_info p50=4.8 µs p95=585.0 µs \| node_edges p50=53.1 µs p95=1.61 ms |
| lookup | 1,000,000 | ok | — | — | — | node_info p50=11.0 µs p95=582.2 µs \| node_edges p50=56.1 µs p95=303.2 µs |
| query | 100,000 | ok | — | — | — | count=40k wall=2.31 ms; 1-hop n=1 wall=18.70 ms |
| query | 300,000 | ok | — | — | — | count=120k wall=3.42 ms; 1-hop n=1 wall=80.32 ms |
| query | 1,000,000 | ok | — | — | — | count=400k wall=11.64 ms; 1-hop n=1 wall=281.55 ms (slow-query log) |
| incr | 100,000 | ok | — | — | — | set_prop p50=23.99 ms p95=42.58 ms n=100 |
| incr | 300,000 | ok | — | — | — | set_prop p50=27.52 ms p95=66.80 ms n=100 |
| incr | 1,000,000 | ok | — | — | — | set_prop p50=21.29 ms p95=69.42 ms n=100 |

---

## Key numbers across scales

| Metric | 100,000 | 300,000 | 1,000,000 |
|---|---|---|---|
| ingest wall | 5.968 s | 20.558 s | 1.27 min |
| ingest throughput (n/s) | 16,756 | 14,593 | 13,158 |
| ingest peak RSS | 284.61 MiB | 770.73 MiB | 2.09 GiB |
| store pre-snapshot (WAL) | 99.64 MiB | 132.91 MiB | 249.37 MiB |
| rules wall | 1.741 s | 2.309 s | 2.922 s |
| derived edges (same_team) | 1,000,000 | 1,000,000 | 1,000,000 |
| remember p50 | 13.91 ms | 21.29 ms | 22.16 ms |
| remember p95 | 42.78 ms | 77.75 ms | 80.09 ms |
| WAL reopen | 2.039 s | 3.923 s | 11.261 s |
| snapshot write | 2.200 s | 2.321 s | 5.645 s |
| snapshot open | 112.50 ms | 356.21 ms | 1.179 s |
| snap open / WAL reopen ratio | 18x faster | 11x faster | 9.6x faster |
| store post-snapshot | 41.61 MiB | 79.28 MiB | 209.31 MiB |
| peak RSS (all phases) | 1.13 GiB | 1.36 GiB | 2.65 GiB |
| recall p50 | 7.01 ms | 21.33 ms | 66.09 ms |
| recall p95 | 10.33 ms | 28.09 ms | 99.41 ms |
| recall p99 | 10.78 ms | 29.21 ms | 107.73 ms |
| node_info p50 | 3.9 µs | 4.8 µs | 11.0 µs |
| set_prop p50 (post-open) | 23.99 ms | 27.52 ms | 21.29 ms |
| disk free after deletion | 15.28 GiB | 14.94 GiB | 14.84 GiB |

---

## Recall latency by query shape

All queries return ≤6 hits.  n=75 per shape, 300 total per scale.
Same rare/common/name words at all scales (seed-stable): rare=`vikixim`, common=`bababey`, stored-name=`cuvep babobig`.

| shape | scale | p50 | p95 | mean hits |
|---|---|---|---|---|
| rare | 100,000 | 181.5 µs | 253.4 µs | 6.0 |
| rare | 300,000 | 425.8 µs | 580.1 µs | 6.0 |
| rare | 1,000,000 | 1.24 ms | 1.48 ms | 6.0 |
| common | 100,000 | 9.54 ms | 10.55 ms | 6.0 |
| common | 300,000 | 26.52 ms | 28.96 ms | 6.0 |
| common | 1,000,000 | 92.23 ms | 105.38 ms | 6.0 |
| two_word | 100,000 | 9.35 ms | 10.34 ms | 6.0 |
| two_word | 300,000 | 26.42 ms | 28.24 ms | 6.0 |
| two_word | 1,000,000 | 92.62 ms | 103.72 ms | 6.0 |
| stored_name | 100,000 | 4.31 ms | 5.15 ms | 6.0 |
| stored_name | 300,000 | 10.95 ms | 12.14 ms | 6.0 |
| stored_name | 1,000,000 | 36.28 ms | 41.05 ms | 6.0 |

Rare-word queries are sub-millisecond even at 1M because the posting list is short.
Common-word and two-word queries scale with corpus size: BM25 scores all candidates
in the posting list before returning the top-6.  At 1M nodes, a common word appears
in ~80k postings; scoring them takes ~90 ms.

---

## Reopen: WAL vs snapshot (0.8 full-text-in-snapshot)

**WAL-only reopen** replays all WAL frames including every `CreateNode` and the
`CreateRule` record for `same_team`.  The FieldEqual rule re-fires on every Person
node during replay.  The full-text index is rebuilt at open — it is not stored in
the WAL.  WAL reopen cost therefore scales with both the node count and the WAL
frame count (which includes 10k `remember` frames at all scales).

**Snapshot open (0.8)** reads the full-text index directly from the snapshot,
skipping the rebuild.  Rules do not re-fire.  Snapshot includes topology +
provenance + full-text index.

| scale | WAL reopen | snap write | snap open | speedup |
|---|---|---|---|---|
| 100,000 | 2.039 s | 2.200 s | 112.50 ms | 18.1× |
| 300,000 | 3.923 s | 2.321 s | 356.21 ms | 11.0× |
| 1,000,000 | 11.261 s | 5.645 s | 1.179 s | 9.6× |

At 1M nodes the snapshot open (1.2 s) is 9.6× faster than WAL reopen (11.3 s).
The ratio falls as the store grows: 18× at 100k, 11× at 300k.

Snapshot store size is compact: 209 MiB for 1M nodes, 1.64M structural edges, 1M
derived edges, 10k `remember` provenance records, and the full-text index.

---

## FieldEqual rule (same_team)

The `same_team` rule (Person→Person FieldEqual on `team_id`, `max_edges=None`)
uses the global 1M-edge cap.  With 80 Persons per team at 100k scale
(400k persons ÷ 5k teams), each person could match 79 others (79 < 1M),
but the global total would reach 31.6M — well above 1M.  The cap trips at 1M
derived edges at all three scales.

| scale | rule wall | tripped | derived edges |
|---|---|---|---|
| 100,000 | 1.741 s | True | 1,000,000 |
| 300,000 | 2.309 s | True | 1,000,000 |
| 1,000,000 | 2.922 s | True | 1,000,000 |

The rule trips its budget at every scale: a tripped rule stops deriving, so the
store holds 1,000,000 of the edges the rule would derive and `stats` reports it
tripped. These timings are therefore the time to reach the cap, not to derive the
rule's whole edge set, and the same is true of every later phase's view of
`same_team`. A rule meant to be complete at this size needs `max_edges` per source
or a narrower predicate.

---

## Engine defects found

- **Slow-query log at 1M: 1-hop Cypher traversal** — the query
  `MATCH (p:Person)-[:KNOWS]->(q) WHERE key(p) = $k RETURN key(q) AS dst`
  logged a slow-query warning at 281 ms (threshold 250 ms) at scale=1M.
  At 100k this query ran in 18.70 ms; at 300k, 80.32 ms; at 1M, 281.55 ms.
  The latency grows with the number of Person nodes, which suggests the
  `key(p) = $k` filter is applied after a scan of the label rather than as a
  lookup; not investigated further here. Results were correct. Ledger row 102.

---

## What this shows / what it does not

**Shows:**
- Bulk ingest throughput and peak RSS at 100k, 300k, 1M memory-schema entities
  with Zipf-distributed synthetic text (no embeddings).
- Per-call `remember` write-path latency: 13.9 ms p50 at 100k, 21.3 ms at 300k,
  22.2 ms at 1M. Each call commits durably; how much of that is the fsync was not
  measured.
- `recall` latency across four query shapes (the primary assistant read path):
  rare-word queries stay sub-millisecond; common-word queries scale ~10× over
  the 10× node range.
- Open after a snapshot against a WAL-only reopen at each scale: 9.6× to 18×
  faster. The snapshot open skips the WAL replay, the rule re-firing and (since
  0.8) the full-text rebuild; this run does not separate the three.
- `node_info`, `node_edges`, Cypher aggregate, and `set_prop` as single-node
  operations across the full scale range.
- Peak RSS: 2.65 GiB at 1M nodes, across every phase.

**Does not show:**
- Semantic / vector similarity (no embeddings in this workload).
- Multi-process or HTTP access.
- Production hardware (local embedded, Apple M-series, warm disk cache).
- Multi-rule backfill, or a rule that derives its whole edge set at this size: the one rule here trips its budget.
- Real assistant text (synthetic Zipf vocabulary, 3000-word corpus).

---

## Exact commands

```bash
python dogfood/scale_memory.py --scale 100000 --seed 20261014
python dogfood/scale_memory.py --scale 300000 --seed 20261014
python dogfood/scale_memory.py --scale 1000000 --seed 20261014
```

Harness: `dogfood/scale_memory.py` (committed alongside these results).
Wheel: `mushroomdb==0.8.0` (PyPI).
