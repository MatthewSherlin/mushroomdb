"""`GraphDb.restore(src, dst)` — the backup round trip, tested (spec §5.10).

`restore` is the CLI's `--restore-from` seeding, exposed: it copies the newest
backup under `src` into `dst` **only when `dst` holds no store**, and refuses
rather than merging when it does. The copy is staged and opened before anything
is moved into place, so a backup that does not open leaves `dst` as it was.

The point of the file is `test_restore_plus_refresh_answers_identically`: the
spec calls restore "a tested round trip", and until this release nothing from
Python exercised it at all. It runs in the normal suite, not as an ignored
benchmark.
"""

from __future__ import annotations

import pytest

from mushroomdb import GraphDb, IoError

VECTORS = {
    "a": [1.0, 0.0, 0.0],
    "b": [0.94, 0.34, 0.0],
    "c": [0.0, 1.0, 0.0],
    "d": [0.0, 0.93, 0.36],
}


def _seed(path, key: str = "solo") -> None:
    """A one-node store at `path`, closed."""
    db = GraphDb.open(str(path))
    db.insert_node("Doc", key, {})
    db.snapshot()
    db.close()


def _build_source(path) -> None:
    """Nodes, hand-written edges, a derived rule and a vector index.

    Snapshotted, then written to again, so the restored copy has to replay a
    WAL tail on top of a snapshot — the two halves a restore can get wrong
    independently.
    """
    db = GraphDb.open(str(path))
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
    db.create_rule(
        {
            "name": "close_emb",
            "src_label": "Person",
            "dst_label": "Person",
            "predicate": {"VectorSimilar": {"field": "emb", "min": 0.5}},
            "edge_type": "SIMILAR",
            "weight_prop": "score",
            "approximate": True,
        }
    )
    db.enable_index("Person", "team")
    for key in ("a", "b", "c"):
        team = "red" if key in ("a", "b") else "blue"
        db.insert_node("Person", key, {"team": team, "emb": VECTORS[key], "rev": 1})
    db.insert_edge("LINKS", "a", "c")
    db.snapshot()

    # After the snapshot on purpose: these land in the WAL tail the restored
    # copy has to replay.
    db.insert_node("Person", "d", {"team": "blue", "emb": VECTORS["d"], "rev": 2})
    db.insert_edge("LINKS", "b", "d")
    db.set_prop("a", "rev", 3)
    db.close()


def _answers(db: GraphDb) -> dict:
    """Everything §5.10 says the two stores must agree on, in one dict."""
    stats = dict(db.stats())
    return {
        "nodes": db.query(
            "MATCH (n:Person) RETURN key(n) AS k, n.team AS t, n.rev AS rev ORDER BY k"
        ),
        "manual": db.query("MATCH (a)-[:LINKS]->(b) RETURN key(a) AS a, key(b) AS b ORDER BY a, b"),
        "derived": db.query(
            "MATCH (a)-[:SAME_TEAM]->(b) RETURN key(a) AS a, key(b) AS b ORDER BY a, b"
        ),
        "similar": db.query(
            "MATCH (a)-[:SIMILAR]->(b) RETURN key(a) AS a, key(b) AS b ORDER BY a, b"
        ),
        "find_similar": db.find_similar("emb", VECTORS["a"], k=10, min=0.0, exact=True),
        "degree": {k: db.degree(k) for k in ("a", "b", "c", "d")},
        "degree_links_out": {k: db.degree(k, edge_type="LINKS", direction="out") for k in "abcd"},
        "stats": stats,
        "history": {k: db.node_history(k) for k in ("a", "b", "c", "d")},
        "index": db.is_index_enabled("Person", "team"),
        "has_vector_rule": db.has_vector_rule("emb"),
        "commits": db.wal_total_commits(),
    }


def test_restore_plus_refresh_answers_identically(tmp_path):
    """The round trip the spec calls tested, tested.

    `stats()` carries no path, so the dicts compare whole: a restored store
    that differed anywhere — a dropped derived edge, an HNSW index that came
    back empty, a WAL tail that never replayed — shows up as a diff here.
    """
    src, dst = tmp_path / "src", tmp_path / "dst"
    _build_source(src)

    outcome = GraphDb.restore(str(src), str(dst))
    assert outcome["outcome"] == "restored"
    assert "snapshot.bin" in outcome["files"] and "wal.bin" in outcome["files"]
    assert outcome["bytes"] > 0

    original = GraphDb.open(str(src))
    restored = GraphDb.open(str(dst))
    assert restored.refresh() == 0, "a fresh open is already current"

    before = _answers(original)
    after = _answers(restored)

    # The fixture has something to lose: without this the comparison could pass
    # on two empty stores.
    assert before["nodes"], "fixture: there are nodes"
    assert before["derived"], "fixture: the rule fired"
    assert before["find_similar"], "fixture: the vector index answers"
    assert before["history"]["d"]["history"], "fixture: the WAL tail carries history"

    for name in sorted(before):
        assert after[name] == before[name], f"{name} differs after restore"

    original.close()
    restored.close()


def test_restore_refuses_a_non_empty_destination(tmp_path):
    """Refuses, rather than merging the backup into what is already there."""
    src, dst = tmp_path / "src", tmp_path / "dst"
    _seed(src, "from_backup")
    _seed(dst, "already_here")

    outcome = GraphDb.restore(str(src), str(dst))
    assert outcome["outcome"] == "already_present"
    assert outcome["files"] == [] and outcome["bytes"] == 0

    db = GraphDb.open(str(dst))
    assert db.node_info("already_here") is not None, "the existing store survives"
    assert db.node_info("from_backup") is None, "nothing was merged in"
    db.close()


def test_restore_reports_a_source_holding_no_backup(tmp_path):
    """An empty backup volume on a first boot is a report, not a failure."""
    src, dst = tmp_path / "src", tmp_path / "dst"
    src.mkdir()
    assert GraphDb.restore(str(src), str(dst))["outcome"] == "empty"
    # A source that does not exist at all answers the same way.
    assert GraphDb.restore(str(src / "nope"), str(dst))["outcome"] == "empty"


def test_restore_picks_the_newest_backup_under_a_vault(tmp_path):
    """`src` may be a directory of backups; `latest` wins outright.

    Written the awkward way round on purpose: `latest` is seeded **first**, so
    it is the older of the two by mtime. A restore that ranked on mtime alone
    would take the dated directory, and the assertions below say which one a
    rolling copy naming itself `latest` must get.
    """
    vault, dst = tmp_path / "vault", tmp_path / "dst"
    _seed(vault / "latest", "newest")
    _seed(vault / "2026-09-01T00-00Z", "old")

    assert GraphDb.restore(str(vault), str(dst))["outcome"] == "restored"
    db = GraphDb.open(str(dst))
    assert db.node_info("newest") is not None
    assert db.node_info("old") is None
    db.close()


def test_restore_raises_when_the_backup_does_not_open(tmp_path):
    """A copy that does not open is a hard failure, not a silent empty start.

    The staged copy is opened before anything moves, so `dst` is left as it was
    found and the next attempt restores rather than seeing a half-written store.
    """
    src, dst = tmp_path / "src", tmp_path / "dst"
    _seed(src, "a")
    snap = src / "snapshot.bin"
    snap.write_bytes(snap.read_bytes()[: snap.stat().st_size // 2])

    with pytest.raises(IoError) as err:
        GraphDb.restore(str(src), str(dst))
    assert str(src) in str(err.value) and str(dst) in str(err.value)
    assert err.value.code == "io"
    assert list(dst.iterdir()) == [], "a failed restore leaves nothing behind"


def test_restore_is_a_staticmethod(tmp_path):
    """Callable off the class without a handle — the sidecar's boot shape."""
    src, dst = tmp_path / "src", tmp_path / "dst"
    _seed(src, "a")
    assert GraphDb.restore(str(src), str(dst))["outcome"] == "restored"
    db = GraphDb.open(str(dst))
    assert db.node_info("a") is not None
    db.close()
