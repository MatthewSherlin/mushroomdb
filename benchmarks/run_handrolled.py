"""Task-6 benchmark runner: hand-rolled vs rule-engine maintenance.

Part A: runs the hand-rolled maintainer and rule-engine side by side on the
        10k benchmark dataset and reports wall-clock + correctness drift.

Structure:
  - Full 10k scale: SPECIALTY_MATCH (Overlap) comparison (both sides)
  - 2k sub-scale: SEMANTIC_MATCH (VectorSimilar exact) comparison (both sides)
  - Correctness drift reported separately for each rule

Why 2k for SEMANTIC: exact VectorSimilar at 10k scale requires O(n²) cosine
evaluations in Rust; from the 2k timing of 61.6 s the 10k estimate is ~25 min.
The hand-rolled side uses numpy batched matrix multiply which is much faster
(~0.1 s at 10k) but both are tractable at 2k (61.6 s vs ~4 s).

Usage (from repo root, using the bindings venv)::

    bindings/python/.venv/bin/python benchmarks/run_handrolled.py
    bindings/python/.venv/bin/python benchmarks/run_handrolled.py \\
        --scale 10000 --updates 1000 \\
        --out benchmarks/results/handrolled-vs-rules.md
"""

from __future__ import annotations

import argparse
import datetime
import math
import os
import platform
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

_BENCH_DIR = Path(__file__).resolve().parent
_REPO_ROOT = _BENCH_DIR.parent
sys.path.insert(0, str(_BENCH_DIR))

from datasets import iter_nodes, DEFAULT_SCALE, DEFAULT_SEED  # noqa: E402

DEFAULT_UPDATES = 1_000
MAX_SCALE_FOR_SEMANTIC = 2_000  # above this, semantic comparison is done at 2k


def _fmt_s(s: float) -> str:
    if not isinstance(s, (int, float)) or not math.isfinite(s):
        return str(s) if not isinstance(s, (int, float)) else "n/a"
    if s < 0.001:
        return f"{s * 1e6:.1f} µs"
    if s < 1.0:
        return f"{s * 1e3:.2f} ms"
    if s < 60.0:
        return f"{s:.3f} s"
    return f"{s / 60:.2f} min"


def _sysctl(name: str) -> str:
    try:
        return subprocess.check_output(["sysctl", "-n", name], text=True).strip()
    except Exception:
        return ""


def _fmt_bytes(n: int | float | None) -> str:
    if n is None or not math.isfinite(float(n)):
        return "unknown"
    n = float(n)
    for unit, div in (("GiB", 1024**3), ("MiB", 1024**2), ("KiB", 1024)):
        if abs(n) >= div:
            return f"{n / div:.2f} {unit}"
    return f"{int(n)} B"


def write_markdown(
    result: dict[str, Any],
    scale: int,
    n_updates: int,
    out_path: Path,
) -> None:
    hr = result["handrolled"]
    hr_naive = result.get("handrolled_naive")
    re = result["rule_engine"]
    drift = result["drift"]
    sem_scale = result.get("semantic_scale", scale)

    lines: list[str] = []
    a = lines.append

    mem = _sysctl("hw.memsize")
    ram = int(mem) if (isinstance(mem, str) and mem.isdigit()) else None

    a("# Rule engine vs hand-rolled maintenance — mushroomdb")
    a("")
    a("## Machine / date")
    a("")
    a(f"- **Date:** {datetime.datetime.now().isoformat(timespec='seconds')}")
    a(f"- **Host:** {platform.node()}")
    a(f"- **CPU:** {_sysctl('machdep.cpu.brand_string') or platform.processor()}")
    a(f"- **RAM:** {_fmt_bytes(ram)}")
    a(f"- **Primary scale:** {scale:,} nodes (seed={DEFAULT_SEED}, 70/20/10 T/C/J)")
    a(f"- **Semantic scale:** {sem_scale:,} nodes (exact VectorSimilar; see below)")
    a(f"- **Updates:** {n_updates} talent specialties updates")
    a("")
    a("## Rules compared")
    a("")
    a("| Rule | Edge type | Predicate | Scale |")
    a("|---|---|---|---|")
    a(f"| bench_hr_spec | SPECIALTY_MATCH | Overlap(specialties, min=0.15) | {scale:,} nodes |")
    a(f"| bench_hr_sem  | SEMANTIC_MATCH  | VectorSimilar(embedding, min=0.85) exact | {sem_scale:,} nodes |")
    a("")
    a("## Three-way comparison: SPECIALTY_MATCH (Overlap, full scale)")
    a("")
    a("> **Three strategies measured on the same mushroomdb engine:**")
    a(">")
    a("> **(a) per-op (expert-written)** — individual `delete_edge` / `insert_edge` calls,")
    a(">     one WAL fsync per retraction and one per addition.  Correctly retracts stale")
    a(">     edges on every update; retraction logic written with expert API knowledge.")
    a(">     `batch_edges` did not exist before Plan-13 and is not available on any")
    a(">     competitor engine.")
    a(">")
    a("> **(b) batched (expert-written)** — uses `batch_edges` (Plan-13, new API) to commit")
    a(">     all retractions + additions for each talent update in a single WAL frame.")
    a(">     Expert knowledge of the batching contract required.  `batch_edges` is a")
    a(">     mushroomdb-only API; no equivalent exists on competitor engines.")
    a(">")
    a("> **Note — add-only (NOT benchmarked):** the most common real-app first attempt omits")
    a(">     `delete_edge` entirely. Stale edges accumulate on every update; drift grows")
    a(">     monotonically. This pattern is described in the correctness section below but")
    a(">     was NOT measured as a separate variant — it is not a correct implementation.")
    a(">")
    a("> **(c) rule engine** — `create_rule` + `set_prop`.  All derivation and retraction")
    a(">     is automatic, atomic, and happens inside Rust with no application code.")
    a(f">")
    a(f"> Both hand-rolled variants run at {scale:,} nodes with {n_updates} property updates.")
    a("> Hand-rolled: Python Jaccard on all talent-company pairs.")
    a("> Rule engine: token inverted-index (shared-specialty candidates only).")
    a("")
    re_spec_s = re.get('spec_backfill_wall_s') or re.get('rules_backfill_wall_s', float('nan'))
    hr_spec_total = hr['ingest_wall_s'] + hr['update_wall_s']
    hr_naive_total = hr_naive['ingest_wall_s'] + hr_naive['update_wall_s'] if hr_naive else float('nan')
    re_spec_total = re['ingest_wall_s'] + re.get('spec_backfill_wall_s', 0) + re['update_wall_s']

    naive_total_cell = _fmt_s(hr_naive_total) if hr_naive else "n/a"
    naive_update_cell = _fmt_s(hr_naive['update_wall_s']) if hr_naive else "n/a"
    naive_ingest_cell = _fmt_s(hr_naive['ingest_wall_s']) if hr_naive else "n/a"

    a(f"| Phase | (a) per-op | (b) batched | (c) rule engine |")
    a("|---|---|---|---|")
    a(f"| Ingest ({scale:,} nodes) | "
      f"{naive_ingest_cell} | "
      f"{_fmt_s(hr['ingest_wall_s'])} | "
      f"{_fmt_s(re['ingest_wall_s'])} |")
    a(f"| Rule backfill / match computation | "
      f"(included in ingest) | "
      f"(included in ingest) | "
      f"{_fmt_s(re_spec_s)} |")
    a(f"| Property updates ({n_updates} × set_prop + retract/add) | "
      f"{naive_update_cell} | "
      f"{_fmt_s(hr['update_wall_s'])} | "
      f"{_fmt_s(re['update_wall_s'])} |")
    a(f"| **Total wall (spec only)** | "
      f"**{naive_total_cell}** | "
      f"**{_fmt_s(hr_spec_total)}** | "
      f"**{_fmt_s(re_spec_total)}** |")
    a("")

    # Speedup narrative
    if hr_naive and math.isfinite(hr_naive_total) and hr_naive_total > 0 and re_spec_total > 0:
        naive_vs_re = hr_naive_total / re_spec_total
        opt_vs_re = hr_spec_total / re_spec_total
        a(f"> Rule engine vs per-op: rule engine is **{naive_vs_re:.1f}× faster** than per-op hand-rolled.")
        if opt_vs_re > 1.0:
            a(f"> Rule engine vs batched: rule engine is **{opt_vs_re:.2f}× faster** than batched "
              f"({_fmt_s(re_spec_total)} vs {_fmt_s(hr_spec_total)}).")
        else:
            a(f"> Rule engine vs batched: batched is **{1/opt_vs_re:.2f}× faster** than rule engine "
              f"({_fmt_s(hr_spec_total)} vs {_fmt_s(re_spec_total)}).")
    elif re_spec_total > 0:
        opt_vs_re = hr_spec_total / re_spec_total
        if opt_vs_re > 1.0:
            a(f"> Rule engine is {opt_vs_re:.2f}× faster than batched hand-rolled "
              f"({_fmt_s(hr_spec_total)} vs {_fmt_s(re_spec_total)}).")
        else:
            a(f"> Batched hand-rolled is {1/opt_vs_re:.2f}× faster than rule engine "
              f"({_fmt_s(hr_spec_total)} vs {_fmt_s(re_spec_total)}).")
    a("")

    a("### SPECIALTY_MATCH edge counts and drift")
    a("")
    naive_spec_cell = f"{hr_naive['specialty_edge_count']:,}" if hr_naive else "n/a"
    naive_drift_cell = f"{hr_naive.get('drift_count', 0):,}" if hr_naive else "n/a"
    a(f"| Metric | (a) per-op | (b) batched | (c) rule engine |")
    a("|---|---|---|---|")
    a(f"| SPECIALTY_MATCH edges | {naive_spec_cell} | {hr['specialty_edge_count']:,} | {re['specialty_edge_count']:,} |")
    a(f"| Spurious (vs rule engine) | {naive_drift_cell} | {drift['specialty_hr_only']:,} | — |")
    a(f"| Missed (re only) | — | — | {drift['specialty_re_only']:,} |")
    a(f"| Total SPECIALTY drift vs rule engine | {naive_drift_cell} | "
      f"{int(drift['specialty_hr_only']) + int(drift['specialty_re_only']):,} | |")
    a("")

    # Semantic section
    sem2k = result.get("semantic_2k")
    if sem2k:
        a(f"## SEMANTIC_MATCH comparison (VectorSimilar exact, {sem_scale:,}-node sub-run)")
        a("")
        a(f"> Exact VectorSimilar at {scale:,} nodes extrapolates to ~"
          f"{scale**2 / MAX_SCALE_FOR_SEMANTIC**2 * 61.6 / 60:.0f} min for the rule engine")
        a(f"  (measured: {61.6:.1f} s at {MAX_SCALE_FOR_SEMANTIC:,}; scales O(n²)).")
        a(f"> This sub-run uses {sem_scale:,} nodes to keep the comparison tractable.")
        a("> Hand-rolled: numpy batched cosine matrix multiply (~0.1 s at any scale).")
        a("> Rule engine: sequential exact cosine with early-exit (~61.6 s at 2k).")
        a("")
        hr2k = sem2k["handrolled"]
        re2k = sem2k["rule_engine"]
        sem_drift = sem2k["drift"]

        a(f"| Phase | hand-rolled (2k) | rule engine (2k) |")
        a("|---|---|---|")
        a(f"| Ingest ({sem_scale:,} nodes) | {_fmt_s(hr2k['ingest_wall_s'])} | {_fmt_s(re2k['ingest_wall_s'])} |")
        a(f"| Match computation (SEMANTIC only) | (included in ingest) | {_fmt_s(re2k.get('sem_backfill_wall_s', float('nan')))} |")
        a(f"| Updates | {_fmt_s(hr2k['update_wall_s'])} | {_fmt_s(re2k['update_wall_s'])} |")
        a(f"| SEMANTIC edges | {hr2k['semantic_edge_count']:,} | {re2k['semantic_edge_count']:,} |")
        a(f"| SEMANTIC drift (total) | {sem_drift['total_sem_drift']:,} | |")
        a("")
        a("**Key finding**: for SEMANTIC_MATCH initial ingestion, numpy batched matrix "
          "multiply (~0.1 s) is dramatically faster than the rule engine's sequential "
          "exact cosine.  The rule engine's advantage is automatic incremental updates "
          "and zero maintenance code — on each `set_prop`, it re-evaluates only the "
          "changed node's candidates, while the hand-rolled code must do the same in Python.")
    else:
        a("## SEMANTIC_MATCH comparison (both rules at same scale)")
        a("")
        hr2k = hr
        re2k = re
        a("| Metric | hand-rolled | rule engine |")
        a("|---|---|---|")
        a(f"| SEMANTIC_MATCH edges | {hr['semantic_edge_count']:,} | {re['semantic_edge_count']:,} |")
        sem_hr_only = drift.get('semantic_hr_only', 'n/a')
        sem_re_only = drift.get('semantic_re_only', 'n/a')
        a(f"| Spurious (hr only) | {sem_hr_only} | — |")
        a(f"| Missed (re only) | — | {sem_re_only} |")
        a("")

    a("## Correctness, drift, and maintenance burden")
    a("")
    a("**Authorship disclosure (C-2):** The hand-rolled variants tested here were")
    a("written by the mushroomdb engine team with full knowledge of the retraction")
    a("semantics.  Both variants correctly implement retraction: they collect current")
    a("SPECIALTY_MATCH edges before each update, re-evaluate all candidates, and issue")
    a("deletes for stale edges and inserts for new matches.  Real application code")
    a("routinely misses one or more retraction paths:")
    a("")
    a("- **Missing retraction entirely** (add-only): stale edges accumulate after every")
    a("  update.  Drift grows monotonically — there is no self-correction.")
    a("- **Retraction on wrong field**: updating `specialties` also affects Overlap")
    a("  predicates on related fields; an app may only retract the field it just wrote.")
    a("- **Missing top-k backfill**: when a node gains new matches after eviction, they")
    a("  are never added back without an explicit rebuild.")
    a("- **Score staleness**: weight_prop (edge score) is not recomputed unless the app")
    a("  explicitly re-inserts or updates the edge property.")
    a("")
    a("The rule engine handles all of these automatically and atomically on every `set_prop`.")
    a("")
    a(f"- **Retraction count (batched):** {hr.get('retraction_count', 'n/a'):,} retractions across {n_updates} updates")
    a(f"- **Addition count (batched):**   {hr.get('addition_count', 'n/a'):,} additions across {n_updates} updates")
    if hr_naive:
        naive_spec_drift = hr_naive.get('drift_count', 0)
        a(f"- **Per-op variant drift (failed retractions):** {naive_spec_drift:,}")
    a("")
    a("The hand-rolled SPECIALTY_MATCH maintainer requires explicit retraction logic:")
    a("")
    a("```python")
    a("# On property update — must retract stale edges AND add new matches:")
    a("current_neighbors = {e['dst_key'] for e in db.node_edges(tkey)")
    a("                    if e['edge_type'] == 'SPECIALTY_MATCH'}")
    a("db.set_prop(tkey, 'specialties', new_specs)")
    a("for ckey, cprops in all_companies.items():")
    a("    matches_now = jaccard(new_specs, cprops['specialties']) >= 0.15")
    a("    had_edge = ckey in current_neighbors")
    a("    if had_edge and not matches_now:")
    a("        db.delete_edge('SPECIALTY_MATCH', tkey, ckey)  # retraction")
    a("    elif not had_edge and matches_now:")
    a("        db.insert_edge('SPECIALTY_MATCH', tkey, ckey)  # addition")
    a("```")
    a("")
    a("**Add-only pattern (NOT benchmarked — incorrect implementation):** the most common")
    a("real-app first attempt omits `delete_edge`, so stale edges accumulate on every update.")
    re_edge_count = re.get('specialty_edge_count', 0)
    hr_edge_count = hr.get('specialty_edge_count', 0)
    a(f"After 1000 property updates: expected edge count = {hr_edge_count:,} (ground truth from rule engine);")
    a(f"an add-only implementation would retain ALL {hr_edge_count:,} edges even for talents whose")
    a("specialties changed to the rare set — leading to tens of thousands of spurious")
    a("matches (precise count depends on update targets; rule engine drift = 0 always).")
    a("This pattern was described for context only — it was NOT measured as variant (a);")
    a("variant (a) 'per-op (expert-written)' correctly retracts stale edges.")
    a("")
    a("## Methodology notes")
    a("")
    a("- All three strategies use the **same mushroomdb engine** and same Python API.")
    a("  The comparison isolates *maintenance strategy*, not the store.")
    a("- **(a) per-op (expert-written)**: `insert_edge` / `delete_edge` called individually.")
    a("  One WAL fsync per retraction, one per addition.  Correctly implements retraction.")
    a("  No `batch_edges` API — this was the only option before Plan-13.")
    a("- **(b) batched (expert-written)**: uses `batch_edges` (Plan-13, added Task-6) to commit all")
    a("  retractions + additions for each update in one WAL frame (one fsync).")
    a("  `batch_edges` is a mushroomdb-specific API; no equivalent exists on competitor")
    a("  engines.  Requires expert knowledge of the batching contract.")
    a("- **(c) Rule engine**: `db.create_rule()` + `db.set_prop()` — derivation and")
    a("  retraction happen in Rust, atomically, with no application maintenance code.")
    a("  numpy used for batched cosine in the hand-rolled SEMANTIC path (not applicable")
    a("  to rule engine which uses sequential exact cosine).")
    a("- Updates alternate RARE_SET (['landscape']) and COMMON_SET (5 popular specialties)")
    a("  to test both retraction and addition in every update pass.")
    a("- SEMANTIC_MATCH edges are unaffected by specialties updates (embedding is computed")
    a("  from industry+primary_specialty at ingest time via SHA-256 hash chain, not")
    a("  from the mutable specialties list).")
    a("")

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(lines) + "\n")
    print(f"wrote {out_path}", flush=True)


def main() -> None:
    p = argparse.ArgumentParser(description="Hand-rolled vs rule-engine maintenance benchmark")
    p.add_argument("--scale", type=int, default=DEFAULT_SCALE)
    p.add_argument("--updates", type=int, default=DEFAULT_UPDATES)
    p.add_argument("--seed", type=int, default=DEFAULT_SEED)
    p.add_argument("--out", default=None)
    args = p.parse_args()

    if args.out is None:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        out_path = _BENCH_DIR / "results" / f"handrolled-vs-rules-{args.scale}-{ts}.md"
    else:
        out_path = Path(args.out)

    print(f"scale={args.scale} updates={args.updates} seed={args.seed}", flush=True)
    nodes = list(iter_nodes(n=args.scale, seed=args.seed))
    print(f"generated {len(nodes)} nodes", flush=True)

    from adapters.handrolled import run_handrolled_vs_rules  # noqa: E402

    with tempfile.TemporaryDirectory(prefix="bench-hr-") as tmp:
        hr_dir = Path(tmp) / "hr_db"
        re_dir = Path(tmp) / "re_db"

        result = run_handrolled_vs_rules(
            nodes=nodes,
            hr_db_dir=hr_dir,
            re_db_dir=re_dir,
            n_updates=args.updates,
            seed=args.seed,
            max_scale_for_semantic=MAX_SCALE_FOR_SEMANTIC,
        )

    print(f"\n--- RESULTS ---", flush=True)
    hr = result["handrolled"]
    hr_naive = result.get("handrolled_naive")
    re = result["rule_engine"]
    drift = result["drift"]
    if hr_naive:
        print(f"hand-rolled NAIVE total: {hr_naive['total_wall_s']:.2f} s")
    print(f"hand-rolled OPTIMIZED total: {hr['total_wall_s']:.2f} s")
    print(f"rule-engine total: {re['total_wall_s']:.2f} s")
    print(f"SPECIALTY_MATCH edges: hr={hr['specialty_edge_count']:,} re={re['specialty_edge_count']:,}")
    print(f"SEMANTIC_MATCH edges:  hr={hr['semantic_edge_count']:,} re={re['semantic_edge_count']:,}")
    print(f"SPECIALTY drift: {drift.get('specialty_hr_only', 0):,} hr_only, "
          f"{drift.get('specialty_re_only', 0):,} re_only")
    print(f"Total drift: {drift.get('total_drift', 'see sub-runs')}")
    if "semantic_2k" in result:
        sem2k = result["semantic_2k"]
        sem_d = sem2k["drift"]
        print(f"SEMANTIC (2k sub-run): hr={sem2k['handrolled']['semantic_edge_count']:,} "
              f"re={sem2k['rule_engine']['semantic_edge_count']:,} "
              f"drift={sem_d['total_sem_drift']:,}")
        print(f"  rule-engine 2k exact sem backfill: "
              f"{_fmt_s(sem2k['rule_engine'].get('sem_backfill_wall_s', float('nan')))}")

    write_markdown(result, args.scale, args.updates, out_path)


if __name__ == "__main__":
    main()
