# Deployment

This page is about reaching the server: TLS, reverse proxies, and the loopback
default. For keeping the store alive across restarts — the volume, backups, and
`serve --restore-from` — see [Running it as a service](service.md).

---

## Terminating TLS at a reverse proxy (recommended)

The simplest and most operationally robust way to serve mushroomdb over HTTPS
is to run it on loopback and let a reverse proxy handle TLS termination.

### nginx

```nginx
server {
    listen 443 ssl;
    server_name db.example.com;

    ssl_certificate     /etc/letsencrypt/live/db.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/db.example.com/privkey.pem;
    ssl_protocols       TLSv1.2 TLSv1.3;
    ssl_ciphers         ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305;
    ssl_prefer_server_ciphers off;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

mushroomdb still requires `--token` (or `MUSHROOMDB_TOKEN`) when the
underlying bind is non-loopback. With a loopback bind behind nginx the token
is optional but recommended.

### Caddy

```text
db.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Caddy manages ACME certificates automatically; no extra configuration is
needed for Let's Encrypt.

---

## Native TLS (`--features tls`)

mushroomdb includes optional rustls support behind a cargo feature flag. Use
this when a reverse proxy is unavailable or inconvenient (e.g., single-binary
deployments without a proxy tier).

### Build

```text
cargo build -p mushroomdb-cli --bin mushroomdb --features tls --release
```

### Run

```text
./target/release/mushroomdb serve ./db \
  --addr 0.0.0.0:8443 \
  --token changeme \
  --tls-cert /path/to/cert.pem \
  --tls-key  /path/to/key.pem
```

Both `--tls-cert` and `--tls-key` must be supplied together; supplying only
one is a startup error. Non-loopback binds still require `--token` or
`MUSHROOMDB_TOKEN` regardless of TLS.

### Self-signed certificates (development only)

Generate a self-signed certificate with `openssl`:

```text
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem \
  -days 365 -nodes -subj '/CN=localhost'
```

Self-signed certificates are not trusted by browsers without manual
configuration. For production, use a certificate from a trusted CA (e.g.
Let's Encrypt via `certbot` or `acme.sh`).

### Binary built without the feature

If you pass `--tls-cert` / `--tls-key` to a binary built without `--features
tls`, mushroomdb exits immediately with:

```text
this binary was built without TLS support; rebuild with --features tls
or terminate TLS at a reverse proxy (see docs/site/deployment.md)
```

---

## Exposing the Bolt port

The Bolt listener is off unless `--bolt-port` (or `MUSHROOMDB_BOLT_PORT`) is set; 7687 is the usual port. It binds `--addr`'s IP, so it is reachable exactly where HTTP is, and the same authentication rule applies: a non-loopback `--addr` needs `--token`, `MUSHROOMDB_TOKEN` or an `admin` key. See [Bolt protocol](bolt.md) for every flag.

- **Publish the port** next to the HTTP one: `-p 7687:7687` for Docker, a second service port on a platform. A reverse proxy for HTTP does not carry Bolt: Bolt is its own TCP protocol, so it needs a TCP (layer 4) route, or is reached directly.
- **TLS.** With `--tls-cert`/`--tls-key`, Bolt serves the same certificate and is TLS unless `--bolt-plaintext` is given. When TLS ends at a TCP proxy in front of the server instead, run Bolt plaintext behind it.
- **The advertised address.** In a container `--addr` is usually `0.0.0.0`, which cannot be handed to a driver, so `serve` refuses to start Bolt there without `--bolt-advertised-host` (the host name clients use). When the published port differs from the listener's, also give `--bolt-advertised-port`.

```text
mushroomdb serve /data --addr 0.0.0.0:8080 --token "$MUSHROOMDB_TOKEN" \
  --bolt-port 7687 --bolt-advertised-host db.example.com
```

### Bolt in the deploy templates

Each template under `deploy/` sets `MUSHROOMDB_BOLT_PORT=7687` only where the platform can carry Bolt without exposing an unencrypted port to the internet. In every case Bolt follows HTTP's authentication rule, so the token the template already requires (`MUSHROOMDB_TOKEN`) is the Bolt credential too. "Routing URI" below means the driver's routing scheme; it connects back to the host and port the server advertises.

| Platform | Bolt | How |
|---|---|---|
| Fly | Public, TLS at the edge | A TCP service on 7687 with Fly's `tls` handler (no `http` handler). Fly terminates TLS and forwards plaintext to the server, so use the TLS form of the Bolt scheme. `MUSHROOMDB_BOLT_ADVERTISED_HOST` is set to `mushroomdb.fly.dev`: change it to your app's address, or a routing URI hands drivers the wrong host. A shared IPv4 is enough with the `tls` handler. |
| Docker Compose | This machine only | `127.0.0.1:7687:7687`. To expose it, change the mapping to `7687:7687` and put TLS in front (`--tls-cert`/`--tls-key`, or a TCP proxy that ends TLS). |
| Kubernetes | In the cluster only | A `bolt` container port and a port on the ClusterIP Service; `MUSHROOMDB_BOLT_ADVERTISED_HOST` is the Service name. To reach it from outside, use a LoadBalancer Service with TLS termination, and set the advertised host to its public name. |
| Render | Not available | A Render web service exposes only HTTP, so the template does not start Bolt. A private service may carry other ports to your other Render services (unverified). |
| Railway | Off; dashboard only | Enable Settings, Networking, TCP Proxy with internal port 7687, then set `MUSHROOMDB_BOLT_PORT`, `MUSHROOMDB_BOLT_ADVERTISED_HOST` and `MUSHROOMDB_BOLT_ADVERTISED_PORT` from the domain and port Railway shows. Whether the proxy encrypts is unverified, and no config-as-code setting for it was found, so the template never opens it. |

None of this has been run on a live platform yet.

---

## Browser access (CORS)

A web page on another origin can call the HTTP API only if the server says so.
Set `MUSHROOMDB_CORS_ORIGINS` beside `MUSHROOMDB_TOKEN` and `MUSHROOMDB_ROLE_TOKENS`:

```text
MUSHROOMDB_CORS_ORIGINS=https://app.example.com,http://localhost:3000
```

- **Unset or blank:** no CORS headers are sent, exactly as before.
- **An explicit list:** comma-separated `scheme://host[:port]` origins, with no path,
  trailing slash or wildcard. Only those origins are allowed, and credentials
  (the `Authorization` header, cookies) are allowed with them.
- **`*`:** any origin is allowed, and credentials are never sent, because browsers
  reject `*` combined with credentials. A page on another origin cannot use a
  cookie or a header token that way. `*` cannot be mixed with other entries.
- **Preflight:** the browser's `OPTIONS` request needs no token, because browsers
  send none on a preflight. It grants nothing: the request that follows is still
  authenticated.
- **Invalid values:** `serve` exits with status 1 and a message before it opens the store.
- **`/watch` is not covered.** CORS does not govern WebSocket upgrades, so this
  setting does not protect `/watch` or `/subscribe` from a page on another origin;
  the token does.

## Volume ownership on managed platforms

The container image runs as uid 65532 (not root) and has no shell, so it cannot
fix the ownership of its own data volume. Fly and Render mount a new volume
owned by root; the server then cannot create `LOCK` or `wal.bin` in `/data`. It
refuses to start and says so:

```text
the data directory is not writable: Permission denied (os error 13) on /data/LOCK.
This process runs as uid 65532; /data is owned by uid 0 gid 0 with mode 0755. …
```

(An older release reported this as `store is busy: write lock held`. That was
wrong: nothing held a lock. A genuinely held lock still reports `busy`.) An
existing store on a deliberately read-only mount still starts, as before, and
serves reads; its writes fail with the same cause.

Fix the volume once, before the first deploy. The image stays non-root.

| Platform | Recipe |
| --- | --- |
| Fly | Run a one-off machine on the volume that changes its owner, then deploy. The volume and the machine must share a region. `fly machine run busybox:stable chown -R 65532:65532 /data --app <app> --region <region> --volume mushroomdb_data:/data --rm` |
| Render | Render has no setting for disk ownership. Build a thin wrapper image that starts as root, changes the owner of `/data`, then drops to uid 65532 before it execs the server (below). Point the service at the wrapper instead of the stock image. |
| Railway | Set the service variable `RAILWAY_RUN_UID=0`. This runs the container as root on Railway; the image itself is unchanged. |
| Docker Compose | Nothing to do. A named volume is initialised from the image's `/data`, which is owned by 65532. A bind mount must be owned by uid 65532 on the host: `sudo chown -R 65532:65532 ./data`. |

Render wrapper image (replace the owner and tag with yours). `runtime: image`
cannot build a Dockerfile, so build this one and push it to a registry Render can
pull from (for example `ghcr.io/<you>/mushroomdb-render:latest`), then set that as
the service's image URL; or keep it in your repository and use `runtime: docker`
with `dockerfilePath`:

```dockerfile
FROM ghcr.io/matthewsherlin/mushroomdb:latest AS app

FROM debian:12-slim
COPY --from=app /usr/local/bin/mushroomdb /usr/local/bin/mushroomdb
ENTRYPOINT ["/bin/sh", "-c", "chown 65532:65532 /data && exec setpriv --reuid=65532 --regid=65532 --clear-groups /usr/local/bin/mushroomdb \"$@\"", "--"]
CMD ["serve", "/data", "--addr", "0.0.0.0:8080", "--snapshot-every", "300"]
```

**Status: not yet confirmed on a real deploy.** The Fly, Render and Railway
recipes are the most reliable routes known, written without a live account.
They are confirmed or corrected at the release gate by a real deploy on each
platform, and this page changes with what that shows.

## Kubernetes

`deploy/k8s/statefulset.yaml` runs one replica as a StatefulSet with a ReadWriteOnce volume. The token is not in the file: create it first, or the pod stays in `CreateContainerConfigError`.

```text
kubectl create secret generic mushroomdb-token --from-literal=token="$(openssl rand -hex 32)"
kubectl apply -f deploy/k8s/statefulset.yaml
```

- **Pin the image.** `:latest` is only the manifest default; set a version tag.
- **One replica.** mushroomdb is single-writer; the manifest ships no autoscaler.
- **Volume ownership.** `fsGroup: 65532` gives the non-root image a writable volume. A storage class that ignores `fsGroup` shows as a permission-denied crash loop on `/data`.
- **Placeholder token.** `serve` refuses the literal `<set-me>` as a token and exits with status 1.
- **Backup.** `MUSHROOMDB_BACKUP_S3_URL` and `MUSHROOMDB_RESTORE_FROM_S3` are in the ConfigMap, commented out. The `AWS_*` credentials belong in a Secret of your own.
- **Not yet verified on a live cluster.**

## How CI checks the templates and the backup

The `hosting-templates` workflow (`.github/workflows/hosting-templates.yml`)
runs on every push and pull request into `main` and `release/**`.

| Job | Runs | What it checks |
| --- | --- | --- |
| `s3-backup` | Always | Every S3-gated test against a MinIO started in the job, then `scripts/minio-smoke-test.sh` |
| `fly-lint` | When `deploy/**`, the `Dockerfile` or the workflow change | `flyctl config validate --strict` on `deploy/fly/fly.toml`; a warning fails it too |
| `k8s-lint` | Same | `kubeconform -strict` on `deploy/k8s/statefulset.yaml`, and `docker compose config` on the Compose file |
| `docker-build` | On pushes that change the image's inputs | `docker build --no-cache`, with `--features tls` as the Dockerfile builds it. On pull requests, the `docker` job in `ci.yml` builds and runs the same Dockerfile |

The S3-gated tests read `MUSHROOMDB_TEST_S3_ENDPOINT`. Without it each one
prints `skipping the S3 test` and passes, which is why the main workflow,
which starts no S3 service, never really ran them. `s3-backup` fails if any
test prints that line. `scripts/s3-gated-tests.py` finds the S3-gated tests
in the source: those that call their file's gate helper (`bucket()` in
`crates/backup/tests/minio.rs`, `S3Env::from_env()` in
`crates/cli/tests/serve_backup.rs`) or read the variable themselves. The job
fails unless every one of them passed, and fails if any other file reads the
variable.

The smoke script runs the built binary end to end:

1. `serve` with backup on takes 10 writes over HTTP.
2. `<prefix>/LATEST` appears in the bucket.
3. `mushroomdb restore --from` returns all 10 nodes.
4. `serve` stops cleanly on SIGTERM.
5. A `serve` with `MUSHROOMDB_RESTORE_FROM_S3`, started on an empty directory, serves the same 10 nodes.

To run it locally against any S3-compatible service:

```text
MUSHROOMDB_TEST_S3_ENDPOINT=http://127.0.0.1:9000 bash scripts/minio-smoke-test.sh
```

The bucket (`MUSHROOMDB_TEST_S3_BUCKET`, default `mushroomdb-test`) must
exist. The credentials come from `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY`, and default to MinIO's. Without the endpoint the
script skips, except when `CI` is set: there, a skip is a failure.

Two of the checks work around tool limits:

- `kubectl apply --dry-run=client` still asks an API server for its schema,
  so the manifest is checked with kubeconform instead. It needs no cluster and
  validates against the Kubernetes 1.31 schemas at a pinned commit.
- `flyctl config validate` will not start without a token. It makes no API
  call for this command, so the job gives it a dummy one.

Every tool is pinned: flyctl and kubeconform by version and SHA-256, the
actions by commit, and the MinIO image by digest.

## One writer per backup bucket (the lease)

When `MUSHROOMDB_BACKUP_S3_URL` turns on continuous backup, `serve` takes a lease
on the backup prefix before it uploads anything. The lease is one object,
`<prefix>/LEASE`, naming the instance that holds it:

```json
{ "instance_id": "…", "acquired_at": "…", "renewed_at": "…", "ttl_s": 30 }
```

The holder renews it every third of a TTL. A second instance pointed at the
same prefix finds a lease renewed less than `ttl_s` ago and uploads nothing.
Its backup health turns false, naming the holder, while it keeps serving from
its local disk. It does not give up: it checks again every third of a TTL. A
holder can stop renewing because it died, or because it is shutting down in a
rolling deploy. Either way the waiting instance takes over within one TTL plus
one check, at most 40 s at the default.

A clean shutdown (SIGTERM or SIGINT) deletes the lease, so a successor holds it
within a few seconds instead of waiting out the TTL (see
[Stopping cleanly](#stopping-cleanly-and-the-grace-period)). A holder that is
killed, or that cannot reach the bucket in time, leaves the lease to expire
after its TTL.

| Variable | Default | Meaning |
| --- | --- | --- |
| `MUSHROOMDB_BACKUP_LEASE_TTL` | `30` | Seconds a lease stays its holder's after a renewal; at least 1 |
| `MUSHROOMDB_BACKUP_LEASE_CONDITIONAL` | `on` | `off` for a service that rejects conditional PUT headers; the lease is then best-effort |

**The lease is advisory, not a distributed lock.** It catches the common mistake
of giving two machines the same bucket. What it guarantees depends on the
service:

- **Conditional services (AWS S3, Cloudflare R2, MinIO).** The lease is taken and
  renewed with `If-None-Match` / `If-Match`. Two instances that start together
  get exactly one lease. A holder whose lease was taken over learns so at its
  next renewal. Each time the lease is taken, a probe checks that the service
  really refuses both headers. A service that accepts either one is refused:
  backup does not start, and health and the log say why and name the switch.
  Exclusion is never lost silently.
- **Best-effort (`MUSHROOMDB_BACKUP_LEASE_CONDITIONAL=off`).** Each step reads
  the lease, then writes it. Two instances that start together can both upload
  until the next renewal, and the log warns of this when the lease is taken.
- **A holder stops itself in time.** A holder that cannot renew stops writing
  two thirds of a TTL after its last successful renewal. Its in-flight uploads
  are cancelled at that point. Nobody may take the lease over until a full TTL
  has passed. So a holder cut off from the bucket writes nothing after a
  takeover, even once the network comes back. Clock skew between machines of up
  to a third of a TTL (10 s at the default) is absorbed.
- **A lost answer is not a lost lease.** A renewal can time out on the client and
  still land on the service. The holder then recognises the lease as its own.
  It never waits behind itself or reports itself as the foreign holder.
- **The bucket refuses a stale writer too.** On a conditional service every
  immutable object is written with `If-None-Match: *`, and `LATEST` and the
  manifests with `If-Match`. An instance that lost the prefix cannot overwrite
  an object, and its `LATEST` is refused once a successor has written its own.

**One store per prefix.** The first backup of a store gives it a lineage, the
`backup.lineage` file in the data directory, and every restore point names it.
`mushroomdb restore --from` gives the store it restores a new lineage (see
[Restoring from the backup](#restoring-from-the-backup)), so back a restored
store up to a new prefix: point `MUSHROOMDB_BACKUP_S3_URL` for it somewhere
fresh. A store restored on boot is the one exception: it continues the prefix
it came from as a new lineage (see [Restoring on boot](#restoring-on-boot)).
The worker refuses the prefix in three cases:

- the prefix's newest restore point is another store's;
- this store has never been backed up but the prefix has restore points;
- this store holds fewer commits than that restore point. It was restored
  from an older backup.

On refusal it uploads nothing and deletes nothing, reports unhealthy with the
reason, and releases the lease. Use a new prefix for such a store. Retention
deletes only this lineage's objects. It never deletes the restore point it has
just written, the one `LATEST` names, or an object another lineage's restore
point names.

**What can go wrong, exactly:**

- **On a conditional service**, a stale holder's request can still complete:
  one the service had already received, or one sent by a process frozen
  mid-request that then resumes. Such a write is refused if it would overwrite
  an object or move `LATEST`. Otherwise it adds an object no current restore
  point names. The worst outcome is an orphaned object. Every restore point
  that `LATEST` and the kept manifests name still verifies.
- **On a best-effort service**, two writers overlapping until the next renewal
  can overwrite each other's objects. A restore point can then fail to
  verify. Run best-effort only where a single writer is otherwise assured.
- **A copy of a store made by hand** (its directory copied with `cp`, say,
  rather than restored) keeps the original's lineage. If it is older than the
  prefix's newest restore point and has since written more commits than that
  restore point holds, it passes the commit check. Its new restore points are
  its own and verify, but the prefix's other history is not removed. Restore
  with `mushroomdb restore`, which gives the copy a new lineage, or give such a
  copy a new prefix.

This was row 138 of the
[defect ledger](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/roadmap/v0.6.10-defects.md),
closed once both restore paths, `restore --from` and restore on boot, gave the
store they restore a new lineage. The hand-made copy above is what remains.

## Restoring from the backup

```text
mushroomdb restore --from s3://bucket/prefix [--at <commit | timestamp>] [--into <dir>]
```

`--from` is the `MUSHROOMDB_BACKUP_S3_URL` the store was backed up to
(`s3://`, `r2://` or `minio://`). Credentials and the endpoint come from the
same `AWS_*` variables backup uses. The store is written to `--into`, by
default `./restored`, which must not exist yet or be an empty directory (not a
mount point: restore into a directory inside one). A restore never writes over
anything.

Without `--at`, the newest restore point is restored: the one `LATEST` names,
plus the segments of its generation uploaded after it, up to the last
contiguous one. A restore of the newest backup loses at most the segment
cadence, not the manifest interval.

`--at` picks a point in time:

- `--at 75` restores the store as it was right after commit 75. Commits are
  numbered as `asof --commit` numbers them, from 0, and the restored store
  holds 76.
- `--at 2026-10-01T12:00:00Z` restores the last commit at or before that
  instant (RFC 3339), resolved through the backup's `commit_times.bin`, as a
  date query resolves it. A backup without that file cannot restore by time,
  and the command says so.
- The times come from the newest restore point's `commit_times.bin`. Segments
  uploaded after that restore point hold commits with no recorded time. An
  instant at or after the newest recorded time could fall after some of them,
  so it is refused with an error naming the last commit with a time, its index
  and when it was: restore an earlier instant, by commit index
  (`--at <commit index>`) instead, or without `--at` for the newest. An instant before
  the newest recorded time resolves as above.

A point in time is restored from the earliest restore point of the backup's
lineage that holds it, then cut back to that commit. The result is what
`asof --commit` shows at that commit, as an ordinary store that takes writes.
A commit is out of reach when it is below the oldest commit the backup holds
(the error names that commit), past the newest, or before a truncating snapshot
or archive retention cut the history it needs.

Before anything is written to `--into`, every file and segment is downloaded
into a staging directory beside it, a hidden `.<name>.restore-…` directory in
the same parent, and checked against its manifest: size, CRC-32, generation,
offsets, frames and lineage. Segments uploaded after the newest restore point
are named by no manifest. They are checked by their own frame checksums, size
and offsets, and the restore takes them up to the first one that is missing,
damaged or after a gap, and says so if it stops early. `commit_times.bin` is
trimmed to the commits restored, so the store stamps its own next commit. The
copy is then opened and must hold the commits the manifest says it does.

Every file is fsynced, and the staging directory then becomes `--into` in one
directory rename. So `--into` is always either as it was, absent or empty, or
the whole restored store, and never part of one, even after a crash.

- An existing empty `--into` is replaced by the staging directory, on every
  platform, so its permissions and ownership are not kept: the restored
  directory has the ones the restore created it with. On Unix the replacement
  is the rename itself. Elsewhere the empty directory is removed first; if the
  rename then fails, it is created again where possible, and the error says
  whether it was.
- Once the rename has happened the restore has succeeded. If syncing the
  parent directory afterwards fails, the command still succeeds and prints a
  warning: the store is in place, but its directory entry may not survive a
  power loss until the system syncs it.
- A restore that fails removes the staging directory. One killed outright
  leaves it behind, beside its lock file
  (`.<name>.restore-<pid>-<tag>.lock`), and it can hold a whole copy of the
  store. The next restore into the same `--into` removes such leftovers whose
  lock no running restore holds, and lists what it removed. Nothing else in
  the directory is touched.

If segments uploaded after the newest restore point exist but cannot be used
(the first one missing or damaged, or a later one after a gap), the restore of
the newest point still succeeds with what it can verify and warns, naming how
many segments and bytes of WAL were not restored and why.

`roles.json` and `keys.json` are restored as the chosen restore point holds
them. They are not cut back to a point in time: roles and keys are current
state, not history, as they are for `asof`.

**A restored store is a new lineage.** It gets a new `backup.lineage`, so its
backups can never be confused with the ones it came from. Pointed at the
prefix it was restored from, the backup worker refuses it: it uploads nothing
and deletes nothing there. Back it up to a new prefix.

## Restoring on boot

```text
MUSHROOMDB_RESTORE_FROM_S3=s3://bucket/prefix
```

Set on `serve`, this restores the newest restore point of the prefix into the
data directory when it holds no store, before the store is opened and before
the port is bound. A replaced machine or a new volume comes back with its data
without anyone running `mushroomdb restore`. Credentials and the endpoint come
from the `AWS_*` variables, and the lease settings
(`MUSHROOMDB_BACKUP_LEASE_TTL`, `MUSHROOMDB_BACKUP_LEASE_CONDITIONAL`,
`MUSHROOMDB_BACKUP_OP_TIMEOUT_MS`) from the same variables backup reads.
Usually it names the same prefix as `MUSHROOMDB_BACKUP_S3_URL`.

What happens at each start:

| The data directory | The prefix | What serve does |
| --- | --- | --- |
| Holds a store | (not contacted) | Logs that it skips the restore, and serves the store |
| Holds no store | Has a backup | Restores the newest restore point, then serves it |
| Holds no store | Holds no backup at all | Warns, and starts an empty store: a template's first deploy |
| Holds no store | Has a backup that cannot be restored | Exits 1 and serves nothing |

"Cannot be restored" covers a manifest or object that is missing, damaged or
partial, a refused credential, and a network failure that outlasts the
client's retries. The data directory is then left without a store, so the
next start tries again rather than serving an empty one. `restore-on-boot:`
lines on stderr say what happened, how many commits were restored from which
manifest, and how long it took.

An empty data directory may hold `lost+found` (a new ext4 volume has one), a
`LOCK` file, an empty `wal.bin`, and what killed restores into it left. Any
other file makes the restore refuse, naming it: move such files away. The
variable cannot be combined with `--restore-from` or `--demo-if-empty`, which
also fill an empty directory; serve refuses the combination at start.

**The lease comes first.** The restore takes the prefix's bucket lease (see
[the lease](#one-writer-per-backup-bucket-the-lease)) before it reads
anything, and renews it while it runs. If another instance holds a fresh
lease, it may still be writing: serve exits 1, naming the holder, and writes
nothing to the data directory. Stop that instance, or wait for its lease to
expire, and start again. When backup continues under the same prefix, the
lease stays renewed while the restored store is opened, however long that
takes. The backup worker then holds it and renews it at once, and never takes
it a second time. Otherwise the lease is released once the restore is done.
It is released as soon as anything fails before the worker holds it.

**The restored store continues the prefix as a new lineage.** Like any restore
it gets a new `backup.lineage`, and it also gets a `backup.parent` file naming
the lineage and restore point it came from. The backup worker uses the record
to continue the prefix the store came from. It does so only while `LATEST`
still names that same restore point. If another instance has written to the
prefix since, the prefix is refused, as any other store's would be. The new
lineage's segments, snapshots and manifests go under generation numbers of
their own, and `LATEST` moves to its first restore point once the store
commits anything new. The parent's restore points stay exactly as they were:
nothing of the parent lineage is overwritten, or deleted by retention. From
then on `restore --from` follows `LATEST` and restores the new lineage. The
parent's objects stay in the prefix, byte for byte, for a restore by hand.

**In place, all or nothing.** The data directory is usually a volume's mount
point. A mount point cannot be replaced by a rename, and its parent may not be
writable, so this restore stages inside the data directory, in a hidden
`.<name>.restore-…` directory. It verifies everything there first, as
`restore --from` does. It then writes `.mushroomdb-restore-install`, naming
the staged files with their sizes and modification times. It moves them into
the data directory (`snapshot.bin` and `wal.bin` last), syncs it, removes the
marker and syncs it again. The store is served only once that last sync has
succeeded. If it fails, serve exits 1: the next start serves the store, or
restores it again if a power loss brought the marker back. A failure partway
removes what it moved.

A crash leaves the marker behind. While it is there, nothing opens the
directory: `serve`, `mcp`, `query`, every other command, the library and the
Python binding all refuse it, naming the marker. The next start with the
variable set takes the lease first, then removes exactly the files the
marker names, and restores again. It never removes a store that has changed
since the install, which would mean it may have served writes. If a file the
marker names has another size or time, or a `snapshot.bin` or `wal.bin` it
does not name has appeared, serve exits 1, names the marker and removes
nothing. Delete the marker to keep that store, or empty the directory to
restore again.

Leftovers of killed restores inside the data directory are swept at the next
start once their lock is free and they are stale, also on a start that skips
the restore. Any the sweep has to leave are named on stderr.

**A large restore delays readiness.** The port is bound only once the store
is restored and open, so the platform's health check sees a process still
starting, never an empty store. The restore takes about as long as
downloading the backup: the newest snapshot, the archives it names, and the
WAL segments since. To measure it, time `mushroomdb restore --from <url>
--into /tmp/probe` from the same region. Then give the platform's start
window that time plus the store's open time, with a margin:

- **Fly.io:** `grace_period` of the `/ready` check in `fly.toml` is 60 s in
  the template, which is the most Fly accepts: flyctl lowers a longer value to
  60 s. A restore and open that take longer leave the check failing and the
  machine marked unhealthy until `/ready` answers. What Fly does with such a
  machine is confirmed at the release gate's Fly deploy.
- **Railway:** `healthcheckTimeout` in `railway.json` (120 s in the template).
- **Kubernetes:** a `startupProbe` on `/ready` whose `failureThreshold` times
  `periodSeconds` covers the restore.
- **Docker Compose:** the healthcheck's `start_period`, if you add one.
- **Render:** the deploy waits for `/ready`. A restore longer than Render's
  wait fails the deploy, so measure it first.

A start window that runs out kills a restore partway. That loses nothing:
the data directory holds no store, and the next start restores again from the
beginning.

## Stopping cleanly, and the grace period

On SIGTERM or SIGINT `serve` stops in three phases, all inside one deadline,
`--shutdown-timeout-secs` (default 55):

| Phase | What happens | Bound at the defaults |
| --- | --- | --- |
| 1. Drain | Stop accepting connections, let requests in flight finish, close live subscribers. The Bolt listener stops accepting too and closes each connection between requests, rolling back an open transaction. Whatever is still open at the bound is cut off. | 25 s |
| 2. Snapshot | Write the shutdown snapshot. If it has not finished by its bound it is skipped: the WAL holds every commit and the next open replays it. | 8 s (what is left) |
| 3. Backup | Tell the backup worker to stop. It uploads the last WAL segment and the new snapshot, writes a final restore point, and deletes the bucket lease (only if it still names this instance). | 22 s |

The arithmetic: the backup phase is reserved first. It is
`MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS` (10 s) plus one request deadline,
`MUSHROOMDB_BACKUP_OP_TIMEOUT_MS` (10 s), plus 2 s, which is 22 s. The drain
takes at most 25 s, or less if the deadline leaves less after that reserve. The
snapshot gets the rest: 55 - 25 - 22 = 8 s. A phase that finishes early gives its
time to the next one.

**The phases scale with `--shutdown-timeout-secs`.** With at least 55 s (more
goes to the snapshot) the split above holds. With less, all three phases shrink
in proportion, but the backup keeps a floor of 2 s and the drain gives way
first, because a stuck client is what stretches it. The backup's own flush grace
and request deadline are shrunk to fit its share, so the lease release still
has a request deadline of its own. At `--shutdown-timeout-secs 25`, for
example, the drain gets about 11 s, the snapshot about 3 s and the backup about
10 s. The whole sequence never exceeds the timeout.

Every phase runs even when an earlier one ran out, so the lease is released
whenever the bucket is reachable. If the bucket is unreachable the log says so,
the process exits, and the lease expires by itself after its TTL. No
acknowledged write is lost in any of these cases, because the WAL holds every
commit.

| Variable or flag | Default | Meaning |
| --- | --- | --- |
| `--shutdown-timeout-secs` | `55` | The whole shutdown must finish within this many seconds |
| `MUSHROOMDB_BACKUP_SHUTDOWN_GRACE_MS` | `10000` | Longest the final flush may take before the upload in flight is abandoned |
| `MUSHROOMDB_BACKUP_OP_TIMEOUT_MS` | `10000` | Deadline on each object-store request, including the lease release |

**A second SIGTERM or SIGINT exits at once.** An operator who presses Ctrl-C
twice on a slow shutdown gets an immediate exit. The log says that the backup
lease, if held, expires on its own after its TTL (at most 40 s until a
successor holds it).

**Set the platform's stop timeout above `--shutdown-timeout-secs`.** A platform
that sends SIGKILL earlier cuts the sequence short. Nothing is lost, but the
lease is left behind and the next start waits for it. The Fly, Compose and Railway templates use 60 s.
If you raise `--shutdown-timeout-secs`, raise the platform's value by the same
amount.

| Platform | Setting | Default | Template |
| --- | --- | --- | --- |
| Fly.io | `kill_timeout`, integer seconds, top level of `fly.toml` | 5 s, maximum 300 s | `60` |
| Docker Compose | `stop_grace_period` | 10 s | `60s` |
| Kubernetes | `terminationGracePeriodSeconds` | 30 | set it to at least `60` in your manifest |
| Render, with a persistent disk | none: the shutdown delay of a service with a disk cannot be customized | 30 s, fixed | `serve` is started with `--shutdown-timeout-secs 25` so it finishes inside the window |
| Render, without a disk | `maxShutdownDelaySeconds`, 1 to 300 | 30 s | not used by the template |
| Railway | `deploy.drainingSeconds` in `railway.json`, or the `RAILWAY_DEPLOYMENT_DRAINING_SECONDS` variable | unverified (not stated in its docs) | `60` |

Sources, read 2026-10-22:
[Fly.io configuration](https://docs.fly.io/reference/configuration),
[Render deploys](https://render.com/docs/deploys) and
[blueprint spec](https://render.com/docs/blueprint-spec),
[Railway config as code](https://docs.railway.com/config-as-code/reference).
Railway's default and maximum are not documented there, so they are
unverified. Compose and Kubernetes defaults are the tools' standard ones.

**On Render with a disk the window is a fixed 30 s**, shorter than the 55 s
default, so the Render template sets `--shutdown-timeout-secs 25`. Raising the
platform value is not possible there; if you drop the disk, you may set
`maxShutdownDelaySeconds` and a longer `--shutdown-timeout-secs`.

**Backup health never gates `/ready`.** `/ready` answers 503 only until the
store is open. A rolling deploy starts the new instance while the old one still
holds the lease, so the new one is backup-unhealthy until the old one stops. If
`/ready` followed backup health, the platform would never route traffic to the
new instance and never stop the old one. Watch backup health in the log (it
warns while the lease is foreign), not through `/ready`.

## Loopback-first posture

mushroomdb defaults to `127.0.0.1:8080` and refuses to bind a non-loopback
address without a token. This is intentional: the DB has no network stack of
its own and should be shielded by the OS network boundary whenever possible.
See [SECURITY.md](https://github.com/MatthewSherlin/mushroomdb/blob/main/SECURITY.md) for the full threat model and
vulnerability reporting process.
