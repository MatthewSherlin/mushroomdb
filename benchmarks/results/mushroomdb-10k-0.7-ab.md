# A/B — mushroomdb v0.6.12 against the 0.7 branch, 10,000 nodes

Written 2026-10-01 from an investigation run that day. It was started because the first 0.7 run
(`mushroomdb-10k-0.7.md`) showed a rule backfill of 8.781 s where runs from 2026-08-21 and
2026-08-24 had shown 2.85–3.51 s.

**Conclusion: no 0.7 regression.** The released v0.6.12 and the 0.7 branch are indistinguishable on
every workload below, with one exception: the first, cold execution of the two-hop query is about
0.15 ms slower on the 0.7 branch (1.23× in the pooled table), and its cause was not established —
see "What was not established" at the end. The warm two-hop is flat. The backfill figure is real, is already in v0.6.12, and has been there since
v0.2.0: the benchmark's rules have derived a different, complete edge set since then.

## Machine / date

- **Date:** 2026-10-01
- **CPU:** Apple M4 Pro (12 cores, arm64)
- **OS:** macOS 15.7.3
- **Python:** 3.11.14
- **Scale:** 10,000 nodes (seed=20260819)
- **A:** `v0.6.12` (`fab9bdc`). **B:** the 0.7 branch at `4d68e9a`. The branch moved to `c3f3c8f`
  during the work; nothing under `benchmarks`, `crates` or `bindings` differs between the two.

## How it was run

- Each side was built in its own detached worktree with its own cargo target:
  `maturin build --release --offline` with `CARGO_NET_OFFLINE=true PIP_NO_INDEX=1`, and the wheel
  installed with `pip install --no-index` into its own virtualenv. Release builds, made offline.
- One harness for both sides: the 0.7 branch's `benchmarks/run.py` and `benchmarks/adapters/ours.py`.
  The v0.6.12 binding has every call that harness makes. The backfill timer wraps
  `db.create_rule(rule)` per rule and the loop in total; backfill is synchronous inside
  `create_rule` for these rules.
- Most runs used a driver, committed as `benchmarks/ab_driver.py`, that calls the same adapter
  functions in the same order as `run_ours()`, then records, outside every timer, `db.stats()`,
  the WAL size, and a two-hop median of 10 after 3 warmups. Every "median of 10" figure in this
  file, and every edge count, comes from it; `benchmarks/run.py` reports the single pass only.
  Each side's own virtualenv python ran it against the 0.7 branch's `benchmarks/` directory:

  ```
  <venv python> benchmarks/ab_driver.py benchmarks <label> <out.json> 10000
  ```

  The `"max_edges": None` control further down ran the same command with `RULE_MAX_EDGES=none` in
  the environment.
- The unmodified harness was also run three times per side:

  ```
  bindings/python/.venv/bin/python benchmarks/run.py --scale 10000 --out <file>
  ```

- **Load.** The machine never went fully quiet. The 1-minute load before measured runs was 2.6–4.5
  (median 3.3), from background system processes. Each run waited up to 1–2 minutes for load ≤ 3
  and then recorded what it was. Six early runs ran at load 7–18 and matched the quiet runs to
  within 1%. The workload is single-threaded on 12 cores.

## Like for like

Both sides ingest 10,000 nodes, hold 0 edges after ingest, and hold **448,000 derived edges after
backfill: 224,000 per rule**, 9,000 fires per rule, neither rule tripped. The WAL is 170.3 MB after
ingest on both and grows by 43.5 MB during backfill on both.

## A against B

Pooled over every default-workload run of the session, 20 per side:

| | A (v0.6.12), n=20 | B (0.7 branch), n=20 | B / A |
|---|---|---|---|
| Backfill | 8.611 s (8.418–8.715) | 8.584 s (8.424–8.734) | 0.997 |
| Ingest | 0.991 s (0.930–1.107) | 0.998 s (0.915–1.107) | 1.007 |
| Two-hop, median of 10 after 3 warmups | 195.3 µs | 192.1 µs (180.6–210.5) | 0.98 |
| Two-hop, single cold pass | 658 µs (582–1490) | 809 µs (698–1983) | 1.23 |
| Scan-filter | 1.65 ms | 1.66 ms (1.37–1.98) | 1.01 |

The first six runs, alternating A, B, A, B, A, B, with the load each ran at:

| Run | 1-min load | Ingest | Backfill total (rule 1 + rule 2) | Two-hop single pass | Two-hop median of 10 | Scan-filter | Derived edges |
|---|---|---|---|---|---|---|---|
| A1 | 7.28 | 1.107 s | 8.642 s (1.628 + 7.013) | 753.4 µs | 197.2 µs | 1.61 ms | 448,000 |
| B1 | 3.50 | 1.100 s | 8.612 s (1.660 + 6.952) | 787.3 µs | 195.8 µs | 1.65 ms | 448,000 |
| A2 | 3.90 | 1.096 s | 8.626 s (1.634 + 6.992) | 649.2 µs | 188.5 µs | 1.38 ms | 448,000 |
| B2 | 3.45 | 1.107 s | 8.598 s (1.644 + 6.954) | 1983.3 µs | 186.9 µs | 1.58 ms | 448,000 |
| A3 | 2.58 | 1.095 s | 8.558 s (1.652 + 6.906) | 793.3 µs | 206.0 µs | 2.06 ms | 448,000 |
| B3 | 2.81 | 1.100 s | 8.734 s (1.639 + 7.094) | 830.7 µs | 194.7 µs | 1.48 ms | 448,000 |

| | A median | B median | B / A |
|---|---|---|---|
| Backfill | 8.626 s | 8.612 s | 0.998 |
| Ingest | 1.096 s | 1.100 s | 1.004 |
| Two-hop, median of 10 | 197.2 µs | 194.7 µs | 0.987 |
| Two-hop, single pass | 753.4 µs | 830.7 µs | 1.10 |

## The unmodified harness, three runs per side

| Run | 1-min load | Ingest | Backfill | Two-hop single pass | Scan-filter |
|---|---|---|---|---|---|
| A-1 | 3.01 | 1.024 s | 8.577 s | 1.11 ms | 1.64 ms |
| B-1 | 3.84 | 1.112 s | 8.520 s | 1.00 ms | 1.41 ms |
| A-2 | 3.58 | 1.040 s | 8.614 s | 734.7 µs | 1.31 ms |
| B-2 | 3.44 | 1.061 s | 8.551 s | 792.4 µs | 2.06 ms |
| A-3 | 4.31 | 1.041 s | 8.577 s | 648.4 µs | 1.42 ms |
| B-3 | 3.46 | 1.034 s | 8.575 s | 3.32 ms | 1.41 ms |

The 0.7 branch's figures, as the median of its three runs (load 3.84, 3.44, 3.46). These are the
figures `README.md` quotes:

| Workload | Median of 3 | Raw | Cross-check, driver n=20 (load 2.8–4.5) |
|---|---|---|---|
| Bulk ingest, 10,000 nodes | **1.061 s** | 1.112, 1.061, 1.034 | 0.998 s (0.915–1.107) |
| Neighborhood depth-1, p50 / p95 | **0.4 µs / 3.2 µs** | p50 0.4, 0.5, 0.4; p95 15.4, 3.2, 2.7 | 0.35 µs / 9.4 µs |
| Neighborhood depth-2, p50 | **0.2 µs** | 0.2, 0.2, 0.2 | 0.19 µs |
| Cypher scan-filter-project, 1,400 rows | **1.41 ms** | 1.41, 2.06, 1.41 | 1.66 ms (1.37–1.98) |
| Cypher two-hop join, 200 rows, single cold pass | **1.00 ms** | 1.00 ms, 792.4 µs, 3.32 ms | 809 µs (698–1983) |
| Cypher two-hop join, 200 rows, median of 10 after 3 warmups, over 448,000 derived edges | **192.1 µs** | not produced by `run.py` | 192.1 µs (180.6–210.5), n=20 |
| Rule backfill, 2 rules, 448,000 edges derived (224,000 each, top 32 per source) | **8.551 s** | 8.520, 8.551, 8.575 | 8.584 s (8.424–8.734) |

## Where the 8.6 s came from

A equals B, so the search went back to the tags the earlier figures came from, with the same harness
and `max_edges` omitted, as the harness is written:

| Build | n | Backfill median | Derived edges | Ingest median | Two-hop median of 10 | Loads |
|---|---|---|---|---|---|---|
| v0.1.1 | 4 | 3.007 s (3.000, 3.015, 3.020, 2.901) | **2,000,000** | 0.852 s | 103.4 µs | 3.25 4.43 3.27 2.95 |
| `ee2efde`, parent of `6e3136d` | 2 | 3.035 s (3.013, 3.058) | **2,000,000** | 0.966 s | 99.5 µs | 7.04 17.98 |
| `6e3136d` | 2 | 8.645 s (8.670, 8.620) | **448,000** | 0.943 s | 179.8 µs | 16.54 14.29 |
| v0.2.0 | 4 | 8.736 s (8.797, 8.700, 8.690, 8.772) | 448,000 | 0.914 s | 193.0 µs | 2.97 3.64 2.92 2.82 |
| v0.3.0 | 3 | 8.706 s | 448,000 | 0.944 s | 191.1 µs | 2.86 3.53 2.92 |
| v0.4.0 | 3 | 8.877 s | 448,000 | 0.955 s | 184.4 µs | 2.90 4.18 2.86 |
| v0.5.0 | 3 | 8.705 s | 448,000 | 0.956 s | 187.2 µs | 2.73 3.63 3.00 |
| v0.6.0 | 3 | 8.641 s | 448,000 | 0.926 s | 190.6 µs | 2.86 2.98 2.99 |
| v0.6.6 | 3 | 8.714 s | 448,000 | 0.978 s | 193.2 µs | 2.97 5.19 4.04 |
| v0.6.10 | 3 | 8.533 s | 448,000 | 0.997 s | 188.5 µs | 2.97 4.22 4.02 |
| v0.6.12 (A) | 6 | 8.617 s | 448,000 | 1.066 s | 195.6 µs | 7.28 3.90 2.58 3.05 3.38 3.54 |
| 0.7 branch (B) | 6 | 8.584 s | 448,000 | 1.075 s | 190.0 µs | 3.50 3.45 2.81 3.73 3.34 3.51 |

**The commit: `6e3136d`**, 2026-08-25, first released in v0.2.0. One commit either side: 3.035 s and
2,000,000 edges before, 8.645 s and 448,000 edges after.

**Cause.** The benchmark's two rules omit `max_edges`. Before that commit an omitted `max_edges`
meant a global budget, and the backfill stopped as soon as a rule owned 1,000,000 edges — an early
exit after a fraction of the 7,000 sources. Since it, the Python binding fills an omitted
`max_edges` with a per-source top-k of 32, so the backfill evaluates every candidate for all 7,000
sources and keeps the best 32 each: 7,000 × 32 = 224,000 edges per rule.

So the earlier figure timed a truncated derivation and every release since v0.2.0 times a complete
one. The work per rule went up, most for the specialty rule: 2.1 s → 7.0 s. It is a deliberate
default change, not a defect.

### The old workload, requested explicitly (`"max_edges": None`)

This holds the derived set equal across versions: 2,000,000 edges everywhere.

| Build | n | Backfill median (raw) | Rule 1 | Rule 2 | WAL growth during backfill | Two-hop median of 10 | Loads |
|---|---|---|---|---|---|---|---|
| v0.1.1 | 5 | 2.973 s (2.973, 2.963, 2.801, 3.043, 3.042) | 0.887 | 2.082 | 0 MB | 100.8 µs | 6.29 3.58 2.78 3.67 2.85 |
| `6e3136d` | 1 | 2.975 s | 0.885 | 2.090 | 0 MB | 107.3 µs | 2.86 |
| `5fc65eb`, parent of `80babac` | 1 | 3.188 s | 0.917 | 2.271 | — | 107.4 µs | 4.20 |
| `80babac` | 1 | 4.974 s | 1.977 | 2.997 | — | 110.0 µs | 4.25 |
| v0.2.0 | 3 | 4.841 s (4.841, 5.009, 4.835) | 1.800 | 3.071 | 194.0 MB | 109.5 µs | 2.98 3.13 3.32 |
| v0.6.12 (A) | 4 | 4.594 s (4.985, 4.565, 4.593, 4.596) | 1.721 | 2.910 | 194.0 MB | 106.5 µs | 3.60 2.93 2.73 4.13 |
| 0.7 branch (B) | 4 | 4.795 s (4.826, 4.861, 4.560, 4.765) | 1.798 | 2.964 | 194.0 MB | 108.7 µs | 3.42 3.69 3.53 2.99 |

On this control the 0.7 branch against v0.6.12 is 4.795 s against 4.594 s, with overlapping ranges
(4.56–4.86 against 4.57–4.99).

## Secondary finding — history markers in the WAL

The control shows a second, smaller step, also before v0.2.0. **First commit with it: `80babac`**,
2026-08-28, which added WAL markers for derived edges being added and retracted, with rule
attribution. Either side of it: 3.188 s at its parent `5fc65eb`, 4.974 s at `80babac`.

Before it, a backfill wrote nothing to the WAL. Since it, every derived edge is also written as a
history marker after the commit. Measured cost: 194.0 MB for 2,000,000 edges, which is 97 bytes and
about 0.9 µs per edge; 43.5 MB for the default workload's 448,000 edges. This is the price of
`edge_history` and `was_linked` attribution, by design. On the default workload it is small:
8.645 s at `6e3136d` against 8.736 s at v0.2.0.

## Two-hop

- **Warm (median of 10 after 3 warmups): flat.** 195.3 µs on v0.6.12, 192.1 µs on the 0.7 branch.
  On the equal 2,000,000-edge set it is 100.8 µs on v0.1.1 and 108.7 µs on the 0.7 branch. The
  103 µs → 190 µs step at `6e3136d` is the different edge set, not the executor.
- **The single pass is a cold number.** With a trivial query placed between the backfill and the
  two-hop, the first read after the backfill commit costs 190–390 µs whatever the query is, and the
  second costs 10–15 µs. The two-hop's first execution then costs 450–490 µs on v0.6.12 and
  607–660 µs on the 0.7 branch (one outlier 1,484 µs), and 202–237 µs from the second pass on, on
  both.

## What was not established

- **The cold first-execution two-hop difference.** The 0.7 branch's cold pass is about 0.15 ms
  slower than v0.6.12's: eight extra alternating pairs gave A median 648 µs (587–1490), B median
  838 µs (790–1263), B above A in 7 of 8. A bisect across the 0.7 plan boundaries, 6 rounds
  round-robin at load 3.1–4.4, found no clean step:

  | Build | Cold single pass, median (raw) | Median of 10 | Backfill | Ingest |
  |---|---|---|---|---|
  | `fab9bdc` v0.6.12 | 693 µs (747, 656, 729, 648, 657, 772) | 191.8 µs | 8.617 s | 0.961 s |
  | `dec698e` end of plan 1 | 738 µs (943, 669, 5104, 723, 754, 694) | 192.0 µs | 8.555 s | 0.970 s |
  | `143143a` end of plan 2 | 832 µs (1069, 1122, 744, 738, 767, 897) | 193.0 µs | 8.635 s | 0.963 s |
  | `837cd4e` end of plan 3 | 859 µs (779, 797, 1497, 1469, 782, 921) | 189.1 µs | 8.548 s | 0.971 s |
  | `ae4d6c6` plan 3 amendments | 784 µs (782, 792, 756, 765, 786, 801) | 197.4 µs | 8.613 s | 0.954 s |
  | `4d68e9a` | 784 µs (798, 698, 761, 770, 894, 839) | 192.5 µs | 8.590 s | 0.987 s |

  Half the drift appears across plans 1–2, where no query-path source changed. The cause is not
  established; code layout in a rebuilt binary is the likeliest explanation and was not proved. It
  is a one-time cost of about 0.1–0.2 ms on the first execution of a query and does not recur.
- **The gradual ingest drift.** A equals B (ratio 1.007, n=20 each). Against v0.1.1 on the same
  harness the same day: 0.873 s (n=9, 0.760–0.981) → 0.998 s on the 0.7 branch, about +14%. The tag
  medians rise gradually (0.85, 0.91, 0.94, 0.96, 0.96, 0.93, 0.98, 1.00, 1.07) with no single
  step, and one binary spreads 0.92–1.11 s across single passes. Not localised; it predates 0.7.
  The earlier 784 ms was one single shot.
