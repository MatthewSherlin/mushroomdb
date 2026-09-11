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

if [[ "$fail" -ne 0 ]]; then
  echo "check-claims.sh: FAILED — see the lines above" >&2
  exit 1
fi
echo "check-claims.sh: OK"
