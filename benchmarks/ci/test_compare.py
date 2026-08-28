"""
Tests for benchmarks/ci/compare.py.

Run with:  python3 -m pytest benchmarks/ci/test_compare.py -v
(pytest is available in the CI python venv; install with pip install pytest locally)
"""

import json
import subprocess
import sys
import tempfile
from pathlib import Path

COMPARE = Path(__file__).parent / "compare.py"


def _run(results: dict, baseline: dict, threshold: float = 0.15, bootstrap: bool = False):
    """Run compare.py in a subprocess, return (returncode, stdout+stderr)."""
    with tempfile.TemporaryDirectory() as tmp:
        results_path = Path(tmp) / "results.json"
        baseline_path = Path(tmp) / "baseline.json"
        results_path.write_text(json.dumps(results))
        baseline_path.write_text(json.dumps(baseline))

        cmd = [
            sys.executable,
            str(COMPARE),
            str(results_path),
            str(baseline_path),
            "--threshold",
            str(threshold),
        ]
        if bootstrap:
            cmd.append("--bootstrap")

        proc = subprocess.run(cmd, capture_output=True, text=True)
        return proc.returncode, proc.stdout + proc.stderr


def test_within_threshold_passes():
    """All metrics within 15% of baseline — must exit 0."""
    baseline = {
        "ingest_wall_s": 1.0,
        "rule_backfill_wall_s": 2.0,
        "snapshot_write_s": 0.5,
        "snapshot_open_s": 0.3,
        "query_p50_ms": 10.0,
    }
    # Exactly at the threshold boundary (14% slower) — should still pass.
    results = {k: v * 1.14 for k, v in baseline.items()}
    rc, out = _run(results, baseline)
    assert rc == 0, f"expected exit 0, got {rc}:\n{out}"


def test_regression_fails():
    """A cpu-bound metric >15% slower than baseline — must exit non-zero."""
    baseline = {
        "ingest_wall_s": 1.0,
        "rule_backfill_wall_s": 2.0,
        "snapshot_write_s": 0.5,
        "snapshot_open_s": 0.3,
        "query_p50_ms": 10.0,
    }
    # query_p50_ms (strict 15% band) regresses by 20% — should fail.
    results = dict(baseline)
    results["query_p50_ms"] = baseline["query_p50_ms"] * 1.20
    rc, out = _run(results, baseline)
    assert rc != 0, f"expected non-zero exit for regression, got {rc}:\n{out}"
    assert "query_p50_ms" in out, f"expected regressed metric name in output:\n{out}"


def test_fsync_bound_band():
    """fsync-bound metrics gate at 60%: +20% passes, +70% fails."""
    baseline = {
        "ingest_wall_s": 1.0,
        "rule_backfill_wall_s": 2.0,
        "snapshot_write_s": 0.5,
        "snapshot_open_s": 0.3,
        "query_p50_ms": 10.0,
    }
    # +20% on fsync-bound metrics: within the 60% band — passes.
    results = dict(baseline)
    results["snapshot_open_s"] = baseline["snapshot_open_s"] * 1.20
    results["snapshot_write_s"] = baseline["snapshot_write_s"] * 1.20
    rc, out = _run(results, baseline)
    assert rc == 0, f"+20% fsync-bound should pass the 60% band, got {rc}:\n{out}"
    # +70% on an fsync-bound metric: beyond the band — fails.
    results["snapshot_write_s"] = baseline["snapshot_write_s"] * 1.70
    rc, out = _run(results, baseline)
    assert rc != 0, f"+70% fsync-bound should fail, got {rc}:\n{out}"
    assert "snapshot_write_s" in out


def test_missing_metric_fails():
    """results.json is missing a metric present in baseline — must exit non-zero."""
    baseline = {
        "ingest_wall_s": 1.0,
        "rule_backfill_wall_s": 2.0,
        "snapshot_write_s": 0.5,
        "snapshot_open_s": 0.3,
        "query_p50_ms": 10.0,
    }
    # Drop query_p50_ms from results.
    results = {k: v for k, v in baseline.items() if k != "query_p50_ms"}
    rc, out = _run(results, baseline)
    assert rc != 0, f"expected non-zero exit for missing metric, got {rc}:\n{out}"
    assert "query_p50_ms" in out, f"expected missing metric name in output:\n{out}"
