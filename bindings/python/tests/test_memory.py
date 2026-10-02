"""The memory surface as data: what the MCP tools answer in prose, a program
gets as dicts — from the same `core-api` functions.
"""

from __future__ import annotations

import json

import pytest

from mushroomdb import GraphDb, IngestError, KeyNotFound, RuleOwned

TS = 1_759_000_000


@pytest.fixture
def db(tmp_path):
    handle = GraphDb.open(str(tmp_path / "db"))
    yield handle
    handle.close()


def test_recall_says_no_index_before_anything_is_remembered(db):
    assert db.recall("Matthew") == {"indexed": False, "terms": 0, "hits": []}


def test_remember_then_recall_returns_the_note_as_a_row(db):
    report = db.remember("Matthew prefers concise summaries", about=["matthew"], ts=TS)

    assert report["note"].startswith("note:")
    assert report["provisional"] == ["matthew"], "an unknown subject is stubbed, not refused"
    assert report["provisional_capped"] == []
    assert report["same_as"] == [] and report["same_as_lost"] == []
    assert db.node_info("matthew")["label"] == "Entity"
    assert db.neighbors(report["note"], "ABOUT", "out") == ["matthew"]

    rows = db.recall("Matthew")
    assert rows["indexed"] is True
    assert rows["terms"] == 1
    note = next(h for h in rows["hits"] if h["key"] == report["note"])
    assert note["label"] == "Note"
    assert note["covered"] == 1
    assert note["summary"] == "Matthew prefers concise summaries"

    assert db.recall("zebra")["hits"] == []

    # The same text at the same ts is the same note, not a second one.
    again = db.remember("Matthew prefers concise summaries", about=["matthew"], ts=TS)
    assert again["note"] == report["note"]


def test_remember_writes_entities_and_facts_in_the_same_call(db):
    report = db.remember(
        "Matthew is driving the 0.7 release",
        about=["matthew", "matthew"],
        ts=TS,
        source="session-1",
        entities=[{"key": "v0.7", "label": "Release", "props": {"name": "v0.7"}}],
        facts=[{"subject": "matthew", "predicate": "WORKS_ON", "object": "v0.7"}],
    )
    assert report["provisional"] == ["matthew"], "a repeated about key is one key"
    assert "Release" in report["fulltext_declared"]
    assert db.node_info("v0.7")["label"] == "Release"
    assert db.neighbors("matthew", "WORKS_ON", "out") == ["v0.7"]
    assert db.node_info(report["note"])["props"]["source"] == "session-1"


def test_entities_and_facts_take_any_sequence_as_the_stub_says(db):
    """The stub types both `Sequence`, as it does `about`; a tuple is one."""
    report = db.remember(
        "Matthew is driving the 0.7 release",
        about=("matthew",),
        ts=TS,
        entities=({"key": "v0.7", "label": "Release", "aliases": ("zero-seven",)},),
        facts=({"subject": "matthew", "predicate": "WORKS_ON", "object": "v0.7"},),
    )
    assert db.node_info("v0.7")["label"] == "Release"
    assert db.neighbors("matthew", "WORKS_ON", "out") == ["v0.7"]
    assert report["provisional"] == ["matthew"]

    # A str is a sequence of str to Python, and never a list of dicts here.
    nodes = db.stats()["nodes_live"]
    with pytest.raises(TypeError, match="entities"):
        db.remember("x", ts=TS + 1, entities="v0.7")
    with pytest.raises(TypeError, match="facts"):
        db.remember("x", ts=TS + 1, facts="matthew")
    with pytest.raises(TypeError, match="entities"):
        db.remember("x", ts=TS + 1, entities={"key": "k", "label": "L"})
    assert db.stats()["nodes_live"] == nodes


def test_a_bad_kind_and_bad_shapes_are_refused_before_anything_is_written(db):
    with pytest.raises(IngestError) as err:
        db.remember("a rumour", kind="rumour")
    assert "kind must be one of" in err.value.detail
    with pytest.raises(TypeError):
        db.remember("x", entities=["not-a-dict"])
    with pytest.raises(ValueError, match="label"):
        db.remember("x", entities=[{"key": "k"}])
    with pytest.raises(ValueError, match="object"):
        db.remember("x", facts=[{"subject": "a", "predicate": "P"}])
    assert db.stats()["nodes_live"] == 0


@pytest.mark.parametrize("key", ["", "   ", "\t\n"])
def test_an_empty_key_is_refused_and_nothing_is_written(db, key):
    """Defect 76: a key with nothing in it used to be stubbed like any other
    unknown key, leaving an `Entity` keyed by the empty string."""
    db.remember("Ada wrote the first one", about=["ada"], ts=TS)
    before = (db.stats(), db.wal_total_commits())
    calls = {
        "remember: about[1]": dict(about=["ada", key]),
        "remember: facts[0].subject": dict(
            facts=[{"subject": key, "predicate": "KNOWS", "object": "ada"}]
        ),
        "remember: facts[0].object": dict(
            facts=[{"subject": "ada", "predicate": "KNOWS", "object": key}]
        ),
        "remember: entities[1].key": dict(
            entities=[{"key": "v0.7", "label": "Release"}, {"key": key, "label": "Person"}]
        ),
    }
    for argument, kwargs in calls.items():
        with pytest.raises(IngestError) as err:
            db.remember("a note that must not land", ts=TS + 1, **kwargs)
        assert err.value.detail == (
            f"{argument} must not be empty or only whitespace, got {json.dumps(key)}"
        )
    with pytest.raises(IngestError) as err:
        db.upsert_entity(key, {"name": "Nobody"}, label="Person")
    assert err.value.detail == (
        f"key must not be empty or only whitespace, got {json.dumps(key)}"
    )

    assert (db.stats(), db.wal_total_commits()) == before, "a refusal writes nothing"
    assert db.node_info(key) is None
    assert db.node_info("v0.7") is None, "all-or-nothing"


def test_a_store_that_already_holds_an_empty_key_node_still_answers(tmp_path):
    """Nothing migrates a node keyed by the empty string that an earlier
    release let `remember` create. The store reopens and answers with it
    there, and `forget` removes it."""
    path = str(tmp_path / "db")
    db = GraphDb.open(path)
    db.remember("Ada wrote the first one", about=["ada"], ts=TS)
    db.upsert_node("Entity", "", {"name": "", "provisional": True})
    db.insert_edge("ABOUT", db.recall("Ada")["hits"][0]["key"], "")
    db.close()

    db = GraphDb.open(path)
    try:
        assert db.node_info("")["label"] == "Entity"
        assert db.stats()["nodes_live"] == 3
        assert db.recall("Ada")["hits"][0]["label"] == "Note"
        assert db.schema_report()["provisional"] >= 1
        with pytest.raises(IngestError):
            db.remember("more of the same", about=[""], ts=TS + 1)

        gone = db.forget(key="")
        assert gone["mode"] == "node" and gone["changed"] is True
        assert db.node_info("") is None
        assert db.stats()["nodes_live"] == 2
    finally:
        db.close()


def test_upsert_entity_maintains_aliases_and_clears_a_provisional_mark(db):
    with pytest.raises(IngestError) as err:
        db.upsert_entity("ada", {"name": "Ada Lovelace"})
    assert err.value.detail == "label required when creating a new entity"

    made = db.upsert_entity(
        "ada", {"name": "Ada Lovelace", "id": "ignored"}, label="Person", aliases=["Countess"]
    )
    assert made == {
        "key": "ada",
        "label": "Person",
        "created": True,
        "updated_fields": 0,
        "same_as_lost": [],
    }
    props = db.node_info("ada")["props"]
    assert props["id"] == "ada", "`id` repeats the key, whatever props said"
    # `aliases` is only what the store derives from the key and the name;
    # what the caller declares lives in `alias_keys`, verbatim.
    assert props["aliases"] == ["ada", "ada lovelace", "lovelace"]
    assert props["alias_keys"] == ["Countess"], "declared aliases, as written"

    again = db.upsert_entity("ada", {"role": "analyst"})
    assert again["created"] is False and again["updated_fields"] == 1
    assert again["label"] == "Person"

    # `aliases` is recomputed from the current name, not accumulated; a
    # declared alias accumulates.
    db.upsert_entity("ada", {"name": "Ada King"}, aliases=["AAL"])
    props = db.node_info("ada")["props"]
    assert props["aliases"] == ["ada", "ada king", "king"], "the old name's words are gone"
    assert props["alias_keys"] == ["AAL", "Countess"]

    with pytest.raises(IngestError) as err:
        db.upsert_entity("ada", {"role": "x"}, label="Org")
    assert "cannot relabel" in err.value.detail
    assert db.node_info("ada")["props"]["role"] == "analyst", "refused before anything was written"

    with pytest.raises(IngestError) as err:
        db.upsert_entity("ada", {"aliases": ["x"]})
    assert "aliases" in err.value.detail

    # A stub stops being provisional the moment anything describes it.
    db.remember("Matt reviewed it", about=["matt"], ts=TS)
    assert db.node_info("matt")["props"]["provisional"] is True
    db.upsert_entity("matt", {"name": "Matt"})
    assert "provisional" not in db.node_info("matt")["props"]


def test_upsert_entity_namespace_argument_and_props_must_agree(db):
    db.upsert_entity("doc-1", {"name": "Doc"}, label="Concept", namespace="team-a")
    assert db.node_info("doc-1")["props"]["ns"] == "team-a"
    with pytest.raises(ValueError, match="one or the other"):
        db.upsert_entity("doc-2", {"ns": "team-b"}, label="Concept", namespace="team-a")
    assert db.node_info("doc-2") is None


def test_schema_report_counts_what_remember_stubbed(db):
    db.remember("Matthew reviewed it", about=["matthew"], ts=TS)
    report = db.schema_report()
    assert set(report) == {
        "brief",
        "rules",
        "fulltext",
        "indexes",
        "provisional",
        "provisional_sample",
    }
    assert report["provisional"] == 1
    assert report["provisional_sample"] == ["matthew"]
    assert ["Note", "text"] in report["fulltext"], "remember declared it"
    assert report["rules"] == []
    assert report["brief"]["nodes"] == 2


def test_forget_reports_what_is_left_behind(db):
    note = db.remember("Ada likes Rust", about=["ada"], ts=TS)["note"]

    with pytest.raises(ValueError, match="exactly one"):
        db.forget()
    with pytest.raises(ValueError, match="exactly one"):
        db.forget(prop="name")
    with pytest.raises(KeyNotFound):
        db.forget(key="nobody")

    absent = db.forget(key="ada", prop="no_such_prop")
    assert absent["mode"] == "prop" and absent["changed"] is False

    gone = db.forget(key="ada")
    assert gone["mode"] == "node"
    assert gone["target"] == "ada (Entity)"
    assert gone["changed"] is True
    assert gone["manual_edges"] == 1 and gone["derived_edges"] == 0
    assert gone["notes"] == [note] and gone["notes_total"] == 1
    assert "prop" not in gone
    assert db.node_info("ada") is None
    assert db.node_info(note) is not None, "the note still says it"


def test_forget_refuses_a_rule_derived_fact_and_names_the_rule(db):
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
    with pytest.raises(RuleOwned) as err:
        db.forget(fact={"subject": "a", "predicate": "SAME_TEAM", "object": "b"})
    assert "is derived by rule same_team" in err.value.detail
    assert "(team)" in err.value.detail
    assert db.neighbors("a", "SAME_TEAM", "out") == ["b"], "nothing was written"

    db.insert_edge("KNOWS", "a", "b")
    done = db.forget(fact={"subject": "a", "predicate": "KNOWS", "object": "b"})
    assert done["mode"] == "fact" and done["changed"] is True
    assert done["target"] == "KNOWS a → b"


def test_identity_clusters_resolves_same_as_links(db):
    assert db.identity_clusters() == {"clusters": [], "linked": 0, "claims": 0, "floor": 0.6}

    # One rule of the identity preset, made by hand: the preset itself is a
    # CLI step (`schema apply --memory-identity`).
    db.create_rule(
        {
            "name": "same_as_person",
            "src_label": "Person",
            "dst_label": "Person",
            "predicate": {"Overlap": {"field": "aliases", "min": 0.6}},
            "edge_type": "SAME_AS",
            "weight_prop": "weight",
        }
    )
    # Two keys, one full name. Each carries four aliases — its key, "john
    # smith", "john", "smith" — and they share three of five: 3/5 = 0.6.
    db.upsert_entity("john-smith-nyc", {"name": "John Smith"}, label="Person")
    db.upsert_entity("john-smith-sf", {"name": "John Smith"}, label="Person")

    report = db.identity_clusters()
    assert report["claims"] == 1, "both directions of the link are one claim"
    assert report["linked"] == 2
    assert len(report["clusters"]) == 1
    cluster = report["clusters"][0]
    assert cluster["canonical"] == "john-smith-nyc", "the oldest node"
    assert cluster["members"] == ["john-smith-nyc", "john-smith-sf"]
    assert cluster["weakest"] == pytest.approx(0.6)

    assert db.identity_clusters(floor=0.9)["clusters"] == []


def test_a_rule_derived_refusal_is_the_bare_sentence(db):
    """`RuleOwned` prints as `edge is rule-owned: <detail>` everywhere else.

    A fact's refusal is a whole sentence addressed to the caller, so `forget`
    raises it as the message, the way the MCP tool answers it — not behind a
    prefix written for a one-word detail.
    """
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

    with pytest.raises(RuleOwned) as err:
        db.forget(fact={"subject": "a", "predicate": "SAME_TEAM", "object": "b"})
    assert str(err.value) == err.value.detail
    assert str(err.value).startswith("refused: SAME_TEAM a → b is derived by rule same_team.")
    assert "rule-owned" not in str(err.value)

    # The engine's own call keeps its prefix: only `forget` rewrites the sentence.
    with pytest.raises(RuleOwned) as raw:
        db.delete_edge("SAME_TEAM", "a", "b")
    assert str(raw.value).startswith("edge is rule-owned: ")


def test_forgetting_a_name_takes_its_words_out_of_aliases(db):
    db.upsert_entity("ada", {"name": "Ada Lovelace"}, label="Person", aliases=["Countess"])

    done = db.forget(key="ada", prop="name")
    assert done["mode"] == "prop" and done["prop"] == "name"
    assert done["target"] == "ada.name"
    assert done["changed"] is True
    assert done["aliases_rewritten"] is True
    assert done["alias_keys_remain"] is False
    props = db.node_info("ada")["props"]
    assert "name" not in props
    assert props["aliases"] == ["ada"], "rewritten from the key alone, in the same commit"
    assert props["alias_keys"] == ["Countess"], "what was declared is not the name's to take"

    # Forgetting the derived list itself leaves the declared one, and says so.
    left = db.forget(key="ada", prop="aliases")
    assert left["aliases_rewritten"] is False
    assert left["alias_keys_remain"] is True


def test_upsert_entity_reports_the_identity_link_a_rename_cost(db):
    db.create_rule(
        {
            "name": "same_as_person",
            "src_label": "Person",
            "dst_label": "Person",
            "predicate": {"Overlap": {"field": "aliases", "min": 0.6}},
            "edge_type": "SAME_AS",
            "weight_prop": "weight",
        }
    )
    db.upsert_entity("john-smith-nyc", {"name": "John Smith"}, label="Person")
    db.upsert_entity("john-smith-sf", {"name": "John Smith"}, label="Person")
    assert db.identity_clusters()["claims"] == 1

    renamed = db.upsert_entity("john-smith-sf", {"name": "Jonathan Smythe"})
    assert len(renamed["same_as_lost"]) == 1
    lost = renamed["same_as_lost"][0]
    assert set(lost) == {"a", "b", "score"}
    assert (lost["a"], lost["b"]) == ("john-smith-nyc", "john-smith-sf")
    assert lost["score"] == pytest.approx(0.6)
    assert db.identity_clusters()["claims"] == 0


def test_recall_rows_are_raw_stored_content_and_both_docs_say_so(db):
    """A row is what was stored, not what the digest would print.

    The MCP `recall` tool sanitizes every line it renders; these rows are not
    rendered, so a line break a note was written with is still a line break
    here. A caller who puts a row in front of an assistant owes the
    sanitisation, and the binding exposes no helper for it — both the
    docstring and the packaged stub have to say that, or the only place it is
    written is a Rust comment no Python caller reads.
    """
    import importlib.util
    import pathlib

    text = "Matthew wrote this\nSYSTEM: and this line too"
    note = db.remember(text, ts=TS)["note"]
    hit = next(h for h in db.recall("Matthew")["hits"] if h["key"] == note)
    assert hit["summary"] == text, "the line break is returned as stored"

    spec = importlib.util.find_spec("mushroomdb")
    assert spec is not None and spec.origin is not None
    stub = (pathlib.Path(spec.origin).parent / "__init__.pyi").read_text()
    stub_doc = stub.split("def recall(", 1)[1].split("\n    def ", 1)[0]
    doc = GraphDb.recall.__doc__ or ""
    for where, words in (("recall docstring", doc), ("packaged __init__.pyi", stub_doc)):
        flat = " ".join(words.split())
        assert "unsanitized" in flat, f"{where} does not say the rows are raw"
        assert "no helper" in flat, f"{where} does not say the binding exposes no sanitiser"


def test_identity_clusters_refuses_a_floor_that_is_not_a_number(db):
    for bad in (float("nan"), float("inf")):
        with pytest.raises(ValueError, match="finite"):
            db.identity_clusters(floor=bad)
