# Scale run — representative matching workload (100k protocol)

## Machine / date

- **Date:** 2026-08-24T18:24:27
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

**Snapshot format:** V6 (zstd-compressed, v0.1.1+). Snapshot size: **1.1 GiB**
(V5 baseline from v0.1.0: ~2.2 GiB; −50%). Write uses zstd level-3 compression;
open decompresses on load. V5 snapshots are readable by v0.1.1 code (backward-compatible).

## Phase timings

| Phase | status | wall | peak RSS (lifetime) | RSS after | notes |
|---|---|---|---|---|---|
| ingest | ok | 1.37 min | 4.12 GiB | 3.41 GiB | ingest_batch 10k chunks (T2); FK rules declared inline |
| backfill | ok | 20.343 s | 4.47 GiB | 3.57 GiB | T1 streaming; max_edges=1M caps; all non-semantic rules |
| semantic | extrapolated | 67.23 ms | 4.47 GiB | 3.57 GiB | 5k probe recorded (T3 early-exit); full 100k ScanAll not attempted (blocking); approximate semantic runs instead |
| semantic_approx | ok | 7.81 min | 4.72 GiB | 2.62 GiB | edges=1000000 recall=0.080 precision=1.000 |
| incremental | ok | 2.128 s | 4.72 GiB | 1.29 GiB | p50=17.78 ms p95=47.33 ms n=100 |
| big3 | ok | 31.14 ms | 4.72 GiB | 1.22 GiB | p50=7.1 µs p95=15.5 µs n=50 mean_matches=0.0 |
| big3_slice | ok | 37.16 ms | 4.72 GiB | 1.56 GiB | 500T×500C metro/industry slice (all 3 rules fire uncapped) |
| explain | ok | 83.96 ms | 4.72 GiB | 1.25 GiB | p50=118.2 µs p95=530.5 µs n=100 |
| reopen | ok | 8.16 min | 8.09 GiB | 3.14 GiB | WAL reopen: WAL CreateRule records trigger rule re-application on full node set (derived edges not in WAL) |
| reopen_snap | ok | 31.443 s | 12.73 GiB | 10.45 GiB | snapshot reopen: snapshot() 22.563 s + open_with 8.880 s; V6 snapshot (1.1 GiB, zstd level-3) includes derived edges + IVF centroids; no rule re-fire |

## Semantic phases (phase 3)

- **Exact status:** `extrapolated`
- **Attempted full 100000:** False
- **Method:** 5k ScanAll probe with T3 early-exit; t_full = t_probe * (n_t_full/n_t_probe) * (n_c_full/n_c_probe)
- **5k probe (T3 early-exit):** scale=5000 pairs=3500000 wall=18.171 s edges=111696 Δrss=83.91 MiB
- **Extrapolation:** factor=400.0 pairs_full=1400000000 projected_wall=121.14 min projected_Δrss=32.78 GiB under_30min=False under_8GiB=False
- **O(n²) method (binding):** `t_full = t_probe * (n_t_full/n_t_probe) * (n_c_full/n_c_probe)`. ScanAll evaluates every Talent×Company pair (not the passing subset). Full attempt only if projected wall < 1800s AND projected Δrss < 8.00 GiB.

### Approximate semantic (T4)

- **Method:** IVF-Flat approximate (T4: approximate=True in RuleDef)
- **Edges materialized:** 1000000
- **Wall:** 7.81 min

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
  - `industry_alignment_tc`: 1.247 s edges=1000000 tripped=True Δrss=254.56 MiB
  - `industry_alignment_tj`: 1.047 s edges=2000000 tripped=True Δrss=652.28 MiB
  - `specialty_match_tc`: 2.394 s edges=1000000 tripped=True Δrss=117.41 MiB
  - `specialty_match_tj`: 2.536 s edges=2000000 tripped=True Δrss=0 B
  - `location_fit_tc`: 1.619 s edges=1000000 tripped=True Δrss=0 B
  - `location_fit_tj`: 1.737 s edges=2000000 tripped=True Δrss=0 B
  - `similar_size_tc`: 1.425 s edges=1000000 tripped=True Δrss=717.50 MiB
  - `matches_design_style_tc`: 4.444 s edges=1000000 tripped=True Δrss=0 B
  - `similar_size_strict_tc`: 1.550 s edges=1000000 tripped=True Δrss=0 B

**T1 change:** The engine now streams the desired set directly into the
store rather than building a `BTreeMap<(src,dst), score>` first.
Combined with explicit `max_edges` caps, cartesian predicates at 70k×20k
no longer OOM the process. Uncapped low-selectivity rules are still O(pairs)
by definition — the cap is the mechanism. Document and enforce caps on any
new rule instance that may reach high-fanout at production scale.

## Incremental / Big-3 / explain

- **Incremental (n=100):** p50=17.78 ms p95=47.33 ms
- **Big-3 full-graph (n=50):** p50=7.1 µs p95=15.5 µs ; mean intersection=0.0
  *(Full-graph Big-3 intersection empty: 1M cap at 70k×20k = 0.07% pair coverage; random talent sample misses the covered slice. This is cap-coverage semantics, not an engine defect. See Big-3 slice below.)*
- **Big-3 slice (500T×500C metro/industry, n=50):** p50=727.5 µs p95=942.2 µs ; mean intersection=500.0
  *(Answers the 5-second latency question in a focused bucket. first_ia=500 first_sm=500 first_lf=500 first_intersection=500.)*
- **explain (n=100):** p50=118.2 µs p95=530.5 µs

## Reopen (cold-start)

**WAL reopen:** WAL replay applies each `CreateRule` record, which triggers rule
re-application on the full node set (same as initial backfill). Derived edges are
not stored in the WAL — only rule declarations + node inserts. The dominant cost
at this rule set is IVF-Flat re-derivation (~7.68 min).

**V6 snapshot reopen:** V6 snapshot includes `topo` (all edges, including derived)
and `provenance` (rule engine state), compressed with zstd (level 3). `open_with`
decompresses and restores from snapshot without re-firing rules — derived edges +
IVF centroids loaded from snapshot payload. Snapshot size: 1.1 GiB.

- **WAL reopen:** 8.16 min (ok) — close + open; CreateRule WAL records trigger rule re-application on full node set; IVF-Flat re-derivation is bottleneck
- **V6 snapshot write:** 22.563 s — `snapshot()` serializes derived edges + IVF centroids, compresses with zstd level-3 (1.1 GiB on disk)
- **V6 snapshot open:** 8.880 s (ok) — `open_with` decompresses V6 stream and restores state; no rule re-fire

## Oracle

- 1k-node industry_alignment exact-set compare: **ok** (expected=58100 got=58100)

## Comparison vs reported pain points

**CONTEXT — not apples-to-apples.** Reference numbers come from a
reported production workload (different hardware, networked multi-shard
search, real high-dim vectors). Ours are a local embedded
process on the machine above, synthetic hash-chain embeddings.

| Path | Reported (production) | mushroomdb this run |
|---|---|---|
| Talent→Company matcher (Big-3) | 5+ second queries | p50=7.1 µs p95=15.5 µs mean_matches=0.0 — intersection empty (matcher rules not live at this scale) |
| Search fan-out | 14 sharded Meilisearch indices + in-memory merge | derived-edge `neighbors` on declared rules |
| Semantic / vector | Meili `_vectors` 1536-dim | exact: extrapolated (full=False); approx: 7.81 min recall=0.080 |
| Ingest 100k | (not published) | 1.37 min peak 4.12 GiB (ingest_batch 10k chunks) |

## Findings

- Matcher backfill at 100k COMPLETED (T1 streaming). Rules: 9. Elapsed: 20.343 s.
- semantic_match exact full backfill not attempted: projected wall=121.14 min Δrss=32.78 GiB (approximate semantic runs instead)

## G3 reopen bench (Phase 3, 2026-08-26)

Branch `feat/phase-3-storage-physics` @ `af9c682` (post-review fixes), release
build, same host. Harness: `crates/core-api/examples/g3_reopen_bench.rs` run
under `/usr/bin/time -l`; db dir copied so the original artifact is untouched.
Note: the existing `scale-100000-db/snapshot.bin` header is **V5** (`GDB1`,
version=5, uncompressed bincode), not V6 as the earlier reopen section says.

| Format | Snapshot on disk | Cold open (3 runs) | Peak RSS (3 runs) |
|---|---|---|---|
| V5 (same binary) | 2.20 GiB | 10.44 / 9.93 s | 6.27 / 8.00 GiB |
| V7 packed CSR+columns+zstd | 1.07 GiB | 11.11 / 10.98 / 10.66 s | 9.80 / 10.31 / 10.24 GiB |

V7 write (from opened V5 state): 34.3-35.2 s, convert-process peak RSS
7.4-7.5 GiB. Node/edge counts identical after reopen: nodes=100500
edges=10009748. (First two V7 runs at `a21b6f2`; third at `af9c682` after
the V7 meta gained the `wal_truncated` flag — within noise.)

**G3 bar (< 1 s): FAIL — ~11 s, ~11x over.** V7 halves disk size but is
~1 s slower to open and peaks 2-4 GiB higher than V5 on the same binary.

Stage profile of the V7 open (`crates/core-storage/examples/v7_profile.rs`):

- file 1.07 GiB → zstd decompress 1.68 s → 2.19 GiB uncompressed payload
- payload split: packed topo 77.8 MiB; packed props 1.57 GiB; bincode meta
  554 MiB (provenance BTreeSets + edge_props for 10M derived edges)
- crc32 0.30 s; total `snapshot::decode` 6.08 s
- remaining ~5 s of open: engine/provenance restore, view+fulltext
  `rebuild_all` after decode

**Why the bar is out of reach for this design:** open cost is dominated by
data volume, not the old HashMap layouts. The 1536-dim embeddings alone are
~1.2 GiB of raw f64 (100k x 1536 x 8 B) stored as `Value::List` per node, and
10M derived edges carry ~550 MiB of provenance/edge-prop meta. Any owned
decompress-and-deserialize open of this dataset pays multiple seconds before
the first query; < 1 s needs zero-copy/lazy loading (mmap + packed columns
read in place, deferred provenance) — explicitly deferred by the Phase 3 plan
(no memmap2/rkyv without sign-off).
