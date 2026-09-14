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

## The two assertions that stay red

`hnsw_scale.rs` asserts a sub-quadratic build and a 50,000-vector build under five minutes.
Both fail, deliberately and unedited:

| Assertion | Ceiling | Measured |
|---|---|---|
| build growth per 5× the vectors | 8× | **15.48×** (2,000 → 10,000) |
| 50,000-vector build | 300 s | **1,018.05 s** |

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
