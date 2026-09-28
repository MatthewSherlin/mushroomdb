# HNSW scale benchmark — v0.6.6

The printed output of `crates/core-rules/tests/hnsw_scale.rs`, kept here so that every
number the release notes and `docs/site/rules.md` quote has a committed home other than the
prose that quotes it.

Reproduce with:

```sh
MUSHROOMDB_BENCH_HNSW=1 cargo test --release -p mushroomdb-rules --features test-hooks \
  --test hnsw_scale -- --ignored --nocapture --test-threads=1
```

Machine: aarch64 (Apple silicon), macOS, release profile, `--test-threads=1` so the three
`#[ignore]`d tests in the file do not contend for memory bandwidth. Parameters are the shipped
defaults, `HnswParams { m: 16, m0: 64, ef_construction: 200, ef_search: 400, prune: Both }`.
Vectors are 1,536-dimensional unit vectors on a fixed seed.

## Within 0.6.6: before and after the distance kernel

Both columns are 0.6.6 code. "before the kernel" is the branch at its diverse-neighbour prune
with `f64` vectors stored per node; "after" is the shipped build, which holds one contiguous
`f32` slab and sums the dot product in eight independent accumulators. **Neither column is
v0.6.5** — the prune and the parameters differ from it too, so this table measures the kernel
and nothing else.

| | 2,000 | 10,000 | 50,000 |
|---|---|---|---|
| build, before the kernel | 49.60 s | 730.84 s | not run |
| **build, shipped** | **8.53 s** | **132.12 s** | **1,018.05 s** |
| per-insert, before the kernel | 24.801 ms | 73.084 ms | — |
| **per-insert, shipped** | **4.267 ms** | **13.212 ms** | **20.361 ms** |
| update (one re-embed), before the kernel | 14.056 ms | 30.710 ms | — |
| **update, shipped** | **2.412 ms** | **6.641 ms** | **14.280 ms** |
| adjacency B/node | 513.3 | 518.6 | 519.0 |
| total B/node, before the kernel | 12,801.3 | 12,806.6 | — |
| **total B/node, shipped** | **6,657.3** | **6,662.6** | **6,663.0** |

The 50,000 row has no "before" figure: that build was never run at the pre-kernel cost, which
the 10,000 row projects at roughly three hours. It is not a measurement and is not quoted as
one anywhere.

At 5,000 × 1,536 (`hnsw_memory_per_node_5k_1536`, a separate test): 43.34 s shipped against
254 s before the kernel at the default prune, and 9.56 s against 45.8 s at
`MUSHROOMDB_HNSW_PARAMS=16,64,200,400,own`. Bytes per node 6,663.2 against 12,807.2, −48.0 %.

## Across releases

One figure, and it is the only cross-release comparison this release makes, because it is the
only one measured by the same committed test on both sides:

| | v0.6.5 | v0.6.6 |
|---|---|---|
| `approximate_recall_5k_timing` HNSW backfill | **227 s** | **49.4 s** |
| recall at that gate | 1.0000 | 1.0000 |

The 227 s is recorded in `.github/workflows/ci.yml` at tag `v0.6.5`
(`git show v0.6.5:.github/workflows/ci.yml`, the `approximate_recall_5k_timing` comment). The
49.4 s is this release's run of the same gate. Wall clocks move a few percent between runs and
between machines; the recall figures are deterministic.

## The three assertions that stay red

> **CORRECTION, 2026-09-24 (v0.6.11 Task 1).** This section said **two** for three releases.
> It is **three**. The count was taken by reading the printed table rather than by watching the
> assertions execute — and at the time this was written they could not all execute. All six
> assertions lived in one `#[test] fn hnsw_insert_cost_is_sublinear_per_vector`, and an assertion
> that fails ends its test, so the 2,000→10,000 build-growth assertion panicked first and the
> 50,000-second wall clock, the update-cost ratio and the update absolute **never ran at all**.
> The 50,000 s figure below was therefore also reported from the table rather than from a
> failure; the one genuinely-executing red assertion was the build ratio.
>
> The missed one is the **update-cost ratio**, and it is the tightest of the three: a 1.97×
> overshoot for 25× the vectors, against the build ratio's 1.94× for 5×. It also measures the
> operation a production store performs most — a changed embedding is a remove plus an insert.
>
> `hnsw_scale.rs` now splits those six assertions into six `#[test]` functions over one shared
> fixture, so no assertion can hide behind another's panic again. The CHANGELOG's v0.6.10
> "Known limits" entry is corrected to match; the v0.6.6, v0.6.8 and v0.6.9 entries are left as
> the historical record of what was believed at those releases.
>
> **The split was then run, and all three reds reported separately for the first time.**
> Apple M4 Pro, macOS 15.7.3, arm64, release profile, `--test-threads=1`, 1,219.71 s total:
>
> ```
> n=  2000  build      9.01s  per-insert   4.503ms  update   2.530ms  total 6657.3 B/node
> n= 10000  build    145.77s  per-insert  14.577ms  update   6.897ms  total 6662.6 B/node
> n= 50000  build   1017.14s  per-insert  20.343ms  update  14.793ms  total 6663.0 B/node
>
> test result: FAILED. 4 passed; 3 failed
>   build_growth_2k_to_10k_is_under_8x   16.19x  (ceiling 8x)     FAILED
>   build_50k_is_under_300s            1017.14s  (ceiling 300 s)  FAILED  ← first execution
>   update_cost_2k_to_50k_is_under_3x     5.85x  (ceiling 3x)     FAILED  ← first execution
>   build_growth_10k_to_50k_is_under_8x   6.98x                   ok
>   update_50k_is_under_25ms           14.793ms                   ok
>   memory_per_node_is_within_the_ceiling                         ok
> ```
>
> These land within machine noise of the 0.6.6 figures below (16.19× against 15.48×, 5.85×
> against 5.92×, 1,017.14 s against 1,018.05 s), which is the point: **nothing regressed and
> nothing was fixed — the third failure was always there and nothing could report it.** The
> table below stays as the 0.6.6 record; these are confirmation, not a replacement.

`hnsw_scale.rs` asserts a sub-quadratic build, a 50,000-vector build under five minutes, and a
re-embed cost that does not grow with the index. Three fail, deliberately and unedited:

| Assertion | Ceiling | Measured |
|---|---|---|
| build growth per 5× the vectors | 8× | **15.48×** (2,000 → 10,000) |
| 50,000-vector build | 300 s | **1,018.05 s** |
| **update cost, 2,000 → 50,000** | **3×** | **5.92×** (14.280 ms ÷ 2.412 ms) |

Two assertions from the same test do pass and are worth naming so the red ones are not read as
the whole picture: the second build step (10,000 → 50,000) grows **7.71×**, inside the same 8×
ceiling, and one 50,000-vector update takes **14.280 ms** against a 25 ms ceiling. The build
ratio fails on the *first* step, not the second.

A constant-factor kernel cancels out of a ratio, so the kernel could not move the first one and
was never expected to; the slab helps the smaller point more, because 12.3 MB of `f32` vectors
at 2,000 is cache-resident where 61.4 MB at 10,000 is not, which is why the ratio rose from
14.73× to 15.48×. Closing either means cutting the distance-evaluation count itself —
`ef_construction`, or a budget on the prune's candidate walk — and both change the graph, which
makes them a recall decision rather than a kernel one.

The evaluation count is measured and gated separately by `dist_evals_per_insert_is_bounded`
(in CI): 12,739.5 per insert at 1,000 nodes and 23,901.7 at 5,000, a ratio of 1.876, against
ceilings of 36,000 and 2.8×. That gate is a count rather than a wall clock, so it holds on any
machine, and it is the one that would notice the insert path doing more work per vector as the
index grows.
