# Engine caps measured at 1,000,000 nodes (ledger row 62)

- **Date:** 2026-10-20
- **Machine:** Apple M4 Pro, 12 cores, 24 GiB, macOS 15.7.3
- **Build:** `cargo run --release -p mushroomdb --example caps_bench`, rustc 1.92.0, base `1200cdf`
- **Command:** `CAPS_N=1000000 CAPS_DIR=/tmp/caps1m ./target/release/examples/caps_bench`
- **Data:** synthetic Zipf text, memory-shaped (Person 40 %, Org 10 %, Project 20 %, Concept 20 %, Event 10 %, about 1.6 edges per node), 1,000,300 nodes (300 are planted needles for the floor test). Store A declares the 8 memory-schema pairs; store B holds the same text spread over 32 declared pairs.
- **Counts come from the engine** (`RecallRows::terms`, `RememberReport::provisional`, `provisional_capped`, `fulltext_declared`), not from a re-implementation. Only the floor test re-asks `recall_rows`' own retention rule at other thresholds.
- Single run, one machine. Write latencies are fsync-bound and noisy (p95 spikes); read the medians.

```text
# caps_bench N=1000000 build=release os=macos cores=12
build: 1000000 nodes, spread=false, pairs=8 in 79.3s, rss 1955 MiB
snapshot written, store 125.9 MiB
build: 1000000 nodes, spread=true, pairs=32 in 52.3s, rss 1820 MiB
snapshot written, store 125.9 MiB

open from snapshot, 8 pairs: median 5185.114 ms (runs 5185.114 ms, 5467.233 ms, 4937.063 ms, 5294.336 ms, 5131.916 ms)
open from snapshot, 32 pairs: median 3620.312 ms (runs 3620.312 ms, 3397.219 ms, 3345.265 ms, 3683.010 ms, 3959.816 ms)
store size: 8 pairs 125.9 MiB, 32 pairs 125.9 MiB
store A: 1000300 nodes, 8 pairs, rss 1321 MiB

## cap 1: MAX_QUERY_TERMS = 24
stand-in prompts: 2018 paragraphs of docs/site; terms/prompt min 0 p50 14 p90 24 p99 24 max 24; at the cap 500 (24.8 %); recall p50 1.139 ms p95 1.209 ms
| terms | word source | p50 | p95 | note |
|---|---|---|---|---|
| 1 | zipf | 2.444 ms | 97.089 ms |  |
| 2 | zipf | 48.707 ms | 124.432 ms |  |
| 4 | zipf | 96.112 ms | 141.388 ms |  |
| 8 | zipf | 142.009 ms | 191.322 ms |  |
| 12 | zipf | 173.001 ms | 231.036 ms |  |
| 24 | zipf | 239.901 ms | 294.098 ms | the cap |
| 32 | zipf | 289.265 ms | 330.614 ms | past the cap: not reachable through recall |
| 48 | zipf | 353.013 ms | 391.122 ms | past the cap: not reachable through recall |
| 96 | zipf | 459.846 ms | 488.646 ms | past the cap: not reachable through recall |
| 1 | head-100 | 4.844 ms | 44.714 ms |  |
| 2 | head-100 | 7.725 ms | 90.195 ms |  |
| 4 | head-100 | 16.222 ms | 114.754 ms |  |
| 8 | head-100 | 41.359 ms | 77.500 ms |  |
| 12 | head-100 | 66.831 ms | 140.853 ms |  |
| 24 | head-100 | 169.146 ms | 268.170 ms | the cap |
| 32 | head-100 | 216.816 ms | 279.265 ms | past the cap: not reachable through recall |
| 48 | head-100 | 309.991 ms | 392.709 ms | past the cap: not reachable through recall |
| 96 | head-100 | 587.997 ms | 624.676 ms | past the cap: not reachable through recall |
400-word topic -> RecallRows::terms = 24

## cap 2: the half-coverage floor (present * 2 >= terms)

planted-target recall (target is the only node with its words; 300 per cell)
| terms (real/total) | 1/4 | 1/3 | 1/2 (shipped) | 2/3 | all |
|---|---|---|---|---|---|
| 1/1 | 300 | 300 | 300 | 300 | 300 |
| 2/2 | 300 | 300 | 300 | 300 | 300 |
| 1/2 | 300 | 300 | 300 | 0 | 0 |
| 2/3 | 300 | 300 | 300 | 300 | 0 |
| 1/3 | 296 | 296 | 0 | 0 | 0 |
| 2/4 | 300 | 300 | 300 | 0 | 0 |
| 1/4 | 282 | 0 | 0 | 0 | 0 |
| 3/5 | 300 | 300 | 300 | 0 | 0 |
| 2/5 | 300 | 300 | 0 | 0 | 0 |
| 1/5 | 0 | 0 | 0 | 0 | 0 |

false positives: 300 noise topics per row of unrelated vocabulary words (rank 100..3000), topics with >= 1 surviving hit
| terms | 1/4 | 1/3 | 1/2 (shipped) | 2/3 | all |
|---|---|---|---|---|---|
| 1 | 300 | 300 | 300 | 300 | 300 |
| 2 | 300 | 300 | 300 | 53 | 53 |
| 3 | 300 | 300 | 104 | 104 | 0 |
| 4 | 300 | 169 | 169 | 0 | 0 |
| 6 | 251 | 251 | 1 | 0 | 0 |
| 10 | 2 | 0 | 0 | 0 | 0 |

## analyze (the four kinds, budget_ms 0 as the tool runs them)
central (pagerank): 1000300 nodes scored, converged=true, 843.622 ms
degree: 1000300 nodes scored, 461.379 ms
components (wcc): 1000300 rows, 631.382 ms
clusters (louvain): 380 rows, 5277.464 ms
rss 1004 MiB

## cap 4: MAX_PROVISIONAL_PER_COMMIT = 20
| unknown `about` keys in one call | calls | p50 | p95 | stubs created (engine report) | capped (engine report) |
|---|---|---|---|---|---|
| 0 | 60 | 21.034 ms | 195.302 ms | 0 | 0 |
| 1 | 60 | 19.079 ms | 40.701 ms | 1 | 0 |
| 5 | 60 | 16.893 ms | 34.980 ms | 5 | 0 |
| 10 | 60 | 17.539 ms | 55.710 ms | 10 | 0 |
| 20 | 60 | 20.051 ms | 63.212 ms | 20 | 0 |
| 21 | 60 | 17.841 ms | 55.146 ms | 20 | 1 |
| 40 | 60 | 19.006 ms | 47.570 ms | 20 | 20 |
| 100 | 60 | 21.699 ms | 319.607 ms | 20 | 80 |

## cap 3: MAX_FULLTEXT_PAIRS = 32
store A: declared pairs 8; remember (1 known about key) p50 14.262 ms p95 29.592 ms over 300 alternating calls
store B: declared pairs 32; remember (1 known about key) p50 14.555 ms p95 32.844 ms over 300 alternating calls
store A: one call naming 40 fresh labels: declared 24 (engine report), pairs now 32, took 352.749 ms
store B: one call naming 40 fresh labels: declared 0 (engine report), pairs now 32, took 21.368 ms

## cost of declaring one pair on an already-populated label
enable_fulltext(Person, team_id) on populated label: 423.260 ms
enable_fulltext(Org, team_id) on populated label: 111.124 ms
```
