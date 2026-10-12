#!/usr/bin/env python3
"""check-doc-examples.py — the commands a docs page shows are run, and the
output it shows is output they printed.

    python3 scripts/check-doc-examples.py --binary PATH PAGE...

In each page, every fenced block tagged `sh` is an example. Its lines are run
in order, in one empty temporary directory a page, so a page's commands build
on each other as a reader's would. Each line must start with `mushroomdb `,
which is replaced by PATH: a docs page that asked a reader to run anything
else would be run here too, and this check will not run it.

When a block tagged `text` follows an `sh` block directly, it is what the page
says those commands print. Every non-blank line of it has to be a line the
commands did print, compared without leading or trailing space. A page may
show part of the output; it may not show a line that was not printed. A line
that is exactly `…` stands for output left out.

A block tagged `sh no-run` is shown and not run: a server that never exits, a
command that needs the network.

Exits 1 on the first failure in each page, naming the page, the line of the
block, the command, and either what it printed or the line it did not print.
"""

from __future__ import annotations

import argparse
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

PROGRAM = "mushroomdb"
ELISION = "…"


class Problem(Exception):
    """Something a person has to fix: said in one line, with no traceback."""


def blocks(markdown: str) -> list[tuple[str, int, list[str]]]:
    """Every fenced block: (info string, 1-based line of its opening fence, lines)."""
    found: list[tuple[str, int, list[str]]] = []
    info: str | None = None
    start = 0
    body: list[str] = []
    for number, line in enumerate(markdown.splitlines(), 1):
        stripped = line.strip()
        if info is None:
            if stripped.startswith("```"):
                info, start, body = stripped[3:].strip(), number, []
        elif stripped == "```":
            found.append((info, start, body))
            info = None
        else:
            body.append(line)
    return found


def examples(markdown: str) -> list[tuple[int, list[str], list[str]]]:
    """(line, commands, expected output lines) for each `sh` block.

    The expected lines are those of a `text` block that starts within two
    lines of the `sh` block's closing fence — that is, directly after it, with
    at most a blank line between.
    """
    found = []
    all_blocks = blocks(markdown)
    for index, (info, start, body) in enumerate(all_blocks):
        if info != "sh":
            continue
        commands = [line.strip() for line in body if line.strip()]
        for command in commands:
            if not command.startswith(PROGRAM + " "):
                raise Problem(f"line {start}: an `sh` example runs something other than {PROGRAM}: {command!r}")
        expected: list[str] = []
        end = start + len(body) + 1
        if index + 1 < len(all_blocks):
            next_info, next_start, next_body = all_blocks[index + 1]
            if next_info == "text" and next_start <= end + 2:
                expected = [l.strip() for l in next_body if l.strip() and l.strip() != ELISION]
        found.append((start, commands, expected))
    return found


def run_page(page: Path, binary: str) -> None:
    text = page.read_text(encoding="utf-8")
    with tempfile.TemporaryDirectory() as cwd:
        for start, commands, expected in examples(text):
            printed: list[str] = []
            for command in commands:
                argv = [binary] + shlex.split(command)[1:]
                done = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, encoding="utf-8", timeout=300)
                if done.returncode != 0:
                    raise Problem(
                        f"line {start}: `{command}` exited {done.returncode}:\n{(done.stdout + done.stderr).strip()}"
                    )
                printed += [line.strip() for line in (done.stdout + done.stderr).splitlines()]
            for line in expected:
                if line not in printed:
                    raise Problem(
                        f"line {start}: the page shows a line the commands did not print: {line!r}\n"
                        "what they printed:\n" + "\n".join(printed)
                    )


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description="Run the mushroomdb commands a docs page shows.")
    ap.add_argument("--binary", required=True, help="path to a built mushroomdb")
    ap.add_argument("pages", nargs="+", metavar="PAGE")
    args = ap.parse_args(argv)
    binary = str(Path(args.binary).resolve())
    failed = False
    for name in args.pages:
        try:
            run_page(Path(name), binary)
            print(f"check-doc-examples.py: OK — {name}")
        except Problem as problem:
            print(f"check-doc-examples.py: {name}: {problem}", file=sys.stderr)
            failed = True
        except (OSError, subprocess.SubprocessError) as error:
            print(f"check-doc-examples.py: {name}: {error}", file=sys.stderr)
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
