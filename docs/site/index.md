# mushroomdb

An embedded Rust property-graph database with native incremental linking rules.

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

mushroomdb is embedded-first (same process, no network round-trip), like
DuckDB or SQLite. The optional `serve` command adds an HTTP API and a
bundled graph explorer when you want them.

The roadmap and the benchmark numbers are in [README.md](../../README.md). The
full design spec is at [docs/design.md](../design.md).

---

## Status

Pre-1.0 alpha — APIs and formats may change between minor versions. Single
writer, no multi-statement transactions. Toolchain pinned to Rust 1.92.0.

v0.5.1 is the current release. Install it with `npx mushroomdb install`, which
writes the `/mushroom` skill, the MCP server entry, and the recall hook for
Claude Code or Cursor. The crates.io (`cargo install mushroomdb-cli`,
`cargo add mushroomdb`) and PyPI (`pip install mushroomdb`) packages are live at
the same version. Docker, `install.sh`, and the build-from-source path are in
[CONTRIBUTING.md](../../CONTRIBUTING.md).

---

## Pages in this section

- [Quickstart](quickstart.md) — two commands to a running graph explorer
- [Rules](rules.md) — all six predicate kinds with examples
- [API reference](api.md) — HTTP endpoints, MCP tools, Python bindings
- [Codebase graph](ingest-git.md) — `ingest-git`, its co-change and ownership rules, incremental sync
- [Node masks and access control](masks.md) — role tokens, client masks, restricted-stub mode
- [Panic policy](panic-policy.md) — which conditions panic vs. return a typed error
