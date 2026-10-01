#!/usr/bin/env bash
# Release gate for the v0.6.10 defect ledger.
#
# Exits non-zero while any defect row is neither CLOSED nor explicitly
# DEFERRED. Task 12 of the implementation plan runs this before it starts.
#
# It parses ONLY between the DEFECT-INDEX markers. The ledger also carries
# per-fix mutation tables whose rows share the index table's shape — 40 rows
# match that shape, of which only 21 are defects — so a naive grep sees
# nineteen phantoms. A gate that can misread its own input is worse than no
# gate, which is why the bounds are explicit rather than inferred.
#
# It also checks that every indexed row has a matching `## N.` section, so a
# row cannot be added to the table without the detail that makes it
# actionable, or removed from the table while its section lingers.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

LEDGER="${1:-$ROOT/docs/roadmap/v0.6.10-defects.md}"
[ -f "$LEDGER" ] || { echo "check-defect-ledger.sh: no ledger at $LEDGER" >&2; exit 1; }

grep -q 'DEFECT-INDEX:BEGIN' "$LEDGER" && grep -q 'DEFECT-INDEX:END' "$LEDGER" || {
  echo "check-defect-ledger.sh: both DEFECT-INDEX markers must be present" >&2
  echo "  without the END marker the parse would run to EOF and swallow the mutation tables" >&2
  exit 1
}

rows="$(awk '/DEFECT-INDEX:BEGIN/{f=1;next} /DEFECT-INDEX:END/{f=0} f && /^\| [0-9]+ \|/' "$LEDGER")"
[ -n "$rows" ] || { echo "check-defect-ledger.sh: no defect rows between the markers" >&2; exit 1; }

total=0; closed=0; deferred=0; open_rows=""
while IFS= read -r row; do
  total=$((total+1))
  num="$(printf '%s' "$row" | awk -F'|' '{gsub(/ /,"",$2); print $2}')"
  # Take the LAST non-empty cell, not `$5`. The Where column is prose about
  # code and regularly contains a literal `|` (Rust patterns, alternations),
  # which shifts a positional field onto the wrong text. Markdown escapes it
  # as `\|`, which awk splits on anyway.
  status="$(printf '%s' "$row" | awk -F'|' '{for (i=NF; i>0; i--) if ($i ~ /[^[:space:]]/) { print $i; exit } }')"
  status="$(printf '%s' "$status" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"

  # Anchored, not a substring glob. `*CLOSED*` would pass a row whose status
  # reads `REOPENED (was CLOSED)` or `NOT CLOSED` — and row #9 really was
  # reopened during this release, so that is a live hazard, not a hypothetical.
  # Trailing prose after the token is fine and common (`**CLOSED** — the guard
  # is now at the choke-point`, `DEFERRED to 0.6.11`); a different word in
  # front of it is not.
  case "$status" in
    '**CLOSED**'*|CLOSED*)     closed=$((closed+1)) ;;
    '**DEFERRED**'*|DEFERRED*) deferred=$((deferred+1)) ;;
    *)                         open_rows="${open_rows}  #${num}: ${status}"$'\n' ;;
  esac
  grep -qE "^## ${num}\. " "$LEDGER" \
    || { echo "check-defect-ledger.sh: row #${num} has no '## ${num}.' section" >&2; exit 1; }
done <<< "$rows"

echo "check-defect-ledger.sh: ${total} defects — ${closed} closed, ${deferred} deferred, $((total-closed-deferred)) open"
if [ -n "$open_rows" ]; then
  printf 'check-defect-ledger.sh: FAILED — these are still open:\n%s' "$open_rows" >&2
  exit 1
fi

# Emits `<row number>|<version>` for each DEFERRED row that names a target.
#
# Written in perl, not awk: the natural expression uses 3-argument
# `match($0, /re/, m)`, a gawk extension. BSD awk (what darwin ships) fails
# at parse time on it. Reuses the bounded DEFECT-INDEX:BEGIN/END window and,
# for the row number, the same "split on |, take field 2" the main parse
# above uses — never a fixed field index for the *status*, because the Where
# column carries literal `|` (Rust patterns, alternations).
deferred_rows_with_targets() {
  perl -ne '
    BEGIN { $inside = 0 }
    if (/DEFECT-INDEX:BEGIN/) { $inside = 1; next }
    if (/DEFECT-INDEX:END/)   { $inside = 0; next }
    next unless $inside;
    next unless /DEFERRED/;
    my @f = split /\|/, $_, -1;
    my $row = defined $f[1] ? $f[1] : "";
    $row =~ s/\D//g;
    next if $row eq "";
    if (/DEFERRED[^0-9]*to[^0-9]*v?([0-9]+\.[0-9]+(?:\.[0-9]+)?)/) {
      print "$row|$1\n";
    }
  ' "$LEDGER"
}

# A deferral to a version that has already shipped is an open defect wearing a
# stale label. Row 36 deferred to 0.6.11; 0.6.11 and 0.6.12 both shipped and
# this gate stayed green, because `DEFERRED*` matched and nothing read the
# target. Compare the named target against the workspace version.
ws="$(awk -F'"' '/^version = "/{print $2; exit}' "$ROOT/Cargo.toml")"
stale=0
while IFS='|' read -r row target; do
  [[ -z "${target// }" ]] && continue
  # Sorts as versions: if the target is <= the shipped workspace version it
  # cannot still be in the future.
  newest="$(printf '%s\n%s\n' "$ws" "$target" | sort -V | tail -1)"
  if [[ "$newest" == "$ws" ]]; then
    echo "check-defect-ledger.sh: row $row defers to $target, which is not after $ws" >&2
    stale=1
  fi
done < <(deferred_rows_with_targets)
if [[ $stale -ne 0 ]]; then
  echo "check-defect-ledger.sh: FAILED — a deferral target has already shipped" >&2
  exit 1
fi

echo "check-defect-ledger.sh: OK — the release gate is satisfied"
