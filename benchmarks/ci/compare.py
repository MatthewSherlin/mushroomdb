#!/usr/bin/env python3
"""
benchmarks/ci/compare.py — regression gate for CI benchmark results.

Usage
-----
Enforcing mode (normal CI run):
    python3 benchmarks/ci/compare.py results.json benchmarks/baselines/ci.json \\
        --threshold 0.15

Bootstrap mode (first run — writes the baseline and exits 0):
    python3 benchmarks/ci/compare.py results.json benchmarks/baselines/ci.json \\
        --threshold 0.15 --bootstrap

Exit codes
----------
0   All pinned metrics are within the threshold, or --bootstrap was given.
1   One or more metrics regressed beyond the threshold, or a metric present
    in the baseline is missing from results.json.
"""

import argparse
import json
import sys


def main() -> int:
    parser = argparse.ArgumentParser(description="CI benchmark regression gate")
    parser.add_argument("results", help="Path to results.json produced by run.sh")
    parser.add_argument("baseline", help="Path to baseline JSON (benchmarks/baselines/ci.json)")
    parser.add_argument(
        "--threshold",
        type=float,
        default=0.15,
        help="Fractional regression allowed before failing (default 0.15 = 15%%)",
    )
    parser.add_argument(
        "--bootstrap",
        action="store_true",
        help="Write results as the new baseline and exit 0 (captures the baseline)",
    )
    args = parser.parse_args()

    with open(args.results) as f:
        results: dict = json.load(f)

    if args.bootstrap:
        with open(args.baseline, "w") as f:
            json.dump(results, f, indent=2)
            f.write("\n")
        print(f"[bench] bootstrap: wrote baseline to {args.baseline}")
        return 0

    with open(args.baseline) as f:
        baseline: dict = json.load(f)

    failures = []
    for metric, base_val in baseline.items():
        if metric not in results:
            failures.append(f"  MISSING  {metric}  (baseline={base_val:.6g})")
            continue
        measured = results[metric]
        # Regression = measured is larger (slower) than baseline by more than threshold.
        # Improvements are never failures.
        if base_val > 0:
            ratio = (measured - base_val) / base_val
        else:
            ratio = 0.0
        if ratio > args.threshold:
            pct = ratio * 100
            failures.append(
                f"  REGRESSED  {metric}  "
                f"baseline={base_val:.6g}  measured={measured:.6g}  "
                f"change=+{pct:.1f}%  (threshold={args.threshold * 100:.0f}%)"
            )

    if failures:
        print("[bench] FAILED — regressions detected:", file=sys.stderr)
        for line in failures:
            print(line, file=sys.stderr)
        return 1

    print("[bench] OK — all metrics within threshold")
    for metric, base_val in baseline.items():
        measured = results.get(metric, float("nan"))
        if base_val > 0:
            ratio = (measured - base_val) / base_val
        else:
            ratio = 0.0
        sign = "+" if ratio >= 0 else ""
        print(f"  {metric:<30} baseline={base_val:.6g}  measured={measured:.6g}  {sign}{ratio * 100:.1f}%")
    return 0


if __name__ == "__main__":
    sys.exit(main())
