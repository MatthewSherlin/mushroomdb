#!/usr/bin/env bash
# Generates docs/site/conformance-functions.md from the vendored
# openCypher TCK: one row per Cypher function, from `tck-runner --function-table`.
#
#   bash scripts/gen-conformance-table.sh           write the page
#   bash scripts/gen-conformance-table.sh --check   fail when the committed page is stale
#
# Environment (for the script's own test): TCK_RUNNER is a command run as
# `$TCK_RUNNER --function-table <path>` in place of cargo; CONFORMANCE_TARGET is
# the page to write or check.
set -euo pipefail
ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
TARGET="${CONFORMANCE_TARGET:-$ROOT/docs/site/conformance-functions.md}"
REGEN="bash scripts/gen-conformance-table.sh"

check=0
case "${1:-}" in
  "") ;;
  --check) check=1 ;;
  *) echo "usage: gen-conformance-table.sh [--check]" >&2; exit 2 ;;
esac

tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
rm -f "$tmp"
log="$(mktemp)"; trap 'rm -f "$tmp" "$log"' EXIT
# The runner's stderr carries every slow-query notice; show it only on failure.
if [[ -n "${TCK_RUNNER:-}" ]]; then
  run=($TCK_RUNNER)
else
  run=(cargo run -q -p tck-runner --)
fi
if ! (cd "$ROOT" && "${run[@]}" --function-table "$tmp" >/dev/null 2>"$log"); then
  tail -n 40 "$log" >&2
  echo "error: tck-runner failed" >&2
  exit 1
fi
if [[ ! -s "$tmp" ]]; then
  echo "error: tck-runner produced no output" >&2
  exit 1
fi

if [[ $check -eq 1 ]]; then
  if ! diff -u "$TARGET" "$tmp" >&2; then
    echo "error: $TARGET is stale: a change moved a TCK result. Regenerate it and commit it: $REGEN" >&2
    exit 1
  fi
  echo "conformance-functions.md is current"
else
  mkdir -p "$(dirname "$TARGET")"
  cp "$tmp" "$TARGET"
  echo "wrote $TARGET"
fi
