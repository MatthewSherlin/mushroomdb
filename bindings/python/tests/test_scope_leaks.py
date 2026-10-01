"""The leak surface of `db.scoped(...)` — **every** method reachable on a
scoped handle has a row here, read or write.

Most tests make the same assertion in the same way: the scoped answer mentions
no key outside the scope. `_mentions_hidden` walks whatever the method returns
— dict, list, tuple, string, at any depth — and fails if a hidden key appears
anywhere in it. A method that silently ignores the scope is the defect this
release exists to prevent, so this file is deliberately uniform and deliberately
separate from the per-method test modules.

Each read test also pins the *unscoped* answer, so a test can never pass because
the fixture had nothing to leak.

**An error is an output.** The write half of this file exists because defect #8
was a refusal, not a return value: `upsert_node` read the node through an
unscoped path *before* reaching the write refusal, so a hidden key raised
`ValueError` naming its label while an absent key raised `RuntimeError`. The
exception class alone was a one-call existence oracle. Every write therefore
asserts that a hidden key and an absent key produce the **same class and the
same message**, and `test_every_method_has_a_row` fails the suite if a method
is ever added without a row — a method missing from this file is a method
nobody checked.

Two rows read differently from the spec's table, and the difference matters:

- §5.3 says the history surfaces answer `KeyNotFound` for a hidden subject.
  `node_history`, `edge_history`, `edges_at` and `was_linked` do **not** raise
  for an *unknown* key — they answer empty / `False`. Raising for a hidden one
  would therefore say "this key exists but you may not see it", which is the
  existence oracle the contract's first sentence forbids. Hidden answers
  exactly as absent does, per method, and that is what is asserted below.
  (The spec was amended to match: defect #10.)
- `neighborhood` has no Python binding to scope; `neighbors` is its one-hop
  surface and carries the row.
"""

from __future__ import annotations

import inspect

import pytest

from mushroomdb import GraphDb

HIDDEN = ("hidden_x", "hidden_y")
VISIBLE = ("vis_a", "vis_b")
ABSENT = "no_such_key_at_all"


def _mentions_hidden(obj) -> list[str]:
    """Every hidden key named anywhere inside `obj`, at any depth."""
    found: list[str] = []

    def walk(o) -> None:
        if isinstance(o, str):
            found.extend(h for h in HIDDEN if h in o)
        elif isinstance(o, dict):
            for k, v in o.items():
                walk(k)
                walk(v)
        elif isinstance(o, (list, tuple, set)):
            for v in o:
                walk(v)

    walk(obj)
    return found


def _assert_clean(label: str, answer) -> None:
    leaked = _mentions_hidden(answer)
    assert not leaked, f"{label} leaked {sorted(set(leaked))}: {answer!r}"


@pytest.fixture
def store(tmp_path):
    """Four Person nodes — two visible, two not — with edges, text and vectors.

    `vis_a`, `vis_b` and `hidden_x` share `team="red"` and near-identical
    embeddings, so every derived edge, degree, similarity and explanation that
    names a visible node also has a hidden one to name. `hidden_y` sits in its
    own namespace so `stats()`'s roster has something to narrow.
    """
    db = GraphDb.open(str(tmp_path / "db"))
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
            "approximate": False,
        }
    )
    db.insert_node("Person", "vis_a", {"team": "red", "bio": "alpha one", "emb": [1.0, 0.0]})
    db.insert_node("Person", "vis_b", {"team": "red", "bio": "alpha two", "emb": [0.95, 0.05]})
    db.insert_node(
        "Person", "hidden_x", {"team": "red", "bio": "alpha three", "emb": [0.9, 0.1]}
    )
    db.insert_node(
        "Person",
        "hidden_y",
        {"team": "blue", "bio": "alpha four", "emb": [0.85, 0.15]},
        namespace="tenant-secret",
    )
    db.insert_edge("LINKS", "vis_a", "vis_b")
    db.insert_edge("LINKS", "vis_a", "hidden_x")
    db.insert_edge("LINKS", "vis_b", "hidden_x")
    # Declared last, so no earlier frame index moves. With the pair enabled,
    # `enable_fulltext(..., if_not_exists=True)` has an early `False` to return
    # — the path a scoped handle must be refused in front of — and `search`
    # has hidden rows to leak.
    db.enable_fulltext("Person", "bio")
    yield db, db.scoped(keys=list(VISIBLE))
    db.close()


# ── §5.3, row by row ─────────────────────────────────────────────────────────


def test_query(store):
    db, s = store
    q = "MATCH (n:Person) RETURN key(n) AS k ORDER BY k"
    assert _mentions_hidden(db.query(q)), "fixture: the unscoped answer names them"
    rows = s.query(q)
    _assert_clean("query", rows)
    assert [r["k"] for r in rows] == list(VISIBLE)


def test_query_with_params(store):
    """Not in §5.3's table, and a full read of the store if left unscoped."""
    db, s = store
    q = "MATCH (n:Person) WHERE n.team = $t RETURN key(n) AS k ORDER BY k"
    assert _mentions_hidden(db.query_with_params(q, [("t", "red")]))
    rows = s.query_with_params(q, [("t", "red")])
    _assert_clean("query_with_params", rows)
    assert [r["k"] for r in rows] == list(VISIBLE)


def test_query_at(store):
    db, s = store
    commit = db.wal_total_commits() - 1
    q = "MATCH (n:Person) RETURN key(n) AS k ORDER BY k"
    assert _mentions_hidden(db.query_at(commit, q))
    rows = s.query_at(commit, q)
    _assert_clean("query_at", rows)
    assert [r["k"] for r in rows] == list(VISIBLE)


def test_node_info(store):
    db, s = store
    assert db.node_info("hidden_x") is not None
    _assert_clean("node_info", s.node_info("vis_a"))
    assert s.node_info("hidden_x") is None, "hidden reads exactly as absent"


def test_node_edges(store):
    db, s = store
    assert _mentions_hidden(db.node_edges("vis_a"))
    _assert_clean("node_edges", s.node_edges("vis_a"))
    with pytest.raises(RuntimeError):
        s.node_edges("hidden_x")


def test_neighborhood_has_no_python_binding(store):
    """§5.3 lists `neighborhood`; the binding exposes only `neighbors`.

    If a `neighborhood` method is ever added, this fails and says so — the
    point being that a new read surface must arrive already scoped.
    """
    _db, _s = store
    assert not hasattr(GraphDb, "neighborhood"), (
        "GraphDb.neighborhood now exists: route it through neighborhood_scoped "
        "and give it a row in this file"
    )


def test_neighbors(store):
    db, s = store
    assert _mentions_hidden(db.neighbors("vis_a", "LINKS", "out"))
    got = s.neighbors("vis_a", "LINKS", "out")
    _assert_clean("neighbors", got)
    assert got == ["vis_b"]
    with pytest.raises(RuntimeError):
        s.neighbors("hidden_x", "LINKS", "out")


def test_explain(store):
    db, s = store
    assert db.explain("vis_a", "hidden_x"), "fixture: they are linked"
    _assert_clean("explain", s.explain("vis_a", "vis_b"))
    with pytest.raises(RuntimeError):
        s.explain("vis_a", "hidden_x")


def test_degree(store):
    db, s = store
    assert db.degree("vis_a", edge_type="LINKS", direction="out") == 2
    assert s.degree("vis_a", edge_type="LINKS", direction="out") == 1, (
        "an unfiltered count discloses a hidden neighbour by arithmetic"
    )
    with pytest.raises(RuntimeError):
        s.degree("hidden_x")


def test_degree_multiplicity_counts_visible_pairs_only(store):
    """A multiplicity count discloses more than a unique one, so it is filtered
    at least as hard: not only that a hidden neighbour exists, but how often it
    was written."""
    db, s = store
    db.enable_multiplicity()
    # `vis_a → hidden_x` written three more times; `vis_a → vis_b` once more.
    for _ in range(3):
        db.insert_edge("LINKS", "vis_a", "hidden_x")
    db.insert_edge("LINKS", "vis_a", "vis_b")

    assert db.degree("vis_a", edge_type="LINKS", direction="out", multiplicity=True) == 6
    assert s.degree("vis_a", edge_type="LINKS", direction="out", multiplicity=True) == 2, (
        "the hidden neighbour's four inserts are outside the scope"
    )
    # The unique reading is unchanged by the preference.
    assert s.degree("vis_a", edge_type="LINKS", direction="out") == 1
    with pytest.raises(RuntimeError):
        s.degree("hidden_x", multiplicity=True)


def test_degrees(store):
    db, s = store
    assert _mentions_hidden(db.degrees(label="Person"))
    got = s.degrees(label="Person")
    _assert_clean("degrees", got)
    assert sorted(k for k, _ in got) == list(VISIBLE)


def test_degrees_multiplicity_filters_both_sides(store):
    db, s = store
    db.enable_multiplicity()
    for _ in range(3):
        db.insert_edge("LINKS", "vis_a", "hidden_x")
    got = s.degrees(label="Person", edge_type="LINKS", direction="out", multiplicity=True)
    _assert_clean("degrees(multiplicity=True)", got)
    assert sorted(k for k, _ in got) == list(VISIBLE)
    assert dict(got)["vis_a"] == 1, "only the visible pair is in the sum"


def test_find_similar(store):
    db, s = store
    assert _mentions_hidden(db.find_similar("emb", [1.0, 0.0], k=10))
    got = s.find_similar("emb", [1.0, 0.0], k=10)
    _assert_clean("find_similar", got)
    assert sorted(k for k, _ in got) == list(VISIBLE)


def test_find_similar_with_a_per_call_mask_narrows_further(store):
    _db, s = store
    got = s.find_similar("emb", [1.0, 0.0], k=10, mask=["vis_a", "hidden_x"])
    _assert_clean("find_similar(mask=)", got)
    assert [k for k, _ in got] == ["vis_a"], "a per-call mask narrows, never widens"


def test_pairwise_similar(store):
    db, s = store
    keys = ["vis_a", "vis_b", "hidden_x"]
    assert _mentions_hidden(db.pairwise_similar(keys, "emb", k=10))
    got = s.pairwise_similar(keys, "emb", k=10)
    _assert_clean("pairwise_similar", got)
    assert [src for src, _ in got] == list(VISIBLE)


def test_search_hybrid(store):
    db, s = store
    assert _mentions_hidden(db.search_hybrid("bio", "alpha", "emb", [1.0, 0.0], k=10))
    got = s.search_hybrid("bio", "alpha", "emb", [1.0, 0.0], k=10)
    _assert_clean("search_hybrid", got)
    assert sorted(k for k, _ in got) == list(VISIBLE)


def test_was_linked(store):
    db, s = store
    commit = db.wal_total_commits() - 1
    assert db.was_linked("vis_a", "hidden_x", "LINKS", commit) is True
    assert s.was_linked("vis_a", "hidden_x", "LINKS", commit) is False, (
        "hidden answers as absent does, and an unknown key here is False"
    )
    assert s.was_linked("vis_a", "vis_b", "LINKS", commit) is True


def test_edges_at(store):
    db, s = store
    commit = db.wal_total_commits() - 1
    assert _mentions_hidden(db.edges_at("vis_a", commit))
    _assert_clean("edges_at", s.edges_at("vis_a", commit))
    assert s.edges_at("hidden_x", commit) == [], "as an unknown key answers"


def test_node_history(store):
    db, s = store
    assert _mentions_hidden(db.node_history("vis_a")["history"])
    _assert_clean("node_history", s.node_history("vis_a")["history"])
    assert s.node_history("hidden_x")["history"] == [], "as an unknown key answers"


def test_edge_history(store):
    db, s = store
    assert db.edge_history("vis_a", "hidden_x")["events"], "fixture: there are events"
    assert s.edge_history("vis_a", "hidden_x")["events"] == []
    _assert_clean("edge_history", s.edge_history("vis_a", "vis_b")["events"])
    assert s.edge_history("vis_a", "vis_b")["events"]


def test_what_if_set_prop(store):
    db, s = store
    assert _mentions_hidden(db.what_if_set_prop("vis_a", "team", "green"))
    _assert_clean("what_if_set_prop", s.what_if_set_prop("vis_a", "team", "green"))
    with pytest.raises(RuntimeError):
        s.what_if_set_prop("hidden_x", "team", "green")


def test_get_edge_prop(store):
    db, s = store
    assert db.get_edge_prop("SIMILAR", "vis_a", "hidden_x", "score") is not None
    assert s.get_edge_prop("SIMILAR", "vis_a", "hidden_x", "score") is None
    assert s.get_edge_prop("SIMILAR", "vis_a", "vis_b", "score") is not None


def test_stats(store):
    db, s = store
    names = [n["name"] for n in db.stats()["namespaces"]]
    assert "tenant-secret" in names, "fixture: the roster has something to narrow"
    scoped = s.stats()
    _assert_clean("stats", scoped["namespaces"])
    assert [n["name"] for n in scoped["namespaces"]] == ["default"]
    assert [n["nodes_live"] for n in scoped["namespaces"]] == [2]


def test_schema_facts_stay_unscoped(store):
    """`has_vector_rule`, `is_index_enabled` and `is_multiplicity_enabled` are
    schema, not node data."""
    db, s = store
    db.enable_index("Person", "team")
    assert s.is_index_enabled("Person", "team") == db.is_index_enabled("Person", "team")
    assert s.has_vector_rule("emb") == db.has_vector_rule("emb")
    # A store-wide flag names no node, so a scoped handle reads it unchanged.
    assert s.is_multiplicity_enabled() == db.is_multiplicity_enabled()
    db.enable_multiplicity()
    assert s.is_multiplicity_enabled() is True
    # A yes/no about a pair the caller named — the twin of `is_index_enabled`.
    assert s.is_fulltext_enabled("Person", "bio") == db.is_fulltext_enabled("Person", "bio")


def test_commit_time_facts_stay_unscoped(store):
    """A commit's wall-clock time is not node data.

    `resolve_date` and `commit_time_ms` answer questions about the store's
    *history of commits*, not about which nodes are in it. Neither names a node,
    neither can be narrowed by a scope, and neither discloses anything a scoped
    caller could not already learn from `wal_total_commits`, which is likewise
    store-wide. They therefore answer unscoped, exactly as `has_vector_rule` and
    `is_index_enabled` do.

    The alternative — refusing them on a scoped handle, as `roles()` is refused
    — would be wrong for the opposite reason to `roles()`: a role definition is
    made of node keys, and a commit timestamp is made of nothing but a clock.
    """
    db, s = store
    total = db.wal_total_commits()
    assert s.wal_total_commits() == total

    for frame in range(total):
        assert s.commit_time_ms(frame) == db.commit_time_ms(frame)

    # And a date resolves to the same frame through either handle.
    assert s.resolve_date("2099-01-01") == db.resolve_date("2099-01-01")


def test_roles_is_refused_on_a_scoped_handle(store):
    """The role list is the one schema fact made of node data.

    `has_vector_rule` and `is_index_enabled` answer unscoped because a field
    name and an index flag name no node. A role definition names node keys
    outright (`keys` is an administrative grant, spelled as keys), plus the
    namespace roster `stats()` is at pains to narrow and the name of every other
    role in the store. Narrowing it is no better: a `RoleDef` with its hidden
    keys filtered out is not the definition, and a caller checking a role at
    boot against a doctored copy is worse off than one that was refused.

    So the whole surface is refused on a scoped handle, and the refusal names
    the handle to call it on — the same shape as the write refusals below.
    """
    _db, s = store
    with pytest.raises(ValueError) as err:
        s.roles()
    _assert_clean("roles refusal", str(err.value))
    assert "scoped" in str(err.value)


# Reads that answer about the whole store and take no mask. No algorithm or
# index reader in the engine can be narrowed, and filtering the rows after the
# fact is unsound — a visible node's score is computed over hidden topology —
# so each is refused outright on a scoped handle, the way `roles()` is.
_REFUSED_READS = {
    "pagerank": lambda h: h.pagerank(),
    "connected_components": lambda h: h.connected_components(),
    "degree_centrality": lambda h: h.degree_centrality(),
    "communities": lambda h: h.communities(),
    "search": lambda h: h.search("bio", "alpha"),
    "fulltext_pairs": lambda h: h.fulltext_pairs(),
    "rules": lambda h: h.rules(),
    "suggest_rules": lambda h: h.suggest_rules(),
    "recall": lambda h: h.recall("alpha"),
    "schema_report": lambda h: h.schema_report(),
    "identity_clusters": lambda h: h.identity_clusters(),
}

# The ones whose unscoped answer names every node in the fixture — a row per
# node from the algorithms, a hit per `bio` from `search` — so the fixture is
# known to hold something to leak.
_NAMES_EVERY_NODE = (
    "pagerank",
    "connected_components",
    "degree_centrality",
    "communities",
    "search",
)


@pytest.mark.parametrize("name", sorted(_REFUSED_READS))
def test_a_whole_store_read_is_refused_on_a_scoped_handle(store, name):
    db, s = store
    call = _REFUSED_READS[name]
    if name in _NAMES_EVERY_NODE:
        assert _mentions_hidden(call(db)), f"fixture: unscoped {name}() names the hidden nodes"
    with pytest.raises(ValueError) as err:
        call(s)
    _assert_clean(f"{name} refusal", str(err.value))
    assert "scoped" in str(err.value)
    assert name in str(err.value), "the refusal names the method"


def test_the_roles_refusal_is_a_plain_value_error_and_both_docs_say_so(store):
    """Defect F7. `roles()` was the first refusal here that is not a typed
    engine error, and until now neither doc named the class.

    The choice is deliberate. `ReadOnly` — what every refused write raises —
    means *a scoped handle never writes*; it is raised only from
    `refuse_if_scoped`, which only the write path reaches. `roles()` is a read
    refused outright (the whole-store reads above follow it), so it has no
    `ReadOnly` precedent to be
    inconsistent with, and raising one would make that class mean two things.

    The cost is real: a sidecar catching `MushroomError` around a boot-time
    role check does not catch this. So the class is pinned here, and both the
    docstring and the packaged stub must say it — this is defect #18's shape,
    where the behaviour was right and the sentence describing it was absent.
    """
    import importlib.util
    import pathlib

    import mushroomdb

    _db, s = store
    with pytest.raises(ValueError) as err:
        s.roles()
    assert type(err.value) is ValueError, (
        f"the roles refusal is {type(err.value).__name__}; it is documented as a "
        "plain ValueError, so changing it is a breaking change to a documented class"
    )
    assert not isinstance(err.value, mushroomdb.MushroomError), (
        "deliberately outside the MushroomError tree: it is caller misuse, not an "
        "engine condition"
    )

    spec = importlib.util.find_spec("mushroomdb")
    assert spec is not None and spec.origin is not None
    stub = (pathlib.Path(spec.origin).parent / "__init__.pyi").read_text()
    stub_doc = stub.split("def roles(", 1)[1].split("\n    def ", 1)[0]
    doc = GraphDb.roles.__doc__ or ""
    for text, where in ((doc, "roles docstring"), (stub_doc, "packaged __init__.pyi")):
        assert "ValueError" in text, (
            f"{where} does not name the class the scoped refusal raises, so a caller "
            "cannot know it is outside the MushroomError tree"
        )
        assert "MushroomError" in text, (
            f"{where} names ValueError without saying it is *not* a MushroomError, "
            "which is the half a sidecar's except clause needs"
        )


def test_restore_is_a_staticmethod_about_other_directories(tmp_path):
    """`restore` is a staticmethod: `s.restore(a, b)` is `GraphDb.restore(a, b)`.

    Like `open`, it is on a scoped handle only because Python puts every
    staticmethod on every instance. It names two other directories and says
    nothing about this store, so it has no scoped contract to leak — that is
    the row.
    """
    db = GraphDb.open(str(tmp_path / "live"))
    db.insert_node("Person", "hidden_x", {})
    s = db.scoped(keys=list(VISIBLE))

    donor = GraphDb.open(str(tmp_path / "donor"))
    donor.insert_node("Person", "donated", {})
    donor.snapshot()
    donor.close()

    assert s.restore(str(tmp_path / "donor"), str(tmp_path / "fresh"))["outcome"] == "restored"
    other = GraphDb.open(str(tmp_path / "fresh"))
    assert other.node_info("donated") is not None, "a different store, not this one"
    assert other.node_info("hidden_x") is None
    other.close()
    db.close()


def test_wal_total_commits_is_a_store_fact(store):
    """Store-wide, like `stats()`'s counts: a frame count names no node."""
    db, s = store
    assert s.wal_total_commits() == db.wal_total_commits()


def test_refresh_is_permitted_and_names_nothing(store):
    """A scoped handle may refresh — it writes nothing — and gets a count."""
    _db, s = store
    assert s.refresh() == 0


def test_scoped_narrows_and_never_widens(store):
    """`scoped()` on a scoped handle intersects; it cannot reach back out."""
    _db, s = store
    narrower = s.scoped(keys=["vis_a", "hidden_x"])
    _assert_clean("scoped().query", narrower.query("MATCH (n) RETURN key(n) AS k"))
    assert [r["k"] for r in narrower.query("MATCH (n) RETURN key(n) AS k")] == ["vis_a"]
    # An unknown key in the leg is not an error — that would be an oracle too.
    assert s.scoped(keys=[ABSENT]).query("MATCH (n) RETURN key(n) AS k") == []


def test_close_is_permitted_on_a_scoped_handle(tmp_path):
    """Closing is not a write, and the two names share one store."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "vis_a", {})
    s = db.scoped(keys=list(VISIBLE))
    s.close()
    with pytest.raises(RuntimeError, match="closed"):
        db.node_info("vis_a")


def test_context_manager_on_a_scoped_handle(tmp_path):
    """`__enter__` / `__exit__` are `close()`, so they are permitted too."""
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Person", "vis_a", {})
    with db.scoped(keys=list(VISIBLE)) as s:
        assert s.node_info("vis_a") is not None
    with pytest.raises(RuntimeError, match="closed"):
        db.node_info("vis_a")


def test_open_carries_no_scope_and_reads_no_other_store(tmp_path):
    """`open` is a staticmethod: `s.open(p)` is `GraphDb.open(p)`, unscoped.

    It is on a scoped handle only because Python puts every staticmethod on
    every instance. It touches a different path and says nothing about this
    store, so it has no scoped contract to leak — that is the row.
    """
    db = GraphDb.open(str(tmp_path / "a"))
    db.insert_node("Person", "hidden_x", {})
    s = db.scoped(keys=list(VISIBLE))
    other = s.open(str(tmp_path / "b"))
    assert other.node_info("hidden_x") is None, "a different store, not this one"
    other.close()
    db.close()


# ── the write surface: a refusal is an output too ────────────────────────────


def test_upsert_node_is_not_an_existence_or_label_oracle(store):
    """Defect #8: the existence read ran unscoped, in front of the refusal.

    Both calls must be the same class with the same message. Before the fix the
    hidden key raised `ValueError` naming `'Person'` and the absent key raised
    `RuntimeError` — the class alone answered "does this key exist".
    """
    db, s = store
    assert db.node_info("hidden_x")["label"] == "Person", "fixture: there is a label to leak"

    with pytest.raises(RuntimeError) as hidden:
        s.upsert_node("__probe__", "hidden_x", {})
    with pytest.raises(RuntimeError) as absent:
        s.upsert_node("__probe__", ABSENT, {})

    assert type(hidden.value) is type(absent.value), (
        "the exception class is a one-call existence oracle: "
        f"hidden → {type(hidden.value).__name__}, absent → {type(absent.value).__name__}"
    )
    assert str(hidden.value) == str(absent.value), (
        f"the message discloses the node: {hidden.value}"
    )
    _assert_clean("upsert_node refusal", str(hidden.value))
    assert "Person" not in str(hidden.value), "the refusal named the hidden node's label"
    assert "scoped" in str(hidden.value)


# Every write, called once against a hidden key and once against an absent one.
# The two calls must be indistinguishable: same class, same message. A write
# that reads the store before refusing shows up here as a difference.
#
# Defect #16: the key is **every** key argument the method takes, not one of
# them. The rows are a cross-product of `(method, key position)`, so a write
# with two endpoints is probed on both. There is no live gap today — all five
# two-key writes refuse at `with_mut` before either argument is read — but the
# property this dict exists to enforce is *no key argument is read before the
# refusal*, and probing one position per method tested that for one argument.
# `upsert_node` (defect #8) is what an endpoint pre-check looks like when it
# slips in; a `src`-side one would have passed the old parametrisation.
_WRITES_BY_KEY = {
    ("insert_node", "key"): lambda s, k: s.insert_node("Person", k, {"t": 1}),
    # A *mismatching* label on purpose: `test_scoped.py` probes with the stored
    # label, which falls straight through to the write refusal and never runs
    # the branch defect #8 lived in.
    ("upsert_node", "key"): lambda s, k: s.upsert_node("__probe_label__", k, {"t": 1}),
    ("insert_edge", "src"): lambda s, k: s.insert_edge("LINKS", k, "vis_b"),
    ("insert_edge", "dst"): lambda s, k: s.insert_edge("LINKS", "vis_a", k),
    ("delete_edge", "src"): lambda s, k: s.delete_edge("LINKS", k, "vis_b"),
    ("delete_edge", "dst"): lambda s, k: s.delete_edge("LINKS", "vis_a", k),
    ("insert_edge_upsert", "src"): lambda s, k: s.insert_edge_upsert(
        "LINKS", k, "vis_b", "Person"
    ),
    ("insert_edge_upsert", "dst"): lambda s, k: s.insert_edge_upsert(
        "LINKS", "vis_a", k, "Person"
    ),
    ("delete_node", "key"): lambda s, k: s.delete_node(k),
    ("set_prop", "key"): lambda s, k: s.set_prop(k, "t", 1),
    ("remove_prop", "key"): lambda s, k: s.remove_prop(k, "t"),
    ("rename_node", "old"): lambda s, k: s.rename_node(k, "__probe__"),
    ("rename_node", "new"): lambda s, k: s.rename_node("vis_a", k),
    ("query_write", "key"): lambda s, k: s.query_write(
        "MATCH (n) WHERE key(n) = $k SET n.t = 1 RETURN key(n)", {"k": k}
    ),
    ("ingest_batch", "key"): lambda s, k: s.ingest_batch(
        [{"key": k, "label": "Person", "props": {"t": 1}}], on_conflict="replace"
    ),
    ("batch_edges", "src"): lambda s, k: s.batch_edges(
        [{"edge_type": "LINKS", "src": k, "dst": "vis_b"}]
    ),
    ("batch_edges", "dst"): lambda s, k: s.batch_edges(
        [{"edge_type": "LINKS", "src": "vis_a", "dst": k}]
    ),
    # A *mismatching* label on purpose, as the `upsert_node` row has: the
    # relabel refusal reads the stored label, and on a scoped handle that read
    # must never be reached.
    ("upsert_entity", "key"): lambda s, k: s.upsert_entity(k, {"t": 1}, label="__probe_label__"),
    ("remember", "about"): lambda s, k: s.remember("probe", about=[k], ts=1),
    ("remember", "entity"): lambda s, k: s.remember(
        "probe", ts=1, entities=[{"key": k, "label": "__probe_label__", "props": {"t": 1}}]
    ),
    ("forget", "key"): lambda s, k: s.forget(key=k),
    ("forget", "subject"): lambda s, k: s.forget(
        fact={"subject": k, "predicate": "LINKS", "object": "vis_b"}
    ),
    ("forget", "object"): lambda s, k: s.forget(
        fact={"subject": "vis_a", "predicate": "LINKS", "object": k}
    ),
}


@pytest.mark.parametrize("name,position", sorted(_WRITES_BY_KEY))
def test_a_keyed_write_refuses_identically_for_hidden_and_absent(store, name, position):
    db, s = store
    call = _WRITES_BY_KEY[(name, position)]
    who = f"{name}({position})"

    with pytest.raises(RuntimeError) as hidden:
        call(s, "hidden_x")
    with pytest.raises(RuntimeError) as absent:
        call(s, ABSENT)

    assert type(hidden.value) is type(absent.value), (
        f"{who}: hidden → {type(hidden.value).__name__}, "
        f"absent → {type(absent.value).__name__}; the class is an existence oracle"
    )
    assert str(hidden.value) == str(absent.value), f"{who} discloses: {hidden.value}"
    _assert_clean(f"{who} refusal", str(hidden.value))
    assert "scoped" in str(hidden.value), f"{who}: {hidden.value}"

    # And nothing landed, on either key.
    assert db.node_info("hidden_x")["props"].get("t") is None
    assert db.node_info(ABSENT) is None
    assert db.node_info("__probe__") is None
    # The `rename_node(new)` row moves `vis_a`, not the probed key, so the
    # subject of every two-key row has to be checked too — otherwise that row
    # asserts nothing about what the refusal prevented.
    assert db.node_info("vis_a") is not None, f"{who}: the write's other endpoint moved"


# Writes with no node key to probe. They still must refuse, and refuse before
# reading anything: the message is the scoped refusal, not a store fact.
_WRITES_KEYLESS = {
    "create_rule": lambda s: s.create_rule(
        {
            "name": "same_team",
            "src_label": "Person",
            "dst_label": "Person",
            "predicate": {"FieldEqual": {"field": "team"}},
            "edge_type": "SAME_TEAM",
            "weight_prop": None,
        },
        if_not_exists=True,
    ),
    "enable_index": lambda s: s.enable_index("Person", "team"),
    "disable_index": lambda s: s.disable_index("Person", "team"),
    "enable_multiplicity": lambda s: s.enable_multiplicity(),
    "snapshot": lambda s: s.snapshot(),
    # Asserting when a commit happened rewrites the store's apparent history,
    # which is a write in the sense that matters: a scoped handle never does it.
    "record_commits_at": lambda s: s.record_commits_at(1_800_000_000_000),
    # `if_not_exists=True` reads before it writes, so it needs the refusal in
    # front of the read — the shape `create_rule` has.
    "enable_fulltext": lambda s: s.enable_fulltext("Person", "bio", if_not_exists=True),
    "disable_fulltext": lambda s: s.disable_fulltext("Person", "bio"),
    "delete_rule": lambda s: s.delete_rule("same_team"),
    "rebuild_rule": lambda s: s.rebuild_rule("same_team"),
}


@pytest.mark.parametrize("name", sorted(_WRITES_KEYLESS))
def test_a_keyless_write_refuses_with_the_scoped_message(store, name):
    _db, s = store
    with pytest.raises(RuntimeError, match="scoped") as err:
        _WRITES_KEYLESS[name](s)
    _assert_clean(f"{name} refusal", str(err.value))


# ── the file is the surface: nothing may be missing from it ──────────────────

# Every public method `GraphDb` exposes, mapped to why it is safe on a scoped
# handle. Adding a method without adding a row fails the test below — which is
# the point: defect #8 existed because `upsert_node` had no row here.
COVERED = {
    # reads — one row of spec §5.3 each
    "query": "test_query",
    "query_with_params": "test_query_with_params",
    "query_at": "test_query_at",
    "node_info": "test_node_info",
    "node_edges": "test_node_edges",
    "neighbors": "test_neighbors",
    "explain": "test_explain",
    "degree": "test_degree",
    "degrees": "test_degrees",
    "find_similar": "test_find_similar",
    "pairwise_similar": "test_pairwise_similar",
    "search_hybrid": "test_search_hybrid",
    "was_linked": "test_was_linked",
    "edges_at": "test_edges_at",
    "node_history": "test_node_history",
    "edge_history": "test_edge_history",
    "what_if_set_prop": "test_what_if_set_prop",
    "get_edge_prop": "test_get_edge_prop",
    "stats": "test_stats",
    # deliberately unscoped, and each says why
    "has_vector_rule": "test_schema_facts_stay_unscoped",
    "is_index_enabled": "test_schema_facts_stay_unscoped",
    # A commit time is made of a clock, not of node data — see the test.
    "resolve_date": "test_commit_time_facts_stay_unscoped",
    "commit_time_ms": "test_commit_time_facts_stay_unscoped",
    "is_multiplicity_enabled": "test_schema_facts_stay_unscoped",
    "wal_total_commits": "test_wal_total_commits_is_a_store_fact",
    # refused outright: schema made of node data, with no honest narrowing
    "roles": "test_roles_is_refused_on_a_scoped_handle",
    # whole-store reads — refused outright, like `roles`
    "pagerank": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "connected_components": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "degree_centrality": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "communities": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "search": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "fulltext_pairs": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "rules": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "suggest_rules": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "recall": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "schema_report": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "identity_clusters": "test_a_whole_store_read_is_refused_on_a_scoped_handle",
    "is_fulltext_enabled": "test_schema_facts_stay_unscoped",
    "enable_fulltext": "test_a_keyless_write_refuses_with_the_scoped_message",
    "disable_fulltext": "test_a_keyless_write_refuses_with_the_scoped_message",
    "delete_rule": "test_a_keyless_write_refuses_with_the_scoped_message",
    "rebuild_rule": "test_a_keyless_write_refuses_with_the_scoped_message",
    # permitted non-writes
    "refresh": "test_refresh_is_permitted_and_names_nothing",
    "scoped": "test_scoped_narrows_and_never_widens",
    "close": "test_close_is_permitted_on_a_scoped_handle",
    "open": "test_open_carries_no_scope_and_reads_no_other_store",
    "restore": "test_restore_is_a_staticmethod_about_other_directories",
    # writes — refused, and the refusal discloses nothing
    "insert_node": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "upsert_node": "test_upsert_node_is_not_an_existence_or_label_oracle",
    "insert_edge": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "delete_edge": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "insert_edge_upsert": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "delete_node": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "set_prop": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "remove_prop": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "rename_node": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "query_write": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "ingest_batch": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "batch_edges": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "remember": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "upsert_entity": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "forget": "test_a_keyed_write_refuses_identically_for_hidden_and_absent",
    "create_rule": "test_a_keyless_write_refuses_with_the_scoped_message",
    "record_commits_at": "test_a_keyless_write_refuses_with_the_scoped_message",
    "enable_index": "test_a_keyless_write_refuses_with_the_scoped_message",
    "disable_index": "test_a_keyless_write_refuses_with_the_scoped_message",
    "enable_multiplicity": "test_a_keyless_write_refuses_with_the_scoped_message",
    "snapshot": "test_a_keyless_write_refuses_with_the_scoped_message",
}

# `__enter__` / `__exit__` are the context-manager spelling of `close()`, and
# carry their own row; no other dunder is a surface.
_DUNDERS = {"__enter__": "test_context_manager_on_a_scoped_handle"}


def test_every_method_has_a_row():
    """A method reachable on a scoped handle that nobody checked is a defect.

    This is the guard the release leans on: `upsert_node` leaked an existence
    oracle for as long as it did because no row in this file named it.
    """
    public = {
        name
        for name, attr in inspect.getmembers(GraphDb)
        if not name.startswith("_") and callable(attr)
    }
    missing = public - set(COVERED)
    assert not missing, (
        f"GraphDb.{sorted(missing)} is reachable on a scoped handle and has no row "
        "in test_scope_leaks.py: give it one, and make sure it applies the scope"
    )
    stale = set(COVERED) - public
    assert not stale, f"COVERED names methods that no longer exist: {sorted(stale)}"

    for dunder in _DUNDERS:
        assert hasattr(GraphDb, dunder), f"{dunder} vanished; its row is now wrong"


def test_covered_names_real_tests():
    """Every row points at a test that exists in this module."""
    here = {n for n in globals() if n.startswith("test_")}
    for method, test in sorted({**COVERED, **_DUNDERS}.items()):
        assert test in here, f"COVERED[{method!r}] names {test!r}, which is not in this file"
