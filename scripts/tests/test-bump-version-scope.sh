#!/usr/bin/env bash
# `bump-version.sh --check` must see inter-crate pins in bindings/, not just
# crates/. The 0.6.10 pins in bindings/python/Cargo.toml resolved under caret
# and would have failed every 0.7.0 publish with this gate reporting OK.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

WS="$(awk -F'"' '/^version = "/{print $2; exit}' Cargo.toml)"
BINDING="bindings/python/Cargo.toml"
BACKUP="$(mktemp)"
cp "$BINDING" "$BACKUP"
restore() { cp "$BACKUP" "$BINDING"; rm -f "$BACKUP"; }
trap restore EXIT

# Baseline: a clean tree must pass.
if ! bash scripts/bump-version.sh --check >/dev/null 2>&1; then
  echo "FAIL: --check does not pass on a clean tree" >&2
  exit 1
fi

# Drift one binding pin and require the gate to notice.
perl -0pi -e 's/(path = "\.\.\/\.\.\/crates\/core-api", version = ")[^"]*(")/${1}0.0.1${2}/' "$BINDING"
if bash scripts/bump-version.sh --check >/dev/null 2>&1; then
  echo "FAIL: --check passed with bindings/python core-api pinned to 0.0.1" >&2
  exit 1
fi

# And the bump must fix it.
restore; cp "$BINDING" "$BACKUP"
perl -0pi -e 's/(path = "\.\.\/\.\.\/crates\/core-api", version = ")[^"]*(")/${1}0.0.1${2}/' "$BINDING"
bash scripts/bump-version.sh "$WS" >/dev/null
if ! grep -q "version = \"$WS\"" "$BINDING"; then
  echo "FAIL: bump did not rewrite the bindings pin to $WS" >&2
  exit 1
fi

echo "test-bump-version-scope.sh: OK"
