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
    LAST_DAY, day_date, form_paths, task_rules, world,
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
# Every relationship a task may ask about. The store also derives a
# `SEMANTIC_MATCH` from a vector the other two forms do not carry; it is never
# a truth value and never a forbidden one — penalising it would cost the graph
# arm for reading its own data and no other arm anything — so comparisons
# against the store intersect with this set and the extra type falls away.
TASK_EDGE_TYPES = frozenset(r["edge_type"] for r in task_rules())
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
    """One world, its live state, and the derived edges over it.

    `avoid` holds entity keys no task may be built around. It is how a task
    whose truth the engine disputes gets dropped: the disagreement is filed,
    its target goes into `avoid`, and the next candidate takes its place.
    """

    def __init__(self, seed: int, scale: int,
                 avoid: frozenset[str] = frozenset()) -> None:
        self.seed = seed
        self.avoid = avoid
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
    if len(forbid) != FORBIDDEN:
        raise AssertionError(f"task {task_id}: {len(forbid)} near-misses, "
                             f"every task carries {FORBIDDEN}")
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
    pair is forced to have at least three specialties that only one of the two
    carries. Those are the near-misses: a specialty one side has and the other
    does not is exactly the one-predicate error a careless reader makes.
    Company-only ones are preferred; the generator gives an entity at most
    four specialties, so a pair sharing two cannot have three company-only
    ones as well, and the talent's own unshared specialties top the count up.
    """
    talents, tasks = s.keys("Talent"), []
    s.rng.shuffle(talents)
    seen: set[str] = set()
    for talent in talents:
        if len(tasks) == n:
            break
        if talent in s.avoid:
            continue
        for company in sorted(s.live_linked(talent, ("SPECIALTY_MATCH",), "Company")):
            if company in seen or company in s.avoid:
                continue
            tp, cp = s.props(talent), s.props(company)
            if abs((tp["size_bucket"] or 0) - (cp["size_bucket"] or 0)) < 2:
                continue                     # a size rule would fire
            types = s.types_between(talent, company)
            held = [t for t in WHY_TYPES if t in types]
            if len(held) != 3:
                continue
            mine_set, theirs_set = set(tp["specialties"]), set(cp["specialties"])
            shared = sorted(mine_set & theirs_set)
            unshared = lambda a, b: sorted(     # noqa: E731
                t for t in a - b if t not in AMBIGUOUS_TERMS)
            theirs, mine = unshared(theirs_set, mine_set), unshared(mine_set, theirs_set)
            near_specialties = (theirs + mine)[:3]
            if len(shared) < 2 or len(near_specialties) < 3:
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
                 "types": held, "shared_specialties": shared,
                 "unshared_specialties": near_specialties},
                held + shared,
                # The one relationship of the four that does not hold, the
                # size family (the buckets are two apart, so a single
                # `SIMILAR_SIZE` catches an answer naming either size rule),
                # and three specialties only one of the two carries.
                missing + ["SIMILAR_SIZE"] + near_specialties))
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


# Two specialty rewrites and two resizes: an `Overlap` retraction and a
# `NumericWithin` one, so the two predicates that can partially retract a
# neighbourhood both get asked about. `industry` and `location` are not here
# because changing either takes the *whole* neighbourhood, which asks nothing.
RETRACTION_PLANS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("specialties", TRI),
    ("specialties", TRI),
    ("size_bucket", TRI_SIZE),
    ("size_bucket", TRI_SIZE),
)


def retraction_tasks(s: Suite, n: int = 4) -> list[dict[str, Any]]:
    """Each on a company with a neighbourhood big enough that the
    counterfactual splits it rather than emptying it."""
    tasks: list[dict[str, Any]] = []
    companies = s.keys("Company")
    s.rng.shuffle(companies)
    used: set[str] = set()
    for field, types in RETRACTION_PLANS[:n]:
        picked = None
        for company in companies:
            if company in used or company in s.avoid:
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
            # `base` is the neighbourhood before the counterfactual — the one
            # part of a retraction task the engine can be asked about, and
            # `verify_against_store` asks it with `explain`.
            {"as_of": day_date(LAST_DAY), "target": company,
             "edge_types": list(types), "field": field, "value": value,
             "base": sorted(lost | kept), "linked_before": len(lost) + len(kept)},
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
        if talent in used or talent in s.avoid or talent not in s.live:
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
            if talent in used or talent in s.avoid:
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


def build(seed: int, scale: int,
          avoid: frozenset[str] = frozenset()) -> dict[str, Any]:
    s = Suite(seed, scale, avoid)
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


def task_targets(task: dict[str, Any]) -> set[str]:
    """The entity keys a task is built around — what goes into `avoid` when
    the engine disputes its truth."""
    target = task["truth"].get("target")
    if target is None:
        return set()
    return set(target) if isinstance(target, list) else {target}


def verify_against_store(data: dict[str, Any], build_dir: Path,
                         progress: bool = False) -> tuple[list[str], set[int]]:
    """Re-ask the engine every claim the tasks make that it can be asked.

    Four of the five kinds carry a claim the engine can answer directly:

    - **why** — `explain(a, b)` must name exactly the truth's relationships.
    - **visibility** — `explain` on every answer key and every near-miss key.
    - **time travel** — `was_linked(a, b, type, commit_of_day)` on every
      (answer, relationship) pair, which must all be true, and on the
      near-misses, at least one of whose relationships must be false at that
      date (a near-miss is "not linked by all of them then", not "linked by
      none of them").
    - **retraction** — `explain` on the whole neighbourhood as it stands
      before the counterfactual. The counterfactual world itself has no
      commit, so the engine cannot be asked about it.

    **Multi-hop stays Python-only.** Its claim is a count over the whole
    2,000-node world — for each company, how many qualifying talents reach it
    by all three relationships — and there is no engine call that answers it
    without either a `query_at` replay (80-90 s each) or ~840,000 `explain`
    calls. `truth.cross_check`'s 200 sampled `explain` probes are what stands
    behind it.

    Returns the disagreements and the ids of the tasks that produced them.
    """
    from mushroomdb import GraphDb
    from association.build import STORE_NAME

    problems: list[str] = []
    bad: set[int] = set()
    graph = form_paths(build_dir)["graph"]
    commit_of = {e["day"]: e["commit"]
                 for e in json.loads((graph / "days.json").read_text())}

    def fail(task: dict[str, Any], line: str) -> None:
        problems.append(f"task {task['id']} ({task['key']}): {line}")
        bad.add(task["id"])

    db = GraphDb.open(str(graph / STORE_NAME), read_only=True)
    try:
        for task in data["tasks"]:
            kind, truth = task["kind"], task["truth"]
            if progress:
                print(f"  verifying {task['key']}", flush=True)

            if kind == "why":
                a, b = truth["target"]
                want = set(truth["types"])
                # Intersecting with the nine task rules' types is what drops
                # the store-only SEMANTIC_MATCH: it is not a type any task
                # asks about, so the store having it is neither agreement nor
                # disagreement. A why pair is picked with the size buckets two
                # apart, so the store must also agree no size rule fires.
                got = {r["edge_type"] for r in db.explain(a, b)} & TASK_EDGE_TYPES
                if want != got:
                    fail(task, f"why {a} {b}: truth {sorted(want)} != store "
                               f"{sorted(got)}")

            elif kind == "visibility":
                target, want = truth["target"], set(truth["edge_types"])
                for key in truth["answer"] + truth["forbid"]:
                    got = {r["edge_type"] for r in db.explain(target, key)}
                    if not want <= got:
                        role = ("answer" if key in truth["answer"]
                                else "near-miss")
                        fail(task, f"{role} {target} {key}: store has "
                                   f"{sorted(got)}, wanted {sorted(want)}")

            elif kind == "retraction":
                target, want = truth["target"], truth["edge_types"]
                for key in truth["base"]:
                    got = {r["edge_type"] for r in db.explain(target, key)}
                    if not set(want) <= got:
                        fail(task, f"before the counterfactual {target} {key}: "
                                   f"store has {sorted(got)}, wanted "
                                   f"{sorted(want)}")

            elif kind == "timetravel":
                target, want = truth["target"], truth["edge_types"]
                commit = commit_of[truth["day"]]
                for key in truth["answer"]:
                    for edge_type in want:
                        if not db.was_linked(target, key, edge_type, commit):
                            fail(task, f"{target} {key} {edge_type} on "
                                       f"{truth['as_of']}: truth says linked, "
                                       f"store says not")
                for key in truth["forbid"]:
                    # Short-circuit: one relationship missing is the whole of
                    # the claim, and each call is ~2.4 s on this store.
                    if all(db.was_linked(target, key, t, commit) for t in want):
                        fail(task, f"near-miss {target} {key} on "
                                   f"{truth['as_of']}: store says it was "
                                   f"linked by all of {sorted(want)}")
    finally:
        db.close()
    return problems, bad


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--seed", type=int, default=20260910)
    ap.add_argument("--scale", type=int, default=2000)
    ap.add_argument("--out", type=Path, default=HERE / "tasks.json")
    ap.add_argument("--build", type=Path, default=None,
                    help="a build.py --out directory; re-asks the engine every "
                         "claim the tasks make that it can be asked")
    ap.add_argument("--rounds", type=int, default=3,
                    help="how many times to drop a disputed task and pick "
                         "its replacement before giving up")
    ap.add_argument("--disagreements", type=Path, default=None,
                    help="file the store's disagreements here")
    args = ap.parse_args(argv)

    avoid: frozenset[str] = frozenset()
    filed: list[str] = []
    data = build(args.seed, args.scale, avoid)
    if args.build is not None:
        for attempt in range(1, args.rounds + 1):
            problems, bad = verify_against_store(data, args.build, progress=True)
            if not problems:
                print(f"store agrees with every claim it can be asked "
                      f"(round {attempt})")
                break
            # The spec's rule: a task whose two truths disagree is dropped and
            # the disagreement filed. Its target goes on the avoid list and
            # the next candidate takes its place.
            filed.extend(problems)
            for line in problems:
                print(f"  store disagrees: {line}")
            dropped = {k for t in data["tasks"] if t["id"] in bad
                       for k in task_targets(t)}
            if not dropped:
                print("  nothing to drop: the disputed task has no target")
                return 1
            print(f"  dropping {sorted(dropped)} and rebuilding")
            avoid = avoid | dropped
            data = build(args.seed, args.scale, avoid)
        else:
            print(f"still disagreeing after {args.rounds} rounds")
            return 1
    if filed and args.disagreements:
        args.disagreements.write_text(
            "# Tasks the engine disputed\n\n"
            + "\n".join(f"- `{line}`" for line in filed) + "\n")
        print(f"filed {len(filed)} disagreement(s) in {args.disagreements}")
    if avoid:
        data["dropped_targets"] = sorted(avoid)
    args.out.write_text(json.dumps(data, indent=2) + "\n")
    print(f"wrote {args.out} (world {data['world_digest']})")
    for task in data["tasks"]:
        check = task["checks"][0]
        print(f"  {task['id']:2d} {task['kind']:11s} {task['key']:18s} "
              f"answer={len(check['values']):3d} forbid={len(check['forbid'])}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
