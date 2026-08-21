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

**To preserve full as-of history:** do not call `snapshot()`, or ensure
`open_at` calls are bounded to the post-snapshot window. The WAL grows without
bound until a snapshot is taken. For large databases this may be gigabytes over
long operational periods.

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

Server-side as-of is not yet implemented. Each HTTP request to `/query` reads
current state. As-of over HTTP is on the roadmap.

## Error reference

| Error | Meaning |
|---|---|
| `GraphError::ReadOnly` | Mutation attempted on an as-of instance. |
| `GraphError::CommitOutOfRange { commit, total }` | `commit >= total`; valid range is `0..total`. |
