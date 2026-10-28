"""`db.memory.*`: the agent-memory methods under their own namespace, with the
old top-level names kept as deprecated aliases.
"""

from __future__ import annotations

import gc
import inspect
import warnings

import pytest

from mushroomdb import GraphDb, IngestError, MushroomBusy

TS = 1_759_000_000

MEMORY_METHODS = (
    "remember",
    "recall",
    "upsert_entity",
    "forget",
    "schema_report",
    "identity_clusters",
)


@pytest.fixture
def db(tmp_path):
    handle = GraphDb.open(str(tmp_path / "g"))
    yield handle
    handle.close()


def test_memory_accessor_exists(db):
    assert hasattr(db, "memory")


def test_memory_remember_succeeds_without_a_warning(db):
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        report = db.memory.remember("Alice is a researcher", about=["alice"], ts=TS)
    assert report["note"].startswith("note:")


@pytest.mark.parametrize("name", MEMORY_METHODS)
def test_accessor_method_has_a_docstring_and_a_text_signature(db, name):
    member = getattr(type(db.memory), name)
    assert inspect.getdoc(member), f"memory.{name} has no docstring"
    assert getattr(member, "__text_signature__", None), f"memory.{name} has no text signature"


@pytest.mark.parametrize("name", MEMORY_METHODS)
def test_accessor_docstring_matches_the_old_method(db, name):
    old = inspect.getdoc(getattr(GraphDb, name))
    new = inspect.getdoc(getattr(type(db.memory), name))
    assert old.startswith("Deprecated"), f"{name}: the alias says it is deprecated"
    assert new and not new.startswith("Deprecated")
    assert f"memory.{name}" in old


def test_a_full_round_through_the_namespace(db):
    db.memory.remember(
        "Matthew drives the release",
        about=["matthew"],
        ts=TS,
        entities=[{"key": "v0.10", "label": "Release"}],
        facts=[{"subject": "matthew", "predicate": "WORKS_ON", "object": "v0.10"}],
    )
    hits = db.memory.recall("Matthew")
    assert hits["indexed"] is True and hits["hits"]
    out = db.memory.upsert_entity("ada", {"name": "Ada Lovelace"}, label="Person")
    assert out["created"] is True
    assert db.memory.schema_report()["brief"]["nodes"] >= 3
    assert db.memory.identity_clusters()["clusters"] == []
    forgotten = db.memory.forget(key="ada")
    assert forgotten["mode"] == "node" and forgotten["changed"] is True


def test_errors_are_the_same_through_the_namespace(db):
    with pytest.raises(IngestError):
        db.memory.remember("", about=["alice"])


def test_deprecated_remember_emits_warning(db):
    with pytest.warns(DeprecationWarning, match="memory.remember"):
        db.remember("Alice is a researcher", about=["alice"], ts=TS)


@pytest.mark.parametrize(
    "name, args, kwargs",
    [
        ("remember", ("Alice",), {"about": ["alice"], "ts": TS}),
        ("recall", ("Alice",), {}),
        ("upsert_entity", ("ada", {"name": "Ada"}), {"label": "Person"}),
        ("schema_report", (), {}),
        ("forget", (), {"key": "alice"}),
        ("identity_clusters", (), {}),
    ],
)
def test_every_deprecated_alias_warns_at_the_caller_and_matches(db, name, args, kwargs):
    # Seed so `forget`/`recall` have something to act on; the seed is the
    # new spelling and must not warn.
    db.memory.remember("Alice is here", about=["alice"], ts=TS)
    with pytest.warns(DeprecationWarning) as rec:
        old = getattr(db, name)(*args, **kwargs)
    assert rec[0].filename.endswith("test_memory_namespace.py"), rec[0].filename
    assert name in str(rec[0].message)
    if name in ("remember", "upsert_entity", "forget"):
        # a write already happened; just check the shape is the same
        assert isinstance(old, dict)
    else:
        assert old == getattr(db.memory, name)(*args, **kwargs)


def test_the_alias_returns_what_the_accessor_returns(db):
    with pytest.warns(DeprecationWarning):
        old = db.remember("Alice", about=["alice"], ts=TS)
    new = db.memory.remember("Alice", about=["alice"], ts=TS)
    assert set(old) == set(new)
    assert old["note"] == new["note"], "same text at the same ts is the same note"


def test_scoped_handle_memory_refuses_like_the_old_names(tmp_path):
    db = GraphDb.open(str(tmp_path / "g"))
    try:
        db.insert_node("Person", "a", {"ns": "t1"})
        s = db.scoped(keys=["a"])
        with pytest.raises(ValueError, match="scoped"):
            s.memory.recall("x")
        with pytest.raises(ValueError, match="scoped"):
            s.memory.schema_report()
        with pytest.raises(ValueError, match="scoped"):
            s.memory.identity_clusters()
        with pytest.raises(Exception) as err:
            s.memory.remember("probe", about=["a"], ts=1)
        assert type(err.value).__name__ == "ReadOnly"
        with pytest.raises(Exception) as err:
            s.memory.forget(key="a")
        assert type(err.value).__name__ == "ReadOnly"
    finally:
        db.close()


def test_dropping_the_handle_releases_the_store(tmp_path):
    """The accessor must not keep the store open: no reference cycle."""
    path = str(tmp_path / "g")
    db = GraphDb.open(path)
    db.memory.remember("Alice", about=["alice"], ts=TS)
    assert db.memory.recall("Alice")["hits"]
    del db
    gc.collect()
    reopened = GraphDb.open(path)
    try:
        assert reopened.memory.recall("Alice")["hits"]
    finally:
        reopened.close()


def test_the_accessor_follows_the_handle_it_came_from(db):
    memory = db.memory
    memory.remember("Alice", about=["alice"], ts=TS)
    assert db.node_info("alice") is not None
    db.close()
    with pytest.raises(Exception):
        memory.recall("Alice")


def test_a_kept_accessor_holds_the_store_open_until_it_is_dropped(tmp_path):
    path = str(tmp_path / "g")
    db = GraphDb.open(path)
    acc = db.memory
    assert db.memory is not acc, "each access is a new accessor"
    del db
    gc.collect()
    with pytest.raises(MushroomBusy):
        GraphDb.open(path)
    del acc
    gc.collect()
    GraphDb.open(path).close()
