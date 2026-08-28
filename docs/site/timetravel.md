# Time travel: as-of queries

mushroomdb supports read-only views of the database at any past commit via
WAL replay. Open the database at commit N and every query, `explain()`, and
`stats()` reflects the graph as it existed the moment that commit was applied.

## How it works

Every write to mushroomdb appends a frame to the Write-Ahead Log (WAL). A
`Batch` frame is one commit; a single-op frame (InsertNode, SetProp, …) is
also one commit. Commits are numbered 0-based: commit 0 is the state after
the first WAL frame, commit N-1 is the state after the most recent frame.

`GraphDb::open_at(dir, commit)` replays WAL frames 0 through `commit`
(inclusive) into a fresh in-memory graph, then marks the instance read-only.
Every mutation method on the returned instance returns `GraphError::ReadOnly`.

## WAL retention and snapshot interaction

`GraphDb::snapshot()` writes the current state to `snapshot.bin` and then
**truncates the WAL to empty**. After a snapshot:

- `GraphDb::open()` loads from the snapshot plus any post-snapshot WAL frames.
- `GraphDb::open_at()` loads from WAL only (snapshot is ignored). Commit 0
  refers to the first WAL frame written after the snapshot. Pre-snapshot
  history is not available.

If the WAL is empty (i.e., snapshot was just taken and no new writes have
occurred), `open_at` returns `GraphError::CommitOutOfRange { commit, total: 0 }`.

**Snapshot tradeoff:** `snapshot()` truncates the WAL — faster cold starts, but
as-of history restarts from that point. These goals are in direct tension.
`snapshot_with(SnapshotOptions { keep_wal: true })` is now the answer: the V6
snapshot is written, but the WAL is left intact. `open_at` can reach commits
from before the snapshot; cold-start loads the snapshot and then replays the
full WAL idempotently (recovery guards in `apply()` skip already-reflected ops).
Choose based on operational priorities:
- **Need as-of history across all time:** use `snapshot_with(SnapshotOptions { keep_wal: true })` regularly; `open_at` can reach all WAL commits.
- **Need fast startup with a short WAL:** use `snapshot()` (default); WAL is truncated; `open_at` can only reach post-snapshot commits.
- **Both:** use `keep_wal=true` for checkpoints; WAL grows until you explicitly truncate it with a standard `snapshot()`.

The WAL grows without bound until a standard `snapshot()` truncates it. For
large databases this may be gigabytes over long operational periods.

**Torn WAL tail:** if the WAL has a partial frame at the end (e.g., after a
crash mid-write), `open_at` silently treats it as fewer commits — only complete,
valid frames are counted. No error is returned; the valid prefix is replayed.

## Replay cost

`open_at` replays rules incrementally, exactly as live writes do. On a database
with many commits, this can be slow — rule evaluation re-runs for every node
touched by every commit in the replay window. A 100,000-commit database with
complex rules may take minutes to replay. The replay is CPU-bound and does not
involve disk I/O beyond the initial WAL read.

## Rust API

```rust
use core_api::{GraphDb, GraphError};

// Count total WAL commits without opening the db.
let total = core_api::wal_commit_count_at(&dir)?;

// Open a read-only view at commit 3.
let db = GraphDb::open_at(&dir, 3)?;

// Queries and explain work normally.
let rs = db.query("MATCH (n:Person) RETURN n", &Default::default())?;
let explanations = db.explain("alice", "bob")?;

// Mutations return ReadOnly.
let err = db.insert_node("Person", "charlie", vec![]).unwrap_err();
assert!(matches!(err, GraphError::ReadOnly));

// Commit out of range.
let err = GraphDb::open_at(&dir, 999).unwrap_err();
assert!(matches!(err, GraphError::CommitOutOfRange { .. }));
```

## CLI

```
mushroomdb asof <db-dir> --commit N [--query "MATCH ..."]
```

Prints the header `as-of commit N of M` (where M is the total WAL commit
count) followed by query results if `--query` is supplied.

```sh
$ mushroomdb asof ./db --commit 5 --query "MATCH (n:Person) RETURN n"
as-of commit 5 of 42
columns: n
  n=alice
  n=bob
```

## HTTP server

Server-side as-of (`open_at` semantics) over `/query` is not yet implemented.

### History endpoints

Three read-only diagnostic endpoints expose WAL history over HTTP. They scan
the on-disk WAL and return all events within the current horizon window (since
the last WAL-truncating snapshot).

**`GET /node/{key}/history`** — per-node change log.

```json
{
  "key": "alice",
  "history": [
    { "commit": 0, "change": { "type": "NodeInserted", "label": "Person" } },
    { "commit": 1, "change": { "type": "PropSet", "field": "age", "value": 30 } }
  ],
  "total_commits": 2
}
```

**`GET /history/edge?a=&b=`** — edge lifecycle between two nodes. Includes
derived (rule-attributed) edges.

```json
{
  "a": "alice", "b": "bob",
  "events": [
    { "edge_type": "SIMILAR", "commit": 1, "event": "Added", "rule": "sim_emb" }
  ],
  "total_commits": 2
}
```

**`GET /history/was_linked?a=&b=&edge_type=&at_commit=`** — point-in-time edge check.

```json
{ "a": "alice", "b": "bob", "edge_type": "SIMILAR", "at_commit": 1, "linked": true }
```

Returns `400` (not `500`) when `at_commit` is outside the visible horizon:
```json
{ "error": "commit 999 is out of range" }
```

### Horizon contract

All three endpoints include `total_commits` in their response. This is the
exclusive upper bound for valid commit indices (`0..total_commits`). When the
WAL is empty (immediately after a truncating snapshot), `total_commits` is 0
and the history list is empty. Pre-snapshot commits are not visible.

This field is the **honesty contract**: clients can always determine what
portion of history is visible and whether their query covers the full timeline.

### Role-token masking

Role tokens may call all three history endpoints (history is a READ operation).
Masking is applied at the HTTP layer using the same read guard as the history
call:

- `node_history`: if the target key is outside the role's visibility mask, the
  response is 404 — identical to querying an absent key (no existence oracle).
- `edge_history`: BOTH `a` AND `b` must be visible. If either is hidden, 404
  for that key.
- `was_linked`: same two-key visibility requirement as `edge_history`.

Write methods (POST/PUT/DELETE) remain 403 for role tokens.

### MCP tools

The same functionality is available as trusted-local MCP tools (no RBAC):
`node_history`, `edge_history`, and `was_linked`. See the [MCP tools table](api.md#tools).

## Error reference

| Error | Meaning |
|---|---|
| `GraphError::ReadOnly` | Mutation attempted on an as-of instance. |
| `GraphError::CommitOutOfRange { commit, total }` | `commit >= total`; valid range is `0..total`. |
