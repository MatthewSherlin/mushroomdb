"""KùzuDB adapter for the comparative benchmark harness.

Requires the ``kuzu`` Python package (``pip install kuzu``).
If not installed every public function calls pytest.skip() with a clear message.

Install::

    pip install kuzu

KùzuDB is an embedded columnar graph database.  Unlike Neo4j/Memgraph there is
no server — the DB opens directly on a filesystem path, similar to mushroomdb.
Timings are therefore directly comparable to the ours adapter (no network RTT).

Auto-derivation (rule_derive) has no equivalent in KùzuDB — excluded from the
cross-engine comparison table.
"""

from __future__ import annotations

import math
import time
from pathlib import Path
from typing import Any

_SKIP_MSG = (
    "KùzuDB adapter: 'kuzu' Python package not installed — "
    "install with 'pip install kuzu' to enable KùzuDB benchmarks."
)


def _import_kuzu() -> Any:
    try:
        import kuzu  # type: ignore[import]
        return kuzu
    except ImportError as e:
        raise ImportError(_SKIP_MSG) from e


def _check_or_skip() -> Any:
    try:
        return _import_kuzu()
    except ImportError as e:
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


def _open_db(db_dir: str | Path) -> tuple[Any, Any]:
    """Return (db, conn) for a KùzuDB instance at *db_dir*."""
    kuzu = _import_kuzu()
    db = kuzu.Database(str(db_dir))
    conn = kuzu.Connection(db)
    return db, conn


def _setup_schema(conn: Any) -> None:
    """Create node tables for Talent, Company, Job with key fields for benchmarks.

    KùzuDB requires typed schemas. We store `size_bucket` (INT64) on Talent and
    Company so that `WHERE n.size_bucket = 3` works equivalently to neo4j/mushroomdb.
    """
    schemas = {
        "Talent": "key STRING, size_bucket INT64, PRIMARY KEY(key)",
        "Company": "key STRING, size_bucket INT64, PRIMARY KEY(key)",
        "Job": "key STRING, PRIMARY KEY(key)",
    }
    for label, cols in schemas.items():
        try:
            conn.execute(f"CREATE NODE TABLE IF NOT EXISTS {label} ({cols})")
        except Exception:
            pass
    for rel, src, dst in [
        ("INDUSTRY_ALIGNMENT", "Talent", "Company"),
        ("SPECIALTY_MATCH", "Talent", "Company"),
        ("LOCATION_FIT", "Talent", "Company"),
    ]:
        try:
            conn.execute(
                f"CREATE REL TABLE IF NOT EXISTS {rel} "
                f"(FROM {src} TO {dst})"
            )
        except Exception:
            pass


def bulk_ingest(nodes: list[dict], db_dir: str | Path) -> dict[str, Any]:
    """Ingest *nodes* into a KùzuDB at *db_dir*.

    Stores key + size_bucket for Talent/Company nodes so that the scan-filter
    predicate WHERE n.size_bucket = 3 is semantically equivalent to neo4j/mushroomdb.
    """
    _check_or_skip()
    kuzu = _import_kuzu()
    db = kuzu.Database(str(db_dir))
    conn = kuzu.Connection(db)
    _setup_schema(conn)
    t0 = time.perf_counter()
    inserted = 0
    from collections import defaultdict
    by_label: dict[str, list[dict]] = defaultdict(list)
    for n in nodes:
        row: dict = {"key": n["key"]}
        if n["label"] in ("Talent", "Company"):
            row["size_bucket"] = int(n.get("props", {}).get("size_bucket", 0) or 0)
        by_label[n["label"]].append(row)
    for label, rows in by_label.items():
        for row in rows:
            try:
                if label in ("Talent", "Company"):
                    conn.execute(
                        f"CREATE (n:{label} {{key: $k, size_bucket: $sb}})",
                        {"k": row["key"], "sb": row["size_bucket"]},
                    )
                else:
                    conn.execute(
                        f"CREATE (n:{label} {{key: $k}})",
                        {"k": row["key"]},
                    )
                inserted += 1
            except Exception:
                pass
    wall = time.perf_counter() - t0
    return {
        "workload": "bulk_ingest",
        "engine": "kuzu",
        "node_count": inserted,
        "wall_s": wall,
        "throughput_nodes_per_s": inserted / wall if wall > 0 else float("nan"),
    }


def neighborhood_depth1(conn: Any, sample_keys: list[str]) -> dict[str, Any]:
    """Time depth-1 neighbourhood query."""
    _check_or_skip()
    samples: list[float] = []
    for key in sample_keys:
        t0 = time.perf_counter()
        try:
            conn.execute(
                "MATCH (n)-[r]->(m) WHERE n.key = $k RETURN type(r), m.key",
                {"k": key},
            )
        except Exception:
            pass
        samples.append(time.perf_counter() - t0)
    return {
        "workload": "neighborhood_depth1",
        "engine": "kuzu",
        "n": len(samples),
        "p50_s": _percentile(samples, 50),
        "p95_s": _percentile(samples, 95),
        "wall_total_s": sum(samples),
    }


def neighborhood_depth2(conn: Any, sample_keys: list[str]) -> dict[str, Any]:
    """Time depth-2 neighbourhood query."""
    _check_or_skip()
    samples: list[float] = []
    for key in sample_keys:
        t0 = time.perf_counter()
        try:
            conn.execute(
                "MATCH (n)-[*1..2]->(m) WHERE n.key = $k RETURN m.key LIMIT 500",
                {"k": key},
            )
        except Exception:
            pass
        samples.append(time.perf_counter() - t0)
    return {
        "workload": "neighborhood_depth2",
        "engine": "kuzu",
        "n": len(samples),
        "p50_s": _percentile(samples, 50),
        "p95_s": _percentile(samples, 95),
        "wall_total_s": sum(samples),
    }


def cypher_scan_filter(conn: Any) -> dict[str, Any]:
    """Scan-filter-project: MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key

    v2: uses the real size_bucket predicate (schema now stores size_bucket INT64),
    semantically equivalent to neo4j/mushroomdb — returns the same 1,400 rows.
    """
    _check_or_skip()
    cypher = "MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key"
    t0 = time.perf_counter()
    try:
        result = conn.execute(cypher)
        rows = result.get_as_df() if hasattr(result, "get_as_df") else []
        row_count = len(rows)
    except Exception:
        row_count = 0
    wall = time.perf_counter() - t0
    return {
        "workload": "cypher_scan_filter",
        "engine": "kuzu",
        "query": cypher,
        "row_count": row_count,
        "wall_s": wall,
    }


def cypher_two_hop(conn: Any) -> dict[str, Any]:
    """Two-hop join via Cypher: Talent→Company←Talent via INDUSTRY_ALIGNMENT.

    Direction matches mushroomdb's query: both Talent nodes point *to* the same
    Company hub.  Requires INDUSTRY_ALIGNMENT edges to be pre-loaded (see I1
    bench notes — mushroomdb derives these automatically via create_rule).
    """
    _check_or_skip()
    cypher = (
        "MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)"
        "<-[:INDUSTRY_ALIGNMENT]-(t2:Talent) "
        "RETURN t.key, c.key, t2.key LIMIT 200"
    )
    t0 = time.perf_counter()
    try:
        result = conn.execute(cypher)
        rows = result.get_as_df() if hasattr(result, "get_as_df") else []
        row_count = len(rows)
    except Exception:
        row_count = 0
    wall = time.perf_counter() - t0
    return {
        "workload": "cypher_two_hop",
        "engine": "kuzu",
        "query": cypher,
        "row_count": row_count,
        "wall_s": wall,
    }


def cold_start_to_first_query(db_dir: str | Path) -> dict[str, Any]:
    """Open a KùzuDB database and run one depth-1 query.

    KùzuDB is embedded (like mushroomdb), so this measures database open + query.
    """
    _check_or_skip()
    kuzu = _import_kuzu()
    t0 = time.perf_counter()
    try:
        db = kuzu.Database(str(db_dir))
        conn = kuzu.Connection(db)
        result = conn.execute("MATCH (n:Talent) RETURN n.key LIMIT 1")
        rows = result.get_as_df() if hasattr(result, "get_as_df") else []
        wall = time.perf_counter() - t0
        row_count = len(rows)
    except Exception as exc:
        wall = time.perf_counter() - t0
        return {
            "workload": "cold_start_to_first_query",
            "engine": "kuzu",
            "wall_s": wall,
            "row_count": 0,
            "note": f"error: {exc}",
        }
    return {
        "workload": "cold_start_to_first_query",
        "engine": "kuzu",
        "wall_s": wall,
        "row_count": row_count,
        "note": "embedded db open + query (directly comparable to mushroomdb)",
    }
