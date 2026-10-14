#!/usr/bin/env bash
# How many #[ignore]d tests exist, and how many does CI actually run?
#
# A gate nothing runs is indistinguishable from a gate that passes. Three
# releases reported "two of three HNSW gates red" while two of those three had
# never executed. This prints the ratio so that is never again invisible.
#
# > **Correction, 2026-09-29.** The message body of commit ac6a06c ("test:
# > unrun assertions are counted where someone will see them") claimed this
# > script reported a post-split census of 28 ignored / 9 named / 19 never
# > executed, against the tree that commit itself created. That triple is
# > wrong: run against ac6a06c, this script reports 30 / 9 / 21. The commit's
# > pre-split baseline (28 / 8 / 20) is correct; only the post-split figure in
# > its body is not. Deliberately not restated here as "the correct number" —
# > it is a fact about one commit, not a live one, and will not track future
# > tests either way. Run this script for what is true of the tree right now.
#
# Exits 1 if any #[ignore]d test is neither named in a workflow nor listed in
# scripts/ignored-tests-allowed.txt.  Add a new never-run test to that file
# (with a one-line reason) instead of leaving it invisible.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ALLOWLIST="$ROOT/scripts/ignored-tests-allowed.txt"

# Collect #[ignore]d test function names.
# Uses Perl to avoid two bugs in the grep -A3 approach:
#
#   Bug 1 (fixed): grep '#\[ignore' matches "#[ignore]" inside line comments
#   (e.g. "//! #[ignore]" in a module-level doc comment) and block comment
#   interiors.  The Perl pass strips line-comment tails and tracks block-comment
#   state so only genuine attribute lines are counted.
#
#   Bug 2 (fixed): a fixed -A3 window misses fn names that appear more than
#   three lines after #[ignore] when multiple attributes are stacked.  The Perl
#   pass scans forward across any number of stacked #[...] attribute lines until
#   it finds the fn name.
mapfile -t ignored < <(
  find crates/ -name '*.rs' -print0 \
    | xargs -0 perl -e '
use strict; use warnings;
while (my $file = shift @ARGV) {
    open(my $fh, "<", $file) or next;
    my @lines = <$fh>;
    close $fh;
    my $in_block = 0;  # inside a /* ... */ block comment
    my $found_ignore = 0;
    for my $line (@lines) {
        # Maintain block-comment state (simplified: does not handle "/*" inside
        # a string literal, which does not occur in these test files).
        if ($in_block) {
            $in_block = 0 if index($line, "*/") >= 0;
            next;
        }
        if (index($line, "/*") >= 0 && index($line, "*/") < 0) {
            $in_block = 1;
        }
        # Strip everything from // onward (handles ///, //!, // inline).
        # After stripping, a genuine #[ignore] attribute starts the line.
        (my $code = $line) =~ s|//.*||;
        if ($code =~ /^\s*#\[ignore/) {
            $found_ignore = 1;
            next;
        }
        if ($found_ignore) {
            if ($code =~ /\bfn\s+([a-z][a-z0-9_]*)/) {
                print "$1\n";
                $found_ignore = 0;
            } elsif ($code =~ /^\s*#\[/) {
                # Another stacked attribute — keep scanning.
            } elsif ($code =~ /\S/) {
                # Non-attribute, non-fn non-blank line — abandon this ignore.
                $found_ignore = 0;
            }
        }
    }
}
' \
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

# Load the allow-list.  Format: one entry per non-comment, non-blank line —
#   crate/path/to/file.rs::fn_name — reason
# Lines starting with '#' and blank lines are skipped.  The fn name is the
# identifier between the last '::' and the first non-identifier character.
declare -A allowed=()
if [[ -f "$ALLOWLIST" ]]; then
  while IFS= read -r entry; do
    [[ "$entry" =~ ^[[:space:]]*# ]] && continue
    [[ -z "${entry//[[:space:]]/}" ]] && continue
    fn_name="$(printf '%s' "$entry" | sed -E 's|^[^:]+::([a-z0-9_]+).*|\1|')"
    [[ -n "$fn_name" ]] && allowed["$fn_name"]=1
  done < "$ALLOWLIST"
fi

# Tests that are neither run by a workflow nor in the allow-list.
unrun=()
for t in "${ignored[@]:-}"; do
  if [[ -z "${run[$t]:-}" ]] && [[ -z "${allowed[$t]:-}" ]]; then
    unrun+=("$t")
  fi
done

# Tests that are in the allow-list (never run by a workflow, deliberate).
n_allowed=0
for t in "${ignored[@]:-}"; do
  if [[ -z "${run[$t]:-}" ]] && [[ -n "${allowed[$t]:-}" ]]; then
    (( n_allowed++ )) || true
  fi
done

echo "ignored-test census"
echo "  #[ignore]d tests in crates/ : ${#ignored[@]}"
echo "  named in a workflow         : $(( ${#ignored[@]} - ${#unrun[@]} - n_allowed ))"
echo "  in allow-list (never run)   : $n_allowed"
echo "  NEVER EXECUTED, NOT ALLOWED : ${#unrun[@]}"
if ((${#unrun[@]})); then
  printf '    %s\n' "${unrun[@]}"
fi

if ((${#unrun[@]})); then
  echo "ignored-test-census.sh: FAILED — add the test(s) to scripts/ignored-tests-allowed.txt or name them in a workflow step" >&2
  exit 1
fi
echo "ignored-test-census.sh: OK"
