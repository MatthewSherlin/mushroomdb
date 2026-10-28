# Upgrade from 0.9

0.10 opens a 0.9 store. A 0.9 binary opens a 0.10 store that has never used a
V11 feature. The breaks below are the ones that change a call, a match on an
enum, or a value on disk.

## Python

These six methods moved under `db.memory`. The old names still run and emit
`DeprecationWarning` on the caller's line. They are removed in 0.11.

| 0.9 | 0.10 |
|---|---|
| `db.remember(...)` | `db.memory.remember(...)` |
| `db.recall(...)` | `db.memory.recall(...)` |
| `db.upsert_entity(...)` | `db.memory.upsert_entity(...)` |
| `db.forget(...)` | `db.memory.forget(...)` |
| `db.schema_report(...)` | `db.memory.schema_report(...)` |
| `db.identity_clusters(...)` | `db.memory.identity_clusters(...)` |

Arguments and return values are unchanged. A `scoped()` handle's `memory` is
scoped the same way.

## Rust

`Value` has five new variants, so an exhaustive `match` needs new arms:
`Date(i32)`, `LocalDateTime(i64)`, `DateTime { micros_utc, offset_seconds }`,
`Duration { months, days, nanos }`, `Binary(Vec<u8>)`.

`GraphError` has four new variants: `ConstraintViolation`,
`ConstraintCreationConflict`, `ValueTooLarge` and
`Unwritable { path, detail, read_only_filesystem }`.

`json_to_value` returns `Result<Option<Value>, String>`.

## JSON values that used to be maps

A one-key object under `$date`, `$ldt`, `$dt`, `$dur` or `$binary` is now a
typed value. Through 0.9 it was stored as a map. A payload that is not the
tag's text is an error naming the field. A bare string is still a string. An
object with any other key, or with more than one key, is still a map.

A store that already holds the old map exports it as a map. Importing that
export stores the typed value, or refuses it when the text does not parse.

Role predicates (`visible_where`) refuse the new types. `roles.json` has no
format version, so a 0.9 reader would report one as corrupt.

## V11

The first write that stores a `Date`, `LocalDateTime`, `DateTime`, `Duration`
or `Binary`, or that creates a unique constraint or a range index, or that
writes a relationship property, first writes a V11 snapshot that keeps the WAL.
On a store of about 1M nodes that snapshot is the 5.6 s one-time cost named in
the 0.10 spec. The store stays V11 after that, including after the last
constraint or range index is dropped.

0.9.x cannot open a V11 store. It stops with `snapshot: unsupported version 11`
and leaves the WAL untouched. There is no downgrade path.

A store that never does any of those writes keeps the format 0.9.x reads.

## Cypher

Queries that 0.9 ran still run. New syntax is additive. These parse and fail
with a precise message rather than a generic one:

- `time()` and `localtime()`: `not supported in mushroomdb 0.10: time types`
- `FOREACH`, a `CALL { }` subquery, and label removal (`REMOVE n:Label`)
- a node pattern with more than one label

`CALL db.labels()`, `CALL db.relationshipTypes()`, `CALL db.propertyKeys()`,
`CALL db.indexes()`, `CALL db.constraints()`, `CALL dbms.components()` and
`CALL db.schema.visualization()` are the procedures that run. Any other name
is `unknown procedure`.

## CLI, keywords, HTTP

Help lists graph commands before import and agent-memory commands. Subcommand
names and flags are the same, including `query --file` and `query --stdin`.

The `mushroomdb` library crate's crates.io keywords no longer include
`agent-memory` or `mcp`. Nothing in code reads those keywords.

`POST /algo/pagerank`, `/algo/wcc` and `/algo/degree` require a JSON object.
An array, which 0.7.2 and 0.9 accepted positionally, is a 400. Unknown fields
stay a 400.

`serve` on a directory it cannot write returns `Unwritable` instead of saying
the store is busy.

## What does not break

Rust callers of the graph API, other than the enum matches above, keep their
signatures. The TypeScript client adds types; it does not rename methods. Bolt,
transactions, backup and the new Cypher clauses are additions. A single-node
`mushroomdb` process is still one file in a directory and still needs no server.
