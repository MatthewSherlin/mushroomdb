# Docs overhaul, pull request 1 — the site renders: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every page under `docs/site` is published as a rendered page with a sidebar and search, reachable from the landing page.

**Architecture:** `docs/site/*.md` stays the flat source. mdBook renders it into `_site/docs/`; the hand-written landing page moves to `docs/landing/` and is copied to the site root with `docs/assets` and the raw Markdown files. One script, `scripts/build-site.sh`, builds and checks the site; the Pages workflow and a CI job both run it.

**Tech Stack:** mdBook 0.5.4 (a prebuilt binary, used only by the build script), bash, Python 3 (the existing link gate), GitHub Actions.

**Spec:** `docs/roadmap/docs-overhaul-spec.md`

## Global Constraints

- Content does not change in this pull request beyond links. Two exceptions, both forced by the build: fifteen links that leave the book, and one `Vec<u32>` in `fulltext.md` that mdBook reads as an unclosed HTML tag.
- `docs/site/*.md` stays the source, flat, one directory. No page is renamed or moved.
- The raw `.md` files stay published at their current URLs (`…/mushroomdb/<page>.md`).
- The book is published under `/docs/`; the landing page stays at the site root.
- Palette and type: as the spec's "Visual direction" gives them. (This plan was written for the landing page's earlier off-white and rust; the owner asked for the design pass after Task 3, and the stylesheet blocks in Task 2 show the earlier values.)
- mdBook is pinned to `0.5.4` in exactly one place, `scripts/build-site.sh`.
- Nothing committed names another product or project as a comparison (`scripts/check-names.py` enforces a hashed list).
- Every existing gate stays green: `check-links.py`, `check-claims.sh`, `gen-llms-full.sh --check`, `test-check-names.sh`, `bump-version.sh --check`.
- Commits use conventional prefixes, lower case, in the repository's sentence style (see `git log --oneline -20`). Commit as the repository's configured git user.

**One deviation from the spec.** The spec moves `docs/format-stability.md` into the site in this pull request. It has eleven inbound references, two of them code comments that cite it by line number, and the README links to it twice. This plan links to it by absolute URL instead and leaves the move to pull request 4, which rewrites those pages anyway.

## Review Focus

1. **A page added to `docs/site` but not to `SUMMARY.md`.** mdBook silently does not render it. Expected: CI fails and names the page. Pinned in Task 1 (`UNLISTED`) and again in Task 2 (the build check looks for each page's `.html`).
2. **A relative link from a site page to a file outside the book.** It works on GitHub and is a 404 on the site. Expected: CI fails and names the line. Pinned in Task 1 (`OFFSITE`).
3. **A non-Markdown file dropped into `docs/site` named `index.html`.** mdBook copies it over the rendered introduction — this is what the current landing page does. Expected: the build fails. Pinned in Task 2 (the rendered `docs/index.html` must be an mdBook page).
4. **A landing-page link to a docs page that does not exist** (a typo, or a page later removed). Expected: the build fails and names the link. Pinned in Task 2.
5. **A different mdBook version on the machine running the build.** Output and config keys differ between 0.4 and 0.5. Expected: the script refuses to build and says which version it wants. Pinned in Task 2.

---

### Task 1: `docs/site` is a book source, and the link gate holds it to that

**Files:**
- Create: `scripts/tests/test-check-links.sh`
- Create: `docs/site/SUMMARY.md`
- Modify: `scripts/check-links.py` (docstring lines 1–17, constants near line 44, `main()` lines 119–148)
- Modify: `scripts/gen-llms-full.sh` (the `EXCLUDED_DOCS` list, about line 66)
- Modify: `docs/site/association-bench.md:135,141,148,188,225`, `docs/site/api.md:6,797,1551`, `docs/site/deployment.md:108`, `docs/site/index.md:47,48,64`, `docs/site/panic-policy.md:108`, `docs/site/quickstart.md:367`, `docs/site/skill.md:34`, `docs/site/fulltext.md:139`
- Modify: `.github/workflows/ci.yml` (the `plugin-validate` job, after the `check-links.py` step)
- Regenerate: `llms-full.txt`

**Interfaces:**
- Consumes: nothing.
- Produces: `docs/site/SUMMARY.md`, the sidebar Task 2's `book.toml` reads. Two new problem kinds printed by `scripts/check-links.py`: `UNLISTED  <page>: not in docs/site/SUMMARY.md` and `OFFSITE   <file>:<line>: <target>`.

- [ ] **Step 1: Write the failing test**

Create `scripts/tests/test-check-links.sh`:

```bash
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

[[ $fails -eq 0 ]] || exit 1
echo "test-check-links.sh: OK"
```

Make it executable: `chmod +x scripts/tests/test-check-links.sh`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `bash scripts/tests/test-check-links.sh`
Expected: exit 1. The first case fails with `UNINDEXED docs/site/SUMMARY.md` in its output (today's gate wants every page, the sidebar included, linked from the index), and the `unlisted`, `nosummary`, `offsite` and `offsite-anchor` cases fail with "exit 0, wanted 1" or "output lacks".

- [ ] **Step 3: Teach the gate the sidebar and the book's edge**

In `scripts/check-links.py`, replace the docstring's checked-items list (the three bullets under "Checked, in each tracked Markdown file under the paths in SOURCES:") with:

```python
  - a relative link or image target exists and is tracked by git;
  - a `#fragment` on a link to a Markdown file matches a heading in that file,
    by the slug GitHub gives it;
  - every page in docs/site/ is linked from docs/site/index.md, so no page is
    reachable from nowhere a reader starts;
  - every page in docs/site/ is listed in docs/site/SUMMARY.md, the sidebar of
    the rendered site: a page it does not list is not rendered at all;
  - a relative link from a page in docs/site/ lands inside docs/site/ or
    docs/assets/, the only two trees the published site has.
```

Replace the `UNINDEXED` constant and its comment with:

```python
# Pages in docs/site that are deliberately not in the index.
UNINDEXED = {"index.md", "SUMMARY.md"}

# The sidebar of the rendered site.
SUMMARY = "docs/site/SUMMARY.md"

# Where a relative link from a site page may land. The published site is the
# book and the assets staged beside it; a link anywhere else resolves on
# GitHub and is a 404 once deployed.
SITE_LINK_ROOTS = ("docs/site/", "docs/assets/")
```

In `main()`, add `summary_targets: set[str] = set()` on the line after `index_targets: set[str] = set()`.

In the link loop, directly after the `except ValueError:` block (after its `continue`) and before `if not dest.exists():`, add:

```python
                if rel.startswith("docs/site/") and file_part and not dest_rel.startswith(SITE_LINK_ROOTS):
                    problems.append(f"OFFSITE   {rel}:{lineno}: {target}")
                    continue
```

Directly after the existing two lines `if rel == "docs/site/index.md":` / `index_targets.add(dest_rel)`, add:

```python
                if rel == SUMMARY:
                    summary_targets.add(dest_rel)
```

Replace the whole `for page in sorted(...)` loop that follows with:

```python
    for page in sorted(f for f in tracked if re.fullmatch(r"docs/site/[^/]+\.md", f)):
        name = Path(page).name
        if page != SUMMARY and page not in summary_targets:
            problems.append(f"UNLISTED  {page}: not in {SUMMARY}")
        if name in UNINDEXED:
            continue
        if page not in index_targets:
            problems.append(f"UNINDEXED {page}: not linked from docs/site/index.md")
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `bash scripts/tests/test-check-links.sh`
Expected: eight `ok:` lines, then `test-check-links.sh: OK`, exit 0.

- [ ] **Step 5: Run the gate on the real tree to see what it now finds**

Run: `python3 scripts/check-links.py`
Expected: exit 1 with 39 problems — 24 `UNLISTED` lines (every page, `SUMMARY.md` not existing yet) and 15 `OFFSITE` lines, at exactly the fifteen file:line pairs listed under **Files** above, `fulltext.md` aside. If the `OFFSITE` count is not 15, stop and compare against `grep -n -E '\]\(\.\./' docs/site/*.md`: the one remaining match, `docs/site/rules.md:11`, points into `docs/assets/` and is allowed.

- [ ] **Step 6: Add the sidebar**

Create `docs/site/SUMMARY.md`:

```markdown
# Summary

[Introduction](index.md)

# Start

- [Quickstart](quickstart.md)
- [Install, plugin and hooks](skill.md)

# Guides

- [Agent memory](mcp.md)
- [Rules](rules.md)
- [Query](query.md)
- [Time travel](timetravel.md)
- [Access control](masks.md)
- [Full-text search](fulltext.md)
- [Indexes](indexes.md)
- [Views](views.md)
- [Subscriptions](subscriptions.md)
- [Algorithms](algorithms.md)
- [Rule suggestion](suggest.md)
- [Git ingest](ingest-git.md)

# Reference

- [API reference](api.md)

# Operate

- [Running as a service](service.md)
- [Deployment](deployment.md)
- [Durability](durability.md)
- [Concurrency](concurrency.md)
- [Several processes, one store](multiprocess.md)
- [The association benchmark](association-bench.md)

# Project

- [Testing](testing.md)
- [Panic policy](panic-policy.md)
```

That is all 24 pages. The group names are the spec's; the pages the spec marks new arrive in later pull requests.

- [ ] **Step 7: Point the fifteen outbound links at GitHub**

Run from the repository root (BSD and GNU `sed` both accept this form):

```bash
for f in docs/site/association-bench.md docs/site/api.md docs/site/deployment.md \
         docs/site/index.md docs/site/panic-policy.md docs/site/quickstart.md docs/site/skill.md; do
  sed -i.bak -E \
    -e 's#\]\(\.\./\.\./#](https://github.com/MatthewSherlin/mushroomdb/blob/main/#g' \
    -e 's#\]\(\.\./format-stability\.md#](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/format-stability.md#g' \
    "$f" && rm "$f.bak"
done
```

Check: `grep -n -E '\]\(\.\./' docs/site/*.md` prints one line, `docs/site/rules.md:11` (the image under `../assets/`). `git diff --stat docs/site` shows 7 files and 15 changed lines. Anchors survive: `docs/site/index.md` now links `…/blob/main/README.md#architecture` and `…/blob/main/README.md#install`.

- [ ] **Step 8: Close the tag mdBook would read as HTML**

In `docs/site/fulltext.md` line 139, change `(Vec<u32> per posting)` to ``(`Vec<u32>` per posting)``. Nothing else on the line changes.

- [ ] **Step 9: Keep the sidebar out of `llms-full.txt`, and regenerate it**

In `scripts/gen-llms-full.sh`, change the `EXCLUDED_DOCS` list to:

```bash
EXCLUDED_DOCS=(
    panic-policy.md   # an internal policy for contributors, not product reference
    SUMMARY.md        # the rendered site's sidebar: a list of links, not a page
)
```

Run: `bash scripts/gen-llms-full.sh`
Then: `bash scripts/gen-llms-full.sh --check`
Expected: the second command exits 0. `git diff --stat llms-full.txt` shows fifteen changed lines and nothing else: fourteen links (`panic-policy.md`, which holds the fifteenth, is not in the file) and one backticked type.

- [ ] **Step 10: Run every gate**

Run:

```bash
python3 scripts/check-links.py
bash scripts/tests/test-check-links.sh
bash scripts/check-claims.sh
bash scripts/gen-llms-full.sh --check
```

Expected: `check-links.py: OK — ` followed by a file count one higher than before this task (the sidebar is a new source), `test-check-links.sh: OK`, and exit 0 from all four.

- [ ] **Step 11: Run the new test in CI**

In `.github/workflows/ci.yml`, in the `plugin-validate` job, directly after the step `- run: python3 scripts/check-links.py`, add:

```yaml
      # The link gate, shown failing on what it guards: a page the sidebar
      # omits, and a site page linking a file the published site does not have.
      - run: bash scripts/tests/test-check-links.sh
```

- [ ] **Step 12: Commit**

```bash
git add scripts/check-links.py scripts/tests/test-check-links.sh scripts/gen-llms-full.sh \
        docs/site/SUMMARY.md docs/site/association-bench.md docs/site/api.md docs/site/deployment.md \
        docs/site/index.md docs/site/panic-policy.md docs/site/quickstart.md docs/site/skill.md \
        docs/site/fulltext.md llms-full.txt .github/workflows/ci.yml
git commit -m "docs(site): the pages have a sidebar, and no link leaves the book"
```

---

### Task 2: one script builds the site and refuses a broken one

**Files:**
- Create: `scripts/build-site.sh`
- Create: `book.toml`
- Create: `docs/theme/mushroom.css`
- Create: `docs/theme/home-link.js`
- Move: `docs/site/index.html` → `docs/landing/index.html`, then modify it (head, a new top bar, the install block, the footer)
- Modify: `scripts/check-claims.sh:45`
- Modify: `.gitignore`

**Interfaces:**
- Consumes: `docs/site/SUMMARY.md` from Task 1.
- Produces: `bash scripts/build-site.sh [out-dir]` — builds into `out-dir` (default `_site` at the repository root) and exits non-zero if the result is not a complete site. `bash scripts/build-site.sh --mdbook-version` prints `0.5.4`; `--mdbook-sha256` prints the SHA-256 of the Linux x86_64 release tarball. Task 3's workflows call all three. `MDBOOK=/path/to/mdbook` overrides the binary found on `PATH`.

- [ ] **Step 1: Get the pinned mdBook onto this machine, outside the repository**

```bash
mkdir -p "$HOME/.local/bin"
case "$(uname -sm)" in
  "Darwin arm64")  t=aarch64-apple-darwin ;;
  "Darwin x86_64") t=x86_64-apple-darwin ;;
  "Linux x86_64")  t=x86_64-unknown-linux-gnu ;;
  "Linux aarch64") t=aarch64-unknown-linux-musl ;;
esac
curl -sSfL "https://github.com/rust-lang/mdBook/releases/download/v0.5.4/mdbook-v0.5.4-$t.tar.gz" \
  | tar -xz -C "$HOME/.local/bin"
"$HOME/.local/bin/mdbook" --version
```

Expected: `mdbook v0.5.4`. Use `export MDBOOK="$HOME/.local/bin/mdbook"` for the rest of this task.

- [ ] **Step 2: Write the build script — it is the test**

Create `scripts/build-site.sh`:

```bash
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
```

Make it executable: `chmod +x scripts/build-site.sh`.

- [ ] **Step 3: Run it to verify it fails**

Run: `bash scripts/build-site.sh`
Expected: exit 1, `mdbook build failed`, with mdBook's own message that it could not find `book.toml` or a `src/SUMMARY.md` under the repository root.

Also run: `MDBOOK=/usr/bin/true bash scripts/build-site.sh`
Expected: exit 1 with a line ending `and this build needs v0.5.4` — the version check refuses a binary that is not the pinned one.

- [ ] **Step 4: Add the book's configuration**

Create `book.toml` at the repository root:

```toml
# The documentation site. docs/site/*.md is the source; scripts/build-site.sh
# is the only thing that should run mdBook, and it pins the version.
[book]
title = "mushroomdb"
description = "The graph that stays true — and knows who's allowed to see it."
language = "en"
src = "docs/site"

[build]
build-dir = "_site/docs"
create-missing = false

[output.html]
site-url = "/mushroomdb/docs/"
git-repository-url = "https://github.com/MatthewSherlin/mushroomdb"
edit-url-template = "https://github.com/MatthewSherlin/mushroomdb/edit/main/{path}"
default-theme = "light"
preferred-dark-theme = "navy"
additional-css = ["docs/theme/mushroom.css"]
additional-js = ["docs/theme/home-link.js"]
```

`create-missing = false` matters: with the default, a typo in `SUMMARY.md` makes mdBook write an empty page into `docs/site` instead of failing.

- [ ] **Step 5: Add the stylesheet and the home link**

Create `docs/theme/mushroom.css`:

```css
/* mushroomdb docs — the landing page's palette on mdBook's layout.
   The values are docs/landing/index.html's; change them in both places. */

:root {
  --content-max-width: 780px;
  --mono-font: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace;
}

html, body, .sidebar, .menu-title {
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
}

.light, html:not(.js) {
  --bg: #fbfaf8;
  --fg: #1b1a18;
  --code-bg: #f3f1ec;
  --sidebar-bg: #f3f1ec;
  --sidebar-fg: #1b1a18;
  --sidebar-non-existant: #a49d92;
  --sidebar-active: #8a3324;
  --sidebar-spacer: #e4e0d8;
  --sidebar-header-border-color: #8a3324;
  --scrollbar: #a49d92;
  --icons: #625d55;
  --icons-hover: #1b1a18;
  --links: #8a3324;
  --inline-code-color: #1b1a18;
  --theme-popup-bg: #ffffff;
  --theme-popup-border: #e4e0d8;
  --theme-hover: #f3f1ec;
  --quote-bg: #f3f1ec;
  --quote-border: #e4e0d8;
  --table-border-color: #e4e0d8;
  --table-header-bg: #f3f1ec;
  --table-alternate-bg: #f7f5f1;
  --searchbar-border-color: #e4e0d8;
  --searchbar-bg: #ffffff;
  --searchbar-fg: #1b1a18;
  --searchbar-shadow-color: #e4e0d8;
  --searchresults-header-fg: #625d55;
  --searchresults-border-color: #e4e0d8;
  --searchresults-li-bg: #f3f1ec;
  --search-mark-bg: #f1d3c9;
}

.navy {
  --bg: #14130f;
  --fg: #ece8e0;
  --code-bg: #23211c;
  --sidebar-bg: #1c1b17;
  --sidebar-fg: #ece8e0;
  --sidebar-non-existant: #625d55;
  --sidebar-active: #e58a6d;
  --sidebar-spacer: #302e28;
  --sidebar-header-border-color: #e58a6d;
  --scrollbar: #625d55;
  --icons: #a49d92;
  --icons-hover: #ece8e0;
  --links: #e58a6d;
  --inline-code-color: #ece8e0;
  --theme-popup-bg: #1c1b17;
  --theme-popup-border: #302e28;
  --theme-hover: #23211c;
  --quote-bg: #1c1b17;
  --quote-border: #302e28;
  --table-border-color: #302e28;
  --table-header-bg: #23211c;
  --table-alternate-bg: #1a1915;
  --searchbar-border-color: #302e28;
  --searchbar-bg: #1c1b17;
  --searchbar-fg: #ece8e0;
  --searchbar-shadow-color: #302e28;
  --searchresults-header-fg: #a49d92;
  --searchresults-border-color: #302e28;
  --searchresults-li-bg: #23211c;
  --search-mark-bg: #5c3327;
}

/* Two themes, the two the palette defines. */
#mdbook-theme-list li:has(> #mdbook-theme-rust),
#mdbook-theme-list li:has(> #mdbook-theme-coal),
#mdbook-theme-list li:has(> #mdbook-theme-ayu) {
  display: none;
}

html .hljs,
html :not(pre) > code {
  background: var(--code-bg);
}
html pre > code.hljs {
  border: 1px solid var(--table-border-color);
  border-radius: 8px;
  padding: 14px 16px;
}
html :not(pre) > code {
  border-radius: 4px;
  padding: 0.1em 0.3em;
}

.menu-title a {
  color: inherit;
  text-decoration: none;
}
.menu-title a:hover {
  color: var(--links);
}

.content img {
  border: 1px solid var(--table-border-color);
  border-radius: 10px;
}
```

Create `docs/theme/home-link.js`:

```js
// The book's title links back to the landing page, one level above /docs/.
// mdBook has no setting for this. Every page of the book is directly under
// /docs/, and 404.html carries a <base> pointing there, so "../" is the
// landing page from all of them.
document.addEventListener("DOMContentLoaded", () => {
  const title = document.querySelector(".menu-title");
  if (!title || title.querySelector("a")) return;
  const link = document.createElement("a");
  link.href = "../";
  link.textContent = title.textContent;
  title.textContent = "";
  title.appendChild(link);
});
```

- [ ] **Step 6: Run the build to see the collision**

Run: `bash scripts/build-site.sh`
Expected: exit 1. mdBook itself now succeeds, and the script stops at its first `cp` with `docs/landing/index.html: No such file or directory`: the landing page is still in `docs/site`, where mdBook has just copied it over the rendered introduction. Step 7 moves it.

- [ ] **Step 7: Move the landing page out of the book**

```bash
mkdir -p docs/landing
git mv docs/site/index.html docs/landing/index.html
```

In `scripts/check-claims.sh` line 45, change `  docs/site/index.html` to `  docs/landing/index.html`.

Run: `bash scripts/build-site.sh`
Expected: `build-site.sh: OK — 27 pages in …/_site` (24 pages, plus mdBook's `print.html`, `404.html` and `toc.html`), exit 0. The landing page still links nowhere inside the site; the next step fixes that.

Then prove the collision check works, and undo it:

```bash
cp docs/landing/index.html docs/site/index.html
bash scripts/build-site.sh; echo "exit=$?"
rm docs/site/index.html
```

Expected: `docs/index.html is not a page of the book — is there an index.html in docs/site?` and `exit=1`.

- [ ] **Step 8: Give the landing page a way into the docs**

Edit `docs/landing/index.html`. Five changes; nothing else in the file moves.

(a) In `<head>`, directly after the `<link rel="icon" …>` line, add:

```html
<meta property="og:type" content="website" />
<meta property="og:title" content="mushroomdb — the graph that stays true" />
<meta property="og:description" content="An embedded Rust graph database in which a relationship is a schema declaration: write a rule once, and every write derives, maintains and retracts the matching edges." />
<meta property="og:url" content="https://matthewsherlin.github.io/mushroomdb/" />
<meta property="og:image" content="https://matthewsherlin.github.io/mushroomdb/assets/social-preview.png" />
<meta name="twitter:card" content="summary_large_image" />
```

(b) In the `<style>` block, directly before the line `  header { padding: 56px 0 8px; text-align: center; }`, add:

```css
  .topbar { display: flex; align-items: center; gap: 22px; padding: 18px 0; font-size: 15px; }
  .topbar .brand { display: flex; align-items: center; gap: 8px; font-weight: 700; color: var(--ink); text-decoration: none; margin-right: auto; }
  .topbar .brand img { width: 22px; }
  .topbar a { text-decoration: none; }
  .topbar a:hover { text-decoration: underline; }
  .cta { display: flex; justify-content: center; gap: 12px; margin: 0 0 30px; }
  .cta a { padding: 9px 18px; border-radius: 8px; border: 1px solid var(--accent); text-decoration: none; font-weight: 600; }
  .cta a.primary { background: var(--accent); color: var(--bg); }
  .install { position: relative; }
  .copy {
    position: absolute; top: 8px; right: 8px; font: inherit; font-size: 13px;
    color: var(--muted); background: var(--panel); border: 1px solid var(--line);
    border-radius: 6px; padding: 3px 9px; cursor: pointer;
  }
  .copy:hover { color: var(--ink); }
```

and change the existing `header { padding: 56px 0 8px; text-align: center; }` to `header { padding: 30px 0 8px; text-align: center; }`.

(c) Directly after `<div class="wrap">` and before `<header>`, add:

```html
  <nav class="topbar">
    <a class="brand" href="./"><img src="assets/mark-small.svg" alt="" />mushroomdb</a>
    <a href="docs/">Docs</a>
    <a href="docs/quickstart.html">Quickstart</a>
    <a href="docs/api.html">Reference</a>
    <a href="https://github.com/MatthewSherlin/mushroomdb">GitHub</a>
  </nav>
```

(d) Replace the install block and its note — the two lines starting `<div class="install">` and `<p class="install-note">` — with:

```html
    <div class="install"><pre><code id="install-cmd">npx mushroomdb install --db ./memory</code></pre><button class="copy" type="button" data-copy="install-cmd">Copy</button></div>
    <p class="install-note">Writes the <code>/mushroom</code> skill, an MCP server listing the twenty-three-tool association surface, and the session hooks, for Claude Code or Cursor.</p>
    <p class="cta"><a class="primary" href="docs/quickstart.html">Quickstart</a><a href="docs/">Read the docs</a></p>
```

(e) Replace the footer's `<nav>…</nav>` with:

```html
    <nav>
      <a href="docs/">Docs</a>
      <a href="docs/quickstart.html">Quickstart</a>
      <a href="https://github.com/MatthewSherlin/mushroomdb/blob/main/CHANGELOG.md">Changelog</a>
      <a href="https://github.com/MatthewSherlin/mushroomdb">GitHub</a>
      <a href="https://github.com/MatthewSherlin/mushroomdb/issues">Issues</a>
    </nav>
```

and directly before `</body>`, add:

```html
<script>
  // Copy the install command. No clipboard permission, no button: the text is still selectable.
  document.querySelectorAll("button.copy").forEach((button) => {
    if (!navigator.clipboard) { button.remove(); return; }
    button.addEventListener("click", async () => {
      const source = document.getElementById(button.dataset.copy);
      await navigator.clipboard.writeText(source.textContent);
      button.textContent = "Copied";
      setTimeout(() => { button.textContent = "Copy"; }, 1500);
    });
  });
</script>
```

- [ ] **Step 9: Run the build, then break a link to see it caught**

Run: `bash scripts/build-site.sh`
Expected: `build-site.sh: OK — 27 pages in …/_site`, exit 0.

Then:

```bash
sed -i.bak 's#docs/quickstart.html">Quickstart</a>#docs/quikstart.html">Quickstart</a>#' docs/landing/index.html
bash scripts/build-site.sh; echo "exit=$?"
mv docs/landing/index.html.bak docs/landing/index.html
bash scripts/build-site.sh
```

Expected: the middle run prints `the landing page links docs/quikstart.html, which the site does not have` and `exit=1`; the last run is OK again.

- [ ] **Step 10: Look at it**

```bash
python3 -m http.server 8765 --directory _site >/dev/null 2>&1 &
```

Open `http://localhost:8765/` and `http://localhost:8765/docs/quickstart.html`, in a light and a dark system appearance. (The 404 page and the edit links assume the `/mushroomdb/` prefix and are checked after deploy, in Task 3.) Check each of these and fix the stylesheet if one is off:

- the landing page's top bar and both buttons go to the docs; the copy button copies;
- the book's background, link colour and sidebar match the landing page in both appearances;
- code blocks and inline code have the warm grey background, not white or blue-black;
- a wide table (`docs/api.html`, the endpoint table) scrolls inside the page and does not push it wider;
- the theme menu offers Auto, Light and Navy only;
- the book's title, top centre, links back to the landing page;
- searching `rename_node` (press `s`) returns results;
- the image on `docs/rules.html` loads.

Stop the server: `kill %1`.

- [ ] **Step 11: Ignore the build output, and run the gates**

Append to `.gitignore`:

```
# The site, as scripts/build-site.sh builds it.
/_site/
```

Run:

```bash
python3 scripts/check-links.py
bash scripts/check-claims.sh
bash scripts/gen-llms-full.sh --check
bash scripts/tests/test-check-names.sh
git status --short
```

Expected: all four exit 0, and `git status` lists no file under `docs/site` other than the rename of `index.html` — mdBook must not have written into the source (`create-missing = false`), and `_site/` must not appear.

- [ ] **Step 12: Commit**

```bash
git add scripts/build-site.sh book.toml docs/theme/mushroom.css docs/theme/home-link.js \
        docs/landing/index.html scripts/check-claims.sh .gitignore
git commit -m "feat(site): the docs build into a book, and the landing page links into it"
```

(`git mv` in Step 7 already staged the removal of `docs/site/index.html`.)

---

### Task 3: the workflows build what the script builds

**Files:**
- Modify: `.github/workflows/pages.yml` (the `paths` filter and the `Stage site` step)
- Modify: `.github/workflows/ci.yml` (a new job, `site`, after `plugin-validate`)

**Interfaces:**
- Consumes: `bash scripts/build-site.sh [out-dir]`, `--mdbook-version` and `--mdbook-sha256` from Task 2.
- Produces: nothing later tasks use.

- [ ] **Step 1: Build the site in the Pages workflow**

In `.github/workflows/pages.yml`, replace the `paths` list with:

```yaml
    paths:
      - "docs/site/**"
      - "docs/landing/**"
      - "docs/theme/**"
      - "docs/assets/**"
      - "book.toml"
      - "scripts/build-site.sh"
      - ".github/workflows/pages.yml"
```

and replace the comment and the `Stage site` step (from `# docs/site holds the page;` through the `cp -R docs/assets _site/assets` line) with:

```yaml
      # The version and its checksum live in scripts/build-site.sh, and
      # nowhere else. The script refuses any other mdBook.
      - name: Install mdBook
        run: |
          v="$(bash scripts/build-site.sh --mdbook-version)"
          f="mdbook-v${v}-x86_64-unknown-linux-gnu.tar.gz"
          curl -sSfL -o "$f" "https://github.com/rust-lang/mdBook/releases/download/v${v}/${f}"
          echo "$(bash scripts/build-site.sh --mdbook-sha256)  $f" | sha256sum -c -
          mkdir -p "$HOME/.local/bin"
          tar -xzf "$f" -C "$HOME/.local/bin"
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"

      # The landing page, the rendered book under docs/, the assets both use,
      # and each page's Markdown at the URL it has always had.
      - name: Build site
        run: bash scripts/build-site.sh _site
```

The `configure-pages`, `upload-pages-artifact` (`path: _site`) and `deploy-pages` steps stay as they are.

- [ ] **Step 2: Build the site on every pull request**

In `.github/workflows/ci.yml`, add this job at the end of the file, at the same indentation as `plugin-validate:`:

```yaml
  # The published site, built exactly as pages.yml builds it. Without this a
  # page that does not render, or a landing-page link to nothing, is found by
  # the deploy on main and not by the pull request that caused it.
  site:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install mdBook
        run: |
          v="$(bash scripts/build-site.sh --mdbook-version)"
          f="mdbook-v${v}-x86_64-unknown-linux-gnu.tar.gz"
          curl -sSfL -o "$f" "https://github.com/rust-lang/mdBook/releases/download/v${v}/${f}"
          echo "$(bash scripts/build-site.sh --mdbook-sha256)  $f" | sha256sum -c -
          mkdir -p "$HOME/.local/bin"
          tar -xzf "$f" -C "$HOME/.local/bin"
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - run: bash scripts/build-site.sh
```

- [ ] **Step 3: Check both files parse and say what they should**

```bash
python3 - <<'EOF'
import yaml
pages = yaml.safe_load(open(".github/workflows/pages.yml"))
ci = yaml.safe_load(open(".github/workflows/ci.yml"))
steps = [s.get("name") or s.get("uses") or s.get("run") for s in pages["jobs"]["deploy"]["steps"]]
assert "Install mdBook" in steps and "Build site" in steps and "Stage site" not in steps, steps
assert "book.toml" in pages[True]["push"]["paths"], pages[True]["push"]["paths"]
assert ci["jobs"]["site"]["steps"][-1]["run"] == "bash scripts/build-site.sh"
print("workflows: OK")
EOF
```

Expected: `workflows: OK`. (`pages[True]` is not a typo: YAML reads the key `on` as a boolean.) If `import yaml` fails, run it with `python3 -m pip install --user pyyaml` first, or in any virtualenv; do not add it to the repository.

- [ ] **Step 4: Run the Linux install commands' checksum locally**

The checksum in the script was computed once, by hand. Confirm it against the release:

```bash
v="$(bash scripts/build-site.sh --mdbook-version)"
curl -sSfL "https://github.com/rust-lang/mdBook/releases/download/v${v}/mdbook-v${v}-x86_64-unknown-linux-gnu.tar.gz" \
  | shasum -a 256 | awk '{print $1}'
bash scripts/build-site.sh --mdbook-sha256
```

Expected: the two lines are identical.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/pages.yml .github/workflows/ci.yml
git commit -m "ci(pages): the deploy builds the book, and a pull request builds it first"
```

- [ ] **Step 6: After the pull request's CI — the `site` job is the proof**

The `site` job is the first run of the install step on Linux. It must be green, and its last line must read `build-site.sh: OK — 27 pages in …/_site`. A red `plugin-validate` naming `test-check-links.sh` or `check-links.py` is a Task 1 problem.

- [ ] **Step 7: After the merge — the deployed site**

The `pages` run on `main` deploys it. Then:

```bash
base=https://matthewsherlin.github.io/mushroomdb
curl -s  "$base/" | grep -c 'href="docs/quickstart.html"'            # 3
curl -sI "$base/docs/quickstart.html" | head -1                       # HTTP/2 200
curl -sI "$base/docs/" | head -1                                      # HTTP/2 200
curl -sI "$base/quickstart.md" | grep -i content-type                 # text/markdown
curl -sI "$base/assets/rule-fire-explain.gif" | head -1               # HTTP/2 200
curl -s  "$base/no-such-page" | grep -c 'class="menu-title"'          # 1 — the book's 404
```

Open `$base/docs/rules.html` and click "Suggest an edit": it must open `docs/site/rules.md` on GitHub.
