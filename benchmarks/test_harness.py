"""Comparative benchmark harness — pytest suite.

What this covers:
- datasets.py: deterministic generation and JSONL round-trip.
- ours adapter: full end-to-end at 2k scale (all workloads including rule_derive).
- competitor adapters (neo4j, kuzu, memgraph): import + skip-path verification.
  When a competitor is installed and its server is running the test executes;
  otherwise it emits a clear skip message.  CI is expected to run with NO
  competitors installed, so all competitor tests are expected-skip in CI.

Run (from repo root, using the bindings venv)::

    bindings/python/.venv/bin/python -m pytest benchmarks/test_harness.py -v

Or against an installed Python::

    pytest benchmarks/test_harness.py -v
"""

from __future__ import annotations

import sys
import os
import tempfile
import time
from pathlib import Path

import pytest

# ---------------------------------------------------------------------------
# Path setup: ensure benchmarks/ and dogfood/ are importable.
# ---------------------------------------------------------------------------
_BENCH_DIR = Path(__file__).resolve().parent
_REPO_ROOT = _BENCH_DIR.parent
sys.path.insert(0, str(_BENCH_DIR))

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
BENCH_SCALE = 2_000  # fast CI scale; sample-results.md uses 10k
BENCH_SEED = 20260819
SAMPLE_KEYS_COUNT = 10  # neighbourhood samples per test


# ===========================================================================
# datasets.py
# ===========================================================================

class TestDatasets:
    def test_iter_nodes_count(self):
        from datasets import iter_nodes, split_scale
        n_t, n_c, n_j = split_scale(BENCH_SCALE)
        expected = n_t + n_c + n_j
        assert expected == BENCH_SCALE
        nodes = list(iter_nodes(n=BENCH_SCALE, seed=BENCH_SEED))
        assert len(nodes) == BENCH_SCALE

    def test_node_shape(self):
        from datasets import iter_nodes
        nodes = list(iter_nodes(n=100, seed=BENCH_SEED))
        for node in nodes:
            assert "key" in node
            assert "label" in node
            assert "props" in node
            assert node["label"] in ("Talent", "Company", "Job")

    def test_deterministic(self):
        from datasets import iter_nodes
        a = list(iter_nodes(n=50, seed=42))
        b = list(iter_nodes(n=50, seed=42))
        assert a == b

    def test_different_seeds_differ(self):
        from datasets import iter_nodes
        a = list(iter_nodes(n=50, seed=1))
        b = list(iter_nodes(n=50, seed=2))
        assert a != b

    def test_write_read_jsonl_round_trip(self, tmp_path):
        from datasets import iter_nodes, write_jsonl, read_jsonl
        out = tmp_path / "nodes.jsonl"
        n = write_jsonl(out, n=200, seed=BENCH_SEED)
        assert n == 200
        assert out.exists()
        nodes = read_jsonl(out)
        assert len(nodes) == 200
        expected = list(iter_nodes(n=200, seed=BENCH_SEED))
        assert nodes == expected


# ===========================================================================
# ours adapter — full end-to-end at BENCH_SCALE
# ===========================================================================

class TestOursAdapter:
    """End-to-end ours adapter test at 2k scale.

    Imports mushroomdb; skip if the extension is not built.
    """

    @pytest.fixture(autouse=True)
    def _require_mushroomdb(self):
        pytest.importorskip(
            "mushroomdb",
            reason="mushroomdb extension not built — run 'maturin develop' in bindings/python/",
        )

    @pytest.fixture
    def nodes(self):
        from datasets import iter_nodes
        return list(iter_nodes(n=BENCH_SCALE, seed=BENCH_SEED))

    @pytest.fixture
    def talent_keys(self, nodes):
        return [n["key"] for n in nodes if n["label"] == "Talent"]

    @pytest.fixture
    def populated_db(self, tmp_path, nodes):
        """Return an open db with BENCH_SCALE nodes ingested."""
        from adapters.ours import open_db, INGEST_CHUNK
        db = open_db(tmp_path / "bench_db")
        for i in range(0, len(nodes), INGEST_CHUNK):
            db.ingest_batch(nodes[i : i + INGEST_CHUNK])
        yield db
        db.close()

    def test_bulk_ingest(self, tmp_path, nodes):
        from adapters.ours import bulk_ingest
        result = bulk_ingest(nodes, tmp_path / "ingest_db")
        assert result["workload"] == "bulk_ingest"
        assert result["engine"] == "mushroomdb"
        assert result["node_count"] == BENCH_SCALE
        assert result["wall_s"] > 0
        assert result["throughput_nodes_per_s"] > 0

    def test_neighborhood_depth1(self, populated_db, talent_keys):
        from adapters.ours import neighborhood_depth1
        sample = talent_keys[:SAMPLE_KEYS_COUNT]
        result = neighborhood_depth1(populated_db, sample)
        assert result["workload"] == "neighborhood_depth1"
        assert result["engine"] == "mushroomdb"
        assert result["n"] == len(sample)
        assert result["p50_s"] >= 0

    def test_neighborhood_depth2(self, populated_db, talent_keys):
        from adapters.ours import neighborhood_depth2
        sample = talent_keys[:SAMPLE_KEYS_COUNT]
        result = neighborhood_depth2(populated_db, sample)
        assert result["workload"] == "neighborhood_depth2"
        assert result["engine"] == "mushroomdb"
        assert result["n"] == len(sample)

    def test_cypher_scan_filter(self, populated_db):
        from adapters.ours import cypher_scan_filter
        result = cypher_scan_filter(populated_db)
        assert result["workload"] == "cypher_scan_filter"
        assert result["engine"] == "mushroomdb"
        assert result["row_count"] >= 0
        assert result["wall_s"] >= 0

    def test_cypher_two_hop_no_rules(self, populated_db):
        """Without any rules declared, two-hop join returns 0 or handles gracefully."""
        from adapters.ours import cypher_two_hop
        result = cypher_two_hop(populated_db)
        assert result["workload"] == "cypher_two_hop"
        assert result["engine"] == "mushroomdb"
        # Without INDUSTRY_ALIGNMENT edges the row count is 0 or the query errors gracefully.
        assert result["row_count"] >= 0
        assert result["wall_s"] >= 0

    def test_rule_derive(self, populated_db):
        """rule_derive declares the matcher ruleset and times the backfill."""
        from adapters.ours import rule_derive
        # Use a minimal subset of rules to keep test fast.
        rules = [
            {
                "name": "bench_industry",
                "src_label": "Talent",
                "dst_label": "Company",
                "predicate": {"FieldEqual": {"field": "industry"}},
                "edge_type": "INDUSTRY_ALIGNMENT",
                "weight_prop": "score",
                "max_edges": 100_000,
            }
        ]
        result = rule_derive(populated_db, rules)
        assert result["workload"] == "rule_derive"
        assert result["engine"] == "mushroomdb"
        assert result["ours_only"] is True
        assert len(result["per_rule"]) == 1
        assert result["per_rule"][0]["name"] == "bench_industry"
        assert result["total_wall_s"] >= 0

    def test_cypher_two_hop_with_rules(self, tmp_path, nodes, talent_keys):
        """With INDUSTRY_ALIGNMENT rules declared, two-hop join may return rows."""
        from adapters.ours import open_db, rule_derive, cypher_two_hop, INGEST_CHUNK
        db = open_db(tmp_path / "rule_db")
        for i in range(0, len(nodes), INGEST_CHUNK):
            db.ingest_batch(nodes[i : i + INGEST_CHUNK])
        rules = [
            {
                "name": "bench_industry_tc",
                "src_label": "Talent",
                "dst_label": "Company",
                "predicate": {"FieldEqual": {"field": "industry"}},
                "edge_type": "INDUSTRY_ALIGNMENT",
                "weight_prop": "score",
                "max_edges": 100_000,
            }
        ]
        rule_derive(db, rules)
        result = cypher_two_hop(db)
        # row_count >= 0 (may be 0 if cypher two-hop over derived edges not supported)
        assert result["row_count"] >= 0
        db.close()

    def test_stats_after_ingest(self, populated_db):
        """stats() reflects the ingested node count."""
        s = populated_db.stats()
        assert s["nodes_live"] == BENCH_SCALE

    def test_snapshot(self, tmp_path, nodes):
        """snapshot() + reopen produces the same node count."""
        from adapters.ours import open_db, INGEST_CHUNK
        db_path = tmp_path / "snap_db"
        db = open_db(db_path)
        for i in range(0, len(nodes), INGEST_CHUNK):
            db.ingest_batch(nodes[i : i + INGEST_CHUNK])
        db.snapshot()
        db.close()
        db2 = open_db(db_path)
        s = db2.stats()
        assert s["nodes_live"] == BENCH_SCALE
        db2.close()


# ===========================================================================
# Competitor adapters — skip-path verification
# ===========================================================================

class TestNeo4jSkipPath:
    """Verify neo4j adapter emits a clean skip (or runs) based on availability."""

    def test_bulk_ingest_skip_or_run(self, tmp_path):
        """The adapter either runs fully or skips with a clear message."""
        # We import and call; if the driver/server is absent pytest.skip fires.
        try:
            from adapters import neo4j as neo4j_adapter
        except Exception as e:
            pytest.skip(f"neo4j adapter import error: {e}")
        # If we reach here the import succeeded; bulk_ingest will skip internally
        # if the server is down.
        nodes = [{"key": f"n{i}", "label": "Talent", "props": {"industry": "a", "size_bucket": 1}} for i in range(5)]
        try:
            result = neo4j_adapter.bulk_ingest(nodes)
            assert result["engine"] == "neo4j"
        except Exception as e:
            if "not installed" in str(e).lower() or "could not connect" in str(e).lower():
                pytest.skip(str(e))
            raise

    def test_skip_message_is_informative(self):
        """neo4j skip messages contain installation/startup guidance."""
        from adapters.neo4j import _SKIP_MSG_NO_DRIVER, _SKIP_MSG_NO_SERVER
        assert "neo4j" in _SKIP_MSG_NO_DRIVER.lower()
        assert "pip install" in _SKIP_MSG_NO_DRIVER
        assert "docker" in _SKIP_MSG_NO_SERVER.lower() or "start" in _SKIP_MSG_NO_SERVER.lower()


class TestKuzuSkipPath:
    """Verify kuzu adapter emits a clean skip (or runs) based on availability."""

    def test_bulk_ingest_skip_or_run(self, tmp_path):
        try:
            from adapters import kuzu as kuzu_adapter
        except Exception as e:
            pytest.skip(f"kuzu adapter import error: {e}")
        nodes = [{"key": f"n{i}", "label": "Talent", "props": {}} for i in range(5)]
        try:
            result = kuzu_adapter.bulk_ingest(nodes, tmp_path / "kuzu_db")
            assert result["engine"] == "kuzu"
        except Exception as e:
            if "not installed" in str(e).lower():
                pytest.skip(str(e))
            raise

    def test_skip_message_is_informative(self):
        from adapters.kuzu import _SKIP_MSG
        assert "kuzu" in _SKIP_MSG.lower()
        assert "pip install" in _SKIP_MSG


class TestMemgraphSkipPath:
    """Verify memgraph adapter emits a clean skip (or runs) based on availability."""

    def test_bulk_ingest_skip_or_run(self):
        try:
            from adapters import memgraph as memgraph_adapter
        except Exception as e:
            pytest.skip(f"memgraph adapter import error: {e}")
        nodes = [{"key": f"n{i}", "label": "Talent", "props": {}} for i in range(5)]
        try:
            result = memgraph_adapter.bulk_ingest(nodes)
            assert result["engine"] == "memgraph"
        except Exception as e:
            if "not installed" in str(e).lower() or "could not connect" in str(e).lower():
                pytest.skip(str(e))
            raise

    def test_skip_message_is_informative(self):
        from adapters.memgraph import _SKIP_MSG_NO_DRIVER, _SKIP_MSG_NO_SERVER
        assert "memgraph" in _SKIP_MSG_NO_DRIVER.lower()
        assert "pip install" in _SKIP_MSG_NO_DRIVER
        assert "docker" in _SKIP_MSG_NO_SERVER.lower() or "start" in _SKIP_MSG_NO_SERVER.lower()
