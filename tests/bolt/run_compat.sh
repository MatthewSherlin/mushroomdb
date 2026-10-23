#!/usr/bin/env bash
# Starts `mushroomdb serve --bolt-port` on a fresh store, waits for /ready,
# runs one driver compatibility script against it, and stops the server.
#
#   tests/bolt/run_compat.sh <mushroomdb-binary> <command> [args...]
#
# The command is run as `<command> [args...] <bolt-uri>`, with the full-access
# token in BOLT_COMPAT_TOKEN and the role token in BOLT_COMPAT_ROLE_TOKEN, for
# example:
#
#   tests/bolt/run_compat.sh target/release/mushroomdb python3 tests/bolt/python_compat.py
#   tests/bolt/run_compat.sh target/release/mushroomdb node tests/bolt/js_compat.mjs
#
# The store carries one role, `docs-reader`, that sees only `Doc` nodes; the
# role token is bound to it. ROUTE advertises `localhost`, so a driver given
# the routing URI on 127.0.0.1 is seen to move to the advertised address.
# Ports default to 17687 (Bolt) and 18087 (HTTP); override with
# BOLT_COMPAT_BOLT_PORT and BOLT_COMPAT_HTTP_PORT. BOLT_COMPAT_TIMEOUT bounds
# the script's run in seconds (default 300).
set -euo pipefail

if [ "$#" -lt 2 ]; then
  echo "usage: $0 <mushroomdb-binary> <command> [args...]" >&2
  exit 2
fi
BIN="$1"
shift

BOLT_PORT="${BOLT_COMPAT_BOLT_PORT:-17687}"
HTTP_PORT="${BOLT_COMPAT_HTTP_PORT:-18087}"
TIMEOUT="${BOLT_COMPAT_TIMEOUT:-300}"
TOKEN="bolt-compat-token-$$-$RANDOM"
ROLE_TOKEN="bolt-compat-role-$$-$RANDOM"

WORK="$(mktemp -d)"
DB="$WORK/db"
LOG="$WORK/serve.log"
pid=""
cleanup() {
  status=$?
  if [ -n "$pid" ]; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ] && [ -f "$LOG" ]; then
    echo "--- serve log ---" >&2
    tail -50 "$LOG" >&2 || true
  fi
  rm -rf "$WORK"
  exit "$status"
}
trap cleanup EXIT

cat >"$WORK/schema.json" <<'JSON'
{
  "roles": [
    { "name": "docs-reader", "keys": [], "labels": ["Doc"] }
  ]
}
JSON
"$BIN" schema apply "$DB" "$WORK/schema.json" >/dev/null

# The token is passed through the environment, never on a command line that
# a process listing or a CI log would show.
MUSHROOMDB_TOKEN="$TOKEN" MUSHROOMDB_ROLE_TOKENS="$ROLE_TOKEN:docs-reader" \
  "$BIN" serve "$DB" --no-ui \
  --addr "127.0.0.1:$HTTP_PORT" \
  --bolt-port "$BOLT_PORT" \
  --bolt-advertised-host localhost \
  >"$LOG" 2>&1 &
pid=$!

ready=0
for _ in $(seq 1 100); do
  if curl -sf "http://127.0.0.1:$HTTP_PORT/ready" >/dev/null 2>&1; then
    ready=1
    break
  fi
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "serve exited before it was ready" >&2
    exit 1
  fi
  sleep 0.1
done
if [ "$ready" != 1 ]; then
  echo "serve was not ready after 10 s" >&2
  exit 1
fi

export BOLT_COMPAT_TOKEN="$TOKEN" BOLT_COMPAT_ROLE_TOKEN="$ROLE_TOKEN"
if command -v timeout >/dev/null 2>&1; then
  timeout "$TIMEOUT" "$@" "bolt://127.0.0.1:$BOLT_PORT"
else
  "$@" "bolt://127.0.0.1:$BOLT_PORT"
fi
