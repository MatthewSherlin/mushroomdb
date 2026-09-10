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


def test_usage_of_missing_result_is_none_not_zero():
    from run import usage_of
    u = usage_of(None)
    for key in ("input_tokens", "output_tokens", "cache_read_tokens",
                "cache_creation_tokens", "total_tokens", "cache_hit_ratio"):
        assert u[key] is None, f"{key} should be None, got {u[key]!r}"
    # A present result event with an empty usage dict still yields zeros,
    # not None — the cell ran, it just used nothing new.
    u2 = usage_of({"usage": {}})
    for key in ("input_tokens", "output_tokens", "cache_read_tokens",
                "cache_creation_tokens", "total_tokens"):
        assert u2[key] == 0


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
    from run import cell_command
    from subjects import MCP_TOOL, SUBJECT_D
    cmd, cwd = cell_command("D", "find the retry policy", 30, None)
    assert cwd == SUBJECT_D
    assert "--strict-mcp-config" in cmd
    tools = cmd[cmd.index("--allowedTools") + 1]
    assert MCP_TOOL not in tools
    assert "Edit" in tools and "Write" in tools
    assert cmd[cmd.index("--max-turns") + 1] == "30"
    assert cmd[2] == "find the retry policy"


def test_cell_command_arm_c_prefixes_and_allows_mcp():
    from run import cell_command
    from subjects import MCP_TOOL
    cmd, _ = cell_command("C", "q", 12, None)
    assert cmd[2] == "/mushroom q"
    assert MCP_TOOL in cmd[cmd.index("--allowedTools") + 1]


def test_cell_command_cwd_override_wins(tmp_path):
    from run import cell_command
    _, cwd = cell_command("A", "q", 30, tmp_path)
    assert cwd == tmp_path


def test_cell_command_arm_e_has_mcp_and_no_prefix():
    from run import cell_command
    from subjects import MCP_TOOL, SUBJECT_E
    cmd, cwd = cell_command("E", "q", 30, None)
    assert cwd == SUBJECT_E
    assert cmd[2] == "q"
    tools = cmd[cmd.index("--allowedTools") + 1]
    assert MCP_TOOL in tools
    assert cmd[cmd.index("--mcp-config") + 1] == ".mcp.json"


def test_cell_command_arm_f_has_no_mcp():
    from run import cell_command
    from subjects import EMPTY_MCP, MCP_TOOL, SUBJECT_F
    cmd, cwd = cell_command("F", "q", 30, None)
    assert cwd == SUBJECT_F
    assert cmd[2] == "q"
    tools = cmd[cmd.index("--allowedTools") + 1]
    assert MCP_TOOL not in tools
    assert cmd[cmd.index("--mcp-config") + 1] == str(EMPTY_MCP)


def test_mcp_arms_matches_what_the_cells_are_actually_given():
    """`MCP_ARMS` is what a summary says; `cell_command` is what ran."""
    from run import cell_command
    from subjects import ARM_LABEL, MCP_ARMS, MCP_TOOL
    for arm in ARM_LABEL:
        cmd, _ = cell_command(arm, "q", 30, None)
        given = MCP_TOOL in cmd[cmd.index("--allowedTools") + 1]
        assert given == (arm in MCP_ARMS), arm


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
    from report import bootstrap_ci
    lo, hi = bootstrap_ci([1.0, 2.0, 3.0, 4.0], n=500, seed=1)
    assert lo <= 2.5 <= hi


def test_bootstrap_ci_of_nothing_is_zero():
    from report import bootstrap_ci
    assert bootstrap_ci([]) == (0.0, 0.0)


def _row(arm, task, score, cost, adopted, outcome="success"):
    return {"arm": arm, "task": task, "score": score, "cost_usd": cost,
            "adopted": adopted, "result_subtype": outcome}


def test_gate_passes_when_graph_arm_beats_stock():
    from report import gate_verdict
    rows = []
    for t in range(1, 5):
        rows += [_row("A", t, 0.8, 0.10, False), _row("C", t, 0.9, 0.09, True), _row("D", t, 0.7, 0.05, True)]
    v = gate_verdict(rows)
    assert v["passed"] and v["best_arm"] == "C"


def test_gate_fails_on_max_turns_where_stock_succeeds():
    from report import gate_verdict
    rows = [_row("A", 1, 1.0, 0.1, False), _row("C", 1, 0.0, 0.2, True, "error_max_turns"),
            _row("A", 2, 1.0, 0.1, False), _row("C", 2, 1.0, 0.1, True)]
    v = gate_verdict(rows)
    assert not v["passed"] and any("max-turns" in r for r in v["reasons"])


def test_gate_fails_on_low_adoption():
    from report import gate_verdict
    rows = [_row("A", t, 0.5, 0.1, False) for t in range(1, 6)]
    rows += [_row("C", t, 0.9, 0.05, t <= 3) for t in range(1, 6)]   # 60% adoption
    assert not gate_verdict(rows)["passed"]


def test_gate_without_a_stock_arm_cannot_pass():
    from report import gate_verdict
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
    from report import write_summary
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


def test_write_summary_survives_cells_that_recorded_nothing(tmp_path):
    """A timeout and a hard error carry no cost, turns or duration."""
    from report import write_summary
    rows = [
        _cell("A", 1, score=1.0, cost_usd=0.20),
        _cell("A", 2, score=1.0, cost_usd=0.20),
        _cell("C", 1, score=0.5, cost_usd=None, num_turns=None, duration_ms=None,
              timed_out=True, ok=False, result_subtype=None, adopted=True),
        _cell("C", 2, score=0.0, cost_usd=None, num_turns=None, ok=False,
              adopted=True, result_subtype="error_max_turns"),
    ]
    text = write_summary(tmp_path, rows, {"head_short": "abc1234"}).read_text()
    assert "cells with no cost recorded | 2 of 4" in text
    assert "| timeout |" in text and "error_max_turns" in text


def test_write_summary_renders_a_failed_gate_with_its_reasons(tmp_path):
    from report import write_summary
    rows = []
    for t in (1, 2, 3):
        rows.append(_cell("A", t, score=1.0, cost_usd=0.10))
        rows.append(_cell("C", t, score=0.4, cost_usd=0.30, adopted=False))
    text = write_summary(tmp_path, rows, {"head_short": "abc1234"}).read_text()
    assert "FAILED" in text and "Why it failed:" in text
    assert "correctness" in text and "cost" in text and "adoption" in text


def test_write_summary_provenance_lists_exactly_the_arms_that_ran(tmp_path):
    """A summary describes the run it has cells for, and no other.

    The provenance block used to be three hardcoded lines, so a run of A, E
    and F opened by describing arms B and C — arms that never executed — and
    by attributing to B an install it no longer performs.
    """
    from report import write_summary
    from subjects import ARM_LABEL, ARM_PROVENANCE
    assert set(ARM_PROVENANCE) == set(ARM_LABEL)

    rows = []
    for t in (1, 2):
        for arm in ("A", "E", "F"):
            rows.append(_cell(arm, t, score=0.5, cost_usd=0.1))
    text = write_summary(tmp_path, rows, {"head_short": "abc1234"}).read_text()
    provenance = [ln for ln in text.split("## Gate", 1)[0].splitlines()
                  if ln.startswith("- arm ")]
    assert [ln.split()[2] for ln in provenance] == ["A", "E", "F"]
    for arm in ("A", "E", "F"):
        assert f"- arm {arm} ({ARM_LABEL[arm]}): {ARM_PROVENANCE[arm]}" in provenance
    # And the tool line names the MCP arms this run actually had: E, not B/C.
    assert "(+ `mcp__mushroomdb` for E)" in text


def test_rerender_rewrites_a_summary_without_moving_a_number(tmp_path):
    """`rescore.py --rerender`: new prose, same cells, and it says so."""
    import json
    import pytest
    from report import write_summary
    from rescore import rerender

    rows = [_cell("A", 1, score=1.0, cost_usd=0.2),
            _cell("B", 1, score=0.5, cost_usd=0.3, mcp_calls=1, adopted=True)]
    write_summary(tmp_path, rows, {"head_short": "abc1234", "max_turns": 30})
    (tmp_path / "cells.json").write_text(json.dumps(rows, indent=2) + "\n")
    before = (tmp_path / "summary.md").read_text()

    assert rerender(tmp_path) == tmp_path / "summary.md"
    assert (tmp_path / "summary.md").read_text() == before

    # A cells.json that disagrees with the committed summary is refused, and
    # the summary that was there is put back untouched.
    rows[1]["score"] = 0.25
    (tmp_path / "cells.json").write_text(json.dumps(rows, indent=2) + "\n")
    with pytest.raises(SystemExit, match="would change a number"):
        rerender(tmp_path)
    assert (tmp_path / "summary.md").read_text() == before


def test_write_summary_lists_every_sub_one_cell_for_classification(tmp_path):
    from report import write_summary
    rows = [_cell("A", 1, score=1.0, key="r1-change-1"),
            _cell("A", 2, score=0.5, key="r1-blast-1", missed=["crates/x.rs"])]
    text = write_summary(tmp_path, rows, {"head_short": "abc1234"}).read_text()
    assert "## Sub-1.0 cells" in text and "classification" in text
    body = text.split("## Sub-1.0 cells", 1)[1].split("## Tool adoption", 1)[0]
    assert "r1-blast-1" in body and "crates/x.rs" in body
    assert "r1-change-1" not in body       # the cell that scored 1.0 is not listed


def test_gate_correctness_is_paired_by_task(tmp_path):
    """Unpaired means would call this arm worse; paired, it wins every task."""
    from report import gate_verdict
    rows = [_row("A", 1, 0.2, 0.10, False), _row("C", 1, 0.4, 0.05, True),
            _row("A", 2, 1.0, 0.10, False), _row("C", 2, 1.0, 0.05, True),
            _row("A", 3, 0.2, 0.10, False), _row("C", 3, 0.3, 0.05, True)]
    rows.append(_row("A", 4, 1.0, 0.10, False))     # a task C never ran
    assert gate_verdict(rows)["passed"]


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
    sized = {t["key"]: t.get("min_stock_turns") for t in tasks}
    unsized = [k for k, n in sized.items() if n is None]
    assert not unsized, f"never piloted: {unsized} (run `run.py --pilot`)"
    too_easy = {k: n for k, n in sized.items() if n < 6}
    assert not too_easy, f"under the six-turn floor: {too_easy}"


def test_every_task_carries_the_fingerprint_it_was_sized_at():
    """A stamp is a measurement of a prompt; the prompt must not have moved."""
    from run import already_sized
    tasks = json.loads((HERE / "tasks.json").read_text())["tasks"]
    stale = [t["key"] for t in tasks if not already_sized(t)]
    assert not stale, f"prompt or truth changed since the pilot sized it: {stale}"


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
    assert out["pilot"]["rounds"][-1]["run"] == "run-1"


def test_apply_pilot_appends_rounds_and_keeps_earlier_stamps():
    from run import apply_pilot
    data = {"tasks": [{"id": 1, "key": "one"}, {"id": 2, "key": "two"}],
            "pilot": {"rounds": [{"run": "round-1", "tasks": [1]}]}}
    data["tasks"][0]["min_stock_turns"] = 9
    out, _turns, _dropped = apply_pilot(data, [{"task": 2, "num_turns": 8}],
                                        30, "round-2")
    assert out["tasks"][0]["min_stock_turns"] == 9       # untouched by this round
    assert out["tasks"][1]["min_stock_turns"] == 8
    assert [r["run"] for r in out["pilot"]["rounds"]] == ["round-1", "round-2"]


def test_a_sized_task_is_not_re_run_until_its_prompt_moves():
    from run import already_sized, apply_pilot, task_fingerprint
    task = {"id": 1, "key": "one", "full_prompt": "where is x?",
            "truth": {"file": "a.rs"}}
    out, _t, _d = apply_pilot({"tasks": [task]}, [{"task": 1, "num_turns": 9}],
                              30, "round-1")
    sized = out["tasks"][0]
    assert already_sized(sized)
    moved = {**sized, "full_prompt": "where is y?"}
    assert not already_sized(moved)
    assert task_fingerprint(sized) != task_fingerprint(moved)


# --- change-task plumbing ------------------------------------------------


def _git_repo(tmp_path):
    import subprocess
    run = lambda *a: subprocess.run(a, cwd=tmp_path, check=True,      # noqa: E731
                                    capture_output=True, text=True)
    run("git", "init", "-q")
    run("git", "config", "user.email", "t@example.com")
    run("git", "config", "user.name", "t")
    (tmp_path / "a.txt").write_text("one\n")
    (tmp_path / ".gitignore").write_text("kept.log\n")
    run("git", "add", "a.txt", ".gitignore")
    run("git", "commit", "-qm", "first")
    return run


def test_changed_files_sees_edits_and_new_files(tmp_path):
    from subjects import changed_files
    _git_repo(tmp_path)
    (tmp_path / "a.txt").write_text("two\n")
    (tmp_path / "b.txt").write_text("new\n")
    assert changed_files(tmp_path) == {"a.txt", "b.txt"}


def test_restore_subject_undoes_a_cell_and_says_it_had_to(tmp_path):
    from subjects import changed_files, restore_subject
    _git_repo(tmp_path)
    (tmp_path / "a.txt").write_text("edited by the agent\n")
    (tmp_path / "b.txt").write_text("new file\n")
    (tmp_path / "kept.log").write_text("the install's own artifact\n")

    assert restore_subject(tmp_path) is True
    assert changed_files(tmp_path) == set()
    assert (tmp_path / "a.txt").read_text() == "one\n"
    assert not (tmp_path / "b.txt").exists()
    assert (tmp_path / "kept.log").exists()      # ignored paths are the install
    assert restore_subject(tmp_path) is False    # a clean clone stays untouched


def test_cell_worktree_path_is_stable_per_repo():
    """Cargo fingerprints are keyed on the crate's absolute path, and cells
    run strictly sequentially, so every arm shares one worktree per repo."""
    from subjects import CELLS, cell_worktree
    a = cell_worktree("B", {"id": 1, "repo": "R1"})
    b = cell_worktree("B", {"id": 7, "repo": "R1"})
    assert a == b == CELLS / "R1"
    assert cell_worktree("B", {"id": 1, "repo": "R2"}) != a
    assert cell_worktree("C", {"id": 1, "repo": "R1"}) == a


def test_run_verify_grades_a_missing_command_rather_than_crashing(tmp_path):
    from run import VERIFY_MISSING_RC, run_verify
    log = tmp_path / "verify.txt"
    task = {"repo": "R1", "verify": {"cmd": ["definitely-not-a-real-binary", "-x"]}}
    assert run_verify(task, tmp_path, log) == VERIFY_MISSING_RC
    assert "definitely-not-a-real-binary" in log.read_text()
