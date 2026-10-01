#!/usr/bin/env bash
# test-check-names.sh — the names gate, shown passing and failing.
#
# A gate must be seen to fail on what it guards. Every case here is a way a
# name got into this tree, or a way the gate could go wrong:
#
#   - in prose, in any letter case;
#   - glued to a suffix inside an identifier, and as a FILE NAME — the three
#     benchmark adapters were exactly this, and a whole-word check passes them;
#   - glued on BOTH sides, in the middle of an identifier — three test class
#     names were exactly this, and a prefix-and-suffix check passes them;
#   - spelled with a diacritic;
#   - and the output must never print the name it found.
#
# Invented names only. No real system's name belongs in a fixture.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GATE="$ROOT/scripts/check-names.py"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

LIST="$TMP/list.sha256"
: > "$LIST"
printf 'ExampleGraph\tsystem A\nOtherStore\tsystem B\n' | python3 "$GATE" --list "$LIST" --add >/dev/null
[[ "$(grep -c -E '^[0-9a-f]{64}$' "$LIST")" -eq 2 ]] || { echo "FAIL: --add did not write two digests"; exit 1; }
if grep -qi -E 'examplegraph|otherstore' "$LIST"; then echo "FAIL: the list file spells a name"; exit 1; fi

fails=0
expect() { # description, expected exit, fixture dir
  local out status=0
  out="$(python3 "$GATE" --list "$LIST" --root "$3" 2>&1)" || status=$?
  if [[ "$status" -ne "$2" ]]; then
    echo "FAIL: $1 — exit $status, wanted $2"; echo "$out" | sed 's/^/    /'; fails=1; return
  fi
  if echo "$out" | grep -qi -E 'examplegraph|otherstore'; then
    echo "FAIL: $1 — the output printed the name it found"; fails=1; return
  fi
  echo "ok: $1"
}

d="$TMP/clean"; mkdir -p "$d"; echo "A graph example, and another store of facts." > "$d/a.md"
expect "ordinary words that merely contain parts of a name pass" 0 "$d"

d="$TMP/prose"; mkdir -p "$d"; echo "It is faster than EXAMPLEGRAPH on this workload." > "$d/a.md"
expect "a name in prose, in capitals, fails" 1 "$d"

d="$TMP/glued"; mkdir -p "$d"; echo "class ExampleGraphAdapter: pass" > "$d/a.py"
expect "a name glued to a suffix in an identifier fails" 1 "$d"

d="$TMP/middle"; mkdir -p "$d"; echo "class TestExampleGraphSkipPath: pass" > "$d/a.py"
expect "a name glued on both sides, in the middle of an identifier, fails" 1 "$d"

d="$TMP/path"; mkdir -p "$d/adapters"; echo "pass" > "$d/adapters/otherstore.py"
expect "a name that is only a file name fails" 1 "$d"

d="$TMP/accent"; mkdir -p "$d"; printf 'Compared with \xc3\x88x\xc3\xa0mpleGraph.\n' > "$d/a.md"
expect "a name spelled with diacritics fails" 1 "$d"

d="$TMP/lock"; mkdir -p "$d"; echo 'name = "examplegraph"' > "$d/Cargo.lock"
expect "a lockfile is exempt: nobody here chose its package names" 0 "$d"

if printf 'abc\tx\n' | python3 "$GATE" --list "$TMP/short.sha256" --add >/dev/null 2>&1; then
  echo "FAIL: --add accepted a three-character name"; fails=1
else
  echo "ok: --add refuses a name too short to match safely"
fi

# --explain is the local diagnostic: it needs the plain list, which is never
# tracked, and it must still not print the name — only where in the token it is
# and which line of the plain list matched.
PLAIN="$TMP/plain.txt"
printf 'ExampleGraph\tsystem A\nOtherStore\tsystem B\n' > "$PLAIN"
out="$(python3 "$GATE" --list "$LIST" --root "$TMP/middle" --plain "$PLAIN" --explain a.py:1 2>&1)" || true
if echo "$out" | grep -qi -E 'examplegraph|otherstore'; then
  echo "FAIL: --explain printed the name it found"; fails=1
elif ! echo "$out" | grep -q -F 'test[blocked]skippath' || ! echo "$out" | grep -q -E 'line 1\b'; then
  echo "FAIL: --explain did not show the masked token and the list line"; echo "$out" | sed 's/^/    /'; fails=1
else
  echo "ok: --explain shows the masked token and the list line, never the name"
fi
# A token can hold two listed names. Reporting on one must not print the other.
d="$TMP/two"; mkdir -p "$d"; echo "class ExampleGraphOtherStoreBridge: pass" > "$d/a.py"
out="$(python3 "$GATE" --list "$LIST" --root "$d" --plain "$PLAIN" --explain a.py:1 2>&1)" || true
if echo "$out" | grep -qi -E 'examplegraph|otherstore'; then
  echo "FAIL: --explain printed one name while reporting the other, in a token holding two"; fails=1
elif ! echo "$out" | grep -q -F '[blocked][blocked]bridge'; then
  echo "FAIL: --explain did not show the token with both names masked"; echo "$out" | sed 's/^/    /'; fails=1
else
  echo "ok: --explain masks every listed name in a token holding two"
fi

# A name on the plain list that was never hashed is still a name: masked too.
printf 'ExampleGraph\tsystem A\nOtherStore\tsystem B\nThirdThing\tsystem C\n' > "$TMP/plain3.txt"
d="$TMP/unhashed"; mkdir -p "$d"; echo "class ExampleGraphThirdThingBridge: pass" > "$d/a.py"
out="$(python3 "$GATE" --list "$LIST" --root "$d" --plain "$TMP/plain3.txt" --explain a.py:1 2>&1)" || true
if echo "$out" | grep -qi -E 'examplegraph|thirdthing'; then
  echo "FAIL: --explain printed a plain-list name that is not on the hashed list"; fails=1
else
  echo "ok: --explain masks a plain-list name even when it was never hashed"
fi

# The same in bare-path mode, with two name-bearing components.
d="$TMP/twopath"; mkdir -p "$d/examplegraph-otherstore"; echo "pass" > "$d/examplegraph-otherstore/otherstoreexamplegraph.py"
out="$(python3 "$GATE" --list "$LIST" --root "$d" --plain "$PLAIN" --explain examplegraph-otherstore/otherstoreexamplegraph.py 2>&1)" || true
if echo "$out" | grep -qi -E 'examplegraph|otherstore'; then
  echo "FAIL: --explain printed a name from a path with two name-bearing components"; fails=1
elif ! echo "$out" | grep -q -F '[blocked][blocked]'; then
  echo "FAIL: --explain did not report the path component holding two names"; echo "$out" | sed 's/^/    /'; fails=1
else
  echo "ok: --explain masks every listed name in a path with two name-bearing components"
fi

# With no digests, the main gate fails; so must --explain, rather than say "nothing found".
: > "$TMP/empty.sha256"
if python3 "$GATE" --list "$TMP/empty.sha256" --root "$TMP/middle" --plain "$PLAIN" --explain a.py:1 >/dev/null 2>&1; then
  echo "FAIL: --explain passed with an empty hashed list"; fails=1
else
  echo "ok: --explain fails closed when the hashed list is empty"
fi

if python3 "$GATE" --list "$LIST" --root "$TMP/middle" --plain "$TMP/absent.txt" --explain a.py:1 >/dev/null 2>&1; then
  echo "FAIL: --explain ran without the plain list"; fails=1
else
  echo "ok: --explain refuses to run without the local plain list"
fi

[[ "$fails" -eq 0 ]] || { echo "test-check-names.sh: FAILED"; exit 1; }
echo "test-check-names.sh: OK"
