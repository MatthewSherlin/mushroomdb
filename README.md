<img src="docs/assets/mark-animated.svg" align="left" width="132" alt="" />

# mushroomdb

An embedded Rust property-graph database with native incremental linking rules.
You declare a predicate once; every later write maintains the matching edges
(and retracts them when properties change). The graph builds itself.

<br clear="left" />

---

## The differentiator

Most graph databases require you to create edges manually or run a batch
similarity script after each load. mushroomdb makes edge creation a schema
declaration. A rule like "connect every Person to every Org whose `skills`
list overlaps theirs by at least 50%" is written once:

```rust
db.create_rule(RuleDef {
    name: "skill_fit".into(),
    src_label: "Person".into(),
    dst_label: "Org".into(),
    predicate: Predicate::Overlap { field: "skills".into(), min: 0.5 },
    edge_type: "FIT".into(),
    weight_prop: Some("score".into()),
    max_edges: None,
}).expect("rule");
```

After that, every `insert_node` and `set_prop` evaluates the rule
incrementally. The engine writes the edge, stores the Jaccard score, and
retracts the edge if the properties later diverge — without any manual work.

The why panel in the bundled explorer shows exactly which rule fired, the
field values that matched, and the computed score for every derived edge.

![Why panel showing overlap rule arithmetic](docs/assets/03-why-overlap.png)

![Neighborhood with derived edges highlighted](docs/assets/02-neighborhood-gold.png)

---

## Quickstart

The two-command flow uses the release binary with the UI embedded:

```text
mushroomdb demo ./db
mushroomdb serve ./db --addr 127.0.0.1:8080
```

Build the embedded binary first:

```text
cd ui && npm ci && npm run build && cd ..
cargo build -p cli --bin mushroomdb --features embed-ui --release
cp target/release/mushroomdb ~/.local/bin/  # or any directory on PATH
```

Or run directly from the source tree (no copy needed):

```text
./target/release/mushroomdb demo ./db && ./target/release/mushroomdb serve ./db --addr 127.0.0.1:8080
```

Open `http://127.0.0.1:8080/`. The demo graph has 10 Orgs, 20 Projects,
30 People, and 334 edges — 304 of them derived by seven rule sets.

`mushroomdb demo ./db` output:

```text
== demo ==
ingested 10 Orgs, 20 Projects, 30 People
overlap rule: skill_fit (Person.skills ∩ Project.skills, min 0.5)
numeric rule: founded_within (Org.founded_year, tolerance 2)
geo rule: nearby_office (Org.office [lat,lon], 50 km)
vector rule: similar_interests (Person.embedding dim 8, min 0.8)

== auto-FK rules ==
  auto_fk_person_org_id
  auto_fk_person_project_id
  auto_fk_project_org_id

== query ==
MATCH (p:Person {id: 'person-01'})-[r:FIT]->(proj:Project)
RETURN p, proj, r.score AS score
ORDER BY score DESC, proj

columns: p, proj, score
  p=person-01  proj=proj-01  score=1.0
  p=person-01  proj=proj-02  score=0.5
  p=person-01  proj=proj-20  score=0.5

== explain (person-01, proj-01) ==
  rule=auto_fk_person_project_id  type=PROJECT  person-01→proj-01  weight=none
  rule=skill_fit  type=FIT  person-01→proj-01  weight=1.0

== serve ==
  mushroomdb serve ./db
```

`mushroomdb serve ./db --addr 127.0.0.1:8080` output:

```text
listening on http://127.0.0.1:8080
```

Without the embedded binary (cargo only, debug build):

```text
cargo run -p cli --bin mushroomdb -- demo ./demo-db
cargo run -p cli --bin mushroomdb -- serve ./demo-db
```

---

## Rules tour

Six predicate kinds ship today. All compose via `All(...)`.

| Predicate | What it tests |
|---|---|
| `KeyMatch` | FK equality — source field matches destination key |
| `FieldEqual` | Exact string match on a named field |
| `Overlap` | Jaccard on list-valued fields, min threshold |
| `NumericWithin` | Absolute numeric difference within a tolerance; score = `1 - |Δ|/tolerance` |
| `GeoRadius` | Haversine distance on `[lat, lon]` fields within km; score = `1 - dist/radius` |
| `VectorSimilar` | Cosine similarity on float arrays, min threshold |

Auto-FK: fields ending in `_id` whose values match existing node keys get
`KeyMatch` rules created automatically at ingest time.

Approximate mode: `VectorSimilar` accepts `approximate: true`, which
switches the candidate path to IVF-Flat. Per-query recall ≥ 0.90
quiesced, ≥ 0.85 immediately post-rebuild. Measured at 5k nodes /
dim 1536: exact ~12 min backfill, approximate ~17 s. Use it when
backfill latency matters more than completeness; document the recall
trade-off for your workload.

Full predicate reference and examples: [`docs/site/rules.md`](docs/site/rules.md).

---

## Benchmarks

10,000-node graph (Apple M4 Pro, macOS 15.7.3, arm64). Full methodology
and honesty notes: [`benchmarks/results/head-to-head-10k.md`](benchmarks/results/head-to-head-10k.md).

| Workload | mushroomdb | Neo4j | KùzuDB | Memgraph |
|---|---|---|---|---|
| Bulk ingest | 8.761 s | 12.460 s | 1.21 min | 46.24 ms † |
| Neighborhood depth-1 (p50) | 1.1 µs | 1.81 ms | 99.7 µs | 3.12 ms |
| Neighborhood depth-1 (p95) | 14.0 µs | 12.81 ms | 476.5 µs | 3.90 ms |
| Neighborhood depth-2 (p50) | 0.9 µs | 6.78 ms | 1.04 ms | 2.97 ms |
| Cypher scan-filter-project | 5.11 ms | 84.81 ms | 1.78 ms | 11.56 ms † |
| Cypher two-hop join | 1.273 s ‡ | 74.25 ms | 1.09 ms | 1.08 ms |

**Honesty notes:**

- mushroomdb numbers are **embedded** (no network RTT, no serialization
  overhead). KùzuDB is also embedded — its numbers are directly comparable
  to mushroomdb's. Neo4j and Memgraph numbers go over bolt/localhost
  (~0.1–1 ms round-trip per query).
- † Memgraph adapter stores only the `key` field (not full node properties).
  Bulk ingest skips property serialization. Cypher scan-filter returns 0
  rows (`size_bucket` was not stored) — not semantically equivalent to the
  neo4j/mushroomdb results for those workloads.
- ‡ Earlier: returned error _intermediate result exceeds 1,000,000 rows_.
  Fixed by LIMIT pushdown (pull-based executor, no materialization).
  Re-measured at 3.5k-node scale: 71 µs median. Full 10k re-measurement
  pending (T5).

Rule derivation (mushroomdb-only, excluded from cross-engine table):
two-rule backfill on 10k nodes completed in 20.7 s. Competitors have no
auto-derivation equivalent; this workload has no cross-engine baseline.

---

## Architecture

```
graph-db/
├── crates/
│   ├── core-storage      # topology + columnar properties + WAL + snapshots
│   ├── core-rules        # linking rules, per-rule indexes, incremental maintenance
│   ├── core-query        # vectorized executor; traversal ops + Cypher subset
│   ├── core-api          # the one public Rust interface; typed error enums
│   ├── arrow-bridge      # results ↔ Arrow buffers
│   ├── bindings-python   # PyO3 thin wrapper over core-api
│   ├── server            # axum HTTP + WebSocket; serves UI
│   └── sim-harness       # DST: virtual clock, fault-injecting IO, seeded runner
├── ui/                   # TypeScript + Vite graph explorer
├── bindings/python/      # maturin package
└── cli/                  # mushroomdb binary
```

Dependency rule (inward only):
`bindings/server/cli → core-api → {core-query, core-rules} → core-storage`

Storage uses a CRC-checksummed WAL with per-commit fsync, plus versioned
snapshots in a zero-copy archived format. Open = snapshot + WAL replay.
Derived edges are not WAL-logged; they are re-materialized from node data
on open by replaying rule application.

Concurrency: single writer, many readers via `RwLock`-backed `SharedDb`.
Lock-free epoch snapshot readers are on the roadmap.

Results surface as Apache Arrow everywhere: zero-copy to pandas/polars in
Python bindings, Arrow IPC over WebSocket to the UI.

---

## Known limitations

| Limitation | Detail |
|---|---|
| Two-hop Cypher joins at scale | Dense patterns that produce >1,000,000 intermediate rows still error without `LIMIT`. Add `LIMIT n` to any such query — the pull-based executor stops early and never materializes the full binding table. |
| Cold-start re-fires all rules | Derived edges are not persisted. Re-opening a rich-rule graph re-derives every edge from node data. At 100k nodes with two vector rules, expect ~8 minutes. Derived-edge persistence is roadmap item #1. |
| Approximate vector mode is opt-in | `approximate: true` enables IVF-Flat candidate selection. Per-query recall ≥ 0.90 quiesced; ≥ 0.85 post-rebuild. Review the recall trade-off before using it in completeness-critical workloads. |
| Memory-first | The in-memory store is RAM-bound. Design target is 10M nodes (~5–15 GB with properties). mmap-backed storage is on the roadmap. |
| Demo refuses existing directories | `mushroomdb demo` exits 1 if the target directory is non-empty, including hidden files (`.DS_Store` counts). Use a fresh path. |
| No node or edge deletes | Not implemented. Nodes can be tombstoned but not removed; derived edges are retracted on property change. |
| No multi-statement transactions | Single-write commits only. |
| Cypher aggregations: no grouping | `COUNT(*)`, `COUNT(n)`, `SUM`, `AVG`, `MIN`, `MAX` on a single property are supported. Grouped aggregation (`RETURN a, COUNT(*)`) is not — the planner rejects it with a clear error. Multi-aggregate RETURN is also v1-limited. |

---

## What the server and CLI expose

| Command | What it does |
|---|---|
| `mushroomdb demo <dir>` | Write a deterministic demo graph (10 Orgs, 20 Projects, 30 People) |
| `mushroomdb serve <dir>` | Start the HTTP server + optional UI |
| `mushroomdb mcp <dir>` | Start a stdio MCP JSON-RPC server for agent tools |
| `mushroomdb stats <dir>` | Print node/edge/rule counts |

Full HTTP endpoint reference: [`docs/site/api.md`](docs/site/api.md).

---

## Roadmap

| Priority | Item |
|---|---|
| High | Derived-edge persistence (snapshot includes derived edges; eliminates cold-start re-derivation) |
| Medium | Lock-free epoch snapshot readers (replacing the `RwLock` facade) |
| Medium | Node and edge deletes |
| Medium | mmap-backed storage (RAM-independent at rest) |
| Medium | Multi-statement transactions |
| Medium | Expanded Cypher surface (grouped aggregations `RETURN a, COUNT(*)`, variable-length paths) |
| Low | TypeScript bindings (napi-rs) |
| Low | WASM playground |
| Low | Time-travel queries |

---

## Distribution

Pre-alpha. No tag has been pushed. The one-liners below are the intended
front door **after the first `v*` tag**; they are not available until then.

### Docker (after the first v* tag)

```text
docker run --rm -p 8080:8080 ghcr.io/matthewsherlin/mushroomdb
```

The image CMD runs `mushroomdb serve /data --addr 0.0.0.0:8080 --demo-if-empty`
(writes the demo graph into the volume when empty, then serves).
Explicit two-step:

```text
docker run --rm -v mushroomdb-data:/data ghcr.io/matthewsherlin/mushroomdb demo /data
docker run --rm -p 8080:8080 -v mushroomdb-data:/data ghcr.io/matthewsherlin/mushroomdb serve /data --addr 0.0.0.0:8080
```

Local image build (available now):

```text
docker build -t mushroomdb:local .
docker run --rm -p 8080:8080 mushroomdb:local
```

### npm (after the first v* tag)

```text
npx mushroomdb --help
```

### curl / install.sh (after the first v* tag)

```text
curl -fsSL https://raw.githubusercontent.com/MatthewSherlin/graph-db/main/packaging/install.sh | sh
```

Writes `~/.local/bin/mushroomdb` (no sudo). Fetches the matching GitHub
Release tarball and checksum-verifies it.

---

## Testing

Rust gate (required before every commit touching `crates/`):

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --workspace --examples
cargo test --workspace
cargo bench --no-run
```

Node gate (commits touching `ui/`):

```text
cd ui && npm ci && npm run typecheck && npm test -- --run && npm run build
```

Python gate (commits touching `bindings/python/`):

```text
cd bindings/python
python -m venv .venv && .venv/bin/pip install -U pip maturin pytest
.venv/bin/maturin develop && .venv/bin/pytest
```

The test suite uses deterministic simulation testing (fault-injecting
`SimFs`, crash recovery), model-based oracle equivalence testing, and
differential Cypher testing against Neo4j on the supported subset.
See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full testing philosophy.

---

## Docs

- Quickstart: [`docs/site/quickstart.md`](docs/site/quickstart.md)
- Rules reference: [`docs/site/rules.md`](docs/site/rules.md)
- API reference: [`docs/site/api.md`](docs/site/api.md)
- Design spec: [`docs/design.md`](docs/design.md)
- Benchmarks: [`benchmarks/results/head-to-head-10k.md`](benchmarks/results/head-to-head-10k.md)
- Dogfood report: [`docs/dogfood-report.md`](docs/dogfood-report.md)

---

## License

Apache-2.0. Copyright 2026 Matthew Sherlin. See [`LICENSE`](LICENSE).
