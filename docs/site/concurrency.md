# Concurrency across processes

A mushroomdb store is **many readers, one writer**. Any number of processes may
read it at the same time, and every one of them sees every commit; exactly one
may be writing at any moment. This is what makes it safe to run a server on a
store while an editor hook, a git hook, and a `mushroomdb` command all touch the
same directory.

Two mechanisms carry the whole model:

- an **advisory write lock**, an empty `LOCK` file in the store directory that
  writers hold while they commit;
- a **frame cursor**, the byte offset of the WAL prefix a handle has applied,
  which lets a handle pick up another process's commits by reading only what is
  new.

## The write lock

Every path that appends to the write-ahead log takes the lock first. There are
two such paths and they take it at different granularities.

**A plain handle holds it for its lifetime.** `GraphDb::open` locks the store
and keeps it locked until the handle drops. One-shot commands are shaped this
way, so they serialise against everything else naturally. Opening a second
read-write handle — in this process or any other — waits up to
`WRITE_LOCK_WAIT` for the first one to let go, then fails with
`GraphError::Busy`.

**A shared handle takes it per write.** `SharedDb`, which the server uses, holds
its handle open for as long as the process runs; keeping the lock that long
would shut every other process out. It takes the lock inside
`SharedDb::write()` instead, and the group-commit writer takes it once per
group. Between writes the store is free for anyone else.

**Snapshots need it too.** A snapshot replaces the write-ahead log with a fresh
baseline, so a peer that is appending would be left holding a descriptor on a
file that no longer exists, silently losing commits it believes durable.
`snapshot()` therefore refuses with `Busy` unless the caller holds the lock. The
server's periodic and shutdown snapshots wait briefly, then skip and log a line:
a missed snapshot only means a longer replay next time, so it is never worth
delaying a shutdown or queueing behind a busy peer.

Taking the lock also refreshes, so a write always lands on top of every commit
other processes have made. Releasing happens only after the commit's fsync
completes: until then the WAL tail holds bytes that are written but not durable,
and another process must not snapshot around them.

Waiting for the lock never costs readers anything. A writer polls for it before
it takes any in-process lock, so a busy peer in another process cannot stall
reads in this one.

The lock is advisory. It coordinates cooperating mushroomdb processes; it does
not stop an unrelated program from editing the files.

## `Busy`

A writer that cannot get the lock within its wait budget — two seconds by
default, `WRITE_LOCK_WAIT` — gets `GraphError::Busy` (`MushroomBusy` in Python).

Nothing was written and no in-memory state changed, so retrying later is always
safe. `Busy` means another process is writing right now, not that anything is
wrong with the store.

Readers never see it. Reading takes no lock and never waits for one.

## Refresh

A handle does not poll the store. `refresh()` brings it up to date and returns
how many commits it applied.

It reads the WAL from the handle's cursor forward, decodes the complete frames
it finds, and applies them through the same code path the open replay uses. That
sameness is the point: rules fire, derived edges appear, and interners, id maps
and indexes stay valid, exactly as they would on a fresh open.

Two cases are worth naming.

**A partial trailing frame is a wait, not a corruption.** When another process
is midway through appending, the last bytes in the file are not yet a whole
frame. They are left alone. `refresh()` returns the count of complete frames it
applied — possibly zero — and the handle stays stale until that frame lands.

**A peer's snapshot triggers a reload.** A snapshot replaces the store's base
image and usually truncates the WAL, so the tail no longer continues the
handle's state. The handle detects this by the snapshot file's identity changing
and rebuilds itself from disk with the options it was opened with, which is
transparent to the caller apart from taking longer.

Nothing is written during a refresh, so a read-only handle can refresh freely.

`is_stale()` answers the same question without doing the work. It costs two
metadata lookups and reads no file contents.

`SharedDb::read()` calls this for you, at most once per `REFRESH_CHECK_INTERVAL`
(50 ms). A server handle therefore stays current without reopening, and a tight
read loop pays nothing.

## Read-only handles

`OpenOptions { read_only: true, .. }` opens a handle that:

- never takes the lock, so it opens immediately even while a writer holds it,
  and never makes a writer wait;
- writes nothing at open — no WAL repair write-back, no snapshot migration;
- returns `GraphError::ReadOnly` from every mutation;
- still refreshes, so it can follow a writer's commits.

This is what an unattended reader wants. `mushroomdb recall`, which runs on
every prompt under a short timeout, opens this way for exactly these reasons: it
cannot delay a writer, cannot fail because one is running, and cannot discard a
frame a writer believes durable.

## What the hooks rely on

An editor hook, a git hook, and a `sync` command all write to a store the server
holds open. Three properties make that safe:

1. The server's handle does not hold the lock between writes, so a hook can get
   it.
2. A hook's write is serialised against the server's by the lock, so the WAL
   never interleaves two processes' frames.
3. The server's handle picks the hook's commits up on its next read, without
   restarting and without the hook telling it anything.

A hook that finds the store busy exits without writing and tries again on the
next event. That is the intended behaviour, not a failure: the work it was going
to do is derived from state that is still there.

## What this does not give you

The lock serialises writers; it is not a transaction manager. There are no
cross-process transactions and no isolation levels. A reader mid-refresh can
observe a commit boundary but not a half-applied commit.

**Subscriptions do not fire for another process's writes.** Commits absorbed by
`refresh` replay exactly as they would at open, and open notifies nobody — so a
mutation subscriber, and the `/watch` and `/subscribe` endpoints built on it,
see only writes made through this process. The data is there on the next read;
the notification is not. Poll if you need to react to a hook's writes.

Within one process, `SharedDb` serialises writers with its own locks and readers
run concurrently. See the durability page for what a commit guarantees once it
returns.
