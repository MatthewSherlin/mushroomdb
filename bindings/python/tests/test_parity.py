"""Binding parity — the gaps a real integrator hit using the Python binding.

Covers v0.5.2 "binding parity": node deletion, key visibility in Cypher,
upsert, property removal, rule-shape round-tripping, idempotent rule creation,
uniform param shapes, and shipped type stubs.
"""

from __future__ import annotations

import inspect

import pytest

from mushroomdb import GraphDb


def _fit_rule(name: str = "same_team") -> dict:
    """A FieldEqual rule linking Person→Person on a shared `team` field."""
    return {
        "name": name,
        "src_label": "Person",
        "dst_label": "Person",
        "predicate": {"FieldEqual": {"field": "team"}},
        "edge_type": "SAME_TEAM",
        "weight_prop": None,
    }


# ---------------------------------------------------------------------------
# 1. delete_node
# ---------------------------------------------------------------------------


def test_delete_node_removes_derived_edge_and_node(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.create_rule(_fit_rule())
    db.insert_node("Person", "alice", {"team": "red"})
    db.insert_node("Person", "bob", {"team": "red"})

    rows = db.query("MATCH (a:Person)-[:SAME_TEAM]->(b:Person) RETURN a, b")
    assert ("alice", "bob") in {(r["a"], r["b"]) for r in rows}

    report = db.delete_node("alice")
    assert isinstance(report, dict)
    assert report["derived_edges"] >= 1
    assert report["manual_edges"] == 0

    assert db.node_info("alice") is None
    rows = db.query("MATCH (a:Person)-[:SAME_TEAM]->(b:Person) RETURN a, b")
    assert rows == []
    db.close()


def test_delete_node_counts_manual_edges(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {})
    db.insert_node("Person", "bob", {})
    db.insert_edge("KNOWS", "alice", "bob")

    report = db.delete_node("alice")
    assert report["manual_edges"] == 1
    assert report["derived_edges"] == 0
    assert db.node_info("alice") is None
    assert db.node_info("bob") is not None
    db.close()


def test_delete_node_unknown_key_raises(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(RuntimeError):
        db.delete_node("ghost")
    db.close()


# ---------------------------------------------------------------------------
# 2. key(n) in Cypher, and node_info["key"]
# ---------------------------------------------------------------------------


def test_key_function_returns_node_key(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"age": 30})
    db.insert_node("Person", "bob", {"age": 41})

    rows = db.query("MATCH (n:Person) RETURN key(n) AS k ORDER BY k")
    assert [r["k"] for r in rows] == ["alice", "bob"]

    # Bare (unaliased) projection also works.
    rows = db.query("MATCH (n:Person) RETURN key(n)")
    assert {next(iter(r.values())) for r in rows} == {"alice", "bob"}
    db.close()


def test_key_function_composes_with_other_scalars(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "Alice", {"age": 30})
    rows = db.query("MATCH (n:Person) RETURN toUpper(key(n)) AS k")
    assert rows[0]["k"] == "ALICE"
    rows = db.query("MATCH (n:Person) WHERE key(n) = 'Alice' RETURN n.age AS age")
    assert rows[0]["age"] == 30
    db.close()


def test_key_function_in_write_return(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"age": 30})
    rows = db.query_write("MATCH (n:Person) SET n.age = 31 RETURN key(n) AS k")
    assert rows[0]["k"] == "alice"
    db.close()


def test_key_function_rejects_non_node_argument(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"age": 30})
    with pytest.raises(RuntimeError):
        db.query("MATCH (n:Person) RETURN key(n.age) AS k")
    db.close()


def test_node_info_includes_key(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"age": 30})
    assert db.node_info("alice")["key"] == "alice"
    db.close()


# ---------------------------------------------------------------------------
# 3. upsert_node
# ---------------------------------------------------------------------------


def test_upsert_node_inserts_then_updates(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    assert db.upsert_node("Person", "alice", {"team": "red", "age": 30}) == "inserted"
    assert db.upsert_node("Person", "alice", {"team": "red", "age": 30}) == "updated"
    info = db.node_info("alice")
    assert info["props"] == {"team": "red", "age": 30}
    db.close()


def test_upsert_node_touches_only_changed_provided_fields(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"team": "red", "age": 30, "city": "NYC"})
    before = len(db.node_history("alice"))

    # Only `age` differs; `team` is unchanged and `city` is not provided.
    assert db.upsert_node("Person", "alice", {"team": "red", "age": 31}) == "updated"
    info = db.node_info("alice")
    assert info["props"] == {"team": "red", "age": 31, "city": "NYC"}

    changes = db.node_history("alice")[before:]
    assert [c.get("field") for c in changes] == ["age"]
    db.close()


def test_upsert_node_fires_and_retracts_rules(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.create_rule(_fit_rule())
    db.upsert_node("Person", "alice", {"team": "red"})
    db.upsert_node("Person", "bob", {"team": "blue"})
    assert db.query("MATCH (a:Person)-[:SAME_TEAM]->(b:Person) RETURN a") == []

    # Moving bob onto red fires the rule.
    db.upsert_node("Person", "bob", {"team": "red"})
    rows = db.query("MATCH (a:Person)-[:SAME_TEAM]->(b:Person) RETURN a, b")
    assert ("alice", "bob") in {(r["a"], r["b"]) for r in rows}

    # Moving bob back off red retracts it.
    db.upsert_node("Person", "bob", {"team": "green"})
    assert db.query("MATCH (a:Person)-[:SAME_TEAM]->(b:Person) RETURN a") == []
    db.close()


def test_upsert_node_label_mismatch_raises(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.upsert_node("Person", "alice", {"team": "red"})
    with pytest.raises(ValueError) as exc:
        db.upsert_node("Org", "alice", {"team": "red"})
    msg = str(exc.value)
    assert "alice" in msg and "Person" in msg and "Org" in msg
    db.close()


def test_upsert_node_empty_props_on_existing_is_a_noop(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"team": "red"})
    before = len(db.node_history("alice"))
    assert db.upsert_node("Person", "alice", {}) == "updated"
    assert len(db.node_history("alice")) == before
    assert db.node_info("alice")["props"] == {"team": "red"}
    db.close()


# ---------------------------------------------------------------------------
# 4. remove_prop / set_prop(..., None)
# ---------------------------------------------------------------------------


def test_remove_prop_drops_field(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"team": "red", "age": 30})
    assert db.remove_prop("alice", "team") is True
    assert db.node_info("alice")["props"] == {"age": 30}
    # Second removal is a no-op.
    assert db.remove_prop("alice", "team") is False
    db.close()


def test_set_prop_none_removes_and_retracts_rule_edge(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.create_rule(_fit_rule())
    db.insert_node("Person", "alice", {"team": "red"})
    db.insert_node("Person", "bob", {"team": "red"})
    assert db.query("MATCH (a:Person)-[:SAME_TEAM]->(b:Person) RETURN a") != []

    db.set_prop("bob", "team", None)
    assert "team" not in db.node_info("bob")["props"]
    assert db.query("MATCH (a:Person)-[:SAME_TEAM]->(b:Person) RETURN a") == []
    db.close()


def test_remove_prop_unknown_key_raises(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(RuntimeError):
        db.remove_prop("ghost", "team")
    db.close()


# ---------------------------------------------------------------------------
# 5. Predicate shape round-trip
# ---------------------------------------------------------------------------


def test_create_rule_accepts_snake_case_predicate(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    rule = _fit_rule("snake")
    rule["predicate"] = {"kind": "field_equal", "fields": ["team"]}
    db.create_rule(rule)
    db.insert_node("Person", "alice", {"team": "red"})
    db.insert_node("Person", "bob", {"team": "red"})
    assert db.query("MATCH (a:Person)-[:SAME_TEAM]->(b:Person) RETURN a") != []
    db.close()


def test_explain_predicate_round_trips_into_create_rule(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.create_rule(_fit_rule())
    db.insert_node("Person", "alice", {"team": "red"})
    db.insert_node("Person", "bob", {"team": "red"})

    why = db.explain("alice", "bob")
    predicate = why[0]["predicate"]
    assert predicate["kind"] == "field_equal"

    clone = _fit_rule("same_team_clone")
    clone["predicate"] = predicate
    clone["edge_type"] = "SAME_TEAM_CLONE"
    db.create_rule(clone)
    rows = db.query("MATCH (a:Person)-[:SAME_TEAM_CLONE]->(b:Person) RETURN a, b")
    assert ("alice", "bob") in {(r["a"], r["b"]) for r in rows}
    db.close()


def test_explain_output_shape_unchanged(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.create_rule(_fit_rule())
    db.insert_node("Person", "alice", {"team": "red"})
    db.insert_node("Person", "bob", {"team": "red"})
    e = db.explain("alice", "bob")[0]
    assert set(e) == {"rule", "edge_type", "src_key", "dst_key", "weight", "predicate"}
    assert set(e["predicate"]) == {"kind", "fields", "min", "tolerance", "km", "parts"}
    db.close()


def test_snake_case_round_trip_for_every_predicate_kind(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    kinds = [
        ({"kind": "key_match", "fields": ["fk"]}, {"KeyMatch": {"field": "fk"}}),
        ({"kind": "field_equal", "fields": ["f"]}, {"FieldEqual": {"field": "f"}}),
        (
            {"kind": "overlap", "fields": ["tags"], "min": 0.5},
            {"Overlap": {"field": "tags", "min": 0.5}},
        ),
        (
            {"kind": "numeric_within", "fields": ["yr"], "tolerance": 2.0},
            {"NumericWithin": {"field": "yr", "tolerance": 2.0}},
        ),
        (
            {"kind": "geo_radius", "fields": ["loc"], "km": 50.0},
            {"GeoRadius": {"field": "loc", "km": 50.0}},
        ),
        (
            {"kind": "vector_similar", "fields": ["emb"], "min": 0.9},
            {"VectorSimilar": {"field": "emb", "min": 0.9}},
        ),
        (
            {
                "kind": "all",
                "fields": ["f", "yr"],
                "parts": [
                    {"kind": "field_equal", "fields": ["f"]},
                    {"kind": "numeric_within", "fields": ["yr"], "tolerance": 2.0},
                ],
            },
            None,
        ),
        (
            {
                "kind": "any",
                "fields": ["f", "yr"],
                "parts": [
                    {"kind": "field_equal", "fields": ["f"]},
                    {"kind": "numeric_within", "fields": ["yr"], "tolerance": 2.0},
                ],
            },
            None,
        ),
    ]
    for i, (snake, _pascal) in enumerate(kinds):
        db.create_rule(
            {
                "name": f"r{i}",
                "src_label": "A",
                "dst_label": "B",
                "predicate": snake,
                "edge_type": f"E{i}",
                "weight_prop": None,
            }
        )
    db.close()


def test_create_rule_rejects_unknown_predicate_kind(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    rule = _fit_rule("bogus")
    rule["predicate"] = {"kind": "no_such_predicate", "fields": ["team"]}
    with pytest.raises(ValueError):
        db.create_rule(rule)
    db.close()


# ---------------------------------------------------------------------------
# 6. create_rule(if_not_exists=...)
# ---------------------------------------------------------------------------


def test_create_rule_if_not_exists(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    assert db.create_rule(_fit_rule(), if_not_exists=True) is True
    assert db.create_rule(_fit_rule(), if_not_exists=True) is False
    db.close()


def test_create_rule_duplicate_without_flag_raises(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    assert db.create_rule(_fit_rule()) is True
    with pytest.raises(RuntimeError):
        db.create_rule(_fit_rule())
    db.close()


# ---------------------------------------------------------------------------
# 7. Param shapes
# ---------------------------------------------------------------------------


def test_query_accepts_dict_and_tuple_params(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"name": "alice"})
    db.insert_node("Person", "bob", {"name": "bob"})

    q = "MATCH (n:Person) WHERE n.name = $name RETURN key(n) AS k"
    assert [r["k"] for r in db.query(q, {"name": "alice"})] == ["alice"]
    assert [r["k"] for r in db.query(q, [("name", "bob")])] == ["bob"]
    db.close()


def test_query_write_accepts_dict_and_tuple_params(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "alice", {"age": 30})
    db.insert_node("Person", "bob", {"age": 30})

    db.query_write(
        "MATCH (n:Person) WHERE key(n) = $k SET n.age = 31 RETURN key(n)",
        {"k": "alice"},
    )
    assert db.node_info("alice")["props"]["age"] == 31

    db.query_write(
        "MATCH (n:Person) WHERE key(n) = $k SET n.age = 32 RETURN key(n)",
        [("k", "bob")],
    )
    assert db.node_info("bob")["props"]["age"] == 32
    db.close()


def test_query_write_rejects_bad_param_shape(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(TypeError):
        db.query_write("MATCH (n:Person) SET n.age = 1 RETURN n", "nope")
    db.close()


# ---------------------------------------------------------------------------
# 8. Type stubs + docstrings
# ---------------------------------------------------------------------------

# Every method the binding exposes must carry a docstring and a text signature.
_DOCUMENTED = [
    "open",
    "insert_node",
    "upsert_node",
    "insert_edge",
    "insert_edge_upsert",
    "delete_edge",
    "delete_node",
    "set_prop",
    "remove_prop",
    "query",
    "query_with_params",
    "query_write",
    "query_at",
    "rename_node",
    "create_rule",
    "explain",
    "neighbors",
    "node_info",
    "node_edges",
    "node_history",
    "was_linked",
    "enable_index",
    "disable_index",
    "is_index_enabled",
    "has_vector_rule",
    "find_similar",
    "search_hybrid",
    "get_edge_prop",
    "ingest_batch",
    "batch_edges",
    "stats",
    "snapshot",
    "close",
]


@pytest.mark.parametrize("name", _DOCUMENTED)
def test_every_method_has_a_docstring(name):
    member = getattr(GraphDb, name)
    doc = inspect.getdoc(member)
    assert doc, f"GraphDb.{name} has no docstring"


@pytest.mark.parametrize("name", _DOCUMENTED)
def test_every_method_has_a_text_signature(name):
    member = getattr(GraphDb, name)
    assert getattr(member, "__text_signature__", None), (
        f"GraphDb.{name} has no __text_signature__"
    )


def _installed_stub():
    """The .pyi maturin packaged from `bindings/python/mushroomdb.pyi`.

    maturin renames a root-level `<module>.pyi` to `<module>/__init__.pyi`
    inside the wheel and drops a `py.typed` marker beside it.
    """
    import importlib.util
    import pathlib

    spec = importlib.util.find_spec("mushroomdb")
    assert spec is not None and spec.origin is not None
    root = pathlib.Path(spec.origin).parent
    return root, root / "__init__.pyi"


def test_type_stub_ships_with_the_package():
    root, stub = _installed_stub()
    assert stub.is_file(), f"type stub not installed under {root}"
    assert (root / "py.typed").is_file(), "py.typed marker not installed"


def test_type_stub_covers_every_public_method():
    _root, stub = _installed_stub()
    text = stub.read_text()
    for name in _DOCUMENTED:
        assert f"def {name}(" in text, f"{name} missing from the type stub"
