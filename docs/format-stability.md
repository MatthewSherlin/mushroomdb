# Format stability promise

**Effective from v0.2.0.**

mushroomdb takes forward compatibility of your on-disk data seriously. This
document is the binding contract for how the snapshot and WAL formats evolve.

---

## Current formats

### Snapshot (`snapshot.bin`)

| Field | Offset | Value |
|-------|--------|-------|
| Magic | 0–3 | `GDB1` |
| Version | 4–5 | little-endian `u16` |
| Payload | 6+ | version-specific |

| Version | Description |
|---------|-------------|
| V5 | Uncompressed bincode payload with CRC32 header |
| V6 | zstd-compressed V5 payload |
| V7 (current) | zstd(CRC32 + packed CSR topology + packed columnar properties + bincode meta) |

The current encoder always writes **V7**. The decoder supports V5, V6, and V7.

### WAL (`wal.bin`)

WAL record discriminants 0–17 are append-only: once assigned, a discriminant
is never reused for a different record shape. New record types receive the next
available discriminant.

---

## Stability guarantees

### Within a minor series (e.g., v0.2.x)

- WAL discriminants are **append-only**: existing discriminants retain their
  shape; new discriminants may be added at the end.
- Snapshot fields within a version are **append-only** when added to the
  bincode-serialized sections (new fields use `#[serde(default)]` so old
  snapshots deserialise without error).

### Breaking changes

A breaking change bumps the snapshot `VERSION` constant and ships a migrator
in the same release. Migration is automatic on open (see below) and available
via the CLI.

The **minimum supported on-disk format** for v0.2.x is **V5**. Stores written
by any release from V5 onward open without manual intervention.

Snapshot versions V3 and V4 are no longer supported and will be rejected with
a clear error message naming the version. Use a v0.1-era binary to
re-snapshot before upgrading.

---

## Automatic migration

`GraphDb::open` (and `GraphDb::open_with_options` with the default
`auto_migrate: true`) migrates old-format snapshots transparently:

1. The existing `snapshot.bin` is copied to `snapshot.bin.bak` (atomic write +
   fsync) **before** any modification.
2. A new snapshot at the current VERSION is written via
   `snapshot_with(keep_wal: true)`, preserving all WAL history.
3. If the migration step fails, the original files are intact (the `.bak` was
   committed before the new snapshot was attempted) and the error is returned —
   log-and-continue is never used.
4. The next clean open at the current VERSION deletes the `.bak`.

To opt out of automatic migration use:
```rust
GraphDb::open_with_options(dir, OpenOptions { auto_migrate: false })
```

---

## `mushroomdb migrate` CLI

```
mushroomdb migrate <db-dir>
```

Performs a **truncating** migration (WAL is truncated after the new snapshot,
unlike the automatic open-path which preserves the WAL). Keeps `.bak`.

Output:
- `migrated V<from> -> V<current>` — migration succeeded.
- `already current (V<current>)` — no migration needed.

---

## `.bak` semantics

| Condition | `.bak` action |
|-----------|---------------|
| Old-format snapshot opened with `auto_migrate: true` | `.bak` written before new snapshot |
| Clean open at current VERSION with `.bak` present | `.bak` deleted |
| `auto_migrate: false` | `.bak` never touched |
| `mushroomdb migrate` | `.bak` written; remains after CLI exits |

The `.bak` file is safe to delete manually once you have verified the migrated
store is correct.

---

## Honesty

Performance claims in README and docs cite measured numbers with date and
methodology links. Format-stability claims in this document are backed by the
golden-fixture pin tests in `crates/core-api/tests/snapshot.rs` and
`crates/core-api/tests/migrate.rs`: if a code change silently alters the
on-disk byte layout the pin tests fail rather than silently corrupting existing
databases.
