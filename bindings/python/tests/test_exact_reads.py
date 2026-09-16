"""Exact-read binding tests: pairwise_similar, find_similar where=/exact=, degree."""

from __future__ import annotations

from mushroomdb import GraphDb


def _l2(xs: list[float]) -> float:
    return sum(x * x for x in xs) ** 0.5


def _cosine(a: list[float], b: list[float]) -> float | None:
    if len(a) != len(b):
        return None
    an, bn = _l2(a), _l2(b)
    if an == 0.0 or bn == 0.0:
        return None
    return sum((x / an) * (y / bn) for x, y in zip(a, b))


def _pairwise_oracle(
    nodes: list[tuple[str, list[float]]], k: int, min_: float
) -> list[tuple[str, list[tuple[str, float]]]]:
    out: list[tuple[str, list[tuple[str, float]]]] = []
    for i, (src, q) in enumerate(nodes):
        neigh: list[tuple[str, float]] = []
        for j, (dst, v) in enumerate(nodes):
            if i == j:
                continue
            sim = _cosine(q, v)
            if sim is None or sim < min_:
                continue
            neigh.append((dst, sim))
        neigh.sort(key=lambda t: (-t[1], t[0]))
        out.append((src, neigh[:k]))
    return out


def _fill_vec(seed: int, dim: int) -> list[float]:
    s = seed | 1
    xs: list[float] = []
    mask = (1 << 64) - 1
    for _ in range(dim):
        s = ((s * 6364136223846793005) + 1) & mask
        x = float(s >> 33) / float(0xFFFFFFFF) * 2.0 - 1.0
        xs.append(0.5 if x == 0.0 else x)
    return xs


def test_pairwise_similar_empty_keys(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    assert db.pairwise_similar([], "emb") == []
    db.close()


def test_pairwise_similar_excludes_self(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Item", "a", {"emb": [1.0, 0.0]})
    db.insert_node("Item", "b", {"emb": [0.9, 0.1]})
    got = db.pairwise_similar(["a", "b"], "emb", k=10, min=0.0)
    assert got, "both keys should pack"
    for src, neigh in got:
        assert src not in [dst for dst, _ in neigh]
    db.close()


def test_pairwise_similar_skip_missing(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Item", "keep", {"emb": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]})
    db.insert_node("Item", "keep2", {"emb": [0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]})
    db.insert_node("Item", "nofield", {"name": "x"})
    db.insert_node("Item", "zero", {"emb": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]})
    db.insert_node("Item", "wrong", {"emb": [1.0, 0.0, 0.0]})
    got = db.pairwise_similar(
        ["keep", "nofield", "zero", "wrong", "keep2"], "emb", k=10, min=0.0
    )
    assert [src for src, _ in got] == ["keep", "keep2"]
    db.close()


def test_pairwise_similar_modal_dim_leftover_first(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Item", "leftover", {"emb": [1.0, 0.0]})
    majority: list[tuple[str, list[float]]] = []
    for i in range(8):
        key = f"n{i}"
        v = _fill_vec(i + 10, 8)
        db.insert_node("Item", key, {"emb": v})
        majority.append((key, v))
    keys = ["leftover"] + [k for k, _ in majority]
    got = db.pairwise_similar(keys, "emb", k=5, min=0.0)
    srcs = [src for src, _ in got]
    assert "leftover" not in srcs
    expect = _pairwise_oracle(majority, 5, 0.0)
    assert srcs == [k for k, _ in expect]
    for (_gk, gneigh), (_ek, eneigh) in zip(got, expect):
        assert [d for d, _ in gneigh] == [d for d, _ in eneigh]
        for (_gd, gs), (_ed, es) in zip(gneigh, eneigh):
            assert abs(gs - es) <= 1e-9
    db.close()


def test_pairwise_similar_naive_32(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    nodes: list[tuple[str, list[float]]] = []
    for i in range(32):
        key = f"n{i:02}"
        v = _fill_vec(i + 1, 8)
        db.insert_node("Item", key, {"emb": v})
        nodes.append((key, v))
    keys = [k for k, _ in nodes]
    got = db.pairwise_similar(keys, "emb", k=5, min=0.0)
    expect = _pairwise_oracle(nodes, 5, 0.0)
    assert [s for s, _ in got] == [s for s, _ in expect]
    for (_gk, gneigh), (_ek, eneigh) in zip(got, expect):
        assert [d for d, _ in gneigh] == [d for d, _ in eneigh]
        for (_gd, gs), (_ed, es) in zip(gneigh, eneigh):
            assert abs(gs - es) <= 1e-9
    db.close()


def test_find_similar_where_eq_matches_mask(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Document", "d1", {"emb": [1.0, 0.0], "resource_scope_id": "a"})
    db.insert_node("Document", "d2", {"emb": [0.9, 0.1], "resource_scope_id": "a"})
    db.insert_node("Document", "d3", {"emb": [1.0, 0.0], "resource_scope_id": "b"})
    q = [1.0, 0.0]
    by_where = db.find_similar(
        "emb",
        q,
        label="Document",
        k=10,
        min=0.0,
        where={"field": "resource_scope_id", "eq": "a"},
    )
    by_mask = db.find_similar("emb", q, label="Document", k=10, min=0.0, mask=["d1", "d2"])
    assert by_where == by_mask
    assert [k for k, _ in by_where] == ["d1", "d2"]
    db.close()


def test_find_similar_where_empty_in(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Document", "d1", {"emb": [1.0, 0.0], "resource_scope_id": "a"})
    hits = db.find_similar(
        "emb",
        [1.0, 0.0],
        label="Document",
        k=10,
        min=0.0,
        where={"field": "resource_scope_id", "in": []},
    )
    assert hits == []
    db.close()


def test_find_similar_where_missing_property(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Document", "kept", {"emb": [1.0, 0.0], "resource_scope_id": "a"})
    db.insert_node("Document", "bare", {"emb": [1.0, 0.0]})
    hits = db.find_similar(
        "emb",
        [1.0, 0.0],
        label="Document",
        k=10,
        min=0.0,
        where={"field": "resource_scope_id", "eq": "a"},
    )
    assert [k for k, _ in hits] == ["kept"]
    db.close()


def test_find_similar_where_invalid_raises_valueerror(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Document", "d1", {"emb": [1.0, 0.0], "resource_scope_id": "a"})
    try:
        db.find_similar(
            "emb",
            [1.0, 0.0],
            label="Document",
            where={"field": "resource_scope_id", "eq": "a", "in": ["b"]},
        )
    except ValueError as e:
        assert "where" in str(e)
        assert "both" in str(e)
    else:
        raise AssertionError("expected ValueError for both eq and in")
    db.close()


def test_find_similar_exact_true(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("Item", "a", {"emb": [1.0, 0.0]})
    db.insert_node("Item", "b", {"emb": [0.9, 0.1]})
    brute = db.find_similar("emb", [1.0, 0.0], label="Item", k=10, min=0.0, exact=True)
    default = db.find_similar("emb", [1.0, 0.0], label="Item", k=10, min=0.0)
    assert brute == default
    db.close()


def test_degree_direction_both(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("N", "a", {})
    db.insert_node("N", "b", {})
    db.insert_edge("E", "a", "b")
    db.insert_edge("E", "b", "a")
    assert db.degree("a", direction="both") == 2
    assert db.degree("a") == 2
    assert db.degree("a", edge_type="E", direction="out") == 1
    db.close()


def test_neighbors_rejects_direction_both(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("N", "a", {})
    try:
        db.neighbors("a", "E", "both")
    except ValueError as e:
        assert "out" in str(e) or "in" in str(e)
    else:
        raise AssertionError("neighbors must still reject direction='both'")
    db.close()


def test_degree_duplicate_edge(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("N", "a", {})
    db.insert_node("N", "b", {})
    assert db.insert_edge("E", "a", "b") is True
    assert db.degree("a", edge_type="E", direction="out") == 1
    assert db.insert_edge("E", "a", "b") is False
    assert db.degree("a", edge_type="E", direction="out") == 1
    db.close()


def test_degrees_empty_keys(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("N", "a", {})
    assert db.degrees(keys=[]) == []
    db.close()


def test_degrees_omits_unknown(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("N", "a", {})
    db.insert_node("N", "b", {})
    db.insert_edge("E", "a", "b")
    got = db.degrees(keys=["ghost", "a"], edge_type="E", direction="out")
    assert got == [("a", 1)]
    db.close()


def test_degrees_where_limit(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    for k, n_out, scope in [("a", 3, "keep"), ("b", 3, "keep"), ("c", 1, "keep"), ("z", 9, "drop")]:
        db.insert_node("Document", k, {"scope": scope})
        for i in range(n_out):
            dst = f"{k}-d{i}"
            db.insert_node("Document", dst, {})
            db.insert_edge("E", k, dst)
    got = db.degrees(
        label="Document",
        where={"field": "scope", "eq": "keep"},
        edge_type="E",
        direction="out",
        limit=2,
    )
    assert got == [("a", 3), ("b", 3)]
    db.close()


def test_degrees_where_invalid_raises_valueerror(tmp_path):
    db = GraphDb.open(str(tmp_path / "db"))
    db.insert_node("N", "a", {})
    try:
        db.degrees(where={"field": "scope", "eq": "a", "in": ["b"]})
    except ValueError as e:
        assert "where" in str(e)
        assert "both" in str(e)
    else:
        raise AssertionError("expected ValueError for both eq and in")
    db.close()
