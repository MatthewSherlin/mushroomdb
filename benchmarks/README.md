# Comparative Benchmark Harness

Measures mushroomdb against Neo4j, KùzuDB, and Memgraph across five shared
workloads at configurable scale.  Competitor adapters skip automatically when
not installed — zero manual configuration required.

## Quick start

```bash
# Run from repo root using the bindings venv:
bindings/python/.venv/bin/python benchmarks/run.py                    # 10k scale
bindings/python/.venv/bin/python benchmarks/run.py --scale 2000       # CI scale
bindings/python/.venv/bin/python benchmarks/run.py --scale 50000 --out benchmarks/results/my-run.md
```

Output is written to `benchmarks/results/run-<scale>-<timestamp>.md`.  A
committed sample run at 10k (ours-only, competitors skipped) is at
`benchmarks/results/sample-results.md`.

## Running the test suite

```bash
# pytest — ours adapter runs end-to-end at 2k; competitors skip cleanly:
bindings/python/.venv/bin/python -m pytest benchmarks/test_harness.py -v
```

## Running with competitors installed

### Neo4j

```bash
# Start Neo4j Community via Docker:
docker run -d --name neo4j-bench \
    -p 7687:7687 -p 7474:7474 \
    -e NEO4J_AUTH=none \
    neo4j:5-community

# Install the Python driver:
pip install neo4j

# Then re-run:
bindings/python/.venv/bin/python benchmarks/run.py
```

### Memgraph

```bash
# Start Memgraph via Docker (note: same default bolt port as Neo4j — run only one at a time):
docker run -d --name memgraph-bench \
    -p 7687:7687 -p 7444:7444 \
    memgraph/memgraph-platform

# Install a driver (mgclient preferred; neo4j driver also works):
pip install mgclient
# or: pip install neo4j

# Then re-run:
bindings/python/.venv/bin/python benchmarks/run.py
```

### KùzuDB

```bash
# KùzuDB is embedded — no server needed:
pip install kuzu

# Then re-run:
bindings/python/.venv/bin/python benchmarks/run.py
```

---

## Honesty section

### What IS comparable across engines

The five workloads in the cross-engine table are structurally equivalent
across all engines:

| Workload | Description |
|---|---|
| `bulk_ingest` | Load 10k nodes (Talent / Company / Job shapes) |
| `neighborhood_depth1` | All edges for a sampled node (`node_edges` / bolt `MATCH (n)-[r]->(m)`) |
| `neighborhood_depth2` | Two-hop neighbourhood traversal |
| `cypher_scan_filter` | `MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key` — full-label scan, filter, project |
| `cypher_two_hop` | `MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)-[:INDUSTRY_ALIGNMENT]->(t2:Talent) LIMIT 200` |

### What is NOT comparable — and why

#### mushroomdb embedding vs. network bolt

mushroomdb numbers in `sample-results.md` are from an **embedded Rust process**
(no network RTT, no serialization overhead).  Neo4j and Memgraph numbers go
over bolt/localhost (~0.1–1 ms round-trip per query).  KùzuDB is also embedded,
so its numbers are directly comparable to mushroomdb's.  **Never compare
mushroomdb embedded numbers against Neo4j/Memgraph bolt numbers without
disclosing this.**

#### rule_derive — ours-only, no competitor equivalent

`rule_derive` measures the time to declare matcher rules (e.g.
`INDUSTRY_ALIGNMENT`, `SPECIALTY_MATCH`) and have mushroomdb automatically
backfill all derived edges across the existing dataset.  This workload is
**excluded from the cross-engine comparison table** for the following reasons:

1. **No equivalent in Neo4j / Memgraph / KùzuDB.** These engines do not
   auto-derive relationship edges from node properties.  Achieving similar
   results requires manual ETL scripts, APOC triggers (Neo4j Enterprise), or
   explicit Cypher `CREATE`/`MERGE` statements — none of which are semantically
   equivalent to mushroomdb's declarative rule system.

2. **The comparison would be misleading.** Running a manual ETL pass in
   Neo4j would measure Python loop overhead, not database engine performance.
   It would not reflect the cost of persistent incremental re-derivation on
   every future ingest/update.

3. **The feature is intentionally novel.** Auto-derivation with streaming
   backfill and incremental re-fire is a core mushroomdb capability.  Claiming
   a competitor "doesn't support it" as a performance win is dishonest; the
   right framing is that this is an ours-only workload with no apples-to-apples
   cross-engine baseline.

The `rule_derive` results appear in a dedicated section in the output Markdown,
clearly labeled **ours-only**, with this explanation repeated inline.

#### Synthetic embeddings

The dataset uses SHA-256 hash-chain synthetic embeddings
(`dogfood/transform.py:synthetic_embedding`), not real OpenAI/production
vectors.  Cosine similarity distributions differ from real data.  VectorSimilar
rules are not included in the benchmark workloads for this reason.

#### Scale vs. production

The default benchmark scale is 10k nodes.  The representative matching dataset
is 70k+ Talent, 20k+ Company.  Numbers do not extrapolate linearly for
O(n²) workloads (rule backfill, certain Cypher patterns).  Always re-run at
production scale before drawing conclusions.

---

## File layout

```
benchmarks/
  README.md             — this file
  run.py                — orchestrator CLI
  datasets.py           — dataset generator (wraps dogfood/synthesize.py)
  adapters/
    ours.py             — mushroomdb adapter (embedded Python bindings)
    neo4j.py            — Neo4j adapter (bolt; skip if absent)
    kuzu.py             — KùzuDB adapter (embedded; skip if absent)
    memgraph.py         — Memgraph adapter (bolt; skip if absent)
  test_harness.py       — pytest: ours end-to-end + competitor skip paths
  results/
    sample-results.md   — committed sample (10k, ours-only, this machine)
    *.md                — other runs (gitignored)
  ci/
    run.sh              — CI bench harness (builds + runs ci_bench example)
    compare.py          — regression gate (15% threshold, --bootstrap mode)
    test_compare.py     — pytest: 3 comparator unit tests
  baselines/
    ci.json             — NOT YET COMMITTED; written after first post-merge CI run (see "How to flip to enforcing mode")
```

---

## CI regression gate

### What it measures

The `bench` CI job runs `benchmarks/ci/run.sh`, which builds a release-mode
Rust example (`crates/core-api/examples/ci_bench.rs`) and times five wall-clock
metrics against a deterministic 10 000-node synthetic store (no external random
library — properties derived arithmetically from node indices):

| Metric | Description |
|---|---|
| `ingest_wall_s` | Wall time to ingest 10 000 Item nodes |
| `rule_backfill_wall_s` | Wall time to create 2 rules and complete their backfill |
| `snapshot_write_s` | Wall time to write a snapshot |
| `snapshot_open_s` | Wall time to re-open the store from the snapshot in-process (OS page cache warm, Rust structures cold) |
| `query_p50_ms` | p50 of 50 two-hop Cypher query executions (ms) |

Results are written to `results.json` and uploaded as a CI artifact named
`bench-results`.

### Threshold: 15%

The regression gate allows up to 15% slowdown relative to the committed
baseline before failing.  This is intentionally wide to absorb the timing
noise inherent on shared GitHub Actions runners (load spikes, cache hits,
runner variance across pools).  Do not tighten the threshold without evidence
that the runner class is stable enough to support it.

### Runner-class caveat

Baselines are captured on **ubuntu-latest GitHub Actions runners**.  Numbers
are not portable between runner classes (e.g. larger runners, macOS, arm64).
A baseline captured on one runner class will produce spurious failures if the
job later moves to a different class.  If the runner class changes, re-capture
the baseline on the new class before merging.

### Baselines: captured deliberately, never silently

Baselines live in `benchmarks/baselines/ci.json`.  They are committed by a
human after a deliberate capture run, not written automatically on every push.

The workflow currently runs in **bootstrap mode** (`--bootstrap`), which writes
the baseline artifact and exits 0.  This is intentional: the baseline cannot
exist before the CI runner has produced one.

### How to flip to enforcing mode

1. Merge this branch.  The first CI run writes `bench-results` (a GitHub
   Actions artifact); download `results.json` from that run.
2. Run a second workflow dispatch to warm any runner caches; download that
   `results.json` (run 2 is the canonical baseline per the task brief).
3. Copy it to `benchmarks/baselines/ci.json` and commit:
   ```
   cp /path/to/downloaded/results.json benchmarks/baselines/ci.json
   git add benchmarks/baselines/ci.json
   git commit -m "chore: commit CI bench baseline (ubuntu-latest runner)"
   ```
4. In `.github/workflows/ci.yml`, remove `--bootstrap` from the compare step
   so it reads:
   ```yaml
   python3 benchmarks/ci/compare.py results.json benchmarks/baselines/ci.json \
     --threshold 0.15
   ```
5. Push and confirm two consecutive main runs are green.

### Verifying a regression

To confirm the gate catches regressions, perturb `results.json` before
comparing:

```bash
python3 -c "
import json
with open('results.json') as f: r = json.load(f)
r['snapshot_open_s'] *= 1.20  # 20% slower — should fail
with open('results_bad.json', 'w') as f: json.dump(r, f)
"
python3 benchmarks/ci/compare.py results_bad.json benchmarks/baselines/ci.json --threshold 0.15
# exits 1 and prints the regressed metric
```

### Comparator tests

Unit tests for `compare.py` live in `benchmarks/ci/test_compare.py` and run
in the CI `python` job (which has pytest installed).  Three cases are covered:
within-threshold pass, regression fail, and missing-metric fail.

To run locally (requires `pip install pytest`):

```bash
python3 -m pytest benchmarks/ci/test_compare.py -v
```
