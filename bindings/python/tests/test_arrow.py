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
