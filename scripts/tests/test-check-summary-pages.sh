#!/usr/bin/env bash
# The summary-pages gate passes on the real sidebar and fails on one that dropped a page.
set -euo pipefail
ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
bash "$ROOT/scripts/check-summary-pages.sh"
tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
grep -vF '(limits.md)' "$ROOT/docs/site/SUMMARY.md" > "$tmp"
if bash "$ROOT/scripts/check-summary-pages.sh" "$tmp" 2>/dev/null; then
  echo "FAIL: a sidebar missing limits.md passed" >&2; exit 1
fi
echo "ok"
