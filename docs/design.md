# Graph-DB Design Spec (working title — name TBD)

**Date:** 2026-08-14
**Status:** Living design document
**License:** Apache 2.0
**Language:** Rust (core), TypeScript (UI), Python/TS/Rust bindings at launch

---

## 1. Vision

An open-source, embedded-first property graph database whose defining feature is
**native automatic connection creation**: users declare general linking rules once,
and the engine maintains those edges incrementally, transactionally, on every write.
Secondary pillars: extreme read speed (in-memory engine, zero-copy everywhere) and a
first-class bundled UI that stays smooth at hundreds of thousands of nodes.

One-line positioning: *"The embedded graph database that builds itself."*

### Market rationale (validated 2026-08)

- Kuzu (the "SQLite for graphs") was archived Oct 2025 after Apple acquired it;
  community forks are thin. There is a live vacuum for a truly open-source embedded
  graph engine.
- No shipping engine offers declarative, incrementally maintained linking rules as a
  native primitive. Closest prior art: TigerGraph ER solution patterns (build-it-
  yourself, enterprise), Neo4j GDS similarity (batch, not incremental), RDF reasoners
  (SPARQL land), Materialize/differential dataflow (not a graph DB). The underlying
  CS is de-risked; the product does not exist.
- GQL became an ISO standard (April 2024); openCypher compatibility inherits the
  ecosystem. A custom query language is explicitly rejected.

### Primary user / dogfood target

Matthew's existing graph workloads (talent-backend Neo4j pipeline; marketplace-app
association graph). Current pain: 5+ second neighborhood queries, hand-built batch
edge-scoring scripts, UI unusable beyond ~40 nodes. These workloads are worked
examples only — see the Generality Guarantee.

## 2. Generality Guarantee

**No engine behavior may depend on specific label, edge-type, or field names.**
Labels, edge types, and fields are arbitrary user strings. Nodes support multiple
labels. Rule types are generic combinators over field predicates and compose with
`all(...)`. Any domain-specific name appearing in docs/examples is illustrative,
exactly as `employees` is in SQL documentation. The heterogeneous-source pattern
(graph derived from rows/JSON/events living in other systems) is a first-class
ingestion posture.

## 3. Locked Decisions

| Decision | Choice |
|---|---|
| Deployment shape | Embedded Rust core + optional thin server + bundled UI (DuckDB playbook); `graphdb ui mydb.graph` serves the UI locally |
| Query surface | Programmatic traversal API (primary) + openCypher subset (compat). No custom language, ever |
| Auto-linking | Layered: zero-config key/FK inference by default + declared incremental rules. LLM extraction is a possible later optional plugin, never core |
| Storage model | Memory-first: Sortledton-style dynamic adjacency + columnar properties; WAL + mmap'd zero-copy snapshots. Storage behind a Rust trait (future disk-native backend possible without touching query/rule layers) |
| Execution | Vectorized batches (~1–2k IDs per operator step), rayon-parallel traversals. Factorized processing / WCOJ deferred to v2 |
| Concurrency | Single writer + epoch-based snapshot reads (lock-free readers). No general MVCC |
| Results format | Apache Arrow everywhere: zero-copy to pandas/polars/JS; Arrow IPC over WebSocket to UI. JSON exists nowhere in the data path |
| UI rendering | cosmos.gl (GPU force layout + rendering; OpenJS Foundation) |
| Bindings | Python (PyO3), TypeScript (napi-rs), Rust — all at launch, generated/derived from one core-api source of truth, shared conformance suite |
| Testing | Deterministic simulation testing (FoundationDB-style) from day one + model-based oracle testing + rule-equivalence invariant + differential Cypher testing vs Neo4j |
| Scale target | Design for 10M nodes in RAM (~5–15 GB with properties); document the RAM ceiling honestly. Real initial workloads are ~10k nodes |

### Explicit non-goals (v1)

Distributed operation; graphs larger than RAM; GPU query execution; WASM build;
full openCypher coverage; LLM-based extraction; io_uring; multi-writer.

## 4. Architecture

### 4.1 Workspace layout

```
graph-db/
├── crates/
│   ├── core-storage      # topology + columnar properties + WAL + snapshots
│   ├── core-rules        # linking rules, per-rule indexes, incremental maintenance
│   ├── core-query        # vectorized executor; traversal ops + openCypher subset
│   ├── core-api          # the ONE public Rust interface; typed error enums
│   ├── arrow-bridge      # results ↔ Arrow buffers (zero-copy)
│   ├── bindings-python   # PyO3 thin wrapper over core-api
│   ├── bindings-node     # napi-rs thin wrapper over core-api
│   ├── server            # axum HTTP + WebSocket (Arrow IPC); serves UI
│   └── sim-harness       # DST: virtual clock, fault-injecting IO, seeded runner
├── ui/                   # TypeScript + cosmos.gl explorer/console/rule-inspector
└── cli/                  # `graphdb` binary: open, serve UI, rebuild, stats
```

Dependency rule (inward only):
`bindings/server/cli → core-api → {core-query, core-rules} → core-storage`.
Storage knows nothing about rules; rules know nothing about Cypher; UI speaks only
Arrow-over-WebSocket. `core-storage` exposes an IO trait so `sim-harness` swaps real
disk for the fault simulator without touching engine code. Crate boundaries are also
multi-agent build boundaries.

### 4.2 Storage internals

- **IDs:** user keys (default field `id`) hash-mapped once to dense internal `u32`.
  All internal structures use dense IDs — integer-array adjacency, no pointer chasing.
- **Topology:** per-vertex sorted neighbor blocks per (edge-type, direction), per
  Sortledton (VLDB 2022): near-CSR scan speed, cheap inserts, ~2.1× CSR memory,
  simple design. Edges partitioned by type so typed expansion touches only relevant
  blocks.
- **Properties:** columnar, stored away from topology; lazily loaded per column.
  Strings dictionary-encoded/interned. Set-valued fields → roaring bitmaps over an
  interned dictionary (overlap scoring = SIMD bitmap AND). Recognized geo pairs →
  R-tree-indexable points. Null bitmaps per column.
- **Durability:** WAL (CRC-checksummed records) + background snapshots in an
  rkyv-style zero-copy archived format. Open = mmap snapshot (milliseconds) + replay
  WAL tail. Snapshot files versioned + checksummed; ambiguous/corrupt files rejected
  loudly.

## 5. Data Model & Write Path

**Model:** property graph. Nodes: ≥1 label, user key, properties. Edges: type,
optional properties, direction. Schema is declared-optional: schema-free ingest
materializes columns with inferred types; declared labels get strict validation.

**Linking rules** are first-class schema objects:

```python
db.rules.create(
    "shared_tags",
    between=("Article", "Article"),
    when=all(overlap("tags", min=0.3), numeric_within("published_year", 2)),
    edge="RELATED", weight="overlap_score",
)
```

v1 rule predicate library: `key_match` (FK-style; also runs zero-config by default),
`field_equal`, `overlap`, `numeric_within`, `geo_radius`, composable via `all(...)`.
Fast-follow: user-defined scoring functions (UDF escape hatch). Each rule declares
watched fields and maintains its own index (hash / token / R-tree / sorted).

**Write path (one insert/update):**

1. WAL append (only mandatory disk touch; fsync per policy).
2. ID map + topology + columns updated in memory.
3. **Incremental rule firing:** changed fields wake only watching rules; each probes
   its own index for candidate partners (never a scan), computes scores,
   writes/updates/deletes its derived edges. Derived edges carry rule provenance;
   they are not hand-editable and are removed cleanly on `rule delete`. Updates diff
   old vs new field values and touch only affected partner edges.
4. **Epoch publish:** atomic pointer flip makes node + derived edges visible
   together. Auto-linking is synchronous and transactional — never an
   eventually-consistent background job.

Budget: sub-millisecond per insert at 10k–100k-node scale; bulk loads amortize rule
firing per batch. Rule create/delete on a live graph = online backfill/removal job
with progress reporting; crash-safe via WAL (resume or clean rollback).

## 6. Read Path & Query Surface

**Traversal API** (primary; identical shape in all bindings):

```python
db.node("Person", key="u123")                       # O(1) point lookup
  .neighborhood(depth=2, edge_types=[...])          # typed expansion
  .grouped_by_edge_type()                           # {edge_type: [nodes]}
db.nodes("Person").where(f.score > 0.5).traverse(...)
db.explain(node_a, node_b)                          # rule + score breakdown
```

All results are Arrow tables (plus `.to_dicts()` convenience).

**openCypher subset (v1):** `MATCH` (fixed + variable-length paths), `WHERE`,
`RETURN`/`ORDER BY`/`LIMIT`/`SKIP`, `CREATE`/`SET`/`DELETE`, parameters, core
function library (aggregations, string/math). Not in v1: full `MERGE` semantics
(simple upsert only), subqueries, `FOREACH`. Parser produces a logical plan shared
with the traversal API — one executor, two front doors.

**Server/UI transport:** WebSocket sessions, incremental Arrow IPC batch delivery —
first batch in milliseconds, cosmos.gl lays out while the rest streams. UI node
expansion = `neighborhood()` call, not a Cypher round-trip. Server enforces
per-query timeout and result-size caps.

## 7. Bundled UI (v1 scope)

1. **Explorer:** open a node, see typed neighborhood groups, expand smoothly at
   500+ nodes (GPU layout + rendering kills canvas caps), filter by
   label/type/score.
2. **Query console:** Cypher / traversal-API input; results as graph or table.
3. **Rule inspector:** list rules, edge counts per rule, non-participation counts,
   duplicate-edge-type flags, and per-edge "why" panel (`db.explain` — rule + score
   breakdown on edge click). This view is the UI expression of the flagship feature.

Not v1: general editing/admin UI, dashboards, saved queries.

## 8. Error Handling & Durability Contract

- **Contract:** a committed write survives crash, power loss, kill -9. Torn WAL
  tails detected via CRC and dropped whole — never half-applied. Recovery target
  < 1 s (bounded by snapshot cadence).
- **Fsync policy:** `strict` (per commit) / `batched` (default, per N ms) /
  `relaxed` (bulk loads).
- **Ingest:** lenient by default (unknown fields → new columns; type conflicts →
  tagged mixed representation + counted warning). Declared schemas = hard errors
  with row detail. No silent drops; all coercions/skips queryable via
  `db.stats().warnings`.
- **Rules:** validated at declaration (fields/types). Missing field on a node ⇒
  node doesn't participate (counted, shown in inspector; normal sparse-data
  behavior). Two rules → same edge type allowed, flagged; provenance prevents
  collision.
- **Resource limits:** memory warnings at 80% of ceiling; writes refused with clear
  error at 100%. Never OOM-killed mid-write, never silently degraded.
- **API errors:** typed enums in core-api, mapped to idiomatic exceptions in
  Python/TS.

## 9. Testing Strategy

1. **Deterministic simulation testing** (sim-harness): virtual clock +
   fault-injecting IO trait; thousands of seeded scenarios per CI run (crash at
   every WAL/snapshot offset, torn writes, fsync lies); byte-for-byte replay from
   seed.
2. **Model-based property testing:** random op sequences run against the engine and
   a deliberately naive in-memory oracle; exact-match required.
3. **Rule-equivalence invariant:** after any op sequence, incremental edges ==
   from-scratch `rebuild`. Shrunken repro on failure.
4. **Differential Cypher testing** vs Neo4j on the supported subset; cargo-fuzz on
   parser and WAL/snapshot readers.
5. **Cross-binding conformance:** one shared corpus (queries + expected Arrow
   results) through Rust/Python/TS in the CI matrix.
6. **Performance:** criterion microbenchmarks with CI regression gates; public
   reproducible benchmark harness vs Neo4j, Kuzu 0.11.3, Memgraph (LDBC-SNB-style
   interactive + typed-neighborhood workload); numbers, hardware, and rerun scripts
   in-repo.
7. **UI:** automated Playwright frame-rate tests (500 / 5k / 50k nodes) asserting
   interaction latency.

## 10. Performance Targets (v1 acceptance)

| Metric | Target |
|---|---|
| Point lookup + depth-2 typed neighborhood, 10k-node graph | < 100 µs engine-side |
| Same, 10M-node graph | < 10 ms |
| Insert with 5 active rules, 100k-node graph | < 1 ms |
| DB open (5 GB snapshot) | < 100 ms |
| UI: click-to-rendered neighborhood (500 nodes, end-to-end) | < 100 ms |
| UI: smooth interaction | 50k+ nodes without frame collapse |
| Replaces talent-backend Neo4j usage | current 5+ s queries < 50 ms end-to-end |

## 11. Open Items

- **Name** — required before repo creation; "graph-db" is a placeholder.
- Git repo not yet initialized (user performs/authorizes git actions explicitly).
- UDF rule escape hatch design (fast-follow, not v1).
- LLM-extraction plugin (post-v1, optional, opt-in cost model).
- Snapshot format spec doc (write during implementation planning).

## 12. Next Step
