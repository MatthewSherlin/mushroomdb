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

## Integrity

`mushroomdb verify <db>` validates a snapshot end to end: per-section CRC32
**and** a structural (rkyv `bytecheck`) pass over the sections the hot read path
otherwise reads unchecked. Run it before restoring a snapshot from an untrusted
source — CRC32 alone cannot detect a maliciously crafted image (an attacker can
recompute the checksum), but the structural pass rejects out-of-bounds pointers.

## Roadmap

A future optimization can make rule-derived edges first-class replayable WAL
records (and persist incremental ANN state) so that even WAL-only-from-genesis
recovery skips re-derivation. It is deferred because the snapshot path already
delivers fast recovery for the common case, and the change touches the
crash-recovery correctness core.
