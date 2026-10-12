#!/usr/bin/env bash
# build-site.sh — the published site, built and checked.
#
# Usage: build-site.sh [out-dir]        build into out-dir (default: _site)
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
# does not stop at "mdBook exited 0". It fails if a page was not rendered, if
# mdBook warned, if the landing page links something the site lacks, or if
# the book's front page is not the book's.
set -euo pipefail

MDBOOK_VERSION="0.5.4"
MDBOOK_SHA256_LINUX_X86_64="3f28de05dafca9d0f2eab99c662116b0e37b89b1d96a08f8f430b9eeae958cd7"

case "${1:-}" in
  --mdbook-version) echo "$MDBOOK_VERSION"; exit 0 ;;
  --mdbook-sha256)  echo "$MDBOOK_SHA256_LINUX_X86_64"; exit 0 ;;
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

rm -rf "$OUT"
mkdir -p "$OUT"

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
