#!/usr/bin/env python3
"""Run two ci_bench binaries alternately and write each one's per-metric median.

Called by the `bench` CI job when a metric misses the stored baseline. Running
this change's binary and the base commit's one turn about, rather than one
after the other, puts a disk or neighbour spike on both sides: measured back to
back, a change that touched no engine code once read 74 % slower ingest than
its own base on the same runner.

Usage: paired.py <head_bin> <base_bin> <rounds> <head_out.json> <base_out.json>
"""

import json
import statistics
import subprocess
import sys


def run(binary: str) -> dict:
    out = subprocess.run([binary], check=True, capture_output=True, text=True).stdout
    return json.loads(out)


def median(runs: list[dict]) -> dict:
    return {k: statistics.median(r[k] for r in runs) for k in runs[0]}


def main() -> int:
    head_bin, base_bin, rounds, head_out, base_out = sys.argv[1:6]
    head_runs, base_runs = [], []
    for i in range(int(rounds)):
        # Alternate which goes first, so neither always runs on a warmer cache.
        order = [(head_bin, head_runs), (base_bin, base_runs)]
        for binary, runs in order if i % 2 == 0 else reversed(order):
            runs.append(run(binary))
    for path, runs in ((head_out, head_runs), (base_out, base_runs)):
        with open(path, "w") as f:
            json.dump(median(runs), f, indent=2)
            f.write("\n")
    print(f"[bench] {rounds} alternating rounds; medians written to {head_out} and {base_out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
