#!/usr/bin/env python3
"""The association suite's twenty tasks, generated from the brute-force truth.

Four tasks per kind — why, multi-hop, retraction, time travel, visibility —
each one a bounded set question: name the keys, and the grader scores recall
over the truth minus a penalty for every near-miss the answer names.

Two things shape every selection here:

**The answer has to be worth computing.** Nine permissive rules over 2,000
entities derive 1.27 M relationships, so "which companies match this talent"
has hundreds of answers and no arm has to be clever. Every target is chosen so
the answer is between 3 and 40 keys — an intersection of three relationships, a
counterfactual over one company's neighbourhood, a role's slice of it — which
is small enough to write down and large enough that guessing does not get you
there.

**The near-misses have to be real.** Each task carries five forbidden keys the
grader penalises: entities of the same label that fail exactly one of the
predicates the question asks about, or that the role cannot see, or that are
matches *today* but were not on the day the question asks about. They are the
answers a plausible-but-wrong method produces.

The seed fixes all of it: same seed, same twenty tasks.

    python association/build_tasks.py --seed 20260910 --out association/tasks.json
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import sys
from pathlib import Path
from typing import Any, Callable, Iterable, Sequence

HERE = Path(__file__).resolve().parent
if str(HERE.parent) not in sys.path:
    sys.path.insert(0, str(HERE.parent))

from association.build import (                     # noqa: E402
    LAST_DAY, day_date, form_paths, world,
)
from association.truth import (                     # noqa: E402
    derived_edges, edges_of, multihop, partners, state_at,
)
# `association.build` puts `dogfood/` on the path; the specialty vocabulary a
# counterfactual draws from is the generator's own, not a second list.
import synthesize                                   # noqa: E402
SPECIALTIES = tuple(synthesize.SPECIALTIES)

# The one preamble every association prompt carries, the code suite's shape.
PREFIX = ("Answer using only the data in this directory. "
          "Be concise: give the keys asked for, one per line.")

# Said first in every question that is about now rather than about a date: all
# three forms ship day 0 plus a changelog, and "now" is the end of it.
NOW = ("Answer for the data as it stands after every change in the history "
       "has been applied.")
# And said last, because a `set` check penalises a near-miss wherever it turns
# up — including inside an explanation of why it is not an answer.
ONLY = "Name nothing else: no near-misses, no workings."

TRI = ("INDUSTRY_ALIGNMENT", "SPECIALTY_MATCH", "LOCATION_FIT")
TRI_SIZE = ("INDUSTRY_ALIGNMENT", "SIMILAR_SIZE", "LOCATION_FIT")
DUO = ("INDUSTRY_ALIGNMENT", "SPECIALTY_MATCH")
# The four relationship types a why task may ask about. The two size rules are
# left out of every why task on purpose: `SIMILAR_SIZE` is a prefix of
# `SIMILAR_SIZE_STRICT`, and the grader matches values as substrings, so an
# answer naming one would be credited for both. A why pair is chosen with the
# size buckets two apart, which puts both rules out and lets a single
# `SIMILAR_SIZE` forbidden value catch an answer that names either.
WHY_TYPES = ("INDUSTRY_ALIGNMENT", "SPECIALTY_MATCH", "LOCATION_FIT",
             "MATCHES_DESIGN_STYLE")
# The store derives this from a vector the other two forms do not carry, and
# the graph form's README says to ignore it. Naming it is a wrong answer.
NOISE_TYPE = "SEMANTIC_MATCH"
# One word is both a specialty and a design style, so it never becomes a
# forbidden specialty: an answer that named it as a shared *style* would be
# penalised for a specialty it never claimed.
AMBIGUOUS_TERMS = ("industrial",)

FORBIDDEN = 5
MIN_ANSWER, MAX_ANSWER = 3, 40

KINDS = ("why", "multihop", "retraction", "timetravel", "visibility")


# --------------------------------------------------------------------------
# helpers over one world
# --------------------------------------------------------------------------


class Suite:
    """One world, its live state, and the derived edges over it."""

    def __init__(self, seed: int, scale: int) -> None:
        self.seed = seed
        self.world = world(seed, scale)
        self.rules = self.world["rules"]
        self.live = state_at(self.world, LAST_DAY)
        self.edges = derived_edges(self.live, self.rules)
        self.rng = random.Random(f"association-tasks-{seed}")
        self._pairs: dict[tuple[str, str], set[str]] = {}
        for edge_type, src, dst in self.edges:
            self._pairs.setdefault((src, dst), set()).add(edge_type)

    def label(self, key: str) -> str:
        return self.live[key]["label"]

    def props(self, key: str) -> dict[str, Any]:
        return self.live[key]["props"]

    def keys(self, label: str) -> list[str]:
        return sorted(k for k, n in self.live.items() if n["label"] == label)

    def types_between(self, src: str, dst: str) -> set[str]:
        return self._pairs.get((src, dst), set())

    def linked(self, nodes: dict[str, dict], key: str,
               edge_types: Sequence[str], label: str) -> set[str]:
        """Everything of `label` joined to `key` by all of `edge_types`, in
        whatever state `nodes` describes."""
        found = partners(edges_of(nodes, self.rules, key), key, edge_types)
        return {k for k in found if nodes[k]["label"] == label}

    def live_linked(self, key: str, edge_types: Sequence[str],
                    label: str) -> set[str]:
        return self.linked(self.live, key, edge_types, label)


def _task(task_id: int, kind: str, n: int, prompt: str, truth: dict[str, Any],
          values: Iterable[str], forbid: Iterable[str]) -> dict[str, Any]:
    values, forbid = sorted(values), sorted(forbid)
    overlap = set(values) & set(forbid)
    if overlap:
        raise AssertionError(f"task {task_id}: {sorted(overlap)} is both truth "
                             f"and near-miss")
    for a in values + forbid:
        for b in values + forbid:
            if a != b and a.lower() in b.lower():
                raise AssertionError(
                    f"task {task_id}: {a!r} is a substring of {b!r}; the "
                    f"grader matches values as substrings and would credit "
                    f"or penalise one for the other")
    return {
        "id": task_id,
        "key": f"assoc-{kind}-{n}",
        "suite": "association",
        "kind": kind,
        "prompt": prompt,
        "truth": {**truth, "answer": values, "size": len(values),
                  "forbid": forbid},
        "checks": [{"kind": "set", "values": values, "forbid": forbid}],
        "extras": {"kind": "none"},
        "verify": None,
        "full_prompt": f"{PREFIX}\n\n{prompt}",
    }


def _listing(names: Sequence[str]) -> str:
    quoted = [f"`{n}`" for n in names]
    return f"{', '.join(quoted[:-1])} and {quoted[-1]}"


# --------------------------------------------------------------------------
# why: one pair, every relationship and the evidence for it
# --------------------------------------------------------------------------


def why_tasks(s: Suite, n: int = 4) -> list[dict[str, Any]]:
    """Pairs where exactly three of the four non-size rules fire.

    The size buckets are forced two apart so neither size rule fires, and the
    talent and the company are forced to have specialties the other lacks, so
    there is evidence to name and evidence to withhold.
    """
    talents, tasks = s.keys("Talent"), []
    s.rng.shuffle(talents)
    seen: set[str] = set()
    for talent in talents:
        if len(tasks) == n:
            break
        for company in sorted(s.live_linked(talent, ("SPECIALTY_MATCH",), "Company")):
            if company in seen:
                continue
            tp, cp = s.props(talent), s.props(company)
            if abs((tp["size_bucket"] or 0) - (cp["size_bucket"] or 0)) < 2:
                continue                     # a size rule would fire
            types = s.types_between(talent, company)
            held = [t for t in WHY_TYPES if t in types]
            if len(held) != 3:
                continue
            shared = sorted(set(tp["specialties"]) & set(cp["specialties"]))
            theirs = sorted(t for t in set(cp["specialties"]) - set(tp["specialties"])
                            if t not in AMBIGUOUS_TERMS)
            if len(shared) < 2 or len(theirs) < 2:
                continue
            missing = [t for t in WHY_TYPES if t not in held]
            seen.add(company)
            tasks.append(_task(
                len(tasks) + 1, "why", len(tasks) + 1,
                f"{NOW} Why is `{talent}` associated with `{company}`? Name "
                f"every relationship type the rules derive between them, one "
                f"per line, and then the specialties the two of them have in "
                f"common, one per line. {ONLY}",
                {"target": [talent, company], "as_of": day_date(LAST_DAY),
                 "types": held, "shared_specialties": shared},
                held + shared,
                missing + ["SIMILAR_SIZE", NOISE_TYPE] + theirs[:2]))
            break
    if len(tasks) != n:
        raise AssertionError(f"only {len(tasks)} why tasks, wanted {n}")
    return tasks


# --------------------------------------------------------------------------
# multi-hop: companies a crowd of qualifying talents all reach
# --------------------------------------------------------------------------

# (minimum years of experience, minimum number of qualifying talents). Chosen
# against the built world so each answer lands inside 3..40 with at least five
# companies one talent short of the bar — see the report's table.
MULTIHOP_BARS: tuple[tuple[int, int], ...] = ((10, 14), (10, 16), (15, 8), (15, 9))


def multihop_tasks(s: Suite, n: int = 4) -> list[dict[str, Any]]:
    tasks = []
    for i, (years, bar) in enumerate(MULTIHOP_BARS[:n], start=1):
        def qualifies(node: dict[str, Any], years: int = years) -> bool:
            props = node["props"]
            return (props.get("status") == "published"
                    and (props.get("years_of_experience") or 0) >= years)

        def reach(minimum: int) -> set[str]:
            return multihop(s.live, s.edges, dst_label="Company",
                            edge_types=TRI, min_sources=minimum,
                            src_filter=qualifies)

        # A near-miss is a company one qualifying talent short of the bar:
        # same shape of answer, one predicate's worth of evidence missing.
        answer = sorted(reach(bar))
        near = sorted(reach(bar - 1) - set(answer))
        _demand(answer, near, f"multihop years>={years} bar={bar}")
        tasks.append(_task(
            len(tasks) + 5, "multihop", i,
            f"{NOW} Which companies are linked by all three of "
            f"{_listing(TRI)} to at least {bar} different talents whose "
            f"`status` is `published` and whose `years_of_experience` is "
            f"{years} or more? Name the company keys, one per line. {ONLY}",
            {"as_of": day_date(LAST_DAY), "edge_types": list(TRI),
             "min_talents": bar, "talent_filter":
                 {"status": "published", "years_of_experience_min": years}},
            answer, s.rng.sample(near, FORBIDDEN)))
    return tasks


# --------------------------------------------------------------------------
# retraction: what a counterfactual would take away
# --------------------------------------------------------------------------


def retraction_tasks(s: Suite, n: int = 4) -> list[dict[str, Any]]:
    """Two specialty rewrites and two resizes, each on a company with a big
    enough neighbourhood that the counterfactual splits it."""
    tasks: list[dict[str, Any]] = []
    companies = s.keys("Company")
    s.rng.shuffle(companies)
    plans = [("specialties", TRI), ("specialties", TRI), ("size_bucket", TRI_SIZE)]
    plans.append(("size_bucket", TRI_SIZE))
    used: set[str] = set()
    for field, types in plans[:n]:
        picked = None
        for company in companies:
            if company in used:
                continue
            base = s.live_linked(company, types, "Talent")
            if not 12 <= len(base) <= 45:
                continue
            picked = _counterfactual(s, company, field, types, base)
            if picked is not None:
                used.add(company)
                break
        if picked is None:
            raise AssertionError(f"no {field} counterfactual left to pick")
        company, value, lost, kept = picked
        shown = json.dumps(value) if isinstance(value, list) else value
        tasks.append(_task(
            len(tasks) + 9, "retraction", len(tasks) + 1,
            f"{NOW} `{company}` is linked to a number of talents by all three "
            f"of {_listing(types)}. Suppose its `{field}` became {shown} and "
            f"nothing else about the data changed. Which of those talents "
            f"would no longer be linked to it by all three? Name the talent "
            f"keys, one per line. {ONLY}",
            {"as_of": day_date(LAST_DAY), "target": company,
             "edge_types": list(types), "field": field, "value": value,
             "linked_before": len(lost) + len(kept)},
            sorted(lost), s.rng.sample(sorted(kept), FORBIDDEN)))
    return tasks


def _counterfactual(s: Suite, company: str, field: str,
                    types: Sequence[str], base: set[str]):
    """A value for `field` that costs `company` some of `base` but not all."""
    nodes = dict(s.live)
    if field == "specialties":
        options = [sorted(s.rng.sample(SPECIALTIES, s.rng.randint(2, 4)))
                   for _ in range(8)]
    else:
        current = s.props(company).get("size_bucket")
        options = [b for b in (1, 2, 3, 4, 5) if b != current]
    for value in options:
        nodes[company] = {**s.live[company],
                          "props": {**s.props(company), field: value}}
        after = s.linked(nodes, company, types, "Talent")
        lost, kept = base - after, base & after
        if MIN_ANSWER <= len(lost) <= MAX_ANSWER and len(kept) >= FORBIDDEN:
            return company, value, lost, kept
    return None


# --------------------------------------------------------------------------
# time travel: the answer on a day, not the answer today
# --------------------------------------------------------------------------


def timetravel_tasks(s: Suite, n: int = 4) -> list[dict[str, Any]]:
    """The day before a talent's own industry, specialties or location moved.

    That is what makes the date load-bearing: the same question asked about
    today has a materially different answer, and the companies that are
    matches only today are exactly the near-misses.
    """
    tasks: list[dict[str, Any]] = []
    edits = [c for c in s.world["changes"]
             if c["op"] == "set_prop" and c["key"].startswith("talent-")
             and c["field"] in ("industry", "specialties", "location")
             and c["day"] >= 3]
    s.rng.shuffle(edits)
    used: set[str] = set()
    for change in edits:
        if len(tasks) == n:
            break
        talent, day = change["key"], change["day"] - 1
        if talent in used or talent not in s.live:
            continue
        nodes = state_at(s.world, day)
        if talent not in nodes:
            continue
        then = s.linked(nodes, talent, TRI, "Company")
        now = s.live_linked(talent, TRI, "Company")
        only_now = sorted(now - then)
        if not MIN_ANSWER <= len(then) <= MAX_ANSWER or len(only_now) < FORBIDDEN:
            continue
        used.add(talent)
        tasks.append(_task(
            len(tasks) + 13, "timetravel", len(tasks) + 1,
            f"On {day_date(day)}, which companies was `{talent}` linked to by "
            f"all three of {_listing(TRI)}? The answer is the one for that "
            f"date, not the one for the end of the history. Name the company "
            f"keys, one per line. {ONLY}",
            {"as_of": day_date(day), "day": day, "target": talent,
             "edge_types": list(TRI), "moved_next_day": change["field"],
             "linked_today": len(now)},
            sorted(then), s.rng.sample(only_now, FORBIDDEN)))
    if len(tasks) != n:
        raise AssertionError(f"only {len(tasks)} time-travel tasks, wanted {n}")
    return tasks


# --------------------------------------------------------------------------
# visibility: the same neighbourhood, through a role
# --------------------------------------------------------------------------

VISIBILITY_ROLE = "recruiter"
VISIBILITY_COMBOS: tuple[tuple[str, ...], ...] = (DUO, DUO, TRI, TRI)


def visibility_tasks(s: Suite, n: int = 4) -> list[dict[str, Any]]:
    """`recruiter` sees Talent and Job, so a talent's company matches are
    invisible to it however strongly they match — which makes them the
    near-misses. A relationship is visible only when both of its ends are."""
    labels = set(next(r["labels"] for r in s.world["roles"]
                      if r["name"] == VISIBILITY_ROLE))
    assert "Job" in labels and "Company" not in labels, labels
    talents = s.keys("Talent")
    s.rng.shuffle(talents)
    tasks: list[dict[str, Any]] = []
    used: set[str] = set()
    for combo in VISIBILITY_COMBOS[:n]:
        picked = None
        for talent in talents:
            if talent in used:
                continue
            jobs = s.live_linked(talent, combo, "Job")
            hidden = s.live_linked(talent, combo, "Company")
            if MIN_ANSWER <= len(jobs) <= MAX_ANSWER and len(hidden) >= FORBIDDEN:
                picked = (talent, sorted(jobs), sorted(hidden))
                used.add(talent)
                break
        if picked is None:
            raise AssertionError(f"no visibility target left for {combo}")
        talent, jobs, hidden = picked
        tasks.append(_task(
            len(tasks) + 17, "visibility", len(tasks) + 1,
            f"You are acting as the `{VISIBILITY_ROLE}` role and may name only "
            f"what that role is allowed to see. {NOW} Which entities is "
            f"`{talent}` linked to by "
            f"{'both' if len(combo) == 2 else 'all three of'} "
            f"{_listing(combo)}? Name their keys, one per line. {ONLY}",
            {"as_of": day_date(LAST_DAY), "target": talent,
             "role": VISIBILITY_ROLE, "edge_types": list(combo),
             "hidden_matches": len(hidden)},
            jobs, s.rng.sample(hidden, FORBIDDEN)))
    return tasks


def _demand(answer: Sequence[str], near: Sequence[str], what: str) -> None:
    if not MIN_ANSWER <= len(answer) <= MAX_ANSWER:
        raise AssertionError(f"{what}: {len(answer)} answers, wanted "
                             f"{MIN_ANSWER}..{MAX_ANSWER}")
    if len(near) < FORBIDDEN:
        raise AssertionError(f"{what}: {len(near)} near-misses, wanted "
                             f"{FORBIDDEN}")


# --------------------------------------------------------------------------
# putting the twenty together
# --------------------------------------------------------------------------


BUILDERS: dict[str, Callable[[Suite], list[dict[str, Any]]]] = {
    "why": why_tasks,
    "multihop": multihop_tasks,
    "retraction": retraction_tasks,
    "timetravel": timetravel_tasks,
    "visibility": visibility_tasks,
}


def world_digest(w: dict[str, Any]) -> str:
    """A fingerprint of the world the tasks were sized against, so a rebuild
    that moves a fact is caught rather than silently regrading."""
    blob = json.dumps({"nodes": w["nodes"], "changes": w["changes"]},
                      sort_keys=True, default=str).encode()
    return hashlib.sha256(blob).hexdigest()[:16]


def build(seed: int, scale: int) -> dict[str, Any]:
    s = Suite(seed, scale)
    tasks: list[dict[str, Any]] = []
    for kind in KINDS:
        tasks.extend(BUILDERS[kind](s))
    assert len(tasks) == 20, len(tasks)
    assert [t["id"] for t in tasks] == list(range(1, 21)), [t["id"] for t in tasks]
    return {
        "suite": "association",
        "seed": seed,
        "scale": scale,
        "day_zero": day_date(0),
        "last_day": day_date(LAST_DAY),
        "world_digest": world_digest(s.world),
        "prefix": PREFIX,
        "tasks": tasks,
    }


def verify_against_store(data: dict[str, Any], build_dir: Path) -> list[str]:
    """Re-ask the engine every pairwise claim the tasks make about today.

    `explain` is a millisecond, so every why pair and every key a visibility
    task names — answer and near-miss alike — is checked against the store the
    graph arm will actually be given. The set-shaped kinds are covered by
    `truth.cross_check`, not here.
    """
    from mushroomdb import GraphDb
    from association.build import STORE_NAME

    problems: list[str] = []
    store = form_paths(build_dir)["graph"] / STORE_NAME
    db = GraphDb.open(str(store), read_only=True)
    try:
        for task in data["tasks"]:
            if task["kind"] == "why":
                a, b = task["truth"]["target"]
                want = set(task["truth"]["types"])
                # Every type but the store-only noise one: a why pair is
                # picked so no size rule fires, so the store must agree that
                # none does.
                got = {r["edge_type"] for r in db.explain(a, b)
                       if r["edge_type"] != NOISE_TYPE}
                if want != got:
                    problems.append(f"task {task['id']} why {a} {b}: truth "
                                    f"{sorted(want)} != store {sorted(got)}")
            elif task["kind"] == "visibility":
                target = task["truth"]["target"]
                want = set(task["truth"]["edge_types"])
                for key in task["truth"]["answer"]:
                    got = {r["edge_type"] for r in db.explain(target, key)}
                    if not want <= got:
                        problems.append(
                            f"task {task['id']} visibility {target} {key}: "
                            f"store has {sorted(got)}, wanted {sorted(want)}")
                for key in task["truth"]["forbid"]:
                    got = {r["edge_type"] for r in db.explain(target, key)}
                    if not want <= got:
                        problems.append(
                            f"task {task['id']} near-miss {target} {key} is "
                            f"not the near-miss it claims: store has "
                            f"{sorted(got)}")
    finally:
        db.close()
    return problems


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--seed", type=int, default=20260910)
    ap.add_argument("--scale", type=int, default=2000)
    ap.add_argument("--out", type=Path, default=HERE / "tasks.json")
    ap.add_argument("--build", type=Path, default=None,
                    help="a build.py --out directory; re-asks the engine every "
                         "pairwise claim the tasks make")
    args = ap.parse_args(argv)

    data = build(args.seed, args.scale)
    if args.build is not None:
        problems = verify_against_store(data, args.build)
        for line in problems:
            print(f"  store disagrees: {line}")
        if problems:
            return 1
        print("store agrees with every pairwise claim")
    args.out.write_text(json.dumps(data, indent=2) + "\n")
    print(f"wrote {args.out} (world {data['world_digest']})")
    for task in data["tasks"]:
        check = task["checks"][0]
        print(f"  {task['id']:2d} {task['kind']:11s} {task['key']:18s} "
              f"answer={len(check['values']):3d} forbid={len(check['forbid'])}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
