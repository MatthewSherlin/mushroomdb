#!/usr/bin/env bash
# Fails if a page listed in scripts/site-pages-baseline.txt is missing from the
# docs sidebar (docs/site/SUMMARY.md). check-links.py catches a page on disk the
# sidebar omits; this catches a page dropped from both. To retire a page on
# purpose, remove it from the baseline in the same change.
set -euo pipefail
ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
SUMMARY="${1:-$ROOT/docs/site/SUMMARY.md}"
BASELINE="${2:-$ROOT/scripts/site-pages-baseline.txt}"
missing=0
while IFS= read -r page; do
  [[ -z "$page" ]] && continue
  if ! grep -qF "($page)" "$SUMMARY"; then
    echo "check-summary-pages: $page is in the baseline but not in $SUMMARY" >&2
    missing=1
  fi
done < "$BASELINE"
exit "$missing"
