#!/usr/bin/env bash
# test-check-links.sh — the link gate, shown passing and failing.
#
# A gate must be seen to fail on what it guards. The gate reads the tree it
# sits in, so each case builds a small git repository with a copy of it:
#
#   - a page the sidebar does not list is never rendered, and nothing says so;
#   - a page the index does not link is reachable from nowhere a reader starts;
#   - a relative link from a site page to a file outside the book works on
#     GitHub and is a 404 on the published site;
#   - the same link from a file that is not a site page is fine.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fails=0

fixture() { # name -> path of a repository whose site passes the gate
  local d="$TMP/$1"
  mkdir -p "$d/scripts" "$d/docs/site" "$d/docs/assets"
  cp "$ROOT/scripts/check-links.py" "$d/scripts/"
  printf '# Readme\n\nSee [the docs](docs/site/index.md).\n' > "$d/README.md"
  printf '# Home\n\n- [A](a.md)\n' > "$d/docs/site/index.md"
  printf '# A\n\n![shot](../assets/shot.gif)\n' > "$d/docs/site/a.md"
  printf '# Summary\n\n[Home](index.md)\n\n- [A](a.md)\n' > "$d/docs/site/SUMMARY.md"
  : > "$d/docs/assets/shot.gif"
  git -C "$d" init -q
  echo "$d"
}

expect() { # description, expected exit, repository, [text the output must contain]
  local out status=0
  git -C "$3" add -A
  out="$(python3 "$3/scripts/check-links.py" 2>&1)" || status=$?
  if [[ "$status" -ne "$2" ]]; then
    echo "FAIL: $1 — exit $status, wanted $2"; echo "$out" | sed 's/^/    /'; fails=1; return
  fi
  if [[ -n "${4:-}" ]] && ! grep -qF -- "$4" <<<"$out"; then
    echo "FAIL: $1 — output lacks: $4"; echo "$out" | sed 's/^/    /'; fails=1; return
  fi
  echo "ok: $1"
}

d="$(fixture clean)"
expect "a site whose pages are in the index and the sidebar passes" 0 "$d"

d="$(fixture unlisted)"
printf '# B\n' > "$d/docs/site/b.md"
printf '# Home\n\n- [A](a.md)\n- [B](b.md)\n' > "$d/docs/site/index.md"
expect "a page missing from the sidebar fails" 1 "$d" "UNLISTED  docs/site/b.md"

d="$(fixture unindexed)"
printf '# B\n' > "$d/docs/site/b.md"
printf '# Summary\n\n[Home](index.md)\n\n- [A](a.md)\n- [B](b.md)\n' > "$d/docs/site/SUMMARY.md"
expect "a page missing from the index still fails" 1 "$d" "UNINDEXED docs/site/b.md"

d="$(fixture nosummary)"
rm "$d/docs/site/SUMMARY.md"
expect "a site with no sidebar fails on every page" 1 "$d" "UNLISTED  docs/site/index.md"

d="$(fixture offsite)"
printf '# A\n\nSee the [readme](../../README.md).\n' > "$d/docs/site/a.md"
expect "a site page linking a file outside the book fails" 1 "$d" "OFFSITE   docs/site/a.md:3: ../../README.md"

d="$(fixture offsite-anchor)"
printf '# A\n\nSee [install](../../README.md#readme).\n' > "$d/docs/site/a.md"
expect "the same link with an anchor fails the same way" 1 "$d" "OFFSITE   docs/site/a.md:3"

d="$(fixture absolute)"
printf '# A\n\nSee the [readme](https://example.com/README.md).\n' > "$d/docs/site/a.md"
expect "an absolute link from a site page passes" 0 "$d"

d="$(fixture fenced)"
printf '# A\n\n```\n[readme](../../README.md)\n```\n' > "$d/docs/site/a.md"
expect "a link inside a fenced block is not a link" 0 "$d"

d="$(fixture reference)"
printf '# A\n\nSee the [readme][r].\n\n[r]: ../../README.md\n' > "$d/docs/site/a.md"
expect "a reference-style link out of the book fails" 1 "$d" "OFFSITE   docs/site/a.md:5: ../../README.md"

d="$(fixture anchor-tag)"
printf '# A\n\nSee the <a href="../../README.md">readme</a>.\n' > "$d/docs/site/a.md"
expect "an <a href> out of the book fails" 1 "$d" "OFFSITE   docs/site/a.md:3: ../../README.md"

d="$(fixture footnote)"
printf '# A\n\nA claim.[^1]\n\n[^1]: Where it came from.\n' > "$d/docs/site/a.md"
expect "a footnote definition is not a link" 0 "$d"

d="$(fixture to-sidebar)"
printf '# A\n\nSee the [contents](SUMMARY.md).\n' > "$d/docs/site/a.md"
expect "a link to the sidebar file, which is never published, fails" 1 "$d" "OFFSITE   docs/site/a.md:3: SUMMARY.md"

d="$(fixture nested)"
mkdir -p "$d/docs/site/sub"
printf '# Deep\n' > "$d/docs/site/sub/deep.md"
expect "a page in a subdirectory, which neither list can see, fails" 1 "$d" "NESTED    docs/site/sub/deep.md"

[[ $fails -eq 0 ]] || exit 1
echo "test-check-links.sh: OK"
