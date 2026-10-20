# W4 — Runs anywhere: a finished self-host story

> Part of the v0.10 spec — see [`../v0.10-spec.md`](../v0.10-spec.md). Where this document and the master spec disagree, the master spec wins; its §4 lists every override.

**Unit id prefix: HO-**

---

## 1. Outcome

After 0.10 a general developer can:

- Deploy mushroomdb to Fly.io, Render, Railway, Docker Compose, or Kubernetes from a ready-made
  template with a single command or button — no hand-writing of YAML or toml.
- Enable continuous off-box backup to any S3-compatible endpoint (AWS S3, Cloudflare R2, MinIO,
  Backblaze B2, Tigris) by setting four env vars; the server uploads snapshot + WAL archives
  automatically and is never interrupted by a failed upload.
- Restore the latest backup — or any earlier commit — with `mushroomdb restore --from s3://bucket/prefix
  [--at <commit-seq>]` and bring up a byte-identical store.
- On ephemeral platforms (Fly machine restart, container recreated from scratch), the server detects
  an empty volume and restores from S3 before serving its first request — without operator
  intervention.
- Know that exactly one writer is active at all times: locally via the `LOCK` flock, remotely via a
  lease object in the bucket that a second instance checks before it starts uploading.

---

## 2. Current state

**Code evidence:**

- `Dockerfile:17` — builds with `--features embed-ui` only; `tls` feature is absent. Published image has no TLS support.
- `crates/server/Cargo.toml:18` — `tls = ["dep:axum-server"]` exists as an opt-in feature, not compiled by default.
- `crates/server/src/http.rs:558` — single `/health` route; no `/ready`. The internal `ready` oneshot (line 194) fires when the TCP listener binds, not when WAL replay completes — misleading name, wrong semantic for a readiness probe.
- `crates/server/src/http.rs:607–656` — middleware stack has no `CorsLayer`. `tower-http` is already a dep but the `cors` feature is not enabled (`crates/server/Cargo.toml`).
- `crates/server/src/lib.rs:29–32` — `AuthIdentity` enum has two variants: `Full` and `Role(String)`. No `Write` tier exists.
- `crates/server/src/http.rs:1421` — `/metrics` returns JSON only. No Prometheus text exposition. `tower-http` dep already present; `prometheus`/`metrics` crates are not.
- `crates/cli/src/main.rs:723` — graceful shutdown: SIGTERM → drain → snapshot → exit. No S3 flush step; no lease release.
- `crates/core-storage/src/fs.rs:81–104,255` — advisory `LOCK` flock enforces single-writer locally. No remote lease mechanism.
- `crates/server/src/http.rs:2815–2845` — `POST /backup` writes to local filesystem only. `--restore-from` reads from local filesystem only. No S3 code anywhere.
- `docker-compose.yml` — single service, single volume, no backup sidecar.
- No `fly.toml`, `render.yaml`, `railway.json`, or K8s manifests exist anywhere in the repo.
- `crates/core-storage/src/fs.rs:440–471` — `wal.N.archive` files are immutable once written; natural streaming unit for continuous backup.
- `crates/cli/src/main.rs:506–563` (run_serve) — `--snapshot-every` triggers `snapshot_automatically()` periodically; graceful shutdown snapshots unconditionally. Both are the correct hook points for post-snapshot S3 upload.
- `docs/site/service.md` — comprehensive local runbook; no hosted-backup or S3 section.

**Recon accuracy:** The recon's claim about `MUSHROOMDB_BACKUP_DIR` defaulting to server CWD is correct (`crates/cli/src/lib.rs:265`). The claim about graceful shutdown having a 30s TLS grace period is confirmed (`http.rs:521`). The description of `--restore-from` seeding an empty store is confirmed (`crates/cli/src/lib.rs:163–179`). The recon correctly identified no deploy templates exist. The recon recommended exec-hook (C1); the brief overrides this — native S3 is in scope, exec-hook is the fallback in Decisions.

---

## 3. Design

### 3.1 Continuous backup to S3-compatible storage

#### Architecture

*Revised per master spec §4.4: the WAL is shipped continuously, not only at snapshots.*

A background Tokio task (`BackupWorker`) runs alongside the serve loop and does three things:

1. **Ships WAL segments.** On a short cadence (`MUSHROOMDB_BACKUP_WAL_INTERVAL_MS`, default 1000),
   it reads the bytes appended to `wal.bin` since the last upload — only bytes already fsynced
   locally, up to the last complete frame — and uploads them as one immutable object,
   `{prefix}/wal/{generation}/{start:020}-{end:020}`. A segment never contains a partial frame.
   Nothing is uploaded when nothing was appended.
2. **Ships snapshots and archives when they are written.** It receives a notification on a
   `watch::Sender<BackupTrigger>` each time `snapshot_automatically()`, an explicit snapshot or the
   graceful-shutdown snapshot completes (after the atomic rename). It uploads the new
   `snapshot.bin`, any new `wal.N.archive` (immutable; never re-uploaded) and the small side files
   (`commit_times.bin`, `roles.json`, `keys.json`, `wal.genesis`, `wal.floor`). A truncating
   snapshot replaces `wal.bin`; the worker starts a new `generation` (a counter stored in the
   manifest) so segment offsets restart at 0 without colliding.
3. **Writes manifests.** After each snapshot upload, and every `MUSHROOMDB_BACKUP_MANIFEST_INTERVAL_S`
   (default 60) while segments accumulate, it writes `{prefix}/manifest/{commit_seq:020}.json`
   naming one consistent restore point — the snapshot, the archives after it, the WAL generation
   and the ordered segment list — with sizes and SHA-256 of each, then overwrites `{prefix}/LATEST`
   with that key. Retention (`MUSHROOMDB_BACKUP_RETAIN_N` manifests) deletes manifests outside the
   window and any snapshot, archive or segment no retained manifest references.

The upload loop must **never block the write path**. The `BackupWorker` runs in its own task, reads files through standard OS buffered I/O (not through the store's internal state), and communicates failure only via the health endpoint and metrics counter — it never panics the serve loop.

Failed uploads retry with exponential back-off (1s, 2s, 4s … 60s cap) up to `MUSHROOMDB_BACKUP_RETRY_LIMIT` (default: indefinite). After three consecutive failures the `/health` extended response includes `"backup_lag_s": N` and `"backup_ok": false`. Prometheus metric: `mushroomdb_backup_upload_errors_total`.

#### User-facing configuration

```
# Required for S3 backup:
MUSHROOMDB_BACKUP_S3_URL=s3://my-bucket/prefix      # or r2://..., minio://, etc.
AWS_ACCESS_KEY_ID=…
AWS_SECRET_ACCESS_KEY=…
# Optional — override endpoint for non-AWS S3-compatible storage:
AWS_ENDPOINT_URL=https://…                           # MinIO, R2, B2, Tigris
AWS_REGION=us-east-1                                 # default: us-east-1

# Backup retention (count of most-recent manifests to keep; 0 = keep forever):
MUSHROOMDB_BACKUP_RETAIN_N=7

# Encryption: SSE-S3 (server-side, AWS-managed key). Default: off.
MUSHROOMDB_BACKUP_SSE=aws:kms                        # or AES256, or unset

# Restore on boot from S3 when the store directory is empty:
MUSHROOMDB_RESTORE_FROM_S3=s3://my-bucket/prefix

# Remote lease TTL (seconds); another instance refuses to write if lease is fresher:
MUSHROOMDB_BACKUP_LEASE_TTL=30
```

#### RPO and "durable" definition

A commit is **locally durable** when it is fsynced to `wal.bin` (unchanged). It is **off-box
durable** when the segment holding it is uploaded: the recovery point objective is the segment
cadence (default 1 s) plus upload time, independent of `--snapshot-every`. `/health` reports
`backup_lag_s` as the age of the oldest fsynced byte not yet uploaded. The docs state both
definitions and the measured lag under sustained writes.

#### Restore CLI

```
mushroomdb restore \
  --from s3://bucket/prefix \
  [--at <commit-seq | ISO-8601 timestamp>] \
  [--into <local-dir>]      # default: ./restored
```

Steps:
1. Read `{prefix}/LATEST` to find the most-recent manifest, or list `{prefix}/manifest/` for the latest manifest at or before the `--at` target.
2. Download the manifest's snapshot and side files, the archives it names, and its segments; concatenate the segments of its generation, in offset order, into `wal.bin` (each segment ends on a frame boundary, so the result is a valid WAL).
3. With `--at`, the restored store is opened through the existing `open_at` path at the target commit and written out as a normal store; without it, the WAL is used as is. Segments uploaded after the newest manifest are included up to the last contiguous one, so a restore of "latest" loses at most the segment cadence.
4. Verify SHA-256 of each file against the manifest. Fail loudly on mismatch.
5. Place files into `--into` dir, then call the existing `restore_from` logic to stage-copy into the store dir.

**PITR decision:** 0.10 ships commit-based PITR (`--at <commit-seq>`). Timestamp PITR (`--at 2026-10-01T12:00:00Z`) resolves to a commit via `commit_times.bin` using the existing timestamp-to-commit mapping (same as `asof` query resolution). If `commit_times.bin` is not in the backup, timestamp PITR is unavailable and the CLI says so.

#### Restore-on-boot

When `MUSHROOMDB_RESTORE_FROM_S3` is set and the store directory is empty (no `snapshot.bin` and no `wal.bin`):

1. Download the latest backup (same as `restore --from` without `--at`).
2. Stage-copy into the store dir using the existing `restore_from` path.
3. Proceed with normal open + WAL replay.
4. If restore fails, exit non-zero (do not start serving a blank store).

The bucket lease (HO-08) is checked **before** restore completes, so a second instance that finds an active lease does not restore and does not start writing.

#### Integrity and manifest schema

```json
{
  "version": 1,
  "created_at": "2026-10-19T12:00:00Z",
  "commit_seq": 4712,
  "files": [
    { "key": "prefix/snapshot.bin",       "size": 18432000, "sha256": "…" },
    { "key": "prefix/archives/wal.4700.archive", "size": 512000, "sha256": "…" },
    { "key": "prefix/commit_times.bin",   "size": 2048,     "sha256": "…" },
    { "key": "prefix/roles.json",         "size": 312,      "sha256": "…" },
    { "key": "prefix/wal.genesis",        "size": 0,        "sha256": "…" },
    { "key": "prefix/wal.floor",          "size": 8,        "sha256": "…" }
  ],
  "wal_generation": 12,
  "segments": [
    { "key": "prefix/wal/12/00000000000000000000-00000000000000065536", "sha256": "…" }
  ]
}
```

`LATEST` object contains the manifest key as a plain UTF-8 string (not JSON, not a redirect).

#### Cost awareness

- WAL segments: at most one PUT per `MUSHROOMDB_BACKUP_WAL_INTERVAL_MS` while writes happen, none while idle (86,400/day at 1 s under continuous writes; segments are small).
- `snapshot.bin` is uploaded only when a snapshot is written, not on a timer.
- Archive files are uploaded once and never re-uploaded (immutability check at step 1 above).
- Manifest: one small PUT per cycle.
- Listing: one LIST per cycle (to find new archives). List costs are ~10x PUT costs but still negligible at this scale.
- Retention deletes: N-RETAIN_N DELETEs per cycle. Bounded.
- Outbound bandwidth: WAL bytes once each, plus one full snapshot per snapshot written. Because the RPO no longer depends on snapshots, large stores can snapshot hourly or less.

### 3.2 Bucket lease — remote single-writer lock

Object key: `{prefix}/LEASE`

Contents:
```json
{
  "instance_id": "<uuid-v4-generated-at-startup>",
  "acquired_at": "2026-10-19T12:00:00Z",
  "renewed_at": "2026-10-19T12:00:05Z",
  "ttl_s": 30
}
```

On `BackupWorker` startup:
1. GET `{prefix}/LEASE`. If present and `now - renewed_at < ttl_s` and `instance_id != ours` → log a fatal error, refuse backup (set `backup_ok: false`, alert operator). Do not crash the local serve process — it can still serve from local disk.
2. Write (PUT) the LEASE with our `instance_id` and current timestamp.
3. Renew every `ttl_s / 3` seconds with a conditional PUT (ETag check to detect a race).
4. On shutdown: DELETE the LEASE.

**This is advisory, not a distributed mutex.** Two instances that start simultaneously may both pass the check. The goal is to catch the common operator error (deploy to two machines sharing one backup bucket). Split-brain protection: the manifest + archive files are immutable once written; a second writer would upload a new manifest that supersedes the old one. The store with the stale LEASE's uploads are orphaned but not corrupted (they remain valid restorable points). Documenting this limitation clearly.

Note: S3 does not support conditional writes universally (R2 does; MinIO does; plain S3 does via `If-None-Match`). The lease uses a last-writer-wins upload with ETag validation — it is not a strong distributed lock. Document this clearly.

### 3.3 TLS in the published image

`Dockerfile:17` changes:
```
cargo build -p mushroomdb-cli --bin mushroomdb --features embed-ui,tls --release
```

No other Dockerfile changes. Binary is ~2–3 MB larger. The `--tls-cert` and `--tls-key` CLI flags already work; they just need the feature compiled in.

Env vars to add (12-factor, mapping existing CLI flags):
```
MUSHROOMDB_TLS_CERT=/path/to/cert.pem
MUSHROOMDB_TLS_KEY=/path/to/key.pem
```

Bolt TLS (W3 dependency): if the Bolt listener is active (`MUSHROOMDB_BOLT_ADDR` set; see W3) and TLS is configured, wrap the Bolt TCP listener with the same `RustlsConfig`. CLI: `--tls-cert` / `--tls-key` apply to both listeners. Operators wanting separate certs get separate flags (`--bolt-tls-cert`, `--bolt-tls-key`); this is a builder's call (recommend: share the cert by default, separate flags available).

ACME auto-cert (`rustls-acme`): OUT of 0.10. The reverse proxy path (Caddy, nginx with certbot) covers auto-cert without a new crate dep. Document it in the self-host guide.

### 3.4 GET /ready — readiness endpoint

`AppState` gains `ready: Arc<AtomicBool>` (initially `false`). After `run_serve` completes WAL replay and `open()` returns, the flag is set to `true`. The `serve_inner` function currently fires the `ready` oneshot after `bind()` (line 424) — this is wrong for readiness purposes (WAL replay happens before bind, during `open()`). The fix: add a `pre_serve_hook` callback or simply set the `Arc<AtomicBool>` from the call site in `main.rs` after `open()` and before spawning the server.

Routes:
```
GET /health   → 200 always (liveness: process is up)
GET /ready    → 200 once ready; 503 {"ok":false,"reason":"replaying"} before
```

The existing `/health` behavior is unchanged. Both routes remain anonymous (no token required).

Kubernetes liveness probe: `GET /health`. Kubernetes readiness probe: `GET /ready`. Fly health check: `GET /ready` (with `grace_period` set to accommodate large replays; document `--grace-period 120` in fly.toml).

### 3.5 CORS

`tower-http`'s `cors` feature (already a dep, just the feature flag). New env var:
```
MUSHROOMDB_CORS_ORIGINS=https://app.example.com,https://admin.example.com
# or:
MUSHROOMDB_CORS_ORIGINS=*
```

Default: unset → no `Access-Control-*` headers (current behavior preserved). Parsed at startup into a `CorsLayer` inserted before the auth middleware in `build_router`. Methods allowed: `GET, POST, PUT, DELETE, OPTIONS`. Headers: `Authorization, Content-Type, x-request-id`. Credentials: allowed when origins are not `*`.

### 3.6 Write-tier access keys

`AuthIdentity` in `crates/server/src/lib.rs:29` gains a third variant:

```rust
pub(crate) enum AuthIdentity {
    /// Full-access token (admin: all routes including /backup, /stats, /metrics).
    Full,
    /// Write-tier token: can mutate nodes/edges/rules/ingest; denied /backup, /stats, /metrics, /watch, /subscribe.
    Write,
    /// Role-bound token: read-only, visibility-masked.
    Role(String),
}
```

`keys.json` sidecar (like `roles.json`, placed alongside the store):
```json
{
  "keys": [
    { "token": "tok-abc", "tier": "admin" },
    { "token": "tok-xyz", "tier": "write" },
    { "token": "tok-def", "tier": "read", "role": "tenant_a" }
  ]
}
```

- `admin` = current `Full` behavior.
- `write` = new `Write` variant: all mutation routes (`/ingest`, `/nodes`, `/edges`, `/rules`, `/node/{key}/prop/*`, etc.); denied `/backup`, `/stats`, `/metrics`, `/watch`, `/subscribe`.
- `read` with optional `role` = current `Role(name)` behavior.

`MUSHROOMDB_TOKEN` (env var) remains the admin token. `MUSHROOMDB_ROLE_TOKENS` remains for backward compat. `keys.json` is an additive source merged at startup; SIGHUP reloads it without restart.

The `auth_middleware` reads `keys.json` from the store directory. If it does not exist, fall back to `MUSHROOMDB_TOKEN` + `MUSHROOMDB_ROLE_TOKENS` only (backward compat).

### 3.7 Prometheus /metrics

Content-type negotiation on the existing `/metrics` route:
- `Accept: application/json` (or no Accept) → current JSON response (backward compat).
- `Accept: text/plain` → Prometheus text format (`Content-Type: text/plain; version=0.0.4`).

Metrics exposed (same as current JSON, reformatted):
```
mushroomdb_nodes_live
mushroomdb_edges_live
mushroomdb_commit_seq
mushroomdb_wal_size_bytes
mushroomdb_rss_bytes
mushroomdb_uptime_seconds
mushroomdb_slow_queries_total
mushroomdb_backup_upload_errors_total   # new (HO-07)
mushroomdb_backup_last_success_timestamp_seconds  # new (HO-07)
```

Implementation: a `format_prometheus()` function in `http.rs` that serializes the existing `MetricsSnapshot` struct into the Prometheus text format. No registry / global state needed — we own all the metrics. This avoids the `prometheus` crate entirely if we write the serializer ourselves (see Dependencies).

### 3.8 Structured JSON logs

`tracing-subscriber` with `json` feature (already a dep for `EnvFilter`; JSON formatting is a feature flag). Gated by env var:
```
MUSHROOMDB_LOG_FORMAT=json    # structured JSON; default: text (human-readable)
```

JSON log entry schema (standard tracing-subscriber/fmt JSON):
```json
{"timestamp":"2026-10-19T12:00:00.123Z","level":"INFO","target":"server","fields":{"method":"POST","path":"/query","status":200,"duration_ms":4,"request_id":"abc123"}}
```

No format changes to the store. Only affects stderr output.

### 3.9 Graceful shutdown with S3 flush and lease release

The existing shutdown sequence in `crates/cli/src/main.rs:723`:
```
SIGTERM → stop accepting → drain in-flight → snapshot → exit
```

With S3 backup, extended to:
```
SIGTERM → stop accepting → drain in-flight → snapshot → flush pending S3 uploads → DELETE LEASE → exit
```

The flush step: send a final `BackupTrigger::Shutdown` on the channel, wait for `BackupWorker` to finish (with a timeout: 30s for non-TLS, existing 30s TLS grace covers it). If it times out, log a warning and exit anyway — data is not lost (the local snapshot is on disk; the S3 gap will self-heal on next boot if restore-on-boot is configured).

### 3.10 Deploy templates

All templates go in a new `deploy/` directory at the repo root.

#### `deploy/fly/fly.toml`
```toml
app = "mushroomdb"
primary_region = "iad"

[build]
  image = "ghcr.io/<owner>/mushroomdb:latest"

[env]
  # Set via fly secrets set MUSHROOMDB_TOKEN=…
  # MUSHROOMDB_RESTORE_FROM_S3=s3://bucket/prefix

[[services]]
  protocol = "tcp"
  internal_port = 8080
  [[services.ports]]
    port = 443
    handlers = ["tls", "http"]
  [[services.ports]]
    port = 80
    handlers = ["http"]
  [services.concurrency]
    type = "requests"
    hard_limit = 256
    soft_limit = 200
  [[services.http_checks]]
    interval = "10s"
    timeout = "5s"
    grace_period = "120s"   # allow WAL replay
    path = "/ready"

# Bolt (W3) — uncomment when W3 ships:
# [[services]]
#   protocol = "tcp"
#   internal_port = 7687
#   [[services.ports]]
#     port = 7687
#     handlers = ["tls"]

[[mounts]]
  source = "mushroomdb_data"
  destination = "/data"

# CRITICAL: replicas = 1 is required. mushroomdb is single-writer; two machines
# sharing one volume will both fail to acquire the write lock. Two machines
# sharing one backup bucket must not both be writers (see LEASE).
[deploy]
  strategy = "immediate"    # rolling deploy fails with a single-volume app
```

Fly secrets to document: `MUSHROOMDB_TOKEN`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `MUSHROOMDB_BACKUP_S3_URL` (Tigris bucket on Fly is the recommended pairing).

#### `deploy/render/render.yaml`
```yaml
services:
  - type: web
    name: mushroomdb
    runtime: image
    image:
      url: ghcr.io/<owner>/mushroomdb:latest
    disk:
      name: mushroomdb-data
      mountPath: /data
      sizeGB: 10
    healthCheckPath: /ready
    envVars:
      - key: MUSHROOMDB_TOKEN
        sync: false
      - key: MUSHROOMDB_BACKUP_S3_URL
        sync: false
      - key: AWS_ACCESS_KEY_ID
        sync: false
      - key: AWS_SECRET_ACCESS_KEY
        sync: false
# NOTE: Set replicas to 1 in the Render dashboard. Multiple replicas sharing
# one disk = write-lock contention.
```

#### `deploy/docker-compose/docker-compose.yml`
```yaml
services:
  mushroomdb:
    image: ghcr.io/<owner>/mushroomdb:latest
    ports:
      - "8080:8080"
      # - "7687:7687"   # Bolt — uncomment when W3 ships
    volumes:
      - mushroomdb-data:/data
    environment:
      MUSHROOMDB_TOKEN: ${MUSHROOMDB_TOKEN:?set a strong token}
      MUSHROOMDB_BACKUP_S3_URL: ${MUSHROOMDB_BACKUP_S3_URL:-}
      AWS_ACCESS_KEY_ID: ${AWS_ACCESS_KEY_ID:-}
      AWS_SECRET_ACCESS_KEY: ${AWS_SECRET_ACCESS_KEY:-}
      AWS_ENDPOINT_URL: http://minio:9000
      AWS_REGION: us-east-1
      MUSHROOMDB_RESTORE_FROM_S3: ${MUSHROOMDB_RESTORE_FROM_S3:-}
    command: ["serve", "/data", "--addr", "0.0.0.0:8080", "--snapshot-every", "300"]
    depends_on:
      minio:
        condition: service_healthy

  # Local S3-compatible storage for development / self-hosted backup.
  # In production, replace with AWS S3 / R2 / Tigris and remove this service.
  minio:
    image: minio/minio:latest
    ports:
      - "9000:9000"
      - "9001:9001"
    volumes:
      - minio-data:/data
    environment:
      MINIO_ROOT_USER: ${MINIO_ROOT_USER:-minioadmin}
      MINIO_ROOT_PASSWORD: ${MINIO_ROOT_PASSWORD:-minioadmin}
    command: server /data --console-address ":9001"
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:9000/minio/health/live"]
      interval: 5s
      timeout: 5s
      retries: 5

volumes:
  mushroomdb-data:
  minio-data:
```

#### `deploy/k8s/statefulset.yaml`
StatefulSet (replicas: 1, hard-comment warning against scaling), PVC (ReadWriteOnce), Service (ClusterIP for HTTP + Bolt), ConfigMap for non-secret env vars, note to use a Secret for tokens. Liveness probe: `/health`. Readiness probe: `/ready` with `initialDelaySeconds: 60`. No HPA (scaling breaks single-writer constraint; note in comments). Bolt port (7687) in containerPort + Service, commented until W3 ships.

#### `deploy/railway/railway.json`
Minimal Railway template pointing to the published image with env var prompts.

### 3.11 CI validation of templates

A new GH Actions job `hosting-templates` (runs on every PR touching `deploy/**` or `Dockerfile`):

1. **Dockerfile build check**: `docker build .` (no push). Verifies TLS compiles.
2. **Fly template lint**: `flyctl config validate deploy/fly/fly.toml` (using pinned `flyctl` action).
3. **render.yaml schema**: `npx @render-oss/render-cli validate deploy/render/render.yaml` or equivalent.
4. **K8s dry-run**: `kubectl apply --dry-run=client -f deploy/k8s/statefulset.yaml`.
5. **MinIO smoke test**: a GH Actions workflow with `services:` MinIO container; starts mushroomdb with `MUSHROOMDB_BACKUP_S3_URL=s3://test-bucket/db`, performs writes, triggers a snapshot, verifies the manifest appears in MinIO, then runs `mushroomdb restore --from s3://test-bucket/db` into a temp dir and verifies the restored store opens cleanly.

The smoke test uses the same docker-compose MinIO image as the template, so template and CI are validated together.

---

## 4. Interaction with differentiators

**Rules engine (derived edges):** Backup captures the full store state post-derivation. On restore, WAL replay re-derives edges exactly as on a normal open. Rules are stored in the WAL; `roles.json` (which holds visibility constraints) is included in every backup manifest. No special handling needed.

**Time travel / as-of:** Archives are the WAL segments that `asof` queries replay into. Backing them up is exactly what enables remote PITR. Restoring with `--at <commit-seq>` downloads archives only up to that seq and uses the existing `open_at` path — no format change. One constraint: an `--at` target earlier than the oldest archive in the backup is not restorable (the manifest records the oldest retained commit; the CLI errors loudly if the target is before it).

**Scoped visibility:** `roles.json` is in every backup manifest. A restore includes it, so role-based visibility works identically after restore. The new `keys.json` sidecar is also included in backups (added to the file list in `BackupWorker`).

**Snapshots + WAL replay:** The backup is built on the snapshot as the base. The `snapshot.intent` crash-window file (row 92) must NOT be included in backups — its presence in a restored store would indicate an interrupted snapshot that didn't happen on the restored machine, which would corrupt the open logic. `BackupWorker` explicitly skips `snapshot.intent`. A backup captured mid-snapshot is safe because: `snapshot_automatically()` is atomic from the Fs layer's perspective (rename-based); the trigger channel fires only after the atomic rename completes, so the backup worker never sees a partial snapshot.

**Full-text / vector indexes:** Since row 36 (`snapshot.bin` carries the full-text index), restoring `snapshot.bin` restores the full-text index. Vector ANN index reconstruction on WAL replay still applies to commits since the last snapshot. No extra backup artifact needed.

**MCP tools:** MCP serves graph data from the running store. After restore, the store is byte-identical in behavior. No action needed; the MCP layer is stateless over the store.

---

## 5. Compatibility & migration

**Store format:** No changes. `snapshot.bin`, WAL, archives are unchanged. Old 0.9.x stores open in 0.10 normally.

**Downgrade:** A 0.10 store that has used `keys.json` can be opened by 0.9.x (the file is read only by the server, not by the storage layer). The only downgrade concern is if 0.10 introduces a snapshot format bump elsewhere in W1/W2 — that is those workstreams' concern; W4 does not touch format.

**API breaks:**
- `GET /metrics` behavior changes: with `Accept: text/plain`, Prometheus format is returned instead of the old behavior of returning JSON (old behavior: JSON regardless of Accept). No existing clients use content-type negotiation on this endpoint (it was full-token only, no public contract). Mitigation: default continues to return JSON; only changes when `Accept: text/plain` is sent.
- `GET /health` is unchanged.
- `GET /ready` is a new endpoint (additive, no break).
- `AuthIdentity::Write` is a new internal enum variant. No public API break (internal to the server crate).

**Migration notes:**
- Operators using `MUSHROOMDB_ROLE_TOKENS` continue to work unchanged. `keys.json` is opt-in.
- The `MUSHROOMDB_BACKUP_DIR` env var continues to work for local backup.
- No migration needed for existing stores.

---

## 6. Verification

### Unit tests

| Scope | What to test |
|---|---|
| `BackupWorker` | Upload triggered after snapshot; archive dedup (no re-upload of existing archives); manifest schema; retention delete logic; retry on upload failure; health flag set on consecutive failures |
| Bucket lease | Startup check: stale lease → permit; fresh foreign lease → refuse write; own renewed lease → permit; shutdown delete |
| `mushroomdb restore --from s3://` | Manifest download + SHA-256 verify; PITR archive subset selection; `--at` before oldest archive → clean error; `--at` by timestamp → commit resolution |
| Restore-on-boot | Empty dir → restore triggered; non-empty dir → no restore; restore failure → non-zero exit |
| `/ready` | Returns 503 before open completes; 200 after; no token required |
| CORS | `MUSHROOMDB_CORS_ORIGINS=*` → headers present; unset → no headers; preflight OPTIONS → 200 |
| Write-tier keys | `Write` token: mutation routes succeed; `/backup`, `/stats`, `/metrics` return 403; WebSocket returns 403. `keys.json` reload on SIGHUP |
| Prometheus metrics | `Accept: text/plain` → valid Prometheus text format; `Accept: application/json` → JSON (unchanged) |
| TLS Dockerfile | Integration test: build image with `--features tls`; start with self-signed cert; HTTPS request succeeds |

### Integration / differential probes

- **Backup → restore round-trip**: write N nodes → snapshot → upload to MinIO (test container) → restore into fresh dir → open → query → assert node count matches. Run this for: `snapshot.bin` only; `snapshot.bin` + one archive; multi-archive case.
- **PITR**: write 100 commits → snapshot (archive 1) → write 50 more commits → snapshot (archive 2) → restore `--at <commit-seq within archive 1>` → assert only first 100 commits present.
- **Restore-on-boot + bucket lease**: start two instances pointing at the same bucket (MinIO); second should refuse to upload. After first exits and deletes lease, second should succeed.
- **Crash during backup upload**: inject network failure mid-upload; verify local store is unaffected; verify retry logic; verify health shows `backup_ok: false`.
- **Graceful shutdown flush**: SIGTERM → verify LEASE deleted from MinIO and final manifest uploaded.

### Performance gates (vs v0.9.x)

| Metric | Gate |
|---|---|
| Open time (large store, no S3 configured) | ≤ v0.9.x + 5% |
| First write latency (no S3 configured) | ≤ v0.9.x + 1% |
| Write throughput during background upload | ≤ 5% degradation vs no-upload baseline |
| `/ready` response time after open | < 1 ms (after ready flag set) |

BackupWorker must prove it doesn't hold a lock during upload: confirm via a concurrent write benchmark that throughput is unaffected.

### Docs checks

- `deploy/fly/fly.toml`: `flyctl config validate` passes in CI.
- `deploy/k8s/`: `kubectl apply --dry-run=client` passes in CI.
- Each template includes a `# SINGLE WRITER — replicas must be 1` comment where relevant.
- Self-host guide (`docs/site/self-host.md`) covers: volume, backup, restore, PITR, TLS (reverse proxy + direct), access keys, `/ready` probe config, RAM sizing formula.

### RAM sizing formula

From `docs/performance.md` and measured numbers (to be confirmed in W4 benchmarks):
```
RAM ≈ graph_size_on_disk × 1.5  (full-text index adds ~0.2× overhead since row 36)
```
Document in self-host guide with example numbers from published benchmarks (W4 deliverable; cross-reference W4-bench if that workstream exists).

---

## 7. Build units

| ID | Description | Size | Files / crates touched | Depends on | Conflict hotspots |
|---|---|---|---|---|---|
| HO-01 | Add `tls` feature to Dockerfile build | S | `Dockerfile:17` | — | Dockerfile |
| HO-02 | `GET /ready` readiness endpoint | S | `crates/server/src/http.rs`, `lib.rs`, `crates/cli/src/main.rs` | — | `http.rs:558`, `lib.rs` AppState |
| HO-03 | CORS config via `MUSHROOMDB_CORS_ORIGINS` | S | `crates/server/src/http.rs`, `crates/server/Cargo.toml` | — | `http.rs:607`, Cargo.toml features |
| HO-04 | Write-tier keys (`keys.json` + `AuthIdentity::Write`) | S | `crates/server/src/lib.rs`, `http.rs`, `crates/cli/src/lib.rs` | — | `lib.rs:29` AuthIdentity enum — hot conflict with any unit touching auth |
| HO-05 | Prometheus text format on `GET /metrics` | M | `crates/server/src/http.rs` | HO-04 (auth check for Write tier) | `http.rs:1421` |
| HO-06 | Structured JSON logs (`MUSHROOMDB_LOG_FORMAT`) | S | `crates/cli/src/main.rs`, `Cargo.toml` | — | `main.rs` subscriber init |
| HO-07 | `BackupWorker` (master §4.4): continuous WAL-segment shipping, snapshot + archive upload on snapshot, manifests with generations, retention | L | new `crates/backup/` crate, `crates/cli/src/main.rs` | HO-01 (image must exist to test) | `main.rs:506` run_serve — hot conflict with HO-08, HO-09, HO-11 |
| HO-08 | Bucket LEASE for remote single-writer | M | `crates/backup/` | HO-07 | `crates/backup/` |
| HO-09 | `mushroomdb restore --from s3://` CLI | M | `crates/cli/src/lib.rs`, `main.rs` | HO-07 | `crates/cli/src/lib.rs` Command enum |
| HO-10 | Restore-on-boot from S3 (`MUSHROOMDB_RESTORE_FROM_S3`) | S | `crates/cli/src/main.rs` | HO-07, HO-08 | `main.rs:506` |
| HO-11 | Graceful shutdown: S3 flush + LEASE delete | S | `crates/cli/src/main.rs` | HO-07, HO-08 | `main.rs:723` shutdown sequence |
| HO-12 | Deploy templates: fly.toml, render.yaml, docker-compose+MinIO, railway.json | M | new `deploy/` directory | HO-01, HO-02 | — |
| HO-13 | K8s StatefulSet manifest | M | new `deploy/k8s/` | HO-01, HO-02 | — |
| HO-14 | CI validation: lint + MinIO smoke test | M | `.github/workflows/hosting-templates.yml` | HO-07, HO-12, HO-13 | CI workflows — potential conflict with W1/W2 CI additions |
| HO-15 | Bolt port in deploy templates (W3 dependency) | S | `deploy/fly/fly.toml`, `deploy/k8s/statefulset.yaml`, `docker-compose.yml` | HO-12, HO-13, **W3 Bolt listener unit** | deploy/ |
| HO-16 | Self-host guide and docs | S | `docs/site/self-host.md` (new), `docs/site/SUMMARY.md` | HO-01 through HO-15 | SUMMARY.md — possible conflict with W3 docs |

**Parallel build opportunities:** HO-01, HO-02, HO-03, HO-04, HO-06 have no dependencies on each other and can be built in parallel. HO-07, HO-08, HO-09 form a sequential chain but HO-07 can start in parallel with HO-01..HO-06. HO-12 and HO-13 are independent of the backup chain and can be built in parallel once HO-01 + HO-02 land.

---

## 8. Decisions

### (a) Builder's call

1. **Encryption: SSE-S3 vs client-side.** Recommend SSE-S3 (`x-amz-server-side-encryption: aws:kms` or `AES256`). Zero key management complexity for the operator; adequate for most self-hosting scenarios. Client-side encryption requires key management out of scope for 0.10. Builder may settle.

2. **Prometheus endpoint: content-type negotiation vs new route.** Recommend content-type negotiation on existing `/metrics` (backward compat; less surface area). A builder may prefer a separate `/metrics/prometheus` route if they believe existing JSON clients will pass unusual Accept headers. Either is fine.

3. **PITR scope in 0.10: commit-based PITR only.** Timestamp-based PITR uses `commit_times.bin` and is included if the file is in the backup. If it's absent, the CLI logs a clear message. Builder's call on the error UX.

4. **Bolt TLS: shared cert vs separate cert flags.** Recommend sharing the same `--tls-cert`/`--tls-key` for both HTTP and Bolt listeners by default, with optional `--bolt-tls-cert`/`--bolt-tls-key` overrides. Builder may settle.

5. **`keys.json` location.** Recommend the store directory (alongside `roles.json`). An alternative is a separate `--keys-file` flag. Builder's call; store-directory colocation is consistent with existing `roles.json` pattern.

6. **Helm chart.** Recommend OUT of 0.10. StatefulSet YAML + README is sufficient for the operators deploying 0.10. Helm adds maintenance surface without proportional value at this stage.

7. **`mushroomdb init-deploy <platform>` CLI generator.** Recommend OUT of 0.10. Static templates + a self-host guide are sufficient. A generator adds scope and becomes stale if templates change.

8. **`crates/backup/` as a separate crate vs in-crate module.** Recommend a new `crates/backup/` crate (clean dependency boundary; `object_store` dep is isolated; can be tested independently). A builder may choose to put it in `crates/cli/` instead.

### (b) Owner must decide

1. **Native S3 vs exec-hook (backup strategy):**
   The brief asks for native S3 in 0.10. The recon recommended exec-hook only. The owner should confirm:
   - **Recommendation: ship native S3 (HO-07..HO-11).** Reason: exec-hook requires operators to have rclone or aws-cli in the container image, which breaks the distroless image (`gcr.io/distroless/cc-debian12`); a sidecar or separate container is needed. Native S3 is the clean story. Consequence: L effort, ~5 new transitive crates (see §9), owner approval required per BRIEF rules.
   - **Fallback: exec-hook only.** `MUSHROOMDB_BACKUP_PUSH_CMD` env var; exec after each snapshot. Drop HO-07..HO-11. Consequence: S-M effort, zero new deps, but the distroless container cannot run shell commands (exec hook is meaningless without a shell). Exec hook only works in non-distroless containers or as a Kubernetes sidecar. This is the weaker story.
   - **Both:** Implement exec-hook (S, always available) AND native S3 (L, optional). Most flexible; highest total effort.

2. **New crates approval (required per BRIEF rules):**
   - `object_store` (with feature `aws`): Apache Arrow project, Apache-2/MIT, version 0.11.x. Transitive deps include `reqwest` (already transitive), `aws-credential-types`, `bytes` (already transitive), `base64` (already transitive). Net new: `aws-credential-types`, `aws-config` (optional), `hyper` (version may conflict — check). **Owner must approve.**
   - Alternative to writing a Prometheus text serializer manually: `prometheus` crate or `metrics`+`metrics-exporter-prometheus`. If writing `format_prometheus()` by hand (it's ~50 lines of string formatting), zero new deps. **Builder's call if manual; owner must approve if crate used.**
   - `tracing-subscriber` JSON feature: already a dep, just the feature flag. **No new crate; no approval needed.**
   - `tower-http` `cors` feature: already a dep, just the feature flag. **No new crate; no approval needed.**

3. **PITR in 0.10 (archive-based) or deferred:**
   - **Recommendation: include commit-based PITR.** Archives are already commit-seq stamped; the restore CLI only needs to know which archives to stop fetching. Effort is small (a few lines in the restore logic). Timestamp PITR piggybacks on `commit_times.bin` — include it if the file is in the backup.
   - **Deferred:** Restore always restores to latest. Simpler. Deferred to 0.11.

---

## 9. New dependencies

| Crate | Version | License | Purpose | No-dep alternative | Owner approval needed? |
|---|---|---|---|---|---|
| `object_store` (with `aws` feature) | 0.11.x | Apache-2/MIT | S3-compatible upload/download in BackupWorker | exec-hook (distroless limitation; see §8b) | **Yes** |
| `uuid` | 1.x | Apache-2/MIT | Generate instance ID for bucket LEASE | Use a fixed-format random string via `rand` (already transitive) | No — use existing rand |
| Prometheus serializer | — | — | Prometheus text format on `/metrics` | Write 50-line `format_prometheus()` by hand — recommend this; zero new dep | N/A if manual |
| `tower-http` `cors` feature | existing dep | MIT | CorsLayer | Write a custom middleware (~30 lines) — not recommended | No — feature flag only |
| `tracing-subscriber` `json` feature | existing dep | MIT | Structured JSON logs | — | No — feature flag only |

**Net new crates requiring owner approval: one (`object_store` with `aws`).** If the exec-hook fallback is chosen instead, the count is zero.

---

## 10. Out of scope / deferred

| Item | Reason |
|---|---|
| ACME auto-cert (`rustls-acme`) | M effort; reverse proxy (Caddy, nginx) covers the use case without a new dep. Defer to 0.11. |
| Native Go / Java / Python backup CLIs | Bolt (W3) makes language-specific drivers unnecessary. Restore is a CLI operation. |
| Multi-store-per-server (one process, multiple stores) | Out of 0.10 per BRIEF. Single-store per process is the invariant. |
| Replication / HA / automatic failover | Explicitly out of 0.10 per BRIEF. Single-writer is the documented constraint. |
| Managed hosted service | Explicitly out of 0.10 per BRIEF. |
| Helm chart | Builder's call → OUT (see §8a). StatefulSet YAML is sufficient. Defer to 0.11 if operator demand warrants it. |
| `mushroomdb init-deploy` CLI generator | Builder's call → OUT (see §8a). |
| Per-request rate limiting | The existing concurrency limit (256) is the backstop. Per-IP rate limiting requires state that doesn't fit the single-instance model. Defer. |
| Azure Blob / GCS native backup | `object_store` supports them; env var config is the same pattern. Untested in 0.10; document as "community-supported" if `object_store` is used. |
| Time-based backup retention (`MUSHROOMDB_BACKUP_RETAIN_DAYS`) | Count-based (`MUSHROOMDB_BACKUP_RETAIN_N`) is simpler. Time-based deferred to 0.11. |
| Client-side encryption for backups | Requires key management design. Out of 0.10. |
| Strong distributed lease (compare-and-swap) | S3 conditional writes are not universal across compatible endpoints. Documented as advisory-only. A strong lease is a 0.11 concern if operators need it. |
