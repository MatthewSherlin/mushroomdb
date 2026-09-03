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
[6..8]         section_count (u16 LE) — currently 12
[8..8+16*N]    section directory: N × { id:u8, _pad:[u8;3], offset:u32, len:u32, crc32:u32 }
[8+16*N..+4]   whole-header CRC32 (covers bytes [0..8+16*N])
[..4096]       zero-pad to complete the 4 KB header page
[4096..]       section payloads at 8-byte-aligned offsets; last section is NOT padded
```

Section ids (fixed):

| ID | Name | Encoding |
|----|------|----------|
| 0  | TOPOLOGY   | rkyv `CsrData` (zero-copy archived CSR) |
| 1  | COLUMNS    | rkyv `ColumnsData` (zero-copy archived column store) |
| 2  | IDS        | rkyv `IdMapData` (zero-copy archived id map) |
| 3  | SYMS       | rkyv `InternerData` (zero-copy archived symbol interner) |
| 4  | META       | bincode `V8Meta` (labels, edge_props, rule_defs, provenance, …) |
| 5  | EDGE_PROPS | rkyv `EdgePropsData` (per-edge property blobs, sorted by (etype,src,dst)) |
| 6  | HNSW       | rkyv `HnswSectionData` (HNSW graph blobs per rule name) |
| 7  | PROVENANCE | rkyv `ProvenanceSectionData` (rule-derived edge provenance) |
| 8  | RULES_META | rkyv `RulesMetaData` (rule definitions, trip flags, fire counts) |
| 9  | VIEWS      | rkyv `ViewsSectionData` (view definition bincode blobs) |
| 10 | IVF_STATE  | bincode `BTreeMap<String, PerRuleIvfState>` (IVF centroid + cluster state per approximate rule) |
| 11 | LAST_CHANGE | bincode `HashMap<u32, u64>` (per-node-id → last-commit-seq; used for CAS precondition checks) |

CRC coverage: each section payload `[offset..offset+len]` is covered by its directory `crc32`.
Alignment padding bytes between sections are written as zeros and are NOT covered by any CRC.
The last section has no trailing pad so the file ends at exactly the last payload byte.

#### Section-CRC hot-path deferral and trust model (v0.2.0+)

Small sections (IDS=2, SYMS=3, META=4, RULES_META=8, VIEWS=9) validate their CRC32
on first access.

Large sections (TOPOLOGY=0, COLUMNS=1, EDGE_PROPS=5, HNSW=6, PROVENANCE=7,
IVF_STATE=10) **skip** the per-touch CRC on the normal query path. A full-section
hash of hundreds of MiB costs 50–200 ms per section. Section bounds are validated
at open time; rkyv archived data on the hot path uses `access_unchecked` (O(1)
root-pointer lookup, no pointer-walk). This is sound for encoder-produced
uncorrupted data, but a bit-flip on a relative-pointer field causes
`ArchivedVec::as_slice` to resolve an out-of-bounds address before any length
check — genuine UB, not a panic. Within-payload corruption that does not affect
relative pointers yields wrong query results rather than a safety violation.
Mitigated by `mushroomdb verify` (full-section CRC32 audit on demand) and
planned Miri/ASAN CI coverage. The `snapshot::decode` path (used during
migration and offline decode) retains validated `rkyv::access` for full
hostile-byte safety.

To audit large-section integrity explicitly:

```
mushroomdb verify <db-dir>
```

Reads every section, computes CRC32, and reports any mismatch. Exits 2 on the
first corrupt section, 0 if all sections are intact. Measured at 0.26 s on
a 1.8 GiB snapshot (12 sections). Run this after any external modification of
the snapshot file, or periodically as a sanity check on storage hardware.

### WAL (`wal.bin`)

WAL record discriminants 0–20 are append-only: once assigned, a discriminant
is never reused for a different record shape. New record types receive the next
available discriminant.

Which `Intern` records a `Batch` frame carries, and where they sit inside it,
is **not** part of the format contract — only that replaying a frame's records
in order reproduces the write-time symbol assignment. Since v0.4.5 a `Batch`
containing `CreateRule` pre-interns the rule's `edge_type` immediately before
that record, because rule backfill would otherwise intern it lazily on replay
and take an id a later `Intern` record already claimed.

### WAL archive sidecar files

Stores that use `snapshot_with(archive_wal: true)` write additional sidecar
files alongside `snapshot.bin` and `wal.bin`:

| File | Written when | Purpose |
|------|-------------|---------|
| `wal.<N>.archive` | each `archive_wal` snapshot | renamed WAL file; N = cumulative end-frame index |
| `wal.floor` | first retention prune | 8-byte LE u64 horizon floor |
| `wal.genesis` | first archive, also the store's first-ever snapshot | empty marker: archive chain covers genesis |

The genesis marker is written only when the first archive snapshot is also the
store's very first snapshot (no `snapshot.bin` existed before it).  This rule
is conservative and covers both cross-session truncation and legacy stores:
a `keep_wal=false` snapshot always writes `snapshot.bin` before truncating the
WAL, so any later archiving session sees `snapshot.bin` present and refuses the
marker.  Stores that predate archive support and have a prior `snapshot.bin` are
treated identically: `open_at` returns `CommitOutOfRange` for archive-resident
commits (safe refusal, never silent wrong data).

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
> peak memory footprint of **~35 GB** (max RSS **~8.3 GB** as of 2026-08-28 — fits a 16 GB machine without heavy swap; the VM-footprint remainder is
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

## Point-in-time recovery (PITR)

mushroomdb supports point-in-time recovery by combining the `backup` command
with a WAL archive chain.  This section explains how to set up and use the
full recovery workflow.

### How PITR works

Every WAL-archiving snapshot (`snapshot --archive-wal`) renames the current
`wal.bin` to `wal.<commit>.archive` and writes a fresh snapshot.  A continuous
sequence of archives, together with `wal.genesis` (marks an unbroken chain from
the store's first commit) and `wal.floor` (the lowest reachable commit index),
lets `open_at` replay any commit in the archive chain — not just commits in the
current `wal.bin`.

### 1 — Take a consistent backup

**If the store is idle** (no concurrent writer process):

```sh
# Archive the WAL before backup so the archive lands in the backup too.
mushroomdb snapshot <db-dir> --archive-wal

# Copy the full directory (snapshot + archives + genesis marker).
mushroomdb backup <db-dir> <backup-dir>
```

**If the store is live-served** (a `mushroomdb serve` process is running), use
the HTTP endpoint — the server holds the read lock during the copy, which is
the correct cross-process synchronisation point:

```sh
curl -X POST http://localhost:8080/backup \
  -H "Authorization: Bearer <full-token>" \
  -H "Content-Type: application/json" \
  -d '{"dest": "/path/to/backup-dir"}'
```

> **WARNING:** Running `mushroomdb backup` against a directory that a separate
> `mushroomdb serve` process is writing to is **unsafe**.  The file copies are
> not atomic across processes and can produce a torn backup that only fails at
> restore time.  The `verified: true` result reduces but does not eliminate
> this risk.

`backup` copies `snapshot.bin`, `wal.bin`, all `wal.<N>.archive` files,
`wal.floor`, `wal.genesis`, and `roles.json` into the destination directory.
It then opens the backup read-only and runs the CRC verifier; the reported
`verified: true` confirms byte-for-byte integrity before the command returns.

### 2 — Keep WAL archives (retention policy)

```sh
# Unlimited retention (default) — keep every archive ever taken.
mushroomdb snapshot <db-dir> --archive-wal

# Bounded retention — keep only the newest 7 archives; older ones are pruned.
mushroomdb snapshot <db-dir> --archive-wal --retention 7
```

Pruning updates `wal.floor` so `open_at` can refuse commits that are no longer
reachable.  The `wal.genesis` marker is removed whenever the chain is broken
(e.g. when a WAL-truncating snapshot is taken after archives exist, or when
the floor advances past zero).

### 3 — Recover to a specific commit

```sh
# List how many commits exist in the WAL.
mushroomdb asof <backup-dir> --commit 0   # shows total commit count

# Open the backup at commit 42 and run a read query against that state.
mushroomdb asof <backup-dir> --commit 42 --query "MATCH (n) RETURN n LIMIT 5"
```

`open_at` rehydrates the graph up to (and including) commit `N` by replaying:
1. `snapshot.bin` (the base), then
2. the ordered archive chain (`wal.<N>.archive` files, oldest first), then
3. the current `wal.bin` tail.

This is only permitted when `wal.genesis` is present and `wal.floor == 0`
(full unbroken chain from genesis), or when the requested commit lies within
the current `wal.bin` range.

### Recovery contract

| Scenario | PITR capability |
|----------|-----------------|
| No archives, no genesis | Recover only within current `wal.bin` |
| Archives + genesis + floor = 0 | Recover any commit from genesis to HEAD |
| Archives pruned (floor > 0) | Recover commits from `wal.floor` to HEAD |
| WAL-truncating snapshot after archives | Genesis marker removed; chain broken |

---

## Honesty

Performance claims in README and docs cite measured numbers with date and
methodology links. Format-stability claims in this document are backed by the
golden-fixture pin tests in `crates/core-api/tests/snapshot.rs` and
`crates/core-api/tests/migrate.rs`: if a code change silently alters the
on-disk byte layout the pin tests fail rather than silently corrupting existing
databases.

---

## Panic policy

Corrupt, truncated, or adversarial on-disk bytes always produce a typed
`GraphError::Corrupt` — never a panic. The complete policy, including which
`.expect()` calls are retained as post-validation invariants and why, is in
[docs/site/panic-policy.md](site/panic-policy.md).
