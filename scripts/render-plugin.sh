#!/usr/bin/env bash
# render-plugin.sh [--check]
#
# Renders the Claude Code plugin (packaging/plugin/) and the repo-root
# marketplace manifest (.claude-plugin/marketplace.json) from the templates
# in scripts/plugin-templates/, substituting the workspace version — read
# from crates/cli/Cargo.toml, the crate that becomes the published
# `mushroomdb` npm package/binary — into every `mushroomdb@<version>` pin.
#
# The version is never hardcoded here: it tracks whatever crates/cli/Cargo.toml
# says right now, so a version bump (Task 21) changes what gets rendered
# without touching this script, and `--check` catches a plugin file that was
# hand-edited or left stale after such a bump.
#
# --check renders into a temp directory and diffs the result against the
# committed files, exiting 1 on any drift. Used by CI (`plugin-validate` job).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMPLATES="$ROOT/scripts/plugin-templates"
CHECK=0
if [[ "${1:-}" == "--check" ]]; then
  CHECK=1
fi

VERSION="$(grep -m1 '^version *= *"' "$ROOT/crates/cli/Cargo.toml" | sed -E 's/^version *= *"([^"]+)".*/\1/')"
if [[ -z "$VERSION" ]]; then
  echo "render-plugin.sh: could not read version from crates/cli/Cargo.toml" >&2
  exit 1
fi
BIN="npx -y mushroomdb@${VERSION}"

# render_file <template-or-source> <dest>
#
# The CLI's own skill template (crates/cli/skills/mushroom/SKILL.md) is
# rendered directly rather than duplicated: {{BIN}} and {{DB_PATH}} are
# substituted exactly as crates/cli/src/install.rs's render_template() does,
# so the plugin skill never drifts from the skill `mushroomdb install` writes.
render_file() {
  local src="$1" dest="$2"
  sed -e "s|{{VERSION}}|${VERSION}|g" \
      -e "s|{{BIN}}|${BIN}|g" \
      -e "s|{{DB_PATH}}|./mushroom-memory|g" \
      "$src" > "$dest"
}

# render_skill <dest>
#
# Same as render_file, plus a plugin-only fixup: the shared skill template
# names its own invocation as `/mushroom` (correct for a `mushroomdb install`
# project/user skill, invoked bare) but a plugin skill is namespaced by
# Claude Code as `/<plugin-name>:<skill-name>` — verified empirically against
# a live `claude --plugin-dir` session, where a plugin named "mushroom" with
# a skill named "mushroom" registers only as `mushroom:mushroom`, never bare.
# The shared source file is left untouched (the npx/install path still wants
# `/mushroom`); only the rendered plugin copy gets the two exact strings that
# name the invocation corrected. This must stay exact-string, not a blanket
# `s|/mushroom|...|g` — the same render also substitutes {{DB_PATH}} to
# `./mushroom-memory`, and a blanket pattern would mangle that into
# `./mushroom:mushroom-memory`.
render_skill() {
  local src="$1" dest="$2"
  render_file "$src" "$dest"
  sed -i.bak \
      -e 's|^# /mushroom$|# /mushroom:mushroom|' \
      -e 's|`/mushroom learn <path>`|`/mushroom:mushroom learn <path>`|' \
      "$dest"
  rm -f "$dest.bak"
}

if [[ "$CHECK" -eq 1 ]]; then
  OUT="$(mktemp -d)"
  trap 'rm -rf "$OUT"' EXIT
else
  OUT="$ROOT"
fi

mkdir -p \
  "$OUT/packaging/plugin/.claude-plugin" \
  "$OUT/packaging/plugin/skills/mushroom" \
  "$OUT/packaging/plugin/hooks" \
  "$OUT/.claude-plugin"

# Relative paths (under $ROOT / $OUT), reused for both writing and diffing.
FILES=(
  "packaging/plugin/.claude-plugin/plugin.json"
  "packaging/plugin/.mcp.json"
  "packaging/plugin/hooks/hooks.json"
  ".claude-plugin/marketplace.json"
  "packaging/plugin/skills/mushroom/SKILL.md"
)

render_file "$TEMPLATES/plugin.json.tmpl"                     "$OUT/packaging/plugin/.claude-plugin/plugin.json"
render_file "$TEMPLATES/mcp.json.tmpl"                        "$OUT/packaging/plugin/.mcp.json"
render_file "$TEMPLATES/hooks.json.tmpl"                      "$OUT/packaging/plugin/hooks/hooks.json"
render_file "$TEMPLATES/marketplace.json.tmpl"                "$OUT/.claude-plugin/marketplace.json"
render_skill "$ROOT/crates/cli/skills/mushroom/SKILL.md"      "$OUT/packaging/plugin/skills/mushroom/SKILL.md"

if [[ "$CHECK" -eq 1 ]]; then
  fail=0
  for f in "${FILES[@]}"; do
    if [[ ! -f "$ROOT/$f" ]]; then
      echo "render-plugin.sh --check: $f is missing" >&2
      fail=1
      continue
    fi
    if ! diff -u "$ROOT/$f" "$OUT/$f"; then
      fail=1
    fi
  done
  if [[ "$fail" -ne 0 ]]; then
    echo "render-plugin.sh --check: rendered output differs from committed files (version ${VERSION}) — see diff above" >&2
    exit 1
  fi
  echo "render-plugin.sh --check: OK (version ${VERSION})"
else
  echo "rendered plugin files at version ${VERSION}"
fi
