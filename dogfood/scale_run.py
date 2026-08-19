"""Orchestrated marketplace-scale dogfood experiment.

Phases (each wall-clock + peak RSS via the resource module):
  (1) ingest N nodes via Python bindings (logical batches; each insert_node
      is one WAL frame — the bindings do not expose batch()/ingest_json)
  (2) declare ALL rule instances EXCEPT semantic_match → backfill
  (3) declare semantic_match SEPARATELY (1536-dim ScanAll). A blocking
      create_rule cannot be aborted, so a 5k subset is timed first and the
      O(n²) cost is extrapolated; the full 100k attempt runs only if that
      projection is under 30 minutes AND projected extra RSS is under 8 GiB
  (4) 100 random prop updates → p50/p95 derive latency
  (5) Big-3: 50 random talent, node_edges + neighbors for Company matches
  (6) explain() on 100 random derived edges
  (7) db reopen (WAL replay — snapshot() is not on the Python bindings)

Sparse User nodes (first 500 talent user_ids) are inserted before rule
declaration so KeyMatch FK rules materialize assertable edges.

Engine misbehavior (1k industry_alignment oracle mismatch, crash, projected
unbounded memory) stops the dangerous phase and is recorded; it is not
"fixed" in this plan.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import math
import os
import platform
import random
import resource
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Iterable, Iterator

from mushroomdb import GraphDb

from rules import SIX_RULES
from synthesize import METROS, SPECIALTIES, generate

DEFAULT_SCALE = 100_000
DEFAULT_SEED = 20260819
N_SPARSE_USERS = 500
SEMANTIC_PROBE_SCALE = 5_000
ORACLE_SCALE = 1_000
SEMANTIC_TIME_BUDGET_S = 30 * 60
# Projected extra RSS above which a blocking backfill is not attempted.
RSS_BUDGET_BYTES = 8 * 1024 * 1024 * 1024
INGEST_BATCH = 1_000
N_INCREMENTAL = 100
N_BIG3 = 50
N_EXPLAIN = 100
ENGINE_EDGE_BUDGET = 1_000_000

BIG3_TYPES = ("INDUSTRY_ALIGNMENT", "SPECIALTY_MATCH", "LOCATION_FIT")
SEMANTIC_NAME = "semantic_match_tc"

_RESULTS_DIR = Path(__file__).resolve().parent / "results"


class EngineMisbehavior(RuntimeError):
    """Wrong derived-edge counts vs the independent oracle, or similar."""


# ---------------------------------------------------------------------------
# Public helpers (tested)
# ---------------------------------------------------------------------------


def split_scale(n: int) -> tuple[int, int, int]:
    """70 / 20 / 10 talent / company / job split. Remainder goes to jobs."""
    n_talent = (n * 7) // 10
    n_companies = (n * 2) // 10
    n_jobs = n - n_talent - n_companies
    return n_talent, n_companies, n_jobs


def sparse_user_nodes(n_talent: int, n_users: int = N_SPARSE_USERS) -> list[dict]:
    """First `n_users` talent user_ids as User-label nodes (T2→T3 inheritance)."""
    n = max(0, min(n_users, n_talent))
    return [
        {"key": f"user-{i:06d}", "label": "User", "props": {"name": f"User {i}"}}
        for i in range(n)
    ]


def fk_rule_defs() -> list[dict[str, Any]]:
    """KeyMatch rules equivalent to ingest auto-FK (not exposed on GraphDb)."""
    return [
        {
            "name": "auto_fk_talent_user_id",
            "src_label": "Talent",
            "dst_label": "User",
            "predicate": {"KeyMatch": {"field": "user_id"}},
            "edge_type": "USER",
            "weight_prop": None,
            "max_edges": None,
        },
        {
            "name": "auto_fk_job_company_id",
            "src_label": "Job",
            "dst_label": "Company",
            "predicate": {"KeyMatch": {"field": "company_id"}},
            "edge_type": "COMPANY",
            "weight_prop": None,
            "max_edges": None,
        },
    ]


def non_semantic_rules() -> list[dict[str, Any]]:
    return [r for r in SIX_RULES if r["name"] != SEMANTIC_NAME]


def semantic_rule() -> dict[str, Any]:
    for r in SIX_RULES:
        if r["name"] == SEMANTIC_NAME:
            return r
    raise KeyError(SEMANTIC_NAME)


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="mushroomdb marketplace scale run")
    p.add_argument("--scale", type=int, default=DEFAULT_SCALE)
    p.add_argument("--seed", type=int, default=DEFAULT_SEED)
    p.add_argument("--out", default=str(_RESULTS_DIR / "scale-100k.md"))
    return p


# ---------------------------------------------------------------------------
# RSS / timing
# ---------------------------------------------------------------------------


def peak_rss_bytes() -> int:
    """Process-lifetime ru_maxrss. Darwin reports bytes; Linux reports KiB."""
    ru = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    if sys.platform == "darwin":
        return int(ru)
    return int(ru) * 1024


def current_rss_bytes() -> int:
    try:
        out = subprocess.check_output(
            ["ps", "-o", "rss=", "-p", str(os.getpid())], text=True
        )
        return int(out.strip().split()[0]) * 1024
    except Exception:
        return peak_rss_bytes()


def percentile(xs: list[float], p: float) -> float:
    if not xs:
        return float("nan")
    s = sorted(xs)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * (p / 100.0)
    f = math.floor(k)
    c = min(f + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)


def _fmt_s(s: float) -> str:
    if not math.isfinite(s):
        return "n/a"
    if s < 0.001:
        return f"{s * 1e6:.1f} µs"
    if s < 1.0:
        return f"{s * 1e3:.2f} ms"
    if s < 60.0:
        return f"{s:.3f} s"
    return f" {s / 60.0:.2f} min".lstrip()


def _fmt_bytes(n: int | float) -> str:
    if not math.isfinite(float(n)):
        return "n/a"
    n = float(n)
    for unit, div in (("GiB", 1024**3), ("MiB", 1024**2), ("KiB", 1024)):
        if abs(n) >= div:
            return f"{n / div:.2f} {unit}"
    return f"{int(n)} B"


def _log(msg: str) -> None:
    print(msg, flush=True)


class _PhaseClock:
    def __init__(self, name: str):
        self.name = name
        self.t0 = time.perf_counter()
        self.rss0 = current_rss_bytes()

    def stop(self, status: str = "ok", **extra: Any) -> dict[str, Any]:
        wall = time.perf_counter() - self.t0
        block = {
            "status": status,
            "wall_s": wall,
            "peak_rss_bytes": peak_rss_bytes(),
            "rss_after_bytes": current_rss_bytes(),
            "rss_before_bytes": self.rss0,
        }
        block.update(extra)
        _log(
            f"  [{self.name}] {status} wall={_fmt_s(wall)} "
            f"peak={_fmt_bytes(block['peak_rss_bytes'])} "
            f"rss={_fmt_bytes(block['rss_after_bytes'])}"
        )
        return block


# ---------------------------------------------------------------------------
# Ingest / rules
# ---------------------------------------------------------------------------


def ingest_nodes(db: GraphDb, nodes: Iterable[dict], batch: int = INGEST_BATCH) -> dict[str, list[str]]:
    keys: dict[str, list[str]] = {"User": [], "Talent": [], "Company": [], "Job": []}
    n = 0
    t_batch = time.perf_counter()
    for item in nodes:
        db.insert_node(item["key"], item["label"], item["props"])
        keys.setdefault(item["label"], []).append(item["key"])
        n += 1
        if n % batch == 0:
            dt = time.perf_counter() - t_batch
            _log(
                f"    ingest {n}  last-{batch} {dt:.2f}s  "
                f"rss={_fmt_bytes(current_rss_bytes())}"
            )
            t_batch = time.perf_counter()
    return keys


def _stream_scale(n_talent: int, n_companies: int, n_jobs: int, seed: int) -> Iterator[dict]:
    for u in sparse_user_nodes(n_talent):
        yield u
    yield from generate(n_talent, n_companies, n_jobs, seed)


def declare_rules(db: GraphDb, rules: list[dict]) -> list[dict[str, Any]]:
    per: list[dict[str, Any]] = []
    for rule in rules:
        clock = _PhaseClock(f"rule:{rule['name']}")
        db.create_rule(rule)
        per.append(clock.stop(ok_name=rule["name"]))
        per[-1]["name"] = rule["name"]
        per[-1]["edge_type"] = rule["edge_type"]
    return per


def count_out_edges(db: GraphDb, src_keys: list[str], etype: str) -> int:
    n = 0
    for k in src_keys:
        n += len(db.neighbors(k, etype, "out"))
    return n


# ---------------------------------------------------------------------------
# Oracle
# ---------------------------------------------------------------------------


def expected_industry_pairs(talent: list[dict], companies: list[dict]) -> set[tuple[str, str]]:
    """Pure-Python field_equal(industry) — independent of the engine."""
    by_ind: dict[str, list[str]] = {}
    for c in companies:
        by_ind.setdefault(c["props"]["industry"], []).append(c["key"])
    out: set[tuple[str, str]] = set()
    for t in talent:
        for ck in by_ind.get(t["props"]["industry"], ()):
            out.add((t["key"], ck))
    return out


def engine_industry_pairs(db: GraphDb, talent_keys: list[str]) -> set[tuple[str, str]]:
    got: set[tuple[str, str]] = set()
    for tk in talent_keys:
        for ck in db.neighbors(tk, "INDUSTRY_ALIGNMENT", "out"):
            got.add((tk, ck))
    return got


def run_industry_oracle(db_dir: Path, seed: int, scale: int = ORACLE_SCALE) -> dict[str, Any]:
    """1k-node exact industry_alignment set compare. Raises EngineMisbehavior."""
    nt, nc, nj = split_scale(scale)
    _log(f"oracle: generating {nt}+{nc}+{nj} (seed={seed})")
    nodes = list(generate(nt, nc, nj, seed))
    talent = [n for n in nodes if n["label"] == "Talent"]
    companies = [n for n in nodes if n["label"] == "Company"]
    expected = expected_industry_pairs(talent, companies)
    db_dir.mkdir(parents=True, exist_ok=True)
    db = GraphDb.open(str(db_dir))
    for u in sparse_user_nodes(nt):
        db.insert_node(u["key"], u["label"], u["props"])
    for item in nodes:
        db.insert_node(item["key"], item["label"], item["props"])
    industry_tc = next(r for r in SIX_RULES if r["name"] == "industry_alignment_tc")
    db.create_rule(industry_tc)
    got = engine_industry_pairs(db, [t["key"] for t in talent])
    db.close()
    if got != expected:
        missing = expected - got
        extra = got - expected
        raise EngineMisbehavior(
            f"industry_alignment oracle failed at scale={scale}: "
            f"expected {len(expected)} got {len(got)} "
            f"missing={len(missing)} extra={len(extra)}"
        )
    _log(f"oracle: ok  edges={len(expected)}")
    return {
        "scale": scale,
        "expected": len(expected),
        "got": len(got),
        "status": "ok",
    }


# ---------------------------------------------------------------------------
# Semantic / backfill probes (5k subset)
# ---------------------------------------------------------------------------


def _pair_scale(n_t: int, n_c: int) -> int:
    return n_t * n_c


def probe_semantic(db_dir: Path, seed: int, scale: int = SEMANTIC_PROBE_SCALE) -> dict[str, Any]:
    """Time VectorSimilar backfill on a subset; return O(n²) extrapolation."""
    nt, nc, nj = split_scale(scale)
    _log(f"semantic probe: ingest {nt}+{nc}+{nj}")
    db_dir.mkdir(parents=True, exist_ok=True)
    db = GraphDb.open(str(db_dir))
    ingest_nodes(db, generate(nt, nc, nj, seed))
    rss_before = current_rss_bytes()
    clock = _PhaseClock("semantic_probe")
    db.create_rule(semantic_rule())
    block = clock.stop()
    n_edges = count_out_edges(db, [f"talent-{i:06d}" for i in range(nt)], "SEMANTIC_MATCH")
    db.close()
    rss_delta = max(0, block["rss_after_bytes"] - rss_before)
    pairs = _pair_scale(nt, nc)
    return {
        "scale": scale,
        "n_talent": nt,
        "n_companies": nc,
        "pairs": pairs,
        "wall_s": block["wall_s"],
        "rss_delta_bytes": rss_delta,
        "peak_rss_bytes": block["peak_rss_bytes"],
        "edges": n_edges,
        "tripped": n_edges >= ENGINE_EDGE_BUDGET,
    }


def probe_backfill(db_dir: Path, seed: int, scale: int = SEMANTIC_PROBE_SCALE) -> dict[str, Any]:
    """Time non-semantic matcher backfill on a subset for memory/time projection."""
    nt, nc, nj = split_scale(scale)
    _log(f"backfill probe: ingest {nt}+{nc}+{nj}")
    db_dir.mkdir(parents=True, exist_ok=True)
    db = GraphDb.open(str(db_dir))
    ingest_nodes(db, generate(nt, nc, nj, seed))
    talent_keys = [f"talent-{i:06d}" for i in range(nt)]
    per_rule: list[dict[str, Any]] = []
    clock = _PhaseClock("backfill_probe")
    for rule in non_semantic_rules():
        rss_before = current_rss_bytes()
        t0 = time.perf_counter()
        db.create_rule(rule)
        wall = time.perf_counter() - t0
        rss_after = current_rss_bytes()
        n_edges = count_out_edges(db, talent_keys, rule["edge_type"])
        per_rule.append(
            {
                "name": rule["name"],
                "edge_type": rule["edge_type"],
                "wall_s": wall,
                "rss_delta_bytes": max(0, rss_after - rss_before),
                "edges": n_edges,
                "tripped": n_edges >= ENGINE_EDGE_BUDGET,
            }
        )
        _log(
            f"    {rule['name']}: {_fmt_s(wall)} edges={n_edges} "
            f"tripped={n_edges >= ENGINE_EDGE_BUDGET} "
            f"Δrss={_fmt_bytes(max(0, rss_after - rss_before))}"
        )
    block = clock.stop()
    db.close()
    return {
        "scale": scale,
        "n_talent": nt,
        "n_companies": nc,
        "pairs": _pair_scale(nt, nc),
        "wall_s": block["wall_s"],
        "peak_rss_bytes": block["peak_rss_bytes"],
        "rules": per_rule,
    }


def extrapolate_o_n2(
    probe: dict[str, Any],
    n_t_full: int,
    n_c_full: int,
) -> dict[str, Any]:
    """pairs_full / pairs_probe scaling — ScanAll and cartesian FieldEqual."""
    pairs_probe = max(1, int(probe["pairs"]))
    pairs_full = _pair_scale(n_t_full, n_c_full)
    factor = pairs_full / pairs_probe
    wall = float(probe["wall_s"]) * factor
    rss = float(probe.get("rss_delta_bytes") or 0) * factor
    return {
        "pairs_probe": pairs_probe,
        "pairs_full": pairs_full,
        "factor": factor,
        "projected_wall_s": wall,
        "projected_rss_delta_bytes": rss,
        "under_time_budget": wall < SEMANTIC_TIME_BUDGET_S,
        "under_rss_budget": rss < RSS_BUDGET_BYTES,
    }


# ---------------------------------------------------------------------------
# Incremental / Big-3 / explain
# ---------------------------------------------------------------------------


def _mutate_specialties(rng: random.Random, cur: list) -> list[str]:
    primary = rng.choice(list(SPECIALTIES))
    pool = [s for s in SPECIALTIES if s != primary]
    n_sec = rng.randint(0, 3)
    return [primary, *rng.sample(pool, n_sec)]


def _mutate_location(rng: random.Random) -> list[float]:
    _name, lat, lon = METROS[rng.randrange(len(METROS))]
    # 1 km jitter so the write is a real geo change without leaving the metro.
    dlat = (rng.random() - 0.5) * 0.02
    dlon = (rng.random() - 0.5) * 0.02
    return [lat + dlat, lon + dlon]


def _mutate_embedding(cur: list) -> list[float]:
    # Negation preserves L2 norm and is a real cosine change.
    return [-float(x) for x in cur]


def run_incremental(db: GraphDb, talent_keys: list[str], seed: int) -> dict[str, Any]:
    rng = random.Random(seed + 1)
    samples: list[float] = []
    kinds = ("specialties", "location", "embedding")
    n = min(N_INCREMENTAL, len(talent_keys))
    for i in range(n):
        key = talent_keys[rng.randrange(len(talent_keys))]
        info = db.node_info(key)
        assert info is not None
        kind = kinds[i % 3]
        props = info["props"]
        if kind == "specialties":
            field, value = "specialties", _mutate_specialties(rng, props.get("specialties") or [])
        elif kind == "location":
            field, value = "location", _mutate_location(rng)
        else:
            field, value = "embedding", _mutate_embedding(props["embedding"])
        t0 = time.perf_counter()
        db.set_prop(key, field, value)
        samples.append(time.perf_counter() - t0)
    return {
        "n": n,
        "samples_s": samples,
        "p50_s": percentile(samples, 50),
        "p95_s": percentile(samples, 95),
    }


def run_big3(db: GraphDb, talent_keys: list[str], seed: int) -> dict[str, Any]:
    rng = random.Random(seed + 2)
    n = min(N_BIG3, len(talent_keys))
    picks = rng.sample(talent_keys, n)
    samples: list[float] = []
    n_matches = 0
    for key in picks:
        t0 = time.perf_counter()
        _edges = db.node_edges(key)
        buckets = [set(db.neighbors(key, et, "out")) for et in BIG3_TYPES]
        matches = set.intersection(*buckets) if buckets else set()
        samples.append(time.perf_counter() - t0)
        n_matches += len(matches)
    return {
        "n": n,
        "samples_s": samples,
        "p50_s": percentile(samples, 50),
        "p95_s": percentile(samples, 95),
        "mean_matches": n_matches / n if n else 0.0,
    }


def _sample_derived_pairs(
    db: GraphDb, talent_keys: list[str], n: int, seed: int
) -> list[tuple[str, str]]:
    rng = random.Random(seed + 3)
    order = list(talent_keys)
    rng.shuffle(order)
    pairs: list[tuple[str, str]] = []
    for key in order:
        for e in db.node_edges(key):
            if e.get("derived"):
                pairs.append((e["src_key"], e["dst_key"]))
                if len(pairs) >= n:
                    return pairs
    return pairs


def run_explain(db: GraphDb, talent_keys: list[str], seed: int) -> dict[str, Any]:
    pairs = _sample_derived_pairs(db, talent_keys, N_EXPLAIN, seed)
    samples: list[float] = []
    for a, b in pairs:
        t0 = time.perf_counter()
        db.explain(a, b)
        samples.append(time.perf_counter() - t0)
    return {
        "n": len(samples),
        "samples_s": samples,
        "p50_s": percentile(samples, 50) if samples else float("nan"),
        "p95_s": percentile(samples, 95) if samples else float("nan"),
    }


def run_reopen(db_dir: Path, db: GraphDb) -> tuple[GraphDb, dict[str, Any]]:
    db.close()
    clock = _PhaseClock("reopen")
    reopened = GraphDb.open(str(db_dir))
    block = clock.stop()
    return reopened, block


# ---------------------------------------------------------------------------
# Machine header + report
# ---------------------------------------------------------------------------


def _sysctl(name: str) -> str:
    try:
        return subprocess.check_output(["sysctl", "-n", name], text=True).strip()
    except Exception:
        return ""


def machine_header() -> dict[str, Any]:
    mem = _sysctl("hw.memsize")
    ram = int(mem) if mem.isdigit() else None
    return {
        "date": _dt.datetime.now().isoformat(timespec="seconds"),
        "os": platform.platform(),
        "python": platform.python_version(),
        "machine": platform.machine(),
        "cpu": _sysctl("machdep.cpu.brand_string") or platform.processor() or "unknown",
        "ncpu": os.cpu_count(),
        "ram_bytes": ram,
        "hostname": platform.node(),
    }


def write_markdown(result: dict[str, Any], path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    mh = result["machine"]
    ram = _fmt_bytes(mh["ram_bytes"]) if mh.get("ram_bytes") else "unknown"
    phases = result["phases"]
    lines: list[str] = []
    a = lines.append
    a("# Scale run — marketplace dogfood (100k protocol)")
    a("")
    a("## Machine / date")
    a("")
    a(f"- **Date:** {mh['date']}")
    a(f"- **Host:** {mh['hostname']}")
    a(f"- **OS:** {mh['os']}")
    a(f"- **CPU:** {mh['cpu']} ({mh['ncpu']} cores, {mh['machine']})")
    a(f"- **RAM:** {ram}")
    a(f"- **Python:** {mh['python']}")
    a(f"- **Seed:** {result['seed']}")
    a(
        f"- **Scale:** {result['scale']} nodes "
        f"({result['n_talent']} Talent + {result['n_companies']} Company + "
        f"{result['n_jobs']} Job + {result['n_users']} User)"
    )
    a("")
    a("Peak RSS is `resource.ru_maxrss` (process-lifetime, Darwin bytes).")
    a("Current RSS is `ps -o rss=` after the phase. Bindings are embedded Rust")
    a("via `mushroomdb.GraphDb` — not HTTP. Numbers are labeled **not")
    a("apples-to-apples** vs the marketplace production stack (different")
    a("hardware, no network, synthetic embeddings).")
    a("")
    a("## Phase timings")
    a("")
    a("| Phase | status | wall | peak RSS (lifetime) | RSS after | notes |")
    a("|---|---|---|---|---|---|")
    notes = {
        "ingest": "insert_node loop; 1 WAL fsync / node (no Python batch API)",
        "backfill": "SIX_RULES minus semantic_match, plus KeyMatch FK",
        "semantic": "VectorSimilar 1536-dim ScanAll, isolated declare",
        "incremental": "100 set_prop (specialty / location / embedding)",
        "big3": "50 talent: node_edges + neighbors on Big-3 types",
        "explain": "explain() on up to 100 derived pairs",
        "reopen": "GraphDb.close + open (WAL replay; snapshot() not in bindings)",
    }
    for name in ("ingest", "backfill", "semantic", "incremental", "big3", "explain", "reopen"):
        b = phases[name]
        extra = ""
        if name == "semantic":
            extra = b.get("verdict", "")
        elif name in ("incremental", "big3", "explain") and b.get("p50_s") is not None:
            extra = f"p50={_fmt_s(b['p50_s'])} p95={_fmt_s(b['p95_s'])} n={b.get('n', '')}"
        a(
            f"| {name} | {b.get('status')} | {_fmt_s(b['wall_s'])} | "
            f"{_fmt_bytes(b['peak_rss_bytes'])} | {_fmt_bytes(b.get('rss_after_bytes', 0))} | "
            f"{extra or notes.get(name, '')} |"
        )
    a("")
    a("## Semantic verdict (phase 3)")
    a("")
    sem = phases["semantic"]
    a(f"- **Status:** `{sem.get('status')}`")
    a(f"- **Attempted full {result['scale']}:** {sem.get('attempted_full')}")
    a(f"- **Method:** {sem.get('method', 'n/a')}")
    if sem.get("probe"):
        pr = sem["probe"]
        ex = sem.get("extrapolation") or {}
        a(
            f"- **5k probe:** scale={pr.get('scale')} "
            f"pairs={pr.get('pairs')} wall={_fmt_s(pr.get('wall_s', float('nan')))} "
            f"edges={pr.get('edges')} Δrss={_fmt_bytes(pr.get('rss_delta_bytes', 0))}"
        )
        a(
            f"- **Extrapolation:** factor={ex.get('factor')} "
            f"pairs_full={ex.get('pairs_full')} "
            f"projected_wall={_fmt_s(ex.get('projected_wall_s', float('nan')))} "
            f"projected_Δrss={_fmt_bytes(ex.get('projected_rss_delta_bytes', 0))} "
            f"under_30min={ex.get('under_time_budget')} "
            f"under_8GiB={ex.get('under_rss_budget')}"
        )
        a(
            "- **O(n²) method:** `t_full = t_probe * (n_t_full/n_t_probe) * "
            "(n_c_full/n_c_probe)`. ScanAll evaluates every Talent×Company pair "
            "(not the passing subset). RSS projection uses the same factor on "
            "the probe's `create_rule` current-RSS delta. Full attempt only if "
            f"projected wall < {SEMANTIC_TIME_BUDGET_S}s AND projected Δrss < "
            f"{_fmt_bytes(RSS_BUDGET_BYTES)}."
        )
    a("")
    a("## Backfill (phase 2) — cartesian materialization")
    a("")
    bf = phases["backfill"]
    a(f"- **Status:** `{bf.get('status')}`")
    a(f"- **Method:** {bf.get('method', 'declared on the target graph')}")
    if bf.get("probe"):
        a(
            f"- **Probe wall:** {_fmt_s(bf['probe'].get('wall_s', float('nan')))} "
            f"at scale={bf['probe'].get('scale')}"
        )
        for rr in bf["probe"].get("rules", []):
            a(
                f"  - `{rr['name']}`: {_fmt_s(rr['wall_s'])} edges={rr['edges']} "
                f"tripped={rr['tripped']} Δrss={_fmt_bytes(rr['rss_delta_bytes'])}"
            )
    if bf.get("extrapolation"):
        ex = bf["extrapolation"]
        a(
            f"- **Extrapolation (pair-count factor {ex.get('factor')}):** "
            f"projected_wall={_fmt_s(ex.get('projected_wall_s', float('nan')))} "
            f"projected_Δrss={_fmt_bytes(ex.get('projected_rss_delta_bytes', 0))}"
        )
    if bf.get("finding"):
        a(f"- **Finding:** {bf['finding']}")
    a("")
    a("The engine's `create_rule` computes the **full desired set**")
    a("(`BTreeMap<(src,dst), score>`) *before* applying `max_edges`")
    a(f"(default {ENGINE_EDGE_BUDGET:,}). Cartesian predicates")
    a("(FieldEqual / Overlap / NumericWithin / GeoRadius) at 70k×20k therefore")
    a("allocate hundreds of millions of pairs even though only the first 1M")
    a("edges are kept. That is why a 5k probe + extrapolation gates the 100k")
    a("backfill the same way the semantic phase is gated — a blocking")
    a("backfill cannot be aborted mid-flight.")
    a("")
    a("## Incremental / Big-3 / explain")
    a("")
    incr = phases["incremental"]
    big3 = phases["big3"]
    expl = phases["explain"]
    a(
        f"- **Incremental (n={incr.get('n')}):** "
        f"p50={_fmt_s(incr.get('p50_s', float('nan')))} "
        f"p95={_fmt_s(incr.get('p95_s', float('nan')))}"
    )
    a(
        f"- **Big-3 (n={big3.get('n')}):** "
        f"p50={_fmt_s(big3.get('p50_s', float('nan')))} "
        f"p95={_fmt_s(big3.get('p95_s', float('nan')))} "
        f"mean intersection size={big3.get('mean_matches')}"
    )
    a(
        f"- **explain (n={expl.get('n')}):** "
        f"p50={_fmt_s(expl.get('p50_s', float('nan')))} "
        f"p95={_fmt_s(expl.get('p95_s', float('nan')))}"
    )
    a(
        f"- **Reopen:** {_fmt_s(phases['reopen']['wall_s'])} "
        f"({phases['reopen'].get('status')})"
    )
    a("")
    a("## Oracle")
    a("")
    oracle = result.get("oracle")
    if oracle:
        a(
            f"- 1k-node industry_alignment exact-set compare: **{oracle.get('status')}** "
            f"(expected={oracle.get('expected')} got={oracle.get('got')})"
        )
    else:
        a("- Skipped (scale < 100k smoke). 2k pytest covers 50-pair brute-force.")
    a("")
    a("## Comparison vs marketplace pain points")
    a("")
    a("**CONTEXT — not apples-to-apples.** Marketplace numbers are their")
    a("reported production pain (different hardware, networked 14-shard")
    a("search, real OpenAI 1536-dim vectors). Ours are a local embedded")
    a("process on the machine above, synthetic hash-chain embeddings.")
    a("")
    a("| Path | Marketplace (reported) | mushroomdb this run |")
    a("|---|---|---|")
    big3_note = (
        f"p50={_fmt_s(big3.get('p50_s', float('nan')))} "
        f"p95={_fmt_s(big3.get('p95_s', float('nan')))}"
    )
    if (big3.get("mean_matches") or 0) == 0:
        big3_note += (
            " — intersection empty (matcher backfill not live; "
            "do not read as a 5s-matcher replacement)"
        )
    a(f"| Talent→Company matcher (Big-3) | 5+ second queries | {big3_note} |")
    a(
        "| Search fan-out | 14 sharded Meilisearch indices + in-memory merge | "
        "derived-edge `neighbors` on declared rules |"
    )
    a(
        f"| Semantic / vector | Meili `_vectors` 1536-dim | "
        f"ScanAll VectorSimilar; {sem.get('status')} "
        f"(full={sem.get('attempted_full')}) |"
    )
    a(
        f"| Ingest 100k | (not published) | "
        f"{_fmt_s(phases['ingest']['wall_s'])} "
        f"peak {_fmt_bytes(phases['ingest']['peak_rss_bytes'])} |"
    )
    a("")
    a("## Product-surface gaps that shaped the run")
    a("")
    a("- Python `GraphDb` has `insert_node` / `create_rule` / `set_prop` /")
    a("  `query` / `explain` / `neighbors` / `node_edges` / `node_info`.")
    a("  It does **not** expose `ingest_json`, auto-FK, `batch()`, `stats()`,")
    a("  or `snapshot()`. Auto-FK is therefore declared as ordinary KeyMatch")
    a("  rules after a sparse User node set is inserted.")
    a("- Cypher has no `COUNT` and caps intermediate rows at 1,000,000;")
    a("  edge counts at scale use `neighbors` per src key.")
    a("")
    a("## Findings")
    a("")
    for f in result.get("findings", []):
        a(f"- {f}")
    if not result.get("findings"):
        a("- (none beyond the phase notes above)")
    a("")
    path.write_text("\n".join(lines) + "\n")
    _log(f"wrote {path}")


# ---------------------------------------------------------------------------
# Experiment
# ---------------------------------------------------------------------------


def run_experiment(
    db_dir: Path | str,
    scale: int,
    seed: int,
    out_path: Path | str,
    oracle: bool | None = None,
    probes: bool | None = None,
) -> dict[str, Any]:
    db_dir = Path(db_dir)
    out_path = Path(out_path)
    db_dir.mkdir(parents=True, exist_ok=True)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    n_talent, n_companies, n_jobs = split_scale(scale)
    n_users = min(N_SPARSE_USERS, n_talent)
    if oracle is None:
        oracle = scale >= DEFAULT_SCALE
    if probes is None:
        probes = scale >= DEFAULT_SCALE

    mh = machine_header()
    _log(
        f"scale_run scale={scale} ({n_talent}+{n_companies}+{n_jobs}+{n_users}U) "
        f"seed={seed} db={db_dir}"
    )
    _log(f"machine {mh['cpu']} ram={_fmt_bytes(mh['ram_bytes'] or 0)} {mh['date']}")

    findings: list[str] = []
    phases: dict[str, Any] = {}
    oracle_result = None
    sem_probe = None
    sem_ex = None
    bf_probe = None
    bf_ex = None

    if oracle:
        oracle_dir = Path(tempfile.mkdtemp(prefix="mush-oracle-"))
        try:
            oracle_result = run_industry_oracle(oracle_dir, seed, ORACLE_SCALE)
        except EngineMisbehavior as e:
            findings.append(f"STOP: {e}")
            result = {
                "scale": scale,
                "seed": seed,
                "n_talent": n_talent,
                "n_companies": n_companies,
                "n_jobs": n_jobs,
                "n_users": n_users,
                "machine": mh,
                "phases": phases,
                "oracle": {"status": "fail", "error": str(e)},
                "findings": findings,
                "out_path": out_path,
                "reopened_db": None,
            }
            write_markdown(result, out_path)
            raise

    if probes:
        probe_root = Path(tempfile.mkdtemp(prefix="mush-probe-"))
        bf_probe = probe_backfill(probe_root / "backfill", seed)
        bf_ex = extrapolate_o_n2(bf_probe, n_talent, n_companies)
        # Use the fattest per-rule RSS delta * factor as the memory signal.
        max_delta = max((r["rss_delta_bytes"] for r in bf_probe["rules"]), default=0)
        bf_ex["projected_rss_delta_bytes"] = max_delta * bf_ex["factor"]
        bf_ex["under_rss_budget"] = bf_ex["projected_rss_delta_bytes"] < RSS_BUDGET_BYTES
        sem_probe = probe_semantic(probe_root / "semantic", seed)
        sem_ex = extrapolate_o_n2(sem_probe, n_talent, n_companies)

    # (1) ingest
    clock = _PhaseClock("ingest")
    db = GraphDb.open(str(db_dir))
    keys = ingest_nodes(db, _stream_scale(n_talent, n_companies, n_jobs, seed))
    talent_keys = keys["Talent"]
    # FK rules are cheap KeyMatch; always declare so the smoke (and 100k) has
    # at least one derived edge even if cartesian matcher backfill is skipped.
    fk_per = declare_rules(db, fk_rule_defs())
    phases["ingest"] = clock.stop(
        n_nodes=sum(len(v) for v in keys.values()),
        fk_rules=[{k: r[k] for k in ("name", "wall_s")} for r in fk_per],
    )

    # (2) non-semantic matcher backfill
    clock = _PhaseClock("backfill")
    attempt_backfill = True
    backfill_method = "declared on the target graph"
    backfill_finding = None
    if bf_ex is not None:
        if not (bf_ex["under_time_budget"] and bf_ex["under_rss_budget"]):
            attempt_backfill = False
            backfill_method = (
                f"5k probe extrapolation (factor={bf_ex['factor']:.1f}); "
                "full cartesian backfill not attempted"
            )
            backfill_finding = (
                "Non-semantic create_rule at full scale projected "
                f"wall={_fmt_s(bf_ex['projected_wall_s'])} "
                f"Δrss={_fmt_bytes(bf_ex['projected_rss_delta_bytes'])} "
                f"(budgets {_fmt_s(SEMANTIC_TIME_BUDGET_S)} / {_fmt_bytes(RSS_BUDGET_BYTES)}). "
                "Engine materializes the full desired cartesian before the 1M "
                "edge budget; attempting it would hang or OOM this 24 GiB machine."
            )
            findings.append(backfill_finding)
    per_matcher: list[dict[str, Any]] = []
    if attempt_backfill:
        per_matcher = declare_rules(db, non_semantic_rules())
        phases["backfill"] = clock.stop(
            method=backfill_method,
            rules=[{k: r[k] for k in ("name", "wall_s", "status")} for r in per_matcher],
            probe=bf_probe,
            extrapolation=bf_ex,
            finding=backfill_finding,
        )
    else:
        phases["backfill"] = clock.stop(
            status="extrapolated",
            method=backfill_method,
            probe=bf_probe,
            extrapolation=bf_ex,
            finding=backfill_finding,
        )

    # (3) semantic, isolated
    clock = _PhaseClock("semantic")
    attempt_sem = True
    sem_method = "declared on the target graph (scale < probe gate)"
    if sem_ex is not None:
        sem_method = (
            "5k ScanAll probe; t_full = t_probe * (n_t_full/n_t_probe) * "
            "(n_c_full/n_c_probe)"
        )
        if not (sem_ex["under_time_budget"] and sem_ex["under_rss_budget"]):
            attempt_sem = False
            findings.append(
                "semantic_match full backfill not attempted: projected "
                f"wall={_fmt_s(sem_ex['projected_wall_s'])} "
                f"Δrss={_fmt_bytes(sem_ex['projected_rss_delta_bytes'])}"
            )
    if attempt_sem:
        db.create_rule(semantic_rule())
        phases["semantic"] = clock.stop(
            attempted_full=True,
            method=sem_method,
            probe=sem_probe,
            extrapolation=sem_ex,
            verdict="ran_full",
        )
    else:
        phases["semantic"] = clock.stop(
            status="extrapolated",
            attempted_full=False,
            method=sem_method,
            probe=sem_probe,
            extrapolation=sem_ex,
            verdict=(
                "recorded 5k extrapolation; full 100k ScanAll not started "
                "(blocking create_rule cannot be aborted)"
            ),
        )

    # (4) incremental
    clock = _PhaseClock("incremental")
    incr = run_incremental(db, talent_keys, seed)
    phases["incremental"] = clock.stop(**incr)

    # (5) Big-3
    clock = _PhaseClock("big3")
    big3 = run_big3(db, talent_keys, seed)
    phases["big3"] = clock.stop(**big3)

    # (6) explain
    clock = _PhaseClock("explain")
    expl = run_explain(db, talent_keys, seed)
    phases["explain"] = clock.stop(**expl)

    # (7) reopen
    db, reopen_block = run_reopen(db_dir, db)
    phases["reopen"] = reopen_block

    result: dict[str, Any] = {
        "scale": scale,
        "seed": seed,
        "n_talent": n_talent,
        "n_companies": n_companies,
        "n_jobs": n_jobs,
        "n_users": n_users,
        "machine": mh,
        "phases": phases,
        "oracle": oracle_result,
        "findings": findings,
        "out_path": out_path,
        "reopened_db": db,
        "talent_keys": talent_keys,
    }
    write_markdown(result, out_path)
    return result


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    db_dir = Path(args.out).resolve().parent / f"scale-{args.scale}-db"
    try:
        result = run_experiment(
            db_dir=db_dir,
            scale=args.scale,
            seed=args.seed,
            out_path=args.out,
        )
    except EngineMisbehavior as e:
        _log(f"STOP: {e}")
        return 2
    db = result.get("reopened_db")
    if db is not None:
        db.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
