#!/usr/bin/env bash
# check-claims.sh — the v0.6.4 claim scrub.
#
# Two rules, both a consequence of the measured 0.6.2 result (240 cells: no
# correctness gain, +20% cost when used, 0 graph calls in 120 unprompted
# sessions — benchmarks/agent-tasks/results/20260910T000418Z/summary.md):
#
#   1. No product-facing file claims a coding-speed or token benefit. Measured
#      latency claims ("3.3x faster", "faster cold open") are about the engine
#      and are deliberately not matched.
#   2. No skill or rules file tells an agent to reach for the graph before or
#      instead of a search. That instruction is what the benchmark measured and
#      it is not worth what it cost.
#   3. The code-graph door removed in 0.7 is not referenced again from source,
#      and no product-facing file names one of its three retired install flags.
#
# The claim scan covers every product-facing surface: the README, the llms
# files, the plugin manifests and their templates, every page under docs/site
# (the Markdown pages and the published index.html), and the three skill files.
# The grep-redirect scan covers the skill files alone, because only a skill can
# instruct an agent.
#
# Exits 1 printing file:line for every hit. Run by the plugin-validate CI job.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail=0

GREP_FILES=(
  crates/cli/skills/mushroom/SKILL.md
  crates/cli/skills/mushroom/cursor-rules.mdc
  packaging/plugin/skills/mushroom/SKILL.md
)

CLAIM_FILES=(
  README.md
  llms.txt
  llms-full.txt
  packaging/plugin/README.md
  packaging/plugin/.claude-plugin/plugin.json
  .claude-plugin/marketplace.json
  scripts/plugin-templates/plugin.json.tmpl
  scripts/plugin-templates/marketplace.json.tmpl
  docs/site/index.html
)
while IFS= read -r f; do CLAIM_FILES+=("$f"); done < <(ls docs/site/*.md)
# The skill files carry product-facing copy too — a retired claim reads the same
# in a skill as it does in the README — so they take both scans, not just the
# grep-redirect one below.
CLAIM_FILES+=("${GREP_FILES[@]}")

CLAIM_PATTERNS=(
  '(code|coding|ship|shipping|develop|work)(s|ing)?[[:space:]]+faster'
  'faster[[:space:]]+(coding|development|sessions?|agents?|turns?)'
  'fewer[[:space:]]+tokens'
  '(save|saves|saving)[[:space:]]+(you[[:space:]]+)?tokens'
  'token[[:space:]]+savings'
  'cheaper[[:space:]]+(sessions?|turns?|coding|agents?)'
  '(beats|outperforms)[[:space:]]+(a[[:space:]]+)?stock'
  # A scale that was never run. 100,000 nodes is the largest store measured;
  # "10M nodes" was a design intent stated as a target (spec section 6.4).
  '10[[:space:]]?M[[:space:]-]+nodes?|10[[:space:]]million[[:space:]]nodes'
)

GREP_PATTERNS=(
  'before[[:space:]]+(any[[:space:]]+)?`?Grep'
  'instead[[:space:]]+of[[:space:]]+`?[Gg]rep'
)

# Two hand-unrolled scans rather than one generic function taking array names:
# `local -n` (nameref) needs bash 4.3+, and stock macOS `/bin/bash` is 3.2.57.
scan_claim_files() {
  local f p
  for f in "${CLAIM_FILES[@]}"; do
    [[ -f "$f" ]] || continue
    for p in "${CLAIM_PATTERNS[@]}"; do
      if grep -nEi "$p" "$f"; then
        echo "check-claims.sh: $f matches the retired-claim pattern /$p/" >&2
        fail=1
      fi
    done
  done
}

scan_grep_files() {
  local f p
  for f in "${GREP_FILES[@]}"; do
    [[ -f "$f" ]] || continue
    for p in "${GREP_PATTERNS[@]}"; do
      if grep -nEi "$p" "$f"; then
        echo "check-claims.sh: $f matches the grep-redirect pattern /$p/" >&2
        fail=1
      fi
    done
  done
}

scan_claim_files
scan_grep_files

# Rule 3: the code-graph door was removed in 0.7 and does not come back by
# accident. A deprecated feature that still ships is a feature that still has
# to work, which is why this runs instead of a comment asking people to
# remember.
#
# Comment lines are skipped: surviving doc comments say what moved out of
# `repograph` in 0.7, and a gate that fires on history is a gate someone
# disables. A declaration (`pub mod repograph;`) or a path (`repograph::`) in
# code is not a comment and is caught. `crates/code-extract/tests/` is skipped
# because its fixtures hold `repograph::render::sanitize` as a parser input,
# not a reference to anything.
#
# The seven tool names are not scanned for: `map`, `why`, `context`, `owners`,
# `explore` and `sync` are ordinary words and `why` is a live CLI subcommand.
# The MCP handshake test pins the tool listing instead.
RETIRED_SOURCE=(repograph)
for pat in "${RETIRED_SOURCE[@]}"; do
  hits="$(git grep -n -- "$pat" -- 'crates/*' ':!crates/code-extract/tests/*' 2>/dev/null \
          | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' || true)"
  if [[ -n "$hits" ]]; then
    echo "check-claims.sh: '$pat' was removed in 0.7 and is referenced again:" >&2
    printf '%s\n' "$hits" >&2
    fail=1
  fi
done

# Rule 3, the flag half: the three opt-in hooks `install` wrote in 0.6 are
# rejected in 0.7, so no product-facing file may tell a reader to pass one.
# CHANGELOG.md is not a claim file and is not scanned: its v0.7.0 section
# names the flags on purpose, as the record of what was removed.
RETIRED_FLAGS=(--intercept-grep --impact-before-edit --enrich-grep)
for f in "${CLAIM_FILES[@]}"; do
  [[ -f "$f" ]] || continue
  for pat in "${RETIRED_FLAGS[@]}"; do
    if hits="$(grep -n -- "$pat" "$f")"; then
      echo "check-claims.sh: $f names the retired flag $pat:" >&2
      printf '%s\n' "$hits" | sed "s|^|  $f:|" >&2
      fail=1
    fi
  done
done

# The stub-docstring drift check. Separate script, one gate: a caller reading a
# thinner contract than the binding carries is the same class of defect as a
# retired claim, and CI already runs this one script.
if ! bash "$ROOT/scripts/check-pyi.sh"; then
  fail=1
fi

# The names gate: nothing tracked names another system. Separate script, one
# gate, for the reason check-pyi.sh is: CI already runs this one.
if ! python3 "$ROOT/scripts/check-names.py"; then
  fail=1
fi

# Install pins that name a version. `llms.txt` carries a copy-pasteable Claude
# Desktop config; it was bumped in every release commit through 0.6.8, then
# silently skipped by 0.6.9's and 0.6.10's, so 0.6.9 shipped telling readers to
# install 0.6.8. `llms-full.txt` and the plugin files are generated and cannot
# drift, but the hand-maintained ones can, and nothing was watching them.
WORKSPACE_VERSION="$(awk -F'"' '/^version = "/{print $2; exit}' "$ROOT/Cargo.toml")"
if [[ -n "$WORKSPACE_VERSION" ]]; then
  # A complete version only. `mushroomdb@0.6.x` is a deliberate *series*
  # reference in the deprecation table, not a pin anyone copy-pastes.
  # Tracked files only. The question is what a *reader* copy-pastes, and a
  # reader gets the repository — not a local build directory, a virtualenv, or
  # a gitignored scratch tree. Walking the whole working copy made this fail on
  # any machine that had done SDD work, whose session ledgers quote the version
  # that was current when they were written, while CI saw a clean checkout and
  # passed. A gate that only fires locally, on files nobody ships, teaches
  # people to ignore it.
  if git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; then
    mapfile -d '' -t _scan < <(git -C "$ROOT" ls-files -z -- \
      '*.md' '*.txt' '*.json' '*.sh' 2>/dev/null)
  else
    # Outside a checkout (a source tarball): fall back to walking the tree.
    mapfile -d '' -t _scan < <(find "$ROOT" \
      \( -name target -o -name 'target-*' -o -name .venv -o -name node_modules \) -prune -o \
      \( -name '*.md' -o -name '*.txt' -o -name '*.json' -o -name '*.sh' \) -print0)
  fi
  stale=""
  if [[ ${#_scan[@]} -gt 0 ]]; then
    stale="$( (cd "$ROOT" && grep -nE "mushroomdb@[0-9]+\.[0-9]+\.[0-9]+" "${_scan[@]}" 2>/dev/null) \
             | grep -v "mushroomdb@${WORKSPACE_VERSION}" \
             | grep -vE "(^|/)CHANGELOG\.md:|(^|/)docs/roadmap/" || true)"
  fi
  # A pin the rewriter mangled: `mushroomdb` immediately followed by a version
  # with no `@`. This exists because the bump script once produced exactly that
  # — perl interpolated `@0` away — and this scan passed it, because the
  # wreckage no longer matched the pattern above. A gate satisfied by text it
  # cannot see is worse than no gate.
  mangled=""
  if [[ ${#_scan[@]} -gt 0 ]]; then
    # `scripts/` is excluded: the bump script's own comment describes this
    # exact wreckage, and a pin inside a script is not reader-facing copy.
    mangled="$( (cd "$ROOT" && grep -nE "mushroomdb\.[0-9]+\.[0-9]+" "${_scan[@]}" 2>/dev/null) \
               | grep -vE "(^|/)CHANGELOG\.md:|(^|/)docs/roadmap/|(^|/)scripts/" || true)"
  fi
  if [[ -n "$mangled" ]]; then
    echo "check-claims.sh: install pins with the '@' missing — a mangled rewrite:" >&2
    printf '%s\n' "$mangled" >&2
    fail=1
  fi
  if [[ -n "$stale" ]]; then
    echo "check-claims.sh: install pins naming a version other than ${WORKSPACE_VERSION}:" >&2
    printf '%s\n' "$stale" >&2
    echo "  (a reader copy-pastes these; the historical CHANGELOG and docs/roadmap are exempt)" >&2
    fail=1
  fi
fi

if [[ "$fail" -ne 0 ]]; then
  echo "check-claims.sh: FAILED — see the lines above" >&2
  exit 1
fi
echo "check-claims.sh: OK"
