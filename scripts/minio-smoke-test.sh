#!/usr/bin/env bash
# Backup and restore end to end, through the built binary, against a real
# S3-compatible service (MinIO in CI):
#
#   1. `serve` with MUSHROOMDB_BACKUP_S3_URL; 10 nodes POSTed over HTTP.
#   2. The writes ship: `<prefix>/LATEST` is in the bucket, and
#      `mushroomdb restore --from` (taken while serve still runs) holds 10 nodes.
#   3. serve is stopped with SIGTERM and must exit 0.
#   4. `serve` on an empty directory with MUSHROOMDB_RESTORE_FROM_S3 restores
#      on boot: /health reports 10 nodes, and so does the stopped store.
#
# Settings (the same ones the S3-gated cargo tests read):
#   MUSHROOMDB_TEST_S3_ENDPOINT  e.g. http://127.0.0.1:9000 (required)
#   MUSHROOMDB_TEST_S3_BUCKET    default mushroomdb-test; must exist
#   AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY  default MinIO's minioadmin
#   MUSHROOMDB_BIN               default target/debug/mushroomdb
#
# Without MUSHROOMDB_TEST_S3_ENDPOINT it prints "skipping the S3 test" and
# exits 0, except under CI (CI set), where that is a failure: a skip in CI
# would leave backup unchecked without anyone seeing it. Credentials are never
# printed and never put on a command line.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ -z "${MUSHROOMDB_TEST_S3_ENDPOINT:-}" ]]; then
  if [[ -n "${CI:-}" ]]; then
    echo "minio-smoke-test: MUSHROOMDB_TEST_S3_ENDPOINT is not set under CI" >&2
    exit 1
  fi
  echo "minio-smoke-test: MUSHROOMDB_TEST_S3_ENDPOINT is not set: skipping the S3 test"
  exit 0
fi

ENDPOINT="${MUSHROOMDB_TEST_S3_ENDPOINT%/}"
BUCKET="${MUSHROOMDB_TEST_S3_BUCKET:-mushroomdb-test}"
export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-minioadmin}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-minioadmin}"
export AWS_ENDPOINT_URL="$ENDPOINT"
BIN="${MUSHROOMDB_BIN:-$ROOT/target/debug/mushroomdb}"
NODES=10

[[ -x "$BIN" ]] || { echo "minio-smoke-test: no binary at $BIN (cargo build -p mushroomdb-cli)" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/mdb-minio-smoke.XXXXXX")"
PREFIX="smoke-$(date +%s)-$$"
URL="minio://$BUCKET/$PREFIX"
SERVE_PID=""

cleanup() {
  if [[ -n "$SERVE_PID" ]] && kill -0 "$SERVE_PID" 2>/dev/null; then
    kill -KILL "$SERVE_PID" 2>/dev/null || true
  fi
  if [[ "${KEEP_WORK:-}" != 1 ]]; then rm -rf "$WORK"; fi
}
trap cleanup EXIT

fail() {
  echo "minio-smoke-test: FAIL: $*" >&2
  for log in "$WORK"/*.err; do
    [[ -f "$log" ]] && { echo "--- $log" >&2; tail -n 40 "$log" >&2; }
  done
  exit 1
}

# GET an object with SigV4; the credentials go to curl on stdin, not argv.
s3_get() {
  printf 'user = "%s:%s"\n' "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY" |
    curl -sf -K - --aws-sigv4 "aws:amz:${AWS_REGION:-us-east-1}:s3" "$ENDPOINT/$BUCKET/$1"
}

# Start serve on $1 (data dir) with the extra environment in the rest of the
# arguments; sets SERVE_PID and ADDR once the `listening on` line is out.
start_serve() {
  local dir="$1" name
  name="$(basename "$dir")"
  shift
  env -u MUSHROOMDB_BACKUP_S3_URL -u MUSHROOMDB_RESTORE_FROM_S3 "$@" \
    "$BIN" serve "$dir" --addr 127.0.0.1:0 --no-ui \
    >"$WORK/$name.out" 2>"$WORK/$name.err" &
  SERVE_PID=$!
  ADDR=""
  for _ in $(seq 1 600); do
    if line="$(grep -m1 '^listening on http://' "$WORK/$name.out" 2>/dev/null)"; then
      ADDR="${line#listening on http://}"
      return 0
    fi
    kill -0 "$SERVE_PID" 2>/dev/null || fail "serve on $name exited before listening"
    sleep 0.1
  done
  fail "serve on $name did not listen within 60 s"
}

stop_serve() {
  kill -TERM "$SERVE_PID"
  local status=0
  wait "$SERVE_PID" || status=$?
  SERVE_PID=""
  [[ "$status" == 0 ]] || fail "serve exited with status $status after SIGTERM"
}

health_nodes() {
  curl -sf "http://$ADDR/health" | python3 -c 'import json, sys; print(json.load(sys.stdin)["nodes"])'
}

# Node count of a stopped store, through the CLI.
store_nodes() {
  "$BIN" query "$1" "MATCH (n) RETURN count(n) AS c" | sed -n 's/^ *c=//p'
}

# 1. serve with backup on, and 10 writes over HTTP.
SRC="$WORK/source"
start_serve "$SRC" MUSHROOMDB_BACKUP_S3_URL="$URL" MUSHROOMDB_BACKUP_WAL_INTERVAL_MS=100
[[ "$(health_nodes)" == 0 ]] || fail "a new store holds nodes"
for i in $(seq 1 "$NODES"); do
  code="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
    --data "{\"label\":\"Thing\",\"key\":\"thing-$i\"}" "http://$ADDR/nodes")"
  [[ "$code" == 200 ]] || fail "POST /nodes $i answered $code"
done
echo "minio-smoke-test: $NODES nodes written to serve at $ADDR, backing up to $URL"

# 2. The writes ship: LATEST is in the bucket, and a restore taken while serve
#    is still running holds every write.
latest=""
for _ in $(seq 1 200); do
  latest="$(s3_get "$PREFIX/LATEST" || true)"
  [[ -n "$latest" ]] && break
  sleep 0.1
done
[[ -n "$latest" ]] || fail "no $PREFIX/LATEST in the bucket within 20 s"
echo "minio-smoke-test: LATEST names $latest"

restored=""
for attempt in $(seq 1 30); do
  INTO="$WORK/restored-$attempt"
  "$BIN" restore --from "$URL" --into "$INTO" >"$WORK/restore-$attempt.out" 2>"$WORK/restore.err" ||
    fail "mushroomdb restore --from $URL failed"
  restored="$(store_nodes "$INTO")"
  [[ "$restored" == "$NODES" ]] && break
  sleep 1
done
[[ "$restored" == "$NODES" ]] || fail "restore --from holds $restored nodes, not $NODES"
echo "minio-smoke-test: restore --from holds $restored nodes"

# 3. A clean stop.
stop_serve

# 4. Restore on boot into an empty directory.
BOOT="$WORK/boot"
mkdir "$BOOT"
start_serve "$BOOT" MUSHROOMDB_RESTORE_FROM_S3="$URL"
grep -q '^restore-on-boot:' "$WORK/boot.err" || fail "serve did not report a restore on boot"
booted="$(health_nodes)"
[[ "$booted" == "$NODES" ]] || fail "restore on boot serves $booted nodes, not $NODES"
stop_serve
stopped="$(store_nodes "$BOOT")"
[[ "$stopped" == "$NODES" ]] || fail "the boot-restored store holds $stopped nodes, not $NODES"
echo "minio-smoke-test: restore on boot serves $booted nodes"

if grep -qF "$AWS_SECRET_ACCESS_KEY" "$WORK"/*.out "$WORK"/*.err; then
  fail "a credential appears in serve or restore output"
fi
echo "minio-smoke-test: OK"
