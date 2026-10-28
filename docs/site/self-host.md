# Self-hosting guide

This page is the path from a first deploy to a store you can lose a machine
under: put it on a volume, secure it, back it up to a bucket, restore it, watch
it, and know where it stops. Each step says what to do and links the page that
holds the detail. The reference pages stay the source of truth; where this page
and one of them differ, that page is right and this one is a bug.

---

## 1. Overview

`mushroomdb serve` is one process holding one store directory. The graph is in
RAM and on that directory, and nowhere else, so everything below is about the
directory: keep it on a volume, copy it somewhere, and be able to put it back.

**One writer.** mushroomdb is single-writer (the `single-writer` rule in
[Concurrency](concurrency.md)). Run exactly one `serve` per store directory
and one per backup prefix. Every template under `deploy/` pins one replica
and says so; none of them can be scaled out. Two servers on one volume fail to
take the write lock, and two on one bucket prefix are caught by the lease in
step 7, which is advisory.

| Step | Do | Reference |
|---|---|---|
| 2 | Pick a platform and deploy | [Platform notes](#platform-notes), `deploy/` |
| 3 | Mount the store directory on a volume | [Running as a service](service.md) |
| 4 | Put TLS in front, set CORS | [Deployment](deployment.md) |
| 5 | Issue access keys | [HTTP API](http.md#access-keys-and-the-write-tier) |
| 6 | Wire the probes and the shutdown window | [Deployment](deployment.md#stopping-cleanly-and-the-grace-period) |
| 7 | Back up to a bucket | [Deployment](deployment.md#one-writer-per-backup-bucket-the-lease) |
| 8 | Restore, to the latest or a point in time | [Deployment](deployment.md#restoring-from-the-backup) |
| 9 | Restore on boot | [Deployment](deployment.md#restoring-on-boot) |
| 10 | Metrics and logs | [HTTP API](http.md#prometheus-text-format) |
| 11 | Size it | [Known limits](limits.md) |

## 2. Requirements

- A host or platform that runs the container image, `ghcr.io/matthewsherlin/mushroomdb`
  (the templates use `:latest`; pin a version tag for anything you keep). The
  image runs as uid 65532, not root, and has no shell.
- One persistent volume for the store directory, mounted at `/data`.
- For backup: an S3-compatible bucket and its credentials (an `s3://`, `r2://` or
  `minio://` URL).
- A token. `serve` refuses a non-loopback `--addr` without `--token`,
  `MUSHROOMDB_TOKEN` or an `admin` access key, and refuses the placeholder
  `<set-me>`.

## 3. The volume

Mount the store directory on storage that outlives the process. A container
that writes its store to its own filesystem starts empty on every restart,
and nothing reports that as an error; see
[What a restart without a volume costs](service.md#5-what-a-restart-without-a-volume-costs).
Every file the directory holds, and what losing each costs, is in
[The volume](service.md#1-the-volume).

Two things to do once:

- **Ownership.** A new Fly or Render volume is owned by root, and the non-root
  image cannot write it: `serve` exits naming `/data/LOCK`. The per-platform fix
  is in [Volume ownership on managed platforms](deployment.md#volume-ownership-on-managed-platforms).
- **A snapshot interval.** The templates pass `--snapshot-every 300`. It bounds
  how much WAL a hard crash leaves to replay, and nothing else; see
  [The snapshot interval](service.md#2-the-snapshot-interval). Automatic
  snapshots keep every archive, so disk grows by one `wal.<N>.archive` per
  snapshot that had WAL to fold.

## 4. TLS and CORS

**TLS.** The recommended path is a reverse proxy that ends TLS in front of a
server bound to loopback, or to the platform's private network: nginx and Caddy
examples are in
[Terminating TLS at a reverse proxy](deployment.md#terminating-tls-at-a-reverse-proxy-recommended).
Fly, Render and Railway end TLS at their edge. Without a proxy, a binary built
with `--features tls` serves TLS itself:

```text
mushroomdb serve /data --addr 0.0.0.0:8443 --token "$MUSHROOMDB_TOKEN" \
  --tls-cert /path/to/cert.pem --tls-key /path/to/key.pem
```

`--tls-cert` and `--tls-key` go together, and a binary built without the
feature refuses them. Getting and renewing the certificate (ACME) is not
mushroomdb's job; the proxy or the platform does it. See
[Native TLS](deployment.md#native-tls---features-tls).

**Bolt.** `--bolt-port` (or `MUSHROOMDB_BOLT_PORT`) serves Bolt too, on the same
credentials. A reverse proxy for HTTP does not carry it. Which platforms can
expose it, and how, is in
[Exposing the Bolt port](deployment.md#exposing-the-bolt-port) and
[Bolt protocol](bolt.md).

**CORS.** A page on another origin can call the API only if
`MUSHROOMDB_CORS_ORIGINS` names it. Unset means no CORS headers. The accepted
forms, `*`, and what it does not cover (`/watch`) are in
[Browser access (CORS)](deployment.md#browser-access-cors).

## 5. Access keys

`MUSHROOMDB_TOKEN` is one all-powerful token. For an application, issue a
narrower key:

```text
mushroomdb keys add /data app --tier write
mushroomdb keys add /data tenant --tier read --role tenant_a
```

The token is printed once; the store keeps only its hash, in `keys.json`. A
running server reloads the file on `SIGHUP`. What each tier (`admin`, `write`,
`read`) may call, the file's strict checks, and what a reload does not reach are
in [Access keys and the write tier](http.md#access-keys-and-the-write-tier).
`keys add` runs against the directory, so on a platform with a volume you run it
where the volume is mounted.

An `admin` key also satisfies the non-loopback token rule. `/metrics` and
`/backup` need the full token or an `admin` key.

## 6. Probes and the shutdown window

| Endpoint | Answers | Use it for |
|---|---|---|
| `GET /health` | 200 while the process is alive; no token | liveness |
| `GET /ready` | 503 until the store is open, then 200; no token | readiness and platform health checks |

See [`GET /health`](http.md#get-health) and [`GET /ready`](http.md#get-ready).
Backup health never gates `/ready`: a rolling deploy starts the new instance
while the old one still holds the lease, and a probe that followed backup health
would never let traffic through. Watch backup health in metrics instead (step 10).

**The start window.** The port is bound only once the store is open, and, with
restore on boot, restored. A probe that runs out before that kills a start
that was working. Give it time for a WAL replay or a restore plus the open:

| Platform | Setting | In the template |
|---|---|---|
| Fly | `grace_period` on the `/ready` check; Fly's maximum is 60 s | `60s` |
| Railway | `healthcheckTimeout` | `120` |
| Kubernetes | a `startupProbe` | five minutes |
| Compose | the healthcheck's `start_period`, if you add one | none |
| Render | the deploy waits for `/ready` | none to set |

**The stop window.** On SIGTERM `serve` drains, writes a shutdown snapshot,
flushes the backup and deletes its lease, all inside `--shutdown-timeout-secs`
(default 55). The platform's stop timeout must be longer, or it kills the
sequence and leaves the lease for the next start to wait out. Nothing
acknowledged is lost either way. The templates use 60 s on Fly, Compose and
Railway, 120 s on Kubernetes, and, because Render fixes the window at 30 s for a
service with a disk, `--shutdown-timeout-secs 25` there. The phases and the
arithmetic are in
[Stopping cleanly](deployment.md#stopping-cleanly-and-the-grace-period).

## 7. Backup to a bucket

Continuous backup is on when `MUSHROOMDB_BACKUP_S3_URL` is set. Credentials and
the endpoint come from the standard `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, `AWS_REGION` (default `us-east-1`) and
`AWS_ENDPOINT_URL` variables; `r2://` and `minio://` need the endpoint. Put the
credentials in the platform's secret store, never in a template.

```text
MUSHROOMDB_BACKUP_S3_URL=s3://my-bucket/prefix
```

A set but malformed value stops `serve` at start; a backup you asked for is
never silently turned off. The settings, with the defaults the code uses:

| Variable | Default | Meaning |
|---|---|---|
| `MUSHROOMDB_BACKUP_S3_URL` | unset (backup off) | `s3://`, `r2://` or `minio://` `bucket/prefix` |
| `MUSHROOMDB_BACKUP_WAL_INTERVAL_MS` | `1000` | How often new WAL bytes ship; at least 1 |
| `MUSHROOMDB_BACKUP_MANIFEST_INTERVAL_S` | `60` | How often a restore point is written while segments accumulate between snapshots |
| `MUSHROOMDB_BACKUP_RETAIN_N` | `7` | Restore points kept; `0` keeps every one |
| `MUSHROOMDB_BACKUP_SSE` | unset | Server-side encryption: `AES256`, `aws:kms` or `aws:kms:dsse`; unset uses the bucket's default |
| `MUSHROOMDB_BACKUP_LEASE_TTL` | `30` | Lease lifetime in seconds; see below |
| `MUSHROOMDB_BACKUP_LEASE_CONDITIONAL` | `on` | `off` for a service that rejects conditional PUT headers |
| `MUSHROOMDB_BACKUP_OP_TIMEOUT_MS` | `10000` | Deadline on each object-store request |
| `MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS` | `10000` | Longest the final flush may take |

**What "backed up" means.** A commit is locally durable once it is fsynced to
`wal.bin`. It is off-box durable once the WAL segment holding it has uploaded.
The worst-case loss on losing the machine is therefore about the WAL interval
plus the upload time, whatever `--snapshot-every` says. The lag is
`mushroomdb_backup_lag_seconds`: the age of the oldest durable byte not yet
shipped. The worker never blocks writes; it reads fsynced bytes only, and
reports failure through metrics and the log.

**Cost.** Each interval with new writes is one object PUT, none while idle.
`snapshot.bin` uploads only when a snapshot is written, an archive uploads once,
and each cycle adds a small manifest PUT and a LIST. Raising the WAL interval
cuts the request count and lengthens the loss window by the same amount. Retention
deletes what no kept restore point names.

**What a backup holds.** `snapshot.bin`, the archives, the WAL segments and the
small side files, including `roles.json` and `keys.json` (key hashes, never
tokens), so a restore brings roles and keys back. The bucket therefore holds
those hashes: give it the access you would give the volume. `mushroomdb backup`
and `POST /backup` are the local copies, take a directory, and do not carry
`keys.json`; see [Backup](service.md#3-backup).

**The lease.** Before it uploads anything, the worker takes a lease on the
prefix, one object named `LEASE`, and renews it every third of the TTL. A second
instance on the same prefix finds it fresh, uploads nothing, reports unhealthy
naming the holder, and keeps serving from its own disk. A clean stop deletes the
lease so a successor starts at once; a killed holder leaves it to expire, at most
40 s at the default.

**The lease is advisory, not a distributed lock.** It catches the usual mistake
of two machines on one bucket. On a service that honours conditional PUTs (AWS
S3, R2, MinIO) the bucket also refuses a stale writer's overwrites, and the worst
outcome is an orphaned object. With `MUSHROOMDB_BACKUP_LEASE_CONDITIONAL=off`
two instances starting together can both upload until the next renewal, and a
restore point can then fail to verify. It is no substitute for running one
instance. The full account is in
[One writer per backup bucket](deployment.md#one-writer-per-backup-bucket-the-lease).

**One store per prefix.** A restored store is a new lineage and the worker
refuses to back it up to the prefix it came from; give it a new prefix. The one
exception is restore on boot (step 9).

## 8. Restore

```text
mushroomdb restore --from s3://my-bucket/prefix --into ./restored
```

This restores the newest restore point, plus the WAL segments uploaded after it
up to the last contiguous one. `--into` must not exist or must be empty. Every
object is checked against its manifest before anything is written there, and the
store appears all at once or not at all.

**Point in time.** `--at` takes a commit number or an RFC 3339 instant:

```text
mushroomdb restore --from s3://my-bucket/prefix --at 2026-10-01T12:00:00Z --into ./restored
```

`--at 75` is the store right after commit 75, numbered as `asof --commit` counts.
An instant resolves through the backup's recorded commit times, and one past the
newest recorded time is refused when later commits have none; restore by commit
number instead. A point is out of reach below the oldest commit the backup holds
or once archive retention cut the history it needs. Details, and what a killed
restore leaves behind, are in
[Restoring from the backup](deployment.md#restoring-from-the-backup).

Start `serve` on the restored directory like any store. Rehearse this before you
need it: restore into a scratch directory and time it from the region you would
restore in. The time sets the start window in step 6.

For a plain directory backup taken with `POST /backup`, the matching restore is
`serve --restore-from <dir>` on an empty store directory; see
[Restore](service.md#4-restore).

## 9. Restore on boot

```text
MUSHROOMDB_RESTORE_FROM_S3=s3://my-bucket/prefix
```

Set beside `MUSHROOMDB_BACKUP_S3_URL`, usually to the same prefix. When the data
directory holds no store, `serve` restores the prefix's newest restore point
before it opens the store and before it binds the port, so a replaced machine or
a new volume comes back with its data.

| Data directory | Prefix | `serve` does |
|---|---|---|
| Holds a store | not contacted | serves the store |
| Empty | has a backup | restores it, then serves |
| Empty | holds no backup | warns, starts empty: a first deploy |
| Empty | backup that cannot be restored | exits 1, serves nothing, tries again next start |

Rules worth knowing before you leave it in a template:

- It never overwrites a store. It cannot be combined with `--restore-from` or
  `--demo-if-empty`; `serve` refuses the pair.
- It takes the bucket lease first. If another instance holds a fresh lease,
  `serve` exits 1 naming it and writes nothing.
- The restored store continues the same prefix as a new lineage, with a
  `backup.parent` record, and the parent's restore points stay as they were.
- A large restore delays readiness, so size the start window to it (step 6). A
  start window that runs out loses nothing; the next start restores again.

The full rules, including the install marker a crash leaves, are in
[Restoring on boot](deployment.md#restoring-on-boot).

## 10. Metrics and logs

**Prometheus.** `GET /metrics` with `Accept: text/plain` answers in the Prometheus
text format; without it, JSON. It needs the full token or an `admin` key. A scrape
job with the token as a bearer credential, the metric list and example alerts are
in [Prometheus text format](http.md#prometheus-text-format). The three to alert
on are `mushroomdb_backup_lag_seconds`, `mushroomdb_backup_healthy` and
`mushroomdb_backup_last_success_timestamp_seconds`. The `mushroomdb_backup_*`
series exist only with backup on.

**Logs.** `MUSHROOMDB_LOG_FORMAT=json` writes one JSON object per line on stderr,
for a log shipper; unset or `text` keeps text. The templates for Fly, Render and
Compose set `json`. `MUSHROOMDB_LOG` takes the usual filter, such as
`server=debug`, for a line per request while you debug. The default is quiet: 5xx,
requests slower than 5 s and 4xx are logged, and every response carries an
`x-request-id` to find them by. The query string is never logged. See
[Stopping it, and what it writes to the log](service.md#7-stopping-it-and-what-it-writes-to-the-log).

## 11. Sizing and limits

There is no measured formula from node count to RAM in this repository, so this
page does not give one. What is measured, on the build and machine named in each
case, is in [Known limits](limits.md): a run of 1,000,000 nodes in the memory
shape (entities with text, about 1.6 edges per node, one rule) peaked at
2.65 GiB and opened in 1.18 s from a snapshot and 11.3 s from the WAL; a run of
100,000 nodes with about 10 million derived edges needed 8.09 GiB to replay the
WAL without a snapshot. Memory depends on edges and rules as much as nodes, so
measure your own data: load a copy and read `rss_bytes` from `GET /metrics` (JSON) or
`mushroomdb_rss_bytes` from its text form. A snapshot is memory-mapped, so a store
reopens far faster from one than from its WAL.

Set the platform's memory limit above the peak you measure, with room for the
shutdown snapshot. The Kubernetes template asks for 512Mi and limits 1Gi, which is
a placeholder to change.

Limits that shape a deployment:

- One writer per store; no read replicas. Scale up, not out.
- HTTP transactions and Bolt connections are bounded: `--tx-max` (100),
  `--tx-timeout-secs` (30), `--tx-max-lifetime-secs` (300) and
  `--bolt-max-connections` (256). See [Transactions](http.md#transactions) and
  [`serve`](cli.md#serve).
- Peer writes by another process fire no subscriber events; the rest are in
  [Known limits](limits.md).

## Platform notes

Each template under `deploy/` is a starting point, not a tested deployment: none
has run on a live platform yet, and the pages that describe them say so.

| Platform | Template | What to know |
|---|---|---|
| Fly | `deploy/fly/fly.toml` | One machine and one volume `mushroomdb_data`; chown the volume once; `kill_timeout = 60`; `/ready` check with a 60 s grace; Bolt public behind Fly's `tls` handler, with `MUSHROOMDB_BOLT_ADVERTISED_HOST` to change |
| Render | `deploy/render/render.yaml` | A disk fixes the stop window at 30 s, so `--shutdown-timeout-secs 25`; the disk mounts root-owned, so use the wrapper image; HTTP only, no Bolt |
| Railway | `deploy/railway/railway.json` | `RAILWAY_RUN_UID=0` for the volume; `healthcheckTimeout` 120; `drainingSeconds` 60; Bolt is a dashboard setting |
| Docker Compose | `deploy/docker-compose/docker-compose.yml` | A named volume needs no chown; `stop_grace_period: 60s`; bundled MinIO for development only; Bolt bound to `127.0.0.1` |
| Kubernetes | `deploy/k8s/statefulset.yaml` | One-replica StatefulSet; create the token Secret first; `fsGroup: 65532`; `/ready` and `/health` probes; Bolt in-cluster only |

Backup and restore-on-boot are in each template as commented-out lines for you to
turn on; the `AWS_*` credentials go in the platform's secret store. The
per-platform recipes, the Kubernetes steps and what CI checks of the templates are
in [Deployment](deployment.md).

## See also

- [Running as a service](service.md): the volume, snapshots, local backup and restore
- [Deployment](deployment.md): TLS, CORS, Bolt, the lease, restore, shutdown
- [HTTP API](http.md): probes, access keys, metrics
- [Known limits](limits.md): what was measured and what was not
