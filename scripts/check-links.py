#!/usr/bin/env python3
"""check-links.py — every relative link and anchor in the hand-written docs resolves.

Checked, in each tracked Markdown file under the paths in SOURCES:

  - a relative link or image target exists and is tracked by git;
  - a `#fragment` on a link to a Markdown file matches a heading in that file,
    by the slug GitHub gives it;
  - every page in docs/site/ is linked from docs/site/index.md, so no page is
    reachable from nowhere a reader starts.

External URLs (http, https, mailto) are not fetched: a gate must not need a
network. Links inside fenced code blocks (``` or ~~~) and inside inline code
spans are ignored, and a heading inside a fence is not an anchor.

Exits 1 printing `KIND  file:line: target` for every problem.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SOURCES = [
    "README.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "llms.txt",
    "docs/site/*.md",
    "docs/design.md",
    "docs/format-stability.md",
    "docs/concurrency-decision.md",
    "docs/dogfood-report.md",
    "bindings/python/README.md",
    "clients/typescript/README.md",
    "packaging/plugin/README.md",
    "benchmarks/README.md",
]

# Pages in docs/site that are deliberately not in the index.
UNINDEXED = {"index.md"}

LINK = re.compile(
    r'(?<!\!)\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)'  # [text](target "title")
    r'|\!\[[^\]]*\]\(([^)\s]+)\)'                     # ![alt](target)
    r'|<img[^>]+src="([^"]+)"'                        # <img src="target">
)


FENCE = re.compile(r"^\s*(```|~~~)")
# A code span may wrap onto the next line, but never across a blank one.
CODE_SPAN = re.compile(r"(`+)(?!`)((?:(?!\n[ \t]*\n).)+?)(?<!`)\1(?!`)", re.DOTALL)


def unfenced(path: Path) -> list[str]:
    """The file's lines, with every line of a fenced block (``` or ~~~) blanked.

    A fence closes only on the marker that opened it. Line numbers are kept.
    """
    out: list[str] = []
    fence: str | None = None
    for line in path.read_text(encoding="utf-8").splitlines():
        m = FENCE.match(line)
        if fence is not None:
            if m and m.group(1) == fence:
                fence = None
            out.append("")
        elif m:
            fence = m.group(1)
            out.append("")
        else:
            out.append(line)
    return out


def without_code_spans(lines: list[str]) -> list[str]:
    """`lines` with the inside of every inline code span blanked, line count kept."""
    blank = lambda m: re.sub(r"[^\n]", " ", m.group(0))
    return CODE_SPAN.sub(blank, "\n".join(lines)).split("\n")


def git(*args: str) -> str:
    return subprocess.check_output(["git", "-C", str(ROOT), *args], text=True)


def slug(heading: str) -> str:
    """The anchor GitHub generates for a heading."""
    h = heading.strip().lower()
    h = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", h)  # a link in a heading keeps its text
    h = h.replace("`", "")
    h = re.sub(r"[^\w\- ]", "", h, flags=re.UNICODE)
    return h.replace(" ", "-")


def anchors(path: Path) -> set[str]:
    out: set[str] = set()
    seen: dict[str, int] = {}
    for line in unfenced(path):
        m = re.match(r"^(#{1,6})\s+(.*?)\s*#*\s*$", line)
        if m:
            s = slug(m.group(2))
            n = seen.get(s, 0)
            seen[s] = n + 1
            out.add(s if n == 0 else f"{s}-{n}")
        out.update(re.findall(r'<a\s+(?:name|id)="([^"]+)"', line))
    return out


def main() -> int:
    tracked = set(git("ls-files").splitlines())
    files = [f for f in git("ls-files", "--", *SOURCES).splitlines() if f]
    problems: list[str] = []
    index_targets: set[str] = set()

    for rel in files:
        path = ROOT / rel
        for lineno, line in enumerate(without_code_spans(unfenced(path)), 1):
            for m in LINK.finditer(line):
                target = next(g for g in m.groups() if g)
                if re.match(r"^(https?:|mailto:)", target):
                    continue
                file_part, _, fragment = target.partition("#")
                dest = (path.parent / file_part).resolve() if file_part else path
                try:
                    dest_rel = str(dest.relative_to(ROOT))
                except ValueError:
                    problems.append(f"OUTSIDE   {rel}:{lineno}: {target}")
                    continue
                if not dest.exists():
                    problems.append(f"MISSING   {rel}:{lineno}: {target}")
                    continue
                if dest.is_file() and dest_rel not in tracked:
                    problems.append(f"UNTRACKED {rel}:{lineno}: {target}")
                if fragment and dest.suffix == ".md" and fragment not in anchors(dest):
                    problems.append(f"ANCHOR    {rel}:{lineno}: {target}")
                if rel == "docs/site/index.md":
                    index_targets.add(dest_rel)

    for page in sorted(f for f in tracked if re.fullmatch(r"docs/site/[^/]+\.md", f)):
        if Path(page).name in UNINDEXED:
            continue
        if page not in index_targets:
            problems.append(f"UNINDEXED {page}: not linked from docs/site/index.md")

    for p in problems:
        print(p)
    if problems:
        print(f"check-links.py: FAILED — {len(problems)} problem(s) in {len(files)} files", file=sys.stderr)
        return 1
    print(f"check-links.py: OK — {len(files)} files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
