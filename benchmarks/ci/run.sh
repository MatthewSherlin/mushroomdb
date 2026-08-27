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

echo "[bench] running ci_bench..."
./target/release/examples/ci_bench | tee "$OUTPUT"

echo "[bench] results written to $OUTPUT"
