"""A/B driver: the harness's own workload sequence, plus what the harness does not record.

Runs the same adapter functions, in the same order and under the same timers,
as `run_ours()` in `benchmarks/run.py`. Then, outside every timer, it records:

  - `db.stats()` after ingest and after the backfill (node and derived-edge counts);
  - the size of every file in the store directory at each stage (WAL growth);
  - a two-hop median of 10 after 3 warmups — the warm figure `run.py` does not produce;
  - the 1-minute load before and after.

One JSON file per run, and one summary line on stdout.

It measures whichever `mushroomdb` the interpreter running it has installed.
That is the point: for an A/B, build each side's release wheel into its own
virtualenv and run this file with each virtualenv's python, alternating.

    <venv python> benchmarks/ab_driver.py <harness_bench_dir> <label> <out.json> [scale]

`<harness_bench_dir>` is the `benchmarks/` directory whose `datasets.py` and
`adapters/ours.py` are used — one harness for both sides.

How it was invoked for `benchmarks/results/mushroomdb-10k-0.7-ab.md` (2026-10-01):
with each side's virtualenv python, against the 0.7 branch's `benchmarks/`
directory, scale 10,000, alternating A and B, each run waiting up to 1-2 minutes
for the 1-minute load to fall to 3 or below. The control rows of that file,
which hold the derived set at 2,000,000 edges, were run with `RULE_MAX_EDGES=none`
in the environment: each rule then carries an explicit `"max_edges": None`, which
the binding leaves as the global budget instead of filling in the per-source top 32.

As committed, positional arguments are parsed with argparse and the imports of
the harness and the binding happen inside `main()`, so `--help` works without a
built binding. The measured sequence is unchanged from the script that produced
the committed figures.
"""
from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
import tempfile
import time
from pathlib import Path

SEED = 20260819
RULES = [
    {
        "name": "bench_industry_tc",
        "src_label": "Talent",
        "dst_label": "Company",
        "predicate": {"FieldEqual": {"field": "industry"}},
        "edge_type": "INDUSTRY_ALIGNMENT",
        "weight_prop": "score",
    },
    {
        "name": "bench_specialty_tc",
        "src_label": "Talent",
        "dst_label": "Company",
        "predicate": {"Overlap": {"field": "specialties", "min": 0.15}},
        "edge_type": "SPECIALTY_MATCH",
        "weight_prop": "score",
    },
]


def dir_bytes(p: Path) -> dict[str, int]:
    out = {}
    for root, _dirs, files in os.walk(p):
        for f in files:
            fp = Path(root) / f
            out[str(fp.relative_to(p))] = fp.stat().st_size
    return out


def main() -> None:
    ap = argparse.ArgumentParser(
        description="A/B driver: the harness's workload sequence, plus edge counts, "
        "WAL sizes, load and a warm two-hop median. Measures the installed mushroomdb."
    )
    ap.add_argument("bench_dir", help="the benchmarks/ directory whose datasets.py and adapters/ours.py to use")
    ap.add_argument("label", help="a name for this run, recorded in the output")
    ap.add_argument("out_path", help="where to write this run's JSON")
    ap.add_argument("scale", nargs="?", type=int, default=10_000, help="node count (default 10000)")
    args = ap.parse_args()
    bench_dir, label, out_path, scale = args.bench_dir, args.label, args.out_path, args.scale
    sys.path.insert(0, bench_dir)

    from datasets import iter_nodes  # noqa: E402
    from adapters.ours import (  # noqa: E402
        bulk_ingest,
        neighborhood_depth1,
        neighborhood_depth2,
        cypher_scan_filter,
        cypher_two_hop,
        rule_derive,
        open_db,
    )
    import importlib.metadata as md  # noqa: E402
    import mushroomdb  # noqa: E402

    if os.environ.get("RULE_MAX_EDGES") == "none":
        for _r in RULES:
            _r["max_edges"] = None  # explicit null: the binding leaves it None -> global budget

    load_before = os.getloadavg()
    nodes = list(iter_nodes(n=scale, seed=SEED))
    talent_keys = [n["key"] for n in nodes if n["label"] == "Talent"]
    sample_keys = talent_keys[:20]
    res: dict = {
        "label": label,
        "module": mushroomdb.__file__,
        "version": md.version("mushroomdb"),
        "scale": scale,
        "rule_max_edges_env": os.environ.get("RULE_MAX_EDGES"),
        "load_before": load_before,
        "date": time.strftime("%Y-%m-%dT%H:%M:%S"),
    }

    with tempfile.TemporaryDirectory(prefix="bench-ours-") as tmp:
        db_dir = Path(tmp) / "db"
        res["bulk_ingest"] = bulk_ingest(nodes, db_dir)
        res["disk_after_ingest"] = dir_bytes(db_dir)
        db = open_db(db_dir)
        try:
            res["stats_after_ingest"] = db.stats()
        except Exception as exc:  # noqa: BLE001
            res["stats_after_ingest"] = {"error": str(exc)}
        res["neighborhood_depth1"] = neighborhood_depth1(db, sample_keys)
        res["neighborhood_depth2"] = neighborhood_depth2(db, sample_keys)
        res["cypher_scan_filter"] = cypher_scan_filter(db)
        res["rule_derive"] = rule_derive(db, RULES)
        res["cypher_two_hop"] = cypher_two_hop(db)
        # ---- everything below is outside the harness's timers ----
        try:
            res["stats_after_derive"] = db.stats()
        except Exception as exc:  # noqa: BLE001
            res["stats_after_derive"] = {"error": str(exc)}
        res["edge_counts_cypher"] = {}
        for et in ("INDUSTRY_ALIGNMENT", "SPECIALTY_MATCH"):
            try:
                rows = db.query(f"MATCH (t:Talent)-[r:{et}]->(c:Company) RETURN count(r) AS n")
                res["edge_counts_cypher"][et] = rows
            except Exception as exc:  # noqa: BLE001
                res["edge_counts_cypher"][et] = f"error: {exc}"
        try:
            res["wal_total_commits"] = db.wal_total_commits()
        except Exception as exc:  # noqa: BLE001
            res["wal_total_commits"] = f"error: {exc}"
        res["disk_after_derive"] = dir_bytes(db_dir)
        samples = []
        for i in range(13):
            r = cypher_two_hop(db)
            if i >= 3:
                samples.append(r["wall_s"])
        res["two_hop_median10_s"] = statistics.median(samples)
        res["two_hop_samples_s"] = samples
        t0 = time.perf_counter()
        db.close()
        res["close_s"] = time.perf_counter() - t0
        res["disk_after_close"] = dir_bytes(db_dir)

    res["load_after"] = os.getloadavg()
    Path(out_path).write_text(json.dumps(res, indent=1, default=str))
    rd = res["rule_derive"]
    print(
        f"{label} v{res['version']} load={load_before[0]:.2f} "
        f"ingest={res['bulk_ingest']['wall_s']:.3f}s "
        f"derive={rd['total_wall_s']:.3f}s "
        f"({' + '.join('%.3f' % p['wall_s'] for p in rd['per_rule'])}) "
        f"twohop1={res['cypher_two_hop']['wall_s']*1e6:.1f}us "
        f"twohop_med10={res['two_hop_median10_s']*1e6:.1f}us "
        f"scan={res['cypher_scan_filter']['wall_s']*1e3:.2f}ms "
        f"edges={res['stats_after_derive'].get('edges')} "
        f"nodes={res['stats_after_derive'].get('nodes_live')} "
        f"cypher_counts={res['edge_counts_cypher']}"
    )


if __name__ == "__main__":
    main()
