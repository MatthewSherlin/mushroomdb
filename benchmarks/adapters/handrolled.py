"""Hand-rolled edge-maintenance adapter for the mushroomdb comparative benchmark.

Mirrors two rules against mushroomdb itself (same engine, same API — only the
maintenance STRATEGY differs):

  Rule 1 — SPECIALTY_MATCH:
      Talent → Company  |  Overlap(specialties, min=0.15)
      Jaccard similarity of the ``specialties`` list >= 0.15.

  Rule 2 — SEMANTIC_MATCH:
      Talent → Company  |  VectorSimilar(embedding, min=0.85) [exact cosine]
      Cosine similarity of the 1536-dim ``embedding`` >= 0.85.

The hand-rolled maintainer (this module) manually inserts and deletes
SPECIALTY_MATCH / SEMANTIC_MATCH edges using the Python API.  On ingest it
computes all pairwise matches in bulk (numpy for cosine, Python sets for
jaccard); on update it re-evaluates affected pairs and retracts/adds.

The rule-engine reference uses ``db.create_rule()`` for the same two rules
(exact VectorSimilar).  Same nodes, same updates, same engine — the
comparison isolates the maintenance strategy.

Correctness-drift metric: after all operations, diff the SPECIALTY_MATCH and
SEMANTIC_MATCH edge sets between the hand-rolled DB and the rule-engine DB.

SCALE NOTE: The exact VectorSimilar rule engine at 10k nodes takes ~25 min
(extrapolated from measured 61.6 s at 2k — scales quadratically with node
count).  This benchmark therefore runs at BENCH_SCALE = 2_000 for the
full two-rule comparison.  The SPECIALTY_MATCH rule is ALSO run at 10k
separately in run_handrolled.py to show the scaling advantage of the rule
engine's token inverted-index vs Python set operations.

NOTE: db.delete_edge() was added to the Python bindings in the Task-6
implementation (see bindings/python/src/lib.rs) — the hand-rolled retraction
path depends on it.
"""

from __future__ import annotations

import math
import random
import time
from pathlib import Path
from typing import Any

import numpy as np

SPECIALTY_RULE_EDGE = "SPECIALTY_MATCH"
SEMANTIC_RULE_EDGE = "SEMANTIC_MATCH"
OVERLAP_MIN = 0.15
COSINE_MIN = 0.85

EDGE_BATCH_SIZE = 5_000   # edges per ingest_batch when bulk-inserting
INGEST_CHUNK = 2_000      # nodes per ingest_batch call

# Two update "specialty sets" that flip matches on and off:
#   RARE_SET   — single specialty → retracts most current SPECIALTY_MATCH edges
#   COMMON_SET — 5 popular specialties → adds many SPECIALTY_MATCH edges
_RARE_SET = ["landscape"]
_COMMON_SET = [
    "single-family",
    "multi-family",
    "residential",
    "commercial",
    "retail",
]


# ---------------------------------------------------------------------------
# Predicate helpers (Python equivalents of the engine's Rust predicates)
# ---------------------------------------------------------------------------

def _jaccard(a: list[str], b: list[str]) -> float:
    """Jaccard similarity of two lists (treated as sets)."""
    sa, sb = set(a), set(b)
    inter = len(sa & sb)
    if inter == 0:
        return 0.0
    union = len(sa | sb)
    return inter / union if union > 0 else 0.0


# ---------------------------------------------------------------------------
# DB helper: collect all edges of a given type from talent nodes
# ---------------------------------------------------------------------------

def collect_edge_set(db: Any, talent_keys: list[str], edge_type: str) -> set[tuple[str, str]]:
    """Return the set of (src_key, dst_key) for *edge_type* outbound from talents."""
    result: set[tuple[str, str]] = set()
    for tkey in talent_keys:
        try:
            for e in db.node_edges(tkey):
                if e["edge_type"] == edge_type and e["src_key"] == tkey:
                    result.add((tkey, e["dst_key"]))
        except Exception:
            pass
    return result


# ---------------------------------------------------------------------------
# Hand-rolled benchmark
# ---------------------------------------------------------------------------

def run_handrolled(
    nodes: list[dict],
    db_dir: str | Path,
    updates: list[tuple[str, list[str]]],
) -> dict[str, Any]:
    """Run the hand-rolled edge-maintenance workload.

    Computes SPECIALTY_MATCH via Python Jaccard and SEMANTIC_MATCH via numpy
    cosine matrix multiply.  Uses insert_edge / delete_edge via the Python API.

    Parameters
    ----------
    nodes:
        Full node list (same as the benchmark dataset).
    db_dir:
        Temporary directory for the hand-rolled DB.
    updates:
        List of (talent_key, new_specialties) to apply after ingest.

    Returns
    -------
    dict with timing keys and the final edge sets for drift comparison.
    """
    from mushroomdb import GraphDb  # type: ignore[import]

    db = GraphDb.open(str(db_dir))

    # In-memory caches for candidate lookup (avoids repeated DB reads)
    talent_index: dict[str, dict] = {}    # key → props
    company_index: dict[str, dict] = {}  # key → props

    # --- Phase 1: ingest (with incremental edge maintenance) ---------------
    t0_ingest = time.perf_counter()

    for chunk_start in range(0, len(nodes), INGEST_CHUNK):
        chunk = nodes[chunk_start : chunk_start + INGEST_CHUNK]
        db.ingest_batch(chunk)

        # Update local index
        new_talents: list[tuple[str, dict]] = []
        new_companies: list[tuple[str, dict]] = []
        for n in chunk:
            p = n["props"]
            if n["label"] == "Talent":
                talent_index[n["key"]] = p
                new_talents.append((n["key"], p))
            elif n["label"] == "Company":
                company_index[n["key"]] = p
                new_companies.append((n["key"], p))

        # --- Specialty edges: new Talent vs. existing Companies
        spec_edges: list[dict] = []
        for tkey, tp in new_talents:
            t_specs = tp.get("specialties") or []
            for ckey, cp in company_index.items():
                if _jaccard(t_specs, cp.get("specialties") or []) >= OVERLAP_MIN:
                    spec_edges.append({"edge_type": SPECIALTY_RULE_EDGE, "src": tkey, "dst": ckey})

        # --- Specialty edges: new Company vs. existing Talents (not in this chunk)
        existing_talent_keys = [k for k in talent_index if k not in {t for t, _ in new_talents}]
        for ckey, cp in new_companies:
            c_specs = cp.get("specialties") or []
            for tkey in existing_talent_keys:
                tp = talent_index[tkey]
                if _jaccard(tp.get("specialties") or [], c_specs) >= OVERLAP_MIN:
                    spec_edges.append({"edge_type": SPECIALTY_RULE_EDGE, "src": tkey, "dst": ckey})

        # --- Semantic edges: use numpy for bulk cosine
        sem_edges: list[dict] = []

        # New Talent vs. existing Companies
        t_valid = [(tkey, tp) for tkey, tp in new_talents if tp.get("embedding")]
        if t_valid and company_index:
            c_keys_list = list(company_index.keys())
            c_embs = [company_index[k].get("embedding") for k in c_keys_list]
            c_valid_idx = [i for i, e in enumerate(c_embs) if e]
            c_valid_keys = [c_keys_list[i] for i in c_valid_idx]
            c_valid_embs = [c_embs[i] for i in c_valid_idx]
            if c_valid_embs:
                T_mat = np.array([tp["embedding"] for _, tp in t_valid], dtype=np.float32)
                C_mat = np.array(c_valid_embs, dtype=np.float32)
                cos_mat = np.minimum(T_mat @ C_mat.T, 1.0)
                tis, cis = np.where(cos_mat >= COSINE_MIN)
                for ti, ci in zip(tis.tolist(), cis.tolist()):
                    sem_edges.append({
                        "edge_type": SEMANTIC_RULE_EDGE,
                        "src": t_valid[ti][0],
                        "dst": c_valid_keys[ci],
                    })

        # New Company vs. existing Talents (not in this chunk)
        c_valid_new = [(ckey, cp) for ckey, cp in new_companies if cp.get("embedding")]
        if c_valid_new and existing_talent_keys:
            t_embs_ex = [(k, talent_index[k].get("embedding")) for k in existing_talent_keys]
            t_valid_ex = [(k, e) for k, e in t_embs_ex if e]
            if t_valid_ex:
                T_mat2 = np.array([e for _, e in t_valid_ex], dtype=np.float32)
                C_mat2 = np.array([cp["embedding"] for _, cp in c_valid_new], dtype=np.float32)
                cos_mat2 = np.minimum(T_mat2 @ C_mat2.T, 1.0)
                tis2, cis2 = np.where(cos_mat2 >= COSINE_MIN)
                for ti, ci in zip(tis2.tolist(), cis2.tolist()):
                    sem_edges.append({
                        "edge_type": SEMANTIC_RULE_EDGE,
                        "src": t_valid_ex[ti][0],
                        "dst": c_valid_new[ci][0],
                    })

        # Bulk-insert all computed edges
        all_edges = spec_edges + sem_edges
        if all_edges:
            for b in range(0, len(all_edges), EDGE_BATCH_SIZE):
                batch_edges = all_edges[b : b + EDGE_BATCH_SIZE]
                try:
                    db.ingest_batch([], batch_edges)
                except Exception:
                    # Duplicates or rule-conflict: fall back to individual inserts
                    for e in batch_edges:
                        try:
                            db.insert_edge(e["edge_type"], e["src"], e["dst"])
                        except Exception:
                            pass

    ingest_wall = time.perf_counter() - t0_ingest

    # --- Phase 2: property updates with retraction + addition --------------
    # Use db.batch_edges() to commit all retractions + additions in one WAL
    # frame per update, instead of one WAL fsync per delete_edge/insert_edge.
    t0_update = time.perf_counter()
    retraction_count = 0
    addition_count = 0

    for tkey, new_specs in updates:
        # Fetch current SPECIALTY_MATCH edges for this talent
        current_spec_neighbors: set[str] = set()
        try:
            for e in db.node_edges(tkey):
                if e["edge_type"] == SPECIALTY_RULE_EDGE and e["src_key"] == tkey:
                    current_spec_neighbors.add(e["dst_key"])
        except Exception:
            pass

        # Apply the property update
        try:
            db.set_prop(tkey, "specialties", new_specs)
        except Exception:
            pass
        if tkey in talent_index:
            talent_index[tkey]["specialties"] = new_specs

        # Re-evaluate SPECIALTY_MATCH for all companies; collect deletes + inserts
        to_delete: list[dict] = []
        to_insert: list[dict] = []
        for ckey, cp in company_index.items():
            matches = _jaccard(new_specs, cp.get("specialties") or []) >= OVERLAP_MIN
            had_edge = ckey in current_spec_neighbors
            if had_edge and not matches:
                to_delete.append({"edge_type": SPECIALTY_RULE_EDGE, "src": tkey, "dst": ckey})
            elif not had_edge and matches:
                to_insert.append({"edge_type": SPECIALTY_RULE_EDGE, "src": tkey, "dst": ckey})

        # Commit all deletes + inserts for this talent in ONE WAL batch
        if to_delete or to_insert:
            try:
                db.batch_edges(inserts=to_insert, deletes=to_delete)
                retraction_count += len(to_delete)
                addition_count += len(to_insert)
            except Exception:
                # Fallback: individual calls
                for e in to_delete:
                    try:
                        db.delete_edge(e["edge_type"], e["src"], e["dst"])
                        retraction_count += 1
                    except Exception:
                        pass
                for e in to_insert:
                    try:
                        db.insert_edge(e["edge_type"], e["src"], e["dst"])
                        addition_count += 1
                    except Exception:
                        pass

        # SEMANTIC_MATCH: embedding is fixed at ingest (from industry+primary_specialty),
        # so specialties updates do NOT change SEMANTIC_MATCH edges.  No retraction needed.

    update_wall = time.perf_counter() - t0_update

    talent_keys = list(talent_index.keys())
    try:
        stats = db.stats()
    except Exception:
        stats = {}
    spec_edge_set = collect_edge_set(db, talent_keys, SPECIALTY_RULE_EDGE)
    sem_edge_set = collect_edge_set(db, talent_keys, SEMANTIC_RULE_EDGE)

    db.close()

    return {
        "engine": "mushroomdb-handrolled",
        "ingest_wall_s": ingest_wall,
        "update_wall_s": update_wall,
        "total_wall_s": ingest_wall + update_wall,
        "retraction_count": retraction_count,
        "addition_count": addition_count,
        "specialty_edge_count": len(spec_edge_set),
        "semantic_edge_count": len(sem_edge_set),
        "db_stats": stats,
        "_spec_edges": spec_edge_set,
        "_sem_edges": sem_edge_set,
    }


# ---------------------------------------------------------------------------
# Rule-engine reference run
# ---------------------------------------------------------------------------

def run_rule_engine(
    nodes: list[dict],
    db_dir: str | Path,
    updates: list[tuple[str, list[str]]],
) -> dict[str, Any]:
    """Run the same workload using mushroomdb's rule engine (exact VectorSimilar).

    Rules:
      bench_hr_spec: Overlap(specialties, min=0.15) → SPECIALTY_MATCH
      bench_hr_sem:  VectorSimilar(embedding, min=0.85) exact → SEMANTIC_MATCH

    NOTE: exact VectorSimilar at 10k scale extrapolates to ~25 min from 2k
    timing of 61.6 s.  This function is designed for ≤ 2k nodes for the
    SEMANTIC rule.  At full 10k scale use run_rule_engine_specialty_only().
    """
    from mushroomdb import GraphDb  # type: ignore[import]

    db = GraphDb.open(str(db_dir))

    # --- Ingest ---
    t0_ingest = time.perf_counter()
    for chunk_start in range(0, len(nodes), INGEST_CHUNK):
        chunk = nodes[chunk_start : chunk_start + INGEST_CHUNK]
        db.ingest_batch(chunk)
    ingest_wall = time.perf_counter() - t0_ingest

    # --- Declare rules (backfill fires here) ---
    # max_edges=10_000_000 → per-source top-10M cap (effectively uncapped
    # at 2k scale: each talent has at most 400 company candidates).
    t0_spec = time.perf_counter()
    db.create_rule({
        "name": "bench_hr_spec",
        "src_label": "Talent",
        "dst_label": "Company",
        "predicate": {"Overlap": {"field": "specialties", "min": OVERLAP_MIN}},
        "edge_type": SPECIALTY_RULE_EDGE,
        "weight_prop": "score",
        "max_edges": 10_000_000,
    })
    spec_wall = time.perf_counter() - t0_spec

    t0_sem = time.perf_counter()
    db.create_rule({
        "name": "bench_hr_sem",
        "src_label": "Talent",
        "dst_label": "Company",
        "predicate": {"VectorSimilar": {"field": "embedding", "min": COSINE_MIN}},
        "edge_type": SEMANTIC_RULE_EDGE,
        "weight_prop": "score",
        "max_edges": 10_000_000,
    })
    sem_wall = time.perf_counter() - t0_sem

    # --- Apply same property updates (rule engine fires incrementally) ---
    t0_update = time.perf_counter()
    for tkey, new_specs in updates:
        try:
            db.set_prop(tkey, "specialties", new_specs)
        except Exception:
            pass
    update_wall = time.perf_counter() - t0_update

    talent_keys = [n["key"] for n in nodes if n["label"] == "Talent"]
    try:
        stats = db.stats()
    except Exception:
        stats = {}
    spec_edge_set = collect_edge_set(db, talent_keys, SPECIALTY_RULE_EDGE)
    sem_edge_set = collect_edge_set(db, talent_keys, SEMANTIC_RULE_EDGE)

    db.close()

    return {
        "engine": "mushroomdb-rule-engine",
        "ingest_wall_s": ingest_wall,
        "spec_backfill_wall_s": spec_wall,
        "sem_backfill_wall_s": sem_wall,
        "rules_backfill_wall_s": spec_wall + sem_wall,
        "update_wall_s": update_wall,
        "total_wall_s": ingest_wall + spec_wall + sem_wall + update_wall,
        "specialty_edge_count": len(spec_edge_set),
        "semantic_edge_count": len(sem_edge_set),
        "db_stats": stats,
        "_spec_edges": spec_edge_set,
        "_sem_edges": sem_edge_set,
    }


def run_rule_engine_specialty_only(
    nodes: list[dict],
    db_dir: str | Path,
    updates: list[tuple[str, list[str]]],
) -> dict[str, Any]:
    """Rule-engine run for SPECIALTY_MATCH only (suitable for 10k scale).

    SEMANTIC_MATCH excluded here because exact VectorSimilar at 10k scale
    takes ~25 min (extrapolated from 61.6 s at 2k — O(n²) cosine evals).
    """
    from mushroomdb import GraphDb  # type: ignore[import]

    db = GraphDb.open(str(db_dir))

    t0_ingest = time.perf_counter()
    for chunk_start in range(0, len(nodes), INGEST_CHUNK):
        chunk = nodes[chunk_start : chunk_start + INGEST_CHUNK]
        db.ingest_batch(chunk)
    ingest_wall = time.perf_counter() - t0_ingest

    t0_spec = time.perf_counter()
    db.create_rule({
        "name": "bench_hr_spec",
        "src_label": "Talent",
        "dst_label": "Company",
        "predicate": {"Overlap": {"field": "specialties", "min": OVERLAP_MIN}},
        "edge_type": SPECIALTY_RULE_EDGE,
        "weight_prop": "score",
        "max_edges": 10_000_000,
    })
    spec_wall = time.perf_counter() - t0_spec

    t0_update = time.perf_counter()
    for tkey, new_specs in updates:
        try:
            db.set_prop(tkey, "specialties", new_specs)
        except Exception:
            pass
    update_wall = time.perf_counter() - t0_update

    talent_keys = [n["key"] for n in nodes if n["label"] == "Talent"]
    try:
        stats = db.stats()
    except Exception:
        stats = {}
    spec_edge_set = collect_edge_set(db, talent_keys, SPECIALTY_RULE_EDGE)

    db.close()

    return {
        "engine": "mushroomdb-rule-engine-spec-only",
        "ingest_wall_s": ingest_wall,
        "spec_backfill_wall_s": spec_wall,
        "rules_backfill_wall_s": spec_wall,
        "update_wall_s": update_wall,
        "total_wall_s": ingest_wall + spec_wall + update_wall,
        "specialty_edge_count": len(spec_edge_set),
        "semantic_edge_count": 0,
        "db_stats": stats,
        "_spec_edges": spec_edge_set,
        "_sem_edges": set(),
    }


# ---------------------------------------------------------------------------
# Top-level: build updates, run both sides, compute drift
# ---------------------------------------------------------------------------

def run_handrolled_vs_rules(
    nodes: list[dict],
    hr_db_dir: str | Path,
    re_db_dir: str | Path,
    n_updates: int = 1_000,
    seed: int = 20260819,
    max_scale_for_semantic: int = 2_000,
) -> dict[str, Any]:
    """Run both maintenance strategies and return a combined result dict.

    At scales > max_scale_for_semantic, the SEMANTIC_MATCH rule (exact
    VectorSimilar) is run on a 2k-node sub-sample to keep the benchmark
    tractable.  The full-scale run uses SPECIALTY_MATCH only for that
    sub-comparison.

    Parameters
    ----------
    nodes:
        The benchmark node list (10k for full run).
    hr_db_dir:
        DB directory for the hand-rolled run (full-scale, both rules).
    re_db_dir:
        DB directory for the rule-engine run.
    n_updates:
        Number of talent property updates to apply.
    seed:
        RNG seed for update target selection.
    max_scale_for_semantic:
        Maximum node count for exact VectorSimilar rule engine comparison.
        At scales above this, the semantic comparison runs separately on the
        first max_scale_for_semantic nodes.

    Returns
    -------
    Combined result with ``handrolled``, ``rule_engine``, ``drift``,
    and optionally ``semantic_2k`` sub-dicts.
    """
    rng = random.Random(seed)

    # Build update list: alternate between RARE_SET and COMMON_SET so each
    # update BOTH retracts old matches (rare) AND adds new matches (common).
    talent_keys_all = [n["key"] for n in nodes if n["label"] == "Talent"]
    update_targets = rng.sample(talent_keys_all, min(n_updates, len(talent_keys_all)))
    updates: list[tuple[str, list[str]]] = []
    for i, tkey in enumerate(update_targets):
        new_specs = _RARE_SET if i % 2 == 0 else _COMMON_SET
        updates.append((tkey, new_specs))

    scale = len(nodes)
    result: dict[str, Any] = {"scale": scale, "n_updates": n_updates}

    # ---- Hand-rolled: full scale, both rules (numpy for cosine) ----
    print(f"  [handrolled] {scale} nodes, {n_updates} updates...", flush=True)
    hr = run_handrolled(nodes, hr_db_dir, updates)
    result["handrolled"] = hr

    # ---- Rule-engine: depends on scale ----
    if scale <= max_scale_for_semantic:
        # Small scale: exact VectorSimilar is tractable
        import tempfile
        re_dir_impl = str(re_db_dir)
        print(f"  [rule-engine] {scale} nodes (both rules, exact VectorSimilar)...", flush=True)
        re = run_rule_engine(nodes, re_db_dir, updates)
        result["rule_engine"] = re
        result["semantic_scale"] = scale

        hr_spec = hr["_spec_edges"]
        re_spec = re["_spec_edges"]
        hr_sem = hr["_sem_edges"]
        re_sem = re["_sem_edges"]
        drift = {
            "specialty_hr_only": len(hr_spec - re_spec),
            "specialty_re_only": len(re_spec - hr_spec),
            "semantic_hr_only": len(hr_sem - re_sem),
            "semantic_re_only": len(re_sem - hr_sem),
            "total_drift": (
                len(hr_spec - re_spec) + len(re_spec - hr_spec)
                + len(hr_sem - re_sem) + len(re_sem - hr_sem)
            ),
        }
        result["drift"] = drift

    else:
        # Large scale: SPECIALTY only for rule engine at full scale;
        # separate 2k run for SEMANTIC comparison
        import tempfile as _tmpmod
        from pathlib import Path as _Path

        print(f"  [rule-engine] {scale} nodes (SPECIALTY only, exact VectorSimilar excluded"
              f" — would take ~{scale**2 / 2_000**2 * 61.6 / 60:.0f} min)...", flush=True)
        re = run_rule_engine_specialty_only(nodes, re_db_dir, updates)
        result["rule_engine"] = re

        # Drift for SPECIALTY only
        hr_spec = hr["_spec_edges"]
        re_spec = re["_spec_edges"]
        drift = {
            "specialty_hr_only": len(hr_spec - re_spec),
            "specialty_re_only": len(re_spec - hr_spec),
            "semantic_hr_only": "N/A (see 2k sub-run)",
            "semantic_re_only": "N/A (see 2k sub-run)",
            "total_drift": len(hr_spec - re_spec) + len(re_spec - hr_spec),
            "note": "SPECIALTY_MATCH drift only; SEMANTIC_MATCH compared at 2k sub-scale below",
        }
        result["drift"] = drift
        result["semantic_scale"] = max_scale_for_semantic

        # 2k sub-run for SEMANTIC comparison — must include Talent + Company nodes.
        # nodes are ordered: Talent (7k), Company (2k), Job (1k) at 10k scale.
        # Build a proportional sub-sample using the same 70/20/10 split.
        print(f"  [semantic-2k] running both sides at {max_scale_for_semantic} nodes...",
              flush=True)
        from datasets import split_scale as _split_scale
        n_t2, n_c2, n_j2 = _split_scale(max_scale_for_semantic)
        talents_pool = [n for n in nodes if n["label"] == "Talent"]
        companies_pool = [n for n in nodes if n["label"] == "Company"]
        jobs_pool = [n for n in nodes if n["label"] == "Job"]
        nodes_2k = (
            talents_pool[:n_t2]
            + companies_pool[:n_c2]
            + jobs_pool[:n_j2]
        )
        rng2 = random.Random(seed + 1)
        t2k_keys = [n["key"] for n in nodes_2k if n["label"] == "Talent"]
        updates_2k = [(k, (_RARE_SET if i % 2 == 0 else _COMMON_SET))
                      for i, k in enumerate(rng2.sample(t2k_keys, min(200, len(t2k_keys))))]

        with _tmpmod.TemporaryDirectory(prefix="bench-hr-2k-") as tmp2:
            hr2k_dir = _Path(tmp2) / "hr2k"
            re2k_dir = _Path(tmp2) / "re2k"

            print("    [handrolled 2k]...", flush=True)
            hr2k = run_handrolled(nodes_2k, hr2k_dir, updates_2k)
            print("    [rule-engine 2k exact]...", flush=True)
            re2k = run_rule_engine(nodes_2k, re2k_dir, updates_2k)

            sem_drift = {
                "semantic_hr_only": len(hr2k["_sem_edges"] - re2k["_sem_edges"]),
                "semantic_re_only": len(re2k["_sem_edges"] - hr2k["_sem_edges"]),
                "total_sem_drift": (
                    len(hr2k["_sem_edges"] - re2k["_sem_edges"])
                    + len(re2k["_sem_edges"] - hr2k["_sem_edges"])
                ),
            }
            result["semantic_2k"] = {
                "handrolled": {k: v for k, v in hr2k.items() if not k.startswith("_")},
                "rule_engine": {k: v for k, v in re2k.items() if not k.startswith("_")},
                "drift": sem_drift,
            }

    # Clean internal edge sets from returned dicts
    for sub in ("handrolled", "rule_engine"):
        if sub in result:
            for k in ("_spec_edges", "_sem_edges"):
                result[sub].pop(k, None)

    return result
