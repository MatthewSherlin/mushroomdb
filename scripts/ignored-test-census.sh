#!/usr/bin/env bash
# How many #[ignore]d tests exist, and how many does CI actually run?
#
# A gate nothing runs is indistinguishable from a gate that passes. Three
# releases reported "two of three HNSW gates red" while two of those three had
# never executed. This prints the ratio so that is never again invisible.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

mapfile -t ignored < <(
  grep -rn --include='*.rs' -A3 '#\[ignore' crates/ \
    | perl -ne 'print "$1\n" if /fn\s+([a-z0-9_]+)/' \
    | sort -u
)

# Every test name that appears as a filter argument between `-- ` and
# `--ignored` in any workflow is executed. This repo's convention is
# `cargo test ... -- <name> [<name>...] --ignored --nocapture`, not `--exact`;
# grepping for `--exact` here would find zero matches, since no CI line uses
# that flag. A line may name more than one test (cargo's filter is an OR over
# every argument), so the captured span is split on whitespace rather than
# assumed to be a single word.
mapfile -t executed < <(
  grep -rhoE -- '-- [a-z0-9_ ]+ --ignored' .github/workflows/ \
    | sed -E 's/^-- //; s/ --ignored$//' \
    | tr ' ' '\n' | sort -u
)

declare -A run=()
for t in "${executed[@]:-}"; do run["$t"]=1; done

unrun=()
for t in "${ignored[@]:-}"; do [[ -n "${run[$t]:-}" ]] || unrun+=("$t"); done

echo "ignored-test census"
echo "  #[ignore]d tests in crates/ : ${#ignored[@]}"
echo "  named in a workflow         : $(( ${#ignored[@]} - ${#unrun[@]} ))"
echo "  NEVER EXECUTED ANYWHERE     : ${#unrun[@]}"
if ((${#unrun[@]})); then
  printf '    %s\n' "${unrun[@]}"
fi
echo "ignored-test-census.sh: reported (informational; does not fail the build)"
