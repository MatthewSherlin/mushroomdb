# Scale run — representative matching workload (100k protocol)

## Machine / date

- **Date:** 2026-08-21T15:42:11
- **Host:** mac.lan
- **OS:** macOS-15.7.3-arm64-arm-64bit
- **CPU:** Apple M4 Pro (12 cores, arm64)
- **RAM:** 24.00 GiB
- **Python:** 3.12.12
- **Seed:** 20260819
- **Scale:** 100000 nodes (70000 Talent + 20000 Company + 10000 Job + 500 User)

Peak RSS is `resource.ru_maxrss` (process-lifetime, Darwin bytes).
Current RSS is `ps -o rss=` after the phase. Bindings are embedded Rust
via `mushroomdb.GraphDb` — not HTTP. This is a representative matching
workload (Talent / Company / Job nodes) with **synthetic hash-chain
embeddings**; numbers are not apples-to-apples with any specific
production deployment (different hardware, no network, real embeddings).

## Phase timings

| Phase | status | wall | peak RSS (lifetime) | RSS after | notes |
|---|---|---|---|---|---|
| ingest | ok | 1.33 min | 6.38 GiB | 6.18 GiB | ingest_batch 10k chunks (T2); FK rules declared inline |
| backfill | ok | 28.650 s | 7.50 GiB | 6.29 GiB | T1 streaming; max_edges=1M caps; all non-semantic rules |
| semantic | extrapolated | 92.83 ms | 7.50 GiB | 6.29 GiB | 5k probe recorded (T3 early-exit); full 100k ScanAll not attempted (blocking); approximate semantic runs instead |
| semantic_approx | ok | 7.70 min | 8.18 GiB | 5.06 GiB | edges=1000000 recall=0.080 precision=1.000 |
| incremental | ok | 1.819 s | 8.18 GiB | 4.29 GiB | p50=17.19 ms p95=31.70 ms n=100 |
| big3 | ok | 76.09 ms | 8.18 GiB | 4.29 GiB | p50=3.3 µs p95=9.9 µs n=50 mean_matches=0.0 |
| big3_slice | ok | 40.48 ms | 8.18 GiB | 4.55 GiB | 500T×500C metro/industry slice (all 3 rules fire uncapped) |
| explain | ok | 157.59 ms | 8.18 GiB | 4.34 GiB | p50=59.9 µs p95=222.1 µs n=100 |
| reopen | ok | 8.25 min | 8.23 GiB | 3.04 GiB | WAL reopen: CreateRule WAL records trigger rule re-application on full node set; bottleneck is IVF-Flat re-derivation (~7.68 min) |
| reopen_snap | ok | 33.804 s | 9.48 GiB | 5.31 GiB | snapshot reopen: snapshot() 25.094 s + open_with 8.710 s; V5 snapshot includes derived edges + IVF centroids; no rule re-fire |

## Semantic phases (phase 3)

- **Exact status:** `extrapolated`
- **Attempted full 100000:** False
- **Method:** 5k ScanAll probe with T3 early-exit; t_full = t_probe * (n_t_full/n_t_probe) * (n_c_full/n_c_probe)
- **5k probe (T3 early-exit):** scale=5000 pairs=3500000 wall=17.419 s edges=111696 Δrss=125.06 MiB
- **Extrapolation:** factor=400.0 pairs_full=1400000000 projected_wall=116.12 min projected_Δrss=48.85 GiB under_30min=False under_8GiB=False
- **O(n²) method (binding):** `t_full = t_probe * (n_t_full/n_t_probe) * (n_c_full/n_c_probe)`. ScanAll evaluates every Talent×Company pair (not the passing subset). Full attempt only if projected wall < 1800s AND projected Δrss < 8.00 GiB.

### Approximate semantic (T4)

- **Method:** IVF-Flat approximate (T4: approximate=True in RuleDef)
- **Edges materialized:** 1000000
- **Wall:** 7.70 min

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
  - `industry_alignment_tc`: 1.910 s edges=1000000 tripped=True Δrss=0 B
  - `industry_alignment_tj`: 1.942 s edges=2000000 tripped=True Δrss=0 B
  - `specialty_match_tc`: 3.150 s edges=1000000 tripped=True Δrss=0 B
  - `specialty_match_tj`: 3.370 s edges=2000000 tripped=True Δrss=248.69 MiB
  - `location_fit_tc`: 2.432 s edges=1000000 tripped=True Δrss=690.45 MiB
  - `location_fit_tj`: 2.630 s edges=2000000 tripped=True Δrss=739.06 MiB
  - `similar_size_tc`: 2.406 s edges=1000000 tripped=True Δrss=819.06 MiB
  - `matches_design_style_tc`: 5.587 s edges=1000000 tripped=True Δrss=0 B
  - `similar_size_strict_tc`: 2.628 s edges=1000000 tripped=True Δrss=161.83 MiB

**T1 change:** The engine now streams the desired set directly into the
store rather than building a `BTreeMap<(src,dst), score>` first.
Combined with explicit `max_edges` caps, cartesian predicates at 70k×20k
no longer OOM the process. Uncapped low-selectivity rules are still O(pairs)
by definition — the cap is the mechanism. Document and enforce caps on any
new rule instance that may reach high-fanout at production scale.

## Incremental / Big-3 / explain

- **Incremental (n=100):** p50=17.19 ms p95=31.70 ms
- **Big-3 full-graph (n=50):** p50=3.3 µs p95=9.9 µs ; mean intersection=0.0
  *(Full-graph Big-3 intersection empty: 1M cap at 70k×20k = 0.07% pair coverage; random talent sample misses the covered slice. This is cap-coverage semantics, not an engine defect. See Big-3 slice below.)*
- **Big-3 slice (500T×500C metro/industry, n=50):** p50=772.5 µs p95=1.19 ms ; mean intersection=500.0
  *(Answers the 5-second latency question in a focused bucket. first_ia=500 first_sm=500 first_lf=500 first_intersection=500. Full-graph coverage awaits derived-edge persistence — see Roadmap.)*
- **explain (n=100):** p50=59.9 µs p95=222.1 µs

## Reopen (cold-start)

**V5 snapshot behavior (corrected):** V5 `SnapshotState` includes `topo` (all edges, including
derived) and `provenance` (rule engine state). `open_with` restores from snapshot without
re-firing rules — derived edges are loaded directly from the snapshot payload. WAL replay
(`open`) does re-fire rules because it applies each WAL record in order, including `CreateRule`
records, which trigger `apply_streaming_create` / `apply_streaming_create_top_k` on the
current node set.

- **WAL reopen:** 8.25 min (ok) — close + open; WAL CreateRule records trigger rule re-application on full node set (9 streaming backfill ~28 s + IVF-Flat fit + apply ~7.68 min = bottleneck)
- **Snapshot reopen:** 33.804 s (snapshot=25.094 s + open=8.710 s) (ok) — snapshot() + close + open_with; derived edges + IVF centroids loaded from V5 snapshot; no rule re-fire; 8.710 s is pure deserialization

**V5 snapshot size (2.2 GiB vs V4's ~283 MB):** V5 `SnapshotState` serializes the full
derived edge set that V4 did not persist. With 9 backfill rules × ~1M edges each plus 1M
semantic_approx edges = ~10.5M derived edges, each edge is stored in three payload sections:
`topo` (src+dst+etype per edge), `edge_props` (per-edge weight scalar), and `provenance`
(rule-name → BTreeSet of (src,dst,etype) triples for each rule-owned edge). Together these
dominate the snapshot size. Node embeddings (90k × 384 f32 = ~138 MB) and rule definitions
are negligible by comparison.

**Note:** This file was generated by the V5 rebuild run (PID 50395, 2026-08-21T15:42:11). The
previous V4 snapshot was unreadable by V5 binary (`view_defs` field added to `SnapshotState`
in V5). The text above supersedes any prior description of snapshot behavior in this document.

## Oracle

- 1k-node industry_alignment exact-set compare: **ok** (expected=58100 got=58100)

## Comparison vs reported pain points

**CONTEXT — not apples-to-apples.** Reference numbers come from a
reported production workload (different hardware, networked multi-shard
search, real high-dim vectors). Ours are a local embedded process on
the machine above, synthetic hash-chain embeddings.

| Path | Reported (production) | mushroomdb this run |
|---|---|---|
| Talent→Company matcher (Big-3) | 5+ second queries | p50=3.3 µs p95=9.9 µs mean_matches=0.0 — intersection empty (matcher rules not live at this scale) |
| Search fan-out | 14 sharded Meilisearch indices + in-memory merge | derived-edge `neighbors` on declared rules |
| Semantic / vector | Meili `_vectors` 1536-dim | exact: extrapolated (full=False); approx: 7.70 min recall=0.080 |
| Ingest 100k | (not published) | 1.33 min peak 6.38 GiB (ingest_batch 10k chunks) |

## Surface gaps and what changed (Plan 11)

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

- Matcher backfill at 100k COMPLETED (T1 streaming). Rules: 9. Elapsed: 28.650 s.
- semantic_match exact full backfill not attempted: projected wall=116.12 min Δrss=48.85 GiB (approximate semantic runs instead)

