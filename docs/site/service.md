# Running it as a service

`mushroomdb serve` is a single process holding one store directory. The graph is
in RAM and on that directory, and nowhere else — there is no replica, no
external log, and nothing reconstructs it from somewhere else. So the operator's
job is small and entirely mechanical: mount the directory, bound the replay,
copy it somewhere, and be able to put it back.

This page is that runbook, end to end.

---

## 1. The volume

A store is a directory. Mount it; everything below follows from that.

| File | What it is | Losing it costs |
|---|---|---|
| `snapshot.bin` | the whole store as one memory-mappable image | a from-genesis WAL replay, or everything if there is no WAL |
| `snapshot.bin.bak` | the pre-migration image, written before a format upgrade rewrites `snapshot.bin` | the pre-upgrade copy to fall back to |
| `wal.bin` | every commit written since the last snapshot | those commits |
| `wal.floor` | where the retained history starts | the horizon a pruned store reports |
| `wal.genesis` | the marker that lets `asof` replay into the archives | `asof` reach below the last snapshot |
| `roles.json` | role tokens and their masks | every configured role |
| `wal.<N>.archive` | the folded WAL each snapshot left behind | `node_history`, `edge_history`, `was_linked` and `asof` reach into that span |
| `LOCK` | always empty; carries the advisory cross-process write lock | cross-process coordination for handles opened afterwards |

All of them matter. A backup that takes `snapshot.bin` alone is a store with no
past and no roles.

**One writer per directory.** `LOCK` is what enforces it across processes: every
path that appends to the write-ahead log takes the advisory lock first, and a
writer that cannot take it within `WRITE_LOCK_WAIT` gets `GraphError::Busy`
rather than corrupting anything (CHANGELOG v0.6.0, **Multi-process safety**).
Readers are unaffected — many of them can share the directory. Two `serve`
processes on one volume is not a supported configuration; one serves, the rest
read. See [Concurrency](concurrency.md).

`LOCK` is not part of the on-disk format. A store copied without it works
normally, which is why no backup carries one.

---

## 2. The snapshot interval

```text
mushroomdb serve /data --snapshot-every 300
```

`--snapshot-every <secs>` (`crates/cli/src/lib.rs:505`) bounds one thing: how
much WAL a **hard** crash leaves to replay on the next open. Without it, a store
that has never snapshotted replays from genesis, which re-derives every rule's
edges and rebuilds the ANN index for any `VectorSimilar` rule — minutes on a
large store. See [Durability and crash recovery](durability.md).

Three properties worth knowing:

- **Graceful shutdown takes one too.** SIGINT and SIGTERM snapshot before
  exiting, so a clean restart always recovers from an image. The interval is
  insurance against the ungraceful case.
- **A tick is skipped, never queued.** A snapshot replaces `wal.bin`, so it needs
  the store's cross-process write lock; the timer waits `SNAPSHOT_LOCK_WAIT`
  (500 ms, `crates/cli/src/lib.rs:91-111`) and, if a peer holds it, drops that
  tick rather than piling up behind it. The next one is only a period away, and
  a missed snapshot costs a longer replay, never data.
- **Automatic snapshots keep every archive.** `AUTO_SNAPSHOT_RETENTION` is
  `None` (`crates/cli/src/lib.rs:75`), so a `--snapshot-every` tick archives the
  folded WAL as `wal.<N>.archive` and prunes nothing. History is what the
  archives exist for. The consequence is disk: one new archive per snapshot that
  had WAL to fold, indefinitely. `mushroomdb stats <db-dir>` prints a `history:`
  line saying where the retained history starts, so growth is never a surprise,
  and `mushroomdb snapshot <db-dir> --retention N` is how to bound it once that
  trade is worth making. Pruning advances the history horizon — see
  [How far back history reaches](timetravel.md#how-far-back-history-reaches) for
  exactly what stops answering.

Pick a period against your crash-replay budget, not against write volume. Five
minutes is a reasonable default for a served store.

---

## 3. Backup

```text
mushroomdb backup /data /backups/2026-09-11T02-00Z
```

The command copies `snapshot.bin`, `snapshot.bin.bak`, `wal.bin`, `wal.floor`,
`wal.genesis`, `roles.json` and every `wal.<N>.archive` into the destination,
then **verifies**: it CRC-checks the copy and opens it. The printed report ends
in `verified: true`, and a copy that does not verify is one you must not use.

> **The CLI form is unsafe against a live `serve`.** It says so in the usage
> text: the copies are not atomic across processes, so a backup taken while a
> server is writing can catch a half-written file. Stop the server, or use the
> HTTP endpoint.

For a running server, `POST /backup` is the correct path. It holds the read lock
for the full duration — copies plus verification — so writers block for that
window and the copy is consistent. It is full-token only; role tokens get 403.
See [POST /backup](api.md#post-backup).

Backups are ordinary directories. A rolling scheme that writes one per run into
a vault directory, optionally with a `latest` symlink or copy, is exactly the
shape restore expects.

---

## 4. Restore

```text
mushroomdb serve /data --restore-from /backups
```

`--restore-from <dir>` seeds an **empty** store directory before the server
opens it:

- If `<dir>` itself holds a store — a `snapshot.bin`, or a non-empty `wal.bin`
  for a store that has never snapshotted — that is the backup.
- Otherwise `<dir>` is a directory *of* backups. An immediate subdirectory named
  `latest` holding a store wins outright — so a symlink or a rolling copy can
  name itself — and failing that, the newest by mtime wins.
- The chosen backup's files are copied in and the store is opened once to prove
  it works, which runs the same CRC and replay checks any open runs. A copy that
  does not open is a hard failure naming the path, and `serve` exits non-zero
  rather than starting empty.

It prints one line:

```text
restored from /backups/latest: 4 files, 2097152 bytes
```

Three rules make it safe to leave in a container command line forever:

1. **It never overwrites a store.** If the directory already holds a store — by
   the same `snapshot.bin`-or-non-empty-`wal.bin` test — it does nothing and says
   `restore-from: <db-dir> already holds a store; not restoring` on stderr. This
   is boot-time seeding for a fresh volume, not a rollback command — to roll
   back deliberately, move the store directory aside first.
2. **An empty vault is a warning, not an error.** A first boot against a backup
   volume with nothing in it yet prints `restore-from: no backup found under
   <path>` and starts empty, which is the only sensible behaviour for day one.
3. **It runs before `--demo-if-empty`.** A restored store is never overwritten
   by the demo seed, whichever order the flags appear in.

The manual equivalent is exactly what the flag automates: with the server
stopped, copy the backup directory's files into the store directory and start
it. Nothing else is required — no import step, no rebuild.

---

## 5. What a restart without a volume costs

Everything.

Nodes, edges, every rule-derived edge, the rules themselves, the WAL, every
archive, and all history. A container that writes its store to the container
filesystem starts from zero on every restart, every redeploy and every
reschedule, and nothing anywhere reports that as an error — the new store is a
perfectly valid empty one.

There is no second copy. The graph lives in RAM and on that directory. If the
directory is ephemeral, so is the graph.

This is the single most likely way to lose a mushroomdb deployment, and the fix
is one line of configuration: mount the store directory on a volume that
outlives the process, and point `--restore-from` at a backup location so a
volume that *is* lost can be re-seeded on the next boot.

---

## 6. Commit numbers are per store lifetime

`was_linked`, `edges_at`, `asof` and the `commit` field on every history event are
indices into **this store's** write-ahead log, counted from its genesis. They are
not timestamps and they are not portable. A store restored from a backup keeps its
numbering, because the WAL and its archives come with it. A store recreated from
scratch starts again at 0, and a commit number recorded against the old one means
something different in the new one. If a commit index has to survive a rebuild,
record a timestamp beside it.

---

## 7. A worked container command line

```text
docker run -d --name mushroomdb \
  -p 8080:8080 \
  -v /srv/mushroomdb/data:/data \
  -v /srv/mushroomdb/backups:/backups \
  -e MUSHROOMDB_TOKEN="$TOKEN" \
  mushroomdb:local serve /data \
    --addr 0.0.0.0:8080 \
    --snapshot-every 300 \
    --restore-from /backups
```

The image's `ENTRYPOINT` is the `mushroomdb` binary, so everything after the
image name replaces the default `CMD` (which is
`serve /data --addr 0.0.0.0:8080 --demo-if-empty`). What each piece is doing:

- `-v …:/data` — the store directory on a volume that outlives the container.
  This is the line §5 is about.
- `-v …:/backups` — the vault `--restore-from` reads and `backup` writes. A
  separate volume on purpose: a restore is only useful if it survives losing the
  first one.
- `MUSHROOMDB_TOKEN` — required, because `--addr 0.0.0.0:8080` is not loopback.
  `serve` refuses to start on a non-loopback bind without a token. Passing it in
  the environment keeps it out of the process's argument list. Terminate TLS at
  a reverse proxy in front of this, or build with `--features tls` and pass
  `--tls-cert`/`--tls-key` — see [Deployment](deployment.md).
- `--snapshot-every 300` — caps crash replay at five minutes of WAL.
- `--restore-from /backups` — a no-op on every boot where `/data` survived, and
  the thing that saves the deployment on the boot where it did not.

Take the backups themselves from outside the container against the running
server, so the copy is consistent:

```text
curl -fsS -X POST http://127.0.0.1:8080/backup \
  -H "Authorization: Bearer $MUSHROOMDB_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"dest":"/backups/2026-09-11T02-00Z"}'
```

Check `verified` in the response body on every run. A backup nobody verified is
a backup nobody has.

---

## See also

- [Durability and crash recovery](durability.md) — what recovery replays, what a
  snapshot archives, and what retention costs
- [Concurrency](concurrency.md) — many readers, one writer, and `Busy`
- [Deployment](deployment.md) — TLS, reverse proxies, and the loopback default
- [Time travel](timetravel.md) — `asof`, the horizon, and archive reachability
