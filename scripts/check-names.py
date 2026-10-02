#!/usr/bin/env python3
"""check-names.py — no tracked file names a system on the blocked list.

The standing rule: nothing committed to this repository names or compares
against another project. mushroomdb is described on its own terms.

The list is scripts/blocked-names.sha256: one SHA-256 hex digest per line, of
a name lowercased with everything but letters and digits removed. The names
themselves are in no tracked file, this one included, so the gate needs no
exemption for its own list.

What is checked, for every tracked file and for its path:

  each word (letters and digits, lowercased, accents folded), and every
  run of MIN_LEN to MAX_LEN characters inside it.

So a name matches wherever it occurs inside a word or a path component: glued
to a suffix, to a prefix, or to both — an identifier, a package, a file name, a
class name with the name in the middle. A name written as two words, or split
by a hyphen, is not found: joining adjacent words would make an ordinary phrase
match. A name that is also an ordinary word cannot be listed at all, and a
listed name that happens to sit inside an ordinary word will fail that word.

    python3 scripts/check-names.py                 check the repository
    python3 scripts/check-names.py --add < names   add names (first tab-separated
                                                   field of each line) to the list
    python3 scripts/check-names.py --explain path:line
                                                   local only: why that line fails

Prints `path:line` for every hit and never the name: a path component that
holds a name is printed as `[blocked]`. Exits 1 on any hit.

`--explain` needs the plain list, which is gitignored and exists only on a
machine that built it (default .superpowers/blocked-names.txt, or --plain). It
prints each offending token with the matched run replaced by `[blocked]`, and
the line of the plain list that matched — still never the name.
"""

from __future__ import annotations

import argparse
import hashlib
import re
import subprocess
import sys
import unicodedata
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_LIST = ROOT / "scripts" / "blocked-names.sha256"
DEFAULT_PLAIN = ROOT / ".superpowers" / "blocked-names.txt"

WORD = re.compile(r"[a-z0-9]+")
MIN_LEN = 4
# The longest name the list may hold. It bounds the work per word: a tracked
# file can hold a "word" thousands of characters long (a digest, a blob).
MAX_LEN = 32

# Content that is not text. The path of such a file is still checked.
BINARY_SUFFIXES = (
    ".png", ".gif", ".jpg", ".jpeg", ".ico", ".svg", ".woff", ".woff2", ".pdf",
    ".bin", ".zip", ".gz",
)
# Not ours: lockfiles, whose package names nobody in this repository chose.
EXEMPT_NAMES = ("Cargo.lock", "package-lock.json")


def fold(text: str) -> str:
    """Lowercase, with accents removed: a diacritic must not hide a name."""
    decomposed = unicodedata.normalize("NFKD", text)
    return "".join(c for c in decomposed if not unicodedata.combining(c)).lower()


def digest(piece: str) -> str:
    return hashlib.sha256(piece.encode("utf-8")).hexdigest()


def pieces(word: str):
    """Every run of MIN_LEN..MAX_LEN characters inside the word."""
    for n in range(MIN_LEN, min(len(word), MAX_LEN) + 1):
        for start in range(len(word) - n + 1):
            yield word[start:start + n]


def normal(name: str) -> str:
    """A name as it is hashed: folded, letters and digits only."""
    return "".join(WORD.findall(fold(name)))


def load(path: Path) -> set[str]:
    if not path.is_file():
        return set()
    return {
        line.strip()
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.startswith("#")
    }


class Matcher:
    def __init__(self, blocked: set[str]) -> None:
        self.blocked = blocked
        self.seen: dict[str, bool] = {}

    def hits(self, text: str) -> bool:
        for word in set(WORD.findall(fold(text))):
            if len(word) < MIN_LEN:
                continue
            known = self.seen.get(word)
            if known is None:
                known = any(digest(p) in self.blocked for p in pieces(word))
                self.seen[word] = known
            if known:
                return True
        return False


def tracked(root: Path) -> list[str]:
    """Tracked files of a checkout; every file under `root` otherwise."""
    probe = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--show-toplevel"],
        capture_output=True, text=True, check=False,
    )
    if probe.returncode == 0 and Path(probe.stdout.strip()).resolve() == root.resolve():
        out = subprocess.check_output(["git", "-C", str(root), "ls-files", "-z"])
        return [p for p in out.decode("utf-8").split("\0") if p]
    return sorted(str(p.relative_to(root)) for p in root.rglob("*") if p.is_file())


def add(list_path: Path) -> int:
    have = load(list_path)
    added = 0
    for raw in sys.stdin.read().splitlines():
        name = raw.split("\t", 1)[0].strip()
        if not name or name.startswith("#"):
            continue
        folded = normal(name)
        if len(folded) < MIN_LEN:
            print(
                f"check-names.py: a name of {len(folded)} characters is too short to match "
                f"safely (minimum {MIN_LEN}); nothing was added",
                file=sys.stderr,
            )
            return 2
        if len(folded) > MAX_LEN:
            print(
                f"check-names.py: a name of {len(folded)} characters is longer than the gate "
                f"looks for (maximum {MAX_LEN}); nothing was added",
                file=sys.stderr,
            )
            return 2
        d = digest(folded)
        if d not in have:
            have.add(d)
            added += 1
    header = [
        "# Blocked names, as SHA-256 of each name lowercased with only letters and digits kept.",
        "# Add one with: python3 scripts/check-names.py --add   (a name per line on stdin)",
        "# The names themselves are in no tracked file.",
    ]
    kept_comments = [
        line
        for line in (list_path.read_text(encoding="utf-8").splitlines() if list_path.is_file() else [])
        if line.startswith("#") and line not in header
    ]
    list_path.write_text("\n".join(header + kept_comments + sorted(have)) + "\n", encoding="utf-8")
    print(f"check-names.py: {added} added, {len(have)} on the list")
    return 0


def explain(root: Path, plain_path: Path, blocked: set[str], target: str) -> int:
    """Why `path:line` (or a bare `path`) fails. Local only; never prints a name."""
    if not plain_path.is_file():
        print(
            f"check-names.py: --explain needs the plain list at {plain_path}, which is "
            "gitignored and is not on this machine",
            file=sys.stderr,
        )
        return 2
    entries: list[tuple[int, str]] = []  # the plain names the hashed list holds
    spelled: set[str] = set()            # every plain name, hashed or not
    for lineno, raw in enumerate(plain_path.read_text(encoding="utf-8").splitlines(), 1):
        name = raw.split("\t", 1)[0].strip()
        if not name or name.startswith("#") or not normal(name):
            continue
        spelled.add(normal(name))
        if digest(normal(name)) in blocked:
            entries.append((lineno, normal(name)))
    rel, _, line_part = target.rpartition(":")
    if not rel or not line_part.isdigit():
        rel, line_part = target, ""
    if line_part:
        try:
            lines = (root / rel).read_text(encoding="utf-8").splitlines()
            text = lines[int(line_part) - 1]
        except (OSError, UnicodeDecodeError, IndexError):
            print(f"check-names.py: cannot read line {line_part} of that path", file=sys.stderr)
            return 2
        where = f"line {line_part}"
    else:
        text, where = rel, "the path itself"
    # Mask EVERY name on the plain list in a token before printing it, not only
    # the one being reported: a token or a path component can hold two, and one
    # of them may not be hashed yet. Longest first, so a name that contains
    # another is masked whole.
    every = re.compile("|".join(
        re.escape(name) for name in sorted(spelled, key=len, reverse=True)
    )) if spelled else None
    found = 0
    for word in sorted(set(WORD.findall(fold(text)))):
        masked = every.sub("[blocked]", word) if every else word
        for lineno, name in entries:
            if name in word:
                print(f"{where}: token `{masked}` holds the name "
                      f"on line {lineno} of the plain list")
                found += 1
    if not found:
        print(f"{where}: no listed name found")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="No tracked file names a blocked system.")
    ap.add_argument("--list", type=Path, default=DEFAULT_LIST)
    ap.add_argument("--root", type=Path, default=ROOT)
    ap.add_argument("--add", action="store_true")
    ap.add_argument("--plain", type=Path, default=DEFAULT_PLAIN)
    ap.add_argument("--explain", metavar="PATH[:LINE]")
    a = ap.parse_args()

    if a.add:
        return add(a.list)

    blocked = load(a.list)
    if not blocked:
        print(f"check-names.py: no digests in {a.list}; nothing to check", file=sys.stderr)
        return 1
    if a.explain:
        return explain(a.root, a.plain, blocked, a.explain)
    matcher = Matcher(blocked)
    bad = 0
    for rel in tracked(a.root):
        if Path(rel).name in EXEMPT_NAMES:
            continue
        # A path that holds a name must not be printed: the path would be
        # the name. Each component that hits is shown as `[blocked]`, here and
        # in every `path:line` below.
        shown = "/".join(
            "[blocked]" if matcher.hits(part) else part for part in rel.split("/")
        )
        if shown != rel:
            print(f"{shown}: the path itself")
            bad += 1
        if rel.endswith(BINARY_SUFFIXES):
            continue
        try:
            text = (a.root / rel).read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            if matcher.hits(line):
                print(f"{shown}:{lineno}")
                bad += 1
    if bad:
        print(
            f"check-names.py: FAILED — {bad} place(s) name a system on the blocked list. "
            "Describe mushroomdb on its own terms; in a historical record, use a neutral "
            "placeholder.",
            file=sys.stderr,
        )
        return 1
    print("check-names.py: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
