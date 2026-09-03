# Durability and crash recovery

mushroomdb persists with a write-ahead log (WAL) plus periodic V8 snapshots.
Every committed write is appended to the WAL and fsynced per the configured
policy; a snapshot is a self-contained, memory-mappable image of the whole
store with per-section CRCs.

## How recovery works

On open, the store is reconstructed as **snapshot (if present) + WAL tail**:

- **With a snapshot** — the V8 image is memory-mapped (open-to-first-query in
  milliseconds even at 100k nodes; derived edges, HNSW/IVF vector indexes, and
  provenance are all restored from the image), then any WAL frames written after
  the snapshot are replayed. This is the normal path and it is fast.
- **WAL-only, from genesis** — if the store has *never* been snapshotted, the
  entire history replays from the beginning. Node/edge/property writes replay
  cheaply, but **rules re-derive their edges**, and for `VectorSimilar` rules
  that means rebuilding the ANN (HNSW/IVF) index from scratch. On a large,
  vector-rule store this can take minutes. This is the worst case, and it only
  happens when no snapshot exists.

## Keeping recovery fast

The tooling already bounds WAL-only-from-genesis exposure:

- The server **snapshots on graceful shutdown** (SIGINT/SIGTERM), so a clean
  restart always recovers from an image.
- `mushroomdb serve … --snapshot-every <secs>` snapshots periodically, so even
  after a hard crash the replay is only the WAL tail since the last snapshot.
- `mushroomdb snapshot <db>` takes one on demand (e.g. before an upgrade).

**Guidance:** for a long-running vector-rule deployment, set `--snapshot-every`
(or snapshot on a schedule). The cost of a periodic snapshot is far smaller than
a from-genesis rebuild, and it caps how much WAL a crash can leave to replay.

## Recovery vs. refresh

Two different things read the WAL tail, and they are easy to confuse.

**Recovery** happens at open. The snapshot is loaded and the whole WAL is
replayed on top of it. A torn trailing frame — the signature of a crash mid-
append — is dropped, and with `repair_wal` on (the default) the valid prefix is
written back over it. That truncation is correct crash recovery: the frame was
never fsynced, so no caller was ever told it committed.

**Refresh** happens while the store is open, and it is not recovery. A handle
tracks how much of the WAL it has applied and, on `refresh()`, decodes only what
another process has appended since. An incomplete trailing frame here means a
peer is still writing, not that anything crashed, so it is left alone and picked
up next time. Nothing is written to disk.

The distinction matters for unattended readers: a reader that treated a live
writer's half-written frame as a torn tail, and repaired it, would destroy a
frame that writer is about to make durable. That is why `repair_wal: false` and
`read_only: true` exist, and why the hooks use them. See
[Concurrency](concurrency.md).

## The `LOCK` file

A store directory holds one extra file for cross-process coordination: `LOCK`,
always empty. It exists only to carry an advisory OS lock that writers hold
while they commit. It is created on the first write, never removed, and carries
no data — deleting it while no process has the store open is harmless, and
deleting it while one does defeats the coordination for handles opened
afterwards.

It is not part of the on-disk format: snapshots and WAL frames are unchanged by
its presence, and a store copied without it works normally.

## Integrity

`mushroomdb verify <db>` validates a snapshot end to end: per-section CRC32
**and** a structural (rkyv `bytecheck`) pass over the sections the hot read path
otherwise reads unchecked. Run it before restoring a snapshot from an untrusted
source — CRC32 alone cannot detect a maliciously crafted image (an attacker can
recompute the checksum), but the structural pass rejects out-of-bounds pointers.

## Format compatibility matrix

Every historical snapshot format is permanently readable.  A format version is
stamped in the 6-byte header (magic `GDB1` + 2-byte little-endian version).

| Version | Introduced in | Status | Notes |
|---------|--------------|--------|-------|
| V5 | 0.1.0 | readable | uncompressed bincode + CRC32 header; **no encoder** — only the golden fixture pin covers this format |
| V6 | 0.1.1 | readable | zstd-compressed V5 payload in a container |
| V7 | pre-0.2.0 (interim) | readable | zstd(CRC + packed CSR + packed columns + bincode meta) |
| V8 | 0.2.0+ | default | 4 KB header page + rkyv sections; mmap-able, zero-copy open |

**Patch-stability promise:** a snapshot written by any 0.4.x release opens
correctly on any other 0.4.x binary without migration.  Format version is only
bumped on minor or major releases.

**What the golden pins prove:** `crates/core-api/tests/fixtures/` contains a
committed binary fixture for each version (golden_v5.bin … golden_v8.bin).  The
`golden_v{5..8}_pin` tests in the `snapshot` integration-test suite load each
fixture into a real `GraphDb::open()` call and assert node count, edge count, and
specific property values.  A change to any decoder that silently corrupts data
will fail its pin, not just the new-data tests.

**V5 limitation:** the V5 encoder was removed when V6 shipped.  The V5 golden
pin contains 2 nodes and 1 edge (the minimal shape that exercises the decoder);
richer content coverage is not possible without a V5 encoder.

**format-compat CI job:** the `format-compat` job runs
`cargo test -p mushroomdb --test snapshot --test migrate`, which exercises every
version pin and every migration path on every CI run.  A failure in that job
names the format-compat category explicitly rather than surfacing as a generic
test failure.

## Roadmap

A future optimization can make rule-derived edges first-class replayable WAL
records (and persist incremental ANN state) so that even WAL-only-from-genesis
recovery skips re-derivation. It is deferred because the snapshot path already
delivers fast recovery for the common case, and the change touches the
crash-recovery correctness core.
