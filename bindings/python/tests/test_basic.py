"""Embedded mushroomdb bindings — open → mutate → rule → query → explain."""

from __future__ import annotations

import pytest

from mushroomdb import GraphDb


def test_round_trip_numeric_within(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("org-01", "Org", {"founded_year": 2010, "name": "Acme"})
    db.insert_node("org-02", "Org", {"founded_year": 2011, "ok": True})
    db.insert_node("org-03", "Org", {"founded_year": 2020, "rating": 0.5})
    db.create_rule(
        {
            "name": "founded_within",
            "src_label": "Org",
            "dst_label": "Org",
            "predicate": {
                "NumericWithin": {"field": "founded_year", "tolerance": 2.0}
            },
            "edge_type": "FOUNDED_WITHIN",
            "weight_prop": "score",
            "max_edges": None,
        }
    )

    rows = db.query(
        "MATCH (a:Org)-[:FOUNDED_WITHIN]->(b:Org) RETURN a, b ORDER BY a, b"
    )
    pairs = {(r["a"], r["b"]) for r in rows}
    assert ("org-01", "org-02") in pairs
    assert ("org-02", "org-01") in pairs
    assert ("org-01", "org-03") not in pairs

    why = db.explain("org-01", "org-02")
    assert any(
        e["rule"] == "founded_within" and e["edge_type"] == "FOUNDED_WITHIN"
        for e in why
    )

    info = db.node_info("org-01")
    assert info["key"] == "org-01"
    assert info["label"] == "Org"
    assert info["props"]["founded_year"] == 2010
    assert info["props"]["name"] == "Acme"
    assert db.node_info("ghost") is None

    edges = db.node_edges("org-01")
    derived = [
        e
        for e in edges
        if e["edge_type"] == "FOUNDED_WITHIN" and e["derived"] is True
    ]
    assert any(e["dst_key"] == "org-02" or e["src_key"] == "org-02" for e in derived)

    db.insert_edge("KNOWS", "org-01", "org-02")
    assert "org-02" in db.neighbors("org-01", "KNOWS", "out")
    assert "org-01" in db.neighbors("org-02", "KNOWS", "in")
    db.close()


def test_value_mapping_rejects_dict(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("p1", "Person", {"skills": ["rust", "graph"], "n": 1})
    with pytest.raises(TypeError, match="dict"):
        db.insert_node("p2", "Person", {"nested": {"a": 1}})
    with pytest.raises(TypeError, match="dict"):
        db.set_prop("p1", "meta", {"a": 1})
    info = db.node_info("p1")
    assert info["props"]["skills"] == ["rust", "graph"]
    assert info["props"]["n"] == 1
    db.close()


def test_query_error_is_runtime_error_with_stage_prefix(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(RuntimeError, match=r"parse:"):
        db.query("THIS IS NOT CYPHER")
    with pytest.raises(RuntimeError, match="node key not found: ghost"):
        db.node_edges("ghost")
    db.close()


def test_context_manager_closes(tmp_path):
    path = str(tmp_path / "db")
    with GraphDb.open(path) as db:
        db.insert_node("k", "L", {})
        assert db.node_info("k")["label"] == "L"
    with pytest.raises(RuntimeError, match="closed"):
        db.node_info("k")
