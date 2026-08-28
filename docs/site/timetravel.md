# Temporal history: what was connected when, and why

mushroomdb records every write — including when derived edges were created or
retracted by rules — and lets you query that history at any granularity: per
node, per edge pair, or as a point-in-time read-only snapshot of the whole
graph. This is the temporal story: history APIs with rule attribution, as-of
time travel, compare-and-set writes, and WAL archives.

---

## History APIs

Three read surfaces expose the recorded timeline. All three scan the on-disk
WAL and include a `total_commits` horizon field so callers can always determine
which portion of history is visible.

### node_history — per-node change log

Returns every WAL-visible change for a node since the last WAL-truncating
snapshot.

**MCP:** `node_history(key)` → `{key, history, total_commits}`

**HTTP:** `GET /node/{key}/history`

```json
{
  "key": "alice",
  "history": [
    { "commit": 0, "change": { "type": "NodeInserted", "label": "Person" } },
    { "commit": 1, "change": { "type": "PropSet", "field": "age", "value": 30 } },
    { "commit": 2, "change": { "type": "EdgeAdded", "edge_type": "KNOWS", "other": "bob", "outgoing": true } }
  ],
  "total_commits": 3
}
```

Change types: `NodeInserted`, `PropSet`, `PropRemoved`, `EdgeAdded`,
`EdgeRemoved`, `NodeDeleted`.

### edge_history — add/retract lifecycle with rule attribution

Returns the full add/retract history for all edges between two nodes. Includes
**derived edges** — those created by rules — with the rule name attributed via
`rule`. This is the Zep-class differentiator: not just *what* was connected,
but *why* (which rule fired) and *when* (at which commit).

**How rule attribution works:** When the rule engine fires or retracts a
derived edge, a `DerivedEdgeAdded` (WAL discriminant 18) or
`DerivedEdgeRetracted` (discriminant 19) HISTORY-MARKER record is appended in
the same commit frame as the triggering mutation. These markers are pure
history — they are state no-ops on replay (derived edges are re-derived from
rules deterministically). History scans read them as ground truth of when the
edge was created or retracted and by which rule.

**MCP:** `edge_history(a, b)` → `{a, b, events, total_commits}`

**HTTP:** `GET /history/edge?a=&b=`

```json
{
  "a": "alice",
  "b": "bob",
  "events": [
    { "edge_type": "KNOWS",   "commit": 2, "event": "Added",     "rule": null },
    { "edge_type": "SIMILAR", "commit": 3, "event": "Added",     "rule": "sim_emb" },
    { "edge_type": "SIMILAR", "commit": 7, "event": "Retracted", "rule": "sim_emb" }
  ],
  "total_commits": 10
}
```

`event` is `"Added"` or `"Retracted"`. `rule` is the rule name for derived
edges, `null` for manually written edges.

### was_linked — point-in-time edge check

Answers whether an edge of a given type was active between two nodes at a
specific WAL commit.

> **Index spaces:** `at_commit` is a **0-based frame index** (the same space
> as `edge_history` event commits and `total_commits`). `last_changed()`
> returns a **1-based commit sequence** from the CAS machinery — a different
> counter. Do not pass `last_changed(key)` directly as `at_commit`; to probe
> "at the moment of the last change", use the commit values reported by
> `edge_history`/`node_history` events instead.

**MCP:** `was_linked(a, b, edge_type, at_commit)` → `{linked}` or error when
outside horizon

**HTTP:** `GET /history/was_linked?a=&b=&edge_type=&at_commit=`

```json
{ "a": "alice", "b": "bob", "edge_type": "SIMILAR", "at_commit": 4, "linked": true }
```

Returns 400 (not 500) when `at_commit` is outside the visible horizon:

```json
{ "error": "commit 999 is out of range" }
```

### Horizon contract

All three history endpoints include `total_commits` in their response. This is
the exclusive upper bound for valid commit indices (`0..total_commits`). When
the WAL is empty (immediately after a WAL-truncating snapshot and before any
new writes), `total_commits` is 0 and the history list is empty. Commits before
the last truncating snapshot are not visible.

This field is the **honesty contract**: clients can always determine what
portion of history is visible and whether their query covers the full timeline.

### Role-token masking

Role tokens may call all three history endpoints. Masking follows the
same-as-absent rule:

- `node_history`: if the target key is outside the role's visibility mask, the
  response is 404 — identical to querying an absent key (no existence oracle).
- `edge_history`: BOTH `a` AND `b` must be visible. If either is hidden, 404
  for that key.
- `was_linked`: same two-key visibility requirement as `edge_history`.

Write endpoints (POST/PUT/DELETE) remain 403 for role tokens.

---

## Compare-and-set writes

mushroomdb supports optimistic concurrency via compare-and-set (CAS)
preconditions on write batches. A CAS batch atomically checks that all
preconditions hold before applying any operation; if any precondition fails,
the entire batch is rejected and no WAL frame is written.

### last_changed and commit_seq

`db.last_changed(key)` returns `Some(commit_seq)` — the sequence number of the
commit that last touched the node — or `None` if the node does not exist or has
been deleted. The commit_seq is a monotonically increasing counter (1-based;
every WAL frame advances it).

**Touch definition:** a node's `last_changed` is updated when:
- `InsertNode` — the newly-inserted node
- `SetProp` / `RemoveProp` — the property-bearing node
- `InsertEdge` / `DeleteEdge` — **both** src and dst endpoints (an edge change
  touches both sides)
- `DeleteNode` — node is tombstoned; `last_changed` then returns `None`

History markers (`DerivedEdgeAdded` / `DerivedEdgeRetracted`) are state no-ops
and do **not** update `last_changed`.

### Precondition types

```rust
use core_api::Precondition;

// Fails if the node was modified since `expected` (or does not exist).
Precondition::NodeUnchangedSince { key: "alice".into(), expected: seq }

// Fails if the node exists (for insert-only semantics).
Precondition::NodeAbsent { key: "alice".into() }
```

### write_batch_cas (Rust API)

```rust
let seq = db.last_changed("alice").unwrap_or(0);

let result = db.write_batch_cas(
    vec![Precondition::NodeUnchangedSince { key: "alice".into(), expected: seq }],
    |b| {
        b.set_prop("alice", "role", Value::Str("admin".into()));
    },
)?;
// On success: Ok((nodes_written, edges_written))
// On precondition failure: Err(GraphError::CasConflict { key, expected, actual })
```

All preconditions are checked atomically under the write lock before any
operation is applied. If any precondition fails, the entire batch is rejected:
no WAL bytes written, no in-memory state changes.

### submit_batch_cas (SharedDb / async API)

```rust
shared_db.submit_batch_cas(preconditions, ops).await?;
```

The same atomicity guarantee holds: preconditions are evaluated under the same
write guard as apply, so there is no TOCTOU window.

### CasConflict error

```rust
GraphError::CasConflict { key: String, expected: u64, actual: u64 }
```

`actual` is the current `last_changed` value at the time of the check.
`NodeAbsent` failures use `expected=u64::MAX, actual=last_changed.unwrap_or(0)`.

### Persistence

`last_changed` values are stored in V8 snapshot section 11 (LAST_CHANGE,
`HashMap<node_id, commit_seq>`) so they survive snapshots. CAS preconditions
remain trustworthy across restarts.

---

## As-of time travel

`GraphDb::open_at(dir, commit)` replays WAL frames 0 through `commit`
(inclusive) into a fresh in-memory graph, then marks the instance read-only.
Every mutation method on the returned instance returns `GraphError::ReadOnly`.

### WAL retention and snapshot interaction

Every write appends a frame to the WAL. Commits are numbered 0-based: commit 0
is the state after the first WAL frame.

`GraphDb::snapshot()` writes the current state to `snapshot.bin` and
**truncates the WAL to empty**. After a snapshot:

- `GraphDb::open()` loads from the snapshot plus any post-snapshot WAL frames.
- `GraphDb::open_at()` loads from WAL only (snapshot is ignored). Commit 0
  refers to the first WAL frame after the snapshot. Pre-snapshot history is
  not available.

`snapshot_with(SnapshotOptions { keep_wal: true })` writes the snapshot but
leaves the WAL intact, so `open_at` can reach all WAL commits including those
before the snapshot.

Choose based on operational priorities:

| Goal | Method |
|---|---|
| Preserve full as-of history | `snapshot_with(keep_wal: true)`; WAL grows until an explicit truncating `snapshot()` |
| Fast startup, short WAL | `snapshot()` (default); as-of history restarts from that point |
| Both: rolling checkpoints + eventual truncation | `keep_wal=true` for routine checkpoints; standard `snapshot()` when you decide to prune |

**Torn WAL tail:** if the WAL has a partial frame (e.g., after a crash
mid-write), `open_at` silently treats it as fewer commits — only complete,
valid frames are counted. No error is returned; the valid prefix is replayed.

### Rust API

```rust
use core_api::{GraphDb, GraphError};

// Count total WAL commits without opening the db.
let total = core_api::wal_commit_count_at(&dir)?;

// Open a read-only view at commit 3.
let db = GraphDb::open_at(&dir, 3)?;

// Queries and explain work normally.
let rs = db.query("MATCH (n:Person) RETURN n", &Default::default())?;

// Mutations return ReadOnly.
let err = db.insert_node("Person", "charlie", vec![]).unwrap_err();
assert!(matches!(err, GraphError::ReadOnly));

// Commit out of range.
let err = GraphDb::open_at(&dir, 999).unwrap_err();
assert!(matches!(err, GraphError::CommitOutOfRange { .. }));
```

### CLI

```sh
mushroomdb asof ./db --commit 5 --query "MATCH (n:Person)-[r:FIT]->(p:Project) RETURN n, p, r.score"
# as-of commit 5 of 42
# columns: n, p, score
#   n=alice  p=proj-01  score=0.87
```

### Replay cost

`open_at` replays rules incrementally, exactly as live writes do. On a database
with many commits this can be slow — rule evaluation re-runs for every node
touched by every commit in the replay window. The replay is CPU-bound and does
not involve disk I/O beyond the initial WAL read.

---

## WAL archives and retention

Archives let you retain long-horizon history across truncating snapshots by
keeping old WAL files as numbered sidecars instead of deleting them.

### Enabling archives

```rust
use core_api::{SnapshotOptions, GraphDb};

db.snapshot_with(SnapshotOptions {
    keep_wal: false,   // truncating snapshot (default)
    archive_wal: true, // rename the live WAL to wal.<N>.archive before truncating
})?;
```

When `archive_wal: true`, the live `wal.bin` is renamed to
`wal.<end_frame_index>.archive` before the snapshot is written. Subsequent
writes go to a new `wal.bin`. The archive files live alongside `snapshot.bin`
in the database directory.

### Sidecar files

| File | Written when | Purpose |
|---|---|---|
| `wal.<N>.archive` | each `archive_wal` snapshot | renamed WAL segment; N = cumulative end-frame index |
| `wal.floor` | first retention prune | 8-byte LE u64; archives at or below this floor are deleted |
| `wal.genesis` | first archive, also the store's first-ever snapshot | empty marker; signals that the archive chain covers genesis |

The genesis marker is written only when the first archive snapshot is also the
store's very first snapshot.  A `keep_wal=false` snapshot always writes
`snapshot.bin` before truncating the WAL, so any later archiving session in a
new session sees `snapshot.bin` present and conservatively refuses the marker.
Legacy stores (any prior `snapshot.bin`) are treated identically.

### Archive reachability — stated plainly

Archives extend **two different guarantees** and they differ:

**History scans always reach archives.** `node_history`, `edge_history`, and
`was_linked` scan WAL records directly (record-scanning is prefix-independent).
Archives are included in all history scans regardless of whether the genesis
chain is intact. If you need deep per-node or per-edge history, archives are
always useful.

**As-of time travel (`open_at`) requires an unpruned genesis chain.** `open_at`
replays the WAL from scratch to reconstruct graph state at a point in time. It
can only replay correctly if it has every frame from the very beginning. The
genesis chain is intact when ALL of these are true:
- `wal.genesis` marker is present (the first archive was also the store's
  first-ever snapshot — no prior `snapshot.bin` existed, which rules out both
  prior truncating snapshots and legacy stores)
- `wal.floor` is 0 or absent (no archives have been pruned)

When the genesis chain is NOT intact (pruned archives, or archiving started
after a prior truncating snapshot), `open_at` for archive-resident commits
returns `GraphError::CommitOutOfRange` — a clean refusal, never silently wrong
data. Live-WAL commits (commits in the current `wal.bin`) are always reachable.

Summary:

| Operation | Live WAL commits | Archive commits (intact genesis chain) | Archive commits (pruned/incomplete) |
|---|---|---|---|
| `node_history` / `edge_history` / `was_linked` | Always reachable | Always reachable | Always reachable |
| `open_at` (as-of) | Always reachable | Reachable | `CommitOutOfRange` (safe refusal) |

### Retention and pruning

Pruning removes archives whose end-frame index falls at or below a retention
floor. This is controlled by `wal.floor`: when the floor is written, all
archive files at or below that floor are deleted.

The floor is written **before** archive deletion (crash-safe ordering): if a
crash occurs mid-prune, orphaned archive files above the new floor are cleaned
up on the next `GraphDb::open`. No archive is ever deleted without the floor
being persisted first.

**Effect on as-of:** pruning archives breaks the genesis chain. After any
prune, `open_at` for pruned-archive commits returns `CommitOutOfRange`. History
scans are unaffected.

---

## Error reference

| Error | Meaning |
|---|---|
| `GraphError::ReadOnly` | Mutation attempted on an as-of instance. |
| `GraphError::CommitOutOfRange { commit, total }` | `commit >= total`; valid range is `0..total`. Also returned for archive-resident commits when the genesis chain is pruned or incomplete. |
| `GraphError::CasConflict { key, expected, actual }` | A `Precondition` failed; the batch was not applied. |
