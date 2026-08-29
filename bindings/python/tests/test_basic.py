"""Embedded mushroomdb bindings — open → mutate → rule → query → explain."""

from __future__ import annotations

import pytest
import time

from mushroomdb import GraphDb


def test_insert_node_label_then_key(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Org", "org-01", {"founded_year": 2010})
    info = db.node_info("org-01")
    assert info["label"] == "Org"
    db.close()


def test_round_trip_numeric_within(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Org", "org-01", {"founded_year": 2010, "name": "Acme"})
    db.insert_node("Org", "org-02", {"founded_year": 2011, "ok": True})
    db.insert_node("Org", "org-03", {"founded_year": 2020, "rating": 0.5})
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


def test_value_mapping_dict_roundtrips(tmp_path):
    """Nested dicts round-trip as Value::Map; non-str keys still raise TypeError."""
    db = GraphDb.open(str(tmp_path / "db"))

    nested = {"a": 1, "b": [True, {"c": 0.5}]}
    db.insert_node("Person", "p1", {"meta": nested, "n": 1})

    # round-trip via node_info
    info = db.node_info("p1")
    assert info["props"]["meta"] == nested
    assert info["props"]["n"] == 1

    # round-trip via set_prop
    db.set_prop("p1", "extra", {"x": "hello", "y": [1, 2]})
    info2 = db.node_info("p1")
    assert info2["props"]["extra"] == {"x": "hello", "y": [1, 2]}

    # non-str key must still raise TypeError
    with pytest.raises(TypeError):
        db.set_prop("p1", "bad", {1: "int key not allowed"})

    db.close()


def test_set_prop_and_scalar_list_round_trip(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("L", "n", {"ok": True, "rating": 0.5})
    db.set_prop("n", "ok", False)
    db.set_prop("n", "rating", 1.25)
    db.set_prop("n", "tags", ["a", ["b", "c"]])
    info = db.node_info("n")["props"]
    assert info["ok"] is False
    assert info["rating"] == pytest.approx(1.25)
    assert info["tags"] == ["a", ["b", "c"]]
    db.close()


def test_create_rule_invalid_surfaces_engine_message(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(RuntimeError, match="invalid rule:"):
        db.create_rule(
            {
                "name": "",
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
    with pytest.raises((ValueError, RuntimeError), match="RuleDef"):
        db.create_rule({"name": "missing-the-rest"})
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
        db.insert_node("L", "k", {})
        assert db.node_info("k")["label"] == "L"
    with pytest.raises(RuntimeError, match="closed"):
        db.node_info("k")


# ---------------------------------------------------------------------------
# ingest_batch
# ---------------------------------------------------------------------------

def test_ingest_batch_atomicity_bad_edge(tmp_path):
    """A bad edge (non-existent dst) must reject the whole batch — zero nodes
    persisted and a RuntimeError is raised."""
    db = GraphDb.open(str(tmp_path / "db"))
    nodes = [{"key": f"n{i}", "label": "N", "props": {}} for i in range(5)]
    bad_edges = [{"edge_type": "KNOWS", "src": "n0", "dst": "ghost"}]
    with pytest.raises(RuntimeError):
        db.ingest_batch(nodes, bad_edges)
    # Nothing should be committed.
    for i in range(5):
        assert db.node_info(f"n{i}") is None
    db.close()


def test_ingest_batch_10k_round_trip(tmp_path):
    """10 000-node single-call ingest returns a report and all nodes are
    queryable afterwards."""
    db = GraphDb.open(str(tmp_path / "db"))
    nodes = [{"key": f"node-{i:05d}", "label": "Thing", "props": {"n": i}} for i in range(10_000)]
    report = db.ingest_batch(nodes)
    assert isinstance(report, dict)
    assert report["inserted"] == 10_000
    assert report["edges_inserted"] == 0
    assert report["row_errors"] == []
    # Spot-check a few nodes.
    for idx in (0, 1, 4999, 9999):
        info = db.node_info(f"node-{idx:05d}")
        assert info is not None
        assert info["label"] == "Thing"
        assert info["props"]["n"] == idx
    db.close()


def test_ingest_batch_with_edges(tmp_path):
    """ingest_batch inserts nodes and edges atomically; report reflects both."""
    db = GraphDb.open(str(tmp_path / "db"))
    nodes = [
        {"key": "a", "label": "X", "props": {}},
        {"key": "b", "label": "X", "props": {}},
    ]
    edges = [{"edge_type": "LINK", "src": "a", "dst": "b"}]
    report = db.ingest_batch(nodes, edges)
    assert report["inserted"] == 2
    assert report["edges_inserted"] == 1
    assert "b" in db.neighbors("a", "LINK", "out")
    db.close()


def test_ingest_batch_duplicate_edge_not_counted(tmp_path):
    """Resubmitting an existing edge must not increment edges_inserted.
    Mirrors HTTP /ingest duplicate-edge semantics from P9 T7."""
    db = GraphDb.open(str(tmp_path / "db"))
    # Create two nodes first via single inserts, then test edge-only batches.
    db.insert_node("N", "m", {})
    db.insert_node("N", "n", {})
    r1 = db.ingest_batch([], [{"edge_type": "REL", "src": "m", "dst": "n"}])
    assert r1["edges_inserted"] == 1  # new edge — counted
    r2 = db.ingest_batch([], [{"edge_type": "REL", "src": "m", "dst": "n"}])
    assert r2["edges_inserted"] == 0  # duplicate — not counted
    db.close()


# ---------------------------------------------------------------------------
# stats
# ---------------------------------------------------------------------------

def test_stats_shape(tmp_path):
    """stats() returns a dict with the expected top-level keys."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("L", "x", {"v": 1})
    s = db.stats()
    assert isinstance(s, dict)
    for key in ("nodes_live", "nodes_tombstoned", "edges", "rules"):
        assert key in s, f"missing key: {key}"
    assert s["nodes_live"] == 1
    assert s["nodes_tombstoned"] == 0
    assert isinstance(s["rules"], list)
    db.close()


def test_stats_rule_shape(tmp_path):
    """Each entry in stats()['rules'] has name/edges/tripped/fires/approximate."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("P", "p", {"score": 10})
    db.create_rule({
        "name": "match_score",
        "src_label": "P",
        "dst_label": "P",
        "predicate": {"NumericWithin": {"field": "score", "tolerance": 5.0}},
        "edge_type": "SIMILAR",
        "weight_prop": None,
        "max_edges": None,
        "approximate": False,
    })
    s = db.stats()
    rule_names = [r["name"] for r in s["rules"]]
    assert "match_score" in rule_names
    for r in s["rules"]:
        for k in ("name", "edges", "tripped", "fires", "approximate"):
            assert k in r, f"missing key '{k}' in rule stats entry"
    # exact rule: approximate must be False
    match_rule = next(r for r in s["rules"] if r["name"] == "match_score")
    assert match_rule["approximate"] is False
    db.close()


def test_stats_rule_approximate_true(tmp_path):
    """stats()['rules'] exposes approximate=True for IVF-Flat rules."""
    db = GraphDb.open(str(tmp_path / "db"))
    # Two nodes with vectors so the rule can index them.
    db.insert_node("Item", "a", {"vec": [1.0, 0.0]})
    db.insert_node("Item", "b", {"vec": [0.9, 0.1]})
    db.create_rule({
        "name": "approx_sim",
        "src_label": "Item",
        "dst_label": "Item",
        "predicate": {"VectorSimilar": {"field": "vec", "min": 0.5}},
        "edge_type": "NEAR",
        "weight_prop": None,
        "max_edges": None,
        "approximate": True,
    })
    s = db.stats()
    approx_rule = next((r for r in s["rules"] if r["name"] == "approx_sim"), None)
    assert approx_rule is not None, "approx_sim rule not found in stats"
    assert "approximate" in approx_rule, "missing 'approximate' key in rule stats"
    assert approx_rule["approximate"] is True
    db.close()


# ---------------------------------------------------------------------------
# snapshot
# ---------------------------------------------------------------------------

def test_delete_edge_happy_path(tmp_path):
    """delete_edge removes a user-inserted edge and returns True; second call returns False."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Thing", "a", {})
    db.insert_node("Thing", "b", {})
    db.insert_edge("KNOWS", "a", "b")

    assert "b" in db.neighbors("a", "KNOWS", "out")
    removed = db.delete_edge("KNOWS", "a", "b")
    assert removed is True
    assert "b" not in db.neighbors("a", "KNOWS", "out")

    # Second delete on an absent edge returns False (idempotent).
    removed2 = db.delete_edge("KNOWS", "a", "b")
    assert removed2 is False
    db.close()


def test_delete_edge_derived_raises(tmp_path):
    """delete_edge on a rule-derived edge should raise (derived edges are rule-owned)."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Org", "x", {"year": 2010})
    db.insert_node("Org", "y", {"year": 2011})
    db.create_rule({
        "name": "r",
        "src_label": "Org",
        "dst_label": "Org",
        "predicate": {"NumericWithin": {"field": "year", "tolerance": 2.0}},
        "edge_type": "SIMILAR_YEAR",
        "weight_prop": "score",
        "max_edges": None,
    })
    # Derived edge exists
    assert "y" in db.neighbors("x", "SIMILAR_YEAR", "out")
    # Deleting it must raise (not silently succeed)
    with pytest.raises(Exception):
        db.delete_edge("SIMILAR_YEAR", "x", "y")
    db.close()


def test_batch_edges_happy_path(tmp_path):
    """batch_edges inserts and deletes edges in one atomic WAL frame."""
    db = GraphDb.open(str(tmp_path / "db"))
    for k in ("a", "b", "c", "d"):
        db.insert_node("N", k, {})
    db.insert_edge("KNOWS", "a", "b")  # will be deleted

    result = db.batch_edges(
        inserts=[
            {"edge_type": "KNOWS", "src": "a", "dst": "c"},
            {"edge_type": "KNOWS", "src": "a", "dst": "d"},
        ],
        deletes=[{"edge_type": "KNOWS", "src": "a", "dst": "b"}],
    )
    assert result["edges_inserted"] == 2
    assert result["edges_deleted"] == 1

    out = db.neighbors("a", "KNOWS", "out")
    assert "c" in out
    assert "d" in out
    assert "b" not in out
    db.close()


def test_batch_edges_bad_edge_is_atomic(tmp_path):
    """batch_edges with a nonexistent-node edge should error; inserts before it roll back."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("N", "a", {})
    db.insert_node("N", "b", {})

    # Insert a valid edge first to confirm it's absent after rollback
    with pytest.raises(Exception):
        db.batch_edges(
            inserts=[
                {"edge_type": "KNOWS", "src": "a", "dst": "b"},
                {"edge_type": "KNOWS", "src": "a", "dst": "ghost"},  # ghost does not exist
            ],
        )
    # On error the whole batch must have been rolled back — a→b must not exist
    assert "b" not in db.neighbors("a", "KNOWS", "out")
    db.close()


def test_snapshot_and_reopen(tmp_path):
    """snapshot() writes a durable checkpoint; reopening after snapshot yields
    the same data without full WAL replay."""
    path = str(tmp_path / "db")
    db = GraphDb.open(path)
    nodes = [{"key": f"s{i}", "label": "S", "props": {"i": i}} for i in range(1000)]
    db.ingest_batch(nodes)
    db.snapshot()
    db.close()

    # Reopen — should be fast (snapshot path, no WAL replay).
    t0 = time.perf_counter()
    db2 = GraphDb.open(path)
    elapsed = time.perf_counter() - t0

    assert db2.node_info("s0")["props"]["i"] == 0
    assert db2.node_info("s999")["props"]["i"] == 999
    # Reopen after snapshot should be sub-second for 1k nodes.
    assert elapsed < 5.0, f"snapshot reopen took {elapsed:.2f}s — unexpectedly slow"
    db2.close()


# ── Task 2: query() params — dict form, tuple-list form, injection inertness ──

def test_query_params_dict_form(tmp_path):
    """query(cypher, params={...}) passes bound params to the engine."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Org", "org-01", {"founded_year": 2010})
    db.insert_node("Org", "org-02", {"founded_year": 2020})
    rows = db.query(
        "MATCH (n:Org) WHERE n.founded_year = $yr RETURN n",
        params={"yr": 2010},
    )
    assert len(rows) == 1
    assert rows[0]["n"] == "org-01"
    db.close()


def test_query_params_tuple_list_form(tmp_path):
    """query(cypher, params=[("name", value)]) tuple-list also works."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Org", "org-01", {"founded_year": 2010})
    db.insert_node("Org", "org-02", {"founded_year": 2020})
    rows = db.query(
        "MATCH (n:Org) WHERE n.founded_year = $yr RETURN n",
        params=[("yr", 2020)],
    )
    assert len(rows) == 1
    assert rows[0]["n"] == "org-02"
    db.close()


def test_query_no_params_unchanged(tmp_path):
    """query(cypher) with no params still works (backward compat)."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Org", "org-01", {"founded_year": 2010})
    rows = db.query("MATCH (n:Org) RETURN n")
    assert len(rows) == 1
    db.close()


def test_query_params_injection_string_inert(tmp_path):
    """An injection string passed as a param value is treated as a literal string value."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Org", "legit", {"name": "legit"})
    # This is the injection motivating test: the value looks like Cypher but is
    # just a string.  The query should return no rows (the name doesn't match).
    rows = db.query(
        "MATCH (n:Org) WHERE n.name = $name RETURN n",
        params={"name": "legit RETURN n //"},
    )
    assert len(rows) == 0, "injection string must be treated as literal, not executed"
    db.close()


# ── Task 2: rename_node ────────────────────────────────────────────────────────

def test_rename_node_python(tmp_path):
    """rename_node: old key gone, new key live, props preserved."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"age": 30})
    db.rename_node("alice", "alice2")
    assert db.node_info("alice") is None
    info = db.node_info("alice2")
    assert info is not None
    assert info["props"]["age"] == 30
    db.close()


# ── Task 2: insert_edge_upsert (Python) ───────────────────────────────────────

def test_insert_edge_upsert_python(tmp_path):
    """insert_edge_upsert: missing endpoints created as placeholder nodes."""
    db = GraphDb.open(str(tmp_path / "db"))
    result = db.insert_edge_upsert("KNOWS", "alice", "bob", "Person")
    assert result["nodes_created"] == 2
    assert result["edge_inserted"] is True
    assert db.node_info("alice")["label"] == "Person"
    assert db.node_info("bob")["label"] == "Person"
    assert "bob" in db.neighbors("alice", "KNOWS", "out")
    db.close()


# ── ANN binding: find_similar, search_hybrid, has_vector_rule, get_edge_prop ──

def test_find_similar_returns_list(tmp_path):
    """find_similar returns a list of (key, score) pairs."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Item", "a", {"vec": [1.0, 0.0]})
    db.insert_node("Item", "b", {"vec": [0.9, 0.1]})
    db.insert_node("Item", "c", {"vec": [-1.0, 0.0]})

    hits = db.find_similar("vec", [1.0, 0.0], label="Item", k=3, min=0.0)
    assert isinstance(hits, list)
    assert len(hits) >= 2
    keys = [h[0] for h in hits]
    assert "a" in keys
    assert "b" in keys
    assert hits[0][1] >= hits[1][1]
    db.close()


def test_find_similar_min_threshold(tmp_path):
    """min threshold filters out low-similarity results."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Item", "a", {"vec": [1.0, 0.0]})
    db.insert_node("Item", "b", {"vec": [-1.0, 0.0]})

    hits = db.find_similar("vec", [1.0, 0.0], label="Item", k=10, min=0.5)
    keys = [h[0] for h in hits]
    assert "a" in keys
    assert "b" not in keys
    db.close()


def test_find_similar_empty_vector(tmp_path):
    """Zero-length query vector returns no results (cannot normalise)."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Item", "a", {"vec": [1.0, 0.0]})
    hits = db.find_similar("vec", [0.0, 0.0], label="Item", k=5, min=0.0)
    assert hits == []
    db.close()


def test_search_hybrid_returns_list(tmp_path):
    """search_hybrid returns a list of (key, score) pairs."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Doc", "d1", {"text": "machine learning algorithms"})
    db.insert_node("Doc", "d2", {"text": "graph database query"})

    hits = db.search_hybrid("text", "machine", "vec", [], label="Doc", k=5)
    assert isinstance(hits, list)
    db.close()


def test_has_vector_rule_without_rule(tmp_path):
    """has_vector_rule returns False when no VectorSimilar rule is registered."""
    db = GraphDb.open(str(tmp_path / "db"))
    assert db.has_vector_rule("embedding") is False
    db.close()


def test_has_vector_rule_with_rule(tmp_path):
    """has_vector_rule returns True once a VectorSimilar rule is created."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Item", "x", {"embedding": [1.0, 0.0]})
    db.create_rule({
        "name": "ann_rule",
        "src_label": "Item",
        "dst_label": "Item",
        "predicate": {"VectorSimilar": {"field": "embedding", "min": 0.5}},
        "edge_type": "SIMILAR",
        "weight_prop": None,
        "max_edges": None,
        "approximate": True,
    })
    assert db.has_vector_rule("embedding") is True
    assert db.has_vector_rule("other_field") is False
    db.close()


def test_get_edge_prop_rule_weight(tmp_path):
    """get_edge_prop reads rule-written weight fields."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Thing", "a", {"score": 10.0})
    db.insert_node("Thing", "b", {"score": 9.0})
    db.create_rule({
        "name": "similar_score",
        "src_label": "Thing",
        "dst_label": "Thing",
        "predicate": {"NumericWithin": {"field": "score", "tolerance": 5.0}},
        "edge_type": "SIMILAR",
        "weight_prop": "score",
        "max_edges": None,
    })

    assert "b" in db.neighbors("a", "SIMILAR", "out")

    val = db.get_edge_prop("SIMILAR", "a", "b", "score")
    assert val is not None
    assert isinstance(val, float)
    db.close()


def test_get_edge_prop_missing_returns_none(tmp_path):
    """get_edge_prop returns None for non-existent edges or missing fields."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("N", "x", {})
    db.insert_node("N", "y", {})
    db.insert_edge("LINK", "x", "y")

    assert db.get_edge_prop("LINK", "x", "y", "weight") is None
    assert db.get_edge_prop("LINK", "y", "x", "weight") is None
    assert db.get_edge_prop("GHOST", "x", "y", "weight") is None
    db.close()


def test_find_similar_rejects_bool_vector(tmp_path):
    """find_similar raises TypeError when the query vector contains booleans."""
    import pytest
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Item", "a", {"vec": [1.0, 0.0]})
    with pytest.raises(TypeError, match="bool"):
        db.find_similar("vec", [True, False], k=5)
    db.close()
