"""Listing, deleting, rebuilding and proposing rules.

`create_rule` was the whole of the rule surface before 0.7: a rule could be
made from Python and never inspected, removed or rebuilt.
"""

from __future__ import annotations

import json

import pytest

from mushroomdb import GraphDb, RuleNotFound

_EDGES = "MATCH (a)-[:SAME_TEAM]->(b) RETURN key(a) AS a, key(b) AS b ORDER BY a, b"


def _rule() -> dict:
    return {
        "name": "same_team",
        "src_label": "Person",
        "dst_label": "Person",
        "predicate": {"FieldEqual": {"field": "team"}},
        "edge_type": "SAME_TEAM",
        "weight_prop": None,
    }


def _pairs(db) -> list[tuple[str, str]]:
    return [(r["a"], r["b"]) for r in db.query(_EDGES)]


def test_list_rebuild_delete_and_recreate_from_the_listing(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    assert db.rules() == []
    db.create_rule(_rule())
    db.insert_node("Person", "alice", {"team": "red"})
    db.insert_node("Person", "bob", {"team": "red"})
    assert _pairs(db) == [("alice", "bob"), ("bob", "alice")]

    listed = db.rules()
    assert [r["name"] for r in listed] == ["same_team"]
    assert listed[0]["predicate"] == {"FieldEqual": {"field": "team"}}
    assert listed[0]["edge_type"] == "SAME_TEAM"
    assert listed[0]["namespace"] is None, "a global rule"

    db.rebuild_rule("same_team")
    assert _pairs(db) == [("alice", "bob"), ("bob", "alice")], "a rebuild derives the same edges"

    db.delete_rule("same_team")
    assert db.rules() == []
    assert _pairs(db) == [], "its edges go with it"

    # What `rules()` lists, `create_rule` accepts.
    assert db.create_rule(listed[0]) is True
    assert _pairs(db) == [("alice", "bob"), ("bob", "alice")]
    db.close()


def test_an_unknown_rule_is_rule_not_found(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    for call in (db.delete_rule, db.rebuild_rule):
        with pytest.raises(RuleNotFound) as err:
            call("no_such_rule")
        assert err.value.name == "no_such_rule"
    db.close()


def test_suggest_rules_hides_bookkeeping_and_its_arguments_create_the_rule(tmp_path):
    """Thirty notes sharing `kind` and `source`, twelve people sharing `team`.

    The engine proposes clique rules over the notes' bookkeeping fields; the
    binding returns what the MCP tool returns — those dropped, and each
    survivor with arguments `create_rule` takes unchanged.
    """
    db = GraphDb.open(str(tmp_path / "db"))
    for i in range(30):
        db.insert_node(
            "Note",
            f"note:{i:02d}",
            {
                "text": f"note {i} about the release",
                "kind": ["note", "decision", "todo"][i % 3],
                "source": ["session-a", "session-b"][i % 2],
                "ts": 1_759_000_000 + i,
            },
        )
    for i in range(12):
        db.insert_node(
            "Person", f"p{i}", {"name": f"Person {i}", "team": ["infra", "ui"][i % 2]}
        )

    report = db.suggest_rules()
    assert set(report) == {"suggestions", "total", "bookkeeping_hidden", "truncated"}
    assert report["bookkeeping_hidden"] > 0, report
    assert report["total"] == len(report["suggestions"]), "the binding does not cap the list"

    over_team = [
        s for s in report["suggestions"] if '"field": "team"' in json.dumps(s["create_rule_args"])
    ]
    assert over_team, f"the one real pattern survives: {report}"
    for s in report["suggestions"]:
        text = json.dumps(s["create_rule_args"])
        for field in ("ns", "kind", "ts", "source", "provisional", "id", "aliases", "alias_keys"):
            assert f'"field": "{field}"' not in text, s

    args = over_team[0]["create_rule_args"]
    assert args["weight_prop"] == "weight", "explicit, so MCP and Python create the same rule"
    assert db.rules() == [], "a suggestion creates nothing"
    assert db.create_rule(args) is True
    created = db.rules()[0]
    assert created["name"] == args["name"]
    assert created["weight_prop"] == "weight"
    edge = args["edge_type"]
    rows = db.query(f"MATCH (a:Person)-[:{edge}]->(b:Person) RETURN count(a) AS c")
    assert rows[0]["c"] > 0, "and the rule it created derives edges"
    db.close()
