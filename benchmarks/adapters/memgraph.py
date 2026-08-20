"""Memgraph adapter for the comparative benchmark harness.

Requires Memgraph running locally on bolt://localhost:7687 (same default port as
Neo4j — **only one can run at once** unless you remap ports).
If the ``mgclient`` or ``neo4j`` driver is not installed, or the bolt endpoint
is unreachable, every public function calls pytest.skip() with a clear message.

Start Memgraph with Docker::

    docker run -d --name memgraph-bench \\
        -p 7687:7687 -p 7444:7444 \\
        memgraph/memgraph-platform

Install a bolt driver (mgclient preferred; neo4j driver also works)::

    pip install mgclient
    # or: pip install neo4j

Auto-derivation (rule_derive) has no equivalent in Memgraph — excluded from the
cross-engine comparison table.
"""

from __future__ import annotations

import math
import time
from typing import Any

_BOLT_URI = "bolt://localhost:7687"
_AUTH = ("", "")  # Memgraph default: no auth

_SKIP_MSG_NO_DRIVER = (
    "Memgraph adapter: no bolt driver installed — "
    "install with 'pip install mgclient' or 'pip install neo4j' to enable "
    "Memgraph benchmarks."
)
_SKIP_MSG_NO_SERVER = (
    f"Memgraph adapter: could not connect to bolt endpoint {_BOLT_URI} — "
    "start Memgraph (e.g. docker run -d memgraph/memgraph-platform -p 7687:7687) "
    "to enable Memgraph benchmarks."
)


def _driver() -> Any:
    """Return a bolt driver connected to Memgraph, or raise ImportError/RuntimeError."""
    drv = None
    # Try mgclient first (Memgraph's own Python client).
    try:
        import mgclient  # type: ignore[import]
        conn = mgclient.connect(host="127.0.0.1", port=7687)
        return ("mgclient", conn)
    except ImportError:
        pass
    except Exception:
        pass
    # Fallback: neo4j driver (also speaks bolt).
    try:
        from neo4j import GraphDatabase  # type: ignore[import]
        drv = GraphDatabase.driver(_BOLT_URI, auth=("", ""))
        drv.verify_connectivity()
        return ("neo4j", drv)
    except ImportError:
        raise ImportError(_SKIP_MSG_NO_DRIVER)
    except Exception as e:
        raise RuntimeError(_SKIP_MSG_NO_SERVER) from e


def _check_or_skip() -> tuple[str, Any]:
    try:
        return _driver()
    except ImportError as e:
        _pytest_skip(str(e))
    except RuntimeError as e:
        _pytest_skip(str(e))
    # unreachable but satisfies type checker
    raise RuntimeError("unreachable")


def _pytest_skip(msg: str) -> None:
    try:
        import pytest  # type: ignore[import]
        pytest.skip(msg)
    except ImportError:
        raise RuntimeError(msg)


def _percentile(xs: list[float], p: float) -> float:
    if not xs:
        return float("nan")
    s = sorted(xs)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * (p / 100.0)
    f = math.floor(k)
    c = min(f + 1, len(s) - 1)
    return s[f] + (s[c] - s[f]) * (k - f)


def _run_query(driver_info: tuple[str, Any], cypher: str, params: dict | None = None) -> list:
    kind, drv = driver_info
    params = params or {}
    if kind == "neo4j":
        with drv.session() as session:
            result = session.run(cypher, **params)
            return list(result)
    else:
        cursor = drv.cursor()
        cursor.execute(cypher, params)
        return cursor.fetchall()


def bulk_ingest(nodes: list[dict]) -> dict[str, Any]:
    """Ingest *nodes* into Memgraph via UNWIND CREATE."""
    info = _check_or_skip()
    kind, drv = info
    t0 = time.perf_counter()
    inserted = 0
    chunk_size = 1_000
    from collections import defaultdict
    by_label: dict[str, list[dict]] = defaultdict(list)
    for n in nodes:
        row: dict = {"key": n["key"], **n.get("props", {})}
        by_label[n["label"]].append(row)
    for label, batch in by_label.items():
        for i in range(0, len(batch), chunk_size):
            chunk = batch[i : i + chunk_size]
            try:
                # SET n = row stores all fields (key + props); n.key and n.size_bucket etc.
                _run_query(
                    info,
                    f"UNWIND $rows AS row CREATE (n:{label}) SET n = row",
                    {"rows": chunk},
                )
                inserted += len(chunk)
            except Exception:
                pass
    wall = time.perf_counter() - t0
    if kind == "neo4j":
        drv.close()
    return {
        "workload": "bulk_ingest",
        "engine": "memgraph",
        "node_count": inserted,
        "wall_s": wall,
        "throughput_nodes_per_s": inserted / wall if wall > 0 else float("nan"),
    }


def neighborhood_depth1(sample_keys: list[str]) -> dict[str, Any]:
    """Time depth-1 neighbourhood query."""
    info = _check_or_skip()
    samples: list[float] = []
    for key in sample_keys:
        t0 = time.perf_counter()
        try:
            _run_query(info, "MATCH (n {key: $k})-[r]->(m) RETURN type(r), m.key", {"k": key})
        except Exception:
            pass
        samples.append(time.perf_counter() - t0)
    kind, drv = info
    if kind == "neo4j":
        drv.close()
    return {
        "workload": "neighborhood_depth1",
        "engine": "memgraph",
        "n": len(samples),
        "p50_s": _percentile(samples, 50),
        "p95_s": _percentile(samples, 95),
        "wall_total_s": sum(samples),
    }


def neighborhood_depth2(sample_keys: list[str]) -> dict[str, Any]:
    """Time depth-2 neighbourhood query."""
    info = _check_or_skip()
    samples: list[float] = []
    for key in sample_keys:
        t0 = time.perf_counter()
        try:
            _run_query(info, "MATCH (n {key: $k})-[*1..2]->(m) RETURN m.key LIMIT 500", {"k": key})
        except Exception:
            pass
        samples.append(time.perf_counter() - t0)
    kind, drv = info
    if kind == "neo4j":
        drv.close()
    return {
        "workload": "neighborhood_depth2",
        "engine": "memgraph",
        "n": len(samples),
        "p50_s": _percentile(samples, 50),
        "p95_s": _percentile(samples, 95),
        "wall_total_s": sum(samples),
    }


def cypher_scan_filter() -> dict[str, Any]:
    """Scan-filter-project: MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key"""
    info = _check_or_skip()
    cypher = "MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key"
    t0 = time.perf_counter()
    try:
        rows = _run_query(info, cypher)
        row_count = len(rows)
    except Exception:
        row_count = 0
    wall = time.perf_counter() - t0
    kind, drv = info
    if kind == "neo4j":
        drv.close()
    return {
        "workload": "cypher_scan_filter",
        "engine": "memgraph",
        "query": cypher,
        "row_count": row_count,
        "wall_s": wall,
    }


def cypher_two_hop() -> dict[str, Any]:
    """Two-hop join via Cypher."""
    info = _check_or_skip()
    cypher = (
        "MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)"
        "-[:INDUSTRY_ALIGNMENT]->(t2:Talent) "
        "RETURN t.key, c.key, t2.key LIMIT 200"
    )
    t0 = time.perf_counter()
    try:
        rows = _run_query(info, cypher)
        row_count = len(rows)
    except Exception:
        row_count = 0
    wall = time.perf_counter() - t0
    kind, drv = info
    if kind == "neo4j":
        drv.close()
    return {
        "workload": "cypher_two_hop",
        "engine": "memgraph",
        "query": cypher,
        "row_count": row_count,
        "wall_s": wall,
    }


def cold_start_to_first_query() -> dict[str, Any]:
    """Connect to an already-running Memgraph server and run one depth-1 query.

    This measures connect+query latency only — server boot time is excluded.
    Boot-to-ready is reported separately in the run script.
    """
    info = _check_or_skip()
    t0 = time.perf_counter()
    try:
        rows = _run_query(info, "MATCH (n:Talent) RETURN n.key LIMIT 1")
        wall = time.perf_counter() - t0
        row_count = len(rows)
    except Exception as exc:
        wall = time.perf_counter() - t0
        row_count = 0
        kind, drv = info
        if kind == "neo4j":
            drv.close()
        return {
            "workload": "cold_start_to_first_query",
            "engine": "memgraph",
            "wall_s": wall,
            "row_count": row_count,
            "note": f"error: {exc}",
        }
    kind, drv = info
    if kind == "neo4j":
        drv.close()
    return {
        "workload": "cold_start_to_first_query",
        "engine": "memgraph",
        "wall_s": wall,
        "row_count": row_count,
        "note": "connect-only (server already running); boot-to-ready reported separately",
    }
