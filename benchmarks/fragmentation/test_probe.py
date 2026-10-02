"""The fragmentation probe's own checks, at scale 200 — seconds, not minutes.

    bindings/python/.venv/bin/python -m pytest benchmarks/fragmentation/test_probe.py -q
"""

from __future__ import annotations

import random
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import probe  # noqa: E402
from association.truth import derived_edges  # noqa: E402

NODES = probe.world(probe.SEED, 200)["nodes"]
RULES = probe.task_rules() + probe.ALL_RULES
TRUTH = derived_edges(NODES, RULES)


def test_splitting_nothing_loses_nothing():
    c = probe.cell(NODES, RULES, TRUTH, 0.0, 3)
    assert all(score == (1.0, 1.0) for score in c.values()), c


def test_a_cell_is_the_same_every_time():
    assert probe.cell(NODES, RULES, TRUTH, 0.25, 3) == probe.cell(NODES, RULES, TRUTH, 0.25, 3)


def test_the_conjunctions_derive_edges_to_lose():
    conj = {r["edge_type"] for r in probe.ALL_RULES}
    assert any(e[0] in conj for e in TRUTH), "the All rules derive nothing; the probe would be blind"


def test_fragmenting_keeps_every_rule_value_somewhere():
    talent = next(n for n in NODES if n["label"] == "Talent")
    parts = probe.fragment(talent, 3, random.Random(1))
    assert [p["key"] for p in parts] == [f"{talent['key']}~{i}" for i in range(3)]
    for field in probe.RULE_FIELDS:
        if field not in talent["props"]:
            continue
        if field in probe.LIST_FIELDS:
            held = sorted(v for p in parts for v in p["props"].get(field, []))
            assert held == sorted(talent["props"][field]), field
        else:
            assert sum(field in p["props"] for p in parts) == 1, field


def test_fragmenting_costs_the_canonical_alias_recall():
    # A blind probe (canonical score == any-alias score) fails the strict
    # inequality; at fraction 0.5, k = 3 a rule's field leaves ~0 about 2/3 of
    # the time, so the canonical alias must lose edges the whole entity holds.
    c = probe.cell(NODES, RULES, TRUTH, 0.5, 3)
    assert c["canonical/leaf"][0] < 1.0, c
    assert c["canonical/every"][0] < c["any-alias/every"][0], c
    assert c["any-alias/every"][0] <= 1.0, c
