# Running several processes against one store

[concurrency.md](concurrency.md) describes the two mechanisms — the advisory
write lock and the WAL frame cursor. This page is the other half: which
*arrangements* of processes those mechanisms support, and what each one does
when it goes wrong.

Two shapes are supported. Two are not. Nothing here is new behaviour; it is
what the code has always done, written down so it does not have to be inferred
from a lock file.

| Shape | Supported | What failure looks like |
|---|---|---|
| One writer, N readers calling `refresh()` | **Yes** | A reader that never refreshes serves stale answers. Consistent, never corrupt. |
| N writers, serialised by `LOCK` | **Yes** | `Busy` after `WRITE_LOCK_WAIT` (2 s). Nothing written, retry safe. |
| A store directory on a network filesystem | **No** | Silent. Two writers can both believe they hold the lock. |
| Two processes writing with the lock bypassed | **No** | Silent, and unreachable: no API, flag or variable turns the lock off. |

---

## Supported: one writer, N readers

The writer opens read-write and holds the store's `LOCK` for as long as its
handle lives. Every reader opens with `read_only=True`, which never takes the
lock, never makes the writer wait, and writes nothing to disk at open — not
even a WAL repair the reader might otherwise think warranted.

Readers pick up the writer's commits by calling `refresh()`. Nothing pushes:
a handle does not poll the store on its own, and no commit reaches a reader
that does not ask.

```python
# writer process
db = mushroomdb.GraphDb.open("./store")
db.upsert_node("Doc", "d-1", {"title": "…"})

# reader process
r = mushroomdb.GraphDb.open("./store", read_only=True)
r.refresh()                 # returns how many commits arrived
r.node_info("d-1")
```

**The failure mode is staleness, and it is the documented cost.** A reader that
never refreshes keeps answering from the last state it loaded. Those answers are
internally consistent — a commit is applied whole or not at all, so no read ever
observes half of one — they are simply from the past. That is not a bug to be
reported; it is what "no polling" buys, and `refresh()` is the entire remedy.

Two related facts are worth having in hand:

- **`refresh()` writes nothing**, so a `read_only=True` handle and a `scoped()`
  child may both call it freely.
- **A commit the writer is midway through appending is left alone.** The
  trailing bytes are not yet a whole frame, so `refresh()` skips them, returns
  the count of complete frames it did apply — possibly zero — and the handle
  stays stale until that frame lands. A partial write is a wait, not an error
  (`crates/core-api/tests/multiprocess.rs`,
  `partial_trailing_frame_is_not_an_error`).

One thing this shape does **not** give you: subscriptions do not fire for
another process's writes. Commits absorbed by `refresh()` replay exactly as
they would at open, and open notifies nobody. The data is there on the next
read; the notification is not.

## Supported: N writers, serialised by the lock

Several processes may write to one store. They do not write at the same time —
the lock serialises them — and the serialisation is visible, not silent.

Where the `Busy` surfaces depends on which handle you are holding, and the
difference matters when you are laying out processes:

- **A plain read-write handle takes the lock at open and holds it for its
  lifetime.** `GraphDb.open("./store")` is this handle, and so is every
  one-shot CLI command. A second one — in this process or any other — polls for
  up to `WRITE_LOCK_WAIT` (2 s, `WRITE_LOCK_WAIT` in `crates/core-api/src/db.rs`)
  and then raises `MushroomBusy`. **The refusal arrives at `open`, not at the
  first write.**
- **`SharedDb`, which the server uses, takes the lock per write instead.** It
  holds its handle open for the life of the process, so holding the lock that
  long would shut every other process out. Between writes the store is free.

```python
from mushroomdb import GraphDb, MushroomBusy

try:
    with GraphDb.open("./store") as db:
        db.ingest_batch(nodes, edges)
except MushroomBusy:
    ...   # another writer has it; nothing changed; try again later
```

**Retrying is always safe.** When the lock is refused, nothing was written to
the WAL and no in-memory state changed — the handle never got far enough to
mutate anything. `Busy` means "another process is writing right now", never
"something is wrong with the store". There is no partial commit to clean up and
no state to reconcile before the retry.

This shape suits occasional writes: a hook, a periodic sync, one task per
document. It does not suit a write-heavy fan-out. Every writer serialises
against every other, so N concurrent writers get one writer's throughput plus
the cost of contending for the lock, and a writer that holds a plain handle
open across a long job holds the lock for that whole job. If your design has
many processes writing continuously, put one writer in front of them rather
than letting them fight for the lock.

Readers are unaffected throughout. Reading takes no lock and never waits for
one, and a writer polls for the cross-process lock *before* it takes any
in-process guard, so a busy peer in another process cannot stall reads in this
one.

## Not supported: a store directory on a network filesystem

Keep the store on a local disk. NFS, SMB, EFS and similar network or overlay
mounts are not supported for a store that more than one process opens, and the
failure is silent in both of the mechanisms this page rests on:

- **The lock stops being exclusive.** `LOCK` is an OS advisory lock on an open
  file description. Its behaviour over a network filesystem depends on the
  protocol version, the mount options and the server's lock daemon, and where
  it degrades it does so by granting — two processes each believe they hold the
  lock and interleave frames into one WAL. Nothing raises. The corruption
  surfaces later, as a WAL that does not decode.
- **Staleness detection stops being prompt.** `refresh()` and `is_stale()` both
  rest on a `stat` after a peer's write and fsync reporting the new length. A
  mount that caches attributes can report the old length for as long as its
  attribute timeout, so a reader will not see a commit that has already landed.

A single-process store on a network mount is a different question — one
process, one handle, no lock contention — but multi-process coordination over a
network filesystem is exactly the folklore this page exists to replace. Do not
rely on it.

## Not supported: two processes writing with the lock bypassed

There is **no supported way to turn the lock off**, and this is worth saying
plainly because the shape gets proposed. No `OpenOptions` field, no Python
argument, no CLI flag and no environment variable disables it. The one
unlocked open in the codebase, `GraphDb::open_unlocked`, is `pub(crate)` and
exists solely so `SharedDb` can take the lock per write instead of per handle —
it is a change of *granularity*, not an opt-out.

What can still happen, because the lock is **advisory**:

- **A program that is not mushroomdb edits the store directory.** The lock
  coordinates cooperating mushroomdb processes. It does not stop an unrelated
  process — a backup tool writing in place, a sync client, a shell redirect —
  from touching `wal.bin` or the snapshot. Nothing detects it at the time.
- **Two paths to the same directory look like two stores.** The lock is held per
  inode, so two processes reaching the same bytes through different mounts or
  through a bind mount may each acquire what they believe is an exclusive lock.
  Address one store by one path.

If you need to copy a store, snapshot it and copy the result, or use
`GraphDb.restore(src, dst)`, which stages the copy inside `dst` and opens it
there before anything is moved into place.

---

## Known limit: a replaced WAL of the same length on non-Unix platforms

Before every write, a handle checks that `wal.bin` is still the file its cursor
describes. On Unix it compares the file's identity (the inode). On Windows and
other non-Unix platforms there is no file identity yet, so the handle falls
back to the WAL's length and the snapshot's identity.

The condition: `wal.bin` is replaced by a different file of exactly the length
this handle last saw, the snapshot is unchanged, and this handle then writes.
No length check can see that. The handle appends frames that were encoded
against the old file onto the new one, and **the store can then fail to open**
with a `Corrupt` error. On Unix the identity check catches it, and the handle
reloads before it writes.

On non-Unix platforms, do not replace a store's `wal.bin` from outside
mushroomdb while other processes have the store open. Copy or restore WAL files
only into a store that no process has open. A platform identity check (the
Windows file index) is tracked as defect-ledger row 122.

## Worked example: the disposable sidecar

The shape this page was written for. A store that is **rebuilt on boot**, not
backed up: one writer per task, readers refreshing on an interval, and nothing
in the store that could not be regenerated from the system of record.

**On boot — rebuild, one writer, then let go of the lock.**

```python
import mushroomdb

STORE = "./sidecar-store"

def rebuild() -> None:
    # A plain handle holds the lock for its lifetime, so the `with` block is
    # how the lock gets released: readers cannot start until it exits.
    with mushroomdb.GraphDb.open(STORE) as db:
        db.ingest_batch(nodes_from_source_of_truth(), edges_from_source_of_truth())
        db.create_rule(SIMILARITY_RULE, if_not_exists=True)
        db.snapshot()   # a baseline, so a reader's first open replays no WAL

rebuild()
```

`snapshot()` needs the lock too, and the handle above already holds it. On a
store whose contents are disposable the snapshot is not about durability — it
is about the next open being a load rather than a replay.

**Per task — one writer, opened and closed.**

```python
from mushroomdb import GraphDb, MushroomBusy

def apply_task(rows) -> bool:
    try:
        with GraphDb.open(STORE) as db:
            db.ingest_batch(rows, on_conflict="replace")
        return True
    except MushroomBusy:
        return False     # nothing was written; the caller re-queues
```

Open, write, close. The handle holds the lock only for the task's duration, so
another task's writer gets it next. `MushroomBusy` here is a re-queue, not an
error to log at warning level: on a disposable store the work is derived from
state that is still there.

**Readers — open once, refresh on an interval.**

```python
import threading, time

reader = GraphDb.open(STORE, read_only=True)

def follow(interval_seconds: float = 1.0) -> None:
    while True:
        reader.refresh()       # 0 when nothing has changed
        time.sleep(interval_seconds)

threading.Thread(target=follow, daemon=True).start()
```

Open the reader once and keep it. Do not reopen per request: a reopen replays
the WAL and rebuilds indexes, which is the expensive thing `refresh()` exists to
avoid. One long-lived read-only handle plus an interval refresh is the shape.

Scoping composes with all of this. `reader.scoped(role="tenant-a")` is a child
of the one open handle — no second open, no second lock — and it may `refresh()`
like any other. See [masks.md](masks.md#scoped-one-front-door-for-visibility).

### Choosing the interval

**A `refresh()` that finds nothing new costs two filesystem metadata calls and
an integer compare.** `GraphDb::refresh` (`crates/core-api/src/db.rs`) stats the
snapshot file for its `(len, mtime)` identity and stats the WAL for its length;
when the identity is unchanged and the WAL length equals this handle's cursor it
returns `0` without reading a byte of file content. `is_stale()` answers the
same question with the same two stats and no application.

This is asserted, not estimated: `refresh_on_unchanged_store_is_zero_cost` in
`crates/core-api/tests/multiprocess.rs` wraps the filesystem in a counter and
requires that five consecutive no-op refreshes read no file contents at all.

So the interval is a freshness decision, not a cost one. Poll as often as your
staleness tolerance requires. What *does* cost is a refresh that finds
something: the new WAL tail is decoded and applied, rules fire, and a peer's
snapshot triggers a full reload from disk.

---

## Summary

- One writer plus N refreshing readers, or N writers taking turns through the
  lock. Both are supported; both fail loudly or not at all.
- The reader's failure mode is staleness. The writer's is `Busy`, after two
  seconds, with nothing written.
- Local disks only for a store more than one process opens.
- The lock cannot be turned off, and it is advisory: it binds mushroomdb
  processes, not the rest of the machine.
- No-op `refresh()` is two stats and a compare, so interval polling is already
  cheap.
