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
| V7 | zstd(CRC32 + packed CSR topology + packed columnar properties + bincode meta) |
| V8 (current) | mmap-able zero-copy rkyv sections; see wire description below |

The current encoder always writes **V8**. The decoder supports V5, V6, V7, and V8.
V5–V7 stores are **automatically migrated** to V8 on `GraphDb::open` (see Automatic migration below).

#### V8 wire description

```text
[0..4]         magic "GDB1"
[4..6]         VERSION = 8 (u16 LE)
[6..8]         section_count (u16 LE) — currently 5
[8..8+16*N]    section directory: N × { id:u8, _pad:[u8;3], offset:u32, len:u32, crc32:u32 }
[8+16*N..+4]   whole-header CRC32 (covers bytes [0..8+16*N])
[..4096]       zero-pad to complete the 4 KB header page
[4096..]       section payloads at 8-byte-aligned offsets; last section is NOT padded
```

Section ids (fixed):

| ID | Name | Encoding |
|----|------|----------|
| 0  | TOPOLOGY | rkyv `CsrData` (zero-copy archived CSR) |
| 1  | COLUMNS  | rkyv `ColumnsData` (zero-copy archived column store) |
| 2  | IDS      | rkyv `IdMapData` (zero-copy archived id map) |
| 3  | SYMS     | rkyv `InternerData` (zero-copy archived symbol interner) |
| 4  | META     | bincode `V8Meta` (labels, edge_props, rule_defs, provenance, …) |

CRC coverage: each section payload `[offset..offset+len]` is covered by its directory `crc32`.
Alignment padding bytes between sections are written as zeros and are NOT covered by any CRC.
The last section has no trailing pad so the file ends at exactly the last payload byte.

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

> **Production note — ANN index re-fit cost.** Stores with approximate
> (`approximate: true`) rules must re-fit k-means ANN indexes during migration
> because the index structures are rebuilt in-process before the new snapshot is
> written. On a 2.2 GiB V5 dogfood store with 9 rules (measured 2026-08-27,
> Apple M-series, macOS), the first migrating open took **~10–11 minutes** with a
> peak memory footprint of **~54 GB** (max RSS ~9.5 GB; the remainder is
> VM/compressed memory pressure). On a 24 GB machine this left little headroom;
> on a more memory-constrained host the OS may kill the process mid-migration.
> The migration is crash-safe — originals and `.bak` remain intact; simply retry
> or rerun. However, for production stores with large ANN indexes, run
> `mushroomdb migrate <dir>` **offline before starting `serve`** so that the
> serving process never blocks on or gets killed during a migration.

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
| `mushroomdb migrate` (old-version snapshot) | `.bak` written; remains after CLI exits |
| `mushroomdb migrate` (WAL-only store — no snapshot) | no `.bak` written (nothing to back up) |

The `.bak` file is safe to delete manually once you have verified the migrated
store is correct.

---

## Rule wire-shape compatibility

`RuleDef` is serialized with bincode inside WAL `CreateRule` records and
snapshot `rule_defs` sections. Two wire shapes are recognized:

| Wire shape | When written | Fields |
|-----------|-------------|--------|
| **Current** | v0.2+ (post-phase-4) | all fields including `via_label`, `via_edge`, `via_dir` |
| **Pre-0.1.2 legacy** | releases ≤ 0.1.2 | all fields **except** `via_label`, `via_edge`, `via_dir` |

When a pre-0.1.2 rule record is decoded, the missing `via_*` fields default to
`None` (no via-hop behaviour). The two shapes are unambiguously distinguished by
exact-consumption checks: a legacy record decoded as current-shape hits EOF on
the missing `via_*` bytes; a current record decoded as legacy-shape has trailing
bytes that the exact-consumption check rejects.

---

## Honesty

Performance claims in README and docs cite measured numbers with date and
methodology links. Format-stability claims in this document are backed by the
golden-fixture pin tests in `crates/core-api/tests/snapshot.rs` and
`crates/core-api/tests/migrate.rs`: if a code change silently alters the
on-disk byte layout the pin tests fail rather than silently corrupting existing
databases.
