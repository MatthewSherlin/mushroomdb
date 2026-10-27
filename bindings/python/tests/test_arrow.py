"""`to_arrow_tables`: the whole graph as two pyarrow Tables.

pyarrow is an optional extra, so the module skips without it, except under CI,
where a missing pyarrow, pandas or polars fails.
"""

from __future__ import annotations

import importlib
import datetime
import json
import os
import sys

import pytest

from mushroomdb import Duration, GraphDb



def _optional(name: str):
    """Import `name`, skipping when absent, except under CI, where absent is a failure."""
    if os.environ.get("CI"):
        return importlib.import_module(name)
    return pytest.importorskip(name)


pa = _optional("pyarrow")


@pytest.fixture
def db(tmp_path):
    handle = GraphDb.open(str(tmp_path / "db"))
    handle.insert_node("Person", "ada", {"age": 36, "name": "Ada"})
    handle.insert_node("Person", "bob", {"age": 41})
    handle.insert_edge("KNOWS", "ada", "bob")
    yield handle
    handle.close()


def test_to_arrow_tables_returns_nodes_and_edges(db):
    tables = db.to_arrow_tables()
    assert set(tables) == {"nodes", "edges"}
    assert isinstance(tables["nodes"], pa.Table)
    assert isinstance(tables["edges"], pa.Table)
    assert len(tables["nodes"]) == 2
    assert len(tables["edges"]) == 1


def test_schema_is_stable_and_rows_are_sorted(db):
    tables = db.to_arrow_tables()
    assert tables["nodes"].schema.names == ["key", "label", "properties"]
    assert tables["edges"].schema.names == [
        "source",
        "type",
        "target",
        "derived",
        "rule",
        "weight",
        "properties",
    ]
    nodes = tables["nodes"].to_pylist()
    assert [n["key"] for n in nodes] == ["ada", "bob"]
    assert nodes[0]["label"] == "Person"
    assert json.loads(nodes[0]["properties"]) == {"age": 36, "name": "Ada"}
    assert json.loads(nodes[1]["properties"]) == {"age": 41}
    assert tables["edges"].to_pylist() == [
        {
            "source": "ada",
            "type": "KNOWS",
            "target": "bob",
            "derived": False,
            "rule": None,
            "weight": None,
            "properties": "{}",
        }
    ]


def test_empty_store_gives_empty_tables_with_the_same_schema(tmp_path):
    with GraphDb.open(str(tmp_path / "empty")) as db:
        tables = db.to_arrow_tables()
    assert len(tables["nodes"]) == 0
    assert len(tables["edges"]) == 0
    assert tables["nodes"].schema.names == ["key", "label", "properties"]
    assert tables["edges"].schema.names == [
        "source", "type", "target", "derived", "rule", "weight", "properties"
    ]
    assert tables["edges"].schema.field("properties").type == pa.string()


def test_derived_edges_carry_rule_and_weight(tmp_path):
    with GraphDb.open(str(tmp_path / "db")) as db:
        db.create_rule(
            {
                "name": "close_score",
                "src_label": "Person",
                "dst_label": "Person",
                "predicate": {"NumericWithin": {"field": "score", "tolerance": 5.0}},
                "edge_type": "CLOSE_SCORE",
                "weight_prop": "score",
            }
        )
        db.insert_node("Person", "a", {"score": 10.0})
        db.insert_node("Person", "b", {"score": 9.0})
        edges = db.to_arrow_tables()["edges"].to_pylist()
        assert edges
        for edge in edges:
            actual = db.get_edge_prop("CLOSE_SCORE", edge["source"], edge["target"], "score")
            assert isinstance(actual, float)
            assert edge["derived"] and edge["rule"] == "close_score"
            assert edge["weight"] == actual
            assert json.loads(edge["properties"]) == {"score": actual}


def test_scoped_handle_is_refused(db):
    scoped = db.scoped(keys=["ada"])
    with pytest.raises(ValueError, match="to_arrow_tables"):
        scoped.to_arrow_tables()


def test_pandas_and_polars_conversions(db):
    _optional("pandas")
    pl = _optional("polars")
    nodes = db.to_arrow_tables()["nodes"]
    assert list(nodes.to_pandas()["key"]) == ["ada", "bob"]
    assert pl.from_arrow(nodes)["key"].to_list() == ["ada", "bob"]


def test_missing_pyarrow_names_the_install_command(db, monkeypatch):
    monkeypatch.setitem(sys.modules, "pyarrow", None)
    with pytest.raises(ImportError, match="pip install pyarrow"):
        db.to_arrow_tables()


def _typed_node(db):
    tz = datetime.timezone(datetime.timedelta(hours=5, minutes=30))
    db.insert_node(
        "T",
        "typed",
        {
            "big": 2**62 + 1,
            "float": 1.5,
            "nan": float("nan"),
            "inf": float("inf"),
            "ninf": float("-inf"),
            "flag": True,
            "text": "hi",
            "list": [1, "a", [2.5]],
            "map": {"k": [1]},
            "date": datetime.date(2024, 1, 15),
            "ldt": datetime.datetime(2024, 1, 15, 12, 34, 56, 789012),
            "dt": datetime.datetime(2024, 1, 15, 12, 34, 56, 789012, tzinfo=tz),
            "dur": Duration(14, 3, 4_000_000_000),
            "bin": b"\x00\xff",
        },
    )


def test_properties_is_valid_json_with_the_http_form_for_every_value_type(db):
    _typed_node(db)
    rows = {r["key"]: r for r in db.to_arrow_tables()["nodes"].to_pylist()}
    text = rows["typed"]["properties"]
    props = json.loads(text, parse_constant=lambda c: pytest.fail(f"non-JSON constant {c}"))
    assert props["big"] == 2**62 + 1
    assert props["float"] == 1.5
    assert props["nan"] is None and props["inf"] is None and props["ninf"] is None
    assert props["flag"] is True and props["text"] == "hi"
    assert props["list"] == [1, "a", [2.5]]
    assert props["map"] == {"k": [1]}
    assert props["date"] == {"$date": "2024-01-15"}
    assert props["ldt"] == {"$ldt": "2024-01-15T12:34:56.789012"}
    assert props["dt"] == {"$dt": "2024-01-15T12:34:56.789012+05:30"}
    assert props["dur"] == {"$dur": "P1Y2M3DT4S"}
    assert props["bin"] == {"$binary": "AP8="}
    assert list(props) == sorted(props)


def test_properties_column_decodes_in_polars(db):
    pl = _optional("polars")
    _typed_node(db)
    nodes = pl.from_arrow(db.to_arrow_tables()["nodes"])
    dtype = pl.Struct({"big": pl.Int64, "flag": pl.Boolean, "text": pl.String})
    typed = nodes.filter(pl.col("key") == "typed")
    decoded = typed.select(pl.col("properties").str.json_decode(dtype)).row(0)[0]
    assert decoded == {"big": 2**62 + 1, "flag": True, "text": "hi"}


def test_properties_agree_with_what_the_query_surface_holds(db):
    _typed_node(db)
    [row] = db.query("MATCH (n:T) RETURN n.big AS big")
    props = {r["key"]: json.loads(r["properties"]) for r in db.to_arrow_tables()["nodes"].to_pylist()}
    assert props["typed"]["big"] == row["big"]


def test_a_date_outside_pythons_range_does_not_abort_the_export(db):
    db.query_write("CREATE (n:D {id: 'far', v: date('+12000-01-01')})")
    rows = {r["key"]: r for r in db.to_arrow_tables()["nodes"].to_pylist()}
    assert json.loads(rows["far"]["properties"])["v"] == {"$date": "+012000-01-01"}
    assert set(rows) == {"ada", "bob", "far"}


def test_manual_edge_properties_use_the_http_value_forms(db):
    _typed_node(db)
    values = db.node_info("typed")["props"]
    values["nested"] = {"z": [0, float("nan"), {"b": b"\x00\xff"}], "a": True}
    fields = ", ".join(f"`{name}`: ${name}" for name in values)
    db.query_write(
        "CREATE (:Arrow {id: 'edge-a'})-[:TYPED {" + fields + "}]->(:Arrow {id: 'edge-b'})",
        values,
    )
    tables = db.to_arrow_tables()
    [edge] = [row for row in tables["edges"].to_pylist() if row["type"] == "TYPED"]
    # Pin actual data, not just the presence of a seventh schema field.
    props = json.loads(edge["properties"], parse_constant=lambda c: pytest.fail(f"non-JSON constant {c}"))
    node = next(row for row in tables["nodes"].to_pylist() if row["key"] == "typed")
    expected = json.loads(node["properties"])
    expected["nested"] = {"a": True, "z": [0, None, {"b": {"$binary": "AP8="}}]}
    assert props == expected
    assert props["big"] == 2**62 + 1
    assert props["date"] == {"$date": "2024-01-15"}
    assert props["nan"] is None and props["inf"] is None and props["ninf"] is None
    assert list(props) == sorted(props)
    assert list(props["nested"]) == ["a", "z"]
    assert edge["weight"] is None and edge["rule"] is None and not edge["derived"]


def test_manual_edge_properties_include_weight_and_insert_count(db):
    db.enable_multiplicity()
    db.query_write(
        "CREATE (:Arrow {id: 'count-a'})-[:COUNTED {w: 7, weight: 3.5}]->(:Arrow {id: 'count-b'})"
    )
    assert db.insert_edge("COUNTED", "count-a", "count-b") is False
    [edge] = [row for row in db.to_arrow_tables()["edges"].to_pylist() if row["type"] == "COUNTED"]
    assert db.get_edge_prop("COUNTED", "count-a", "count-b", "count") == 2
    assert json.loads(edge["properties"]) == {"count": 2, "w": 7, "weight": 3.5}
    # Metadata continues to describe a deriving rule, rather than a manual field named weight.
    assert edge["weight"] is None and edge["rule"] is None and not edge["derived"]


def test_edge_rows_keep_their_order_with_property_maps(db):
    db.insert_edge("Z", "bob", "ada")
    db.insert_edge("A", "bob", "ada")
    db.insert_edge("A", "ada", "bob")
    edges = db.to_arrow_tables()["edges"].to_pylist()
    assert [(e["type"], e["source"], e["target"]) for e in edges] == [
        ("A", "ada", "bob"), ("A", "bob", "ada"), ("KNOWS", "ada", "bob"), ("Z", "bob", "ada")
    ]
    assert all(e["properties"] == "{}" for e in edges)


@pytest.mark.parametrize("initial_snapshot", [False, True], ids=["wal", "snapshot"])
def test_edge_properties_follow_cold_reopen_update_remove_and_reinsert(tmp_path, initial_snapshot):
    path = str(tmp_path / "db")

    def properties(db):
        [edge] = db.to_arrow_tables()["edges"].to_pylist()
        assert (edge["type"], edge["source"], edge["target"]) == ("K", "a", "b")
        return json.loads(edge["properties"])

    with GraphDb.open(path) as db:
        db.query_write("CREATE (:P {id: 'a'})-[:K {keep: 1, stale: 2, nested: {v: 7}}]->(:P {id: 'b'})")
        assert properties(db) == {"keep": 1, "stale": 2, "nested": {"v": 7}}
        if initial_snapshot:
            db.snapshot()

    with GraphDb.open(path) as db:
        assert properties(db) == {"keep": 1, "stale": 2, "nested": {"v": 7}}
        db.query_write("MATCH (a)-[r:K]->(b) SET r.keep = 3")
        db.query_write("MATCH (a)-[r:K]->(b) REMOVE r.stale")
        assert properties(db) == {"keep": 3, "nested": {"v": 7}}

    with GraphDb.open(path) as db:
        assert properties(db) == {"keep": 3, "nested": {"v": 7}}
        assert db.delete_edge("K", "a", "b")
        assert db.to_arrow_tables()["edges"].num_rows == 0
        assert db.insert_edge("K", "a", "b")
        assert properties(db) == {}
        db.snapshot()

    with GraphDb.open(path) as db:
        assert properties(db) == {}
