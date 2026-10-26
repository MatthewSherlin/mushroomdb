"""The offline graph retrieval example runs and shows a graph hop."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

EXAMPLE = Path(__file__).resolve().parents[3] / "examples" / "graph-retrieval" / "graph_retrieval.py"


def test_example_runs_offline_and_pulls_in_a_chunk_by_graph_hop():
    done = subprocess.run(
        [sys.executable, "-W", "error::DeprecationWarning", str(EXAMPLE)],
        capture_output=True,
        text=True,
        timeout=60,
        check=True,
    )
    assert "[c1] (vector match" in done.stdout
    # c3 shares no vector neighbourhood with the question; only the graph finds it.
    assert "[c3] (shares Ada Lovelace with c1)" in done.stdout
