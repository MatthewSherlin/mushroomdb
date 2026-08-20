"""Neo4j adapter for the comparative benchmark harness.

Requires Neo4j running locally on bolt://localhost:7687 (default port).
If the driver package is not installed or the bolt endpoint is unreachable,
every public function calls pytest.skip() with a clear message.

Start Neo4j with Docker for a quick local run::

    docker run -d --name neo4j-bench \\
        -p 7687:7687 -p 7474:7474 \\
        -e NEO4J_AUTH=none \\
        neo4j:5-community

Install the driver::

    pip install neo4j

Workloads mirror the ours adapter at the same scale/seed but use Cypher over
bolt.  Auto-derivation (rule_derive) has no equivalent in Neo4j — that
workload is excluded from the cross-engine table.
"""

from __future__ import annotations

import math
import time
from typing import Any

_BOLT_URI = "bolt://localhost:7687"
_AUTH = ("neo4j", "neo4j")

_SKIP_MSG_NO_DRIVER = (
    "Neo4j adapter: 'neo4j' Python driver not installed — "
    "install with 'pip install neo4j' to enable Neo4j benchmarks."
)
_SKIP_MSG_NO_SERVER = (
    f"Neo4j adapter: could not connect to bolt endpoint {_BOLT_URI} — "
    "start Neo4j (e.g. docker run -d neo4j:5-community -p 7687:7687) "
    "to enable Neo4j benchmarks."
)


def _driver() -> Any:
    """Return a neo4j.GraphDatabase driver or raise ImportError/RuntimeError."""
    try:
        from neo4j import GraphDatabase  # type: ignore[import]
    except ImportError as e:
        raise ImportError(_SKIP_MSG_NO_DRIVER) from e
    try:
        drv = GraphDatabase.driver(_BOLT_URI, auth=_AUTH)
        drv.verify_connectivity()
        return drv
    except Exception as e:
        raise RuntimeError(_SKIP_MSG_NO_SERVER) from e


def _check_or_skip() -> Any:
    """Return a driver, or call pytest.skip() if unavailable."""
    try:
        return _driver()
    except ImportError as e:
        _pytest_skip(str(e))
    except RuntimeError as e:
        _pytest_skip(str(e))


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


def bulk_ingest(nodes: list[dict]) -> dict[str, Any]:
    """Ingest *nodes* into Neo4j via UNWIND CREATE."""
    drv = _check_or_skip()
    t0 = time.perf_counter()
    inserted = 0
    chunk_size = 1_000
    with drv.session() as session:
        for i in range(0, len(nodes), chunk_size):
            chunk = nodes[i : i + chunk_size]
            records = [
                {"key": n["key"], "label": n["label"], "props": n["props"]}
                for n in chunk
            ]
            # Neo4j does not support dynamic labels in UNWIND; group by label.
            from collections import defaultdict
            by_label: dict[str, list[dict]] = defaultdict(list)
            for r in records:
                by_label[r["label"]].append({"key": r["key"], **r["props"]})
            for label, batch in by_label.items():
                result = session.run(
                    f"UNWIND $rows AS row "
                    f"CREATE (n:{label}) SET n = row",
                    rows=batch,
                )
                result.consume()
            inserted += len(chunk)
    wall = time.perf_counter() - t0
    drv.close()
    return {
        "workload": "bulk_ingest",
        "engine": "neo4j",
        "node_count": inserted,
        "wall_s": wall,
        "throughput_nodes_per_s": inserted / wall if wall > 0 else float("nan"),
    }


def neighborhood_depth1(sample_keys: list[str]) -> dict[str, Any]:
    """Time depth-1 neighbourhood query for each key."""
    drv = _check_or_skip()
    samples: list[float] = []
    with drv.session() as session:
        for key in sample_keys:
            t0 = time.perf_counter()
            result = session.run(
                "MATCH (n {key: $k})-[r]->(m) RETURN type(r), m.key",
                k=key,
            )
            list(result)
            samples.append(time.perf_counter() - t0)
    drv.close()
    return {
        "workload": "neighborhood_depth1",
        "engine": "neo4j",
        "n": len(samples),
        "p50_s": _percentile(samples, 50),
        "p95_s": _percentile(samples, 95),
        "wall_total_s": sum(samples),
    }


def neighborhood_depth2(sample_keys: list[str]) -> dict[str, Any]:
    """Time depth-2 neighbourhood query for each key."""
    drv = _check_or_skip()
    samples: list[float] = []
    with drv.session() as session:
        for key in sample_keys:
            t0 = time.perf_counter()
            result = session.run(
                "MATCH (n {key: $k})-[*1..2]->(m) RETURN m.key LIMIT 500",
                k=key,
            )
            list(result)
            samples.append(time.perf_counter() - t0)
    drv.close()
    return {
        "workload": "neighborhood_depth2",
        "engine": "neo4j",
        "n": len(samples),
        "p50_s": _percentile(samples, 50),
        "p95_s": _percentile(samples, 95),
        "wall_total_s": sum(samples),
    }


def cypher_scan_filter() -> dict[str, Any]:
    """Scan-filter-project: MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key"""
    drv = _check_or_skip()
    cypher = "MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key"
    t0 = time.perf_counter()
    with drv.session() as session:
        rows = list(session.run(cypher))
    wall = time.perf_counter() - t0
    drv.close()
    return {
        "workload": "cypher_scan_filter",
        "engine": "neo4j",
        "query": cypher,
        "row_count": len(rows),
        "wall_s": wall,
    }


def cypher_two_hop() -> dict[str, Any]:
    """Two-hop join: Talent→Company→Talent via INDUSTRY_ALIGNMENT."""
    drv = _check_or_skip()
    cypher = (
        "MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)"
        "-[:INDUSTRY_ALIGNMENT]->(t2:Talent) "
        "RETURN t.key, c.key, t2.key LIMIT 200"
    )
    t0 = time.perf_counter()
    with drv.session() as session:
        rows = list(session.run(cypher))
    wall = time.perf_counter() - t0
    drv.close()
    return {
        "workload": "cypher_two_hop",
        "engine": "neo4j",
        "query": cypher,
        "row_count": len(rows),
        "wall_s": wall,
    }
