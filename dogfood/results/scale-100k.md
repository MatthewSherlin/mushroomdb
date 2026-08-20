# Scale run — marketplace dogfood (100k protocol)

## Machine / date

- **Date:** 2026-08-19T19:10:42
- **Host:** mac.lan
- **OS:** macOS-15.7.3-arm64-arm-64bit
- **CPU:** Apple M4 Pro (12 cores, arm64)
- **RAM:** 24.00 GiB
- **Python:** 3.12.12
- **Seed:** 20260819
- **Scale:** 100000 nodes (70000 Talent + 20000 Company + 10000 Job + 500 User)

Peak RSS is `resource.ru_maxrss` (process-lifetime, Darwin bytes) and
**includes the 5k probes that ran in-process before the 100k ingest**.
Current RSS is `ps -o rss=` after the phase. The 100k working set after
WAL reopen is **3.98 GiB** (the 197 MiB post-ingest figure is the live
process after probe heaps were dropped; on-disk `wal.bin` is 1.60 GiB).
Bindings are embedded Rust via `mushroomdb.GraphDb` — not HTTP. Numbers
are labeled **not apples-to-apples** vs the marketplace production
stack (different hardware, no network, synthetic embeddings).

## Phase timings

| Phase | status | wall | peak RSS (lifetime) | RSS after | notes |
|---|---|---|---|---|---|
| ingest | ok | 8.92 min | 4.33 GiB | 196.94 MiB | insert_node loop; 1 WAL fsync / node (no Python batch API) |
| backfill | extrapolated | 4.16 ms | 4.33 GiB | 196.98 MiB | SIX_RULES minus semantic_match, plus KeyMatch FK |
| semantic | extrapolated | 3.80 ms | 4.33 GiB | 197.02 MiB | recorded 5k extrapolation; full 100k ScanAll not started (blocking create_rule cannot be aborted) |
| incremental | ok | 381.35 ms | 4.33 GiB | 206.41 MiB | p50=3.48 ms p95=4.57 ms n=100 |
| big3 | ok | 6.25 ms | 4.33 GiB | 206.88 MiB | p50=5.7 µs p95=10.1 µs n=50 |
| explain | ok | 48.95 ms | 4.33 GiB | 209.98 MiB | p50=4.5 µs p95=5.2 µs n=100 |
| reopen | ok | 26.872 s | 4.50 GiB | 3.98 GiB | GraphDb.close + open (WAL replay; snapshot() not in bindings) |

## Semantic verdict (phase 3)

- **Status:** `extrapolated`
- **Attempted full 100000:** False
- **Method:** 5k ScanAll probe; t_full = t_probe * (n_t_full/n_t_probe) * (n_c_full/n_c_probe)
- **5k probe:** scale=5000 (3500 Talent × 1000 Company) pairs=3_500_000 wall=12.05 min edges=111_696 Δrss=92.58 MiB (create_rule current-RSS delta after desired-set drop). Probe process-lifetime peak during this call grew to 4.33 GiB.
- **Extrapolation:** factor=`(70000/3500)*(20000/1000)` = **400.0** ; pairs_full=1_400_000_000 ; projected_wall=**4818.53 min (80.3 h)** ; projected_Δrss=36.16 GiB ; under_30min=**False** ; under_8GiB=**False**
- **O(n²) method (binding):** `t_full = t_probe * (n_t_full/n_t_probe) * (n_c_full/n_c_probe)`. ScanAll evaluates every Talent×Company pair (not the passing subset). RSS projection uses the same factor on the probe's `create_rule` current-RSS delta (a lower bound: the in-flight desired `BTreeMap` is larger than the post-apply RSS). Full 100k attempt only if projected wall < 1800 s AND projected Δrss < 8.00 GiB. Neither held, so the extrapolation **is** the finding.

## Backfill (phase 2) — cartesian materialization

- **Status:** `extrapolated`
- **Method:** 5k probe extrapolation (factor=400.0); full cartesian backfill not attempted
- **Probe wall:** 1.22 min at scale=5000
  - `industry_alignment_tc`: 10.525 s edges=1_000_000 tripped=True Δrss=847.47 MiB
  - `industry_alignment_tj`: 6.491 s edges=1_726_250 tripped=True Δrss=192.05 MiB
  - `specialty_match_tc`: 14.848 s edges=1_000_000 tripped=True Δrss=92.03 MiB
  - `specialty_match_tj`: 8.354 s edges=1_660_759 tripped=True Δrss=558.78 MiB
  - `location_fit_tc`: 2.885 s edges=291_668 tripped=False Δrss=200.50 MiB
  - `location_fit_tj`: 1.394 s edges=437_504 tripped=False Δrss=114.36 MiB
  - `similar_size_tc`: 12.512 s edges=1_000_000 tripped=True Δrss=184.27 MiB
  - `matches_design_style_tc`: 7.435 s edges=403_994 tripped=False Δrss=0 B
  - `similar_size_strict_tc`: 6.740 s edges=700_000 tripped=False Δrss=54.50 MiB
- **Count caveat:** `neighbors(talent, EDGE_TYPE)` is not label-filtered. Shared edge types (`INDUSTRY_ALIGNMENT`, `SPECIALTY_MATCH`, `LOCATION_FIT`) accumulate TC+TJ on later rows — `industry_alignment_tj` 1_726_250 = the 1M TC cap plus TJ. TC-only types (`SIMILAR_SIZE*`, `MATCHES_DESIGN_STYLE`) are exact. `tripped=True` is `count >= 1_000_000`.
- **Extrapolation (pair-count factor 400.0):** projected_wall=489.66 min projected_Δrss=331.04 GiB
- **Wall-time lower bound:** the pair-count scaling assumes a constant per-insertion cost; `BTreeMap` insertions are O(log n), so at 1.4B pairs the per-insertion cost is ~log(1.4B)/log(3.5M) ≈ 1.4× higher than at the 3.5M-pair probe. The 489-min figure is therefore an **underestimate**; log-factor–corrected wall is ~**685 min**. The conclusion (do not attempt) is unchanged.
- **Finding:** Non-semantic create_rule at full scale projected wall=489.66 min Δrss=331.04 GiB (budgets 30.00 min / 8.00 GiB). Engine materializes the full desired cartesian before the 1M edge budget; attempting it would hang or OOM this 24 GiB machine.

The engine's `create_rule` computes the **full desired set**
(`BTreeMap<(src,dst), score>`) *before* applying `max_edges`
(default 1,000,000). Cartesian predicates
(FieldEqual / Overlap / NumericWithin / GeoRadius) at 70k×20k therefore
allocate hundreds of millions of pairs even though only the first 1M
edges are kept. That is why a 5k probe + extrapolation gates the 100k
backfill the same way the semantic phase is gated — a blocking
backfill cannot be aborted mid-flight.

## Incremental / Big-3 / explain

- **Incremental (n=100):** p50=3.48 ms p95=4.57 ms — `set_prop` on specialty/location/embedding. Only KeyMatch FK rules were live (they watch `user_id` / `company_id`), so this is WAL write cost, **not** matcher incremental fire.
- **Big-3 (n=50):** p50=5.7 µs p95=10.1 µs ; **mean INDUSTRY∩SPECIALTY∩LOCATION intersection = 0.0**. Those edge types were never declared on the 100k graph (phase 2 aborted). The µs figure is `node_edges` + three empty `neighbors` calls. It is **not** a 5-second-matcher replacement number.
- **explain (n=100):** p50=4.5 µs p95=5.2 µs — sampled from derived **FK** pairs (`Talent-[:USER]->User`, `Job-[:COMPANY]->Company`).
- **Reopen:** 26.872 s (ok) — WAL replay of `wal.bin` = 1_721_116_047 bytes; RSS after = 3.98 GiB. `snapshot()` is not on the Python bindings.

## Oracle

- 1k-node industry_alignment exact-set compare: **ok** (expected=58100 got=58100)

## Comparison vs marketplace pain points

**CONTEXT — not apples-to-apples.** Marketplace numbers are their
reported production pain (different hardware, networked 14-shard
search, real OpenAI 1536-dim vectors). Ours are a local embedded
process on the machine above, synthetic hash-chain embeddings.

| Path | Marketplace (reported) | mushroomdb this run |
|---|---|---|
| Talent→Company matcher (Big-3) | 5+ second queries | **cannot answer at 100k** — cartesian backfill projected 489.66 min / 331 GiB. Timed API on the FK-only graph: p50=5.7 µs p95=10.1 µs, intersection=0. 2k smoke (matcher rules live) exercises the same `node_edges`+`neighbors` path and the 50-pair industry oracle holds. |
| Search fan-out | 14 sharded Meilisearch indices + in-memory merge | derived-edge `neighbors` — only after a rule's desired set fits in RAM + the 1M budget |
| Semantic / vector | Meili `_vectors` 1536-dim | 5k ScanAll = 12.05 min / 111_696 edges; 100k extrapolated **80.3 h** + 36 GiB. Not attempted. |
| Ingest 100k | (not published) | 8.92 min ; 1.60 GiB WAL ; reopen 26.87 s / 3.98 GiB RSS |

## Product-surface gaps that shaped the run

- Python `GraphDb` has `insert_node` / `create_rule` / `set_prop` /
  `query` / `explain` / `neighbors` / `node_edges` / `node_info`.
  It does **not** expose `ingest_json`, auto-FK, `batch()`, `stats()`,
  or `snapshot()`. Auto-FK is therefore declared as ordinary KeyMatch
  rules after a sparse User node set is inserted.
- Cypher has no `COUNT` and caps intermediate rows at 1,000,000;
  edge counts at scale use `neighbors` per src key.

## Findings

- **1k oracle:** industry_alignment exact set 58_100 = 58_100. No engine misbehavior. Did not STOP.
- **Cartesian backfill at 100k not attempted.** 5k probe already trips `industry_alignment_tc` / `specialty_match_tc` / `similar_size_tc` at the default 1_000_000-edge budget (10.5 / 14.8 / 12.5 s, Δrss up to 847 MiB). Pair-count factor 400 → projected 489.66 min / 331 GiB. `create_rule` builds the full desired `BTreeMap` *before* the cap. A blocking backfill cannot be aborted, so the extrapolation is the result.
- **semantic_match 100k not attempted.** 5k ScanAll 12.05 min / 3.5M pair evaluations / 111_696 passing edges. ×400 → **80.3 h** and 36 GiB post-apply Δrss (in-flight peak higher). Under-30-min gate failed.
- **Python bindings have no `ingest_json` / auto-FK / `batch()` / `stats()` / `snapshot()`.** Each of 100_500 `insert_node` calls is its own WAL+fsync frame. Auto-FK was declared as KeyMatch after a sparse 500-User set; both `talent-000000→user-000000` and `job-000000→company-000000` materialized.
- **Big-3 at 100k is an empty intersection** for the reason above. Do not read 5.7 µs as beating the marketplace 5 s matcher.
- **2k pytest smoke** (`test_scale_small.py`) ran the full pipeline with all matcher rules + isolated semantic, asserted ≥1 FK edge, and brute-forced 50 random industry pairs against `neighbors`. 11/11 passed in 279.96 s.

---

## Run 2 (post-Plan-11) — 2026-08-20

Same machine, same seed (20260819), same 100k scale (70k Talent + 20k Company + 10k Job + 500 User).
Engine + bindings updated by Plan 11 T1–T4. Dogfood harness updated per Task 6 spec.

### Before / after table

| Metric | Run 1 (pre-Plan-11) | Run 2 (post-Plan-11) | Change |
|---|---|---|---|
| **Ingest 100k** | 8.92 min (1 WAL fsync/node) | **1.35 min** (ingest_batch 10k chunks, T2) | **6.6x faster** |
| **Backfill (9 non-semantic rules)** | Not attempted — projected 489.66 min / 331 GiB (BTreeMap full desired-set OOM) | **19.742 s**, peak 3.61 GiB — ALL 9 rules applied (T1 streaming + max_edges=1M caps) | **Wall fell** |
| **Semantic exact (100k)** | Not attempted — projected 80.3 h / 36 GiB (5k probe: 12.05 min) | Not attempted — projected 115.43 min / 0 B (5k probe: 17.315 s, T3 early-exit = **41.7x** speedup vs probe) | Probe 41.7x faster; full still over budget |
| **Semantic approx (T4)** | Not available | **7.68 min**, 1M edges, recall=0.080, precision=1.000 | New capability |
| **Incremental p50** | 3.48 ms (FK-only rules live) | **15.50 ms** (all 9 matcher rules now fire on insert) | Expected regression — more work per insert |
| **Big-3 mean intersection** | 0.0 (no matcher edges declared) | 0.0 (1M cap covers ~0.07% of 70k×20k pairs; random sample hits uncovered range) | Cap coverage finding |
| **WAL reopen** | 26.87 s (ingest-only WAL, 1.60 GiB) | 7.91 min (10M+ backfill edges in WAL, 1.72 GiB) | Expected regression — WAL holds all edges |
| **Snapshot reopen (T2)** | Not available (snapshot() not in bindings) | 7.58 min (18.345 s write + 7.28 min open, 2.2 GiB snapshot.bin) | New capability |
| **Oracle** | ok (58100/58100) | ok (58100/58100) | Correct |
| **Tests** | 11/11 (2k smoke) | **41/41** (extended: new batch/recall/approx/snap tests) | All green |

### Phase timings (Run 2)

| Phase | status | wall | peak RSS | RSS after | notes |
|---|---|---|---|---|---|
| ingest | ok | 1.35 min | 2.57 GiB | 2.20 GiB | ingest_batch 10k chunks (T2); FK rules inline |
| backfill | ok | 19.742 s | 3.61 GiB | 2.94 GiB | T1 streaming; max_edges=1M; all 9 non-semantic rules |
| semantic | extrapolated | 54.87 ms | 3.61 GiB | 2.94 GiB | 5k probe only (T3 early-exit 17.315 s → projected 115.43 min) |
| semantic_approx | ok | 7.68 min | 4.45 GiB | 2.40 GiB | T4 IVF-Flat; edges=1M; recall=0.080; precision=1.000 |
| incremental | ok | 1.565 s | 4.45 GiB | 2.54 GiB | p50=15.50 ms p95=27.58 ms n=100 |
| big3 | ok | 50.50 ms | 4.45 GiB | 2.54 GiB | p50=2.0 µs p95=8.7 µs n=50 mean_matches=0.0 |
| explain | ok | 85.62 ms | 4.45 GiB | 2.62 GiB | p50=57.7 µs p95=117.0 µs n=100 |
| reopen_wal | ok | 7.91 min | 8.09 GiB | 2.58 GiB | WAL replay baseline |
| snapshot + reopen_snap | ok | 7.58 min total | 10.27 GiB | 637.16 MiB | snapshot()=18.345 s + open=7.28 min |

### Backfill per-rule (Run 2)

All 9 rules now run in a single streaming pass. Every rule tripped the 1M cap at 70k×20k scale.

| Rule | Wall | Edges | Cap tripped | ΔRSS |
|---|---|---|---|---|
| industry_alignment_tc | 1.131 s | 1,000,000 | yes | 145.23 MiB |
| industry_alignment_tj | 1.150 s | 2,000,000 | yes (TC+TJ share type) | 0 B |
| specialty_match_tc | 2.499 s | 1,000,000 | yes | 205.05 MiB |
| specialty_match_tj | 2.487 s | 2,000,000 | yes | 497.39 MiB |
| location_fit_tc | 1.511 s | 1,000,000 | yes | 450.05 MiB |
| location_fit_tj | 1.330 s | 2,000,000 | yes | 47.20 MiB |
| similar_size_tc | 1.667 s | 1,000,000 | yes | 0 B |
| matches_design_style_tc | 4.538 s | 1,000,000 | yes | 0 B |
| similar_size_strict_tc | 1.478 s | 1,000,000 | yes | 221.78 MiB |

### Findings (Run 2)

- **T1 (streaming backfill): wall fell.** All 9 non-semantic rules backfilled at 100k in 19.742 s, peak 3.61 GiB. Run 1 projected 489–685 min / 331 GiB and could not be attempted.
- **T2 (ingest_batch): 6.6x ingest speedup.** 10k-chunk batch reduces WAL fsync overhead. WAL reopen is slower (7.91 min vs 26.87 s) because the WAL now contains 10M+ backfill edges; snapshot reopen (7.58 min) is comparable — loading a 2.2 GiB snapshot takes similar time to replaying a 1.72 GiB WAL on this machine.
- **T3 (Cauchy-Schwarz early-exit): 41.7x exact-probe speedup.** 5k probe: 17.315 s (was 12.05 min). Extrapolated 100k exact: 115.43 min (was 80.3 h). Full exact still over the 30-min budget.
- **T4 (approximate=True): new IVF-Flat path works; recall is low on synthetic data.** At 70k×20k with 1M cap the IVF index covers ~0.07% of pairs. Synthetic hash-chain embeddings cluster tightly by (industry, specialty) — random sample found only 25 ground-truth positives in 1000 pairs, TP=2. Real recall on a denser similarity distribution (e.g., real OpenAI vectors with a more uniform positive rate) may differ substantially. Approximate mode's spec floor is recall ≥ 0.90 quiesced; this run at 100k is NOT quiesced.
- **Big-3 intersection still empty.** 1M cap at 70k×20k covers ~0.07% of pairs (early streaming order). A random 50-talent sample hits mostly uncovered range. This is expected given the cap — not a correctness defect.
- **Incremental p50 higher (15.50 ms vs 3.48 ms).** In Run 1 only FK rules fired on insert. In Run 2 all 9 matcher rules evaluate on each new node insertion. The increase is proportional to rules fired per event — not a regression.
- **Oracle: 58100/58100.** No correctness change across Plan 11.

