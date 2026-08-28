#!/usr/bin/env bash
# benchmarks/ci/run.sh — build the release bench binary, run it, write results.json.
#
# Called by the `bench` CI job.  Must be run from the repository root.
#
# Usage:
#   benchmarks/ci/run.sh [--output <path>]
#
# Options:
#   --output <path>   Where to write results.json  (default: results.json)
#
# The script exits non-zero if the build or the bench example fails.
set -euo pipefail

OUTPUT="results.json"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --output)
            OUTPUT="$2"
            shift 2
            ;;
        *)
            echo "unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

echo "[bench] building release binary..."
cargo build --release -p mushroomdb --example ci_bench

# Shared CI runners are heterogeneous (different CPU generations run-to-run)
# and single-sample sub-second measurements flap far beyond the 15% gate.
# Run three iterations and compare the per-metric MEDIAN against the baseline.
echo "[bench] running ci_bench (3 iterations, per-metric median)..."
./target/release/examples/ci_bench > /tmp/bench_run_1.json
./target/release/examples/ci_bench > /tmp/bench_run_2.json
./target/release/examples/ci_bench > /tmp/bench_run_3.json

python3 - "$OUTPUT" << 'PYEOF'
import json, statistics, sys
runs = [json.load(open(f"/tmp/bench_run_{i}.json")) for i in (1, 2, 3)]
median = {k: statistics.median(r[k] for r in runs) for k in runs[0]}
with open(sys.argv[1], "w") as f:
    json.dump(median, f, indent=2)
    f.write("\n")
print(json.dumps(median, indent=2))
PYEOF

echo "[bench] results written to $OUTPUT (median of 3)"
