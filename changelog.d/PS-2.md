### Added
- **Python `db.memory.*` namespace.** The agent-memory methods (`remember`, `recall`, `upsert_entity`, `forget`, `schema_report`, `identity_clusters`) now live under `db.memory`, taking the same arguments and returning the same results. A plain `GraphDb` is a general-purpose graph store and imposes none of these behaviours on ordinary nodes. `db.memory` shares the handle's store and scope (a `scoped()` handle's `memory` is scoped the same way) and holds no reference back to the handle, so dropping the handle still releases the store. The type stub gains a `MemoryAccessor` class.

### Deprecated
- **Python top-level memory methods.** `db.remember`, `db.recall`, `db.upsert_entity`, `db.forget`, `db.schema_report` and `db.identity_clusters` still work, unchanged, but emit a `DeprecationWarning` that names the caller's line. Use `db.memory.<name>`.
