# Benchmarks

mushroomdb's own numbers. Each row names the committed file it comes from, and each file records
its machine, date and command. Embedded: no figure includes a network round-trip.

| Workload | Result | Measured |
|---|---|---|
| Bulk ingest, 10,000 nodes | 1.061 s | 0.7 branch at `4d68e9a`, 2026-10-01 |
| Neighborhood depth-1 (p50 / p95) | 0.4 µs / 3.2 µs | 0.7 branch at `4d68e9a`, 2026-10-01 |
| Neighborhood depth-2 (p50) | 0.2 µs | 0.7 branch at `4d68e9a`, 2026-10-01 |
| Cypher scan-filter-project (1,400 rows) | 1.41 ms | 0.7 branch at `4d68e9a`, 2026-10-01 |
| Cypher two-hop join (200 rows), median of 10 after 3 warmups, over 448,000 derived edges | 192.1 µs | 0.7 branch at `4d68e9a`, 2026-10-01 |
| Cypher two-hop join (200 rows), single cold pass | 1.00 ms | 0.7 branch at `4d68e9a`, 2026-10-01 |
| Rule backfill, 2 rules, 448,000 derived edges | 8.551 s | 0.7 branch at `4d68e9a`, 2026-10-01 |
| Open from a snapshot, 100,000 nodes / ~10M derived edges | 0.02 s at 31–41 MiB RSS | v0.2, 2026-08-28 |
| Open from the WAL alone, same store | 8.16 min | v0.1.1, 2026-08-24 |

Rows 1–7: [`benchmarks/results/mushroomdb-10k-0.7-ab.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/benchmarks/results/mushroomdb-10k-0.7-ab.md),
Apple M4 Pro, 1-minute load 3.4–3.8 on 12 cores. Each is the median of three runs of the harness,
except the warm two-hop, which is the median of twenty runs of `benchmarks/ab_driver.py`;
`benchmarks/run.py` reports the single pass only. The released 0.6.12 was measured alongside on
the same day and is indistinguishable. That comparison is the driver's twenty runs a side, not
the medians of three above: backfill 8.611 s for 0.6.12 against 8.584 s for this tree, ingest
0.991 s against 0.998 s, warm two-hop 195.3 µs against 192.1 µs. The first single run on this
tree is
[`mushroomdb-10k-0.7.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/benchmarks/results/mushroomdb-10k-0.7.md). Rows 8–9:
[`dogfood/results/scale-100k.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/dogfood/results/scale-100k.md), warm file cache, cold process;
cold-cache was not measured. A memory-shaped run to 1,000,000 nodes on the 0.8.0 wheel — ingest, `remember`, `recall`, open from a snapshot and from the WAL — is
[`dogfood/results/scale-memory-1m.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/dogfood/results/scale-memory-1m.md).

Runs from 2026-08-21 and 2026-08-24 reported 2.85–3.51 s for the rule backfill. That was a
different workload: before v0.2.0 a rule without an explicit `max_edges` stopped at a global cap,
and since v0.2.0 it keeps the top 32 per source when declared through the Python binding, as the
harness does, so the backfill evaluates every source. The
earlier two-hop figure, 261.6 µs, was a warm median over 5.81M derived edges, a different edge set,
and the earlier 784 ms ingest was a single shot
([`head-to-head-10k-v2.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/benchmarks/results/head-to-head-10k-v2.md),
[`regression-v0.1-20260821.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/benchmarks/results/regression-v0.1-20260821.md),
[`regression-v0.1.1-20260824.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/benchmarks/results/regression-v0.1.1-20260824.md)).

Rule engine against hand-rolled maintenance (10,000 nodes, 1,000 specialty updates, drift 0 for
all three): per-operation hand-written **64.93 min**, batched hand-written **24.98 s**, rule
engine **17.58 s**. Both hand-rolled variants were written by the engine team with full
knowledge of retraction semantics — drift 0 is a property of that, not of hand-rolling in
general. [`benchmarks/results/handrolled-vs-rules.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/benchmarks/results/handrolled-vs-rules.md)

**Agent benchmarks** measure the entity engine directly. `benchmarks/agent-tasks/` runs real
`claude -p` sessions against executable truth. The association suite (`--suite association`) asks
twenty relationship questions of one generated world written three ways — as JSON files, as a
single relational file, and as a mushroomdb store. **The pre-registered gate passed on 0.6.12**, in
[`20260928T195302Z`](https://github.com/MatthewSherlin/mushroomdb/blob/main/benchmarks/agent-tasks/results/20260928T195302Z/summary.md), on a
2,000-entity world — the first run in which it has. The graph arm scored **1.000 on all twenty
tasks and all sixty cells** against the relational baseline's 0.990, at **$0.0614 against
$0.1127**:

| | graph (R) | relational (Q) | delta | 95% interval |
|---|---|---|---|---|
| score | **1.000** | 0.990 | +0.0101 | `[0.0005, 0.0229]` |
| cost $ | **0.0614** | 0.1127 | -45.6% | `[-0.06875, -0.03147]` |
| total tokens | **155,397** | 211,487 | -26.5% | `[-95482, -8268]` |
| turns | **3.93** | 7.07 | -44.3% | `[-4.017, -2.133]` |

That run was on 0.6.12, whose default listing was 19 tools; 0.7's is 23 and about 15% larger,
so the cost row is not 0.7's. The suite has not been re-run on 0.7.

180 cells, 0 timeouts, 0 errors, 0 dropped. Correctness had been amended on 2026-09-11 to pass on
a tie, because the relational arm saturated at 1.00 and the original wording — exceed both baselines,
interval excluding zero — was judged unpassable; **this run passes the original wording.** Every
one of the six sub-1.0 cells belongs to a baseline, and each key they missed was named correctly by
the graph arm in the same rep:
[`classification.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/benchmarks/agent-tasks/results/20260928T195302Z/classification.md).

The run before it ([`20260925T200950Z`](https://github.com/MatthewSherlin/mushroomdb/blob/main/benchmarks/agent-tasks/results/20260925T200950Z/summary.md))
failed on correctness by one key on one task. That key was a date-resolution defect in the engine,
not a limit of the surface, and it is fixed in 0.6.12. The code suite (`--suite code`) is retired;
its committed summaries stay as the record.
[`docs/site/association-bench.md`](association-bench.md) describes the suite and how to
rebuild the world.
