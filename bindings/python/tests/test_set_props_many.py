"""`set_props_many`: many nodes' properties in one commit.

`wal_total_commits()` counts WAL frames. One call is one frame — plus one
more, of history markers, when a rule derives or retracts an edge because of
it. Each case is asserted separately, because a test that only changes a
property no rule watches cannot tell an implementation that fires rules from
one that does not.
"""

from __future__ import annotations

import pytest

from mushroomdb import GraphDb, KeyNotFound, NamespaceImmutable, ReadOnly

_SAME_TEAM = "MATCH (a)-[:SAME_TEAM]->(b) RETURN key(a) AS a, key(b) AS b ORDER BY a, b"


@pytest.fixture
def db(tmp_path):
    """`a` is on team red, `b` on team blue; a rule links people on one team."""
    handle = GraphDb.open(str(tmp_path / "db"))
    handle.create_rule(
        {
            "name": "same_team",
            "src_label": "Person",
            "dst_label": "Person",
            "predicate": {"FieldEqual": {"field": "team"}},
            "edge_type": "SAME_TEAM",
            "weight_prop": None,
        }
    )
    handle.insert_node("Person", "a", {"team": "red", "n": 1, "emb": [1.0, 0.0]})
    handle.insert_node("Person", "b", {"team": "blue", "n": 1, "emb": [0.0, 1.0]})
    yield handle
    handle.close()


def _pairs(db) -> list[tuple[str, str]]:
    return [(r["a"], r["b"]) for r in db.query(_SAME_TEAM)]


def test_one_call_is_one_commit_and_an_identical_repeat_is_none(db):
    before = db.wal_total_commits()
    out = db.set_props_many([("a", {"n": 2}), ("b", {"n": 2})])
    assert out == {"nodes": 2, "props_set": 2, "props_removed": 0}
    assert db.wal_total_commits() == before + 1, "two nodes, one frame"
    assert db.node_info("a")["props"]["n"] == 2
    assert db.node_info("b")["props"]["n"] == 2
    assert db.node_info("a")["props"]["emb"] == [1.0, 0.0]
    assert db.node_info("b")["props"]["emb"] == [0.0, 1.0]

    again = db.set_props_many([("a", {"n": 2}), ("b", {"n": 2})])
    assert again == {"nodes": 0, "props_set": 0, "props_removed": 0}
    assert db.wal_total_commits() == before + 1, "nothing changed, so nothing was written"


def test_rules_fire_in_the_same_commit_and_cost_one_marker_frame(db):
    assert _pairs(db) == []
    before = db.wal_total_commits()

    # `b` joins team red: the rule derives a↔b inside the commit that set it.
    db.set_props_many([("b", {"team": "red"})])
    assert _pairs(db) == [("a", "b"), ("b", "a")]
    assert db.explain("a", "b")[0]["rule"] == "same_team"
    assert db.wal_total_commits() == before + 2, (
        "one frame for the batch, one for the history markers of the edges it derived"
    )

    # A watched property that changes no edge costs the one frame only.
    before = db.wal_total_commits()
    db.set_props_many([("a", {"team": "red", "n": 9})])
    assert db.wal_total_commits() == before + 1
    assert _pairs(db) == [("a", "b"), ("b", "a")]

    # Leaving the team retracts, in the commit that changed it: two frames.
    before = db.wal_total_commits()
    db.set_props_many([("b", {"team": "blue"})])
    assert _pairs(db) == []
    assert db.wal_total_commits() == before + 2


def test_an_unknown_key_is_an_error_and_nothing_is_written(db):
    before = db.wal_total_commits()
    with pytest.raises(KeyNotFound) as err:
        db.set_props_many([("a", {"n": 5}), ("nobody", {"n": 5})])
    assert err.value.key == "nobody"
    assert db.wal_total_commits() == before
    assert db.node_info("a")["props"]["n"] == 1, "the row before the bad one was not applied"
    assert db.node_info("nobody") is None, "never created: existing nodes only"


def test_an_unknown_key_is_an_error_even_when_its_row_changes_nothing(db):
    """RF-2. The engine checks a key only when an operation names it; a row
    with an empty dict queues none, so the binding has to check for itself."""
    before = db.wal_total_commits()
    with pytest.raises(KeyNotFound):
        db.set_props_many([("nobody", {})])
    with pytest.raises(KeyNotFound):
        db.set_props_many([("a", {"n": 1}), ("nobody", {})])
    assert db.wal_total_commits() == before


def test_embeddings_and_unnamed_properties_are_untouched(db):
    db.set_props_many([("a", {"n": 3})])
    props = db.node_info("a")["props"]
    assert props["emb"] == [1.0, 0.0]
    assert props["team"] == "red"

    # Re-sending an identical embedding is not a change.
    before = db.wal_total_commits()
    assert db.set_props_many([("a", {"emb": [1.0, 0.0]})])["props_set"] == 0
    assert db.wal_total_commits() == before


def test_none_removes_and_an_absent_property_is_not_removed_twice(db):
    out = db.set_props_many([("a", {"n": None})])
    assert out == {"nodes": 1, "props_set": 0, "props_removed": 1}
    assert "n" not in db.node_info("a")["props"]
    before = db.wal_total_commits()
    assert db.set_props_many([("a", {"n": None})])["props_removed"] == 0
    assert db.wal_total_commits() == before


def test_an_int_over_an_equal_float_is_a_change(db):
    db.set_props_many([("a", {"n": 1.0})])
    assert isinstance(db.node_info("a")["props"]["n"], float)
    assert db.set_props_many([("a", {"n": 1})])["props_set"] == 1


def test_bad_shapes_are_refused_before_the_store_is_read(db):
    before = db.wal_total_commits()
    with pytest.raises(ValueError, match="twice"):
        db.set_props_many([("a", {"n": 2}), ("a", {"n": 3})])
    with pytest.raises(TypeError):
        db.set_props_many([("a", "not-a-dict")])
    with pytest.raises(TypeError):
        db.set_props_many([["a", {"n": 2}]])
    with pytest.raises(TypeError):
        db.set_props_many([("a", {"n": (1, 2)})])
    assert db.wal_total_commits() == before
    assert db.set_props_many([]) == {"nodes": 0, "props_set": 0, "props_removed": 0}


def test_rows_may_be_any_sequence_but_each_row_is_a_tuple(db):
    """The stub types `rows` as a `Sequence`; a tuple of rows is one."""
    out = db.set_props_many((("a", {"n": 2}), ("b", {"n": 2})))
    assert out == {"nodes": 2, "props_set": 2, "props_removed": 0}
    before = db.wal_total_commits()
    with pytest.raises(TypeError):
        db.set_props_many("ab")
    with pytest.raises(TypeError):
        db.set_props_many({"a": {"n": 3}})
    with pytest.raises(TypeError):
        db.set_props_many([("a", {"n": 3}, "extra")])
    with pytest.raises(TypeError):
        db.set_props_many([(1, {"n": 3})])
    with pytest.raises(TypeError):
        db.set_props_many([("a", {1: 3})])
    assert db.wal_total_commits() == before


def test_a_deleted_key_is_an_unknown_key(db):
    db.delete_node("b")
    before = db.wal_total_commits()
    with pytest.raises(KeyNotFound):
        db.set_props_many([("a", {"n": 5}), ("b", {"n": 5})])
    with pytest.raises(KeyNotFound):
        db.set_props_many([("b", {})])
    assert db.wal_total_commits() == before
    assert db.node_info("a")["props"]["n"] == 1
    assert db.node_info("b") is None, "not resurrected"


def test_an_engine_refusal_in_a_later_row_refuses_the_whole_call(db):
    """A namespace is set at insert. The row before the refused one is a
    change the engine would accept on its own, and it is not written."""
    before = db.wal_total_commits()
    with pytest.raises(NamespaceImmutable):
        db.set_props_many([("a", {"n": 5, "team": "blue"}), ("b", {"ns": "elsewhere"})])
    assert db.wal_total_commits() == before
    assert db.node_info("a")["props"]["n"] == 1
    assert _pairs(db) == [], "no rule fired on a row that was never written"


def test_a_thousand_nodes_are_one_frame(db):
    db.ingest_batch([{"key": f"k{i}", "label": "Doc", "props": {"v": 0}} for i in range(1000)])
    before = db.wal_total_commits()
    out = db.set_props_many([(f"k{i}", {"v": 1}) for i in range(1000)])
    assert out == {"nodes": 1000, "props_set": 1000, "props_removed": 0}
    assert db.wal_total_commits() == before + 1


def test_a_read_only_handle_refuses_every_call_as_set_prop_does(tmp_path):
    """Whatever the rows say: a call that would change nothing and a call that
    names an unknown key are refused the same way, before the store is read."""
    path = str(tmp_path / "db")
    writer = GraphDb.open(path)
    writer.insert_node("Person", "a", {"n": 1})
    writer.close()

    reader = GraphDb.open(path, read_only=True)
    with pytest.raises(ReadOnly) as by_one:
        reader.set_prop("a", "n", 1)
    for rows in ([("a", {"n": 2})], [("a", {"n": 1})], [("nobody", {"n": 1})], [("a", {})], []):
        with pytest.raises(ReadOnly) as err:
            reader.set_props_many(rows)
        assert str(err.value) == str(by_one.value), rows
    assert reader.node_info("a")["props"]["n"] == 1
    reader.close()


def test_it_is_a_raw_write_and_does_not_maintain_an_entitys_aliases(db):
    """A pin of today's behaviour, which `set_prop` shares: `aliases` is
    derived by `upsert_entity` and `remember`, not by a property write."""
    db.upsert_entity("ada", {"name": "Ada Lovelace"}, label="Person")
    assert db.node_info("ada")["props"]["aliases"] == ["ada", "ada lovelace", "lovelace"]

    db.set_props_many([("ada", {"name": "Ada King"})])
    props = db.node_info("ada")["props"]
    assert props["name"] == "Ada King"
    assert props["aliases"] == ["ada", "ada lovelace", "lovelace"], "stale: still the old name"

    # The next describing write recomputes them from the name as it stands.
    db.upsert_entity("ada", {})
    assert db.node_info("ada")["props"]["aliases"] == ["ada", "ada king", "king"]
