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

Deleting the lease on a clean shutdown, so that a successor can start at once,
arrives with graceful shutdown (HO-11). Until then, a stopped holder's lease
expires after its TTL.

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
A store restored from the prefix keeps the lineage, and continues the backup
where it left off. The worker refuses the prefix in three cases:

- the prefix's newest restore point is another store's;
- this store has never been backed up but the prefix has restore points;
- this store holds fewer commits than that restore point. It was restored
  from an older backup.

On refusal it uploads nothing and deletes nothing, reports unhealthy with the
reason, and releases the lease. Use a new prefix for such a store. Retention
deletes only this lineage's objects. It never deletes the restore point it has
just written or the one `LATEST` names.

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
- **A store restored from an older backup that has since written more commits
  than the prefix's newest restore point** passes the commit check. Its new
  restore points are its own and verify. The prefix's older history is not
  removed, though, so give such a store a new prefix.

## Loopback-first posture

mushroomdb defaults to `127.0.0.1:8080` and refuses to bind a non-loopback
address without a token. This is intentional: the DB has no network stack of
its own and should be shielded by the OS network boundary whenever possible.
See [SECURITY.md](https://github.com/MatthewSherlin/mushroomdb/blob/main/SECURITY.md) for the full threat model and
vulnerability reporting process.
