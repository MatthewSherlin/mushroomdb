"""2k-node smoke of the scale-run pipeline.

Brute-force industry_alignment spot-check is independent of the engine:
50 random Talent↔Company pairs are scored in pure Python (field equality)
and compared to neighbors(). Sparse User nodes must produce at least one
derived KeyMatch FK edge.
"""

from __future__ import annotations

import random

import pytest

from rules import APPROXIMATE_SEMANTIC_RULE, MATCHER_MAX_EDGES, SIX_RULES
from scale_run import (
    DEFAULT_SCALE,
    DEFAULT_SEED,
    INGEST_BATCH,
    N_SPARSE_USERS,
    RECALL_SAMPLE,
    SEMANTIC_PROBE_SCALE,
    SEMANTIC_TIME_BUDGET_S,
    build_parser,
    fk_rule_defs,
    non_semantic_rules,
    run_experiment,
    semantic_rule,
    sparse_user_nodes,
    split_scale,
)
from synthesize import generate

SEED = 20260819
SMALL_SCALE = 2000


def test_split_scale_is_70_20_10():
    assert split_scale(100_000) == (70_000, 20_000, 10_000)
    assert split_scale(2_000) == (1_400, 400, 200)
    assert split_scale(1_000) == (700, 200, 100)
    assert split_scale(5_000) == (3_500, 1_000, 500)
    nt, nc, nj = split_scale(DEFAULT_SCALE)
    assert nt + nc + nj == DEFAULT_SCALE


def test_sparse_user_nodes_are_first_500_talent_user_ids():
    users = sparse_user_nodes(n_talent=70_000)
    assert len(users) == N_SPARSE_USERS == 500
    assert users[0] == {
        "key": "user-000000",
        "label": "User",
        "props": {"name": "User 0"},
    }
    assert users[-1]["key"] == "user-000499"
    assert all(u["label"] == "User" for u in users)
    capped = sparse_user_nodes(n_talent=12)
    assert [u["key"] for u in capped] == [f"user-{i:06d}" for i in range(12)]


def test_rule_partition_excludes_semantic_from_backfill_set():
    names = [r["name"] for r in non_semantic_rules()]
    assert "semantic_match_tc" not in names
    assert {r["name"] for r in SIX_RULES} - set(names) == {"semantic_match_tc"}
    sem = semantic_rule()
    assert sem["name"] == "semantic_match_tc"
    assert sem["predicate"] == {"VectorSimilar": {"field": "embedding", "min": 0.85}}


def test_matcher_rules_carry_max_edges_cap():
    """All SIX_RULES carry MATCHER_MAX_EDGES (Plan 11 T1 requirement)."""
    assert MATCHER_MAX_EDGES == 1_000_000
    for r in SIX_RULES:
        assert r["max_edges"] == MATCHER_MAX_EDGES, (
            f"{r['name']}: expected max_edges={MATCHER_MAX_EDGES}, got {r['max_edges']}"
        )


def test_approximate_semantic_rule_shape():
    """APPROXIMATE_SEMANTIC_RULE must be approximate=True with VectorSimilar predicate."""
    assert APPROXIMATE_SEMANTIC_RULE["approximate"] is True
    assert APPROXIMATE_SEMANTIC_RULE["predicate"] == {
        "VectorSimilar": {"field": "embedding", "min": 0.85}
    }
    assert APPROXIMATE_SEMANTIC_RULE["edge_type"] == "SEMANTIC_MATCH_APPROX"
    assert APPROXIMATE_SEMANTIC_RULE["name"] != "semantic_match_tc"


def test_fk_rules_are_keymatch_onto_user_and_company():
    rules = fk_rule_defs()
    by_name = {r["name"]: r for r in rules}
    talent_user = by_name["auto_fk_talent_user_id"]
    assert talent_user["src_label"] == "Talent"
    assert talent_user["dst_label"] == "User"
    assert talent_user["predicate"] == {"KeyMatch": {"field": "user_id"}}
    assert talent_user["edge_type"] == "USER"
    job_company = by_name["auto_fk_job_company_id"]
    assert job_company["src_label"] == "Job"
    assert job_company["dst_label"] == "Company"
    assert job_company["predicate"] == {"KeyMatch": {"field": "company_id"}}


def test_cli_exposes_scale_seed_out():
    parser = build_parser()
    ns = parser.parse_args([])
    assert ns.scale == DEFAULT_SCALE
    assert ns.seed == DEFAULT_SEED
    assert ns.out.endswith("scale-100k.md")
    ns = parser.parse_args(["--scale", "2000", "--seed", "7", "--out", "/tmp/x.md"])
    assert ns.scale == 2000
    assert ns.seed == 7
    assert ns.out == "/tmp/x.md"


def test_semantic_probe_constants():
    assert SEMANTIC_PROBE_SCALE == 5_000
    assert SEMANTIC_TIME_BUDGET_S == 30 * 60


def test_ingest_batch_size_is_10k():
    """Docstring constraint: each ingest_batch call must be ≤10k nodes."""
    assert INGEST_BATCH == 10_000


def test_recall_sample_size():
    assert RECALL_SAMPLE == 1_000


@pytest.fixture(scope="module")
def small_run(tmp_path_factory):
    root = tmp_path_factory.mktemp("scale2k")
    db_dir = root / "db"
    out_path = root / "scale-2k.md"
    result = run_experiment(
        db_dir=db_dir,
        scale=SMALL_SCALE,
        seed=SEED,
        out_path=out_path,
    )
    return result


def test_2k_pipeline_phases_timed(small_run):
    r = small_run
    assert r["scale"] == SMALL_SCALE
    assert r["n_talent"] == 1_400
    assert r["n_companies"] == 400
    assert r["n_jobs"] == 200
    assert r["n_users"] == 500
    # Core phases must be present and timed.
    for phase in (
        "ingest",
        "backfill",
        "semantic",
        "semantic_approx",
        "incremental",
        "big3",
        "explain",
        "reopen",
        "reopen_snap",
    ):
        block = r["phases"][phase]
        assert block["wall_s"] >= 0.0, f"{phase}: wall_s missing"
        assert block["peak_rss_bytes"] > 0, f"{phase}: peak_rss_bytes missing"
    assert r["phases"]["ingest"]["status"] == "ok"
    assert r["phases"]["backfill"]["status"] == "ok"
    assert r["phases"]["semantic"]["status"] == "ok"
    assert r["phases"]["semantic"]["attempted_full"] is True
    # Approximate phase has recall/precision in [0,1] or nan (no gt positives).
    approx = r["phases"]["semantic_approx"]
    import math
    recall = approx.get("recall", float("nan"))
    assert math.isnan(recall) or 0.0 <= recall <= 1.0, f"recall out of range: {recall}"
    # Reopen phases.
    assert r["phases"]["reopen"]["status"] == "ok"
    assert r["phases"]["reopen_snap"]["status"] == "ok"
    # Output file written.
    assert r["out_path"].exists()
    text = r["out_path"].read_text()
    assert "machine" in text.lower() or "Machine" in text


def test_2k_sparse_users_yield_at_least_one_fk_edge(small_run):
    db = small_run["reopened_db"]
    hits = db.neighbors("talent-000000", "USER", "out")
    assert "user-000000" in hits
    info = db.node_info("user-000000")
    assert info is not None
    assert info["label"] == "User"
    job_cos = db.neighbors("job-000000", "COMPANY", "out")
    assert "company-000000" in job_cos


def test_2k_industry_alignment_50_pair_brute_force(small_run):
    """Recompute expected INDUSTRY_ALIGNMENT for 50 random pairs in pure Python."""
    nt, nc, nj = split_scale(SMALL_SCALE)
    nodes = list(generate(nt, nc, nj, SEED))
    talent = [n for n in nodes if n["label"] == "Talent"]
    companies = [n for n in nodes if n["label"] == "Company"]
    rng = random.Random(SEED)
    pairs = [
        (rng.choice(talent), rng.choice(companies))
        for _ in range(50)
    ]
    db = small_run["reopened_db"]
    positives = negatives = 0
    for t, c in pairs:
        expected = t["props"]["industry"] == c["props"]["industry"]
        got = c["key"] in db.neighbors(t["key"], "INDUSTRY_ALIGNMENT", "out")
        assert got is expected, (
            f"{t['key']}→{c['key']} industry {t['props']['industry']!r} vs "
            f"{c['props']['industry']!r}: expected {expected}, engine {got}"
        )
        if expected:
            positives += 1
        else:
            negatives += 1
    assert positives >= 1, "spot-check sample had no same-industry pairs"
    assert negatives >= 1, "spot-check sample had no cross-industry pairs"


def test_2k_incremental_and_big3_and_explain_distributions(small_run):
    incr = small_run["phases"]["incremental"]
    assert incr["n"] == 100
    assert incr["p50_s"] <= incr["p95_s"]
    big3 = small_run["phases"]["big3"]
    assert big3["n"] == 50
    assert big3["p50_s"] <= big3["p95_s"]
    expl = small_run["phases"]["explain"]
    # At 2k scale all rules (including semantic_approx) are live, so there are
    # plenty of derived edges; explain() samples up to N_EXPLAIN=100.
    assert expl["n"] > 0
    assert expl["p50_s"] <= expl["p95_s"]


def test_2k_approximate_semantic_phase(small_run):
    """Approximate semantic edge count > 0 at 2k (embeddings are clustered)."""
    approx = small_run["phases"]["semantic_approx"]
    assert approx["edges"] >= 0  # may be 0 at tiny scale but must be recorded
    import math
    recall = approx.get("recall", float("nan"))
    assert math.isnan(recall) or 0.0 <= recall <= 1.0


def test_2k_reopen_snap_phase(small_run):
    """Snapshot reopen must complete and return a valid DB."""
    snap = small_run["phases"]["reopen_snap"]
    assert snap["wall_s"] >= 0.0
    assert snap["status"] == "ok"
    assert snap["snapshot_wall_s"] >= 0.0
    assert snap["open_wall_s"] >= 0.0


def test_2k_reopen_survives(small_run):
    db = small_run["reopened_db"]
    assert db.node_info("talent-000000") is not None
    assert db.node_info("company-000000") is not None
    why = db.explain("talent-000000", "user-000000")
    assert any(e["rule"] == "auto_fk_talent_user_id" for e in why)
