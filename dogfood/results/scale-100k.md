# Scale run — marketplace dogfood (100k protocol)

## Machine / date

- **Date:** 2026-08-20T19:46:29
- **Host:** mac.lan
- **OS:** macOS-15.7.3-arm64-arm-64bit
- **CPU:** Apple M4 Pro (12 cores, arm64)
- **RAM:** 24.00 GiB
- **Python:** 3.12.12
- **Seed:** 20260819
- **Scale:** 100000 nodes (70000 Talent + 20000 Company + 10000 Job + 500 User)

Peak RSS is `resource.ru_maxrss` (process-lifetime, Darwin bytes).
Current RSS is `ps -o rss=` after the phase. Bindings are embedded Rust
via `mushroomdb.GraphDb` — not HTTP. Numbers are labeled **not
apples-to-apples** vs the marketplace production stack (different
hardware, no network, synthetic embeddings).

## Phase timings

| Phase | status | wall | peak RSS (lifetime) | RSS after | notes |
|---|---|---|---|---|---|
| ingest | ok | 1.46 min | 2.09 GiB | 1.38 GiB | ingest_batch 10k chunks (T2); FK rules declared inline |
| backfill | ok | 21.228 s | 2.58 GiB | 2.04 GiB | T1 streaming; max_edges=1M caps; all non-semantic rules |
| semantic | extrapolated | 46.17 ms | 2.58 GiB | 2.04 GiB | 5k probe recorded (T3 early-exit); full 100k ScanAll not attempted (blocking); approximate semantic runs instead |
| semantic_approx | ok | 8.37 min | 7.04 GiB | 2.34 GiB | edges=1000000 recall=0.080 precision=1.000 |
| incremental | ok | 1.779 s | 7.04 GiB | 1.32 GiB | p50=17.26 ms p95=32.23 ms n=100 |
| big3 | ok | 32.11 ms | 7.04 GiB | 1.32 GiB | p50=7.8 µs p95=18.0 µs n=50 mean_matches=0.0 |
| big3_slice | ok | 34.03 ms | 7.04 GiB | 1.68 GiB | 500T×500C metro/industry slice (all 3 rules fire uncapped) |
| explain | ok | 90.33 ms | 7.04 GiB | 1.37 GiB | p50=112.1 µs p95=570.7 µs n=100 |
| reopen | ok | 8.86 min | 8.48 GiB | 1.45 GiB | WAL reopen: rules re-fire on open() (derived edges not persisted) |
| reopen_snap | ok | 47.252 s | 8.48 GiB | 3.68 GiB | snapshot V4: snapshot() + close + open; derived edges + IVF centroids loaded from snapshot; no rule re-fire. write=36.105 s + open=11.148 s |

## Semantic phases (phase 3)

- **Exact status:** `extrapolated`
- **Attempted full 100000:** False
- **Method:** 5k ScanAll probe with T3 early-exit; t_full = t_probe * (n_t_full/n_t_probe) * (n_c_full/n_c_probe)
- **5k probe (T3 early-exit):** scale=5000 pairs=3500000 wall=18.690 s edges=111696 Δrss=0 B
- **Extrapolation:** factor=400.0 pairs_full=1400000000 projected_wall=124.60 min projected_Δrss=0 B under_30min=False under_8GiB=True
- **O(n²) method (binding):** `t_full = t_probe * (n_t_full/n_t_probe) * (n_c_full/n_c_probe)`. ScanAll evaluates every Talent×Company pair (not the passing subset). Full attempt only if projected wall < 1800s AND projected Δrss < 8.00 GiB.

### Approximate semantic (T4)

- **Method:** IVF-Flat approximate (T4: approximate=True in RuleDef)
- **Edges materialized:** 1000000
- **Wall:** 8.37 min

  **Set-coverage recall** (measured): fraction of ALL threshold-passing global pairs
  stored in the 1M-edge materialized set.  NOT the per-query IVF recall the
  ≥0.90 spec floor applies to.  At 70k×20k with 1M cap, ~3% of global positives
  are stored (cap_size/total_positives ceiling), so set-coverage recall is bounded at
  ~3% regardless of IVF quality.
- **Set-cov recall (n=1000 random pairs):** 0.080
- **Set-cov precision:** 1.000
- **TP/FP/FN/TN:** 2/0/23/975
- **Ground-truth positives in sample:** 25 (cosine ≥ 0.85)

  **Per-query IVF recall** (spec-floor metric): fraction of a Talent node's exact
  cosine≥0.85 Company neighbors returned by the IVF-Flat index (uncapped, measured
  on a fresh 5000-node probe graph where cap does not interfere).
- **Per-query recall (n=100 queries evaluated):** mean=0.991 median=1.000 min=0.625 max=1.000
- **Queries skipped (empty exact set):** 0

## Backfill (phase 2) — streaming with caps (T1)

- **Status:** `ok`
- **Method:** streaming backfill with max_edges caps (T1)
- **max_edges cap per rule:** 1,000,000 (ENGINE_EDGE_BUDGET)
  - `industry_alignment_tc`: 1.216 s edges=1000000 tripped=True Δrss=101.58 MiB
  - `industry_alignment_tj`: 1.202 s edges=2000000 tripped=True Δrss=166.11 MiB
  - `specialty_match_tc`: 2.573 s edges=1000000 tripped=True Δrss=0 B
  - `specialty_match_tj`: 2.658 s edges=2000000 tripped=True Δrss=637.67 MiB
  - `location_fit_tc`: 1.757 s edges=1000000 tripped=True Δrss=242.70 MiB
  - `location_fit_tj`: 1.667 s edges=2000000 tripped=True Δrss=89.14 MiB
  - `similar_size_tc`: 1.706 s edges=1000000 tripped=True Δrss=0 B
  - `matches_design_style_tc`: 4.843 s edges=1000000 tripped=True Δrss=0 B
  - `similar_size_strict_tc`: 1.653 s edges=1000000 tripped=True Δrss=159.14 MiB

**T1 change:** The engine now streams the desired set directly into the
store rather than building a `BTreeMap<(src,dst), score>` first.
Combined with explicit `max_edges` caps, cartesian predicates at 70k×20k
no longer OOM the process. Uncapped low-selectivity rules are still O(pairs)
by definition — the cap is the mechanism. Document and enforce caps on any
new rule instance that may reach high-fanout at production scale.

## Incremental / Big-3 / explain

- **Incremental (n=100):** p50=17.26 ms p95=32.23 ms
- **Big-3 full-graph (n=50):** p50=7.8 µs p95=18.0 µs ; mean intersection=0.0
  *(Full-graph Big-3 intersection empty: 1M cap at 70k×20k = 0.07% pair coverage; random talent sample misses the covered slice. This is cap-coverage semantics, not an engine defect. See Big-3 slice below.)*
- **Big-3 slice (500T×500C metro/industry, n=50):** p50=633.2 µs p95=848.9 µs ; mean intersection=500.0
  *(Answers marketplace 5-second question in a focused bucket. first_ia=500 first_sm=500 first_lf=500 first_intersection=500. Full-graph coverage awaits derived-edge persistence — see Roadmap.)*
- **explain (n=100):** p50=112.1 µs p95=570.7 µs

## Reopen (cold-start)

**WAL-only path:** On `open()` without a snapshot, the engine replays the WAL
(node inserts + rule declarations) and re-fires all declared rules from node data.
Cold-start time scales with rule count × rule computation complexity.
The dominant cost is IVF-Flat re-derivation (~8.37 min at this rule set).

**Snapshot V4 path (the V4 snapshot release):** `snapshot()` writes derived edges + IVF centroids
to the snapshot file (V4 format). On subsequent `open()`, these are loaded directly —
no rule re-fire. The 11.148 s open time (vs 8.86 min WAL-only) reflects loading
pre-materialized edges from disk, not rule recomputation.

- **WAL reopen:** 8.86 min (ok) — close + open, no snapshot; re-fires all rules (non-semantic ~21 s + IVF-Flat ~8.37 min = bottleneck)
- **Snapshot reopen:** 47.252 s total (snapshot write=36.105 s + open=11.148 s) (ok) — V4 snapshot: derived edges + IVF centroids loaded; no rule re-fire; **47.7× faster than WAL-only on the open() step**

## Oracle

- 1k-node industry_alignment exact-set compare: **ok** (expected=58100 got=58100)

## Comparison vs marketplace pain points

**CONTEXT — not apples-to-apples.** Marketplace numbers are their
reported production pain (different hardware, networked 14-shard
search, real OpenAI 1536-dim vectors). Ours are a local embedded
process on the machine above, synthetic hash-chain embeddings.

| Path | Marketplace (reported) | mushroomdb this run |
|---|---|---|
| Talent→Company matcher (Big-3) | 5+ second queries | p50=7.8 µs p95=18.0 µs mean_matches=0.0 — intersection empty (matcher rules not live at this scale) |
| Search fan-out | 14 sharded Meilisearch indices + in-memory merge | derived-edge `neighbors` on declared rules |
| Semantic / vector | Meili `_vectors` 1536-dim | exact: extrapolated (full=False); approx: 8.37 min recall=0.080 |
| Ingest 100k | (not published) | 1.46 min peak 2.09 GiB (ingest_batch 10k chunks) |

## Surface gaps and what changed

- **T1 (streaming backfill):** Matcher backfill at 100k NOW COMPLETES.
  Engine no longer builds a full `BTreeMap` of desired pairs before capping.
  Uncapped rules remain O(pairs) by definition — caps are the mechanism.
- **T2 (bindings):** `ingest_batch`, `stats`, `snapshot` added to Python bindings.
  `ingest_batch` in 10k chunks reduces WAL fsync overhead vs one-node-at-a-time.
- **T3 (exact early-exit):** Cauchy-Schwarz suffix-norm bound prunes exact
  VectorSimilar candidates without materializing all dot-products.
- **T4 (approximate=True):** IVF-Flat candidate selection for VectorSimilar rules.
  Opt-in, non-exact (per-query recall ≥ 0.90 quiesced per spec).
  Set-coverage recall at 100k is bounded at ~3% by cap/total_positives, NOT by IVF quality.
  Measure per-query ANN recall (uncapped probe graph) before enabling in prod.
- **Auto-FK:** Still declared as explicit KeyMatch rules (no ingest auto-FK).
- **Cypher COUNT:** Not available; edge counts use `neighbors` per src key.

## Findings

- Matcher backfill at 100k COMPLETED (T1 streaming). Rules: 9. Elapsed: 21.228 s.
- semantic_match exact full backfill not attempted: projected wall=124.60 min Δrss=0 B (approximate semantic runs instead)

