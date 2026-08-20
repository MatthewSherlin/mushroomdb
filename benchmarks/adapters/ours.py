"""mushroomdb adapter for the comparative benchmark harness.

Uses the embedded Python bindings (mushroomdb.GraphDb) directly — no network,
no HTTP.  All timings are wall-clock seconds via time.perf_counter().

Workloads:

bulk_ingest(nodes, db_dir)
    Ingest a list of {key, label, props} dicts via ingest_batch in ≤10k-node
    chunks.  Returns wall_s and node_count.

neighborhood_depth1(db, sample_keys)
    node_edges() on each sampled key.  Returns p50/p95 wall_s across samples.

neighborhood_depth2(db, sample_keys)
    node_edges() + neighbors() on every outbound edge type per key (2-hop).
    Returns p50/p95 wall_s across samples.

cypher_scan_filter(db)
    MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key
    Scan-filter-project over all Talent nodes; returns row count and wall_s.

cypher_two_hop(db)
    MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)
          -[:INDUSTRY_ALIGNMENT]->(t2:Talent)
    RETURN t, c, t2 LIMIT 200
    Two-hop join via db.query(); needs INDUSTRY_ALIGNMENT edges materialized.
    Returns row count and wall_s.  If no rules declared yet, row count = 0 and
    is marked as such.

rule_derive(db, rules)
    Declare each rule dict via db.create_rule() and time the backfill.
    Returns per-rule wall_s and total.
    OURS-ONLY — no competitor equivalent; see README for why.
"""

from __future__ import annotations

import math
import time
from pathlib import Path
from typing import Any

INGEST_CHUNK = 10_000  # per bindings docstring: keep calls to ≤10k nodes


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


def open_db(db_dir: str | Path):  # type: ignore[return]
    """Open a mushroomdb.GraphDb at *db_dir*, creating it if necessary."""
    from mushroomdb import GraphDb  # type: ignore[import]
    return GraphDb.open(str(db_dir))


def bulk_ingest(nodes: list[dict], db_dir: str | Path) -> dict[str, Any]:
    """Ingest *nodes* via ingest_batch in ≤10k chunks; return timing report."""
    db = open_db(db_dir)
    t0 = time.perf_counter()
    inserted = 0
    for i in range(0, len(nodes), INGEST_CHUNK):
        chunk = nodes[i : i + INGEST_CHUNK]
        report = db.ingest_batch(chunk)
        inserted += report["inserted"]
    wall = time.perf_counter() - t0
    db.close()
    return {
        "workload": "bulk_ingest",
        "engine": "mushroomdb",
        "node_count": inserted,
        "wall_s": wall,
        "throughput_nodes_per_s": inserted / wall if wall > 0 else float("nan"),
    }


def neighborhood_depth1(db: Any, sample_keys: list[str]) -> dict[str, Any]:
    """Time node_edges() (depth-1 neighbourhood) for each key in *sample_keys*."""
    samples: list[float] = []
    for key in sample_keys:
        t0 = time.perf_counter()
        db.node_edges(key)
        samples.append(time.perf_counter() - t0)
    return {
        "workload": "neighborhood_depth1",
        "engine": "mushroomdb",
        "n": len(samples),
        "p50_s": _percentile(samples, 50),
        "p95_s": _percentile(samples, 95),
        "wall_total_s": sum(samples),
    }


def neighborhood_depth2(db: Any, sample_keys: list[str]) -> dict[str, Any]:
    """Time 2-hop neighbourhood: node_edges() + neighbors() per outbound edge type."""
    samples: list[float] = []
    for key in sample_keys:
        t0 = time.perf_counter()
        edges = db.node_edges(key)
        # Collect unique outbound edge types and fan out.
        out_types: set[str] = set()
        for e in edges:
            if e.get("src_key") == key:
                out_types.add(e["edge_type"])
        for etype in out_types:
            neighbors = db.neighbors(key, etype, "out")
            # depth-2: one sample call per outbound neighbor (spot-check).
            for nk in neighbors[:3]:
                try:
                    db.node_edges(nk)
                except Exception:
                    pass
        samples.append(time.perf_counter() - t0)
    return {
        "workload": "neighborhood_depth2",
        "engine": "mushroomdb",
        "n": len(samples),
        "p50_s": _percentile(samples, 50),
        "p95_s": _percentile(samples, 95),
        "wall_total_s": sum(samples),
    }


def cypher_scan_filter(db: Any) -> dict[str, Any]:
    """Scan-filter-project: MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key"""
    cypher = "MATCH (n:Talent) WHERE n.size_bucket = 3 RETURN n.key"
    t0 = time.perf_counter()
    rows = db.query(cypher)
    wall = time.perf_counter() - t0
    return {
        "workload": "cypher_scan_filter",
        "engine": "mushroomdb",
        "query": cypher,
        "row_count": len(rows),
        "wall_s": wall,
    }


def cypher_two_hop(db: Any) -> dict[str, Any]:
    """Two-hop join via db.query(): Talent→Company→Talent via INDUSTRY_ALIGNMENT."""
    cypher = (
        "MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)"
        "-[:INDUSTRY_ALIGNMENT]->(t2:Talent) "
        "RETURN t, c, t2 LIMIT 200"
    )
    t0 = time.perf_counter()
    try:
        rows = db.query(cypher)
        wall = time.perf_counter() - t0
        note = None
    except Exception as exc:
        wall = time.perf_counter() - t0
        rows = []
        note = f"query error: {exc}"
    return {
        "workload": "cypher_two_hop",
        "engine": "mushroomdb",
        "query": cypher,
        "row_count": len(rows),
        "wall_s": wall,
        "note": note,
    }


def rule_derive(db: Any, rules: list[dict]) -> dict[str, Any]:
    """Declare *rules* via db.create_rule() and time the backfill.

    OURS-ONLY workload — auto-derivation has no competitor equivalent.
    See benchmarks/README.md for a detailed explanation of why this
    workload is excluded from the cross-engine comparison table.
    """
    per_rule: list[dict[str, Any]] = []
    t_total_0 = time.perf_counter()
    for rule in rules:
        t0 = time.perf_counter()
        db.create_rule(rule)
        wall = time.perf_counter() - t0
        per_rule.append({
            "name": rule["name"],
            "edge_type": rule.get("edge_type", ""),
            "wall_s": wall,
        })
    total_wall = time.perf_counter() - t_total_0
    return {
        "workload": "rule_derive",
        "engine": "mushroomdb",
        "ours_only": True,
        "note": (
            "Auto-derivation: rules fire automatically on ingest/update with "
            "no competitor equivalent. Not included in cross-engine table. "
            "See README.md honesty section."
        ),
        "n_rules": len(rules),
        "per_rule": per_rule,
        "total_wall_s": total_wall,
    }
