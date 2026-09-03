<img src="docs/assets/mark-animated.svg" width="110" align="right" alt="" />

# mushroomdb

[![stars](https://img.shields.io/github/stars/MatthewSherlin/mushroomdb?style=flat&logo=github)](https://github.com/MatthewSherlin/mushroomdb/stargazers)
[![crates.io](https://img.shields.io/crates/v/mushroomdb-cli?logo=rust&label=crates.io)](https://crates.io/crates/mushroomdb-cli)
[![npm](https://img.shields.io/npm/v/mushroomdb?logo=npm&label=npm)](https://www.npmjs.com/package/mushroomdb)
[![PyPI](https://img.shields.io/pypi/v/mushroomdb?logo=python&logoColor=white&label=PyPI)](https://pypi.org/project/mushroomdb/)
[![CI](https://img.shields.io/github/actions/workflow/status/MatthewSherlin/mushroomdb/ci.yml?branch=main&label=CI)](https://github.com/MatthewSherlin/mushroomdb/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

**The graph that stays true — and knows who's allowed to see it.**

An embedded Rust graph database where edges are a schema declaration: write a rule once, and
every write creates, maintains, and retracts the matching edges. Ships a 16-tool MCP server.

*Pre-1.0 alpha — APIs and formats may change between minor versions.*

[Docs](docs/site/index.md) · [Changelog](CHANGELOG.md) · [Issues](https://github.com/MatthewSherlin/mushroomdb/issues)

![Ingest this repository, query which files co-change, hand one file to a new owner, and watch the KNOWS edges follow in the same write](docs/assets/ingest-git-cascade.gif)

## Quick start

```sh
npx mushroomdb install                        # /mushroom skill + MCP server + recall hook
mushroomdb ingest-git ~/.mushroomdb/memory .  # Author, Commit, File nodes; CO_CHANGED + KNOWS by rule
```

Then type `/mushroom` in Claude Code (or Cursor) to query, explain, and time-travel that graph
from the assistant.

- **Live, not a snapshot.** One `SET f.top_author_id = …` moves the `TOP_AUTHOR` edge *and*
  re-derives that author's `KNOWS` edges before the write closes — the `SET` in the GIF above is
  that one write.
- **Retracts instead of going stale.** Re-run `ingest-git` to sync: only new commits replay,
  deleted files drop their derived edges, and renamed files carry their history to the new path.
- **Explains any link.** `explain` names the rule and the score behind an edge, so *"which files
  change together with `src/api.rs`, and why?"* has an answer your assistant can quote instead of
  a guess.
- **Knows who's allowed to see it.** Pass a `mask` with a query and the same graph answers
  differently per caller; write statements are rejected on masked queries.
- **Answers what it said last week.** `mushroomdb asof ./db --commit 5 --query "…"` replays the
  WAL to a past commit, derived edges included.

## Where it fits

**What it is**

- An embedded, single-binary graph database with a rule engine that maintains edges for you.
- A 16-tool MCP server plus a `/mushroom` skill for Claude Code and Cursor.
- Local-first: your data stays on disk, no cloud service, no LLM in the write path.

**What it isn't**

- Not a hosted memory service — there is no account, no endpoint, nothing to sign up for.
- Not a vector database. Vector predicates and HNSW are built in; bring your own embeddings.
- Not a Postgres replacement. Single writer, no interactive transactions, memory-first storage.

---

## The differentiator

Most graph databases require you to create edges manually or run a batch similarity script after
each load. mushroomdb makes edge creation a schema declaration. A rule like "connect every Person
to every Org whose `skills` list overlaps theirs by at least 50%" is written once:

```rust
db.create_rule(RuleDef {
    name: "skill_fit".into(),
    src_label: "Person".into(),
    dst_label: "Org".into(),
    predicate: Predicate::Overlap { field: "skills".into(), min: 0.5 },
    edge_type: "FIT".into(),
    weight_prop: Some("score".into()),
    max_edges: Some(5), // keep the 5 best-matching Orgs per Person (top-k per source)
}).expect("rule");
```

After that, every `insert_node` and `set_prop` evaluates the rule incrementally. The engine writes
the edge, stores the Jaccard score, and retracts the edge if the properties later diverge — without
any manual work.

Watch it live — a Cypher `SET` changes one property, the `founded_within` rule fires, and new
scored edges appear in the bundled explorer:

![A SET statement deriving new scored edges live](docs/assets/demo-set-derives.gif)

Open the Rules panel, and the Why slide-over shows the exact predicate arithmetic behind every
derived edge:

![Rules panel and Why slide-over showing overlap arithmetic](docs/assets/rules-why.gif)

### Predicates

Six predicate kinds ship today. They compose via `All(...)` (AND, score = min) and `Any(...)`
(OR, score = max), nested up to depth 4.

| Predicate | What it tests |
|---|---|
| `KeyMatch` | FK equality — source field matches destination key |
| `FieldEqual` | Exact match on a named scalar field (string, int, float, bool) |
| `Overlap` | Jaccard on list-valued fields, min threshold |
| `NumericWithin` | Absolute numeric difference within a tolerance; score = `1 - |Δ|/tolerance` |
| `GeoRadius` | Haversine distance on `[lat, lon]` fields within km; score = `1 - dist/radius` |
| `VectorSimilar` | Cosine similarity on float arrays, min threshold |

Auto-FK: fields ending in `_id` whose values match existing node keys get `KeyMatch` rules created
automatically at ingest time. `VectorSimilar` accepts `approximate: true` to switch candidate
selection to in-tree HNSW (per-query recall min 0.90, mean 0.998 at 5k nodes / dim 1536,
fixed-seed probe). Full reference: [`docs/site/rules.md`](docs/site/rules.md).

### Built on the same engine

- **Live subscriptions.** `subscribe_rule` (Rust) and `GET /subscribe` (WebSocket) stream
  `EdgeFired` / `EdgeRetracted` the moment they hit the WAL — not polled, not batched. Bounded
  65,536-event queue; slow consumers get a `Lagged { missed: N }` marker instead of a disconnect.
  [`docs/site/subscriptions.md`](docs/site/subscriptions.md)
- **Rule attribution across time.** Every derived edge writes a HISTORY-MARKER WAL record carrying
  the rule name, so `edge_history`, `node_history`, and `was_linked` answer *which* rule created a
  link and *at which commit*. `GraphDb::open_at(&dir, 5)` replays to a past commit, derived edges
  included; out-of-range commits return `CommitOutOfRange`, never wrong data.
  [`docs/site/timetravel.md`](docs/site/timetravel.md)
- **Materialized views.** Degree counts and neighbor aggregates (sum/avg/min/max) maintained
  incrementally on every edge change — no cron, no triggers, no stale caches.
  [`docs/site/views.md`](docs/site/views.md)
- **Rule suggestions.** `db.suggest_rules()` (or `mushroomdb suggest ./db`) profiles your data and
  ranks candidate rules with estimated edge counts and rationale. Seeded sampling, so the same
  database always returns the same suggestions. No rule is ever applied automatically.
  [`docs/site/suggest.md`](docs/site/suggest.md)

---

## Agent memory

Graph structure captures the shape of real knowledge — entities, associations, similarity, and
lineage — and rule-derived edges keep those associations fresh as new facts arrive.

- Entities map to nodes (`Person`, `Document`, `Project`, `Concept`, …).
- Associations are edges derived from data: cosine similarity on embeddings, shared field values,
  FK relationships, geographic proximity. Declare a rule once; every write maintains the matching
  edges without agent-side bookkeeping.
- Recall has three modes: `find_similar` by query vector (HNSW when available, brute force
  otherwise); `find_similar` by key (neighbors along a rule-derived edge type); `query` for
  structured Cypher recall. `hybrid_search` fuses fulltext and vector results via Reciprocal Rank
  Fusion.
- Explanations are built in: `explain_association` shows which rules and scores produced each
  link, so an agent can cite evidence instead of asserting a conclusion.
- Node masks are the ACL primitive: pass `mask: [key1, key2, …]` to `query` to restrict the visible
  node set. Write statements are rejected on masked queries.
  [`docs/site/masks.md`](docs/site/masks.md)
- Schema-as-code: `mushroomdb schema apply <dir> <schema.json>` idempotently applies rules, views,
  and fulltext indexes, printing a created/updated/unchanged diff.

**Minimal workflow** (four tool calls):

```text
upsert_entity  →  create_rule  →  find_similar  →  explain_association
  (store)           (link)           (recall)          (explain)
```

**All sixteen MCP tools:**

| Tool | Purpose |
|---|---|
| `upsert_entity` | Insert or update a node by key (no existence check needed) |
| `ingest_json` | Batch-ingest nodes of one label from a JSON array |
| `create_rule` | Declare a derivation rule; backfills existing nodes immediately |
| `find_similar` | Find similar nodes by query vector (HNSW) or by derived edge traversal |
| `hybrid_search` | RRF over fulltext + vector results |
| `explain_association` | Show rules and scores that link two nodes |
| `explain` | Alias for `explain_association` |
| `query` | Cypher query (read or write); pass `mask` for ACL-scoped read |
| `neighborhood` | Multi-hop neighborhood traversal with optional edge-type filter |
| `node_info` | Return a node's key, label, and properties |
| `node_edges` | Return all edges incident on a node |
| `stats` | Live node, edge, and rule counts |
| `node_history` | WAL change history for a node (since last truncating snapshot) |
| `edge_history` | Add/retract lifecycle for edges between two nodes, with rule attribution |
| `was_linked` | Point-in-time edge check: was an edge active at a given commit? |
| `rename_node` | Rename a node's key; old_key, new_key |

Full walkthrough, tool reference, and Claude Desktop setup: [`docs/site/mcp.md`](docs/site/mcp.md).
Skill and recall-hook details: [`docs/site/skill.md`](docs/site/skill.md).

---

## Install options

```sh
npx mushroomdb install            # skill + MCP server + recall hook, no toolchain needed
cargo install mushroomdb-cli      # `mushroomdb` binary from crates.io (no embedded UI)
cargo add mushroomdb              # embedded Rust library
pip install mushroomdb            # Python bindings
```

To see the bundled explorer, write a demo graph and serve it:

```sh
mushroomdb demo ./db
mushroomdb serve ./db
```

Open `http://127.0.0.1:8080/`. The demo graph has 10 Orgs, 20 Projects, 30 People, and 334
edges — 304 of them derived by seven rule sets. When a token is configured, open
`http://host:8080/?token=…`. Building the binary with the UI embedded, Docker, and the
`install.sh` script are covered in [CONTRIBUTING.md](CONTRIBUTING.md).

**Role-bound tokens** limit a caller to a named subset of nodes. Define roles in `schema.json`
under the `roles` key (each role has a `label` selector list), then pass `--role-token TOKEN:ROLE`
(repeatable) when starting the server, or set `MUSHROOMDB_ROLE_TOKENS="tok1:role1,tok2:role2"`.
A role token receives only the nodes matching its label selectors — read endpoints return rows
filtered to the visible set; write, subscription, and analytics endpoints return 403. Unknown token
or role name: 401. The never-widen invariant is enforced in the server: a client-supplied mask is
always intersected with the role mask. The MCP interface (`mushroomdb mcp`) is a stdio JSON-RPC
server for local agent use and is not subject to bearer-token or role enforcement.

---

## CLI reference

| Command | What it does |
|---|---|
| `mushroomdb install [--platform claude-code\|cursor\|all] [--project] [--db <path>]` | Write the `/mushroom` skill + MCP server entry for Claude Code or Cursor. Auto-detects platform |
| `mushroomdb uninstall [--platform …] [--project] [--db <path>]` | Remove exactly what `install` wrote (manifest-driven; leaves user files) |
| `mushroomdb ingest-git <dir> <repo> [--exclude <pattern>]...` | Graph a git repository: `Author`, `Commit`, `File` nodes plus `CO_CHANGED` and `KNOWS` rules. Re-run to sync. See [`docs/site/ingest-git.md`](docs/site/ingest-git.md) |
| `mushroomdb recall <dir>` | Hook body for the `/mushroom` skill's `UserPromptSubmit` recall hook: reads a prompt payload on stdin, prints related graph facts. Wired automatically by `install` |
| `mushroomdb mcp <dir>` | Start a stdio MCP JSON-RPC server for agent tools |
| `mushroomdb demo <dir>` | Write a deterministic demo graph (10 Orgs, 20 Projects, 30 People) |
| `mushroomdb serve <dir>` | Start the HTTP server + optional UI (default `127.0.0.1:8080`; `--token` on non-loopback; `--role-token TOKEN:ROLE`) |
| `mushroomdb query <dir> <cypher>` | Run a Cypher read or write (`--query` also accepted) |
| `mushroomdb asof <dir> --commit N` | Read-only view at a WAL commit |
| `mushroomdb stats <dir>` | Print node/edge/rule counts |
| `mushroomdb suggest <dir>` | Rank candidate linking rules (scored top-k 32, KeyMatch 1) |
| `mushroomdb schema apply <dir> <schema.json>` | Idempotently apply a schema file (rules, views, fulltext indexes); prints a diff |
| `mushroomdb snapshot <dir> [--keep-wal]` | Write `snapshot.bin` (truncates WAL unless `--keep-wal`) |
| `mushroomdb verify <dir>` | Audit snapshot integrity: CRC32 all 12 sections, exit 2 on any mismatch |
| `mushroomdb migrate <dir>` | Migrate an older store format in place |
| `mushroomdb backup <dir> <dest>` | Copy store files to `<dest>` and CRC-verify the copy. WARNING: unsafe against a running `serve` — use `POST /backup` for live-served stores |
| `mushroomdb export <dir> <dest> [--format jsonl\|parquet]` | Export nodes, edges, and rules. JSONL is byte-identical across runs; Parquet is not across library versions |
| `mushroomdb algo pagerank\|wcc\|degree <dir> [--top N]` | PageRank, weakly-connected components, or degree centrality over manual + derived edges |
| `mushroomdb --version` | Print the CLI's version and exit |

**Concurrency:** `ingest-git` and other CLI write commands open the database directory directly and
have no coordination with a running `mushroomdb serve` process's locking. Do not run them against a
store a live `serve` holds — write through the HTTP API instead. The `recall` hook is the same
hazard unattended: it opens the store on every prompt. It opens without migration or WAL repair
(`auto_migrate: false`, `repair_wal: false`) so it writes nothing, but it has no coordination with a
live `serve` either.

Full HTTP endpoint reference: [`docs/site/api.md`](docs/site/api.md).

---

## Known limitations

| Limitation | Detail |
|---|---|
| Memory-first | The in-memory store is RAM-bound. Design target is 10M nodes (~5–15 GB with properties). mmap-backed storage is deferred. |
| Single writer, no interactive transactions | One writer, many readers via `RwLock`. `write_batch` commits all ops in one WAL frame (all-or-nothing on crash replay) but is **not isolated**: readers may observe intermediate states while a committed batch is applied in memory. Multi-statement `BEGIN`/`COMMIT` is not supported. |
| Cold start without a snapshot re-fires all rules | Snapshots persist derived edges, ANN state, and view definitions. At 100k nodes / ~10M derived edges: **0.02 s** from a V8 snapshot vs **8.16 min** WAL-only (ANN re-fit dominates). Call `snapshot()` before close. See [`dogfood/results/scale-100k.md`](dogfood/results/scale-100k.md). |
| Two-hop Cypher joins at scale | Dense patterns producing >1,000,000 intermediate rows error without `LIMIT`. Add `LIMIT n` — the pull-based executor stops early and never materializes the full binding table. |
| Cypher write subset | CREATE, MATCH…SET, MATCH…DELETE, MATCH…DETACH DELETE, and MERGE (single-key, with `ON CREATE SET` / `ON MATCH SET`) are supported. Derived edges cannot be deleted manually. Variable-length paths are hard-capped at 10 hops; unbounded `*min..` is rejected at parse time. Full coverage table: [`docs/site/query.md`](docs/site/query.md). |
| Approximate vector mode is opt-in | `approximate: true` enables HNSW candidate selection. Per-query recall min 0.90, mean 0.998 at 5k / dim 1536 (fixed-seed probe). Review the trade-off before using it in completeness-critical workloads. |
| Demo refuses existing directories | `mushroomdb demo` exits 1 if the target directory is non-empty, including hidden files (`.DS_Store` counts). Use a fresh path. |
| Python bindings return dicts | pandas/polars zero-copy is not wired yet. HTTP `POST /query` defaults to Arrow IPC; JSON via `?format=json`. |

---

## Benchmarks

10,000-node graph (Apple M4 Pro, macOS 15.7.3, arm64), mushroomdb v0.1.1 release build, 2026-08-24.
Full methodology and honesty notes:
[`benchmarks/results/head-to-head-10k-v2.md`](benchmarks/results/head-to-head-10k-v2.md).

| Workload | mushroomdb | Neo4j | KùzuDB | Memgraph |
|---|---|---|---|---|
| Bulk ingest | 784 ms | 13.2 s | 1.21 min | 12.5 s |
| Neighborhood depth-1 (p50) | 0.4 µs | 1.22 ms | 99.6 µs | 1.34 ms |
| Neighborhood depth-1 (p95) | 2.2 µs | 1.46 ms | 519 µs | 2.14 ms |
| Neighborhood depth-2 (p50) | 0.2 µs | 7.18 ms | 1.08 ms | 9.22 ms |
| Cypher scan-filter-project (1.4k rows) | 1.22 ms | 93.7 ms | 3.95 ms | 83.7 ms |
| Cypher two-hop join (200 rows) | 261.6 µs ★ | 3.99 ms ★ | 1.59 ms ★ | 1.96 ms ★ |
| Cold-start: V8 snapshot open | 0.02 s ▽ | — | — | — |
| Cold-start: WAL-only open | 8.16 min ▽ | — | — | — |
| Server boot-to-ready | n/a (embedded) | 6.6 s | n/a (embedded) | 4.3 s |

**Honesty notes:**

- mushroomdb numbers are **embedded** — no network round-trip, no serialization overhead. KùzuDB
  is also embedded, so its numbers are directly comparable. Neo4j and Memgraph go over
  bolt/localhost (~0.1–1 ms round-trip per query).
- ★ Two-hop join: same dataset, same warmup policy, all four engines on **5,810,000
  INDUSTRY_ALIGNMENT edges**. Fresh process → ingest + preload → 3 discarded warmups → median of 10
  runs. mushroomdb derives the edges via `create_rule`; competitors were pre-loaded via UNWIND MERGE
  or COPY FROM CSV. All engines return 200 rows.
- ★ Earlier v2.1 two-hop values were **retracted** for cross-engine contamination; the v2
  mushroomdb 307 µs figure was **retired** (measured on a smaller 1M-edge graph). Both are
  documented in the methodology file rather than quietly dropped.
- ▽ 100k cold-start measured 2026-08-28, warm file cache, cold process, `/usr/bin/time -l`:
  V8 snapshot open 0.02 s at 31–41 MiB RSS; snapshot size 1.8 GiB; snapshot write ~35 s. Cold-cache
  was not measured. See [`dogfood/results/scale-100k.md`](dogfood/results/scale-100k.md).
- Rule engine vs hand-rolled maintenance (10k nodes, 1,000 specialty updates, drift = 0 for all
  three): per-op expert-written **64.93 min**, batched expert-written **24.98 s**, rule engine
  **17.58 s**. Both hand-rolled variants were written by the engine team with full knowledge of
  retraction semantics — drift = 0 is a property of that, not of hand-rolling in general.
  [`benchmarks/results/handrolled-vs-rules.md`](benchmarks/results/handrolled-vs-rules.md)

---

## Architecture

```text
graph-db/
├── crates/
│   ├── core-storage      # Packed adjacency topology + columnar property store + WAL + snapshots
│   ├── core-rules        # linking rules, per-rule indexes, incremental maintenance
│   ├── core-query        # pull-based interpreter; traversal ops + Cypher subset
│   ├── core-api          # the one public Rust interface; typed error enums
│   ├── arrow-bridge      # results ↔ Arrow buffers
│   ├── server            # axum HTTP + WebSocket; serves UI
│   ├── cli               # mushroomdb binary
│   └── sim-harness       # DST: virtual clock, fault-injecting IO, seeded runner
├── ui/                   # TypeScript + Vite graph explorer
├── bindings/python/      # PyO3 / maturin
└── clients/typescript/   # HTTP + WebSocket client
```

Dependency rule (inward only):
`bindings/server/cli → core-api → {core-query, core-rules} → core-storage`

Storage uses a dense-id WAL with per-commit fsync (configurable via `FsyncPolicy`), plus mmap-able
V8 rkyv snapshots (12 sections: CSR topology, columnar properties, HNSW blobs, provenance, IVF
state, per-node last-change index, and more — zero-copy, no heap allocation on open). V5/V6/V7
stores are auto-migrated to V8 on `GraphDb::open`. Derived edges are not WAL-logged; they are
restored directly from the mmap'd sections. See [`docs/format-stability.md`](docs/format-stability.md)
for the format evolution contract.

---

## Roadmap

Phases 1–4 and Plan 18 all landed. What remains:

| Priority | Item |
|---|---|
| Medium | mmap snapshots; lock-free epoch readers |
| Medium | v1.0 format stability (snapshot + WAL semver guarantee) |
| Low | `CASE` in a write-statement `RETURN`; subqueries; napi-rs; WASM |
| Low | Multi-statement `BEGIN/COMMIT` interactive transactions |

---

## Docs

- [Quickstart](docs/site/quickstart.md) · [Rules](docs/site/rules.md) · [Cypher reference](docs/site/query.md) · [HTTP + MCP API](docs/site/api.md)
- [Agent skill and recall hook](docs/site/skill.md) · [MCP tools](docs/site/mcp.md) · [Codebase graph](docs/site/ingest-git.md)
- [Time travel](docs/site/timetravel.md) · [Subscriptions](docs/site/subscriptions.md) · [Views](docs/site/views.md) · [Rule suggestions](docs/site/suggest.md)
- [Masks and access control](docs/site/masks.md) · [Full-text search](docs/site/fulltext.md) · [Property indexes](docs/site/indexes.md) · [Graph algorithms](docs/site/algorithms.md)
- [Durability and recovery](docs/site/durability.md) · [Panic policy](docs/site/panic-policy.md) · [Testing](docs/site/testing.md) · [Format stability](docs/format-stability.md)
- [Design spec](docs/design.md) · [Moat roadmap](docs/site/roadmap-moat.md) · [Case study](docs/dogfood-report.md)

Building from source, Docker, packaging, and the test gates are in
[CONTRIBUTING.md](CONTRIBUTING.md).

---

## License

Copyright 2026 Matthew Sherlin.

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
