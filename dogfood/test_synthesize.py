"""Seeded synthesizer: determinism, distributions, geo/style negatives.

Industry / specialty / style enums mirror the matching workload schema
(13 specialty values, 23 design-style values) — not imported
from synthesize.py, so a typo in the generator cannot echo-pass.

Cosine and haversine oracles below are independent reimplementations
(no import of transform.cosine). Engine GEO/STYLE assertions use
rules.SIX_RULES against a handful of generated nodes — the first real
validation of those two kinds (T1 fixtures carry neither lat/lon nor
design_styles).
"""

from __future__ import annotations

import math
import time
from collections import Counter
from itertools import islice

import pytest
from mushroomdb import GraphDb

from rules import SIX_RULES
from synthesize import (
    DEFAULT_N_COMPANIES,
    DEFAULT_N_JOBS,
    DEFAULT_N_TALENT,
    generate,
)

# Matching workload specialty values — independent of synthesize.py.
SPECIALTIES = {
    "single-family",
    "multi-family",
    "residential",
    "hospitality",
    "retail",
    "healthcare",
    "large-scale",
    "civic",
    "industrial",
    "landscape",
    "commercial",
    "educational",
    "institutional",
}

# Matching workload design-style values — independent of synthesize.py.
DESIGN_STYLES = {
    "bohemian",
    "bright",
    "classic",
    "coastal",
    "colorful",
    "cottage",
    "contemporary",
    "country",
    "eclectic",
    "farmhouse",
    "glamorous",
    "industrial",
    "mediterranean",
    "mid-century",
    "minimal",
    "modern",
    "rustic",
    "scandinavian",
    "southern",
    "sustainable",
    "transitional",
    "traditional",
    "preppy",
}

# Brief names 10 cities and asks for 12 centers. Denver + Atlanta complete a
# well-separated set: every inter-metro haversine is >> 160.9 km even after
# 30 km jitter (closest named pair is NYC–Boston ≈ 306 km).
METRO_CENTERS = {
    "NYC": (40.7128, -74.0060),
    "LA": (34.0522, -118.2437),
    "SF": (37.7749, -122.4194),
    "Chicago": (41.8781, -87.6298),
    "Miami": (25.7617, -80.1918),
    "Austin": (30.2672, -97.7431),
    "Seattle": (47.6062, -122.3321),
    "Boston": (42.3601, -71.0589),
    "London": (51.5074, -0.1278),
    "Paris": (48.8566, 2.3522),
    "Denver": (39.7392, -104.9903),
    "Atlanta": (33.7490, -84.3880),
}

LOCATION_FIT_KM = 160.9
JITTER_KM = 30.0
EMBED_DIM = 1536
INDUSTRIES = {"architecture", "interior-design", "both"}

# WGS-84 authalic mean radius — independent copy of the engine constant.
_EARTH_RADIUS_KM = 6371.0088

SEED = 20260819


def _cosine_oracle(a: list[float], b: list[float]) -> float | None:
    """Plain-Python cosine — independent of transform.cosine."""
    if len(a) != len(b) or not a:
        return None
    dot = na2 = nb2 = 0.0
    for x, y in zip(a, b):
        dot += x * y
        na2 += x * x
        nb2 += y * y
    na = math.sqrt(na2)
    nb = math.sqrt(nb2)
    if not (na > 0.0 and nb > 0.0):
        return None
    c = dot / (na * nb)
    return min(1.0, c) if math.isfinite(c) else None


def _haversine_km(lat1: float, lon1: float, lat2: float, lon2: float) -> float:
    """Independent haversine (same formula as the engine, not imported)."""
    phi1 = math.radians(lat1)
    phi2 = math.radians(lat2)
    dphi = math.radians(lat2 - lat1)
    dlam = math.radians(lon2 - lon1)
    a = math.sin(dphi / 2.0) ** 2 + math.cos(phi1) * math.cos(phi2) * math.sin(dlam / 2.0) ** 2
    a = min(1.0, max(0.0, a))
    c = 2.0 * math.atan2(math.sqrt(a), math.sqrt(1.0 - a))
    return _EARTH_RADIUS_KM * c


def _nearest_metro(lat: float, lon: float) -> tuple[str, float]:
    best_name = ""
    best_d = float("inf")
    for name, (mlat, mlon) in METRO_CENTERS.items():
        d = _haversine_km(lat, lon, mlat, mlon)
        if d < best_d:
            best_name, best_d = name, d
    return best_name, best_d


def _loc(node: dict) -> tuple[float, float]:
    pair = node["props"]["location"]
    return float(pair[0]), float(pair[1])


def _jaccard(a: list[str], b: list[str]) -> float | None:
    sa, sb = set(a), set(b)
    union = sa | sb
    if not union:
        return None
    return len(sa & sb) / len(union)


def _by_label(nodes: list[dict]) -> dict[str, list[dict]]:
    out: dict[str, list[dict]] = {"Talent": [], "Company": [], "Job": []}
    for n in nodes:
        out[n["label"]].append(n)
    return out


@pytest.fixture(scope="module")
def sample() -> list[dict]:
    return list(generate(2400, 600, 240, SEED))


@pytest.fixture(scope="module")
def labeled(sample: list[dict]) -> dict[str, list[dict]]:
    return _by_label(sample)


def test_default_scale_is_100k():
    assert DEFAULT_N_TALENT == 70_000
    assert DEFAULT_N_COMPANIES == 20_000
    assert DEFAULT_N_JOBS == 10_000
    assert DEFAULT_N_TALENT + DEFAULT_N_COMPANIES + DEFAULT_N_JOBS == 100_000


def test_generate_is_iterator_of_node_dicts():
    it = generate(2, 2, 2, SEED)
    assert iter(it) is it or hasattr(it, "__next__")
    first = next(iter(generate(2, 2, 2, SEED)))
    assert set(first) == {"key", "label", "props"}
    assert isinstance(first["key"], str) and first["key"]
    assert first["label"] in {"Talent", "Company", "Job"}
    assert isinstance(first["props"], dict)


def test_generate_counts_and_yield_order():
    nodes = list(generate(3, 2, 1, SEED))
    assert [n["label"] for n in nodes] == ["Talent", "Talent", "Talent", "Company", "Company", "Job"]
    assert len({n["key"] for n in nodes}) == 6


def test_determinism_same_seed_first_100():
    a = list(islice(generate(80, 15, 10, SEED), 100))
    b = list(islice(generate(80, 15, 10, SEED), 100))
    assert len(a) == 100
    assert a == b


def test_different_seed_diverges():
    a = list(islice(generate(20, 5, 5, SEED), 20))
    b = list(islice(generate(20, 5, 5, SEED + 1), 20))
    assert a != b


def test_industry_ratios_within_2_percent(labeled):
    talent = labeled["Talent"]
    counts = Counter(n["props"]["industry"] for n in talent)
    assert set(counts) <= INDUSTRIES
    n = len(talent)
    assert counts["architecture"] / n == pytest.approx(0.45, abs=0.02)
    assert counts["interior-design"] / n == pytest.approx(0.45, abs=0.02)
    assert counts["both"] / n == pytest.approx(0.10, abs=0.02)
    for group in labeled["Company"], labeled["Job"]:
        c = Counter(n["props"]["industry"] for n in group)
        m = len(group)
        assert c["architecture"] / m == pytest.approx(0.45, abs=0.02)
        assert c["interior-design"] / m == pytest.approx(0.45, abs=0.02)
        assert c["both"] / m == pytest.approx(0.10, abs=0.02)


def test_specialty_enum_primary_plus_0_to_3_secondary(labeled):
    seen: set[str] = set()
    for group in labeled.values():
        for n in group:
            specs = n["props"]["specialties"]
            assert isinstance(specs, list) and specs
            assert 1 <= len(specs) <= 4
            assert len(specs) == len(set(specs))
            assert set(specs) <= SPECIALTIES
            seen.update(specs)
    assert seen == SPECIALTIES


def test_design_styles_enum_and_0_to_5_cardinality(labeled):
    seen: set[str] = set()
    lengths: set[int] = set()
    for group in labeled.values():
        for n in group:
            styles = n["props"]["design_styles"]
            assert isinstance(styles, list)
            assert 0 <= len(styles) <= 5
            assert len(styles) == len(set(styles))
            assert set(styles) <= DESIGN_STYLES
            seen.update(styles)
            lengths.add(len(styles))
    assert seen == DESIGN_STYLES
    assert 0 in lengths and 5 in lengths


def test_size_bucket_and_experience_tier_cover_1_to_5(labeled):
    talent_buckets = {n["props"]["size_bucket"] for n in labeled["Talent"]}
    company_buckets = {n["props"]["size_bucket"] for n in labeled["Company"]}
    job_buckets = {n["props"]["size_bucket"] for n in labeled["Job"]}
    assert talent_buckets == {1, 2, 3, 4, 5}
    assert company_buckets == {1, 2, 3, 4, 5}
    assert job_buckets == {1, 2, 3, 4, 5}
    for n in labeled["Talent"]:
        years = n["props"]["years_of_experience"]
        bucket = n["props"]["size_bucket"]
        assert isinstance(years, int)
        if bucket == 1:
            assert 0 <= years <= 2
        elif bucket == 2:
            assert 3 <= years <= 5
        elif bucket == 3:
            assert 6 <= years <= 9
        elif bucket == 4:
            assert 10 <= years <= 14
        else:
            assert years >= 15


def test_size_bucket_negative_pairs_exist(labeled):
    """|delta| > 1 must exist so similar_size (tol=1) is not vacuous."""
    t_buckets = {n["props"]["size_bucket"] for n in labeled["Talent"]}
    c_buckets = {n["props"]["size_bucket"] for n in labeled["Company"]}
    assert any(abs(t - c) > 1 for t in t_buckets for c in c_buckets)


def test_founded_year_range_on_companies(labeled):
    years = [n["props"]["founded_year"] for n in labeled["Company"]]
    assert years
    assert all(isinstance(y, int) and 1950 <= y <= 2024 for y in years)
    assert min(years) != max(years)
    for n in labeled["Talent"]:
        assert "founded_year" not in n["props"]


def test_user_id_fk_props(labeled):
    for n in labeled["Talent"]:
        uid = n["props"]["user_id"]
        assert isinstance(uid, str) and uid.startswith("user-")
    talent_ids = {n["props"]["user_id"] for n in labeled["Talent"]}
    assert len(talent_ids) == len(labeled["Talent"])
    for n in labeled["Company"]:
        assert n["props"]["user_id"].startswith("user-")
    for n in labeled["Job"]:
        assert n["props"]["user_id"].startswith("user-")
        assert n["props"]["company_id"].startswith("company-")


def test_geo_cluster_membership(labeled):
    seen: set[str] = set()
    for group in labeled.values():
        for n in group:
            lat, lon = _loc(n)
            name, dist = _nearest_metro(lat, lon)
            assert dist <= JITTER_KM + 5.0, f"{n['key']} {dist:.1f} km from {name}"
            seen.add(name)
    assert seen == set(METRO_CENTERS)


def test_distinct_metros_not_within_location_fit(labeled):
    """Negative geo: at least one distinct-metro pair is > 160.9 km.

    With 30 km jitter around these 12 centers, EVERY distinct-metro pair
    should miss LOCATION_FIT; we assert the existence of at least one and
    sample across all observed metro pairs.
    """
    talent = labeled["Talent"]
    companies = labeled["Company"]
    t_by_metro: dict[str, dict] = {}
    c_by_metro: dict[str, dict] = {}
    for n in talent:
        name, _ = _nearest_metro(*_loc(n))
        t_by_metro.setdefault(name, n)
    for n in companies:
        name, _ = _nearest_metro(*_loc(n))
        c_by_metro.setdefault(name, n)
    far_pairs = 0
    for tm, tnode in t_by_metro.items():
        for cm, cnode in c_by_metro.items():
            if tm == cm:
                continue
            d = _haversine_km(*_loc(tnode), *_loc(cnode))
            assert d > LOCATION_FIT_KM, f"{tm}↔{cm} = {d:.1f} km ≤ {LOCATION_FIT_KM}"
            far_pairs += 1
    assert far_pairs >= 1
    # Named pair from the brief: NYC vs LA must not cluster.
    assert "NYC" in t_by_metro and "LA" in c_by_metro
    nyc_la = _haversine_km(*_loc(t_by_metro["NYC"]), *_loc(c_by_metro["LA"]))
    assert nyc_la > LOCATION_FIT_KM


def test_same_metro_within_location_fit(labeled):
    t_by_metro: dict[str, dict] = {}
    c_by_metro: dict[str, dict] = {}
    for n in labeled["Talent"]:
        name, _ = _nearest_metro(*_loc(n))
        t_by_metro.setdefault(name, n)
    for n in labeled["Company"]:
        name, _ = _nearest_metro(*_loc(n))
        c_by_metro.setdefault(name, n)
    close = 0
    for metro in set(t_by_metro) & set(c_by_metro):
        d = _haversine_km(*_loc(t_by_metro[metro]), *_loc(c_by_metro[metro]))
        assert d <= LOCATION_FIT_KM, f"{metro} intra-cluster {d:.1f} km"
        close += 1
    assert close >= 1


def test_embedding_unit_norm_and_dim(labeled):
    for n in labeled["Talent"] + labeled["Company"]:
        emb = n["props"]["embedding"]
        assert len(emb) == EMBED_DIM
        norm = math.sqrt(sum(x * x for x in emb))
        assert norm == pytest.approx(1.0, abs=1e-9)
    for n in labeled["Job"]:
        assert "embedding" not in n["props"]


def test_similar_profile_cosine_exceeds_distinct(labeled):
    talent = labeled["Talent"]
    similar = None
    distinct = None
    for i, a in enumerate(talent):
        a_ind = a["props"]["industry"]
        a_pri = a["props"]["specialties"][0]
        for b in talent[i + 1 :]:
            b_ind = b["props"]["industry"]
            b_pri = b["props"]["specialties"][0]
            score = _cosine_oracle(a["props"]["embedding"], b["props"]["embedding"])
            assert score is not None
            if similar is None and a_ind == b_ind and a_pri == b_pri:
                similar = score
            if (
                distinct is None
                and a_ind != b_ind
                and a_ind != "both"
                and b_ind != "both"
                and a_pri != b_pri
            ):
                distinct = score
            if similar is not None and distinct is not None:
                break
        if similar is not None and distinct is not None:
            break
    assert similar is not None and distinct is not None
    assert similar > distinct
    assert similar >= 0.85
    assert distinct < 0.5


def test_style_overlap_positive_and_negative_pairs_exist(labeled):
    talent = labeled["Talent"]
    companies = labeled["Company"]
    positive = negative = False
    for t in talent:
        for c in companies:
            j = _jaccard(t["props"]["design_styles"], c["props"]["design_styles"])
            if j is not None and j >= 0.2:
                positive = True
            else:
                negative = True
            if positive and negative:
                return
    assert positive, "no Talent↔Company pair with design_styles Jaccard >= 0.2"
    assert negative, "no Talent↔Company pair below MATCHES_DESIGN_STYLE min=0.2"


def test_geo_and_style_predicates_fire_and_reject(tmp_path, labeled):
    """Engine-level GEO / STYLE: first real validation of those two kinds."""
    talent = labeled["Talent"]
    companies = labeled["Company"]

    t_by_metro: dict[str, dict] = {}
    c_by_metro: dict[str, dict] = {}
    for n in talent:
        t_by_metro.setdefault(_nearest_metro(*_loc(n))[0], n)
    for n in companies:
        c_by_metro.setdefault(_nearest_metro(*_loc(n))[0], n)

    metro = next(m for m in t_by_metro if m in c_by_metro)
    other = next(m for m in c_by_metro if m != metro)
    t_near = t_by_metro[metro]
    c_near = c_by_metro[metro]
    c_far = c_by_metro[other]

    style_pos = style_neg = None
    for t in talent:
        for c in companies:
            if t["key"] in {t_near["key"]} and c["key"] in {c_near["key"], c_far["key"]}:
                continue
            j = _jaccard(t["props"]["design_styles"], c["props"]["design_styles"])
            if style_pos is None and j is not None and j >= 0.2:
                if _nearest_metro(*_loc(t))[0] == _nearest_metro(*_loc(c))[0]:
                    style_pos = (t, c)
            if style_neg is None and (j is None or j < 0.2):
                style_neg = (t, c)
            if style_pos and style_neg:
                break
        if style_pos and style_neg:
            break
    assert style_pos is not None, "need a same-metro style-overlap pair"
    assert style_neg is not None, "need a style-negative pair"

    nodes = [t_near, c_near, c_far, style_pos[0], style_pos[1], style_neg[0], style_neg[1]]
    seen: set[str] = set()
    uniq = []
    for n in nodes:
        if n["key"] not in seen:
            seen.add(n["key"])
            uniq.append(n)

    db = GraphDb.open(str(tmp_path / "db"))
    for item in uniq:
        db.insert_node(item["key"], item["label"], item["props"])
    for rule in SIX_RULES:
        if rule["name"] in {
            "location_fit_tc",
            "matches_design_style_tc",
        }:
            db.create_rule(rule)

    loc_rows = db.query(
        "MATCH (a:Talent)-[:LOCATION_FIT]->(b:Company) RETURN a, b"
    )
    loc_pairs = {(r["a"], r["b"]) for r in loc_rows}
    assert (t_near["key"], c_near["key"]) in loc_pairs
    assert (t_near["key"], c_far["key"]) not in loc_pairs

    style_rows = db.query(
        "MATCH (a:Talent)-[:MATCHES_DESIGN_STYLE]->(b:Company) RETURN a, b"
    )
    style_pairs = {(r["a"], r["b"]) for r in style_rows}
    assert (style_pos[0]["key"], style_pos[1]["key"]) in style_pairs
    assert (style_neg[0]["key"], style_neg[1]["key"]) not in style_pairs

    why_near = db.explain(t_near["key"], c_near["key"])
    assert any(e["rule"] == "location_fit_tc" for e in why_near)
    why_far = db.explain(t_near["key"], c_far["key"])
    assert not any(e["rule"] == "location_fit_tc" for e in why_far)
    db.close()


def test_throughput_10k_under_30s():
    t0 = time.perf_counter()
    n = 0
    for _ in generate(7000, 2000, 1000, SEED):
        n += 1
    elapsed = time.perf_counter() - t0
    assert n == 10_000
    assert elapsed < 30.0, f"10k generate took {elapsed:.2f}s"
