"""The six marketplace matcher rules as mushroomdb RuleDef dicts.

Cross-label: a rule binds one (src_label, dst_label) pair. Industry and
specialty (and location, because jobs carry geo) are declared twice —
Talent→Company and Talent→Job. Size / design-style / semantic are
Talent→Company only. That is 9 rule instances.

`field_equal(industry)` is binary (score 1.0). marketplace scores industry
`both` at 0.8 — documented composition gap, not chased.
"""

from __future__ import annotations

from typing import Any


def _rule(
    name: str,
    src: str,
    dst: str,
    predicate: dict,
    edge_type: str,
) -> dict[str, Any]:
    return {
        "name": name,
        "src_label": src,
        "dst_label": dst,
        "predicate": predicate,
        "edge_type": edge_type,
        "weight_prop": "score",
        "max_edges": None,
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
]
