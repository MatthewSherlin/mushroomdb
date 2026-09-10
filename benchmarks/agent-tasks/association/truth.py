#!/usr/bin/env python3
"""Ground truth for the association suite, computed without the engine.

Every question the suite asks has an answer that can be derived from the base
facts alone: the rules are pure functions of two nodes' props, and the history
is a list of mutations in order. So this module brute-forces all of it in
Python — and then `cross_check()` asks the engine the same questions and
reports every disagreement. A task whose two truths disagree is dropped and
the disagreement filed as an engine bug; that is the whole point of having two.

The predicates here are transcriptions of `crates/core-rules/src/def.rs`, not
paraphrases of the README:

- `FieldEqual{field}` — both sides have the field, both reduce to a scalar
  `ValueKey` (a list or a map does not), and the keys are equal. Types do not
  coerce: `Int(3)` is not `Float(3.0)`.
- `Overlap{field, min}` — both sides are lists; tokens are the set of their
  scalar values (duplicates collapse, nothing is case-folded); the rule holds
  when the intersection is non-empty **and** the **Jaccard** index
  `|A ∩ B| / |A ∪ B|` is at least `min`. The denominator is the union, not the
  smaller list.
- `GeoRadius{field, km}` — both sides are `[lat, lon]` pairs in range;
  haversine on `EARTH_RADIUS_KM = 6371.0088`; the rule holds when `d <= km`.
- `NumericWithin{field, tolerance}` — both sides are finite numbers;
  `|a - b| <= tolerance`, and `tolerance == 0` means exactly equal.

`VectorSimilar` is deliberately not implemented: it reads the 1536-dimension
`embedding` only the graph form has, `build.task_rules()` excludes it, and
evaluating it over a 2,000-node world in Python would cost hours. Passing a
rule that uses it raises rather than quietly returning a wrong answer.

A node here is `{"key", "label", "props"}` — the shape `build.world()` emits.
"""

from __future__ import annotations

import json
import math
import random
import sys
from pathlib import Path
from typing import Any, Callable, Iterable, Sequence

HERE = Path(__file__).resolve().parent
if str(HERE.parent) not in sys.path:
    sys.path.insert(0, str(HERE.parent))

from association.build import (                     # noqa: E402
    LAST_DAY, STORE_NAME, task_rules,
)

# `crates/core-rules/src/def.rs`: WGS-84 authalic mean radius.
EARTH_RADIUS_KM = 6371.0088


# --------------------------------------------------------------------------
# the four predicates, as the engine computes them
# --------------------------------------------------------------------------


def _props(node: dict[str, Any]) -> dict[str, Any]:
    """A node's props, whether it is a `{key,label,props}` record or already
    a flat one."""
    return node["props"] if "props" in node else node


def _value_key(value: Any) -> tuple[str, Any] | None:
    """`ValueKey::from_value` — the scalar identity of a value, or `None`.

    The tag keeps the engine's typing: an `Int` and a `Float` of the same
    magnitude are different keys, and so are a `Bool` and an `Int`. Python's
    `True == 1` would silently merge two of those.
    """
    if value is None:
        return None
    if isinstance(value, bool):
        return ("bool", value)
    if isinstance(value, int):
        return ("int", value)
    if isinstance(value, float):
        return ("float", value)
    if isinstance(value, str):
        return ("str", value)
    return None                                     # List, Map: no single key


def _list_tokens(value: Any) -> set[tuple[str, Any]] | None:
    """`list_tokens` — a list's scalar members as a set, or `None` if the
    value is not a list. Non-scalar members are dropped, not fatal."""
    if not isinstance(value, list):
        return None
    return {k for k in (_value_key(v) for v in value) if k is not None}


def _finite(value: Any) -> float | None:
    """`as_finite_f64` — an int or a finite float, else `None`."""
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return float(value)
    if isinstance(value, float) and math.isfinite(value):
        return value
    return None


def _latlon(value: Any) -> tuple[float, float] | None:
    """`as_latlon` — a two-element list of finite numbers, in range."""
    if not isinstance(value, list) or len(value) != 2:
        return None
    lat, lon = _finite(value[0]), _finite(value[1])
    if lat is None or lon is None:
        return None
    if -90.0 <= lat <= 90.0 and -180.0 <= lon <= 180.0:
        return (lat, lon)
    return None


def haversine_km(lat1: float, lon1: float, lat2: float, lon2: float) -> float:
    """`haversine_km` — great-circle distance on the engine's Earth radius."""
    phi1, phi2 = math.radians(lat1), math.radians(lat2)
    dphi = math.radians(lat2 - lat1)
    dlam = math.radians(lon2 - lon1)
    a = math.sin(dphi / 2.0) ** 2 + math.cos(phi1) * math.cos(phi2) * math.sin(dlam / 2.0) ** 2
    a = min(1.0, max(0.0, a))
    return EARTH_RADIUS_KM * 2.0 * math.atan2(math.sqrt(a), math.sqrt(1.0 - a))


def predicate_holds(rule: dict[str, Any], src: dict[str, Any],
                    dst: dict[str, Any]) -> bool:
    """Does `rule`'s predicate hold for this ordered pair?

    `rule` may be a whole RuleDef (the usual case) or a bare predicate dict.
    Labels are not checked here — `derived_edges` does that.
    """
    predicate = rule.get("predicate", rule) if isinstance(rule, dict) else rule
    return _eval(predicate, _props(src), _props(dst))


def _eval(predicate: dict[str, Any], a: dict[str, Any], b: dict[str, Any]) -> bool:
    (kind, args), = predicate.items()
    if kind == "FieldEqual":
        ka = _value_key(a.get(args["field"]))
        kb = _value_key(b.get(args["field"]))
        return ka is not None and ka == kb
    if kind == "Overlap":
        ta = _list_tokens(a.get(args["field"]))
        tb = _list_tokens(b.get(args["field"]))
        if ta is None or tb is None:
            return False
        inter = len(ta & tb)
        union = len(ta | tb)
        if union == 0 or inter == 0:
            return False
        return inter / union >= args["min"]
    if kind == "GeoRadius":
        km = args["km"]
        if not math.isfinite(km) or km <= 0.0:
            return False
        pa, pb = _latlon(a.get(args["field"])), _latlon(b.get(args["field"]))
        if pa is None or pb is None:
            return False
        d = haversine_km(pa[0], pa[1], pb[0], pb[1])
        return math.isfinite(d) and d <= km
    if kind == "NumericWithin":
        tol = args["tolerance"]
        if not math.isfinite(tol) or tol < 0.0:
            return False
        va, vb = _finite(a.get(args["field"])), _finite(b.get(args["field"]))
        if va is None or vb is None:
            return False
        delta = abs(va - vb)
        return delta == 0.0 if tol == 0.0 else delta <= tol
    if kind == "All":
        return bool(args) and all(_eval(p, a, b) for p in args)
    if kind == "Any":
        return any(_eval(p, a, b) for p in args)
    if kind == "VectorSimilar":
        raise ValueError(
            "truth.py does not evaluate VectorSimilar: it reads the embedding "
            "only the graph form carries, and build.task_rules() excludes it. "
            "Pass task_rules(), not store_rules().")
    raise ValueError(f"unknown predicate {kind!r}")


# --------------------------------------------------------------------------
# derivation
# --------------------------------------------------------------------------

Edge = tuple[str, str, str]                        # (edge_type, src_key, dst_key)


def by_key(nodes: Iterable[dict] | dict[str, dict]) -> dict[str, dict]:
    """Nodes as a key→node mapping, whichever way they arrived."""
    if isinstance(nodes, dict):
        return nodes
    return {n["key"]: n for n in nodes}


def _by_label(nodes: dict[str, dict]) -> dict[str, list[dict]]:
    out: dict[str, list[dict]] = {}
    for key in sorted(nodes):
        out.setdefault(nodes[key]["label"], []).append(nodes[key])
    return out


def derived_edges(nodes: Iterable[dict] | dict[str, dict],
                  rules: Sequence[dict[str, Any]]) -> set[Edge]:
    """Every edge the rules derive over these nodes.

    The whole cartesian product per rule, which is what makes this an
    independent check rather than a reimplementation of the engine's indexes.
    """
    index = _by_label(by_key(nodes))
    edges: set[Edge] = set()
    for rule in rules:
        srcs = index.get(rule["src_label"], ())
        dsts = index.get(rule["dst_label"], ())
        if not srcs or not dsts:
            continue
        predicate, edge_type = rule["predicate"], rule["edge_type"]
        for src in srcs:
            sp = _props(src)
            skey = src["key"]
            for dst in dsts:
                if _eval(predicate, sp, _props(dst)):
                    edges.add((edge_type, skey, dst["key"]))
    return edges


def edges_of(nodes: Iterable[dict] | dict[str, dict],
             rules: Sequence[dict[str, Any]], key: str) -> set[Edge]:
    """Every derived edge with `key` at either end.

    A counterfactual on one node can only move these, so a retraction question
    costs one node against the world rather than the world against itself.
    """
    index = by_key(nodes)
    node = index.get(key)
    if node is None:
        return set()
    label = node["label"]
    grouped = _by_label(index)
    edges: set[Edge] = set()
    for rule in rules:
        predicate, edge_type = rule["predicate"], rule["edge_type"]
        if label == rule["src_label"]:
            for dst in grouped.get(rule["dst_label"], ()):
                if _eval(predicate, _props(node), _props(dst)):
                    edges.add((edge_type, key, dst["key"]))
        if label == rule["dst_label"]:
            for src in grouped.get(rule["src_label"], ()):
                if _eval(predicate, _props(src), _props(node)):
                    edges.add((edge_type, src["key"], key))
    return edges


def why_at(nodes: Iterable[dict] | dict[str, dict],
           rules: Sequence[dict[str, Any]], a: str, b: str) -> list[tuple[str, str]]:
    """`(edge_type, rule_name)` for every rule linking `a` and `b`, either way
    round. Sorted, so two truths can be compared as lists."""
    index = by_key(nodes)
    na, nb = index.get(a), index.get(b)
    if na is None or nb is None:
        return []
    found: set[tuple[str, str]] = set()
    for rule in rules:
        for src, dst in ((na, nb), (nb, na)):
            if src["label"] != rule["src_label"] or dst["label"] != rule["dst_label"]:
                continue
            if _eval(rule["predicate"], _props(src), _props(dst)):
                found.add((rule["edge_type"], rule["name"]))
    return sorted(found)


def why(world: dict[str, Any], day: int, a: str, b: str) -> list[tuple[str, str]]:
    """The same, at a day in the history."""
    return why_at(state_at(world, day), world["rules"], a, b)


# --------------------------------------------------------------------------
# history
# --------------------------------------------------------------------------


def state_at(world: dict[str, Any], day: int) -> dict[str, dict]:
    """The world as of the end of `day`, as a key→node mapping.

    Day 0 is the base state: the changelog starts on day 1. Changes are in day
    order and never touch a key twice in one day, so replaying them in order
    is the whole of it.
    """
    nodes = {
        n["key"]: {"key": n["key"], "label": n["label"], "props": dict(n["props"])}
        for n in world["nodes"]
    }
    for change in world["changes"]:
        if change["day"] > day:
            break
        op, key = change["op"], change["key"]
        if op == "set_prop":
            node = nodes.get(key)
            if node is not None:
                node["props"][change["field"]] = change["value"]
        elif op == "delete_node":
            nodes.pop(key, None)
        elif op == "insert_node":
            fresh = change["node"]
            nodes[key] = {"key": fresh["key"], "label": fresh["label"],
                          "props": dict(fresh["props"])}
        else:
            raise ValueError(f"unknown op {op!r}")
    return nodes


def retraction(world: dict[str, Any], day: int, key: str, field: str,
               value: Any) -> set[Edge]:
    """The edges that disappear if `key`'s `field` became `value` on `day`.

    Only edges touching `key` can move, so both sides are `edges_of`.
    """
    nodes = state_at(world, day)
    if key not in nodes:
        raise KeyError(f"{key} does not exist on day {day}")
    rules = world["rules"]
    before = edges_of(nodes, rules, key)
    nodes[key]["props"][field] = value
    after = edges_of(nodes, rules, key)
    return before - after


# --------------------------------------------------------------------------
# neighbourhoods
# --------------------------------------------------------------------------


def partners(edges: Iterable[Edge], key: str,
             edge_types: Sequence[str] | None = None) -> set[str]:
    """Everything joined to `key` by **all** of `edge_types` (any type, if
    none are named). Direction-agnostic, like the questions."""
    seen: dict[str, set[str]] = {}
    for edge_type, src, dst in edges:
        other = dst if src == key else src if dst == key else None
        if other is None:
            continue
        seen.setdefault(other, set()).add(edge_type)
    if edge_types is None:
        return set(seen)
    want = set(edge_types)
    return {other for other, types in seen.items() if want <= types}


def multihop(nodes: Iterable[dict] | dict[str, dict], edges: Iterable[Edge], *,
             dst_label: str, edge_types: Sequence[str], min_sources: int,
             src_filter: Callable[[dict], bool] | None = None,
             dst_filter: Callable[[dict], bool] | None = None) -> set[str]:
    """Two hops: the `dst_label` nodes reached by **all** of `edge_types` from
    at least `min_sources` distinct sources that pass `src_filter`."""
    index = by_key(nodes)
    want = set(edge_types)
    reached: dict[str, dict[str, set[str]]] = {}
    for edge_type, src, dst in edges:
        if edge_type not in want:
            continue
        for a, b in ((src, dst), (dst, src)):
            node = index.get(a)
            if node is None or node["label"] != dst_label:
                continue
            other = index.get(b)
            if other is None or (src_filter is not None and not src_filter(other)):
                continue
            reached.setdefault(a, {}).setdefault(b, set()).add(edge_type)
    out: set[str] = set()
    for key, sources in reached.items():
        if dst_filter is not None and not dst_filter(index[key]):
            continue
        full = sum(1 for types in sources.values() if want <= types)
        if full >= min_sources:
            out.add(key)
    return out


# --------------------------------------------------------------------------
# visibility
# --------------------------------------------------------------------------


def role_labels(world: dict[str, Any], role: str) -> set[str]:
    for defined in world["roles"]:
        if defined["name"] == role:
            return set(defined["labels"])
    raise KeyError(f"no role named {role!r}; have "
                   f"{[r['name'] for r in world['roles']]}")


def visible(world: dict[str, Any], role: str, items: Iterable[Any],
            nodes: Iterable[dict] | dict[str, dict]) -> set[Any]:
    """The subset of `items` a `role` can see.

    An item is either a node key or an edge tuple. A node is visible when its
    label is one of the role's; an edge is visible only when **both** its
    endpoints are — a relationship you can only see one end of is not a
    relationship you can see.
    """
    index = by_key(nodes)
    labels = role_labels(world, role)

    def sees(key: str) -> bool:
        node = index.get(key)
        return node is not None and node["label"] in labels

    out: set[Any] = set()
    for item in items:
        if isinstance(item, str):
            if sees(item):
                out.add(item)
        else:
            _edge_type, src, dst = item
            if sees(src) and sees(dst):
                out.add(item)
    return out


# --------------------------------------------------------------------------
# the cross-check: brute force against the engine
# --------------------------------------------------------------------------

# `query_at` costs 80-90 s per call on the built store — even at the head
# commit — so nothing here uses it. `explain` is ~1 ms, `was_linked` and
# `node_history` ~2.4 s, and those three answer every question we need to ask.
LIVE_PAIRS = 200
TIME_PROBES = 20
HISTORY_PROBES = 20

# How many `prop_set` entries `node_history` reports for the props an
# `insert_node` brought with it. Measured: none — the record carries them.
INSERT_PROP_SET_RECORDS = 0

# Disagreements the cross-check has already found, filed, and is not going to
# re-litigate on every run. They are still reported — prefixed, so an unknown
# disagreement can never hide behind one — and `association/cross-check.md`
# carries the reproduction and the diagnosis.
GAP_DELETED_PROPS = "node-history-deleted-props"
KNOWN_GAP_PREFIX = "known-gap"
KNOWN_GAPS: dict[str, str] = {
    GAP_DELETED_PROPS:
        "`node_history` on a deleted node reports its insert and its delete "
        "but none of its `prop_set` records: `db.rs`'s SetPropId branch "
        "resolves the id with `key_of`, which returns None for a tombstoned "
        "id, while the insert/delete branches match on the key string. "
        "`edge_history` and `was_linked` use `key_of_historical` instead.",
}


def _gap(gap_id: str, line: str) -> str:
    return f"{KNOWN_GAP_PREFIX}[{gap_id}]: {line}"


def unknown(problems: Iterable[str]) -> list[str]:
    """The disagreements that are not already filed."""
    return [p for p in problems if not p.startswith(f"{KNOWN_GAP_PREFIX}[")]


def _days_map(store_dir: Path, days: Any) -> dict[int, int]:
    if days is None:
        days = json.loads((store_dir / "days.json").read_text())
    if isinstance(days, dict):
        return {int(d): int(c) for d, c in days.items()}
    return {int(e["day"]): int(e["commit"]) for e in days}


def cross_check(world: dict[str, Any], store_dir: str | Path, days: Any = None, *,
                seed: int = 20260910, live_pairs: int = LIVE_PAIRS,
                time_probes: int = TIME_PROBES,
                history_probes: int = HISTORY_PROBES,
                progress: bool = False) -> list[str]:
    """Ask the engine the questions this module answered, report every gap.

    Three probes, because three engine calls are cheap enough to run:

    - **why** — `explain(a, b)` on live pairs against `why_at` on the replayed
      final state, restricted to the rules a task may ask about.
    - **time travel** — `was_linked(a, b, edge_type, commit_of_day)` against
      `why` on the replayed state of that day, half on pairs the truth says
      were linked and half on pairs it says were not.
    - **history** — `node_history(key)` against the changelog, on keys the
      history deleted or mutated.

    Returns one line per disagreement. A disagreement already filed in
    `KNOWN_GAPS` is prefixed `known-gap[<id>]`, so `unknown()` gives the ones
    that still need explaining; an empty `unknown()` means the two truths
    agree everywhere the engine claims they should.
    """
    from mushroomdb import GraphDb

    store_dir = Path(store_dir)
    commit_of = _days_map(store_dir, days)
    rules = world["rules"]
    rule_names = {r["name"] for r in rules}
    edge_types = sorted({r["edge_type"] for r in rules})
    rng = random.Random(f"association-cross-check-{seed}")

    live = state_at(world, LAST_DAY)
    talents = sorted(k for k, n in live.items() if n["label"] == "Talent")
    others = sorted(k for k, n in live.items() if n["label"] in ("Company", "Job"))
    problems: list[str] = []

    db = GraphDb.open(str(store_dir / STORE_NAME), read_only=True)
    try:
        # --- why -------------------------------------------------------
        for i in range(live_pairs):
            a, b = rng.choice(talents), rng.choice(others)
            mine = {t for t, _rule in why_at(live, rules, a, b)}
            theirs = {row["edge_type"] for row in db.explain(a, b)
                      if row["rule"] in rule_names}
            if mine != theirs:
                problems.append(
                    f"why {a} {b}: truth {sorted(mine)} != explain {sorted(theirs)}")
            if progress and i % 50 == 0:
                print(f"  why {i}/{live_pairs}", flush=True)

        # --- time travel -----------------------------------------------
        for i in range(time_probes):
            want_linked = i % 2 == 0
            probe = _pick_time_probe(rng, world, rules, edge_types, commit_of,
                                     want_linked)
            if probe is None:
                problems.append(
                    f"time travel: no pair found that was "
                    f"{'linked' if want_linked else 'unlinked'} at a sampled day")
                continue
            day, a, b, edge_type, expected = probe
            got = db.was_linked(a, b, edge_type, commit_of[day])
            if got != expected:
                problems.append(
                    f"was_linked {a} {b} {edge_type} day {day} "
                    f"(commit {commit_of[day]}): truth {expected} != engine {got}")
            if progress:
                print(f"  time travel {i + 1}/{time_probes}", flush=True)

        # --- history ---------------------------------------------------
        for i, key in enumerate(_history_keys(rng, world, history_probes)):
            problems.extend(_history_problems(db, world, key))
            if progress:
                print(f"  history {i + 1}/{history_probes}", flush=True)
    finally:
        db.close()
    return problems


def _pick_time_probe(rng: random.Random, world: dict[str, Any],
                     rules: Sequence[dict], edge_types: Sequence[str],
                     commit_of: dict[int, int], want_linked: bool,
                     tries: int = 200):
    """A `(day, a, b, edge_type, expected)` probe the truth says is linked (or
    not) at that day. Sampling rather than enumerating keeps this cheap; the
    caller reports a failure to find one rather than silently skipping."""
    for _ in range(tries):
        day = rng.randint(1, LAST_DAY)
        nodes = state_at(world, day)
        talents = [k for k, n in nodes.items() if n["label"] == "Talent"]
        others = [k for k, n in nodes.items() if n["label"] in ("Company", "Job")]
        if not talents or not others:
            continue
        a, b = rng.choice(sorted(talents)), rng.choice(sorted(others))
        linked = {t for t, _rule in why_at(nodes, rules, a, b)}
        if want_linked:
            if linked:
                return (day, a, b, sorted(linked)[rng.randrange(len(linked))], True)
        else:
            # The probed type has to be one a rule could actually derive for
            # *this* label pair — asking whether a Talent and a Job were ever
            # `SIMILAR_SIZE` proves nothing, because no rule declares it.
            # Filtering per type, not over the batch, is what guarantees that.
            derivable = [t for t in edge_types if t not in linked
                         and _label_pair_is_possible(nodes, rules, a, b, [t])]
            if derivable:
                return (day, a, b, rng.choice(derivable), False)
    return None


def _label_pair_is_possible(nodes: dict[str, dict], rules: Sequence[dict],
                            a: str, b: str, edge_types: Sequence[str]) -> bool:
    """Is at least one of these edge types even declarable between these two
    labels? Probing `SIMILAR_SIZE` on a Talent/Job pair proves nothing — no
    rule could ever derive it."""
    la, lb = nodes[a]["label"], nodes[b]["label"]
    want = set(edge_types)
    return any(r["edge_type"] in want
               and {r["src_label"], r["dst_label"]} == {la, lb}
               for r in rules)


def _history_keys(rng: random.Random, world: dict[str, Any], n: int) -> list[str]:
    """Keys the history actually moved, hardest first.

    A key that was mutated *and then deleted* is the interesting one — it is
    where the engine's node history and the changelog part company — so those
    lead, followed by plain deletions and plain mutations. Deterministic in
    the rng, and never vacuous while the changelog has any of each.
    """
    changes: dict[str, list[str]] = {}
    for change in world["changes"]:
        changes.setdefault(change["key"], []).append(change["op"])
    deleted_after_edit = [k for k, ops in changes.items()
                          if "delete_node" in ops and "set_prop" in ops]
    deleted = [k for k, ops in changes.items()
               if "delete_node" in ops and "set_prop" not in ops]
    mutated = [k for k, ops in changes.items()
               if "delete_node" not in ops and "set_prop" in ops]
    for group in (deleted_after_edit, deleted, mutated):
        rng.shuffle(group)
    quota = max(1, n // 4)
    picked = (deleted_after_edit[:quota] + deleted[:quota]
              + mutated + deleted_after_edit + deleted)
    out: list[str] = []
    for key in picked:
        if key not in out:
            out.append(key)
        if len(out) == n:
            break
    return out


def _history_problems(db, world: dict[str, Any], key: str) -> list[str]:
    """Does the engine's per-node history match the changelog for this key?"""
    events = db.node_history(key)
    kinds = [e["kind"] for e in events]
    changes = [c for c in world["changes"] if c["key"] == key]
    problems: list[str] = []

    wants_delete = any(c["op"] == "delete_node" for c in changes)
    if wants_delete != ("node_deleted" in kinds):
        problems.append(
            f"node_history {key}: changelog deletes it = {wants_delete}, "
            f"engine reports node_deleted = {'node_deleted' in kinds}")

    # An `insert_node` carries the whole record on the WAL's InsertNode frame,
    # so it contributes no `prop_set` entries of its own — a key the changelog
    # inserted counts exactly like a base one. That is measured, not assumed:
    # `test_an_insert_contributes_no_prop_set_records` pins it, so if the
    # engine ever starts writing them this stops being silently wrong.
    want_sets = (sum(1 for c in changes if c["op"] == "set_prop")
                 + INSERT_PROP_SET_RECORDS
                 * sum(1 for c in changes if c["op"] == "insert_node"))
    got_sets = kinds.count("prop_set")
    line = (f"node_history {key}: changelog sets {want_sets} props, "
            f"engine reports {got_sets}")
    if wants_delete and want_sets and got_sets == 0:
        # The filed gap: a tombstone takes the node's property history with it.
        problems.append(_gap(GAP_DELETED_PROPS, line))
    elif got_sets != want_sets:
        problems.append(line)

    if "node_inserted" not in kinds:
        problems.append(f"node_history {key}: engine never reports node_inserted")
    return problems


# --------------------------------------------------------------------------
# CLI: run the cross-check against a built world
# --------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    import argparse
    from association.build import form_paths, world as build_world

    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--seed", type=int, default=20260910)
    ap.add_argument("--scale", type=int, default=2000)
    ap.add_argument("--build", type=Path, required=True,
                    help="the `--out` a build.py run wrote")
    ap.add_argument("--live-pairs", type=int, default=LIVE_PAIRS)
    ap.add_argument("--time-probes", type=int, default=TIME_PROBES)
    ap.add_argument("--history-probes", type=int, default=HISTORY_PROBES)
    ap.add_argument("--out", type=Path, default=None,
                    help="write the disagreements here (only if there are any)")
    args = ap.parse_args(argv)

    w = build_world(args.seed, args.scale)
    assert [r["name"] for r in w["rules"]] == [r["name"] for r in task_rules()]
    problems = cross_check(w, form_paths(args.build)["graph"], seed=args.seed,
                           live_pairs=args.live_pairs,
                           time_probes=args.time_probes,
                           history_probes=args.history_probes, progress=True)
    print(f"\nprobes: {args.live_pairs} why, {args.time_probes} time travel, "
          f"{args.history_probes} history")
    fresh = unknown(problems)
    print(f"disagreements: {len(problems)} "
          f"({len(problems) - len(fresh)} already filed, {len(fresh)} new)")
    for line in problems:
        print(f"  {line}")
    if args.out:
        # Always, even on a clean run: a file that says "0 disagreements" is
        # the record. Writing only on failure would leave the last bad run's
        # file sitting there looking current.
        args.out.write_text(_cross_check_md(args, problems, fresh))
        print(f"wrote {args.out}")
    return 1 if fresh else 0


def _cross_check_md(args, problems: list[str], fresh: list[str]) -> str:
    lines = [
        "# Association suite — truth vs the engine",
        "",
        f"`truth.py --build <dir> --seed {args.seed} --scale {args.scale}`: "
        f"{args.live_pairs} `explain` probes, {args.time_probes} `was_linked` "
        f"probes and {args.history_probes} `node_history` probes against the "
        f"brute-force truth.",
        "",
        f"**{len(fresh)} unexplained disagreement(s)"
        + ("" if fresh else " — every task's truth stands") + ".**",
        "",
    ]
    for gap_id, prose in KNOWN_GAPS.items():
        hits = [p for p in problems if p.startswith(f"{KNOWN_GAP_PREFIX}[{gap_id}]")]
        lines += [f"## Filed: `{gap_id}` ({len(hits)} probes hit it)", "",
                  prose, ""]
        lines += [f"- `{h}`" for h in hits[:5]] + [""]
    if fresh:
        lines += ["## Unexplained", ""] + [f"- `{f}`" for f in fresh] + [""]
    return "\n".join(lines)


if __name__ == "__main__":
    raise SystemExit(main())
