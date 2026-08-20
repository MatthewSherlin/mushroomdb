"""The six marketplace matcher rules as mushroomdb RuleDef dicts.

Cross-label: a rule binds one (src_label, dst_label) pair. Industry and
specialty (and location, because jobs carry geo) are declared twice —
Talent→Company and Talent→Job. Size / design-style / semantic are
Talent→Company only. That is 9 primary rule instances.

A tenth instance, `similar_size_strict_tc` (tolerance=0), is added as a
negative-case oracle: the verbatim fixture bucket space {2, 3} means
tolerance=1 is vacuously all-match; tolerance=0 produces exactly the 5
same-bucket pairs and provably excludes the 5 cross-bucket pairs.

`field_equal(industry)` is binary (score 1.0). marketplace scores industry
`both` at 0.8 — documented composition gap, not chased.

**max_edges cap (Plan 11 T1):** all cartesian rule instances carry
`max_edges=1_000_000` — the engine default that the original 5k probe
tripped. With T1's streaming backfill the engine no longer builds a
full desired `BTreeMap` before capping; the cap governs only how many
edges are *kept*, not how much memory is used during evaluation.
`APPROXIMATE_SEMANTIC_RULE` is the `approximate=True` variant of
`semantic_match_tc` added in T4.
"""

from __future__ import annotations

from typing import Any

# Consistent cap for all cartesian rule instances at 100k.
# Equal to the engine default (DEFAULT_MAX_EDGES = 1_000_000) so
# the original probe behaviour is reproduced; the explicit value
# documents the choice and lets tests assert it.
MATCHER_MAX_EDGES = 1_000_000


def _rule(
    name: str,
    src: str,
    dst: str,
    predicate: dict,
    edge_type: str,
    max_edges: int | None = MATCHER_MAX_EDGES,
) -> dict[str, Any]:
    return {
        "name": name,
        "src_label": src,
        "dst_label": dst,
        "predicate": predicate,
        "edge_type": edge_type,
        "weight_prop": "score",
        "max_edges": max_edges,
    }


SIX_RULES: list[dict[str, Any]] = [
    _rule(
        "industry_alignment_tc",
        "Talent",
        "Company",
        {"FieldEqual": {"field": "industry"}},
        "INDUSTRY_ALIGNMENT",
    ),
    _rule(
        "industry_alignment_tj",
        "Talent",
        "Job",
        {"FieldEqual": {"field": "industry"}},
        "INDUSTRY_ALIGNMENT",
    ),
    _rule(
        "specialty_match_tc",
        "Talent",
        "Company",
        {"Overlap": {"field": "specialties", "min": 0.15}},
        "SPECIALTY_MATCH",
    ),
    _rule(
        "specialty_match_tj",
        "Talent",
        "Job",
        {"Overlap": {"field": "specialties", "min": 0.15}},
        "SPECIALTY_MATCH",
    ),
    _rule(
        "location_fit_tc",
        "Talent",
        "Company",
        {"GeoRadius": {"field": "location", "km": 160.9}},
        "LOCATION_FIT",
    ),
    _rule(
        "location_fit_tj",
        "Talent",
        "Job",
        {"GeoRadius": {"field": "location", "km": 160.9}},
        "LOCATION_FIT",
    ),
    _rule(
        "similar_size_tc",
        "Talent",
        "Company",
        {"NumericWithin": {"field": "size_bucket", "tolerance": 1.0}},
        "SIMILAR_SIZE",
    ),
    _rule(
        "matches_design_style_tc",
        "Talent",
        "Company",
        {"Overlap": {"field": "design_styles", "min": 0.2}},
        "MATCHES_DESIGN_STYLE",
    ),
    _rule(
        "semantic_match_tc",
        "Talent",
        "Company",
        {"VectorSimilar": {"field": "embedding", "min": 0.85}},
        "SEMANTIC_MATCH",
    ),
    # Negative-case oracle: tolerance=0 fires only on exact-bucket pairs.
    # With verbatim fixture buckets {2, 3} the 5 cross-bucket pairs are provably
    # absent, giving a genuine false-positive guard for numeric_within.
    _rule(
        "similar_size_strict_tc",
        "Talent",
        "Company",
        {"NumericWithin": {"field": "size_bucket", "tolerance": 0.0}},
        "SIMILAR_SIZE_STRICT",
    ),
]

# Approximate semantic rule (Plan 11 T4): IVF-Flat candidate selection.
# `approximate=True` is only valid with a VectorSimilar-rooted predicate.
# Recall ≥ 0.90 quiesced per the engine spec (not exact by definition).
# Named differently from `semantic_match_tc` so both can coexist in the
# same graph if needed; the scale_run uses this for the 100k phase and
# separately probes the exact rule on a 5k subset.
APPROXIMATE_SEMANTIC_RULE: dict[str, Any] = {
    "name": "semantic_match_approx_tc",
    "src_label": "Talent",
    "dst_label": "Company",
    "predicate": {"VectorSimilar": {"field": "embedding", "min": 0.85}},
    "edge_type": "SEMANTIC_MATCH_APPROX",
    "weight_prop": "score",
    "max_edges": MATCHER_MAX_EDGES,
    "approximate": True,
}
