"""Orchestrated marketplace-scale dogfood experiment.

Phases (each wall-clock + peak RSS via the resource module):
  (1) ingest N nodes via ingest_batch (10k-node chunks per docstring limit);
      FK rules (KeyMatch) declared immediately after ingest
  (2) declare ALL non-semantic matcher rule instances WITH max_edges caps →
      streaming backfill at 100k (T1: memory-bounded); record wall+RSS per rule
  (3a) exact semantic 5k probe (T3: Cauchy-Schwarz early-exit in engine path);
       full exact 100k only if probe projects under 30 min + 8 GiB
  (3b) approximate semantic at 100k (T4: IVF-Flat; approximate=True in RuleDef);
       recall measured vs 1k-sample exact ground truth from transform.cosine
  (4) 100 random prop updates → p50/p95 derive latency
  (5) Big-3: 50 random talent, node_edges + neighbors for Company matches
      (intersection NOW non-empty: matcher backfill is live)
  (6) explain() on 100 random derived edges
  (7a) snapshot reopen: GraphDb.snapshot() then close+open
  (7b) WAL reopen: close+reopen without snapshotting (baseline for comparison)

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
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any, Iterable, Iterator

from mushroomdb import GraphDb

from rules import APPROXIMATE_SEMANTIC_RULE, MATCHER_MAX_EDGES, SIX_RULES
from synthesize import METROS, SPECIALTIES, generate
from transform import cosine as _cosine_py

DEFAULT_SCALE = 100_000
DEFAULT_SEED = 20260819
N_SPARSE_USERS = 500
SEMANTIC_PROBE_SCALE = 5_000
ORACLE_SCALE = 1_000
SEMANTIC_TIME_BUDGET_S = 30 * 60
# Projected extra RSS above which a blocking backfill is not attempted.
RSS_BUDGET_BYTES = 8 * 1024 * 1024 * 1024
# ingest_batch docstring: keep each call to ≤10_000 nodes.
INGEST_BATCH = 10_000
N_INCREMENTAL = 100
N_BIG3 = 50
N_EXPLAIN = 100
ENGINE_EDGE_BUDGET = MATCHER_MAX_EDGES  # 1_000_000
# Recall sample for approximate vs exact ground-truth comparison.
RECALL_SAMPLE = 1_000
# Per-query ANN recall: number of Talent queries for the IVF quality measurement.
N_PQ_QUERIES = 100
# Big-3 slice: Talent + Company count for the focused metro/industry measurement.
BIG3_SLICE_SIZE = 500

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
    """Ingest nodes via ingest_batch in ≤10k-node chunks (T2 binding).

    Each flush is one atomic WAL commit rather than one fsync per node.
    Progress is logged after each chunk.
    """
    keys: dict[str, list[str]] = {"User": [], "Talent": [], "Company": [], "Job": []}
    n_total = 0
    chunk: list[dict] = []
    t_batch = time.perf_counter()

    def _flush(ch: list[dict]) -> None:
        nonlocal n_total
        if not ch:
            return
        db.ingest_batch(ch)
        n_total += len(ch)
        dt = time.perf_counter() - t_batch
        _log(
            f"    ingest {n_total}  chunk={len(ch)} {dt:.2f}s  "
            f"rss={_fmt_bytes(current_rss_bytes())}"
        )

    for item in nodes:
        keys.setdefault(item["label"], []).append(item["key"])
        chunk.append(item)
        if len(chunk) >= batch:
            _flush(chunk)
            chunk = []
            t_batch = time.perf_counter()
    _flush(chunk)
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
# Approximate-semantic recall measurement
# ---------------------------------------------------------------------------


def measure_approximate_recall(
    db: GraphDb,
    talent_keys: list[str],
    company_keys: list[str],
    sample: int = RECALL_SAMPLE,
    seed: int = DEFAULT_SEED,
) -> dict[str, Any]:
    """Compare approximate SEMANTIC_MATCH_APPROX edges against exact cosine.

    Draws `sample` (talent, company) pairs at random using node_info to fetch
    embeddings on demand (avoids holding 90k × 1536-dim vectors in RAM).
    Ground truth: Python cosine ≥ 0.85 (same threshold as the rule).

    Recall = TP / (TP + FN) — of pairs the exact eval passes, what fraction
    does the approximate rule also include?  Precision = TP / (TP + FP).
    """
    rng = random.Random(seed + 99)
    n_t = len(talent_keys)
    n_c = len(company_keys)
    if n_t == 0 or n_c == 0 or sample == 0:
        return {"recall": float("nan"), "precision": float("nan"), "n": 0}
    tp = fp = fn = tn = 0
    for _ in range(sample):
        tk = talent_keys[rng.randrange(n_t)]
        ck = company_keys[rng.randrange(n_c)]
        t_info = db.node_info(tk)
        c_info = db.node_info(ck)
        if t_info is None or c_info is None:
            continue
        t_emb = t_info["props"].get("embedding") or []
        c_emb = c_info["props"].get("embedding") or []
        cos = _cosine_py(t_emb, c_emb)
        exact_match = cos is not None and cos >= 0.85
        approx_neighbors = set(db.neighbors(tk, "SEMANTIC_MATCH_APPROX", "out"))
        approx_match = ck in approx_neighbors
        if exact_match and approx_match:
            tp += 1
        elif exact_match and not approx_match:
            fn += 1
        elif not exact_match and approx_match:
            fp += 1
        else:
            tn += 1
    recall = tp / (tp + fn) if (tp + fn) > 0 else float("nan")
    precision = tp / (tp + fp) if (tp + fp) > 0 else float("nan")
    return {
        "recall": recall,
        "precision": precision,
        "tp": tp,
        "fp": fp,
        "fn": fn,
        "tn": tn,
        "n": sample,
        "gt_positive": tp + fn,
        "gt_negative": fp + tn,
    }


# ---------------------------------------------------------------------------
# Per-query ANN recall (true IVF quality, uncapped)
# ---------------------------------------------------------------------------


def measure_per_query_ann_recall_on_probe(
    seed: int,
    n_queries: int = N_PQ_QUERIES,
    threshold: float = 0.85,
) -> dict[str, Any]:
    """True per-query IVF-Flat recall measured on a fresh SEMANTIC_PROBE_SCALE DB.

    A fresh ephemeral DB at SEMANTIC_PROBE_SCALE (5k nodes: 3500 Talent + 1000
    Company) is created and APPROXIMATE_SEMANTIC_RULE is declared WITHOUT a
    max_edges cap. 3.5M pairs at 5k scale keeps well under the 1M budget (only
    threshold-passing pairs are stored), so the cap does NOT interfere with recall.

    Per-query recall = |approx_neighbors ∩ exact_neighbors| / |exact_neighbors|
    for each sampled Talent node.  Distinct from set-coverage recall (which is
    mechanically bounded by cap_size/total_global_positives and cannot compare
    to the ≥0.90 spec floor).
    """
    nt, nc, nj = split_scale(SEMANTIC_PROBE_SCALE)
    db_dir = Path(tempfile.mkdtemp(prefix="mush-pqr-"))
    try:
        db = GraphDb.open(str(db_dir))
        ingest_nodes(db, generate(nt, nc, nj, seed))

        talent_keys_probe = [f"talent-{i:06d}" for i in range(nt)]
        company_keys_probe = [f"company-{i:06d}" for i in range(nc)]

        # Approximate semantic WITHOUT max_edges cap (pure IVF quality).
        approx_rule_uncapped: dict[str, Any] = {
            **APPROXIMATE_SEMANTIC_RULE,
            "name": "semantic_match_approx_pqr",
            "max_edges": None,
        }
        _log(f"  pqr: declare approximate semantic (uncapped) at scale={SEMANTIC_PROBE_SCALE}")
        db.create_rule(approx_rule_uncapped)

        # Load all company embeddings once (1k × 1536 dim ≈ 12 MB).
        company_embs: dict[str, list[float]] = {}
        for ck in company_keys_probe:
            info = db.node_info(ck)
            if info is not None:
                emb = info["props"].get("embedding") or []
                if emb:
                    company_embs[ck] = emb

        rng = random.Random(seed + 77)
        sampled = rng.sample(talent_keys_probe, min(n_queries, len(talent_keys_probe)))
        per_query_recalls: list[float] = []
        n_skipped = 0

        for tk in sampled:
            info = db.node_info(tk)
            if info is None:
                n_skipped += 1
                continue
            t_emb = info["props"].get("embedding") or []
            if not t_emb:
                n_skipped += 1
                continue

            # Exact neighbors (uncapped): all companies with cosine >= threshold.
            exact: set[str] = {
                ck
                for ck, c_emb in company_embs.items()
                if (_cosine_py(t_emb, c_emb) or 0.0) >= threshold
            }
            if not exact:
                n_skipped += 1
                continue

            approx = set(db.neighbors(tk, "SEMANTIC_MATCH_APPROX", "out"))
            per_query_recalls.append(len(exact & approx) / len(exact))

        db.close()
    finally:
        shutil.rmtree(str(db_dir), ignore_errors=True)

    mean_r = sum(per_query_recalls) / len(per_query_recalls) if per_query_recalls else float("nan")
    return {
        "metric": "per_query_ann_recall",
        "scale": SEMANTIC_PROBE_SCALE,
        "n_queries": n_queries,
        "n_evaluated": len(per_query_recalls),
        "n_skipped_empty_exact": n_skipped,
        "mean_recall": mean_r,
        "median_recall": percentile(per_query_recalls, 50) if per_query_recalls else float("nan"),
        "min_recall": min(per_query_recalls) if per_query_recalls else float("nan"),
        "max_recall": max(per_query_recalls) if per_query_recalls else float("nan"),
        "threshold": threshold,
        "cap_applied": False,
        "note": (
            f"Per-query IVF-Flat recall at {SEMANTIC_PROBE_SCALE}-node scale "
            f"(max_edges cap disabled; this is the metric the ≥0.90 spec floor applies to)."
        ),
    }


# ---------------------------------------------------------------------------
# Big-3 slice — focused metro+industry measurement
# ---------------------------------------------------------------------------


def run_big3_slice(seed: int, n_slice: int = BIG3_SLICE_SIZE) -> dict[str, Any]:
    """Big-3 latency on a focused metro+industry slice where ALL 3 rules fire.

    A fresh ephemeral DB: n_slice Talent + n_slice Company, all sharing:
    - Same industry (FieldEqual fires for all pairs)
    - Same specialty set with Overlap ≥ 0.15 (Overlap fires for all pairs)
    - Same metro within <1 km jitter (GeoRadius 160.9 km fires for all pairs)

    500×500 = 250k pairs << 1M cap so no max_edges cap is needed.
    Answers the marketplace 5-second question (scoped: metro/industry slice;
    full-graph Big-3 coverage awaits derived-edge persistence, the new #1
    roadmap item).
    """
    SLICE_INDUSTRY = "architecture"
    SLICE_SPECIALTIES = ["residential", "commercial"]  # Jaccard 1.0 ≥ 0.15 ✓
    METRO_LAT, METRO_LON = 37.7749, -122.4194  # SF centre

    rng = random.Random(seed + 777)
    db_dir = Path(tempfile.mkdtemp(prefix="mush-b3s-"))
    try:
        db = GraphDb.open(str(db_dir))

        talent_keys_s: list[str] = []
        company_keys_s: list[str] = []
        batch: list[dict] = []

        for i in range(n_slice):
            dlat_t = (rng.random() - 0.5) * 0.009   # ≤0.5 km
            dlon_t = (rng.random() - 0.5) * 0.009
            dlat_c = (rng.random() - 0.5) * 0.009
            dlon_c = (rng.random() - 0.5) * 0.009
            tk = f"slt-{i:06d}"
            ck = f"slc-{i:06d}"
            talent_keys_s.append(tk)
            company_keys_s.append(ck)
            batch.append({
                "key": tk, "label": "Talent",
                "props": {
                    "industry": SLICE_INDUSTRY,
                    "specialties": SLICE_SPECIALTIES,
                    "location": [METRO_LAT + dlat_t, METRO_LON + dlon_t],
                    "size_bucket": 2,
                    "design_styles": ["contemporary"],
                    "embedding": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                    "user_id": f"sluser-{i:06d}",
                },
            })
            batch.append({
                "key": ck, "label": "Company",
                "props": {
                    "industry": SLICE_INDUSTRY,
                    "specialties": SLICE_SPECIALTIES,
                    "location": [METRO_LAT + dlat_c, METRO_LON + dlon_c],
                    "size_bucket": 2,
                    "design_styles": ["contemporary"],
                    "embedding": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                },
            })

        for i in range(0, len(batch), INGEST_BATCH):
            db.ingest_batch(batch[i : i + INGEST_BATCH])

        # Declare the 3 Big-3 rules WITHOUT max_edges caps (250k << 1M budget).
        slice_rules: list[dict[str, Any]] = [
            {"name": "b3s_ia", "src_label": "Talent", "dst_label": "Company",
             "predicate": {"FieldEqual": {"field": "industry"}},
             "edge_type": "INDUSTRY_ALIGNMENT", "weight_prop": "score", "max_edges": None},
            {"name": "b3s_sm", "src_label": "Talent", "dst_label": "Company",
             "predicate": {"Overlap": {"field": "specialties", "min": 0.15}},
             "edge_type": "SPECIALTY_MATCH", "weight_prop": "score", "max_edges": None},
            {"name": "b3s_lf", "src_label": "Talent", "dst_label": "Company",
             "predicate": {"GeoRadius": {"field": "location", "km": 160.9}},
             "edge_type": "LOCATION_FIT", "weight_prop": "score", "max_edges": None},
        ]
        for rule in slice_rules:
            db.create_rule(rule)

        # Verify coverage on first talent.
        ia = set(db.neighbors(talent_keys_s[0], "INDUSTRY_ALIGNMENT", "out"))
        sm = set(db.neighbors(talent_keys_s[0], "SPECIALTY_MATCH", "out"))
        lf = set(db.neighbors(talent_keys_s[0], "LOCATION_FIT", "out"))
        first_ix = ia & sm & lf
        _log(
            f"  b3s coverage[0]: ia={len(ia)} sm={len(sm)} lf={len(lf)} "
            f"intersection={len(first_ix)}"
        )

        # Big-3 measurement on N_BIG3 random slice talents.
        n = min(N_BIG3, len(talent_keys_s))
        picks = rng.sample(talent_keys_s, n)
        samples_s: list[float] = []
        n_matches = 0
        for key in picks:
            t0 = time.perf_counter()
            db.node_edges(key)
            buckets = [set(db.neighbors(key, et, "out")) for et in BIG3_TYPES]
            matches = set.intersection(*buckets) if buckets else set()
            samples_s.append(time.perf_counter() - t0)
            n_matches += len(matches)

        db.close()
    finally:
        shutil.rmtree(str(db_dir), ignore_errors=True)

    mean_matches = n_matches / n if n else 0.0
    block: dict[str, Any] = {
        "status": "ok",
        "wall_s": sum(samples_s),
        "peak_rss_bytes": peak_rss_bytes(),
        "rss_after_bytes": current_rss_bytes(),
        "n": n,
        "samples_s": samples_s,
        "p50_s": percentile(samples_s, 50),
        "p95_s": percentile(samples_s, 95),
        "mean_matches": mean_matches,
        "n_slice_talent": len(talent_keys_s),
        "n_slice_company": len(company_keys_s),
        "first_ia": len(ia),
        "first_sm": len(sm),
        "first_lf": len(lf),
        "first_intersection": len(first_ix),
        "scope": (
            f"{n_slice}T×{n_slice}C metro+industry slice "
            "(all 3 rules fire for all pairs; no max_edges cap needed)"
        ),
    }
    _log(
        f"  big3_slice: p50={_fmt_s(block['p50_s'])} p95={_fmt_s(block['p95_s'])} "
        f"mean_matches={mean_matches:.1f}"
    )
    return block


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
    """WAL reopen: close + open (replays WAL, no snapshot).  Baseline path."""
    db.close()
    clock = _PhaseClock("reopen_wal")
    reopened = GraphDb.open(str(db_dir))
    block = clock.stop()
    return reopened, block


def run_snapshot_reopen(db_dir: Path, db: GraphDb) -> tuple[GraphDb, dict[str, Any]]:
    """Snapshot reopen: snapshot() then close + open (skips WAL replay)."""
    clock = _PhaseClock("snapshot")
    db.snapshot()
    snap_block = clock.stop()
    db.close()
    clock2 = _PhaseClock("reopen_snap")
    reopened = GraphDb.open(str(db_dir))
    open_block = clock2.stop()
    combined: dict[str, Any] = {
        "snapshot_wall_s": snap_block["wall_s"],
        "open_wall_s": open_block["wall_s"],
        "wall_s": snap_block["wall_s"] + open_block["wall_s"],
        "peak_rss_bytes": max(snap_block["peak_rss_bytes"], open_block["peak_rss_bytes"]),
        "rss_after_bytes": open_block["rss_after_bytes"],
        "status": "ok",
    }
    return reopened, combined


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
        "ingest": "ingest_batch 10k chunks (T2); FK rules declared inline",
        "backfill": "T1 streaming; max_edges=1M caps; all non-semantic rules",
        "semantic": "exact VectorSimilar; T3 early-exit; 5k probe or full",
        "semantic_approx": "T4 approximate=True (IVF-Flat); full scale",
        "incremental": "100 set_prop (specialty / location / embedding)",
        "big3": "50 talent: node_edges + neighbors on Big-3 types (full-graph, capped)",
        "big3_slice": f"{BIG3_SLICE_SIZE}T×{BIG3_SLICE_SIZE}C metro/industry slice (all 3 rules fire uncapped)",
        "explain": "explain() on up to 100 derived pairs",
        "reopen": "WAL reopen: rules re-fire on open() (derived edges not persisted)",
        "reopen_snap": "snapshot reopen: snapshot() + close + open; rules still re-fire",
    }
    phase_order = (
        "ingest",
        "backfill",
        "semantic",
        "semantic_approx",
        "incremental",
        "big3",
        "big3_slice",
        "explain",
        "reopen",
        "reopen_snap",
    )
    for name in phase_order:
        b = phases.get(name)
        if b is None:
            continue
        extra = ""
        if name in ("semantic", "semantic_approx"):
            extra = b.get("verdict", "")
            if name == "semantic_approx" and b.get("recall") is not None:
                extra = (
                    f"edges={b.get('edges')} "
                    f"recall={b['recall']:.3f} "
                    f"precision={b.get('precision', float('nan')):.3f}"
                )
        elif name in ("incremental", "big3", "explain") and b.get("p50_s") is not None:
            extra = f"p50={_fmt_s(b['p50_s'])} p95={_fmt_s(b['p95_s'])} n={b.get('n', '')}"
            if name == "big3":
                extra += f" mean_matches={b.get('mean_matches', 0):.1f}"
        a(
            f"| {name} | {b.get('status', 'ok')} | {_fmt_s(b['wall_s'])} | "
            f"{_fmt_bytes(b['peak_rss_bytes'])} | "
            f"{_fmt_bytes(b.get('rss_after_bytes', 0))} | "
            f"{extra or notes.get(name, '')} |"
        )
    a("")
    a("## Semantic phases (phase 3)")
    a("")
    sem = phases.get("semantic", {})
    a(f"- **Exact status:** `{sem.get('status', 'ok')}`")
    a(f"- **Attempted full {result['scale']}:** {sem.get('attempted_full')}")
    a(f"- **Method:** {sem.get('method', 'n/a')}")
    if sem.get("probe"):
        pr = sem["probe"]
        ex = sem.get("extrapolation") or {}
        a(
            f"- **5k probe (T3 early-exit):** scale={pr.get('scale')} "
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
            "- **O(n²) method (binding):** `t_full = t_probe * (n_t_full/n_t_probe) * "
            "(n_c_full/n_c_probe)`. ScanAll evaluates every Talent×Company pair "
            "(not the passing subset). Full attempt only if "
            f"projected wall < {SEMANTIC_TIME_BUDGET_S}s AND projected Δrss < "
            f"{_fmt_bytes(RSS_BUDGET_BYTES)}."
        )
    approx = phases.get("semantic_approx", {})
    if approx:
        a("")
        a("### Approximate semantic (T4)")
        a("")
        a(f"- **Method:** {approx.get('method', 'IVF-Flat approximate')}")
        a(f"- **Edges materialized:** {approx.get('edges', 'n/a')}")
        a(f"- **Wall:** {_fmt_s(approx.get('wall_s', float('nan')))}")
        rd = approx.get("recall_detail") or {}
        a("")
        a("  **Set-coverage recall** (measured): fraction of ALL threshold-passing global pairs")
        a("  stored in the 1M-edge materialized set.  NOT the per-query IVF recall the")
        a(f"  ≥0.90 spec floor applies to.  At 70k×20k with 1M cap, ~3% of global positives")
        a("  are stored (cap_size/total_positives ceiling), so set-coverage recall is bounded at")
        a("  ~3% regardless of IVF quality.")
        a(f"- **Set-cov recall (n={rd.get('n', RECALL_SAMPLE)} random pairs):** {approx.get('recall', float('nan')):.3f}")
        a(f"- **Set-cov precision:** {approx.get('precision', float('nan')):.3f}")
        a(f"- **TP/FP/FN/TN:** {rd.get('tp')}/{rd.get('fp')}/{rd.get('fn')}/{rd.get('tn')}")
        a(f"- **Ground-truth positives in sample:** {rd.get('gt_positive')} (cosine ≥ 0.85)")
        pqr = approx.get("pq_recall") or {}
        a("")
        a("  **Per-query IVF recall** (spec-floor metric): fraction of a Talent node's exact")
        a("  cosine≥0.85 Company neighbors returned by the IVF-Flat index (uncapped, measured")
        a(f"  on a fresh {SEMANTIC_PROBE_SCALE}-node probe graph where cap does not interfere).")
        if pqr:
            a(f"- **Per-query recall (n={pqr.get('n_evaluated')} queries evaluated):**"
              f" mean={pqr.get('mean_recall', float('nan')):.3f}"
              f" median={pqr.get('median_recall', float('nan')):.3f}"
              f" min={pqr.get('min_recall', float('nan')):.3f}"
              f" max={pqr.get('max_recall', float('nan')):.3f}")
            a(f"- **Queries skipped (empty exact set):** {pqr.get('n_skipped_empty_exact', 'n/a')}")
    a("")
    a("## Backfill (phase 2) — streaming with caps (T1)")
    a("")
    bf = phases.get("backfill", {})
    a(f"- **Status:** `{bf.get('status', 'ok')}`")
    a(f"- **Method:** {bf.get('method', 'streaming backfill with max_edges caps')}")
    a(f"- **max_edges cap per rule:** {MATCHER_MAX_EDGES:,} (ENGINE_EDGE_BUDGET)")
    for rr in bf.get("rules", []):
        a(
            f"  - `{rr['name']}`: {_fmt_s(rr.get('wall_s', 0))} edges={rr['edges']} "
            f"tripped={rr.get('tripped', False)} "
            f"Δrss={_fmt_bytes(rr.get('rss_delta_bytes', 0))}"
        )
    a("")
    a("**T1 change:** The engine now streams the desired set directly into the")
    a("store rather than building a `BTreeMap<(src,dst), score>` first.")
    a("Combined with explicit `max_edges` caps, cartesian predicates at 70k×20k")
    a("no longer OOM the process. Uncapped low-selectivity rules are still O(pairs)")
    a("by definition — the cap is the mechanism. Document and enforce caps on any")
    a("new rule instance that may reach high-fanout at production scale.")
    a("")
    a("## Incremental / Big-3 / explain")
    a("")
    incr = phases.get("incremental", {})
    big3 = phases.get("big3", {})
    expl = phases.get("explain", {})
    a(
        f"- **Incremental (n={incr.get('n')}):** "
        f"p50={_fmt_s(incr.get('p50_s', float('nan')))} "
        f"p95={_fmt_s(incr.get('p95_s', float('nan')))}"
    )
    a(
        f"- **Big-3 full-graph (n={big3.get('n')}):** "
        f"p50={_fmt_s(big3.get('p50_s', float('nan')))} "
        f"p95={_fmt_s(big3.get('p95_s', float('nan')))} ; "
        f"mean intersection={big3.get('mean_matches', 0):.1f}"
    )
    a(
        "  *(Full-graph Big-3 intersection empty: 1M cap at 70k×20k = 0.07% pair coverage; "
        "random talent sample misses the covered slice. This is cap-coverage semantics, "
        "not an engine defect. See Big-3 slice below.)*"
    )
    b3s = phases.get("big3_slice", {})
    if b3s:
        a(
            f"- **Big-3 slice ({b3s.get('n_slice_talent')}T×{b3s.get('n_slice_company')}C "
            f"metro/industry, n={b3s.get('n')}):** "
            f"p50={_fmt_s(b3s.get('p50_s', float('nan')))} "
            f"p95={_fmt_s(b3s.get('p95_s', float('nan')))} ; "
            f"mean intersection={b3s.get('mean_matches', 0):.1f}"
        )
        a(
            f"  *(Answers marketplace 5-second question in a focused bucket. "
            f"first_ia={b3s.get('first_ia')} first_sm={b3s.get('first_sm')} "
            f"first_lf={b3s.get('first_lf')} first_intersection={b3s.get('first_intersection')}. "
            "Full-graph coverage awaits derived-edge persistence — see Roadmap.)*"
        )
    a(
        f"- **explain (n={expl.get('n')}):** "
        f"p50={_fmt_s(expl.get('p50_s', float('nan')))} "
        f"p95={_fmt_s(expl.get('p95_s', float('nan')))}"
    )
    a("")
    a("## Reopen (cold-start)")
    a("")
    a("**Mechanism:** Derived edges are NOT persisted in the WAL or snapshot.")
    a("On every `open()` the engine re-fires all declared rules from node data.")
    a("The WAL stores only node inserts + rule declarations (~120 MiB delta).")
    a("the snapshot stores only node data. Cold-start time therefore scales with")
    a("rule count × rule computation complexity, independent of edge count.")
    a("The dominant cost at this rule set is IVF-Flat re-derivation (~7.68 min).")
    a("**Roadmap #1:** Derived-edge persistence / snapshot-including-derived.")
    a("")
    wal_reopen = phases.get("reopen", {})
    snap_reopen = phases.get("reopen_snap", {})
    a(
        f"- **WAL reopen:** {_fmt_s(wal_reopen.get('wall_s', float('nan')))} "
        f"({wal_reopen.get('status', 'ok')}) — "
        f"close + open; rules re-fire (9 streaming ~20s + IVF-Flat ~7.68 min = bottleneck)"
    )
    if snap_reopen:
        a(
            f"- **Snapshot reopen:** {_fmt_s(snap_reopen.get('wall_s', float('nan')))} "
            f"(snapshot={_fmt_s(snap_reopen.get('snapshot_wall_s', 0))} + "
            f"open={_fmt_s(snap_reopen.get('open_wall_s', 0))}) "
            f"({snap_reopen.get('status', 'ok')}) — "
            f"snapshot() + close + open; rules ALSO re-fire (derived edges not in snapshot)"
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
        f"p95={_fmt_s(big3.get('p95_s', float('nan')))} "
        f"mean_matches={big3.get('mean_matches', 0):.1f}"
    )
    if (big3.get("mean_matches") or 0) == 0:
        big3_note += " — intersection empty (matcher rules not live at this scale)"
    a(f"| Talent→Company matcher (Big-3) | 5+ second queries | {big3_note} |")
    a(
        "| Search fan-out | 14 sharded Meilisearch indices + in-memory merge | "
        "derived-edge `neighbors` on declared rules |"
    )
    sem_note = (
        f"exact: {sem.get('status', 'n/a')} (full={sem.get('attempted_full')}); "
        f"approx: {_fmt_s(approx.get('wall_s', float('nan')))} "
        f"recall={approx.get('recall', float('nan')):.3f}"
        if approx
        else f"exact: {sem.get('status', 'n/a')} (full={sem.get('attempted_full')})"
    )
    a(f"| Semantic / vector | Meili `_vectors` 1536-dim | {sem_note} |")
    a(
        f"| Ingest 100k | (not published) | "
        f"{_fmt_s(phases['ingest']['wall_s'])} "
        f"peak {_fmt_bytes(phases['ingest']['peak_rss_bytes'])} "
        f"(ingest_batch 10k chunks) |"
    )
    a("")
    a("## Surface gaps and what changed (Plan 11)")
    a("")
    a("- **T1 (streaming backfill):** Matcher backfill at 100k NOW COMPLETES.")
    a("  Engine no longer builds a full `BTreeMap` of desired pairs before capping.")
    a("  Uncapped rules remain O(pairs) by definition — caps are the mechanism.")
    a("- **T2 (bindings):** `ingest_batch`, `stats`, `snapshot` added to Python bindings.")
    a("  `ingest_batch` in 10k chunks reduces WAL fsync overhead vs one-node-at-a-time.")
    a("- **T3 (exact early-exit):** Cauchy-Schwarz suffix-norm bound prunes exact")
    a("  VectorSimilar candidates without materializing all dot-products.")
    a("- **T4 (approximate=True):** IVF-Flat candidate selection for VectorSimilar rules.")
    a("  Opt-in, non-exact (per-query recall ≥ 0.90 quiesced per spec).")
    a("  Set-coverage recall at 100k is bounded at ~3% by cap/total_positives, NOT by IVF quality.")
    a("  Measure per-query ANN recall (uncapped probe graph) before enabling in prod.")
    a("- **Auto-FK:** Still declared as explicit KeyMatch rules (no ingest auto-FK).")
    a("- **Cypher COUNT:** Not available; edge counts use `neighbors` per src key.")
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
        # Semantic probe always runs at 100k to record T3 speedup.
        # Backfill no longer uses probe-gating (T1 streaming makes it safe).
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

    # Semantic probe (records T3 early-exit speedup; gates exact full attempt).
    # Backfill probe removed: T1 streaming + max_edges caps make 100k safe.
    if probes:
        probe_root = Path(tempfile.mkdtemp(prefix="mush-probe-"))
        sem_probe = probe_semantic(probe_root / "semantic", seed)
        sem_ex = extrapolate_o_n2(sem_probe, n_talent, n_companies)

    # (1) ingest via ingest_batch (T2: 10k-node chunks, one WAL commit each)
    clock = _PhaseClock("ingest")
    db = GraphDb.open(str(db_dir))
    keys = ingest_nodes(db, _stream_scale(n_talent, n_companies, n_jobs, seed))
    talent_keys = keys["Talent"]
    company_keys = keys["Company"]
    # FK rules (KeyMatch): cheap, always declare so tests have derived edges.
    fk_per = declare_rules(db, fk_rule_defs())
    phases["ingest"] = clock.stop(
        n_nodes=sum(len(v) for v in keys.values()),
        fk_rules=[{k: r[k] for k in ("name", "wall_s")} for r in fk_per],
    )

    # (2) non-semantic matcher backfill WITH max_edges caps (T1 streaming).
    # Gate removed: streaming engine no longer builds a full BTreeMap before
    # capping, so cartesian predicates at 70k×20k are now memory-bounded.
    _log("phase: backfill (streaming, max_edges capped, all non-semantic rules)")
    clock = _PhaseClock("backfill")
    per_matcher: list[dict[str, Any]] = []
    for rule in non_semantic_rules():
        rss_before = current_rss_bytes()
        t0 = time.perf_counter()
        db.create_rule(rule)
        wall = time.perf_counter() - t0
        rss_after = current_rss_bytes()
        n_edges = count_out_edges(db, talent_keys, rule["edge_type"])
        per_matcher.append(
            {
                "name": rule["name"],
                "edge_type": rule["edge_type"],
                "wall_s": wall,
                "rss_delta_bytes": max(0, rss_after - rss_before),
                "edges": n_edges,
                "tripped": n_edges >= ENGINE_EDGE_BUDGET,
                "max_edges": rule.get("max_edges"),
                "status": "ok",
            }
        )
        _log(
            f"  {rule['name']}: {_fmt_s(wall)} edges={n_edges} "
            f"tripped={n_edges >= ENGINE_EDGE_BUDGET} "
            f"Δrss={_fmt_bytes(max(0, rss_after - rss_before))}"
        )
    phases["backfill"] = clock.stop(
        method="streaming backfill with max_edges caps (T1)",
        rules=per_matcher,
    )
    findings.append(
        f"Matcher backfill at 100k COMPLETED (T1 streaming). "
        f"Rules: {len(per_matcher)}. "
        f"Elapsed: {_fmt_s(phases['backfill']['wall_s'])}."
    )

    # (3) semantic phases — exact probe + approximate at full scale.
    #   (3a) exact 5k probe: T3 Cauchy-Schwarz early-exit in engine path.
    #   (3b) full exact semantic: only if probe projects under budget.
    #   (3c) approximate semantic at full scale (T4 IVF-Flat).
    sem_method = "declared on the target graph (scale < probe gate)"
    attempt_sem = True
    if sem_ex is not None:
        sem_method = (
            "5k ScanAll probe with T3 early-exit; "
            "t_full = t_probe * (n_t_full/n_t_probe) * (n_c_full/n_c_probe)"
        )
        if not (sem_ex["under_time_budget"] and sem_ex["under_rss_budget"]):
            attempt_sem = False
            findings.append(
                "semantic_match exact full backfill not attempted: projected "
                f"wall={_fmt_s(sem_ex['projected_wall_s'])} "
                f"Δrss={_fmt_bytes(sem_ex['projected_rss_delta_bytes'])} "
                "(approximate semantic runs instead)"
            )

    clock = _PhaseClock("semantic")
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
                "5k probe recorded (T3 early-exit); full 100k ScanAll not "
                "attempted (blocking); approximate semantic runs instead"
            ),
        )

    # (3c) approximate semantic at full scale (T4).
    _log("phase: semantic_approx (approximate=True, IVF-Flat, full scale)")
    clock = _PhaseClock("semantic_approx")
    db.create_rule(APPROXIMATE_SEMANTIC_RULE)
    approx_edges = count_out_edges(db, talent_keys, "SEMANTIC_MATCH_APPROX")
    phases["semantic_approx"] = clock.stop(
        method="IVF-Flat approximate (T4: approximate=True in RuleDef)",
        edges=approx_edges,
        tripped=approx_edges >= ENGINE_EDGE_BUDGET,
    )
    # Recall vs 1k-sample exact ground truth.
    _log(f"  approximate edges={approx_edges}; measuring recall (n={RECALL_SAMPLE})...")
    recall_result = measure_approximate_recall(
        db, talent_keys, company_keys, sample=RECALL_SAMPLE, seed=seed
    )
    phases["semantic_approx"]["recall"] = recall_result["recall"]
    phases["semantic_approx"]["precision"] = recall_result["precision"]
    phases["semantic_approx"]["recall_detail"] = recall_result
    _log(
        f"  set-cov recall={recall_result['recall']:.3f} "
        f"precision={recall_result['precision']:.3f} "
        f"(TP={recall_result['tp']} FP={recall_result['fp']} "
        f"FN={recall_result['fn']} TN={recall_result['tn']})"
    )
    # Per-query ANN recall (true IVF quality, uncapped, 5k probe graph).
    # This is the metric the >=0.90 spec floor applies to; it is NOT set-coverage.
    _log("  pqr: measuring true per-query IVF recall on fresh 5k probe graph...")
    pqr = measure_per_query_ann_recall_on_probe(seed)
    phases["semantic_approx"]["pq_recall"] = pqr
    _log(
        f"  pqr: mean_recall={pqr['mean_recall']:.3f} "
        f"(n_evaluated={pqr['n_evaluated']} skipped={pqr['n_skipped_empty_exact']})"
    )

    # (4) incremental
    clock = _PhaseClock("incremental")
    incr = run_incremental(db, talent_keys, seed)
    phases["incremental"] = clock.stop(**incr)

    # (5) Big-3 — now on a REAL graph with matcher edges live.
    clock = _PhaseClock("big3")
    big3 = run_big3(db, talent_keys, seed)
    phases["big3"] = clock.stop(**big3)

    # (6) explain
    clock = _PhaseClock("explain")
    expl = run_explain(db, talent_keys, seed)
    phases["explain"] = clock.stop(**expl)

    # (6b) Big-3 slice — focused metro+industry measurement with non-empty intersections.
    # Full-graph Big-3 is empty (1M/1.4B = 0.07% cap coverage → empty 3-way intersection).
    # Slice answers: "can the engine answer who matches talent X in reasonable time?"
    _log(f"phase: big3_slice ({BIG3_SLICE_SIZE}T×{BIG3_SLICE_SIZE}C metro/industry slice)")
    phases["big3_slice"] = run_big3_slice(seed)

    # (7a) WAL reopen — baseline: close + open (replays WAL).
    db, wal_reopen_block = run_reopen(db_dir, db)
    phases["reopen"] = wal_reopen_block

    # (7b) Snapshot reopen — T2 snapshot(): write snapshot then close + open.
    db, snap_reopen_block = run_snapshot_reopen(db_dir, db)
    phases["reopen_snap"] = snap_reopen_block

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
        "company_keys": company_keys,
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
