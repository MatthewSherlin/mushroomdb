"""`to_arrow_tables`: the whole graph as two pyarrow Tables.

pyarrow is an optional extra, so the module skips without it, except under CI,
where a missing pyarrow, pandas or polars fails.
"""

from __future__ import annotations

import importlib
import json
import os
import sys

import pytest

from mushroomdb import GraphDb



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
        }
    ]


def test_empty_store_gives_empty_tables_with_the_same_schema(tmp_path):
    with GraphDb.open(str(tmp_path / "empty")) as db:
        tables = db.to_arrow_tables()
    assert len(tables["nodes"]) == 0
    assert len(tables["edges"]) == 0
    assert tables["nodes"].schema.names == ["key", "label", "properties"]


def test_derived_edges_carry_rule_and_weight(tmp_path):
    with GraphDb.open(str(tmp_path / "db")) as db:
        db.create_rule(
            {
                "name": "same_team",
                "src_label": "Person",
                "dst_label": "Person",
                "predicate": {"FieldEqual": {"field": "team"}},
                "edge_type": "SAME_TEAM",
                "weight_prop": None,
            }
        )
        db.insert_node("Person", "a", {"team": "red"})
        db.insert_node("Person", "b", {"team": "red"})
        edges = db.to_arrow_tables()["edges"].to_pylist()
    assert edges
    assert all(e["derived"] and e["rule"] == "same_team" for e in edges)


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
