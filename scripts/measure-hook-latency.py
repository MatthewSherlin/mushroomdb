#!/usr/bin/env python3
"""Time the prompt hook against a store: `mushroomdb recall <db>`, one process per prompt.

    python3 scripts/measure-hook-latency.py --binary target/release/mushroomdb \
        --db /path/to/store [--runs 20]

Prints a Markdown report on stdout. Measures; fails nothing. Exits 2 only when
it cannot run at all (no binary, no store). The procedure it implements is
benchmarks/hook-latency/PROCEDURE.md.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import statistics
import subprocess
import sys
import time
from pathlib import Path

TIMEOUT_MS = 5_000  # the hook's own timeout, scripts/plugin-templates/hooks.json.tmpl

PROMPTS = [
    # person-0421's name, as the builder writes it: word(421) and word(1421).
    ("a name the store holds", "wlomipuka wlomipulo"),
    # word(0) and word(1): each occurs in about 600 of 100,000 notes.
    ("two common words", "wkakakaka wlokakaka"),
    ("no match", "zzqx vvkw"),
]


def run_once(cmd: list[str], stdin: str) -> tuple[float, bytes]:
    start = time.perf_counter()
    out = subprocess.run(cmd, input=stdin.encode(), capture_output=True, check=False)
    return (time.perf_counter() - start) * 1000.0, out.stdout


def series(cmd: list[str], stdin: str, runs: int) -> tuple[list[float], int]:
    run_once(cmd, stdin)  # warm the file cache; not timed
    times, size = [], 0
    for _ in range(runs):
        ms, out = run_once(cmd, stdin)
        times.append(ms)
        size = len(out)
    return times, size


def row(label: str, times: list[float], size: int | None) -> str:
    ordered = sorted(times)
    p95 = ordered[max(0, int(round(0.95 * len(ordered))) - 1)]
    cells = [
        label,
        f"{statistics.median(ordered):.0f}",
        f"{p95:.0f}",
        f"{ordered[-1]:.0f}",
        "—" if size is None else str(size),
    ]
    return "| " + " | ".join(cells) + " |"


def dir_bytes(path: Path) -> int:
    return sum(f.stat().st_size for f in path.rglob("*") if f.is_file())


def fingerprint(binary: Path, db: Path) -> tuple[str, bytes]:
    """Every file's path and bytes under `db`, hashed, and what `stats` says of it.

    Taken before the timed runs and again after: the hook is read-only by
    contract, and a measurement that changed the store it timed would be timing
    a different store on its last run than on its first.
    """
    digest = hashlib.sha256()
    for f in sorted(p for p in db.rglob("*") if p.is_file()):
        digest.update(str(f.relative_to(db)).encode())
        digest.update(b"\0")
        digest.update(f.read_bytes())
    stats = subprocess.run([str(binary), "stats", str(db)], capture_output=True, check=False)
    return digest.hexdigest(), stats.stdout


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--binary", required=True)
    ap.add_argument("--db", required=True)
    ap.add_argument("--runs", type=int, default=20)
    a = ap.parse_args()

    binary, db = Path(a.binary), Path(a.db)
    if not binary.is_file():
        print(f"measure-hook-latency: no binary at {binary}", file=sys.stderr)
        return 2
    if not db.is_dir():
        print(f"measure-hook-latency: no store at {db}", file=sys.stderr)
        return 2

    version = subprocess.run(
        [str(binary), "--version"], capture_output=True, text=True, check=False
    ).stdout.strip()
    commit = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"], capture_output=True, text=True, check=False
    ).stdout.strip()

    before = fingerprint(binary, db)
    lines = [
        "# Prompt-hook latency",
        "",
        f"- **Commit:** `{commit}`",
        f"- **Binary:** `{binary}` — {version}",
        f"- **Machine:** {platform.machine()}, {platform.system()} {platform.release()}, "
        f"{os.cpu_count()} cores; load average {os.getloadavg()[0]:.2f}",
        f"- **Store:** `{db}` — {dir_bytes(db) / 1_048_576:.1f} MiB on disk",
        f"- **Runs:** {a.runs} per row, one process each, file cache warm",
        f"- **Hook timeout:** {TIMEOUT_MS} ms",
        "",
        "| What | median ms | p95 ms | max ms | digest bytes |",
        "|---|---|---|---|---|",
    ]
    worst = 0.0
    raw: list[tuple[str, list[float]]] = []
    base, _ = series([str(binary), "stats", str(db)], "", a.runs)
    lines.append(row("`stats` — open the store, nothing else", base, None))
    raw.append(("stats", base))
    worst = max(worst, max(base))
    for label, prompt in PROMPTS:
        times, size = series(
            [str(binary), "recall", str(db)], json.dumps({"prompt": prompt}), a.runs
        )
        lines.append(row(f"`recall` — {label}: {prompt!r}", times, size))
        raw.append((f"recall {prompt!r}", times))
        worst = max(worst, max(times))
    after = fingerprint(binary, db)
    lines += [
        "",
        f"Slowest single run: {worst:.0f} ms — "
        + ("**over the hook's timeout**" if worst > TIMEOUT_MS else "inside the hook's timeout")
        + ".",
        "",
        "Store after the runs: "
        + (
            "byte-identical to the store before them, and `stats` says the same."
            if after == before
            else "**changed by the runs** — its files or its `stats` differ from before them."
        ),
        "",
        "## Every run, in order, ms",
        "",
    ]
    lines += [f"- {label}: " + " ".join(f"{t:.1f}" for t in times) for label, times in raw]
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())
