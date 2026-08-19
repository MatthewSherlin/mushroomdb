"""Fixture ingest + six-rule exact-semantics validation.

Hand-computed expected INDUSTRY / SPECIALTY / LOCATION / SIMILAR_SIZE /
MATCHES_DESIGN_STYLE sets come from the copied ListingsHub fixtures, not
from transform.py's runtime output. Semantic pairs are recomputed from
the documented synthetic-embedding recipe (fixtures carry no vectors).
"""

from __future__ import annotations

import math
from pathlib import Path

import pytest

from mushroomdb import GraphDb

from rules import SIX_RULES
from transform import (
    EMBED_DIM,
    FIXTURE_FILES,
    cosine,
    load_fixtures,
    load_user_edges,
    synthetic_embedding,
)

FIXTURES = Path(__file__).resolve().parent / "fixtures"

# Plan 10 Task 1 said 10/4/4; the verbatim fixtures are 5/2/2/7.
EXPECTED_COUNTS = {"Talent": 5, "Company": 2, "Job": 2, "User": 7}

# field_equal(industry) — binary 1.0. 'both' is absent from these fixtures.
INDUSTRY_TC = {
    ("listing-talent-alice", "listing-company-firma"),
    ("listing-talent-carol", "listing-company-firma"),
    ("listing-talent-eve-2", "listing-company-firma"),
    ("listing-talent-bob", "listing-company-firmb"),
    ("listing-talent-eve-1", "listing-company-firmb"),
}
INDUSTRY_TJ = {
    ("listing-talent-alice", "listing-job-firma"),
    ("listing-talent-carol", "listing-job-firma"),
    ("listing-talent-eve-2", "listing-job-firma"),
    ("listing-talent-bob", "listing-job-firmb"),
    ("listing-talent-eve-1", "listing-job-firmb"),
}

# overlap(specialties) Jaccard. Each side is a singleton; same string → 1.0.
SPECIALTY_TC = {
    ("listing-talent-alice", "listing-company-firma"),
    ("listing-talent-eve-1", "listing-company-firma"),
    ("listing-talent-bob", "listing-company-firmb"),
    ("listing-talent-eve-2", "listing-company-firmb"),
}
SPECIALTY_TJ = {
    ("listing-talent-alice", "listing-job-firma"),
    ("listing-talent-eve-1", "listing-job-firma"),
    ("listing-talent-bob", "listing-job-firmb"),
    ("listing-talent-eve-2", "listing-job-firmb"),
}

# No fixture listing carries lat/lon (or style). Exact sets are empty.
LOCATION_TC: set[tuple[str, str]] = set()
LOCATION_TJ: set[tuple[str, str]] = set()
DESIGN_TC: set[tuple[str, str]] = set()

# size_bucket: experience 0-2→1, 3-5→2, 6-9→3, 10-14→4, 15+→5
#              company headcount <10→1, <50→2, <200→3, <500→4, else 5
# alice/eve-1/eve-2 = 3; bob/carol = 2; firma = 2; firmb = 3
# numeric_within tolerance 1 → every Talent↔Company pair matches.
SIMILAR_SIZE_TC = {
    ("listing-talent-alice", "listing-company-firma"),  # |3-2| = 1 → score 0.0
    ("listing-talent-alice", "listing-company-firmb"),  # |3-3| = 0 → score 1.0
    ("listing-talent-bob", "listing-company-firma"),  # |2-2| = 0 → score 1.0
    ("listing-talent-bob", "listing-company-firmb"),  # |2-3| = 1 → score 0.0
    ("listing-talent-carol", "listing-company-firma"),
    ("listing-talent-carol", "listing-company-firmb"),
    ("listing-talent-eve-1", "listing-company-firma"),
    ("listing-talent-eve-1", "listing-company-firmb"),
    ("listing-talent-eve-2", "listing-company-firma"),
    ("listing-talent-eve-2", "listing-company-firmb"),
}

# Worked scores (engine arithmetic, pinned to 1e-9).
SCORE_INDUSTRY_ALICE_FIRMA = 1.0
SCORE_SPECIALTY_ALICE_FIRMA = 1.0  # |{residential}| / |{residential}| = 1
SCORE_SIZE_ALICE_FIRMA = 0.0  # 1.0 − |3 − 2| / 1
SCORE_SIZE_ALICE_FIRMB = 1.0  # 1.0 − |3 − 3| / 1

INQUIRED = {("st-user-alice", "listing-job-firma")}
CONNECTED: set[tuple[str, str]] = set()


def _pairs(db: GraphDb, src_label: str, edge_type: str, dst_label: str) -> set[tuple[str, str]]:
    rows = db.query(
        f"MATCH (a:{src_label})-[:{edge_type}]->(b:{dst_label}) RETURN a, b"
    )
    return {(r["a"], r["b"]) for r in rows}


def _explain_rule(db: GraphDb, src: str, dst: str, rule: str) -> dict:
    hits = [e for e in db.explain(src, dst) if e["rule"] == rule]
    assert hits, f"explain({src!r}, {dst!r}) missing rule {rule!r}"
    return hits[0]


def _expected_semantic_tc(nodes: dict) -> dict[tuple[str, str], float]:
    talents = [n for n in nodes["Talent"] if "embedding" in n["props"]]
    companies = [n for n in nodes["Company"] if "embedding" in n["props"]]
    out: dict[tuple[str, str], float] = {}
    for t in talents:
        for c in companies:
            score = cosine(t["props"]["embedding"], c["props"]["embedding"])
            if score is not None and score >= 0.85:
                out[(t["key"], c["key"])] = score
    return out


def test_fixture_files_are_the_five_copied_jsons():
    names = sorted(p.name for p in FIXTURES.glob("*.json"))
    assert names == sorted(FIXTURE_FILES)


def test_load_fixtures_counts_and_shape():
    nodes = load_fixtures(FIXTURES)
    for label, n in EXPECTED_COUNTS.items():
        assert label in nodes, f"missing label {label}"
        assert len(nodes[label]) == n, f"{label}: {len(nodes[label])} != {n}"
        for item in nodes[label]:
            assert set(item) == {"key", "label", "props"}
            assert item["label"] == label
            assert isinstance(item["key"], str) and item["key"]
            assert isinstance(item["props"], dict)

    alice = next(n for n in nodes["Talent"] if n["key"] == "listing-talent-alice")
    assert alice["props"]["industry"] == "architecture"
    assert alice["props"]["specialties"] == ["residential"]
    assert alice["props"]["size_bucket"] == 3
    assert alice["props"]["user_id"] == "st-user-alice"
    assert "location" not in alice["props"]
    emb = alice["props"]["embedding"]
    assert len(emb) == EMBED_DIM
    assert all(isinstance(x, float) for x in emb)
    norm = math.sqrt(sum(x * x for x in emb))
    assert norm == pytest.approx(1.0, abs=1e-9)
    assert emb == synthetic_embedding("listing-talent-alice", "architecture")

    firma = next(n for n in nodes["Company"] if n["key"] == "listing-company-firma")
    assert firma["props"]["industry"] == "architecture"
    assert firma["props"]["specialties"] == ["residential"]
    assert firma["props"]["size_bucket"] == 2
    assert firma["props"]["company_size"] == "10-50"

    job = next(n for n in nodes["Job"] if n["key"] == "listing-job-firma")
    assert job["props"]["industry"] == "architecture"
    assert job["props"]["specialties"] == ["residential"]
    assert "embedding" not in job["props"]

    edges = load_user_edges(FIXTURES)
    inquired = {(s, d) for et, s, d in edges if et == "INQUIRED"}
    connected = {(s, d) for et, s, d in edges if et == "CONNECTED"}
    assert inquired == INQUIRED
    assert connected == CONNECTED


def test_six_rules_are_per_pair_instances():
    names = [r["name"] for r in SIX_RULES]
    assert names == [
        "industry_alignment_tc",
        "industry_alignment_tj",
        "specialty_match_tc",
        "specialty_match_tj",
        "location_fit_tc",
        "location_fit_tj",
        "similar_size_tc",
        "matches_design_style_tc",
        "semantic_match_tc",
    ]
    assert all(r["weight_prop"] == "score" and r["max_edges"] is None for r in SIX_RULES)


def test_ingest_rules_derived_edges_and_explain(tmp_path):
    nodes = load_fixtures(FIXTURES)
    db = GraphDb.open(str(tmp_path / "db"))
    for label in ("User", "Talent", "Company", "Job"):
        for item in nodes[label]:
            db.insert_node(item["key"], item["label"], item["props"])
    for etype, src, dst in load_user_edges(FIXTURES):
        db.insert_edge(etype, src, dst)
    for rule in SIX_RULES:
        db.create_rule(rule)

    assert _pairs(db, "User", "INQUIRED", "Job") == INQUIRED
    assert _pairs(db, "User", "CONNECTED", "User") == CONNECTED

    assert _pairs(db, "Talent", "INDUSTRY_ALIGNMENT", "Company") == INDUSTRY_TC
    assert _pairs(db, "Talent", "INDUSTRY_ALIGNMENT", "Job") == INDUSTRY_TJ
    assert _pairs(db, "Talent", "SPECIALTY_MATCH", "Company") == SPECIALTY_TC
    assert _pairs(db, "Talent", "SPECIALTY_MATCH", "Job") == SPECIALTY_TJ
    assert _pairs(db, "Talent", "LOCATION_FIT", "Company") == LOCATION_TC
    assert _pairs(db, "Talent", "LOCATION_FIT", "Job") == LOCATION_TJ
    assert _pairs(db, "Talent", "SIMILAR_SIZE", "Company") == SIMILAR_SIZE_TC
    assert _pairs(db, "Talent", "MATCHES_DESIGN_STYLE", "Company") == DESIGN_TC

    semantic = _expected_semantic_tc(nodes)
    assert semantic, "synthetic embeddings must fire on at least one Talent↔Company pair"
    got_sem = _pairs(db, "Talent", "SEMANTIC_MATCH", "Company")
    assert got_sem == set(semantic)

    industry = _explain_rule(
        db, "listing-talent-alice", "listing-company-firma", "industry_alignment_tc"
    )
    assert industry["edge_type"] == "INDUSTRY_ALIGNMENT"
    assert industry["weight"] == pytest.approx(SCORE_INDUSTRY_ALICE_FIRMA, abs=1e-9)
    assert industry["predicate"]["kind"] == "field_equal"
    assert industry["predicate"]["fields"] == ["industry"]

    specialty = _explain_rule(
        db, "listing-talent-alice", "listing-company-firma", "specialty_match_tc"
    )
    assert specialty["edge_type"] == "SPECIALTY_MATCH"
    assert specialty["weight"] == pytest.approx(SCORE_SPECIALTY_ALICE_FIRMA, abs=1e-9)
    assert specialty["predicate"]["kind"] == "overlap"
    assert specialty["predicate"]["fields"] == ["specialties"]
    assert specialty["predicate"]["min"] == pytest.approx(0.15)

    size = _explain_rule(
        db, "listing-talent-alice", "listing-company-firma", "similar_size_tc"
    )
    assert size["edge_type"] == "SIMILAR_SIZE"
    assert size["weight"] == pytest.approx(SCORE_SIZE_ALICE_FIRMA, abs=1e-9)
    assert size["predicate"]["kind"] == "numeric_within"
    assert size["predicate"]["fields"] == ["size_bucket"]
    assert size["predicate"]["tolerance"] == pytest.approx(1.0)
    size_b = _explain_rule(
        db, "listing-talent-alice", "listing-company-firmb", "similar_size_tc"
    )
    assert size_b["weight"] == pytest.approx(SCORE_SIZE_ALICE_FIRMB, abs=1e-9)

    sem_pair, sem_score = next(iter(semantic.items()))
    sem = _explain_rule(db, sem_pair[0], sem_pair[1], "semantic_match_tc")
    assert sem["edge_type"] == "SEMANTIC_MATCH"
    assert sem["weight"] == pytest.approx(sem_score, abs=1e-9)
    assert sem["predicate"]["kind"] == "vector_similar"
    assert sem["predicate"]["fields"] == ["embedding"]
    assert sem["predicate"]["min"] == pytest.approx(0.85)

    # LOCATION_FIT / MATCHES_DESIGN_STYLE: fixtures have no geo / no styles.
    why = db.explain("listing-talent-alice", "listing-company-firma")
    assert not any(e["rule"].startswith("location_fit") for e in why)
    assert not any(e["rule"].startswith("matches_design_style") for e in why)

    db.close()
