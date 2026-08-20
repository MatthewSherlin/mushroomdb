"""Comparative benchmark orchestrator.

Runs all workloads at --scale (default 10k) for every available engine and
writes a Markdown results table to benchmarks/results/<run-id>.md.

Competitor adapters are skipped automatically when the required driver/server
is not present — zero manual configuration needed.

Usage (from repo root, using the bindings venv)::

    bindings/python/.venv/bin/python benchmarks/run.py
    bindings/python/.venv/bin/python benchmarks/run.py --scale 2000
    bindings/python/.venv/bin/python benchmarks/run.py --scale 10000 --out benchmarks/results/my-run.md

The ours engine always runs.  Competitor engines that are absent produce a
'not installed — skipped' row in the output table.
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
import time
from pathlib import Path
from typing import Any

_BENCH_DIR = Path(__file__).resolve().parent
_REPO_ROOT = _BENCH_DIR.parent
sys.path.insert(0, str(_BENCH_DIR))

from datasets import iter_nodes, split_scale  # noqa: E402

DEFAULT_SCALE = 10_000
DEFAULT_SEED = 20260819
SAMPLE_KEYS_COUNT = 20  # neighbourhood samples


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _fmt_s(s: float) -> str:
    if not math.isfinite(s):
        return "n/a"
    if s < 0.001:
        return f"{s * 1e6:.1f} µs"
    if s < 1.0:
        return f"{s * 1e3:.2f} ms"
    if s < 60.0:
        return f"{s:.3f} s"
    return f"{s / 60:.2f} min"


def _fmt_thr(n: float) -> str:
    if not math.isfinite(n) or n == 0:
        return "n/a"
    if n >= 1_000:
        return f"{n / 1_000:.1f}k nodes/s"
    return f"{n:.0f} nodes/s"


def _sysctl(name: str) -> str:
    try:
        return subprocess.check_output(["sysctl", "-n", name], text=True).strip()
    except Exception:
        return ""


def machine_header() -> dict[str, Any]:
    mem = _sysctl("hw.memsize")
    ram = int(mem) if mem.isdigit() else None
    return {
        "date": datetime.datetime.now().isoformat(timespec="seconds"),
        "os": platform.platform(),
        "python": platform.python_version(),
        "machine": platform.machine(),
        "cpu": _sysctl("machdep.cpu.brand_string") or platform.processor() or "unknown",
        "ncpu": os.cpu_count(),
        "ram_bytes": ram,
        "hostname": platform.node(),
    }


def _fmt_bytes(n: int | float | None) -> str:
    if n is None or not math.isfinite(float(n)):
        return "unknown"
    n = float(n)
    for unit, div in (("GiB", 1024**3), ("MiB", 1024**2), ("KiB", 1024)):
        if abs(n) >= div:
            return f"{n / div:.2f} {unit}"
    return f"{int(n)} B"


# ---------------------------------------------------------------------------
# ours engine runner
# ---------------------------------------------------------------------------

def run_ours(nodes: list[dict], scale: int, seed: int) -> dict[str, Any]:
    """Run all workloads against mushroomdb; return per-workload results."""
    from adapters.ours import (
        bulk_ingest,
        neighborhood_depth1,
        neighborhood_depth2,
        cypher_scan_filter,
        cypher_two_hop,
        rule_derive,
        open_db,
        INGEST_CHUNK,
    )

    results: dict[str, Any] = {"engine": "mushroomdb", "scale": scale, "seed": seed}
    talent_keys = [n["key"] for n in nodes if n["label"] == "Talent"]
    sample_keys = talent_keys[:SAMPLE_KEYS_COUNT]

    with tempfile.TemporaryDirectory(prefix="bench-ours-") as tmp:
        db_dir = Path(tmp) / "db"

        # 1. Bulk ingest
        print("  [ours] bulk_ingest ...", flush=True)
        results["bulk_ingest"] = bulk_ingest(nodes, db_dir)

        # Open db for remaining workloads
        db = open_db(db_dir)

        # 2. Depth-1 neighbourhood
        print("  [ours] neighborhood_depth1 ...", flush=True)
        results["neighborhood_depth1"] = neighborhood_depth1(db, sample_keys)

        # 3. Depth-2 neighbourhood
        print("  [ours] neighborhood_depth2 ...", flush=True)
        results["neighborhood_depth2"] = neighborhood_depth2(db, sample_keys)

        # 4. Cypher scan-filter-project
        print("  [ours] cypher_scan_filter ...", flush=True)
        results["cypher_scan_filter"] = cypher_scan_filter(db)

        # 5. Rule-derive (ours-only; must come before cypher_two_hop)
        print("  [ours] rule_derive (INDUSTRY_ALIGNMENT) ...", flush=True)
        rules = [
            {
                "name": "bench_industry_tc",
                "src_label": "Talent",
                "dst_label": "Company",
                "predicate": {"FieldEqual": {"field": "industry"}},
                "edge_type": "INDUSTRY_ALIGNMENT",
                "weight_prop": "score",
                "max_edges": 1_000_000,
            },
            {
                "name": "bench_specialty_tc",
                "src_label": "Talent",
                "dst_label": "Company",
                "predicate": {"Overlap": {"field": "specialties", "min": 0.15}},
                "edge_type": "SPECIALTY_MATCH",
                "weight_prop": "score",
                "max_edges": 1_000_000,
            },
        ]
        results["rule_derive"] = rule_derive(db, rules)

        # 6. Cypher two-hop join (after rule_derive so edges exist)
        print("  [ours] cypher_two_hop ...", flush=True)
        results["cypher_two_hop"] = cypher_two_hop(db)

        db.close()

    return results


# ---------------------------------------------------------------------------
# Competitor runners (skip-safe wrappers)
# ---------------------------------------------------------------------------

def _skipped_result(engine: str, reason: str) -> dict[str, Any]:
    return {"engine": engine, "skipped": True, "reason": reason}


def run_neo4j(nodes: list[dict]) -> dict[str, Any]:
    try:
        from adapters.neo4j import _driver
        drv = _driver()
        drv.close()
    except ImportError as e:
        return _skipped_result("neo4j", str(e))
    except RuntimeError as e:
        return _skipped_result("neo4j", str(e))
    # Server is available — run workloads.
    from adapters import neo4j as a
    talent_keys = [n["key"] for n in nodes if n["label"] == "Talent"]
    sample = talent_keys[:SAMPLE_KEYS_COUNT]
    out: dict[str, Any] = {"engine": "neo4j"}
    out["bulk_ingest"] = a.bulk_ingest(nodes)
    out["neighborhood_depth1"] = a.neighborhood_depth1(sample)
    out["neighborhood_depth2"] = a.neighborhood_depth2(sample)
    out["cypher_scan_filter"] = a.cypher_scan_filter()
    out["cypher_two_hop"] = a.cypher_two_hop()
    return out


def run_kuzu(nodes: list[dict]) -> dict[str, Any]:
    try:
        import kuzu  # type: ignore[import]  # noqa: F401
    except ImportError as e:
        return _skipped_result("kuzu", f"'kuzu' not installed — pip install kuzu ({e})")
    from adapters import kuzu as a
    talent_keys = [n["key"] for n in nodes if n["label"] == "Talent"]
    sample = talent_keys[:SAMPLE_KEYS_COUNT]
    with tempfile.TemporaryDirectory(prefix="bench-kuzu-") as tmp:
        db_dir = Path(tmp) / "kuzu_db"
        out: dict[str, Any] = {"engine": "kuzu"}
        out["bulk_ingest"] = a.bulk_ingest(nodes, db_dir)
        try:
            import kuzu  # type: ignore[import]
            db = kuzu.Database(str(db_dir))
            conn = kuzu.Connection(db)
            out["neighborhood_depth1"] = a.neighborhood_depth1(conn, sample)
            out["neighborhood_depth2"] = a.neighborhood_depth2(conn, sample)
            out["cypher_scan_filter"] = a.cypher_scan_filter(conn)
            out["cypher_two_hop"] = a.cypher_two_hop(conn)
        except Exception as e:
            out["error"] = str(e)
    return out


def run_memgraph(nodes: list[dict]) -> dict[str, Any]:
    from adapters.memgraph import _driver, _SKIP_MSG_NO_DRIVER, _SKIP_MSG_NO_SERVER
    try:
        _driver()
    except ImportError as e:
        return _skipped_result("memgraph", str(e))
    except RuntimeError as e:
        return _skipped_result("memgraph", str(e))
    from adapters import memgraph as a
    talent_keys = [n["key"] for n in nodes if n["label"] == "Talent"]
    sample = talent_keys[:SAMPLE_KEYS_COUNT]
    out: dict[str, Any] = {"engine": "memgraph"}
    out["bulk_ingest"] = a.bulk_ingest(nodes)
    out["neighborhood_depth1"] = a.neighborhood_depth1(sample)
    out["neighborhood_depth2"] = a.neighborhood_depth2(sample)
    out["cypher_scan_filter"] = a.cypher_scan_filter()
    out["cypher_two_hop"] = a.cypher_two_hop()
    return out


# ---------------------------------------------------------------------------
# Markdown report
# ---------------------------------------------------------------------------

def _cell(engine_result: dict, workload: str, field: str = "wall_s", fmt: Any = _fmt_s) -> str:
    if engine_result.get("skipped"):
        return "not installed — skipped"
    w = engine_result.get(workload)
    if w is None:
        return "n/a"
    v = w.get(field)
    if v is None:
        return "n/a"
    return fmt(v)


def write_markdown(
    mh: dict[str, Any],
    scale: int,
    seed: int,
    ours: dict[str, Any],
    competitors: list[dict[str, Any]],
    out_path: Path,
) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    lines: list[str] = []
    a = lines.append

    ram = _fmt_bytes(mh.get("ram_bytes"))
    a("# Comparative benchmark — mushroomdb")
    a("")
    a("## Machine / date")
    a("")
    a(f"- **Date:** {mh['date']}")
    a(f"- **Host:** {mh['hostname']}")
    a(f"- **OS:** {mh['os']}")
    a(f"- **CPU:** {mh['cpu']} ({mh['ncpu']} cores, {mh['machine']})")
    a(f"- **RAM:** {ram}")
    a(f"- **Python:** {mh['python']}")
    a(f"- **Scale:** {scale:,} nodes (seed={seed}, 70/20/10 Talent/Company/Job split)")
    a("")
    a("## Honesty note")
    a("")
    a("See `benchmarks/README.md` for the full honesty section.")
    a("Short version: mushroomdb numbers are **embedded Rust** (no network RTT);")
    a("competitor numbers are over bolt/localhost. `rule_derive` is ours-only —")
    a("competitors have no auto-derivation equivalent, so it is excluded from")
    a("the cross-engine table.")
    a("")

    all_engines = [ours] + competitors
    headers = ["workload"] + [e["engine"] for e in all_engines]

    def row(label: str, *cells: str) -> str:
        return "| " + " | ".join([label] + list(cells)) + " |"

    sep = "| " + " | ".join(["---"] * len(headers)) + " |"
    a("## Cross-engine comparison (wall time)")
    a("")
    a(row(*headers))
    a(sep)
    a(row(
        "bulk_ingest",
        *[_cell(e, "bulk_ingest") for e in all_engines],
    ))
    a(row(
        "neighborhood_depth1 (p50)",
        *[_cell(e, "neighborhood_depth1", "p50_s") for e in all_engines],
    ))
    a(row(
        "neighborhood_depth1 (p95)",
        *[_cell(e, "neighborhood_depth1", "p95_s") for e in all_engines],
    ))
    a(row(
        "neighborhood_depth2 (p50)",
        *[_cell(e, "neighborhood_depth2", "p50_s") for e in all_engines],
    ))
    a(row(
        "cypher scan-filter-project",
        *[_cell(e, "cypher_scan_filter") for e in all_engines],
    ))
    a(row(
        "cypher two-hop join",
        *[_cell(e, "cypher_two_hop") for e in all_engines],
    ))
    a("")
    a("## mushroomdb — bulk ingest throughput")
    a("")
    bi = ours.get("bulk_ingest", {})
    a(f"- **Nodes ingested:** {bi.get('node_count', 0):,}")
    a(f"- **Wall time:** {_fmt_s(bi.get('wall_s', float('nan')))}")
    a(f"- **Throughput:** {_fmt_thr(bi.get('throughput_nodes_per_s', 0))}")
    a(f"- **Chunk size:** 2k nodes / ingest_batch call (5 sequential chunks at 10k scale)")
    a("")
    a("## mushroomdb — neighbourhood latencies")
    a("")
    d1 = ours.get("neighborhood_depth1", {})
    d2 = ours.get("neighborhood_depth2", {})
    a(f"- **depth-1 (node_edges):** p50={_fmt_s(d1.get('p50_s', float('nan')))} "
      f"p95={_fmt_s(d1.get('p95_s', float('nan')))} (n={d1.get('n', 0)})")
    a(f"- **depth-2 (node_edges + neighbors + second-hop):** "
      f"p50={_fmt_s(d2.get('p50_s', float('nan')))} "
      f"p95={_fmt_s(d2.get('p95_s', float('nan')))} (n={d2.get('n', 0)})")
    a("")
    a("## mushroomdb — Cypher workloads")
    a("")
    csf = ours.get("cypher_scan_filter", {})
    cth = ours.get("cypher_two_hop", {})
    a(f"- **scan-filter-project** (`{csf.get('query', '')}`): "
      f"rows={csf.get('row_count', 0)} wall={_fmt_s(csf.get('wall_s', float('nan')))}")
    a(f"- **two-hop join** "
      f"(`MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company)<-[:INDUSTRY_ALIGNMENT]-(t2:Talent) LIMIT 200`): "
      f"rows={cth.get('row_count', 0)} wall={_fmt_s(cth.get('wall_s', float('nan')))}"
      + (f" — {cth['note']}" if cth.get("note") else ""))
    a("")
    a("## mushroomdb — rule_derive (ours-only)")
    a("")
    a("> **Auto-derivation has no competitor equivalent.**")
    a("> Edges are derived automatically when rules are declared and on every")
    a("> subsequent ingest/update. Competitors require manual ETL / triggers.")
    a("> This workload is intentionally excluded from the cross-engine table.")
    a("> See `benchmarks/README.md` for the full explanation.")
    a("")
    rd = ours.get("rule_derive", {})
    a(f"- **Rules declared:** {rd.get('n_rules', 0)}")
    a(f"- **Total backfill wall:** {_fmt_s(rd.get('total_wall_s', float('nan')))}")
    for pr in rd.get("per_rule", []):
        a(f"  - `{pr['name']}` ({pr['edge_type']}): {_fmt_s(pr['wall_s'])}")
    a("")
    a("## Competitors")
    a("")
    for e in competitors:
        if e.get("skipped"):
            a(f"- **{e['engine']}:** not installed — skipped  ")
            a(f"  _{e.get('reason', '')}_")
        else:
            a(f"- **{e['engine']}:** ran successfully")
    a("")
    out_path.write_text("\n".join(lines) + "\n")
    print(f"wrote {out_path}", flush=True)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main() -> None:
    p = argparse.ArgumentParser(description="mushroomdb comparative benchmark")
    p.add_argument("--scale", type=int, default=DEFAULT_SCALE)
    p.add_argument("--seed", type=int, default=DEFAULT_SEED)
    p.add_argument("--out", default=None)
    args = p.parse_args()

    if args.out is None:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        out_path = _BENCH_DIR / "results" / f"run-{args.scale}-{ts}.md"
    else:
        out_path = Path(args.out)

    print(f"scale={args.scale} seed={args.seed}", flush=True)
    mh = machine_header()
    print(f"machine: {mh['cpu']} | {mh['hostname']}", flush=True)

    nodes = list(iter_nodes(n=args.scale, seed=args.seed))
    print(f"generated {len(nodes)} nodes", flush=True)

    print("[ours]", flush=True)
    ours = run_ours(nodes, args.scale, args.seed)

    competitors: list[dict[str, Any]] = []

    print("[neo4j]", flush=True)
    competitors.append(run_neo4j(nodes))

    print("[kuzu]", flush=True)
    competitors.append(run_kuzu(nodes))

    print("[memgraph]", flush=True)
    competitors.append(run_memgraph(nodes))

    for e in competitors:
        if e.get("skipped"):
            print(f"  {e['engine']}: skipped — {e.get('reason', '')[:80]}", flush=True)
        else:
            print(f"  {e['engine']}: done", flush=True)

    write_markdown(mh, args.scale, args.seed, ours, competitors, out_path)


if __name__ == "__main__":
    main()
