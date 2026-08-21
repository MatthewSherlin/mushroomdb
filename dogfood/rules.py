"""Six matching rules for the representative workload, as mushroomdb RuleDef dicts.

Cross-label: a rule binds one (src_label, dst_label) pair. Industry and
specialty (and location, because jobs carry geo) are declared twice —
Talent→Company and Talent→Job. Size / design-style / semantic are
Talent→Company only. That is 9 primary rule instances.

A tenth instance, `similar_size_strict_tc` (tolerance=0), is added as a
negative-case oracle: the verbatim fixture bucket space {2, 3} means
tolerance=1 is vacuously all-match; tolerance=0 produces exactly the 5
same-bucket pairs and provably excludes the 5 cross-bucket pairs.

`field_equal(industry)` is binary (score 1.0). An alternate scoring convention
weights industry `both` at 0.8 — documented composition gap, not chased.

**max_edges semantics (V5 note):** cartesian rule instances use
`max_edges=None` to engage the global-budget path (DEFAULT_MAX_EDGES=1M
global cap, tripped latch). In V5 the engine added per-source top-k
semantics: `max_edges=Some(k)` now means each source gets up to k
destination edges. For dogfood rules where a single industry can span
9k companies at 100k scale, a per-source cap of 1M would materialize
all 9k edges per talent (283M+ total for FieldEqual), causing OOM.
The global-budget path (max_edges=None) preserves the V4 behavior:
a global 1M cap is applied in BTree order across all source nodes.
`APPROXIMATE_SEMANTIC_RULE` is the `approximate=True` variant of
`semantic_match_tc` added in T4.
"""

from __future__ import annotations

from typing import Any

# Global 1M cap for approximate-recall reporting in the backfill summary.
# This is the count the global-budget path will produce (it hits the cap).
MATCHER_MAX_EDGES = 1_000_000


def _rule(
    name: str,
    src: str,
    dst: str,
    predicate: dict,
    edge_type: str,
    max_edges: int | None = None,  # None = global-budget path (DEFAULT_MAX_EDGES=1M cap)
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
    "max_edges": None,  # None = global-budget path (DEFAULT_MAX_EDGES=1M cap)
    # V5 note: max_edges=Some(k) is now per-source top-k. For VectorSimilar at
    # 100k scale, IVF returns ~160 qualifying Companies per Talent (above 0.85
    # threshold). Per-source cap of 1M is never hit, materializing 11M+ edges
    # and taking 90+ min. Use None to keep the global 1M cap (same as V4).
    "approximate": True,
}
