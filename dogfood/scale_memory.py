"""Memory-shaped scale experiment for mushroomdb.

Phases (each wall-clock + peak RSS via the resource module):
  (1) schema   — enable_fulltext for memory label.field pairs + enable_index
  (2) ingest   — N entities via ingest_batch (10k-node chunks); edges via
                 batch_edges (3–5 edges per entity on average)
  (3) rules    — same_team FieldEqual Person→Person (max_edges=100_000)
  (4) remember — 10_000 per-call writes; p50/p95 on the real write path
  (5) reopen_wal  — close + open without snapshot (WAL replay; rule re-fires)
  (6) reopen_snap — snapshot() then close + open (0.8: full-text in snapshot)
  (7) recall   — p50/p95/p99 over four query shapes: rare word, common word,
                 two-word, a stored entity name
  (8) lookup   — node_info p50/p95 + node_edges p50/p95 on random keys
  (9) query    — Cypher COUNT + 1-hop neighbourhood MATCH
  (10) incr    — 100 set_prop calls p50/p95 after reopen

Entity labels (per memory schema): Person, Org, Project, Concept, Event
Full-text pairs: Person.name, Org.name, Project.name, Concept.name,
                 Event.name, Concept.summary, Note.text, Entity.name
Rule: same_team  Person→Person  FieldEqual(team_id)  max_edges=100_000

RSS guard: harness reports and exits before a scale step when peak RSS
> 18 GiB or disk < 5 GB free.  Stores are deleted after numbers are
recorded.
"""

from __future__ import annotations

import argparse
import bisect
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
from typing import Any, Iterator


from mushroomdb import GraphDb

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

DEFAULT_SCALE = 100_000
DEFAULT_SEED = 20261014

INGEST_BATCH = 10_000          # max nodes per ingest_batch call
N_REMEMBER = 10_000            # per-call remember measurement
N_RECALL_QUERIES = 300         # queries for recall latency (per shape: 75 each)
N_LOOKUP = 200                 # node_info / node_edges samples
N_INCR = 100                   # incremental set_prop samples

# max_edges=None engages the global-budget path (DEFAULT_MAX_EDGES=1M cap).
# max_edges=Some(k) is per-source top-k; at large scales the global-budget
# path is the right choice to bound derived-edge count across all sources.
RULE_MAX_EDGES: int | None = None   # global 1M cap (same as scale_run.py non-semantic rules)

# Guard: stop before a scale step if peak RSS exceeds this.
RSS_GUARD_BYTES = 18 * 1024 ** 3
# Guard: stop before a scale step if free disk is below this.
DISK_GUARD_BYTES = 5 * 1024 ** 3

# Memory schema full-text pairs (mushroomdb::memory_schema::memory_defaults).
ENTITY_LABELS = ("Person", "Org", "Project", "Concept", "Event")
FULLTEXT_PAIRS: tuple[tuple[str, str], ...] = (
    *tuple((lbl, "name") for lbl in ENTITY_LABELS),
    ("Concept", "summary"),
    ("Note", "text"),
    ("Entity", "name"),      # provisional label
)

_RESULTS_DIR = Path(__file__).resolve().parent / "results"


# ---------------------------------------------------------------------------
# Synthetic text / name generation (Zipf-distributed vocabulary)
# ---------------------------------------------------------------------------

_VOCAB_SIZE = 3_000
_VOCAB_SEED_OFFSET = 9_999


def _make_vocab(seed: int) -> list[str]:
    """Deterministic vocabulary of ~3000 lowercase words from a seed."""
    rng = random.Random(seed + _VOCAB_SEED_OFFSET)
    consonants = "bcdfghjklmnpqrstvwxyz"
    vowels = "aeiou"
    words: set[str] = set()
    while len(words) < _VOCAB_SIZE:
        length = rng.randint(3, 9)
        w = []
        for i in range(length):
            if i % 2 == 0:
                w.append(rng.choice(consonants))
            else:
                w.append(rng.choice(vowels))
        words.add("".join(w))
    return sorted(words)


def _make_zipf_sampler(vocab: list[str]) -> Any:
    """Return a callable(rng) -> word sampling with Zipf-1 frequency distribution."""
    n = len(vocab)
    weights = [1.0 / (i + 1) for i in range(n)]
    total = sum(weights)
    cumulative: list[float] = []
    s = 0.0
    for wt in weights:
        s += wt / total
        cumulative.append(s)

    def _sample(rng: random.Random) -> str:
        r = rng.random()
        idx = bisect.bisect_left(cumulative, r)
        return vocab[min(idx, n - 1)]

    _sample._vocab = vocab            # type: ignore[attr-defined]
    _sample._cumulative = cumulative  # type: ignore[attr-defined]
    return _sample


def _words(rng: random.Random, sample_word: Any, k: int) -> str:
    return " ".join(sample_word(rng) for _ in range(k))


# ---------------------------------------------------------------------------
# Node / edge generators
# ---------------------------------------------------------------------------

def split_scale(n: int) -> tuple[int, int, int, int, int]:
    """Person 40%, Org 10%, Project 20%, Concept 20%, Event 10%."""
    n_person  = (n * 40) // 100
    n_org     = (n * 10) // 100
    n_project = (n * 20) // 100
    n_concept = (n * 20) // 100
    n_event   = n - n_person - n_org - n_project - n_concept
    return n_person, n_org, n_project, n_concept, n_event


def _n_teams(n_person: int) -> int:
    """One team per ~80 persons; at least 1."""
    return max(1, n_person // 80)


def generate_nodes(
    n_person: int, n_org: int, n_project: int,
    n_concept: int, n_event: int, seed: int,
) -> Iterator[dict]:
    """Stream entity node dicts for ingest_batch."""
    rng = random.Random(seed)
    vocab = _make_vocab(seed)
    w = _make_zipf_sampler(vocab)
    n_teams = _n_teams(n_person)

    for i in range(n_person):
        org_idx = i % max(1, n_org)
        team_id = f"team-{i % n_teams:05d}"
        yield {
            "key": f"person-{i:07d}",
            "label": "Person",
            "props": {
                "name": f"{w(rng)} {w(rng)}",
                "org_id": f"org-{org_idx:06d}",
                "team_id": team_id,
                "domain": w(rng),
                "bio": _words(rng, w, rng.randint(5, 10)),
            },
        }
    for i in range(n_org):
        yield {
            "key": f"org-{i:06d}",
            "label": "Org",
            "props": {
                "name": f"{w(rng)} {w(rng)} org",
                "sector": w(rng),
            },
        }
    for i in range(n_project):
        yield {
            "key": f"project-{i:06d}",
            "label": "Project",
            "props": {
                "name": f"{w(rng)} {w(rng)} project",
                "status": rng.choice(("active", "completed", "planned")),
            },
        }
    for i in range(n_concept):
        yield {
            "key": f"concept-{i:06d}",
            "label": "Concept",
            "props": {
                "name": f"{w(rng)} {w(rng)}",
                "summary": _words(rng, w, rng.randint(5, 10)),
            },
        }
    for i in range(n_event):
        yield {
            "key": f"event-{i:06d}",
            "label": "Event",
            "props": {
                "name": f"{w(rng)} {w(rng)} event",
                "year": str(2020 + (i % 6)),
            },
        }


def generate_edges(
    n_person: int, n_org: int, n_project: int,
    n_concept: int, n_event: int, seed: int,
) -> list[dict]:
    """~3–5 manual edges per entity: MEMBER_OF, WORKS_ON, KNOWS, RELATED_TO, ATTENDED."""
    rng = random.Random(seed + 777)
    edges: list[dict] = []

    # Person → Org: MEMBER_OF (assigned org)
    for i in range(n_person):
        org_idx = i % max(1, n_org)
        edges.append({"edge_type": "MEMBER_OF",
                      "src": f"person-{i:07d}", "dst": f"org-{org_idx:06d}"})

    # Person → Project: WORKS_ON (1–2 projects each)
    if n_project > 0:
        for i in range(n_person):
            p1 = i % n_project
            edges.append({"edge_type": "WORKS_ON",
                           "src": f"person-{i:07d}", "dst": f"project-{p1:06d}"})
            if rng.random() < 0.4 and n_project > 1:
                p2 = (i + n_project // 3 + 1) % n_project
                if p2 != p1:
                    edges.append({"edge_type": "WORKS_ON",
                                   "src": f"person-{i:07d}", "dst": f"project-{p2:06d}"})

    # Person → Person: KNOWS (ring + 1 extra)
    for i in range(n_person):
        nxt = (i + 1) % n_person
        edges.append({"edge_type": "KNOWS",
                       "src": f"person-{i:07d}", "dst": f"person-{nxt:07d}"})

    # Concept → Concept: RELATED_TO (ring)
    if n_concept > 1:
        for i in range(n_concept):
            nxt = (i + 1) % n_concept
            edges.append({"edge_type": "RELATED_TO",
                           "src": f"concept-{i:06d}", "dst": f"concept-{nxt:06d}"})

    # Person → Event: ATTENDED (every 5th person)
    if n_event > 0:
        for i in range(0, n_person, 5):
            ev = i % n_event
            edges.append({"edge_type": "ATTENDED",
                           "src": f"person-{i:07d}", "dst": f"event-{ev:06d}"})

    return edges


def generate_remember_texts(
    n: int, n_person: int, n_project: int, seed: int,
) -> list[tuple[str, str, list[str]]]:
    """Generate (text, kind, about_keys) tuples for remember() calls."""
    rng = random.Random(seed + 42)
    vocab = _make_vocab(seed)
    w = _make_zipf_sampler(vocab)
    results = []
    kinds = ("note", "decision", "todo")
    for _ in range(n):
        text = _words(rng, w, rng.randint(10, 20))
        kind = rng.choice(kinds)
        about_keys = []
        if rng.random() < 0.7 and n_person > 0:
            about_keys.append(f"person-{rng.randrange(n_person):07d}")
        if rng.random() < 0.3 and n_project > 0:
            about_keys.append(f"project-{rng.randrange(n_project):06d}")
        results.append((text, kind, about_keys))
    return results


# ---------------------------------------------------------------------------
# RSS / timing / disk helpers
# ---------------------------------------------------------------------------


def peak_rss_bytes() -> int:
    """Process-lifetime ru_maxrss. Darwin returns bytes; Linux returns KiB."""
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


def free_disk_bytes(path: Path) -> int:
    try:
        return shutil.disk_usage(str(path)).free
    except Exception:
        return 0


def dir_size_bytes(path: Path) -> dict[str, int]:
    """Return {total, wal, snapshot, archives} byte counts for a store dir."""
    total = wal = snapshot = archives = 0
    try:
        for p in path.rglob("*"):
            if not p.is_file():
                continue
            sz = p.stat().st_size
            total += sz
            name = p.name.lower()
            if "wal" in name or name.endswith(".wal"):
                wal += sz
            elif "snapshot" in name or name.endswith(".bin"):
                snapshot += sz
            elif "archive" in name:
                archives += sz
    except Exception:
        pass
    return {"total": total, "wal": wal, "snapshot": snapshot, "archives": archives}


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
    return f"{s / 60.0:.2f} min"


def _fmt_bytes(n: int | float) -> str:
    if not math.isfinite(float(n)):
        return "n/a"
    n = float(n)
    for unit, div in (("GiB", 1024 ** 3), ("MiB", 1024 ** 2), ("KiB", 1024)):
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
        }
        block.update(extra)
        _log(
            f"  [{self.name}] {status} wall={_fmt_s(wall)} "
            f"peak={_fmt_bytes(block['peak_rss_bytes'])} "
            f"rss={_fmt_bytes(block['rss_after_bytes'])}"
        )
        return block


# ---------------------------------------------------------------------------
# Phase implementations
# ---------------------------------------------------------------------------


def phase_schema(db: GraphDb) -> dict[str, Any]:
    """Enable full-text pairs and property index for FieldEqual rule."""
    clock = _PhaseClock("schema")
    for label, field in FULLTEXT_PAIRS:
        db.enable_fulltext(label, field, if_not_exists=True)
    db.enable_index("Person", "team_id")
    return clock.stop(pairs=len(FULLTEXT_PAIRS))


def phase_ingest(
    db: GraphDb,
    n_person: int, n_org: int, n_project: int,
    n_concept: int, n_event: int, seed: int,
) -> dict[str, Any]:
    """Ingest entity nodes via ingest_batch (10k chunks), then bulk edges."""
    clock = _PhaseClock("ingest")
    n_total = 0
    t_batch = time.perf_counter()

    def _flush(chunk: list[dict]) -> None:
        nonlocal n_total
        if not chunk:
            return
        db.ingest_batch(chunk)
        n_total += len(chunk)
        dt = time.perf_counter() - t_batch
        _log(
            f"    ingest {n_total}  chunk={len(chunk)} {dt:.2f}s  "
            f"rss={_fmt_bytes(current_rss_bytes())}"
        )

    chunk: list[dict] = []
    for node in generate_nodes(n_person, n_org, n_project, n_concept, n_event, seed):
        chunk.append(node)
        if len(chunk) >= INGEST_BATCH:
            _flush(chunk)
            chunk = []
            t_batch = time.perf_counter()
    _flush(chunk)

    _log("    building and inserting edges ...")
    edge_list = generate_edges(n_person, n_org, n_project, n_concept, n_event, seed)
    t_e = time.perf_counter()
    n_edges_inserted = 0
    for i in range(0, len(edge_list), INGEST_BATCH):
        result = db.batch_edges(inserts=edge_list[i : i + INGEST_BATCH])
        n_edges_inserted += result.get("edges_inserted", 0)
    _log(
        f"    edges inserted={n_edges_inserted}/{len(edge_list)} "
        f"in {_fmt_s(time.perf_counter() - t_e)}"
    )

    block = clock.stop(nodes=n_total, edges_inserted=n_edges_inserted,
                       edges_requested=len(edge_list))
    block["throughput_nodes_s"] = n_total / max(block["wall_s"], 1e-9)
    return block


def phase_rules(db: GraphDb) -> dict[str, Any]:
    """Declare same_team FieldEqual rule."""
    clock = _PhaseClock("rules")
    db.create_rule({
        "name": "same_team",
        "src_label": "Person",
        "dst_label": "Person",
        "predicate": {"FieldEqual": {"field": "team_id"}},
        "edge_type": "COLLEAGUE",
        "weight_prop": None,
        "max_edges": RULE_MAX_EDGES,
    })
    max_str = str(RULE_MAX_EDGES) if RULE_MAX_EDGES is not None else "None (global 1M default)"
    block = clock.stop(rule="same_team", max_edges_str=max_str)
    st = db.stats()
    rule_stat = next((r for r in st.get("rules", []) if r["name"] == "same_team"), {})
    block["tripped"] = rule_stat.get("tripped", False)
    block["derived_edges"] = rule_stat.get("edges", 0)   # stats key is "edges" per-rule
    return block


def phase_remember(
    db: GraphDb,
    n_person: int, n_project: int, seed: int,
    n: int = N_REMEMBER,
) -> dict[str, Any]:
    """10k per-call remember() calls; measure p50/p95 latency."""
    clock = _PhaseClock("remember")
    texts = generate_remember_texts(n, n_person, n_project, seed)
    samples: list[float] = []
    n_errors = 0
    for text, kind, about_keys in texts:
        t0 = time.perf_counter()
        try:
            db.memory.remember(text, about=about_keys or None, kind=kind)
            samples.append(time.perf_counter() - t0)
        except Exception as exc:
            n_errors += 1
            if n_errors <= 3:
                _log(f"    remember error: {exc}")
    block = clock.stop(n=len(samples), n_errors=n_errors)
    block["p50_s"] = percentile(samples, 50)
    block["p95_s"] = percentile(samples, 95)
    block["throughput_s"] = len(samples) / max(block["wall_s"], 1e-9)
    return block


def phase_reopen_wal(db_dir: Path, db: GraphDb) -> tuple[GraphDb, dict[str, Any]]:
    """Close + open without snapshot (WAL replay + rule re-application).

    WAL replay re-inserts all nodes and re-fires CreateRule records.  At large
    scales this can be minutes; the harness measures it directly (no timeout)
    and reports the actual elapsed wall time.  This is the cold-start cost
    without a snapshot, establishing the baseline the snapshot-open improves on.
    """
    db.close()
    clock = _PhaseClock("reopen_wal")
    try:
        reopened = GraphDb.open(str(db_dir))
        return reopened, clock.stop()
    except Exception as exc:
        block = clock.stop(status="error")
        block["error"] = str(exc)
        _log(f"  [reopen_wal] error: {exc}")
        # Re-open so phases can continue.
        try:
            reopened = GraphDb.open(str(db_dir))
        except Exception:
            reopened = None
        return reopened, block  # type: ignore[return-value]


def phase_reopen_snap(db_dir: Path, db: GraphDb) -> tuple[GraphDb, dict[str, Any]]:
    """Write snapshot, close, reopen.  0.8 carries the full-text index in the
    snapshot so the open does not rebuild the index."""
    snap_clock = _PhaseClock("snapshot")
    db.snapshot()
    snap_block = snap_clock.stop()

    store_size = dir_size_bytes(db_dir)
    _log(f"    store after snapshot: total={_fmt_bytes(store_size['total'])} "
         f"wal={_fmt_bytes(store_size['wal'])} snap={_fmt_bytes(store_size['snapshot'])}")
    db.close()

    open_clock = _PhaseClock("open_snap")
    reopened = GraphDb.open(str(db_dir))
    open_block = open_clock.stop()

    return reopened, {
        "status": "ok",
        "snapshot_wall_s": snap_block["wall_s"],
        "open_wall_s": open_block["wall_s"],
        "wall_s": snap_block["wall_s"] + open_block["wall_s"],
        "peak_rss_bytes": max(snap_block["peak_rss_bytes"], open_block["peak_rss_bytes"]),
        "rss_after_bytes": open_block["rss_after_bytes"],
        "store_size": store_size,
    }


def phase_recall(
    db: GraphDb, seed: int, n_person: int,
    n_queries: int = N_RECALL_QUERIES,
) -> dict[str, Any]:
    """recall() latency over four query shapes."""
    vocab = _make_vocab(seed)
    w = _make_zipf_sampler(vocab)

    # Query terms.
    n = len(w._vocab)
    rare_word = w._vocab[n * 4 // 5]          # rank ~80%: rare
    common_word = w._vocab[0]                  # rank 1: most common
    two_word = f"{common_word} {rare_word}"
    stored_name: str | None = None
    if n_person > 0:
        info = db.node_info("person-0000000")
        if info:
            stored_name = info["props"].get("name")

    shapes = {
        "rare":        [rare_word]    * (n_queries // 4),
        "common":      [common_word]  * (n_queries // 4),
        "two_word":    [two_word]     * (n_queries // 4),
        "stored_name": [stored_name]  * (n_queries // 4) if stored_name else [],
    }

    all_samples: list[float] = []
    shape_stats: dict[str, Any] = {}
    n_hits_total = 0

    for shape_name, query_list in shapes.items():
        samples: list[float] = []
        n_hits = 0
        for q in query_list:
            if q is None:
                continue
            t0 = time.perf_counter()
            try:
                result = db.memory.recall(q)
                samples.append(time.perf_counter() - t0)
                n_hits += len(result.get("hits", []))
            except Exception as exc:
                _log(f"    recall error ({shape_name}): {exc}")
        shape_stats[shape_name] = {
            "n": len(samples),
            "p50_s": percentile(samples, 50),
            "p95_s": percentile(samples, 95),
            "mean_hits": n_hits / max(len(samples), 1),
        }
        all_samples.extend(samples)
        n_hits_total += n_hits
        _log(
            f"    recall [{shape_name}] n={len(samples)} "
            f"p50={_fmt_s(shape_stats[shape_name]['p50_s'])} "
            f"p95={_fmt_s(shape_stats[shape_name]['p95_s'])} "
            f"hits={shape_stats[shape_name]['mean_hits']:.1f}"
        )

    return {
        "status": "ok",
        "n": len(all_samples),
        "p50_s": percentile(all_samples, 50),
        "p95_s": percentile(all_samples, 95),
        "p99_s": percentile(all_samples, 99),
        "mean_hits": n_hits_total / max(len(all_samples), 1),
        "shapes": shape_stats,
        "rare_word": rare_word,
        "common_word": common_word,
        "stored_name": stored_name,
    }


def phase_lookup(
    db: GraphDb, all_keys: list[str], seed: int, n: int = N_LOOKUP,
) -> dict[str, Any]:
    """node_info and node_edges latency on random keys."""
    rng = random.Random(seed + 55)
    keys = [all_keys[rng.randrange(len(all_keys))] for _ in range(n)]
    info_s: list[float] = []
    edges_s: list[float] = []
    for key in keys:
        t0 = time.perf_counter()
        db.node_info(key)
        info_s.append(time.perf_counter() - t0)
        t0 = time.perf_counter()
        try:
            db.node_edges(key)
        except Exception:
            pass
        edges_s.append(time.perf_counter() - t0)
    return {
        "status": "ok",
        "n": n,
        "node_info_p50_s":  percentile(info_s, 50),
        "node_info_p95_s":  percentile(info_s, 95),
        "node_edges_p50_s": percentile(edges_s, 50),
        "node_edges_p95_s": percentile(edges_s, 95),
    }


def phase_query(db: GraphDb, n_person: int, seed: int) -> dict[str, Any]:
    """Cypher COUNT(*) and 1-hop KNOWS neighbourhood."""
    # Count all Person nodes.
    t0 = time.perf_counter()
    count_n = -1
    count_err: str | None = None
    try:
        rows = db.query("MATCH (n:Person) RETURN count(n) AS cnt")
        count_n = rows[0]["cnt"] if rows else 0
    except Exception as exc:
        count_err = str(exc)
        _log(f"    query COUNT error: {exc}")
    count_wall = time.perf_counter() - t0

    # 1-hop neighbourhood from a deterministic key.
    rng = random.Random(seed + 11)
    src_key = f"person-{rng.randrange(max(1, n_person)):07d}"
    t0 = time.perf_counter()
    hop1_n = -1
    hop1_err: str | None = None
    try:
        rows2 = db.query(
            "MATCH (p:Person)-[:KNOWS]->(q) WHERE key(p) = $k RETURN key(q) AS dst",
            params={"k": src_key},
        )
        hop1_n = len(rows2)
    except Exception as exc:
        hop1_err = str(exc)
        _log(f"    query 1-hop error: {exc}")
    hop1_wall = time.perf_counter() - t0

    _log(
        f"    query: count_n={count_n} wall={_fmt_s(count_wall)} | "
        f"1-hop from {src_key}: n={hop1_n} wall={_fmt_s(hop1_wall)}"
    )
    return {
        "status": "ok",
        "count_n": count_n,
        "count_wall_s": count_wall,
        "count_error": count_err,
        "hop1_src": src_key,
        "hop1_n": hop1_n,
        "hop1_wall_s": hop1_wall,
        "hop1_error": hop1_err,
    }


def phase_incr(
    db: GraphDb, n_person: int, seed: int, n: int = N_INCR,
) -> dict[str, Any]:
    """100 set_prop writes after reopen; incremental write latency."""
    rng = random.Random(seed + 99)
    vocab = _make_vocab(seed)
    w = _make_zipf_sampler(vocab)
    samples: list[float] = []
    for _ in range(n):
        key = f"person-{rng.randrange(max(1, n_person)):07d}"
        new_bio = _words(rng, w, rng.randint(5, 10))
        t0 = time.perf_counter()
        try:
            db.set_prop(key, "bio", new_bio)
            samples.append(time.perf_counter() - t0)
        except Exception as exc:
            _log(f"    set_prop error: {exc}")
    return {
        "status": "ok",
        "n": len(samples),
        "p50_s": percentile(samples, 50),
        "p95_s": percentile(samples, 95),
    }


# ---------------------------------------------------------------------------
# Machine header / guards
# ---------------------------------------------------------------------------


def _sysctl(name: str) -> str:
    try:
        return subprocess.check_output(["sysctl", "-n", name], text=True).strip()
    except Exception:
        return ""


def machine_header() -> dict[str, Any]:
    mem = _sysctl("hw.memsize")
    ram = int(mem) if mem.isdigit() else None
    try:
        pip_out = subprocess.check_output(
            [sys.executable, "-m", "pip", "show", "mushroomdb"], text=True
        )
        version = next(
            (ln.split(":", 1)[1].strip()
             for ln in pip_out.splitlines() if ln.startswith("Version:")),
            "unknown",
        )
    except Exception:
        version = "unknown"
    return {
        "date": _dt.datetime.now().isoformat(timespec="seconds"),
        "os": platform.platform(),
        "python": platform.python_version(),
        "machine": platform.machine(),
        "cpu": _sysctl("machdep.cpu.brand_string") or platform.processor() or "unknown",
        "ncpu": os.cpu_count(),
        "ram_bytes": ram,
        "hostname": platform.node(),
        "wheel_version": version,
    }


def check_guards(store_base: Path, scale: int) -> str | None:
    """Return a stop reason string, or None when safe to proceed."""
    rss = peak_rss_bytes()
    if rss > RSS_GUARD_BYTES:
        return (f"peak RSS {_fmt_bytes(rss)} > guard {_fmt_bytes(RSS_GUARD_BYTES)}"
                f" before scale={scale:,}")
    free = free_disk_bytes(store_base)
    if free < DISK_GUARD_BYTES:
        return (f"free disk {_fmt_bytes(free)} < guard {_fmt_bytes(DISK_GUARD_BYTES)}"
                f" before scale={scale:,}")
    return None


# ---------------------------------------------------------------------------
# Per-scale run
# ---------------------------------------------------------------------------


def run_scale(scale: int, seed: int, store_base: Path) -> dict[str, Any]:
    """Run all ten phases at `scale` nodes and return collected results."""
    n_person, n_org, n_project, n_concept, n_event = split_scale(scale)
    _log(f"\n=== scale={scale:,} seed={seed} ===")
    _log(f"    Person={n_person:,} Org={n_org:,} Project={n_project:,} "
         f"Concept={n_concept:,} Event={n_event:,}")

    store_dir = store_base / f"scale-{scale}"
    store_dir.mkdir(parents=True, exist_ok=True)
    phases: dict[str, Any] = {}
    findings: list[str] = []

    # (1) Schema
    _log("\n[1] schema")
    db = GraphDb.open(str(store_dir))
    phases["schema"] = phase_schema(db)

    # (2) Ingest
    _log("\n[2] bulk ingest")
    phases["ingest"] = phase_ingest(db, n_person, n_org, n_project, n_concept, n_event, seed)
    st = db.stats()
    _log(f"    nodes={st['nodes_live']} edges={st.get('edges', '?')}")

    # (3) Rules
    _log("\n[3] rule declaration")
    phases["rules"] = phase_rules(db)
    _log(f"    tripped={phases['rules']['tripped']} derived={phases['rules']['derived_edges']}")

    # (4) per-call remember
    _log(f"\n[4] per-call remember ({N_REMEMBER:,} calls)")
    phases["remember"] = phase_remember(db, n_person, n_project, seed)

    # Pre-snapshot store size.
    pre_snap = dir_size_bytes(store_dir)
    phases["store_pre_snap"] = pre_snap
    _log(f"    store (pre-snapshot): total={_fmt_bytes(pre_snap['total'])} "
         f"wal={_fmt_bytes(pre_snap['wal'])}")

    # (5) WAL-only reopen.
    _log("\n[5] WAL-only reopen")
    db, wal_block = phase_reopen_wal(store_dir, db)
    phases["reopen_wal"] = wal_block
    if wal_block["status"] != "ok":
        findings.append(f"WAL-only reopen failed at scale={scale:,}: {wal_block.get('error')}")

    # (6) Snapshot + open.
    _log("\n[6] snapshot + open")
    db, snap_block = phase_reopen_snap(store_dir, db)
    phases["reopen_snap"] = snap_block
    _log(f"    snap_write={_fmt_s(snap_block['snapshot_wall_s'])} "
         f"snap_open={_fmt_s(snap_block['open_wall_s'])} "
         f"store={_fmt_bytes(snap_block['store_size']['total'])}")

    # Sample keys for lookup / query.
    person_keys = [f"person-{i:07d}" for i in range(min(n_person, 10_000))]
    concept_keys = [f"concept-{i:06d}" for i in range(min(n_concept, 5_000))]
    all_keys = person_keys + concept_keys

    # (7) recall
    _log(f"\n[7] recall latency ({N_RECALL_QUERIES} queries)")
    phases["recall"] = phase_recall(db, seed, n_person)
    _log(f"    overall: p50={_fmt_s(phases['recall']['p50_s'])} "
         f"p95={_fmt_s(phases['recall']['p95_s'])} "
         f"p99={_fmt_s(phases['recall']['p99_s'])}")

    # (8) lookup
    _log(f"\n[8] lookup latency ({N_LOOKUP} samples)")
    phases["lookup"] = phase_lookup(db, all_keys, seed)
    _log(f"    node_info p50={_fmt_s(phases['lookup']['node_info_p50_s'])} "
         f"p95={_fmt_s(phases['lookup']['node_info_p95_s'])} | "
         f"node_edges p50={_fmt_s(phases['lookup']['node_edges_p50_s'])} "
         f"p95={_fmt_s(phases['lookup']['node_edges_p95_s'])}")

    # (9) query
    _log("\n[9] Cypher query")
    phases["query"] = phase_query(db, n_person, seed)

    # (10) incremental write
    _log(f"\n[10] incremental write ({N_INCR} set_prop)")
    phases["incr"] = phase_incr(db, n_person, seed)
    _log(f"    p50={_fmt_s(phases['incr']['p50_s'])} p95={_fmt_s(phases['incr']['p95_s'])}")

    db.close()

    # Clean up store.
    _log(f"    deleting {store_dir} ...")
    shutil.rmtree(str(store_dir), ignore_errors=True)
    _log(f"    disk free: {_fmt_bytes(free_disk_bytes(store_base))}")

    return {
        "scale": scale,
        "n_person": n_person, "n_org": n_org, "n_project": n_project,
        "n_concept": n_concept, "n_event": n_event,
        "phases": phases,
        "findings": findings,
    }


# ---------------------------------------------------------------------------
# Markdown report
# ---------------------------------------------------------------------------


def write_markdown(
    results: list[dict[str, Any]], mh: dict[str, Any], seed: int, path: Path,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    ram = _fmt_bytes(mh["ram_bytes"]) if mh.get("ram_bytes") else "unknown"
    lines: list[str] = []
    a = lines.append

    a("# Scale run — memory-shaped workload (1M-node protocol)")
    a("")
    a("## Machine / date")
    a("")
    a(f"- **Date:** {mh['date']}")
    a(f"- **Host:** {mh['hostname']}")
    a(f"- **OS:** {mh['os']}")
    a(f"- **CPU:** {mh['cpu']} ({mh['ncpu']} cores, {mh['machine']})")
    a(f"- **RAM:** {ram}")
    a(f"- **Python:** {mh['python']}")
    a(f"- **mushroomdb wheel:** {mh['wheel_version']}")
    a(f"- **Seed:** {seed}")
    a("")
    a("Peak RSS is `resource.ru_maxrss` (process-lifetime, Darwin bytes).  Current RSS")
    a("is `ps -o rss=` after the phase.  Bindings are embedded Rust via")
    a("`mushroomdb.GraphDb` — not HTTP.  This is a memory-schema workload (Person, Org,")
    a("Project, Concept, Event nodes) with **synthetic Zipf-distributed text**; numbers")
    a("are not apples-to-apples with any production deployment (different hardware,")
    a("no network, synthetic text, no real embeddings).")
    a("")
    a("## Phase timings (all scales)")
    a("")
    a("| Phase | scale | status | wall | peak RSS | RSS after | notes |")
    a("|---|---|---|---|---|---|---|")

    for r in results:
        scale = r["scale"]
        phases = r["phases"]
        for pname in ("schema", "ingest", "rules", "remember",
                      "reopen_wal", "reopen_snap", "recall", "lookup", "query", "incr"):
            b = phases.get(pname)
            if b is None:
                continue
            if pname == "ingest":
                extra = (f"nodes={b.get('nodes')} "
                         f"edges={b.get('edges_inserted')} "
                         f"tput={b.get('throughput_nodes_s', 0):.0f} n/s")
            elif pname == "remember":
                extra = (f"p50={_fmt_s(b.get('p50_s', float('nan')))} "
                         f"p95={_fmt_s(b.get('p95_s', float('nan')))} n={b.get('n')}")
            elif pname == "rules":
                extra = (f"same_team max={b.get('max_edges_str', 'N/A')} "
                         f"tripped={b.get('tripped')} "
                         f"derived={b.get('derived_edges')}")
            elif pname == "reopen_wal":
                extra = b.get("error", "WAL replay + rule re-application") if b.get("status") != "ok" else "WAL replay + rule re-application"
            elif pname == "reopen_snap":
                extra = (f"snap={_fmt_s(b.get('snapshot_wall_s', 0))}+"
                         f"open={_fmt_s(b.get('open_wall_s', 0))} "
                         f"size={_fmt_bytes(b.get('store_size', {}).get('total', 0))}")
            elif pname == "recall":
                extra = (f"p50={_fmt_s(b.get('p50_s', float('nan')))} "
                         f"p95={_fmt_s(b.get('p95_s', float('nan')))} "
                         f"p99={_fmt_s(b.get('p99_s', float('nan')))} "
                         f"mean_hits={b.get('mean_hits', 0):.1f}")
            elif pname == "lookup":
                extra = (f"info p50={_fmt_s(b.get('node_info_p50_s', float('nan')))} "
                         f"p95={_fmt_s(b.get('node_info_p95_s', float('nan')))} | "
                         f"edges p50={_fmt_s(b.get('node_edges_p50_s', float('nan')))} "
                         f"p95={_fmt_s(b.get('node_edges_p95_s', float('nan')))}")
            elif pname == "query":
                extra = (f"count={b.get('count_n')} wall={_fmt_s(b.get('count_wall_s', float('nan')))}; "
                         f"1-hop n={b.get('hop1_n')} wall={_fmt_s(b.get('hop1_wall_s', float('nan')))}")
            elif pname == "incr":
                extra = (f"p50={_fmt_s(b.get('p50_s', float('nan')))} "
                         f"p95={_fmt_s(b.get('p95_s', float('nan')))} n={b.get('n')}")
            else:
                extra = ""
            a(f"| {pname} | {scale:,} | {b.get('status', 'ok')} | "
              f"{_fmt_s(b.get('wall_s', float('nan')))} | "
              f"{_fmt_bytes(b.get('peak_rss_bytes', 0))} | "
              f"{_fmt_bytes(b.get('rss_after_bytes', 0))} | {extra} |")

    a("")
    a("## Key numbers per scale")
    a("")
    scale_labels = " | ".join(f"{r['scale']:,}" for r in results)
    a(f"| Metric | {scale_labels} |")
    sep = "|---" * (1 + len(results)) + "|"
    a(sep)

    def _row(label: str, fn: Any) -> None:
        cells = []
        for r in results:
            try:
                cells.append(str(fn(r)))
            except Exception:
                cells.append("n/a")
        a(f"| {label} | " + " | ".join(cells) + " |")

    _row("ingest wall",             lambda r: _fmt_s(r["phases"]["ingest"]["wall_s"]))
    _row("ingest throughput (n/s)", lambda r: f"{r['phases']['ingest']['throughput_nodes_s']:.0f}")
    _row("ingest peak RSS",         lambda r: _fmt_bytes(r["phases"]["ingest"]["peak_rss_bytes"]))
    _row("store pre-snapshot",      lambda r: _fmt_bytes(r["phases"]["store_pre_snap"]["total"]))
    _row("remember p50",            lambda r: _fmt_s(r["phases"]["remember"]["p50_s"]))
    _row("remember p95",            lambda r: _fmt_s(r["phases"]["remember"]["p95_s"]))
    _row("WAL reopen",              lambda r: f"{_fmt_s(r['phases']['reopen_wal']['wall_s'])} ({r['phases']['reopen_wal']['status']})")
    _row("snapshot write",          lambda r: _fmt_s(r["phases"]["reopen_snap"]["snapshot_wall_s"]))
    _row("snapshot open",           lambda r: _fmt_s(r["phases"]["reopen_snap"]["open_wall_s"]))
    _row("store post-snapshot",     lambda r: _fmt_bytes(r["phases"]["reopen_snap"]["store_size"]["total"]))
    _row("recall p50",              lambda r: _fmt_s(r["phases"]["recall"]["p50_s"]))
    _row("recall p95",              lambda r: _fmt_s(r["phases"]["recall"]["p95_s"]))
    _row("recall p99",              lambda r: _fmt_s(r["phases"]["recall"]["p99_s"]))
    _row("node_info p50",           lambda r: _fmt_s(r["phases"]["lookup"]["node_info_p50_s"]))
    _row("node_info p95",           lambda r: _fmt_s(r["phases"]["lookup"]["node_info_p95_s"]))
    _row("set_prop p50 (post-open)",lambda r: _fmt_s(r["phases"]["incr"]["p50_s"]))
    _row("set_prop p95 (post-open)",lambda r: _fmt_s(r["phases"]["incr"]["p95_s"]))

    a("")
    a("## Recall by query shape")
    a("")
    for r in results:
        a(f"### scale={r['scale']:,}")
        recall_meta = r["phases"].get("recall", {})
        a(f"- rare word: `{recall_meta.get('rare_word')}`  "
          f"common word: `{recall_meta.get('common_word')}`  "
          f"stored name: `{recall_meta.get('stored_name')}`")
        a("")
        a("| shape | n | p50 | p95 | mean hits |")
        a("|---|---|---|---|---|")
        for shape_name, st in recall_meta.get("shapes", {}).items():
            a(f"| {shape_name} | {st.get('n')} | "
              f"{_fmt_s(st.get('p50_s', float('nan')))} | "
              f"{_fmt_s(st.get('p95_s', float('nan')))} | "
              f"{st.get('mean_hits', 0):.1f} |")
        a("")

    a("## Reopen comparison")
    a("")
    a("**WAL-only reopen** replays all WAL frames including every `CreateNode` and the")
    a("`CreateRule` record for same_team.  The FieldEqual rule re-fires on every Person")
    a("node during replay.  The full-text index is rebuilt at open (not stored in WAL).")
    a("")
    a("**Snapshot open (0.8)** reads the full-text index from the snapshot, skipping the")
    a("rebuild.  Rules do not re-fire.  Snapshot includes topology + provenance.")
    a("")
    for r in results:
        wal = r["phases"].get("reopen_wal", {})
        snap = r["phases"].get("reopen_snap", {})
        a(f"- **scale={r['scale']:,}:**  "
          f"WAL open = {_fmt_s(wal.get('wall_s', float('nan')))} ({wal.get('status', 'n/a')})  |  "
          f"snapshot write = {_fmt_s(snap.get('snapshot_wall_s', 0))}  "
          f"open = {_fmt_s(snap.get('open_wall_s', 0))}  "
          f"size = {_fmt_bytes(snap.get('store_size', {}).get('total', 0))}")
    a("")

    a("## Engine defects found")
    a("")
    all_findings: list[str] = []
    for r in results:
        all_findings.extend(r.get("findings", []))
    if all_findings:
        for f in all_findings:
            a(f"- {f}")
    else:
        a("- None during this run.")
    a("")

    a("## What this shows / what it does not")
    a("")
    a("**Shows:**")
    a("- Bulk ingest throughput and peak RSS at 100k, 300k, 1M memory-schema entities")
    a("  with Zipf-distributed word text (realistic full-text postings, no embeddings).")
    a("- Per-call `remember` write-path latency (one WAL fsync per call).")
    a("- `recall` latency across four query shapes, the primary assistant read path.")
    a("- Open-after-snapshot vs WAL-only reopen at each scale, confirming 0.8's")
    a("  full-text-in-snapshot shortens cold start.")
    a("- `node_info`, `node_edges` and `set_prop` as single-node operations.")
    a("")
    a("**Does not show:**")
    a("- Semantic vector similarity (no embeddings).")
    a("- Multi-process or HTTP access.")
    a("- Production hardware (local embedded, Apple M-series, warm disk cache).")
    a("- Multi-rule or complex-rule backfill.")
    a("")

    a("## Exact commands")
    a("")
    a("```bash")
    for r in results:
        a(f"python dogfood/scale_memory.py --scale {r['scale']} --seed {seed}")
    a("```")
    a("")

    path.write_text("\n".join(lines) + "\n")
    _log(f"wrote {path}")


# ---------------------------------------------------------------------------
# CLI / main
# ---------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="mushroomdb memory-shaped scale run")
    p.add_argument("--scale", type=int, default=DEFAULT_SCALE)
    p.add_argument("--seed", type=int, default=DEFAULT_SEED)
    p.add_argument(
        "--store-dir",
        default=str(Path(tempfile.gettempdir()) / "mush-scale-memory"),
        help="Base directory for temporary stores (deleted after measuring)",
    )
    p.add_argument("--out", default=None,
                   help="Output markdown path (default: results/scale-memory-<N>.md)")
    return p


def main() -> None:
    args = build_parser().parse_args()
    scale = args.scale
    seed = args.seed
    store_base = Path(args.store_dir)
    store_base.mkdir(parents=True, exist_ok=True)

    out_path = (Path(args.out) if args.out
                else _RESULTS_DIR / f"scale-memory-{scale}.md")

    mh = machine_header()
    _log(f"mushroomdb memory scale run  scale={scale:,}  seed={seed}")
    _log(f"host={mh['hostname']}  cpu={mh['cpu']}  "
         f"ram={_fmt_bytes(mh['ram_bytes']) if mh.get('ram_bytes') else '?'}  "
         f"wheel={mh['wheel_version']}")

    stop = check_guards(store_base, scale)
    if stop:
        _log(f"STOP before scale={scale:,}: {stop}")
        sys.exit(1)

    result = run_scale(scale, seed, store_base)
    write_markdown([result], mh, seed, out_path)
    _log(f"\nDone.  Results: {out_path}")


if __name__ == "__main__":
    main()
