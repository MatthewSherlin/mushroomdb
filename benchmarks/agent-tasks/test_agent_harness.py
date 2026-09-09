# benchmarks/agent-tasks/test_agent_harness.py
"""Harness v2 unit tests. Run: bindings/python/.venv/bin/python -m pytest benchmarks/agent-tasks/test_agent_harness.py -v"""
from __future__ import annotations
import json, sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))


def test_usage_of_reports_cache_hit_ratio():
    from run import usage_of
    u = usage_of({"usage": {"input_tokens": 100, "output_tokens": 50,
                            "cache_read_input_tokens": 700, "cache_creation_input_tokens": 200}})
    assert u["total_tokens"] == 1050
    assert abs(u["cache_hit_ratio"] - 700 / 1000) < 1e-9


def test_usage_of_zero_context_has_zero_ratio():
    from run import usage_of
    assert usage_of({"usage": {}})["cache_hit_ratio"] == 0.0


def test_parse_stream_counts_tool_search_and_graph_calls(tmp_path):
    from run import parse_stream
    events = [
        {"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "ToolSearch", "input": {"query": "select:mcp__mushroomdb__explore"}},
            {"type": "tool_use", "name": "mcp__mushroomdb__explore", "input": {"target": "x"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "mushroomdb explore render_map"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "/opt/bin/mushroomdb query 'MATCH (n) RETURN n'"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "grep -rn mushroomdb ."}},
            {"type": "tool_use", "name": "Grep", "input": {"pattern": "foo"}},
        ]}},
        {"type": "result", "result": "done", "usage": {}},
    ]
    p = tmp_path / "s.jsonl"
    p.write_text("\n".join(json.dumps(e) for e in events))
    parsed = parse_stream(p)
    assert parsed["tool_search_calls"] == 1
    assert parsed["graph_calls"] == 3
    assert parsed["tool_calls"]["Grep"] == 1


def test_cell_command_arm_d_has_no_mcp_and_no_prefix():
    from run import cell_command, MCP_TOOL, SUBJECT_D
    cmd, cwd = cell_command("D", "find the retry policy", 30, None)
    assert cwd == SUBJECT_D
    assert "--strict-mcp-config" in cmd
    tools = cmd[cmd.index("--allowedTools") + 1]
    assert MCP_TOOL not in tools
    assert "Edit" in tools and "Write" in tools
    assert cmd[cmd.index("--max-turns") + 1] == "30"
    assert cmd[2] == "find the retry policy"


def test_cell_command_arm_c_prefixes_and_allows_mcp():
    from run import cell_command, MCP_TOOL
    cmd, _ = cell_command("C", "q", 12, None)
    assert cmd[2] == "/mushroom q"
    assert MCP_TOOL in cmd[cmd.index("--allowedTools") + 1]


def test_cell_command_cwd_override_wins(tmp_path):
    from run import cell_command
    _, cwd = cell_command("A", "q", 30, tmp_path)
    assert cwd == tmp_path


# --- grading -------------------------------------------------------------


def test_grade_change_task_needs_tests_and_files():
    from ground_truth import grade
    task = {"kind": "change", "checks": [], "truth": {"files": ["a.py", "b.py"]}}
    assert grade(task, "", {"a.py", "b.py"}, 0)["score"] == 1.0
    assert grade(task, "", {"a.py"}, 0)["score"] == 0.5
    assert grade(task, "", {"a.py", "b.py"}, 1)["score"] == 0.0


def test_grade_change_task_without_a_verify_run_scores_zero():
    from ground_truth import grade
    task = {"kind": "change", "checks": [], "truth": {"files": ["a.py"]}}
    assert grade(task, "I renamed it", None, None)["score"] == 0.0
    assert grade(task, "I renamed it", set(), 0)["score"] == 0.0


def test_grade_change_task_counts_files_outside_the_truth_set():
    from ground_truth import grade
    task = {"kind": "change", "checks": [], "truth": {"files": ["a.py"]}}
    got = grade(task, "", {"a.py", "junk.txt"}, 0)
    assert got["score"] == 1.0 and got["wrong_extra"] == 1


def test_grade_text_task_unchanged():
    from ground_truth import grade
    task = {"kind": "navigate", "checks": [{"kind": "text", "value": "src/x.rs"}], "truth": {}}
    assert grade(task, "it is in src/x.rs", None, None)["score"] == 1.0


# --- statistics and the gate --------------------------------------------


def test_bootstrap_ci_brackets_the_mean():
    from run import bootstrap_ci
    lo, hi = bootstrap_ci([1.0, 2.0, 3.0, 4.0], n=500, seed=1)
    assert lo <= 2.5 <= hi


def test_bootstrap_ci_of_nothing_is_zero():
    from run import bootstrap_ci
    assert bootstrap_ci([]) == (0.0, 0.0)


def _row(arm, task, score, cost, adopted, outcome="success"):
    return {"arm": arm, "task": task, "score": score, "cost_usd": cost,
            "adopted": adopted, "result_subtype": outcome}


def test_gate_passes_when_graph_arm_beats_stock():
    from run import gate_verdict
    rows = []
    for t in range(1, 5):
        rows += [_row("A", t, 0.8, 0.10, False), _row("C", t, 0.9, 0.09, True), _row("D", t, 0.7, 0.05, True)]
    v = gate_verdict(rows)
    assert v["passed"] and v["best_arm"] == "C"


def test_gate_fails_on_max_turns_where_stock_succeeds():
    from run import gate_verdict
    rows = [_row("A", 1, 1.0, 0.1, False), _row("C", 1, 0.0, 0.2, True, "error_max_turns"),
            _row("A", 2, 1.0, 0.1, False), _row("C", 2, 1.0, 0.1, True)]
    v = gate_verdict(rows)
    assert not v["passed"] and any("max-turns" in r for r in v["reasons"])


def test_gate_fails_on_low_adoption():
    from run import gate_verdict
    rows = [_row("A", t, 0.5, 0.1, False) for t in range(1, 6)]
    rows += [_row("C", t, 0.9, 0.05, t <= 3) for t in range(1, 6)]   # 60% adoption
    assert not gate_verdict(rows)["passed"]


def test_gate_without_a_stock_arm_cannot_pass():
    from run import gate_verdict
    v = gate_verdict([_row("C", 1, 1.0, 0.0, True)])
    assert not v["passed"] and v["reasons"] == ["no stock arm"]


# --- summary -------------------------------------------------------------


def _cell(arm, task, **over):
    row = {"task": task, "arm": arm, "rep": 1, "ok": True, "timed_out": False,
           "returncode": 0, "wall_seconds": 1.0, "duration_ms": 1000,
           "num_turns": 7, "cost_usd": 0.1, "is_error": False,
           "input_tokens": 10, "output_tokens": 5, "cache_read_tokens": 70,
           "cache_creation_tokens": 20, "total_tokens": 105,
           "cache_hit_ratio": 0.7, "tool_calls_total": 3, "mcp_calls": 0,
           "tool_calls": {}, "tool_search_calls": 0, "graph_calls": 0,
           "adopted": False, "score": 0.5, "wrong_extra": 0, "missed": [],
           "answer": "a", "answer_source": "result", "permission_denials": 0,
           "result_subtype": "success", "stream_file": "s.jsonl",
           "verify_rc": None, "diff_files": []}
    row.update(over)
    return row


def test_write_summary_reports_the_gate_deltas_and_the_new_columns(tmp_path):
    from run import write_summary
    rows = []
    for t in (1, 2):
        rows.append(_cell("A", t, score=0.5, cost_usd=0.20))
        rows.append(_cell("C", t, score=0.9, cost_usd=0.10, adopted=True,
                          graph_calls=2, mcp_calls=2, tool_search_calls=1,
                          cache_hit_ratio=0.9))
    text = write_summary(tmp_path, rows, {"head_short": "abc1234"}).read_text()
    assert "## Gate" in text and "## Deltas vs stock" in text
    assert "PASSED" in text
    assert "cache hit" in text and "graph calls" in text
    assert "tool search" in text and "adoption" in text


# --- the task set --------------------------------------------------------


def test_r2_pins_a_repository_and_a_sha():
    import truth_r2
    assert truth_r2.R2["url"].startswith("https://")
    assert len(truth_r2.R2["sha"]) == 40
    assert truth_r2.R2["kind"] in {"python", "typescript"}


def test_task_set_is_twenty_tasks_over_two_repos_in_the_designed_mix():
    tasks = json.loads((HERE / "tasks.json").read_text())["tasks"]
    assert len(tasks) == 20
    assert len({t["id"] for t in tasks}) == 20
    for repo in ("R1", "R2"):
        mix = {}
        for t in tasks:
            if t["repo"] == repo:
                mix[t["kind"]] = mix.get(t["kind"], 0) + 1
        assert mix == {"change": 3, "blast": 2, "localize": 2,
                       "navigate": 2, "history": 1}, (repo, mix)


def test_every_change_task_carries_an_executable_verify_and_file_truth():
    tasks = json.loads((HERE / "tasks.json").read_text())["tasks"]
    for t in tasks:
        if t["kind"] == "change":
            assert t["verify"]["cmd"], t["key"]
            assert t["truth"]["files"], t["key"]
        else:
            assert "verify" not in t, t["key"]


def test_every_task_survived_the_pilot_turn_floor():
    tasks = json.loads((HERE / "tasks.json").read_text())["tasks"]
    assert all(t["min_stock_turns"] >= 6 for t in tasks), \
        [(t["key"], t.get("min_stock_turns")) for t in tasks]


def test_no_prompt_points_the_agent_at_the_graph():
    """§3.2: a prompt never mentions the thing under test.

    R1 *is* mushroomdb, so its prompts name the crates and commands they ask
    about — that is the repository, not the door. What no prompt may do is
    name the graph door itself: the skill, the store, an MCP tool, or the
    ingest that fills it.
    """
    tasks = json.loads((HERE / "tasks.json").read_text())["tasks"]
    banned = ("/mushroom", "mushroom-memory", "mcp__", "ingest-git",
              "the graph", "code graph")
    for t in tasks:
        low = t["full_prompt"].lower()
        assert not any(b in low for b in banned), (t["key"], low)
        if t["repo"] == "R2":
            assert "mushroom" not in low, t["key"]


# --- change-task plumbing ------------------------------------------------


def test_pilot_drops_only_the_tasks_stock_found_easy():
    from run import apply_pilot
    data = {"tasks": [{"id": 1, "key": "easy"}, {"id": 2, "key": "hard"},
                      {"id": 3, "key": "ran-out"}, {"id": 4, "key": "not-piloted"}]}
    rows = [{"task": 1, "num_turns": 3}, {"task": 2, "num_turns": 11},
            {"task": 3, "num_turns": None}]
    out, turns, dropped = apply_pilot(data, rows, 30, "run-1")
    assert [t["key"] for t in out["tasks"]] == ["hard", "ran-out", "not-piloted"]
    assert out["tasks"][0]["min_stock_turns"] == 11
    assert out["tasks"][1]["min_stock_turns"] == 30      # spent every turn it had
    assert dropped == [("easy", 3)]
    assert out["pilot"]["dropped"] == [{"key": "easy", "turns": 3}]


def test_changed_files_sees_edits_and_new_files(tmp_path):
    import subprocess
    from run import changed_files
    run = lambda *a: subprocess.run(a, cwd=tmp_path, check=True,
                                    capture_output=True, text=True)
    run("git", "init", "-q")
    run("git", "config", "user.email", "t@example.com")
    run("git", "config", "user.name", "t")
    (tmp_path / "a.txt").write_text("one\n")
    run("git", "add", "a.txt")
    run("git", "commit", "-qm", "first")
    (tmp_path / "a.txt").write_text("two\n")
    (tmp_path / "b.txt").write_text("new\n")
    assert changed_files(tmp_path) == {"a.txt", "b.txt"}
