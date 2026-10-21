"""The `id` property mirrors the node's key (defect ledger row 128).

A write that gives a node an `id` other than its key, or removes `id`, is a
`QueryError` on every path. Restating the key is accepted, and `rename_node`
still moves a matching `id` with the key.
"""

from __future__ import annotations

import pytest

from mushroomdb import GraphDb, QueryError


@pytest.fixture
def db(tmp_path):
    handle = GraphDb.open(str(tmp_path / "db"))
    handle.insert_node("P", "alice", {"id": "alice", "a": 1})
    yield handle
    handle.close()


def _refused(excinfo) -> None:
    detail = str(excinfo.value)
    assert "key property `id`" in detail and "rename_node" in detail, detail


def _untouched(db) -> None:
    info = db.node_info("alice")
    assert info["props"]["id"] == "alice"
    assert info["props"]["a"] == 1


def test_set_prop_refuses_a_different_id_and_accepts_the_key(db):
    with pytest.raises(QueryError) as e:
        db.set_prop("alice", "id", "mallory")
    _refused(e)
    with pytest.raises(QueryError):
        db.set_prop("alice", "id", 7)
    _untouched(db)
    db.set_prop("alice", "id", "alice")


def test_set_props_many_refuses_a_different_id_and_applies_nothing(db):
    db.insert_node("P", "bob", {"id": "bob", "a": 1})
    with pytest.raises(QueryError) as e:
        db.set_props_many([("bob", {"a": 9}), ("alice", {"id": "mallory"})])
    _refused(e)
    _untouched(db)
    assert db.node_info("bob")["props"]["a"] == 1
    db.set_props_many([("alice", {"id": "alice", "a": 2})])
    assert db.node_info("alice")["props"]["a"] == 2


def test_remove_prop_of_id_is_refused(db):
    with pytest.raises(QueryError) as e:
        db.remove_prop("alice", "id")
    assert "key property `id`" in str(e.value)
    _untouched(db)


def test_insert_node_with_an_id_that_is_not_the_key_is_refused(db):
    with pytest.raises(QueryError) as e:
        db.insert_node("P", "carol", {"id": "other"})
    _refused(e)
    assert db.node_info("carol") is None
    db.insert_node("P", "carol", {"id": "carol"})


def test_cypher_set_and_remove_of_id_are_refused(db):
    for q in (
        "MATCH (n:P) WHERE n.id = 'alice' SET n.id = 'mallory'",
        "MATCH (n:P) WHERE n.id = 'alice' SET n += {id: 'mallory'}",
        "MATCH (n:P) WHERE n.id = 'alice' REMOVE n.id",
    ):
        with pytest.raises(QueryError) as e:
            db.query_write(q)
        assert "key property `id`" in str(e.value), q
    _untouched(db)


def test_rename_node_still_moves_a_matching_id(db):
    db.rename_node("alice", "alicia")
    assert db.node_info("alicia")["props"]["id"] == "alicia"
