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
```
