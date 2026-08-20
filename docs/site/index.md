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

| Predicate | What it tests |
|---|---|
| `key_match` | FK equality — one node's field matches another's key |
| `field_equal` | Exact string match on a named field |
| `overlap` | Jaccard coefficient on list-valued fields, min threshold |
| `numeric_within` | Absolute numeric difference within a tolerance |
| `geo_radius` | Haversine distance between `[lat, lon]` fields within a radius in km |
| `vector_similar` | Cosine similarity on fixed-dimension float arrays, min threshold |

All six compose with `All` to require multiple conditions on the same edge.

---

## Positioning

mushroomdb is embedded-first (same process, no network round-trip), like
DuckDB or SQLite. The optional `serve` command adds an HTTP API and a
bundled graph explorer when you want them.

The design spec and roadmap are in `README.md`. The full spec is at
`docs/superpowers/specs/2026-08-14-graph-db-design.md`.

---

## Status

Pre-alpha. Single-writer, no node/edge deletes, no multi-statement
transactions. Toolchain pinned to Rust 1.92.0.

The distribution commands below (Docker, npm, install.sh) are available
after the first `v*` tag is pushed. The source build path is available now.

---

## Pages in this section

- [Quickstart](quickstart.md) — two commands to a running graph explorer
- [Rules](rules.md) — all six predicate kinds with examples
- [API reference](api.md) — HTTP endpoints, MCP tools, Python bindings
