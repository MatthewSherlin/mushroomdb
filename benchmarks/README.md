# Benchmark harness

Measures mushroomdb's embedded engine on six workloads at a configurable scale. The engine runs
in the benchmark's own process: no figure here includes a network round-trip or serialization.

## Run it

```bash
# From the repository root, with the binding's virtualenv.
(cd bindings/python && .venv/bin/maturin develop --release)   # a debug build is not worth quoting
bindings/python/.venv/bin/python benchmarks/run.py                    # 10,000 nodes
bindings/python/.venv/bin/python benchmarks/run.py --scale 2000       # the size CI's tests use
bindings/python/.venv/bin/python benchmarks/run.py --scale 10000 --out benchmarks/results/my-run.md
```

Output goes to `benchmarks/results/run-<scale>-<timestamp>.md` unless `--out` names a file. The
run the README quotes is `benchmarks/results/mushroomdb-10k-0.7.md`.

```bash
# The harness's own tests: every workload end to end at 2,000 nodes.
bindings/python/.venv/bin/python -m pytest benchmarks/test_harness.py -v
```

## The workloads

The graph is synthetic and seeded (`datasets.py`): Talent, Company and Job nodes in a 70/20/10
split, with the properties the rules read.

| Workload | What is timed |
|---|---|
| `bulk_ingest` | Inserting every node through `ingest_batch`, 2,000 nodes per call |
| `neighborhood_depth1` | `node_edges` for a sample of keys; p50 and p95 |
| `neighborhood_depth2` | `node_edges`, then each neighbour's; p50 and p95 |
| `cypher_scan_filter` | One `MATCH … WHERE … RETURN` over a label |
| `cypher_two_hop` | A two-hop join through a rule-derived edge type, `LIMIT 200` |
| `rule_derive` | Declaring the rules over the loaded graph: the backfill, per rule and in total |

`cold_start_to_first_query` (opening a store and answering one query) is exercised by the
tests and is not in the results table; the open-time figures the README quotes come from
`dogfood/results/scale-100k.md`.

## What a number here means

- One run, one machine, one process. The results file records the machine, the date and the
  scale; a figure without those is not a figure.
- Latencies under a microsecond are in-memory adjacency reads. They say the engine adds little
  to a pointer walk, not that a service built on it answers in a microsecond.
- `rule_derive` is a one-time cost paid when a rule is declared. Afterwards each write
  re-derives only what it changed.

## Hand-rolled maintenance versus rules

`run_handrolled.py` and `adapters/handrolled.py` compare the rule engine with edge maintenance
written by hand against the same store — per operation, and batched. It is a comparison of two
ways to use mushroomdb, and its result is `benchmarks/results/handrolled-vs-rules.md`.

## A/B against another build

`ab_driver.py` runs the same workload sequence under the same timers and also records what
`run.py` does not: derived-edge counts, WAL sizes, the machine's load, and a warm two-hop median
of 10 after 3 warmups. It measures whichever `mushroomdb` its interpreter has installed, so an A/B
is one virtualenv per build. `benchmarks/results/mushroomdb-10k-0.7-ab.md` was made with it.

## Older results

`benchmarks/results/` keeps earlier runs as they were committed. Where an older file compared
engines, the others appear as `system A`, `system B` and `system C`.
