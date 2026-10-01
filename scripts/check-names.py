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
  prefix and suffix of it at least MIN_LEN characters long.

So a name glued to a suffix — an identifier, a package, a file name — is
found. A name written as two words, or split by a hyphen, is not: joining
adjacent words would make an ordinary phrase match. A name that is also an
ordinary word cannot be listed at all.

    python3 scripts/check-names.py                 check the repository
    python3 scripts/check-names.py --add < names   add names (first tab-separated
                                                   field of each line) to the list

Prints `path:line` for every hit and never the name: a path component that is
itself a name is printed as `[blocked]`. Exits 1 on any hit.
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

WORD = re.compile(r"[a-z0-9]+")
MIN_LEN = 4

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
    """The word, and each prefix and suffix of it at least MIN_LEN long."""
    yield word
    for n in range(MIN_LEN, len(word)):
        yield word[:n]
        yield word[-n:]


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
        normal = "".join(WORD.findall(fold(name)))
        if len(normal) < MIN_LEN:
            print(
                f"check-names.py: a name of {len(normal)} characters is too short to match "
                f"safely (minimum {MIN_LEN}); nothing was added",
                file=sys.stderr,
            )
            return 2
        d = digest(normal)
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


def main() -> int:
    ap = argparse.ArgumentParser(description="No tracked file names a blocked system.")
    ap.add_argument("--list", type=Path, default=DEFAULT_LIST)
    ap.add_argument("--root", type=Path, default=ROOT)
    ap.add_argument("--add", action="store_true")
    a = ap.parse_args()

    if a.add:
        return add(a.list)

    blocked = load(a.list)
    if not blocked:
        print(f"check-names.py: no digests in {a.list}; nothing to check", file=sys.stderr)
        return 1
    matcher = Matcher(blocked)
    bad = 0
    for rel in tracked(a.root):
        if Path(rel).name in EXEMPT_NAMES:
            continue
        # A path that is itself a name must not be printed: the path would be
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
