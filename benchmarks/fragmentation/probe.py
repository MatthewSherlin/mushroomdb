#!/usr/bin/env python3
"""The fragmentation probe — spec §8.2, pre-registered in PREREGISTRATION.md.

How much rule accuracy is lost when one entity's properties are split across
several alias nodes instead of living on one? Link-first identity (spec §3.3)
accepts that cost for 0.7; this measures it so 0.8 decides on a number.

Deterministic and model-free. It derives edges with `association/truth.py`,
the pure-Python transcription of the engine's predicates whose cross-check
against the engine has zero disagreements (`association/cross-check.md`), so
no store is built and no engine is run: the question is what the rules'
semantics do to fragmented data, and that is what truth.py computes.

    bindings/python/.venv/bin/python benchmarks/fragmentation/probe.py \
        --out benchmarks/fragmentation/results/$(date -u +%Y%m%dT%H%M%SZ)

Writes `summary.md` into `--out` and prints it.
"""

from __future__ import annotations

import argparse
import random
import sys
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
BENCH = HERE.parent / "agent-tasks"
if str(BENCH) not in sys.path:
    sys.path.insert(0, str(BENCH))

from association.build import task_rules, world   # noqa: E402
from association.truth import derived_edges        # noqa: E402

SEED = 20260910
SCALE = 2000
FRACTIONS = (0.10, 0.25, 0.50)
ALIASES = (2, 3, 5)
REFERENCE = (0.25, 3)
SMALL_LOSS = 0.05

# The fields any rule reads. Each scalar goes to one alias; each element of a
# list goes to one alias. Everything else stays on the canonical alias.
RULE_FIELDS = ("industry", "specialties", "location", "size_bucket", "design_styles")
LIST_FIELDS = {"specialties", "design_styles"}

# The two conjunctive rules this probe adds. Every rule the association world
# loads is a single leaf predicate, and a leaf predicate reads one field — so
# whichever alias holds that field derives the edge, and fragmentation can only
# cost a conjunction of fields split across aliases. Without these the probe
# could not see the cost spec §3.3 describes.
ALL_RULES: list[dict[str, Any]] = [
    {
        "name": "fit_all_tc", "src_label": "Talent", "dst_label": "Company",
        "edge_type": "FIT_ALL",
        "predicate": {"All": [
            {"FieldEqual": {"field": "industry"}},
            {"GeoRadius": {"field": "location", "km": 160.9}},
            {"NumericWithin": {"field": "size_bucket", "tolerance": 1.0}},
        ]},
    },
    {
        "name": "fit_all_tj", "src_label": "Talent", "dst_label": "Job",
        "edge_type": "FIT_ALL",
        "predicate": {"All": [
            {"FieldEqual": {"field": "industry"}},
            {"Overlap": {"field": "specialties", "min": 0.15}},
        ]},
    },
]


def fragment(node: dict, k: int, rng: random.Random) -> list[dict]:
    """`node` as `k` aliases: `~0` is the canonical one, created first."""
    props = node["props"]
    parts: list[dict[str, Any]] = [{} for _ in range(k)]
    for field, value in props.items():
        if field not in RULE_FIELDS:
            parts[0][field] = value
        elif field in LIST_FIELDS:
            for item in value:
                parts[rng.randrange(k)].setdefault(field, []).append(item)
        else:
            parts[rng.randrange(k)][field] = value
    for p in parts[1:]:
        p["name"] = props["name"]
    return [{"key": f"{node['key']}~{i}", "label": node["label"], "props": p}
            for i, p in enumerate(parts)]


def canonical(key: str) -> str:
    return key.split("~", 1)[0]


def is_canonical(key: str) -> bool:
    return "~" not in key or key.endswith("~0")


def pr(predicted: set, truth: set) -> tuple[float, float]:
    hit = len(predicted & truth)
    return (hit / len(truth) if truth else 1.0,
            hit / len(predicted) if predicted else 1.0)


def cell(nodes: list[dict], rules: list[dict], truth: set, fraction: float,
         k: int) -> dict[str, tuple[float, float]]:
    rng = random.Random(f"fragmentation-{SEED}-{fraction}-{k}")
    split: list[dict] = []
    for n in nodes:
        if n["label"] == "Talent" and rng.random() < fraction:
            split.extend(fragment(n, k, rng))
        else:
            split.append(n)
    got = derived_edges(split, rules)
    any_alias = {(t, canonical(s), canonical(d)) for t, s, d in got}
    on_canonical = {(t, canonical(s), canonical(d)) for t, s, d in got if is_canonical(s)}
    all_types = {r["edge_type"] for r in ALL_RULES}
    only = lambda es, conj: {e for e in es if (e[0] in all_types) == conj}  # noqa: E731
    out = {}
    for name, conj in (("leaf", False), ("all", True)):
        out[f"canonical/{name}"] = pr(only(on_canonical, conj), only(truth, conj))
        out[f"any-alias/{name}"] = pr(only(any_alias, conj), only(truth, conj))
    out["canonical/every"] = pr(on_canonical, truth)
    out["any-alias/every"] = pr(any_alias, truth)
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--scale", type=int, default=SCALE)
    a = ap.parse_args()

    nodes = world(SEED, a.scale)["nodes"]
    rules = task_rules() + ALL_RULES
    truth = derived_edges(nodes, rules)
    cells = {(f, k): cell(nodes, rules, truth, f, k) for f in FRACTIONS for k in ALIASES}

    ref = cells[REFERENCE]["canonical/every"][0]
    loss = 1.0 - ref
    verdict = "SMALL" if loss <= SMALL_LOSS else "LARGE"
    lines = [
        "# Fragmentation probe (spec §8.2)",
        "",
        f"World: association, seed {SEED}, scale {a.scale}, day 0 — "
        f"{len(nodes)} nodes, {len(truth)} truth edges over "
        f"{len(rules)} rules ({len(ALL_RULES)} conjunctive, added by this probe).",
        "",
        "## Decision",
        "",
        "Pre-registered in `benchmarks/fragmentation/PREREGISTRATION.md`: the primary "
        "score is recall of the edges the canonical alias holds, over every rule, at "
        f"{REFERENCE[0]:.0%} of Talent split into {REFERENCE[1]} aliases. A loss of at "
        f"most {SMALL_LOSS:.2f} is SMALL — link-only stays; above it is LARGE — the "
        "canonical node is 0.8's headline.",
        "",
        "| reference cell | canonical recall | loss | verdict |",
        "|---|---|---|---|",
        f"| {REFERENCE[0]:.2f} × {REFERENCE[1]} | {ref:.4f} | {loss:.4f} | **{verdict}** |",
        "",
        "## The curve",
        "",
        "Recall / precision. `canonical` counts only edges the canonical alias holds; "
        "`any-alias` counts an edge when any alias of the source holds it — what a reader "
        "following `SAME_AS` would see. `leaf` is the world's nine single-predicate rules, "
        "`all` the two conjunctions.",
        "",
        "| fraction | aliases | canonical every | canonical leaf | canonical all | "
        "any-alias every | any-alias leaf | any-alias all |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for (f, k), c in cells.items():
        fmt = lambda key: f"{c[key][0]:.4f} / {c[key][1]:.4f}"  # noqa: E731
        lines.append(
            f"| {f:.2f} | {k} | {fmt('canonical/every')} | {fmt('canonical/leaf')} | "
            f"{fmt('canonical/all')} | {fmt('any-alias/every')} | "
            f"{fmt('any-alias/leaf')} | {fmt('any-alias/all')} |")
    text = "\n".join(lines) + "\n"
    a.out.mkdir(parents=True, exist_ok=True)
    (a.out / "summary.md").write_text(text)
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
