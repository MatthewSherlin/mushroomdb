#!/usr/bin/env bash
# build-site.sh — the published site, built and checked.
#
# Usage: build-site.sh [out-dir]        build into out-dir (default: _site);
#                                       out-dir is deleted first, so it must be
#                                       empty, absent, or an earlier build
#        build-site.sh --mdbook-version print the mdBook version this needs
#        build-site.sh --mdbook-sha256  print the SHA-256 of that version's
#                                       x86_64-unknown-linux-gnu tarball
#
# The site is four things side by side:
#
#   out/index.html   the landing page, hand-written (docs/landing/)
#   out/docs/        the book: docs/site/*.md rendered by mdBook
#   out/assets/      docs/assets, which both of the above reference
#   out/<page>.md    each page's Markdown, at the URL it has always had
#
# Before this script the workflow copied docs/site as it was: the pages were
# served as text and the landing page linked to none of them. So the build
# does not stop at "mdBook exited 0". It fails if docs/site holds anything but
# top-level Markdown pages, if a page was not rendered, if mdBook warned, if
# a heading lost its {braces}, if the landing page links something the site
# lacks, or if the book's front page is not the book's.
set -euo pipefail

MDBOOK_VERSION="0.5.4"
MDBOOK_SHA256_LINUX_X86_64="3f28de05dafca9d0f2eab99c662116b0e37b89b1d96a08f8f430b9eeae958cd7"

case "${1:-}" in
  --mdbook-version) echo "$MDBOOK_VERSION"; exit 0 ;;
  --mdbook-sha256)  echo "$MDBOOK_SHA256_LINUX_X86_64"; exit 0 ;;
  -*) echo "usage: build-site.sh [out-dir] | --mdbook-version | --mdbook-sha256" >&2; exit 2 ;;
esac

ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
OUT="${1:-$ROOT/_site}"
case "$OUT" in /*) ;; *) OUT="$PWD/$OUT" ;; esac
MDBOOK="${MDBOOK:-mdbook}"

die() { echo "build-site.sh: $*" >&2; exit 1; }

command -v "$MDBOOK" >/dev/null 2>&1 \
  || die "mdbook is not on PATH — this build needs v$MDBOOK_VERSION (set MDBOOK to its path)"
have="$("$MDBOOK" --version | awk '{print $2}' | sed 's/^v//')"
[[ "$have" == "$MDBOOK_VERSION" ]] \
  || die "mdbook is v$have, and this build needs v$MDBOOK_VERSION"

# mdBook renders only what SUMMARY.md lists and copies every other file
# across, over a rendered page of the same name. So the source is top-level
# Markdown and nothing else: no subdirectory, no stray file.
strays="$(cd "$ROOT" && find docs/site -mindepth 1 \( -type d -o ! -name '*.md' \) ! -name '.DS_Store' | sort)"
[[ -z "$strays" ]] \
  || die "docs/site holds something that is not a top-level Markdown page:"$'\n'"$strays"

# The output directory is deleted before the build. The marker is how a
# later run knows the directory is its own.
MARKER=".built-by-build-site"
if [[ -e "$OUT" ]]; then
  [[ -d "$OUT" ]] || die "$OUT exists and is not a directory"
  if [[ -n "$(ls -A "$OUT")" && ! -f "$OUT/$MARKER" ]]; then
    die "$OUT is not empty and was not built by this script — refusing to replace it"
  fi
fi
rm -rf "$OUT"
mkdir -p "$OUT"
: > "$OUT/$MARKER"

log="$(mktemp)"
trap 'rm -f "$log"' EXIT
if ! (cd "$ROOT" && "$MDBOOK" build -d "$OUT/docs") >"$log" 2>&1; then
  cat "$log" >&2
  die "mdbook build failed"
fi

cp "$ROOT/docs/landing/index.html" "$OUT/index.html"
cp -R "$ROOT/docs/assets" "$OUT/assets"
for path in "$ROOT"/docs/site/*.md; do
  [[ "$(basename "$path")" == "SUMMARY.md" ]] || cp "$path" "$OUT/"
done
# GitHub Pages serves /404.html for any missing path. mdBook's is built
# against the book's absolute URL, so it works from the site root too.
cp "$OUT/docs/404.html" "$OUT/404.html"

problems=0
bad() { echo "build-site.sh: $*" >&2; problems=1; }

if grep -q -E '(^|[[:space:]])(WARN|ERROR)[[:space:]]' "$log"; then
  cat "$log" >&2
  bad "mdbook warned — a warning here is a page that renders wrong"
fi

for path in "$ROOT"/docs/site/*.md; do
  page="$(basename "$path" .md)"
  [[ "$page" == "SUMMARY" ]] && continue
  [[ -f "$OUT/docs/$page.html" ]] \
    || bad "docs/site/$page.md was not rendered — is it in docs/site/SUMMARY.md?"
  [[ -f "$OUT/$page.md" ]] \
    || bad "docs/site/$page.md was not published as Markdown"
done

# mdBook reads `{…}` at the end of a heading as attributes for it, so
# `### GET /node/{key}` is rendered as "GET /node/" with a stray `key=""`,
# and every link to its anchor dangles. A heading that needs braces carries
# them in a code span. Any heading attribute but id and class is that mistake.
for page in "$OUT"/docs/*.html; do
  [[ "$(basename "$page")" == "print.html" ]] && continue
  cut_off="$(grep -o -E '<h[1-6][^>]*>' "$page" | grep -v -E '^<h[1-6]( (id|class)="[^"]*")*>$' || true)"
  [[ -z "$cut_off" ]] \
    || bad "docs/$(basename "$page") has a heading mdBook cut off at its {braces} — put the heading's text in a code span: $(echo "$cut_off" | tr '\n' ' ')"
done

# A stray index.html in docs/site is copied over the rendered introduction.
grep -q 'class="menu-title"' "$OUT/docs/index.html" \
  || bad "docs/index.html is not a page of the book — is there an index.html in docs/site?"
if grep -q 'class="menu-title"' "$OUT/index.html"; then
  bad "the site root is a page of the book, not the landing page"
fi

# Every local link and image on the landing page resolves inside the site.
while IFS= read -r target; do
  target="${target%%#*}"
  [[ -z "$target" ]] && continue
  if [[ -d "$OUT/$target" ]]; then
    [[ -f "$OUT/$target/index.html" ]] || bad "the landing page links $target, a directory with no index.html"
  elif [[ ! -f "$OUT/$target" ]]; then
    bad "the landing page links $target, which the site does not have"
  fi
done < <(grep -o -E '(href|src)="[^"]+"' "$OUT/index.html" \
           | sed -E 's/^(href|src)="//; s/"$//' \
           | grep -v -E '^(https?:|mailto:|#)' || true)

# Search is the reason for a generator: a tool name has to be findable.
grep -q 'rename_node' "$OUT"/docs/searchindex-*.js 2>/dev/null \
  || bad "the search index does not contain rename_node"

[[ $problems -eq 0 ]] || die "the site in $OUT is not publishable"
echo "build-site.sh: OK — $(find "$OUT/docs" -maxdepth 1 -name '*.html' | wc -l | tr -d ' ') pages in $OUT"
