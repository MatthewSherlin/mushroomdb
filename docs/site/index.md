# mushroomdb

mushroomdb is an embedded graph database for any app that needs a graph — and a memory layer for AI
agents. A relationship is a schema declaration: write a rule once, and every write
derives, maintains and **retracts** the matching edges, each one carrying the rule, the score and
the values that produced it.

It speaks [Cypher](query.md), keeps [time-travel history](timetravel.md), and searches by
[full text](fulltext.md) and [vector similarity](vector.md).

---

## What it does

You declare a rule once. mushroomdb evaluates it on every subsequent write and
maintains the matching edges automatically — adding them when properties align,
retracting them when they no longer do. The graph builds itself.

This is different from running a similarity script after each batch load or
manually creating edges in a trigger. The rules are schema objects: they carry
provenance, produce a score on each derived edge, and are queryable via the
`explain` API and the bundled UI's why panel.

Six predicate kinds ship today:

| Display name | Wire shape (JSON `predicate` field) | What it tests |
|---|---|---|
| KeyMatch | `{"KeyMatch": {"field": "..."}}` | FK equality — one node's field matches another's key |
| FieldEqual | `{"FieldEqual": {"field": "..."}}` | Exact match on a named field (any `ValueKey`: string, int, float, bool) |
| Overlap | `{"Overlap": {"field": "...", "min": 0.5}}` | Jaccard coefficient on list-valued fields, min threshold |
| NumericWithin | `{"NumericWithin": {"field": "...", "tolerance": 2.0}}` | Absolute numeric difference within a tolerance |
| GeoRadius | `{"GeoRadius": {"field": "...", "km": 50.0}}` | Haversine distance between `[lat, lon]` fields within a radius in km |
| VectorSimilar | `{"VectorSimilar": {"field": "...", "min": 0.8}}` | Cosine similarity on float arrays, min threshold |

All six compose with `All` to require multiple conditions on the same edge.

---

## Positioning

mushroomdb is embedded-first: it runs in your process, against a directory on
disk, with no network round-trip and no server to start. The optional `serve`
command adds an HTTP API and a bundled graph explorer when you want them, and
several processes can share one store — see [Concurrency](concurrency.md).

`ingest-git` stays supported as a **data source**: commits, pull requests, files and authors become
entities with rule-derived relationships, which is what makes a ticket↔commit link a rule rather
than a script.

The benchmark numbers are in [Benchmarks](benchmarks.md), and the architecture as built, with the
roadmap, is in [Architecture](architecture.md).

---

## Status

Pre-1.0 alpha — APIs and formats may change between minor versions. Single
writer, no multi-statement transactions. Toolchain pinned to Rust 1.92.0.

The current release is on the [releases page](https://github.com/MatthewSherlin/mushroomdb/releases).

---

## Pages in this section

- [Get started](get-started.md) — pick a language: [Rust](get-started-rust.md), [Python](get-started-python.md), [TypeScript](get-started-typescript.md), [Docker](get-started-docker.md), or [an existing Cypher app over Bolt](get-started-bolt.md)
- [Drivers and Bolt](drivers-bolt.md) — placeholder until the Bolt server lands
- [Transactions](transactions.md) — placeholder until transactions land
- [Quickstart](quickstart.md) — a store, an assistant wired to it, and the four questions with one worked call each
- [Concepts](concepts.md) — how it works: stores, nodes, rules, explain, time, access, memory
- [Rules](rules.md) — all six predicate kinds with examples
- [API reference](api.md) — which page covers which surface
- [HTTP API](http.md) — every route `mushroomdb serve` answers
- [Python guide](python.md) — the binding, with worked examples
- [`ingest-git` as a data source](ingest-git.md) — what `ingest-git` writes, its rules, submodules, pull requests, incremental sync
- [Install](install.md) — one row per way in, and what each leaves out
- [Install, plugin and hooks](skill.md) — the two install routes, what each writes, and `doctor`
- [MCP tools](mcp.md) — the eleven task tools, the fourteen graph tools, and the twenty-three every store lists
- [The association benchmark](association-bench.md) — one world in three forms, the twenty questions, and the gate
- [Concurrency](concurrency.md) — many readers, one writer; the write lock, `Busy`, and `refresh`
- [Node masks and access control](masks.md) — role tokens, client masks, restricted-stub mode
- [Running it as a service](service.md) — the volume, the snapshot interval, backup, restore, and what a restart costs
- [Panic policy](panic-policy.md) — which conditions panic vs. return a typed error
- [Known limits](limits.md) — what it does not do, and what was measured
- [Benchmarks](benchmarks.md) — its own numbers, each with the file it comes from
- [Architecture](architecture.md) — the crates, the storage format, and the roadmap

Reference:

- [CLI reference](cli.md)
- [MCP tools](mcp-tools.md) — every tool with its arguments, generated from the server's own listing
- [Python API](python-api.md) — every method of `GraphDb`, generated from the type stub
- [Rust](rust.md) — embedding the engine: the types you touch, with examples that are compiled
- [TypeScript client](typescript.md) — the HTTP client: its methods, and the routes it leaves to `fetch`
- [Cypher Query Reference](query.md)
- [Bolt protocol](bolt.md) — the versions the server negotiates and the driver audit behind them
- [Temporal history: what was connected when, and why](timetravel.md)
- [Full-Text Search](fulltext.md)
- [Vector search](vector.md) — similarity rules, `find_similar`, exact and approximate, the index
- [Property (equality) indexes](indexes.md)
- [Graph Algorithms](algorithms.md)
- [Materialized Property Views](views.md)
- [Live Subscriptions](subscriptions.md)
- [Rule Suggestion](suggest.md)
- [Durability and crash recovery](durability.md)
- [Running several processes against one store](multiprocess.md)
- [Self-hosting guide](self-host.md)
- [Deployment](deployment.md)
- [Testing](testing.md)

---

## Install for an AI agent

The shortest way in is the Claude Code plugin — `claude plugin marketplace add
MatthewSherlin/mushroomdb` then `claude plugin install mushroom@mushroomdb`, and type
`/mushroom:mushroom` in a repository. Or run `npx mushroomdb install`, which writes the
`/mushroom` skill, the MCP server entry, and the session-start and prompt hooks for Claude Code
or Cursor. The crates.io (`cargo install mushroomdb-cli`, `cargo add mushroomdb`) and PyPI
(`pip install mushroomdb`) packages are published at the same version; which path gives you what
is in the [Install](install.md) table.
