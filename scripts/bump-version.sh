#!/usr/bin/env bash
# One version, twenty-four places.
#
# The workspace version lives in `Cargo.toml`, and nothing else inherits it:
# four crates carry a literal because `cargo package` needs one, fourteen
# inter-crate dependency pins carry it again so a published crate can resolve
# its siblings, and five package manifests outside Cargo carry it for npm, PyPI,
# the MCP registry and the discovery card.
#
# Hand-editing twenty-four places is how 0.6.9 shipped telling readers to
# install 0.6.8, and how a v0.6.11 tag would have published a Python wheel
# labelled 0.6.10 — `pyproject.toml` is the one npm and the TypeScript client
# rewrite from the tag and maturin does not.
#
# The dependency pins matter for a different reason and only bite once: they sat
# at `0.6.10` while the workspace was `0.6.11`, which resolves, because `^0.6.10`
# matches `0.6.11`. It does not match `0.7.0`. Left alone, every `cargo publish`
# in the 0.7 release would have failed mid-sequence, across eight crates
# published in dependency order.
#
#   bump-version.sh 0.6.12     set every site
#   bump-version.sh --check    verify every site already agrees (CI)
#
# `--check` is the half that keeps this from happening again: it needs no
# argument and fails when any site disagrees with `Cargo.toml`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SEMVER='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'

usage() { echo "usage: bump-version.sh <new-version> | --check" >&2; exit 2; }
[[ $# -eq 1 ]] || usage

CHECK=0
if [[ "$1" == "--check" ]]; then
  CHECK=1
else
  [[ "$1" =~ $SEMVER ]] || { echo "not a version: $1" >&2; usage; }
  NEW="$1"
fi

WS="$(awk -F'"' '/^version = "/{print $2; exit}' Cargo.toml)"
[[ -n "$WS" ]] || { echo "could not read the workspace version from Cargo.toml" >&2; exit 1; }
[[ $CHECK -eq 1 ]] && NEW="$WS"

# Every site, as `file:matcher`. The matcher is a sed address so a version that
# appears in prose in the same file is never touched — only the declaration.
#
# `packaging/plugin/**` is deliberately absent: those are rendered by
# `scripts/render-plugin.sh` from the crate version, and this script re-runs it.
cargo_manifests=(
  "Cargo.toml"
  "crates/cli/Cargo.toml"
  "crates/core-bench/Cargo.toml"
  "crates/sim-harness/Cargo.toml"
  "bindings/python/Cargo.toml"
)
json_manifests=(
  "packaging/npm/package.json"
  "clients/typescript/package.json"
  ".well-known/mcp/server-card.json"
  "server.json"
)

fail=0
report() { # file, what, found
  if [[ $CHECK -eq 1 ]]; then
    echo "bump-version.sh: $1 — $2 is $3, workspace is $WS" >&2
    fail=1
  fi
}

# 1. `version = "X"` at the top of a Cargo manifest (the package's own version).
for f in "${cargo_manifests[@]}"; do
  cur="$(awk -F'"' '/^version = "/{print $2; exit}' "$f")"
  [[ "$cur" == "$NEW" ]] && continue
  if [[ $CHECK -eq 1 ]]; then report "$f" "package version" "$cur"; else
    perl -0pi -e 's/^version = "[^"]*"/version = "'"$NEW"'"/m' "$f"
  fi
done

# 2. Inter-crate pins: `path = "../x", version = "Y"`. A sibling is always
#    published at the workspace version, so Y is always the workspace version.
while IFS= read -r f; do
  while IFS= read -r cur; do
    [[ "$cur" == "$NEW" ]] && continue
    if [[ $CHECK -eq 1 ]]; then report "$f" "an inter-crate pin" "$cur"; else :; fi
  done < <(perl -ne 'print "$1\n" if /path = "\.\.\/[^"]*", version = "([^"]*)"/' "$f")
  [[ $CHECK -eq 1 ]] || perl -0pi -e 's/(path = "\.\.\/[^"]*", version = ")[^"]*(")/${1}'"$NEW"'${2}/g' "$f"
done < <(ls crates/*/Cargo.toml)

# 3. `"version": "X"` in a JSON manifest. `server.json` carries two.
for f in "${json_manifests[@]}"; do
  while IFS= read -r cur; do
    [[ "$cur" == "$NEW" ]] && continue
    if [[ $CHECK -eq 1 ]]; then report "$f" "a version field" "$cur"; else :; fi
  done < <(perl -ne 'print "$1\n" if /"version"\s*:\s*"([^"]*)"/' "$f")
  [[ $CHECK -eq 1 ]] || perl -0pi -e 's/("version"\s*:\s*")[^"]*(")/${1}'"$NEW"'${2}/g' "$f"
done

# 4. `pyproject.toml` — maturin reads this, and unlike npm it is not rewritten
#    from the git tag at publish time.
cur="$(awk -F'"' '/^version = "/{print $2; exit}' bindings/python/pyproject.toml)"
if [[ "$cur" != "$NEW" ]]; then
  if [[ $CHECK -eq 1 ]]; then report "bindings/python/pyproject.toml" "version" "$cur"; else
    perl -0pi -e 's/^version = "[^"]*"/version = "'"$NEW"'"/m' bindings/python/pyproject.toml
  fi
fi

if [[ $CHECK -eq 1 ]]; then
  if [[ $fail -ne 0 ]]; then
    echo "bump-version.sh: FAILED — run 'bash scripts/bump-version.sh $WS' to set every site" >&2
    exit 1
  fi
  echo "bump-version.sh: OK — every version site is $WS"
  exit 0
fi

# 5. Install pins a reader copy-pastes: `npx -y mushroomdb@X.Y.Z …` in the
#    hand-maintained docs. `check-claims.sh` already fails the build when one
#    of these lags — it was added because 0.6.9 shipped telling readers to
#    install 0.6.8 — so the bump has to move them or every release trips its
#    own gate. CHANGELOG and docs/roadmap are exempt for the same reason
#    check-claims exempts them: they are history, and history keeps its pins.
if git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; then
  while IFS= read -r -d '' f; do
    case "$f" in
      CHANGELOG.md|docs/roadmap/*) continue ;;
    esac
    if grep -qE "mushroomdb@[0-9]+\.[0-9]+\.[0-9]+" "$ROOT/$f" 2>/dev/null; then
      # The `@` must be escaped on BOTH sides: unescaped in the replacement,
      # perl reads `@0` as an array and interpolates it away, turning
      # `mushroomdb@0.6.12` into `mushroomdb.6.12` — which `check-claims.sh`
      # then passes, because the wreckage no longer matches the pattern it
      # scans for. A gate satisfied by text it cannot see is worse than none.
      perl -0pi -e 's/mushroomdb\@[0-9]+\.[0-9]+\.[0-9]+/mushroomdb\@'"$NEW"'/g' "$ROOT/$f"
    fi
  done < <(git -C "$ROOT" ls-files -z -- '*.md' '*.txt' '*.json' '*.sh')
fi

# Regenerate what is derived from the version rather than editing it.
cargo metadata --format-version 1 --offline >/dev/null 2>&1 || cargo metadata --format-version 1 >/dev/null
(cd bindings/python && cargo metadata --format-version 1 >/dev/null 2>&1 || true)
bash scripts/render-plugin.sh >/dev/null
bash scripts/gen-llms-full.sh >/dev/null

echo "bump-version.sh: set every version site to $NEW"
echo "  next: update CHANGELOG.md, then 'bash scripts/bump-version.sh --check'"
