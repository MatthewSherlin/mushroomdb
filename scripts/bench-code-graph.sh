#!/usr/bin/env bash
# bench-code-graph.sh — measure the code graph on real repositories.
#
# Prints a markdown table: for each repository, how big the graph is, how long
# it took to build, how long an incremental `touch` and a `map` take once it
# exists, and whether two independent ingests of the same tree export the same
# bytes.
#
# Two rows are measured:
#   - this checkout (a scratch `git worktree`, so nothing here is touched)
#   - a shallow clone of https://github.com/tokio-rs/axum, a public MIT-licensed
#     Rust repository, used purely as a second tree of a different shape
#
# The clone needs the network. Without it that row is reported as skipped and
# the rest of the run still produces a table.
#
# Usage:
#   cargo build --release -p mushroomdb-cli
#   MUSHROOMDB=target/release/mushroomdb bash scripts/bench-code-graph.sh
#
# Environment:
#   MUSHROOMDB   binary under test (default: target/release/mushroomdb)
#   BENCH_REPOS  space-separated extra clone URLs to add as rows
#   CLONE_DEPTH  history depth for cloned repositories (default 300)
#
# Latencies are the median of five runs of the real CLI, process start to
# process exit — the number a person waits for, not an in-process timer.
# Build the binary in release mode: a debug build measures the compiler's
# choices, not the database's.
set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

MUSHROOMDB="${MUSHROOMDB:-target/release/mushroomdb}"
case "$MUSHROOMDB" in
  /*) ;;
  *) MUSHROOMDB="$REPO_ROOT/$MUSHROOMDB" ;;
esac
CLONE_DEPTH="${CLONE_DEPTH:-300}"

command -v python3 >/dev/null 2>&1 || { echo "python3 is required" >&2; exit 1; }
[ -x "$MUSHROOMDB" ] || { echo "no binary at $MUSHROOMDB — cargo build --release -p mushroomdb-cli" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/mushroomdb-bench.XXXXXX")"
WT="$WORK/self"
cleanup() {
  git -C "$REPO_ROOT" worktree remove --force "$WT" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

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
run_ms() { run_ms_out /dev/null "$@"; }

median() { printf '%s\n' "$@" | sort -n | awk '{v[NR]=$1} END {print v[int((NR+1)/2)]}'; }

count_query() { "$MUSHROOMDB" query "$1" "$2" | sed -n 's/^ *n=\([0-9][0-9]*\)$/\1/p'; }

# Redact the two GitSync fields that cannot match between two runs: the path
# the store was built from and the wall clock it was built at.
redact() {
  sed -e '/"label":"GitSync"/ s/"repo":"[^"]*"/"repo":"<redacted>"/' \
      -e '/"label":"GitSync"/ s/"synced_at":[0-9]*/"synced_at":0/' "$1"
}

fmt_s() { python3 -c 'import sys; print("%.2f s" % (int(sys.argv[1]) / 1000.0))' "$1"; }
tick() { if [ "$1" = yes ]; then printf 'yes'; else printf '%s' "$1"; fi; }

ROWS=""
NOTES=""

# measure <label> <tree> — appends one markdown row for an already-checked-out tree.
measure() {
  label="$1"
  tree="$2"
  slug="$(printf '%s' "$label" | tr -c 'A-Za-z0-9' '-')"
  db="$WORK/db-$slug"
  db2="$WORK/db2-$slug"
  out="$WORK/ingest-$slug.txt"

  ingest_ms="$(run_ms_out "$out" "$MUSHROOMDB" ingest-git "$db" "$tree")"
  files="$(sed -n 's/^ingest-git: [0-9]* commit(s), \([0-9]*\) file(s).*/\1/p' "$out")"
  # A snapshot before the latency runs: an unsnapshotted store replays its
  # whole WAL on every open, which measures the WAL, not the query.
  "$MUSHROOMDB" snapshot "$db" >/dev/null

  symbols="$(count_query "$db" 'MATCH (s:Symbol) RETURN count(s) AS n')"
  edges="$("$MUSHROOMDB" stats "$db" | sed -n 's/^edges: \([0-9][0-9]*\)$/\1/p')"

  # The touched file is the repository's first tracked Rust file, so the
  # measurement covers a real extract rather than a hash-only skip.
  target="$(git -C "$tree" ls-files '*.rs' | head -1)"
  if [ -z "$target" ]; then target="$(git -C "$tree" ls-files | head -1)"; fi

  t1="$(run_ms "$MUSHROOMDB" touch "$db" "$tree/$target")"
  t2="$(run_ms "$MUSHROOMDB" touch "$db" "$tree/$target")"
  t3="$(run_ms "$MUSHROOMDB" touch "$db" "$tree/$target")"
  t4="$(run_ms "$MUSHROOMDB" touch "$db" "$tree/$target")"
  t5="$(run_ms "$MUSHROOMDB" touch "$db" "$tree/$target")"
  touch_ms="$(median "$t1" "$t2" "$t3" "$t4" "$t5")"

  m1="$(run_ms "$MUSHROOMDB" map "$db")"
  m2="$(run_ms "$MUSHROOMDB" map "$db")"
  m3="$(run_ms "$MUSHROOMDB" map "$db")"
  m4="$(run_ms "$MUSHROOMDB" map "$db")"
  m5="$(run_ms "$MUSHROOMDB" map "$db")"
  map_ms="$(median "$m1" "$m2" "$m3" "$m4" "$m5")"

  "$MUSHROOMDB" ingest-git "$db2" "$tree" >/dev/null
  "$MUSHROOMDB" export "$db" "$WORK/e1-$slug" --format jsonl >/dev/null
  "$MUSHROOMDB" export "$db2" "$WORK/e2-$slug" --format jsonl >/dev/null
  det="yes"
  for f in nodes.jsonl edges.jsonl rules.jsonl; do
    redact "$WORK/e1-$slug/$f" >"$WORK/da-$slug"
    redact "$WORK/e2-$slug/$f" >"$WORK/db-diff-$slug"
    diff -q "$WORK/da-$slug" "$WORK/db-diff-$slug" >/dev/null 2>&1 || det="NO ($f)"
  done

  printf 'measured %-12s files=%s symbols=%s edges=%s ingest=%sms touch=%sms map=%sms determinism=%s\n' \
    "$label" "$files" "$symbols" "$edges" "$ingest_ms" "$touch_ms" "$map_ms" "$det" >&2

  ROWS="$ROWS| $label | $files | $symbols | $edges | $(fmt_s "$ingest_ms") | ${touch_ms} ms | ${map_ms} ms | $(tick "$det") |
"
}

# ── this repository ──────────────────────────────────────────────────────────

git -C "$REPO_ROOT" worktree add --detach "$WT" HEAD >/dev/null 2>&1 \
  || { echo "cannot create a scratch worktree at $WT" >&2; exit 1; }
measure "mushroomdb" "$WT"

# ── cloned repositories ──────────────────────────────────────────────────────

for url in https://github.com/tokio-rs/axum ${BENCH_REPOS:-}; do
  name="$(basename "$url" .git)"
  dest="$WORK/clone-$name"
  if git clone --depth "$CLONE_DEPTH" --quiet "$url" "$dest" 2>"$WORK/clone-$name.err"; then
    measure "$name" "$dest"
  else
    NOTES="$NOTES- \`$name\` skipped: \`git clone --depth $CLONE_DEPTH $url\` failed (no network?).
"
    printf 'skipped %s: clone failed\n' "$name" >&2
    sed 's/^/  | /' "$WORK/clone-$name.err" >&2
  fi
done

# ── table ────────────────────────────────────────────────────────────────────

printf '\n'
printf '| repo | files | symbols | edges | time-to-graph | touch latency | map latency | determinism |\n'
printf '| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n'
printf '%s' "$ROWS"
printf '\n'
printf 'Binary: `%s` (%s). Latencies are the median of five end-to-end CLI runs\n' \
  "${MUSHROOMDB#"$REPO_ROOT"/}" "$("$MUSHROOMDB" --version)"
printf 'against a snapshotted store. Cloned repositories are shallow (`--depth %s`).\n' "$CLONE_DEPTH"
printf 'Determinism compares two independent ingests of the same tree, exported as\n'
printf 'JSONL, with the `GitSync` marker'"'"'s repo path and sync timestamp redacted.\n'
if [ -n "$NOTES" ]; then printf '\n%s' "$NOTES"; fi
