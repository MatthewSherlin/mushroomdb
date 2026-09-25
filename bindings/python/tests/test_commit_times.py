"""The date surface, from Python.

The engine tests cover the map. These cover the binding's own contract: what
`edges_at` accepts, what it refuses, and that the two new exception classes are
importable and carry their fields.
"""

import mushroomdb
import pytest


def _store(tmp_path):
    db = mushroomdb.GraphDb.open(str(tmp_path / "s"))
    db.insert_node("N", "a", {})
    db.insert_node("N", "b", {})
    db.insert_edge("KNOWS", "a", "b")
    return db


def test_edges_at_accepts_a_date_string(tmp_path):
    db = _store(tmp_path)
    rows = db.edges_at("a", "2099-01-01T00:00:00Z")
    assert any(r["dst"] == "b" or r["src"] == "b" for r in rows), rows


def test_edges_at_still_accepts_a_frame_index(tmp_path):
    db = _store(tmp_path)
    by_index = db.edges_at("a", db.wal_total_commits() - 1)
    by_date = db.edges_at("a", "2099-01-01T00:00:00Z")
    assert by_index == by_date


def test_was_linked_accepts_a_date_string(tmp_path):
    db = _store(tmp_path)
    assert db.was_linked("a", "b", "KNOWS", "2099-01-01T00:00:00Z") is True


def test_resolve_date_returns_a_frame_index(tmp_path):
    db = _store(tmp_path)
    assert db.resolve_date("2099-01-01") == db.wal_total_commits() - 1


def test_commit_time_ms_is_recorded_for_every_frame(tmp_path):
    db = _store(tmp_path)
    for frame in range(db.wal_total_commits()):
        assert db.commit_time_ms(frame) is not None, f"frame {frame} has no time"


def test_a_bool_is_refused_rather_than_read_as_an_index(tmp_path):
    # Python's bool is an int, so True would otherwise mean frame 1.
    db = _store(tmp_path)
    with pytest.raises(ValueError):
        db.edges_at("a", True)


def test_an_unparseable_date_is_refused(tmp_path):
    db = _store(tmp_path)
    with pytest.raises(mushroomdb.QueryError) as e:
        db.edges_at("a", "last Tuesday")
    assert "RFC 3339" in str(e.value)


def test_a_date_before_the_floor_carries_its_fields(tmp_path):
    db = _store(tmp_path)
    with pytest.raises(mushroomdb.TimeBeforeFloor) as e:
        db.edges_at("a", "1971-01-01")
    assert e.value.floor_commit == 0
    assert isinstance(e.value.floor_ms, int)
    assert e.value.code == "time_before_floor"


def test_the_new_classes_are_mushroom_errors_and_runtime_errors():
    for cls in (mushroomdb.NoRecordedTime, mushroomdb.TimeBeforeFloor):
        assert issubclass(cls, mushroomdb.MushroomError)
        assert issubclass(cls, RuntimeError)
    assert mushroomdb.NoRecordedTime.code == "no_recorded_time"
    assert mushroomdb.TimeBeforeFloor.code == "time_before_floor"
