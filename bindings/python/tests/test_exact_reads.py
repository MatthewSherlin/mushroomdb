"""Exact-read binding tests: pairwise_similar (and later where=/degree)."""

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
