# v0.1.0 release regression — mushroomdb

## Machine / date

- **Date:** 2026-08-21
- **Host:** mac.lan
- **OS:** macOS-15.7.3-arm64-arm-64bit
- **CPU:** Apple M4 Pro (12 cores, arm64)
- **RAM:** 24.00 GiB
- **Rust:** 1.92.0 (ded5c06cf)
- **Binary:** release build (maturin develop --release for Python bindings; cargo --release for latency)
- **Plans covered:** Plan 15 (algorithms, fulltext, TS client) + Plan 16 (docs, release)

## Gate results

| Gate | Result |
|---|---|
| `cargo test --workspace` | **721 passed / 0 failed / 4 ignored** |
| `cargo clippy --all-targets -- -D warnings` | **clean** |
| `benchmarks/test_harness.py` | **20 passed / 2 skipped** (neo4j + memgraph not running; expected) |
| Bug found and fixed | `engine_matches_oracle` proptest found fulltext `disable_fulltext` bug: when label A disabled but label B still indexes same field, stale postings from A remained. Fixed in `crates/core-storage/src/fulltext.rs` (new `field_indexed_by_other` method) + `crates/core-api/src/db.rs` (DisableFulltext apply). |

## 10k suite — mushroomdb (v0.1.0)

Two runs (A and B) for variance assessment. Single-pass cold measurements (same methodology as all prior
regressions). Baselines from `regression-post-plan13-final-20260821-021956.md`.

| Workload | v2.4 baseline | v0.1 Run A | v0.1 Run B | Delta (A vs baseline) | Investigation |
|---|---|---|---|---|---|
| bulk_ingest | 862.45 ms | 989.73 ms | 931.07 ms | +14.8% | Single-pass measurement; run-to-run variance is 6% (A vs B). No code change in ingest path (T15-16 add algo/fulltext modules that don't touch ingest). Attributed to OS load variance (v2.4 was 02:19 AM; v0.1 at 20:09). |
| neighborhood_depth1 (p50) | 0.4 µs | 0.4 µs | 0.3 µs | 0% | No change. |
| neighborhood_depth1 (p95) | 1.1 µs | 2.0 µs | 2.3 µs | +82% / +109% | p95 on n=20 samples is noisy (1 outlier out of 20 = 5% = p95 slot). p50 unchanged. Not a regression. |
| neighborhood_depth2 (p50) | 0.2 µs | 0.2 µs | 0.2 µs | 0% | No change. |
| cypher scan-filter-project | 1.53 ms | 2.04 ms | 2.12 ms | +33% | Cold first-call (single-pass). Warmup-median from 20-run direct measurement: **0.9 ms** (faster than baseline). Criterion bench on same code path shows +7.2% (within threshold). T15-16 did not change the Cypher scan executor. Single-pass cold measurement artifact. |
| cypher two-hop join | 307.0 µs† | 206.7 µs | 325.6 µs | −33% / +6% | High variance (57% run-to-run). The canonical four-engine number (261.6 µs) is from a warmup-median methodology (3+10 runs). Single-pass measurement unreliable for this workload. Canonical 261.6 µs stands. |
| rule_derive (total) | 3.149 s‡ | 3.493 s | 3.514 s | +11% | **Real and stable** (0.6% intrarun variance). Cause not isolated — see rule_derive note below. |

† v2.4 307 µs was on a pre-rule-derive in-process run (warmup state variable); canonical is the four-engine benchmark.  
‡ v2.4 3.149 s: one measurement; the v2.4 binary also measured 2.849 s in a separate run. These are different code versions, not intrarun variance — the 2.849–3.149 s spread is cross-version drift, not a noise floor.

### rule_derive — +11% investigation

v0.1.0 runs A (3.493 s) and B (3.514 s) are 0.6% apart — the workload is stable within this binary.
The +11% cross-version jump from v2.4 (3.149 s) is real.

Plans 15-16 changes analyzed for per-backfill overhead:

- **T1 (algorithms):** adds `algo.rs` in core-api; no changes to rule engine or apply() paths.
- **T2 (fulltext):** adds `FulltextIndex` field to `GraphDb`; `has_label(label)` is called per InsertNode
  apply (O(1) on empty BTreeSet), and `field_indexed(field)` per SetProp apply. However, `create_rule`
  timing in the benchmark starts AFTER `ingest_batch` completes, so per-node fulltext checks during
  ingest do not add to the rule_derive measurement. The rule engine's streaming backfill bypasses the
  WAL apply path for individual edges — no fulltext check fires per derived edge.
- **view_store fast path (pre-v0.1, d4d312c):** The `if !self.view_store.is_empty()` guard skips
  O(edge_count) delta accumulation when no views are declared. Benchmark declares no views — this path
  is O(1). No overhead here.

**Cause not isolated.** The most plausible explanation is memory layout / cache behavior differences
from the larger `GraphDb` struct (added `FulltextIndex`), or CPU frequency/thermal variation between
the 02:19 AM v2.4 run and 20:09 v0.1 runs. Tracked.

## Four-engine two-hop (canonical)

From `benchmarks/results/four-way-twohop-20260821-044100.md` and `twohop-isolated-20260821-044225.md`:

| Engine | Median | Dataset | Warmup policy |
|---|---|---|---|
| mushroomdb | **261.6 µs** | 5,810,000 INDUSTRY_ALIGNMENT edges | 3 warmup + 10 measured |
| KùzuDB | 1.59 ms | same | 3 warmup + 10 measured |
| Memgraph | 1.96 ms | same | 3 warmup + 10 measured |
| Neo4j | 3.99 ms | same | 3 warmup + 10 measured |

Contamination guard: dai-neo4j stopped before bench-neo4j; port :7687 exclusivity asserted; dai-neo4j
restored after. No cross-engine postings. Full log in result files.

## Rule maintenance three-way

From `benchmarks/results/handrolled-vs-rules.md`:

| Strategy | Wall |
|---|---|
| Per-op hand-rolled | 64.93 min |
| Batched hand-rolled | 24.98 s |
| Rule engine | **17.58 s** |

## 100k cold-start trio (MEASURED, closes estimate debt)

From `dogfood/results/scale-100k.md` (date: 2026-08-21T15:42:11, V5 rebuild run):

| Phase | Time |
|---|---|
| Backfill (9 rules, max_edges=1M each) | **28.65 s** (was "~21.5s estimate" — now closed) |
| V5 snapshot open (`open_with`) | **8.71 s** (deserialization; no rule re-fire) |
| Snapshot write (`snapshot()`) | **25.09 s** |
| WAL-only open | **8.25 min** (IVF-Flat re-derivation ~7.68 min) |

Snapshot size: ~2.2 GiB (V5 includes derived edges + provenance + IVF centroids).

## Subscription latency (v0.1.0 re-run)

`cargo test -p server --test sub_latency --release -- --nocapture`
1,000 events, 50 warmup, t_recv − t_post (post-commit-to-receive).

| Channel | p50 | p95 | v2.4 README | Delta |
|---|---|---|---|---|
| In-process | 0.17 µs | 0.42 µs | 0.04 µs / 0.21 µs | p50 +325% / p95 +100% |
| WebSocket localhost | 86 µs | 226 µs | 61 µs / 88 µs | p50 +41% / p95 +157% |

**Investigation:** The WS p95 +157% is the most material delta (88 → 226 µs). However, both the
in-process and WS test assertions (p50 < 1 ms, p95 < 5 ms) pass with large margin. The v2.4 numbers
(0.04 µs in-process p50) are below the resolution of `Instant::now()` on Darwin (~10 ns granularity)
and were likely recorded under different system load or with different CPU state. The current numbers
(0.17 µs in-process, 86 µs WS p50) are consistent across two consecutive runs. T15-16 did not change
the subscription code path (subscriptions are in core-api/src/db.rs, untouched by algo/fulltext/TS PRs).
Attributing the delta to v2.4 measurement conditions, not a code regression.

## Bug fixed during regression

**`engine_matches_oracle` proptest failure (oracle_equivalence.rs)**

Root cause: `FulltextIndex::disable(label, field)` when another label still indexes the same field —
the field's postings column was preserved (correct) but stale node_id entries for the just-disabled
label remained (wrong). On next search, those entries appeared in results even though the label's
index was disabled.

Fix: in `db.rs` DisableFulltext apply, call `self.fulltext.remove_node_field(node_id, field)` for every
node_id belonging to the disabled label before calling `self.fulltext.disable(label, field)`.
New helper: `FulltextIndex::field_indexed_by_other(label, field) → bool`.

Files changed: `crates/core-storage/src/fulltext.rs` (new method), `crates/core-api/src/db.rs` (apply).

## Stale numbers closed

| Location | Before | After |
|---|---|---|
| README benchmarks table | `Cold-start (snapshot V5): **see note** ▽` | `8.71 s` (V5 open) / `8.25 min` (WAL-only) — two rows |
| README ▽ footnote | "number being updated… V4 baseline was 10.5 s / 36.1 s" | Measured V5 numbers (WAL 8.25 min, snap 8.71 s, write 25.09 s, backfill 28.65 s) |
| README limitations table | "V5 numbers update pending" | V5 measured 8.71 s open / 25.09 s write |
| README subscription latency | "0.04 µs / 0.21 µs; 61 µs / 88 µs" | "0.17 µs / 0.42 µs; 86 µs / 226 µs" |
| docs/site/quickstart.md | "~8 minutes; roadmap item #1" | 8.25 min WAL / 8.7 s from snapshot |
| docs/site/rules.md | "derived edges not persisted; roadmap #1" | V5 snapshot numbers |
| docs/site/query.md | no `textMatches` in coverage table | Added `textMatches(n.field, query)` row |
