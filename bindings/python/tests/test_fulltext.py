"""Full-text declarations and search.

`enable_fulltext` is not idempotent in the engine, which is hostile to a
caller that declares its schema at boot, so the binding takes
`if_not_exists=` as `create_rule` does.
"""

from __future__ import annotations

import pytest

from mushroomdb import GraphDb, RuleInvalid, RuleNotFound


def test_enable_search_disable(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    assert db.fulltext_pairs() == []
    assert db.is_fulltext_enabled("Doc", "body") is False

    assert db.enable_fulltext("Doc", "body") is True
    assert db.is_fulltext_enabled("Doc", "body") is True
    assert db.fulltext_pairs() == [("Doc", "body")]

    db.insert_node("Doc", "d1", {"body": "alpha beta"})
    db.insert_node("Doc", "d2", {"body": "alpha gamma"})

    assert {k for k, _ in db.search("body", "alpha")} == {"d1", "d2"}
    assert [k for k, _ in db.search("body", "beta")] == ["d1"]
    assert db.search("body", "delta") == []
    assert len(db.search("body", "alpha", k=1)) == 1
    key, score = db.search("body", "beta")[0]
    assert isinstance(score, float) and score > 0

    db.disable_fulltext("Doc", "body")
    assert db.is_fulltext_enabled("Doc", "body") is False
    assert db.search("body", "alpha") == []
    db.close()


def test_enabling_twice_raises_unless_asked_not_to(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.enable_fulltext("Doc", "body")
    with pytest.raises(RuleInvalid):
        db.enable_fulltext("Doc", "body")
    assert db.enable_fulltext("Doc", "body", if_not_exists=True) is False
    assert db.enable_fulltext("Doc", "title", if_not_exists=True) is True
    assert db.fulltext_pairs() == [("Doc", "body"), ("Doc", "title")]
    db.close()


def test_disabling_a_pair_that_is_not_enabled_names_it(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    with pytest.raises(RuleNotFound) as err:
        db.disable_fulltext("Doc", "body")
    assert err.value.name == "fulltext(Doc,body)"
    db.close()
