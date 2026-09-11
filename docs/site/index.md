# mushroomdb

mushroomdb is the data layer for agents that reason over entities. It is an embedded Rust graph
database in which a relationship is a schema declaration: write a rule once, and every write
derives, maintains and **retracts** the matching edges, each one carrying the rule, the score and
the values that produced it.

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

**Deprecated in 0.6.4:** the code-graph door — the `explore`, `map`, `context`, `impact`, `owners`,
`why` and `sync` tools, the three grep/edit hooks, and the plugin's coding-assistant positioning. It
still works and is still tested; it is **removed in 0.7**. See
[Deprecations](../../README.md#deprecations).

The roadmap and the benchmark numbers are in [README.md](../../README.md). The
full design spec is at [docs/design.md](../design.md).

---

## Status

Pre-1.0 alpha — APIs and formats may change between minor versions. Single
writer, no multi-statement transactions. Toolchain pinned to Rust 1.92.0.

v0.6.5 is the current release. The shortest way in is the Claude Code plugin —
`claude marketplace add MatthewSherlin/mushroomdb` then `claude plugin install
mushroom@mushroomdb`, and type `/mushroom:mushroom` in a repository. Or run
`npx mushroomdb install`, which writes the `/mushroom` skill, the MCP server
entry, the prompt and post-edit hooks, and the git hooks for Claude Code or
Cursor. The crates.io (`cargo install mushroomdb-cli`, `cargo add mushroomdb`)
and PyPI (`pip install mushroomdb`) packages are live at the same version.
Docker, `install.sh`, and the build-from-source path are in
[CONTRIBUTING.md](../../CONTRIBUTING.md).

---

## Pages in this section

- [Quickstart](quickstart.md) — a store, an assistant wired to it, and the four questions with one worked call each
- [Rules](rules.md) — all six predicate kinds with examples
- [API reference](api.md) — HTTP endpoints, MCP tools, Python bindings
- [Codebase graph](ingest-git.md) — `ingest-git`, its rules, submodules, pull requests, incremental sync
- [Install, plugin and hooks](skill.md) — the two install routes, what each writes, and `doctor`
- [MCP tools](mcp.md) — the fourteen task tools, the thirteen graph tools, and the listing each store gets
- [The association benchmark](association-bench.md) — one world in three forms, the twenty questions, and the gate
- [The live code graph](code-graph.md) — **deprecated in 0.6.4, removed in 0.7**: what the repository graph guarantees, measured
- [Concurrency](concurrency.md) — many readers, one writer; the write lock, `Busy`, and `refresh`
- [Node masks and access control](masks.md) — role tokens, client masks, restricted-stub mode
- [Running it as a service](service.md) — the volume, the snapshot interval, backup, restore, and what a restart costs
- [Panic policy](panic-policy.md) — which conditions panic vs. return a typed error
