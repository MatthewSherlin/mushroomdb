"""Cypher schema statements (0.10 TY-9) through the Python binding.

`query_write` runs `CREATE`/`DROP INDEX|CONSTRAINT`; `query` refuses them and
answers `SHOW …`; a scoped handle lists only the labels it can see.
"""

from __future__ import annotations

import pytest

import mushroomdb
from mushroomdb import GraphDb


def test_query_write_runs_ddl_and_query_shows_it(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.query_write("CREATE INDEX i FOR (n:P) ON (n.a)")
    db.query_write("CREATE CONSTRAINT c FOR (n:P) REQUIRE n.b IS UNIQUE")
    indexes = db.query("SHOW INDEXES")
    assert {"name": "i", "type": "range", "label": "P", "property": "a", "state": "online"} in indexes
    assert db.query("SHOW CONSTRAINTS") == [
        {"name": "c", "type": "unique", "label": "P", "property": "b"}
    ]
    db.query_write("DROP CONSTRAINT c")
    db.query_write("DROP INDEX i")
    assert db.query("SHOW CONSTRAINTS") == []
    db.close()


def test_query_refuses_a_schema_change(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(mushroomdb.QueryError) as e:
        db.query("CREATE INDEX i FOR (n:P) ON (n.a)")
    assert "query_write" in str(e.value)
    assert db.query("SHOW INDEXES") == []
    db.close()


def test_a_scoped_handle_shows_only_the_labels_it_can_see(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("P", "p1", {})
    db.insert_node("Q", "q1", {})
    db.query_write("CREATE INDEX i FOR (n:P) ON (n.a)")
    db.query_write("CREATE INDEX q FOR (n:Q) ON (n.a)")
    scoped = db.scoped(keys=["p1"])
    assert [r["name"] for r in scoped.query("SHOW INDEXES")] == ["i"]
    with pytest.raises(mushroomdb.MaskedReadOnly):
        scoped.query("DROP INDEX q")
    db.close()
