#!/usr/bin/env bash
# acceptance-0.6.sh — the v0.6 code-graph acceptance run.
#
# Seven steps, each asserted against a real run on a real checkout of this
# repository. Nothing here is a unit test: every step drives the shipped
# `mushroomdb` binary the way a person or an assistant host drives it, and
# every number printed is measured, not quoted.
#
#   1. ingest-git      a scratch worktree of this repo becomes a graph
#   2. map             the digest is short and carries clusters/key files/owners
#   3. determinism     two independent ingests export byte-identical data
#   4. freshness       an edit adds an IMPORTS edge; reverting it retracts one
#   5. nudge           a dirty file makes `recall` name its co-change partners
#   6. concurrency     20 parallel writers against a live MCP server
#   7. timings         ingest, touch and map wall-clock
#
# Usage:
#   bash scripts/acceptance-0.6.sh
#   MUSHROOMDB=target/release/mushroomdb bash scripts/acceptance-0.6.sh
#
# Environment:
#   MUSHROOMDB          binary under test (default: target/debug/mushroomdb)
#   MUSHROOMDB_RELEASE  set to 1 to force the timing thresholds on
#   TOUCH_BUDGET_MS     touch latency budget (default 200)
#   MAP_BUDGET_MS       map latency budget (default 1000)
#
# Timing thresholds are asserted only against a release build — a debug build
# is several times slower for reasons that have nothing to do with the code
# under test, so its timings are printed and the assertions skipped. A build is
# taken to be a release build when MUSHROOMDB_RELEASE=1 or when the binary sits
# under a `target/release/` directory.
#
# Nothing in the checkout this runs from is ever modified: every edit happens
# in a throwaway `git worktree`, which the exit trap removes.
#
# Requires: git, python3 (timing and JSON), and a POSIX mkfifo. `gh` is
# optional — without it, or without an authenticated `gh`, the ingest drops
# `--prs` and says so.
set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

MUSHROOMDB="${MUSHROOMDB:-target/debug/mushroomdb}"
case "$MUSHROOMDB" in
  /*) ;;
  *) MUSHROOMDB="$REPO_ROOT/$MUSHROOMDB" ;;
esac

TOUCH_BUDGET_MS="${TOUCH_BUDGET_MS:-200}"
MAP_BUDGET_MS="${MAP_BUDGET_MS:-1000}"

# The file whose import is added and retracted in step 4, and made dirty in
# step 5. It has to be a tracked Rust file that (a) does not already import
# the target and (b) has co-change partners in the graph; step 4 asserts the
# first precondition before it edits anything.
EDIT_FILE="crates/cli/src/ingest_git.rs"
IMPORT_TARGET="crates/cli/src/recall.rs"
IMPORT_LINE="use crate::recall;"

# ── plumbing ─────────────────────────────────────────────────────────────────

FAILURES=0
STEP=""

step() {
  STEP="$1"
  printf '\n=== step %s ===\n' "$1"
}

pass() { printf 'PASS  %s\n' "$1"; }

fail() {
  printf 'FAIL  %s\n' "$1"
  FAILURES=$((FAILURES + 1))
}

die() {
  printf 'ERROR (step %s): %s\n' "$STEP" "$1" >&2
  exit 1
}

# assert_contains <haystack-file> <needle> <label>
assert_contains() {
  if grep -qF -- "$2" "$1"; then pass "$3"; else
    fail "$3 — not found: $2"
    sed -n '1,40p' "$1" | sed 's/^/      | /'
  fi
}

# assert_absent <haystack-file> <needle> <label>
assert_absent() {
  if grep -qF -- "$2" "$1"; then
    fail "$3 — unexpectedly found: $2"
    sed -n '1,40p' "$1" | sed 's/^/      | /'
  else pass "$3"; fi
}

# assert_gt <actual> <floor> <label>
assert_gt() {
  if [ "$1" -gt "$2" ]; then pass "$3 ($1 > $2)"; else fail "$3 ($1 <= $2)"; fi
}

# assert_eq <actual> <expected> <label>
assert_eq() {
  if [ "$1" = "$2" ]; then pass "$3 ($1)"; else fail "$3 (got $1, want $2)"; fi
}

# assert_le <actual-ms> <budget-ms> <label> — a no-op on a debug build.
assert_le() {
  if [ "$RELEASE" = 1 ]; then
    if [ "$1" -le "$2" ]; then pass "$3 (${1} ms <= ${2} ms)"; else fail "$3 (${1} ms > ${2} ms)"; fi
  else
    printf 'SKIP  %s (%s ms) (debug build: timing thresholds not asserted)\n' "$3" "$1"
  fi
}

# run_ms <cmd...> — runs the command with its stdout discarded and prints the
# wall-clock milliseconds it took. Exits with the command's status, so a
# failure still stops the script. `time` is not used: it has no portable
# machine-readable millisecond output across bash 3.2 on macOS and GNU coreutils.
run_ms() { run_ms_out /dev/null "$@"; }

# run_ms_out <stdout-file> <cmd...> — as `run_ms`, but the command's stdout is
# written to the named file instead of being discarded, so one run can be both
# measured and inspected.
run_ms_out() {
  python3 -c '
import subprocess, sys, time
with open(sys.argv[1], "wb") as out:
    t0 = time.perf_counter()
    rc = subprocess.call(sys.argv[2:], stdout=out)
    dt = time.perf_counter() - t0
print(int(dt * 1000))
sys.exit(rc)
' "$@"
}

# median <ms...> — prints the middle value of an odd-length sorted list.
median() {
  printf '%s\n' "$@" | sort -n | awk '{v[NR]=$1} END {print v[int((NR+1)/2)]}'
}

# count_query <db> <cypher> — prints the single integer a `RETURN count(x) AS n`
# query produced. The CLI prints `  n=<value>`.
count_query() {
  "$MUSHROOMDB" query "$1" "$2" | sed -n 's/^ *n=\([0-9][0-9]*\)$/\1/p'
}

# ── setup ────────────────────────────────────────────────────────────────────

command -v python3 >/dev/null 2>&1 || die "python3 is required (timings and JSON parsing)"
[ -x "$MUSHROOMDB" ] || die "no binary at $MUSHROOMDB — build it first (cargo build -p mushroomdb-cli)"

RELEASE=0
if [ "${MUSHROOMDB_RELEASE:-}" = 1 ]; then
  RELEASE=1
else
  case "$MUSHROOMDB" in */target/release/*) RELEASE=1 ;; esac
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/mushroomdb-acceptance.XXXXXX")"
WT="$WORK/wt"
DB="$WORK/db"
DB2="$WORK/db2"

MCP_PID=""
cleanup() {
  # A step-6 failure can leave the MCP server holding the store; nothing else
  # closes its FIFO once the script has stopped writing to it.
  if [ -n "$MCP_PID" ]; then kill "$MCP_PID" >/dev/null 2>&1 || true; fi
  # The worktree is removed through git so the parent repo's administrative
  # record goes with it; --force because step 4/5 leave it dirty on a failure.
  git -C "$REPO_ROOT" worktree remove --force "$WT" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

printf 'mushroomdb v0.6 acceptance\n'
printf '  binary   %s (%s)\n' "$MUSHROOMDB" "$("$MUSHROOMDB" --version)"
printf '  build    %s\n' "$([ "$RELEASE" = 1 ] && echo release || echo 'debug (timing thresholds not asserted)')"
printf '  repo     %s at %s\n' "$REPO_ROOT" "$(git -C "$REPO_ROOT" rev-parse --short HEAD)"
printf '  workdir  %s\n' "$WORK"

git -C "$REPO_ROOT" worktree add --detach "$WT" HEAD >/dev/null 2>&1 \
  || die "cannot create a scratch worktree at $WT"

# ── 1. ingest ────────────────────────────────────────────────────────────────

step "1 — ingest-git"

PRS_FLAG=""
PRS_NOTE="--prs skipped: gh is not installed"
if command -v gh >/dev/null 2>&1; then
  if gh auth status >/dev/null 2>&1; then
    PRS_FLAG="--prs"
    PRS_NOTE="--prs enabled: gh is installed and authenticated"
  else
    PRS_NOTE="--prs skipped: gh is installed but not authenticated"
  fi
fi
printf '%s\n' "$PRS_NOTE"

INGEST_OUT="$WORK/ingest.txt"
# shellcheck disable=SC2086 # PRS_FLAG is deliberately unquoted: empty means "no flag".
INGEST_MS="$(run_ms_out "$INGEST_OUT" "$MUSHROOMDB" ingest-git "$DB" "$WT" $PRS_FLAG)"
sed 's/^/  | /' "$INGEST_OUT"

INGEST_FILES="$(sed -n 's/^ingest-git: [0-9]* commit(s), \([0-9]*\) file(s).*/\1/p' "$INGEST_OUT")"
[ -n "$INGEST_FILES" ] || die "cannot read the file count out of the ingest report"

SYMBOLS="$(count_query "$DB" 'MATCH (s:Symbol) RETURN count(s) AS n')"
IMPORTS="$(count_query "$DB" 'MATCH ()-[r:IMPORTS]->() RETURN count(r) AS n')"
assert_gt "$SYMBOLS" 500 "Symbol nodes"
assert_gt "$IMPORTS" 100 "IMPORTS edges"

# ── 2. map ───────────────────────────────────────────────────────────────────

step "2 — map"

MAP_OUT="$WORK/map.txt"
"$MUSHROOMDB" map "$DB" >"$MAP_OUT" || die "map failed"
sed 's/^/  | /' "$MAP_OUT"

MAP_LINES="$(wc -l <"$MAP_OUT" | tr -d ' ')"
if [ "$MAP_LINES" -le 40 ]; then pass "map is $MAP_LINES lines (<= 40)"; else fail "map is $MAP_LINES lines (> 40)"; fi
assert_contains "$MAP_OUT" "clusters" "map names clusters"
assert_contains "$MAP_OUT" "key files" "map names key files"
assert_contains "$MAP_OUT" "owners" "map names owners"

# ── 3. determinism ───────────────────────────────────────────────────────────

step "3 — determinism"

# shellcheck disable=SC2086
"$MUSHROOMDB" ingest-git "$DB2" "$WT" $PRS_FLAG >/dev/null || die "second ingest failed"
"$MUSHROOMDB" export "$DB" "$WORK/exp1" --format jsonl >/dev/null || die "first export failed"
"$MUSHROOMDB" export "$DB2" "$WORK/exp2" --format jsonl >/dev/null || die "second export failed"

# The GitSync marker is the one node that cannot match: `repo` is the path the
# store was built from and `synced_at` is a wall clock reading. Both are
# redacted on that node's line only — every other byte must be identical.
redact() {
  sed -e '/"label":"GitSync"/ s/"repo":"[^"]*"/"repo":"<redacted>"/' \
      -e '/"label":"GitSync"/ s/"synced_at":[0-9]*/"synced_at":0/' "$1"
}

DIFF_OUT="$WORK/export.diff"
: >"$DIFF_OUT"
for f in nodes.jsonl edges.jsonl rules.jsonl; do
  redact "$WORK/exp1/$f" >"$WORK/a.$f"
  redact "$WORK/exp2/$f" >"$WORK/b.$f"
  if ! diff -u "$WORK/a.$f" "$WORK/b.$f" >>"$DIFF_OUT" 2>&1; then
    printf '  %s differs\n' "$f"
  fi
done
if [ -s "$DIFF_OUT" ]; then
  fail "two ingests export identical JSONL (minus GitSync.repo and GitSync.synced_at)"
  sed -n '1,40p' "$DIFF_OUT" | sed 's/^/      | /'
  DETERMINISTIC=no
else
  pass "two ingests export identical JSONL (minus GitSync.repo and GitSync.synced_at)"
  DETERMINISTIC=yes
fi

# A snapshot from here on: `verify` (step 6) has nothing to check without one,
# and a store that has only ever been written through the WAL pays a full
# replay on every open, which is not the latency a real installation sees.
"$MUSHROOMDB" snapshot "$DB" >/dev/null || die "snapshot failed"

# ── 4. freshness ─────────────────────────────────────────────────────────────

step "4 — freshness"

BEFORE="$(count_query "$DB" "MATCH (a:File)-[r:IMPORTS]->(b:File) WHERE a.path = '$EDIT_FILE' AND b.path = '$IMPORT_TARGET' RETURN count(r) AS n")"
[ "$BEFORE" = 0 ] || die "precondition: $EDIT_FILE already imports $IMPORT_TARGET"

printf '%s\n' "$IMPORT_LINE" >>"$WT/$EDIT_FILE"
"$MUSHROOMDB" touch "$DB" "$WT/$EDIT_FILE" | sed 's/^/  | /'

WHY_OUT="$WORK/why-linked.txt"
"$MUSHROOMDB" why "$DB" "$EDIT_FILE" "$IMPORT_TARGET" >"$WHY_OUT" || die "why failed"
sed 's/^/  | /' "$WHY_OUT"
assert_contains "$WHY_OUT" "IMPORTS a→b  imports" "an edit adds a direct IMPORTS link"
ADDED_LINE="$(sed -n "s|^ *$EDIT_FILE line \([0-9]*\): import $IMPORT_TARGET$|\1|p" "$WHY_OUT")"
if [ -n "$ADDED_LINE" ]; then pass "why quotes the import's line number ($ADDED_LINE)"; else fail "why does not quote the import's line number"; fi

git -C "$WT" checkout -- "$EDIT_FILE"
"$MUSHROOMDB" touch "$DB" "$WT/$EDIT_FILE" | sed 's/^/  | /'

WHY_GONE="$WORK/why-retracted.txt"
"$MUSHROOMDB" why "$DB" "$EDIT_FILE" "$IMPORT_TARGET" >"$WHY_GONE" || die "why failed"
sed 's/^/  | /' "$WHY_GONE"
assert_absent "$WHY_GONE" "IMPORTS a→b  imports" "reverting the edit retracts the direct IMPORTS link"
AFTER="$(count_query "$DB" "MATCH (a:File)-[r:IMPORTS]->(b:File) WHERE a.path = '$EDIT_FILE' AND b.path = '$IMPORT_TARGET' RETURN count(r) AS n")"
assert_eq "$AFTER" 0 "no IMPORTS edge survives the revert"

# ── 5. nudge ─────────────────────────────────────────────────────────────────

step "5 — nudge"

printf '%s\n' "$IMPORT_LINE" >>"$WT/$EDIT_FILE"
NUDGE_OUT="$WORK/nudge.txt"
printf '{"cwd":"%s","user_input":"hi"}' "$WT" | "$MUSHROOMDB" recall "$DB" >"$NUDGE_OUT"
sed 's/^/  | /' "$NUDGE_OUT"
assert_contains "$NUDGE_OUT" "usually changes with" "a dirty file makes recall name its co-change partners"
git -C "$WT" checkout -- "$EDIT_FILE"
"$MUSHROOMDB" touch "$DB" "$WT/$EDIT_FILE" >/dev/null

# ── 6. concurrency ───────────────────────────────────────────────────────────

step "6 — concurrency: 20 parallel writers against a live MCP server"

MCP_IN="$WORK/mcp.in"
MCP_OUT="$WORK/mcp.out"
MCP_ERR="$WORK/mcp.err"
mkfifo "$MCP_IN"
"$MUSHROOMDB" mcp "$DB" <"$MCP_IN" >"$MCP_OUT" 2>"$MCP_ERR" &
MCP_PID=$!
# Holding the write end open keeps the server alive; closing fd 9 is the EOF
# that must make it exit 0.
exec 9>"$MCP_IN"

send() { printf '%s\n' "$1" >&9; }
send '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"acceptance-0.6","version":"0"}}}'
send '{"jsonrpc":"2.0","method":"notifications/initialized"}'

# await_id <json-rpc id> — waits up to 30 s for the complete response line
# carrying that id and prints it. A line still missing its terminating newline
# is half-written, so it is skipped rather than parsed.
await_id() {
  python3 -c '
import json, sys, time
path, want = sys.argv[1], int(sys.argv[2])
deadline = time.time() + 30
while time.time() < deadline:
    with open(path) as fh:
        for line in fh:
            if not line.endswith("\n"):
                continue
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if msg.get("id") == want:
                sys.stdout.write(line)
                sys.exit(0)
    time.sleep(0.05)
sys.exit(1)
' "$MCP_OUT" "$1"
}

INIT_LINE="$WORK/mcp-init.json"
await_id 1 >"$INIT_LINE" || die "MCP server did not answer initialize"
assert_contains "$INIT_LINE" '"name":"mushroomdb"' "MCP server answers initialize"

# Exactly 20 concurrent writers, which is the number the step is specified in.
# The tracked Rust files are cycled to fill the list, so a repository with
# fewer than 20 of them still gets 20 processes rather than silently fewer.
PARALLEL=20
CANDIDATES="$(git -C "$WT" ls-files '*.rs')"
[ -n "$CANDIDATES" ] || die "no tracked Rust files in $WT to touch"
TOUCH_FILES=""
n=0
while [ "$n" -lt "$PARALLEL" ]; do
  for f in $CANDIDATES; do
    [ "$n" -lt "$PARALLEL" ] || break
    TOUCH_FILES="$TOUCH_FILES $f"
    n=$((n + 1))
  done
done

PIDS=""
i=0
for f in $TOUCH_FILES; do
  ( "$MUSHROOMDB" touch "$DB" "$WT/$f" >"$WORK/t.$i.out" 2>&1; printf '%s\n' "$?" >"$WORK/t.$i.rc" ) &
  PIDS="$PIDS $!"
  i=$((i + 1))
done
for p in $PIDS; do wait "$p" || true; done

BAD=0
for rc in "$WORK"/t.*.rc; do
  [ "$(cat "$rc")" = 0 ] || BAD=$((BAD + 1))
done
if [ "$BAD" = 0 ]; then
  pass "$PARALLEL parallel touch processes all exited 0"
else
  fail "$BAD of $PARALLEL parallel touch processes failed"
  for out in "$WORK"/t.*.out; do sed 's/^/      | /' "$out"; done
fi

send '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"map","arguments":{}}}'
MAP_LINE="$WORK/mcp-map.json"
await_id 2 >"$MAP_LINE" || die "MCP server did not answer tools/call map"

MCP_FILES="$(python3 -c '
import json, sys
msg = json.load(open(sys.argv[1]))
print(msg["result"]["structuredContent"]["files"])
' "$MAP_LINE")"
assert_eq "$MCP_FILES" "$INGEST_FILES" "the live server's map still counts every ingested file"

# Closing the write end is the EOF. The wait for the server to notice it is
# bounded: a server that stayed alive past EOF — exactly the bug this step
# exists to catch — must produce a FAIL line, not an unkillable hang here and
# a job-timeout kill in CI. A watchdog kills it after 30 s and leaves a flag
# behind so the failure can say which of the two things went wrong. The
# watchdog is itself killed the moment `wait` returns, so it can only ever
# fire while the server is genuinely still running.
exec 9>&-
WATCHDOG_FLAG="$WORK/mcp-watchdog.fired"
( sleep 30
  if kill -0 "$MCP_PID" 2>/dev/null; then
    : >"$WATCHDOG_FLAG"
    kill -9 "$MCP_PID" 2>/dev/null || true
  fi ) &
WATCHDOG_PID=$!

MCP_RC=0
wait "$MCP_PID" || MCP_RC=$?
MCP_PID="" # reaped: the exit trap must not kill whatever inherits the pid
kill "$WATCHDOG_PID" 2>/dev/null || true
wait "$WATCHDOG_PID" 2>/dev/null || true

if [ -f "$WATCHDOG_FLAG" ]; then
  fail "the MCP server exits 0 on EOF — still alive 30 s after the FIFO closed, killed"
else
  assert_eq "$MCP_RC" 0 "the MCP server exits 0 on EOF"
fi
if [ -s "$MCP_ERR" ]; then sed 's/^/      | stderr: /' "$MCP_ERR"; fi

VERIFY_OUT="$WORK/verify.txt"
if "$MUSHROOMDB" verify "$DB" >"$VERIFY_OUT" 2>&1; then
  pass "verify passes after the stress ($(head -1 "$VERIFY_OUT"))"
else
  fail "verify failed after the stress"
  sed 's/^/      | /' "$VERIFY_OUT"
fi

# ── 7. timings ───────────────────────────────────────────────────────────────

step "7 — timings"

TOUCH_FILE="$WT/$EDIT_FILE"
T1="$(run_ms "$MUSHROOMDB" touch "$DB" "$TOUCH_FILE")"
T2="$(run_ms "$MUSHROOMDB" touch "$DB" "$TOUCH_FILE")"
T3="$(run_ms "$MUSHROOMDB" touch "$DB" "$TOUCH_FILE")"
T4="$(run_ms "$MUSHROOMDB" touch "$DB" "$TOUCH_FILE")"
T5="$(run_ms "$MUSHROOMDB" touch "$DB" "$TOUCH_FILE")"
TOUCH_MS="$(median "$T1" "$T2" "$T3" "$T4" "$T5")"

M1="$(run_ms "$MUSHROOMDB" map "$DB")"
M2="$(run_ms "$MUSHROOMDB" map "$DB")"
M3="$(run_ms "$MUSHROOMDB" map "$DB")"
M4="$(run_ms "$MUSHROOMDB" map "$DB")"
M5="$(run_ms "$MUSHROOMDB" map "$DB")"
MAP_MS="$(median "$M1" "$M2" "$M3" "$M4" "$M5")"

printf '\n  %-24s %10s  %s\n' "measurement" "value" "budget"
printf '  %-24s %10s  %s\n' "------------------------" "----------" "------"
printf '  %-24s %10s  %s\n' "ingest (wall clock)" "${INGEST_MS} ms" "-"
printf '  %-24s %10s  %s\n' "touch one file (median)" "${TOUCH_MS} ms" "${TOUCH_BUDGET_MS} ms"
printf '  %-24s %10s  %s\n' "map (median)" "${MAP_MS} ms" "${MAP_BUDGET_MS} ms"
printf '  %-24s %10s  %s\n' "files ingested" "$INGEST_FILES" "-"
printf '  %-24s %10s  %s\n' "symbols" "$SYMBOLS" "> 500"
printf '  %-24s %10s  %s\n' "IMPORTS edges" "$IMPORTS" "> 100"
printf '  touch runs (ms): %s %s %s %s %s\n' "$T1" "$T2" "$T3" "$T4" "$T5"
printf '  map runs (ms):   %s %s %s %s %s\n\n' "$M1" "$M2" "$M3" "$M4" "$M5"

assert_le "$TOUCH_MS" "$TOUCH_BUDGET_MS" "touch latency"
assert_le "$MAP_MS" "$MAP_BUDGET_MS" "map latency"

# ── verdict ──────────────────────────────────────────────────────────────────

printf '\n'
if [ "$FAILURES" = 0 ]; then
  printf 'acceptance-0.6: ALL STEPS PASSED (determinism=%s)\n' "$DETERMINISTIC"
  exit 0
fi
printf 'acceptance-0.6: %s ASSERTION(S) FAILED\n' "$FAILURES"
exit 1
