# Architecture

```text
graph-db/
├── crates/
│   ├── core-storage      # Packed adjacency topology + columnar property store + WAL + snapshots
│   ├── core-rules        # linking rules, per-rule indexes, incremental maintenance
│   ├── core-query        # pull-based interpreter; traversal ops + Cypher subset
│   ├── core-api          # the one public Rust interface; typed error enums
│   ├── code-extract      # tree-sitter symbol/import/call extraction; bytes in, facts out
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
V9 rkyv snapshots (14 sections: CSR topology, columnar properties, one shared string table for every
string column, HNSW blobs, provenance, IVF state, per-node last-change index, and more — zero-copy,
no heap allocation on open). V5–V8 stores are auto-migrated to V9 on `GraphDb::open`, keeping the
original beside the new one as `snapshot.bin.bak` until the next clean open. The upgrade is one-way:
an earlier binary refuses a V9 snapshot with `snapshot: unsupported version 9` rather than reading it
wrongly. Derived edges are not WAL-logged; they are restored directly from the mmap'd sections. See
[`docs/format-stability.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/format-stability.md) for the format evolution contract.

## Roadmap

What is not built yet:

| Priority | Item |
|---|---|
| Medium | A measured run at 1,000,000 nodes; persisting the full-text index in the snapshot, so opening a store does not rebuild it |
| Medium | v1.0 format stability (snapshot + WAL semver guarantee) |
| Low | `CASE` in a write-statement `RETURN`; subqueries; napi-rs; WASM |
| Low | Multi-statement `BEGIN/COMMIT` interactive transactions |
