"""The four graph algorithms, and the name they must not take.

`degree()` and `degrees()` are the per-node adjacency degree and predate
these; the centrality algorithm is `degree_centrality()`. Adding it under
either existing name would change what a working call returns.
"""

from __future__ import annotations

import pytest

from mushroomdb import GraphDb


@pytest.fixture
def star(tmp_path):
    """`a`, `b`, `c` each link to `hub`; `x` links to `y`, apart from them."""
    db = GraphDb.open(str(tmp_path / "db"))
    for key in ("hub", "a", "b", "c", "x", "y"):
        db.insert_node("Person", key, {})
    for key in ("a", "b", "c"):
        db.insert_edge("LINKS", key, "hub")
    db.insert_edge("LINKS", "x", "y")
    yield db
    db.close()


def test_pagerank_ranks_the_hub_first_and_repeats_itself(star):
    report = star.pagerank()
    assert set(report) == {"scores", "converged"}
    assert isinstance(report["converged"], bool)
    scores = dict(report["scores"])
    assert set(scores) == {"hub", "a", "b", "c", "x", "y"}, "one row per live node"
    assert max(scores, key=scores.get) == "hub"
    # No wall-clock budget by default, so the same store gives the same answer.
    assert star.pagerank() == report


def test_connected_components_groups_what_is_linked(star):
    report = star.connected_components()
    assert report["truncated"] is False
    of = dict(report["components"])
    assert of["hub"] == of["a"] == of["b"] == of["c"]
    assert of["x"] == of["y"]
    assert of["hub"] != of["x"]


def test_degree_centrality_is_not_degree(star):
    report = star.degree_centrality()
    assert report["truncated"] is False
    assert report["scores"][0] == ("hub", 3), "sorted, highest first"
    assert dict(report["scores"])["a"] == 1
    inbound = dict(star.degree_centrality(direction="in")["scores"])
    assert inbound["hub"] == 3 and inbound["a"] == 0

    # The names that were already here still mean what they meant.
    assert star.degree("hub") == 3
    assert isinstance(star.degree("hub"), int)
    assert star.degrees(keys=["hub"]) == [("hub", 3)]


def test_communities_finds_two_triangles(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    for group in ("t", "u"):
        keys = [f"{group}{i}" for i in (1, 2, 3)]
        for key in keys:
            db.insert_node("Person", key, {})
        for a, b in ((0, 1), (1, 2), (0, 2)):
            db.insert_edge("LINKS", keys[a], keys[b])
    report = db.communities(edge_types=["LINKS"])
    assert report["truncated"] is False
    assert report["modularity"] > 0
    members = {frozenset(c["members"]) for c in report["communities"]}
    assert members == {frozenset({"t1", "t2", "t3"}), frozenset({"u1", "u2", "u3"})}
    for c in report["communities"]:
        assert set(c) == {"id", "members", "internal_weight", "cohesion"}
    db.close()


def test_a_bad_direction_is_a_value_error_before_the_store_is_read(star):
    with pytest.raises(ValueError, match="direction"):
        star.pagerank(direction="sideways")
