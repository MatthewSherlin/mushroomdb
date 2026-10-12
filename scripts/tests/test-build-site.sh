#!/usr/bin/env bash
# test-build-site.sh — the site build, shown passing and failing.
#
# build-site.sh builds the tree it sits in, so each case is a small git
# repository with a copy of it, this repository's book.toml and theme, and a
# two-page site. Every failing case is a way a broken site could be published
# with mdBook exiting 0:
#
#   - a page the sidebar omits is not rendered;
#   - a stray file in docs/site is copied over the page of the same name;
#   - a page in a subdirectory is seen by none of the checks;
#   - the landing page links a page that does not exist;
#   - a heading that ends in `{braces}` loses them, and its anchor, to mdBook's
#     heading-attribute syntax;
#   - and the output directory is deleted before the build, so the script
#     must not accept a directory it did not build.
#
# Needs the mdBook that build-site.sh pins. It does not skip without it: a
# test that skips is a gate that cannot see what it guards.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

command -v "${MDBOOK:-mdbook}" >/dev/null 2>&1 \
  || { echo "test-build-site.sh: mdbook is not on PATH (set MDBOOK to its path)" >&2; exit 1; }

fails=0

fixture() { # name -> path of a repository whose site builds
  local d="$TMP/$1"
  mkdir -p "$d/scripts" "$d/docs/site" "$d/docs/assets" "$d/docs/landing"
  cp "$ROOT/scripts/build-site.sh" "$d/scripts/"
  cp "$ROOT/book.toml" "$d/"
  cp -R "$ROOT/docs/theme" "$d/docs/theme"
  printf '# Home\n\n- [A](a.md)\n' > "$d/docs/site/index.md"
  printf '# A\n\nThe tool is rename_node.\n' > "$d/docs/site/a.md"
  printf '# Summary\n\n[Home](index.md)\n\n- [A](a.md)\n' > "$d/docs/site/SUMMARY.md"
  printf '<!doctype html><title>t</title><a href="docs/">Docs</a> <a href="docs/a.html">A</a> <img src="assets/mark.svg" alt="">\n' \
    > "$d/docs/landing/index.html"
  : > "$d/docs/assets/mark.svg"
  git -C "$d" init -q
  echo "$d"
}

expect() { # description, expected exit, repository, text the output must contain, [argument]
  local out status=0
  out="$(cd "$3" && bash scripts/build-site.sh ${5:+"$5"} 2>&1)" || status=$?
  if [[ "$status" -ne "$2" ]]; then
    echo "FAIL: $1 — exit $status, wanted $2"; echo "$out" | sed 's/^/    /'; fails=1; return
  fi
  if ! grep -qF -- "$4" <<<"$out"; then
    echo "FAIL: $1 — output lacks: $4"; echo "$out" | sed 's/^/    /'; fails=1; return
  fi
  echo "ok: $1"
}

d="$(fixture clean)"
expect "a two-page site builds" 0 "$d" "build-site.sh: OK"
[[ -f "$d/_site/docs/a.html" && -f "$d/_site/a.md" && -f "$d/_site/index.html" ]] \
  || { echo "FAIL: the built site lacks a rendered page, its Markdown or the landing page"; fails=1; }
expect "a second build replaces the first" 0 "$d" "build-site.sh: OK"

d="$(fixture unlisted)"
printf '# B\n' > "$d/docs/site/b.md"
expect "a page the sidebar omits fails" 1 "$d" "docs/site/b.md was not rendered"

d="$(fixture stray)"
printf '<p>not the page</p>\n' > "$d/docs/site/a.html"
expect "a stray file beside the pages fails" 1 "$d" "docs/site/a.html"

d="$(fixture nested)"
mkdir -p "$d/docs/site/sub"
printf '# Deep\n' > "$d/docs/site/sub/deep.md"
expect "a subdirectory of pages fails" 1 "$d" "docs/site/sub"

d="$(fixture dead-link)"
sed -i.bak 's#docs/a.html#docs/nope.html#' "$d/docs/landing/index.html" && rm "$d/docs/landing/index.html.bak"
expect "a landing-page link to nothing fails" 1 "$d" "the landing page links docs/nope.html"

d="$(fixture braces)"
printf '# A\n\nThe tool is rename_node.\n\n### GET /node/{key}\n\nReads a node.\n' > "$d/docs/site/a.md"
expect "a heading ending in {braces}, which mdBook takes for attributes and cuts off, fails" 1 "$d" "a.html"

d="$(fixture braces-in-code)"
printf '# A\n\nThe tool is rename_node.\n\n### `GET /node/{key}`\n\nReads a node.\n' > "$d/docs/site/a.md"
expect "the same heading in a code span is rendered whole" 0 "$d" "build-site.sh: OK"
grep -q 'GET /node/{key}' "$d/_site/docs/a.html" \
  || { echo "FAIL: the heading in a code span lost its braces"; fails=1; }

d="$(fixture unknown-flag)"
expect "an unknown flag is refused" 2 "$d" "usage:" "--help"
[[ ! -e "$d/--help" ]] || { echo "FAIL: an unknown flag became a directory"; fails=1; }

d="$(fixture not-ours)"
mkdir -p "$d/keep"; echo "work" > "$d/keep/notes.txt"
expect "a directory this script did not build is not deleted" 1 "$d" "refusing to replace" "keep"
[[ -f "$d/keep/notes.txt" ]] || { echo "FAIL: the build deleted a directory it was pointed at"; fails=1; }

[[ $fails -eq 0 ]] || exit 1
echo "test-build-site.sh: OK"
