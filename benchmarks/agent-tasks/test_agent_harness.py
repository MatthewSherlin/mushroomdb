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


def test_grade_set_check_scores_the_fraction_it_names():
    """A `set` check is graded as recall over the truth set, minus a penalty
    for each near-miss the answer names. Floor is zero: a wrong answer scores
    nothing, it never scores negative."""
    from ground_truth import grade
    task = {"kind": "why", "truth": {}, "extras": {"kind": "none"},
            "checks": [{"kind": "set",
                        "values": ["company-000001", "company-000002",
                                   "company-000003", "company-000004"],
                        "forbid": ["company-000900", "company-000901"]}]}
    everything = "\n".join(f"company-00000{i}" for i in (1, 2, 3, 4))
    assert grade(task, everything)["score"] == 1.0
    assert grade(task, everything)["wrong_extra"] == 0

    half = "company-000001\ncompany-000002"
    assert grade(task, half)["score"] == 0.5

    penalised = grade(task, half + "\ncompany-000900")
    assert penalised["score"] == 0.25            # 0.5 - one 0.25 penalty
    assert penalised["wrong_extra"] == 1

    floored = grade(task, "company-000900\ncompany-000901")
    assert floored["score"] == 0.0               # 0.0 - 0.5, floored
    assert floored["wrong_extra"] == 2


def test_grade_set_check_unit_passes_only_on_a_clean_sweep():
    from ground_truth import grade, unit_passed
    task = {"kind": "why", "truth": {}, "extras": {"kind": "none"},
            "checks": [{"kind": "set", "values": ["a-1", "a-2"],
                        "forbid": ["b-9"]}]}
    clean = grade(task, "a-1 a-2")["units"]
    dirty = grade(task, "a-1 a-2 b-9")["units"]
    assert len(clean) == len(dirty) == 1        # the check produced a unit
    assert unit_passed(clean[0])
    assert not unit_passed(dirty[0])


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
    assert not v["passed"] and v["reasons"] == ["no baseline arm A"]


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
    assert "## Gate" in text and "## Deltas vs arm A" in text
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

    # The association suite has no repository at all, so nothing there may
    # name the engine, its store, its tools or the shape of its data.
    assoc = json.loads((HERE / "association" / "tasks.json").read_text())["tasks"]
    assoc_banned = banned + ("mushroom", "graph", "store", "explain_association",
                             "was_linked", "node_history", "edge_history",
                             "commit", "days.json", "cypher")
    for t in assoc:
        low = t["full_prompt"].lower()
        assert not any(b in low for b in assoc_banned), (t["id"], low)


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


# --------------------------------------------------------------------------
# association suite: one world, three forms
# --------------------------------------------------------------------------


def test_world_is_deterministic_and_the_expected_shape():
    from association.build import world
    a, b = world(seed=7, scale=200), world(seed=7, scale=200)
    assert a["nodes"] == b["nodes"] and a["changes"] == b["changes"]
    assert len(a["nodes"]) == 200 and len(a["changes"]) == 300 and a["changes"][-1]["day"] <= 89
    assert {r["name"] for r in a["roles"]} == {"recruiter", "client"}


def test_a_changelog_never_touches_a_key_twice_a_day_or_after_deleting_it():
    """The two invariants the truth script leans on: replaying a day is
    order-independent within that day, and a delete is final."""
    from association.build import world
    w = world(seed=7, scale=200)
    seen: set[tuple[int, str]] = set()
    deleted: dict[str, int] = {}
    for seq, change in enumerate(w["changes"]):
        day_key = (change["day"], change["key"])
        assert day_key not in seen, f"{change['key']} changed twice on day {change['day']}"
        seen.add(day_key)
        assert change["key"] not in deleted, (
            f"change {seq} refers to {change['key']}, deleted by change "
            f"{deleted[change['key']]}")
        if change["op"] == "delete_node":
            deleted[change["key"]] = seq
    assert deleted, "a 300-change history with no deletion tests nothing"


def test_the_three_forms_carry_the_same_base_facts(tmp_path):
    from association.build import (SQLITE_NAME, equivalent, world, write_files,
                                   write_sqlite, write_store)
    from subjects import MUSHROOMDB
    w = world(seed=7, scale=200)
    sqlite_path = tmp_path / "sqlite" / SQLITE_NAME
    write_files(w, tmp_path / "files")
    write_sqlite(w, sqlite_path)
    days = write_store(w, tmp_path / "graph", MUSHROOMDB)
    assert equivalent(tmp_path / "files", sqlite_path, tmp_path / "graph")
    assert days[0] >= 0 and days[89] > days[0]


def test_the_built_store_is_never_snapshotted(tmp_path):
    """The WAL is the history every time-travel task asks about; a snapshot
    truncates it. `snapshot.bin` must not exist in a built store."""
    from association.build import SNAPSHOT_FILES, STORE_NAME, world, write_store
    from subjects import MUSHROOMDB
    w = world(seed=7, scale=200)
    write_store(w, tmp_path / "graph", MUSHROOMDB)
    present = {p.name for p in (tmp_path / "graph" / STORE_NAME).iterdir()}
    assert present.isdisjoint(SNAPSHOT_FILES), f"store was snapshotted: {present}"
    assert "wal.bin" in present, f"store has no WAL: {present}"
    assert {p.name for p in (tmp_path / "graph").iterdir()} == {
        STORE_NAME, "README.md", "days.json", "schema.json"}


def test_a_deleted_key_is_absent_later_and_present_earlier(tmp_path):
    """Time travel is the point of the graph form: the store must still be
    able to show a node the changelog removed, at a commit before the removal."""
    from association.build import world, write_store
    from subjects import MUSHROOMDB
    from mushroomdb import GraphDb
    w = world(seed=7, scale=200)
    days = write_store(w, tmp_path / "graph", MUSHROOMDB)
    gone = next(c for c in w["changes"] if c["op"] == "delete_node")
    before, after = days[gone["day"] - 1], days[89]
    db = GraphDb.open(str(tmp_path / "graph" / "world.mushroomdb"), read_only=True)
    try:
        was = {r["n"] for r in db.query_at(before, "MATCH (n) RETURN n")}
        now = {r["n"] for r in db.query_at(after, "MATCH (n) RETURN n")}
    finally:
        db.close()
    assert gone["key"] in was
    assert gone["key"] not in now


# --------------------------------------------------------------------------
# association suite: the truth script
# --------------------------------------------------------------------------


def _node(key, label, **props):
    return {"key": key, "label": label, "props": props}


def _rule(name, src, dst, predicate, edge_type):
    return {"name": name, "src_label": src, "dst_label": dst,
            "predicate": predicate, "edge_type": edge_type,
            "weight_prop": "score", "max_edges": None}


def test_predicate_holds_is_the_engines_definition_not_a_guess():
    """The four predicates, read off `crates/core-rules/src/def.rs`.

    `Overlap` is the one worth pinning: the denominator is the **union**, so
    the score is the Jaccard index, and a non-empty intersection is required
    on top of the threshold.
    """
    from association.truth import predicate_holds

    def holds(pred, a, b, field="f"):
        return predicate_holds({"predicate": pred},
                               _node("a", "Talent", **{field: a}),
                               _node("b", "Company", **{field: b}))

    eq = {"FieldEqual": {"field": "f"}}
    assert holds(eq, "architecture", "architecture")
    assert not holds(eq, "architecture", "interior-design")
    assert not holds(eq, None, None)             # a missing field never matches
    assert not holds(eq, ["x"], ["x"])           # a list has no ValueKey

    # Jaccard, not overlap-over-the-smaller-list: {a,b} vs {a,b,c,d} is
    # 2/4 = 0.5, which clears 0.5 and fails 0.6. Over the smaller list it
    # would be 2/2 = 1.0 and clear both — that is the guess this pins down.
    half = {"Overlap": {"field": "f", "min": 0.5}}
    steep = {"Overlap": {"field": "f", "min": 0.6}}
    assert holds(half, ["a", "b"], ["a", "b", "c", "d"])
    assert not holds(steep, ["a", "b"], ["a", "b", "c", "d"])
    assert not holds({"Overlap": {"field": "f", "min": 0.0001}}, ["a"], ["b"])
    assert not holds(half, ["a", "b"], [])       # empty union, no match
    assert not holds(half, "a", "a")             # non-list, no tokens
    assert holds(half, ["a", "a", "b"], ["a", "b"])   # duplicates collapse

    # Haversine on the WGS-84 authalic mean radius. NYC to Philadelphia is
    # ~130 km; NYC to Boston ~306 km.
    nyc, philly, boston = [40.7128, -74.0060], [39.9526, -75.1652], [42.3601, -71.0589]
    near = {"GeoRadius": {"field": "f", "km": 160.9}}
    assert holds(near, nyc, philly)
    assert not holds(near, nyc, boston)
    assert holds({"GeoRadius": {"field": "f", "km": 306.5}}, nyc, boston)
    assert not holds(near, nyc, [40.7128])       # not a lat/lon pair
    assert not holds(near, nyc, [999.0, 0.0])    # out of range

    loose = {"NumericWithin": {"field": "f", "tolerance": 1.0}}
    strict = {"NumericWithin": {"field": "f", "tolerance": 0.0}}
    assert holds(loose, 2, 3) and holds(loose, 3, 2) and holds(loose, 2, 2)
    assert not holds(loose, 2, 4)
    assert holds(strict, 3, 3)
    assert not holds(strict, 3, 4)
    assert not holds(strict, 3, None)


def test_derived_edges_on_a_six_node_world_is_the_hand_computed_set():
    from association.truth import derived_edges
    nyc, la = [40.7128, -74.0060], [34.0522, -118.2437]
    nodes = [
        _node("t1", "Talent", industry="architecture", specialties=["a", "b"],
              location=nyc, size_bucket=2),
        _node("t2", "Talent", industry="interior-design", specialties=["b", "c"],
              location=la, size_bucket=3),
        _node("t3", "Talent", industry="architecture", specialties=["a", "b"],
              location=[40.8, -74.0], size_bucket=3),
        _node("c1", "Company", industry="architecture", specialties=["a", "b"],
              location=[40.75, -74.0], size_bucket=2),
        _node("c2", "Company", industry="interior-design", specialties=["c", "d"],
              location=[34.05, -118.2], size_bucket=3),
        _node("j1", "Job", industry="architecture", specialties=["a"],
              location=[40.7, -74.0], size_bucket=3),
    ]
    rules = [
        _rule("ind", "Talent", "Company", {"FieldEqual": {"field": "industry"}},
              "INDUSTRY_ALIGNMENT"),
        _rule("spec", "Talent", "Company",
              {"Overlap": {"field": "specialties", "min": 0.5}}, "SPECIALTY_MATCH"),
        _rule("loc", "Talent", "Company",
              {"GeoRadius": {"field": "location", "km": 160.9}}, "LOCATION_FIT"),
        _rule("size", "Talent", "Job",
              {"NumericWithin": {"field": "size_bucket", "tolerance": 0.0}},
              "SIMILAR_SIZE_STRICT"),
    ]
    assert derived_edges(nodes, rules) == {
        ("INDUSTRY_ALIGNMENT", "t1", "c1"),
        ("INDUSTRY_ALIGNMENT", "t2", "c2"),
        ("INDUSTRY_ALIGNMENT", "t3", "c1"),
        ("SPECIALTY_MATCH", "t1", "c1"),
        ("SPECIALTY_MATCH", "t3", "c1"),
        ("LOCATION_FIT", "t1", "c1"),
        ("LOCATION_FIT", "t2", "c2"),
        ("LOCATION_FIT", "t3", "c1"),
        ("SIMILAR_SIZE_STRICT", "t2", "j1"),
        ("SIMILAR_SIZE_STRICT", "t3", "j1"),
    }


def _tiny_world():
    """Three talents, two companies, one job, four rules — the same shape the
    hand-computed edge set above covers, wrapped as a world so the
    history-and-role helpers can be exercised on it."""
    nyc, la = [40.7128, -74.0060], [34.0522, -118.2437]
    nodes = [
        _node("t1", "Talent", industry="architecture", specialties=["a", "b"],
              location=nyc, size_bucket=2),
        _node("t2", "Talent", industry="interior-design", specialties=["b", "c"],
              location=la, size_bucket=3),
        _node("t3", "Talent", industry="architecture", specialties=["a", "b"],
              location=[40.8, -74.0], size_bucket=3),
        _node("c1", "Company", industry="architecture", specialties=["a", "b"],
              location=[40.75, -74.0], size_bucket=2),
        _node("c2", "Company", industry="interior-design", specialties=["c", "d"],
              location=[34.05, -118.2], size_bucket=3),
        _node("j1", "Job", industry="architecture", specialties=["a"],
              location=[40.7, -74.0], size_bucket=3),
    ]
    rules = [
        _rule("ind", "Talent", "Company", {"FieldEqual": {"field": "industry"}},
              "INDUSTRY_ALIGNMENT"),
        _rule("spec", "Talent", "Company",
              {"Overlap": {"field": "specialties", "min": 0.5}}, "SPECIALTY_MATCH"),
        _rule("loc", "Talent", "Company",
              {"GeoRadius": {"field": "location", "km": 160.9}}, "LOCATION_FIT"),
        _rule("size", "Talent", "Job",
              {"NumericWithin": {"field": "size_bucket", "tolerance": 0.0}},
              "SIMILAR_SIZE_STRICT"),
    ]
    return {"nodes": nodes, "rules": rules, "changes": [],
            "roles": [{"name": "recruiter", "labels": ["Talent", "Job"]},
                      {"name": "client", "labels": ["Company", "Job"]}],
            "days": ["2026-06-01"]}


def test_why_partners_and_multihop_read_the_same_edges():
    from association.truth import derived_edges, multihop, partners, why
    w = _tiny_world()
    assert why(w, 0, "t1", "c1") == [
        ("INDUSTRY_ALIGNMENT", "ind"), ("LOCATION_FIT", "loc"),
        ("SPECIALTY_MATCH", "spec")]
    assert why(w, 0, "c1", "t1") == why(w, 0, "t1", "c1")   # either way round
    assert why(w, 0, "t2", "c1") == []

    edges = derived_edges(w["nodes"], w["rules"])
    assert partners(edges, "c1") == {"t1", "t3"}
    assert partners(edges, "c1", ["INDUSTRY_ALIGNMENT", "SPECIALTY_MATCH"]) == {
        "t1", "t3"}
    assert partners(edges, "c2", ["SPECIALTY_MATCH"]) == set()

    tri = ["INDUSTRY_ALIGNMENT", "SPECIALTY_MATCH", "LOCATION_FIT"]
    assert multihop(w["nodes"], edges, dst_label="Company", edge_types=tri,
                    min_sources=2) == {"c1"}
    assert multihop(w["nodes"], edges, dst_label="Company", edge_types=tri,
                    min_sources=3) == set()
    # t1 and t3 both reach c1 by all three; only t1 is in bucket 2, so the
    # filter takes the second source away and the bar of two stops being met.
    bucket2 = lambda n: n["props"]["size_bucket"] == 2         # noqa: E731
    assert multihop(w["nodes"], edges, dst_label="Company", edge_types=tri,
                    min_sources=1, src_filter=bucket2) == {"c1"}
    assert multihop(w["nodes"], edges, dst_label="Company", edge_types=tri,
                    min_sources=2, src_filter=bucket2) == set()


def test_retraction_names_only_the_edges_a_counterfactual_costs():
    from association.truth import retraction
    w = _tiny_world()
    # c1's specialties no longer overlap either talent's, so both specialty
    # edges go — and nothing else does.
    assert retraction(w, 0, "c1", "specialties", ["x", "y"]) == {
        ("SPECIALTY_MATCH", "t1", "c1"), ("SPECIALTY_MATCH", "t3", "c1")}
    # Moving c1 across the country costs it the location edges instead.
    assert retraction(w, 0, "c1", "location", [34.05, -118.2]) == {
        ("LOCATION_FIT", "t1", "c1"), ("LOCATION_FIT", "t3", "c1")}
    # A change that changes nothing retracts nothing.
    assert retraction(w, 0, "c1", "size_bucket", 4) == set()


def test_a_role_sees_a_relationship_only_when_it_sees_both_ends():
    from association.truth import derived_edges, visible
    w = _tiny_world()
    edges = derived_edges(w["nodes"], w["rules"])
    keys = {n["key"] for n in w["nodes"]}
    assert visible(w, "recruiter", keys, w["nodes"]) == {"t1", "t2", "t3", "j1"}
    assert visible(w, "client", keys, w["nodes"]) == {"c1", "c2", "j1"}
    # A recruiter sees Talent and Job, so the Talent-Company edges are hidden
    # however strongly they match, and the Talent-Job ones are not.
    seen = visible(w, "recruiter", edges, w["nodes"])
    assert seen == {("SIMILAR_SIZE_STRICT", "t2", "j1"),
                    ("SIMILAR_SIZE_STRICT", "t3", "j1")}
    # A client sees Company and Job, and no rule joins those two: it sees no
    # derived relationship at all.
    assert visible(w, "client", edges, w["nodes"]) == set()


def test_state_at_replays_a_set_prop_and_a_delete():
    from association.build import world
    from association.truth import state_at
    w = world(seed=7, scale=200)

    edit = next(c for c in w["changes"] if c["op"] == "set_prop")
    before = state_at(w, edit["day"] - 1)
    after = state_at(w, edit["day"])
    assert after[edit["key"]]["props"][edit["field"]] == edit["value"]
    assert before[edit["key"]]["props"][edit["field"]] != edit["value"]

    gone = next(c for c in w["changes"] if c["op"] == "delete_node")
    assert gone["key"] in state_at(w, gone["day"] - 1)
    assert gone["key"] not in state_at(w, gone["day"])

    born = next(c for c in w["changes"] if c["op"] == "insert_node")
    assert born["key"] not in state_at(w, born["day"] - 1)
    assert born["key"] in state_at(w, born["day"])

    # Day 0 is the base state: nothing has happened yet.
    assert len(state_at(w, 0)) == len(w["nodes"])


def test_the_truth_script_and_the_engine_agree_on_a_small_world(tmp_path):
    """The cross-check the suite's credibility rests on: brute force in
    Python and the engine's own derivation must name the same relationships,
    live and at a past commit.

    Nothing is excused any more: the one disagreement this ever filed was an
    engine bug and it is fixed, so every line the cross-check returns is a bug
    in one of the two truths and fails here.
    """
    from association.build import world, write_store
    from association.truth import cross_check
    from subjects import MUSHROOMDB
    w = world(seed=7, scale=200)
    write_store(w, tmp_path / "graph", MUSHROOMDB)
    problems = cross_check(w, tmp_path / "graph", seed=7,
                           live_pairs=60, time_probes=6, history_probes=6)
    assert problems == [], problems
    # And the history probe is not passing vacuously: the keys it picks are
    # keys the changelog deleted after editing — the shape that used to fail.
    import random
    from association.truth import _history_keys
    picked = _history_keys(random.Random("k"), w, 6)
    edited_then_deleted = {c["key"] for c in w["changes"] if c["op"] == "set_prop"} \
        & {c["key"] for c in w["changes"] if c["op"] == "delete_node"}
    assert set(picked) & edited_then_deleted, picked


def test_a_negative_time_probe_always_asks_about_a_derivable_type():
    """A `was_linked` probe that expects False proves nothing if no rule could
    ever have derived that type for that pair of labels — `SIMILAR_SIZE`
    between a Talent and a Job is False whatever the engine does."""
    import random
    from association.build import world
    from association.truth import (
        _label_pair_is_possible, _pick_time_probe, state_at,
    )
    w = world(seed=7, scale=200)
    rules = w["rules"]
    edge_types = sorted({r["edge_type"] for r in rules})
    commit_of = {d: d for d in range(90)}
    rng = random.Random(11)
    negatives = 0
    for _ in range(12):
        probe = _pick_time_probe(rng, w, rules, edge_types, commit_of,
                                 want_linked=False)
        assert probe is not None
        day, a, b, edge_type, expected = probe
        assert expected is False
        nodes = state_at(w, day)
        assert _label_pair_is_possible(nodes, rules, a, b, [edge_type]), (
            f"{edge_type} is not derivable between {nodes[a]['label']} and "
            f"{nodes[b]['label']}; the probe proves nothing")
        negatives += 1
    assert negatives == 12


def test_an_insert_contributes_no_prop_set_records(tmp_path):
    """What the changelog calls one `insert_node` the engine records as one
    InsertNode frame carrying the whole record — not a prop_set per field. The
    history cross-check counts on it, so it is measured rather than assumed."""
    from association.truth import INSERT_PROP_SET_RECORDS
    from mushroomdb import GraphDb
    db = GraphDb.open(str(tmp_path / "insert.mushroomdb"))
    try:
        db.insert_node("Talent", "t1", {"industry": "architecture",
                                        "size_bucket": 2, "status": "published"})
        kinds = [e["kind"] for e in db.node_history("t1")]
        assert kinds.count("prop_set") == 3 * INSERT_PROP_SET_RECORDS == 0
        db.set_prop("t1", "status", "draft")
        assert [e["kind"] for e in db.node_history("t1")].count("prop_set") == 1
    finally:
        db.close()


def test_a_deleted_nodes_property_history_survives_the_tombstone(tmp_path):
    """The disagreement this suite once filed, reduced to three calls.

    `node_history` used to keep a deleted node's insert and its delete but
    drop every `prop_set` in between: `db.rs`'s `SetPropId` branch resolved the
    id with `key_of`, which is `None` for a tombstoned id. It resolves it the
    way the other branches do now, so the property history outlives the
    tombstone and the cross-check has no known gap left to excuse.
    """
    from association import truth
    from mushroomdb import GraphDb
    assert not hasattr(truth, "KNOWN_GAPS"), "the filed gap is fixed; drop it"
    db = GraphDb.open(str(tmp_path / "gap.mushroomdb"))
    try:
        db.insert_node("Talent", "t1", {"industry": "architecture"})
        db.set_prop("t1", "industry", "interior-design")
        assert [e["kind"] for e in db.node_history("t1")] == [
            "node_inserted", "prop_set"]
        db.delete_node("t1")
        assert [e["kind"] for e in db.node_history("t1")] == [
            "node_inserted", "prop_set", "node_deleted"]
    finally:
        db.close()


# --------------------------------------------------------------------------
# association suite: the task set
# --------------------------------------------------------------------------

ASSOC_KINDS = ("why", "multihop", "retraction", "timetravel", "visibility")


def test_association_task_set_is_twenty_tasks_four_per_kind():
    data = json.loads((HERE / "association" / "tasks.json").read_text())
    tasks = data["tasks"]
    assert len(tasks) == 20
    assert len({t["id"] for t in tasks}) == 20
    mix = {}
    for t in tasks:
        mix[t["kind"]] = mix.get(t["kind"], 0) + 1
    assert mix == {k: 4 for k in ASSOC_KINDS}, mix
    assert data["prefix"].startswith("Answer using only the data")
    for t in tasks:
        assert t["suite"] == "association"
        assert t["verify"] is None
        assert t["extras"] == {"kind": "none"}
        assert t["full_prompt"].endswith(t["prompt"])
        assert data["prefix"] in t["full_prompt"]


def test_every_association_task_is_a_bounded_set_question():
    """Brute force has to be real work, and the answer has to be checkable:
    every task is graded by one `set` check whose values are the truth and
    whose forbidden values are near-misses the truth excludes."""
    data = json.loads((HERE / "association" / "tasks.json").read_text())
    for t in data["tasks"]:
        assert len(t["checks"]) == 1, t["id"]
        check = t["checks"][0]
        assert check["kind"] == "set", t["id"]
        assert check["values"], t["id"]
        assert t["truth"]["size"] == len(check["values"]), t["id"]
        assert not set(check["values"]) & set(check["forbid"]), t["id"]
        assert len(set(check["values"])) == len(check["values"]), t["id"]
        assert len(check["forbid"]) == 5, (t["id"], len(check["forbid"]))
        if t["kind"] != "why":
            assert 3 <= t["truth"]["size"] <= 40, (t["id"], t["truth"]["size"])
        # Every value the answer must name has to appear in the answer as a
        # whole token; a value that is a substring of another would be graded
        # by accident.
        for a in check["values"] + check["forbid"]:
            others = [b for b in check["values"] + check["forbid"] if b != a]
            assert not any(a.lower() in b.lower() for b in others), (t["id"], a)
        # And the question must not contain its own answer or its own traps:
        # an agent that quotes the prompt back would be scored for it.
        low = t["full_prompt"].lower()
        echoed = [v for v in check["values"] + check["forbid"] if v.lower() in low]
        assert not echoed, (t["id"], echoed)


def test_no_association_task_penalises_an_arm_for_its_own_data():
    """`SEMANTIC_MATCH` is derived from a vector only the graph form carries.
    Forbidding it would cost that arm for reading its own store and cost no
    other arm anything, so no task may name it — as truth or as a near-miss."""
    data = json.loads((HERE / "association" / "tasks.json").read_text())
    for t in data["tasks"]:
        check = t["checks"][0]
        assert "SEMANTIC_MATCH" not in check["values"] + check["forbid"], t["id"]
        assert "SEMANTIC_MATCH" not in t["full_prompt"], t["id"]


def test_every_engine_checkable_task_records_what_the_engine_was_asked():
    """Four kinds carry a claim `verify_against_store` can put to the engine;
    each one has to carry the fields that verification reads."""
    data = json.loads((HERE / "association" / "tasks.json").read_text())
    for t in data["tasks"]:
        truth = t["truth"]
        if t["kind"] == "why":
            assert len(truth["target"]) == 2 and truth["types"], t["id"]
        elif t["kind"] == "visibility":
            assert truth["target"] and truth["edge_types"], t["id"]
        elif t["kind"] == "retraction":
            # The neighbourhood before the counterfactual is what `explain`
            # can be asked about, so it is recorded, not just counted.
            assert truth["base"], t["id"]
            assert len(truth["base"]) == truth["linked_before"], t["id"]
            assert set(truth["answer"]) <= set(truth["base"]), t["id"]
            assert set(truth["forbid"]) <= set(truth["base"]), t["id"]
        elif t["kind"] == "timetravel":
            assert isinstance(truth["day"], int) and truth["edge_types"], t["id"]
        else:
            assert t["kind"] == "multihop", t["id"]


def test_association_tasks_are_not_yet_pilot_sized():
    """`min_baseline_turns` is a measurement the pilot makes; a task must not
    ship carrying one it never earned."""
    data = json.loads((HERE / "association" / "tasks.json").read_text())
    assert not any("min_baseline_turns" in t for t in data["tasks"])


# --------------------------------------------------------------------------
# association suite: the harness — arms P/Q/R, the suite switch, the gate
# --------------------------------------------------------------------------


def test_cell_command_arm_p_is_the_files_subject_without_mcp():
    from run import cell_command
    from subjects import EMPTY_MCP, MCP_TOOL, SUBJECT_P
    cmd, cwd = cell_command("P", "who matches whom?", 30, None)
    assert cwd == SUBJECT_P
    assert cmd[2] == "who matches whom?"
    assert cmd[cmd.index("--mcp-config") + 1] == str(EMPTY_MCP)
    tools = cmd[cmd.index("--allowedTools") + 1]
    assert MCP_TOOL not in tools
    assert tools == "Read,Grep,Glob,Bash,Edit,Write"


def test_cell_command_arm_q_is_the_sqlite_subject_without_mcp():
    from run import cell_command
    from subjects import EMPTY_MCP, MCP_TOOL, SUBJECT_Q
    cmd, cwd = cell_command("Q", "q", 30, None)
    assert cwd == SUBJECT_Q
    assert cmd[2] == "q"
    assert cmd[cmd.index("--mcp-config") + 1] == str(EMPTY_MCP)
    assert MCP_TOOL not in cmd[cmd.index("--allowedTools") + 1]


def test_cell_command_arm_r_is_the_graph_subject_with_mcp():
    from run import cell_command
    from subjects import MCP_TOOL, SUBJECT_R
    cmd, cwd = cell_command("R", "q", 30, None)
    assert cwd == SUBJECT_R
    assert cwd.name == "graph"
    assert cmd[2] == "q"                          # plain prompt, no prefix
    assert cmd[cmd.index("--mcp-config") + 1] == ".mcp.json"
    assert MCP_TOOL in cmd[cmd.index("--allowedTools") + 1]


def test_the_three_association_subjects_are_the_three_built_forms():
    from association.build import form_paths
    from subjects import ASSOC_BUILD, SUBJECT_P, SUBJECT_Q, SUBJECT_R
    where = form_paths(ASSOC_BUILD)
    assert (SUBJECT_P, SUBJECT_Q, SUBJECT_R) == (
        where["files"], where["sqlite"], where["graph"])


def test_load_tasks_reads_the_suites_own_file():
    from run import load_tasks
    code = load_tasks("code")
    assert len(code["tasks"]) == 20 and code["tasks"][0]["repo"] in ("R1", "R2")
    assoc = load_tasks("association")
    assert assoc["suite"] == "association" and len(assoc["tasks"]) == 20
    assert all(t["suite"] == "association" for t in assoc["tasks"])


def test_suites_pin_the_arms_baseline_and_gate_of_each_suite():
    from run import SUITES
    code, assoc = SUITES["code"], SUITES["association"]
    assert code["arms"] == ["A", "B", "C", "D"] and code["baseline"] == "A"
    assert code["gate"] == {"require_ci": False, "adoption_gate": True}
    assert code["graph_arms"] is None            # every arm but A is gated
    assert code["pilot_floor"] == 6 and code["pilot_arm"] == "A"
    assert code["turns_field"] == "min_stock_turns"
    assert code["tasks"].name == "tasks.json"

    assert assoc["arms"] == ["P", "Q", "R"] and assoc["baseline"] == "Q"
    assert assoc["gate"] == {"require_ci": True, "adoption_gate": False}
    assert assoc["graph_arms"] == ["R"]          # P is the second baseline
    assert assoc["pilot_floor"] == 5 and assoc["pilot_arm"] == "Q"
    assert assoc["turns_field"] == "min_baseline_turns"
    assert assoc["tasks"].parent.name == "association"


def test_paired_deltas_measures_against_the_baseline_it_is_given():
    from report import paired_deltas
    rows = [_row("Q", 1, 0.4, 0.1, False), _row("R", 1, 0.9, 0.1, True),
            _row("P", 1, 0.2, 0.1, False),
            _row("Q", 2, 0.5, 0.1, False), _row("R", 2, 0.8, 0.1, True),
            _row("P", 2, 0.1, 0.1, False)]
    approx = lambda ds: [round(d, 6) for d in ds]                    # noqa: E731
    assert approx(paired_deltas(rows, "R", "score", baseline="Q")) == [0.5, 0.3]
    assert approx(paired_deltas(rows, "R", "score", baseline="P")) == [0.7, 0.7]
    # No baseline cells for that arm means no differences at all, not zeros.
    assert paired_deltas(rows, "R", "score", baseline="A") == []


def _assoc_rows(r_scores, p_scores=None, q_score=0.4, r_cost=0.05,
                q_cost=0.10, adopted=True):
    """One rep per task for arms P, Q and R, with R's score per task given."""
    p_scores = p_scores or [0.2] * len(r_scores)
    rows = []
    for t, (rs, ps) in enumerate(zip(r_scores, p_scores), start=1):
        rows += [_row("Q", t, q_score, q_cost, False),
                 _row("P", t, ps, q_cost, False),
                 _row("R", t, rs, r_cost, adopted)]
    return rows


ASSOC_GATE = {"baseline": "Q", "require_ci": True, "adoption_gate": False,
              "graph_arms": ["R"]}


def test_association_gate_passes_when_the_graph_beats_both_baselines():
    from report import gate_verdict
    rows = _assoc_rows([0.9] * 6)
    v = gate_verdict(rows, **ASSOC_GATE)
    assert v["passed"] and v["best_arm"] == "R", v["reasons"]


def test_association_gate_fails_when_the_score_interval_includes_zero():
    """A positive paired mean is not enough: §1.1 wants the interval clear."""
    from report import gate_verdict
    rows = _assoc_rows([1.0, 0.0, 0.5, 0.5, 1.0, 0.0], q_score=0.4)
    v = gate_verdict(rows, **ASSOC_GATE)
    assert not v["passed"]
    assert any("interval" in r for r in v["reasons"]), v["reasons"]


def test_association_gate_fails_when_a_baseline_matches_the_graph():
    """The correctness leg is 'beats every other arm', not 'beats the
    baseline': a files arm that scores as well as the graph sinks it, and P
    winning is never itself a pass — it is the second baseline, not a
    contender."""
    from report import gate_verdict
    rows = _assoc_rows([0.9] * 6, p_scores=[0.95] * 6)
    v = gate_verdict(rows, **ASSOC_GATE)
    assert not v["passed"] and v["best_arm"] == "R"
    assert sorted(v["arms"]) == ["R"]
    assert any("P" in r for r in v["reasons"]), v["reasons"]


def test_association_gate_records_adoption_without_gating_it():
    """In arm R the store is the only data path, so adoption is not a leg."""
    from report import gate_verdict
    rows = _assoc_rows([0.9] * 6, adopted=False)
    v = gate_verdict(rows, **ASSOC_GATE)
    assert v["passed"], v["reasons"]
    # ... and the same rows fail the code suite's gate, which does gate on it.
    assert not gate_verdict(rows, baseline="Q")["passed"]


def test_association_gate_still_fails_on_cost_and_on_max_turns():
    from report import gate_verdict
    dear = _assoc_rows([0.9] * 6, r_cost=0.20)
    assert any("cost" in r for r in gate_verdict(dear, **ASSOC_GATE)["reasons"])
    rows = _assoc_rows([0.9] * 6)
    rows[2]["result_subtype"] = "error_max_turns"
    reasons = gate_verdict(rows, **ASSOC_GATE)["reasons"]
    assert any("max-turns" in r for r in reasons), reasons


def test_write_summary_names_the_suite_and_the_baseline_arm(tmp_path):
    from report import write_summary
    rows = []
    for t in (1, 2, 3):
        rows.append(_cell("Q", t, score=0.4, cost_usd=0.10))
        rows.append(_cell("P", t, score=0.2, cost_usd=0.10))
        rows.append(_cell("R", t, score=0.9, cost_usd=0.05, adopted=True,
                          mcp_calls=3, graph_calls=3))
    text = write_summary(tmp_path, rows, {
        "suite": "association", "baseline": "Q", "require_ci": True,
        "adoption_gate": False, "graph_arms": ["R"],
        "world_digest": "abc123def456", "max_turns": 30}).read_text()
    assert "- suite: association" in text
    assert "- baseline arm: Q" in text
    assert "abc123def456" in text
    assert "## Deltas vs arm Q" in text
    from subjects import ARM_PROVENANCE
    for arm in ("P", "Q", "R"):
        assert f"- arm {arm} (" in text and ARM_PROVENANCE[arm] in text
    # The code suite's own footnotes describe a run this one did not do.
    assert "DEVIATION" not in text and "R2 subject" not in text
    assert "PASSED" in text


def test_write_summary_keeps_the_code_suites_own_provenance(tmp_path):
    from report import write_summary
    rows = [_cell("A", 1, score=0.5, cost_usd=0.2),
            _cell("B", 1, score=0.6, cost_usd=0.1, adopted=True, mcp_calls=1)]
    text = write_summary(tmp_path, rows, {"head_short": "abc1234"}).read_text()
    assert "- suite: code" in text and "- baseline arm: A" in text
    assert "DEVIATION" in text and "R2 subject" in text
    assert "## Deltas vs arm A" in text


def test_the_graph_subject_holds_the_store_the_install_and_nothing_else(tmp_path):
    """§5: the install goes into an otherwise empty directory, and what it
    leaves behind is the whole of arm R's world. A stray `.gitignore` (the
    install writes one) or a leftover build artefact would be data the other
    two arms do not have."""
    from association.build import world, write_store
    from subjects import (ASSOC_GRAPH_CONTENTS, MUSHROOMDB,
                          install_association_graph)
    w = world(seed=7, scale=200)
    graph = tmp_path / "graph"
    write_store(w, graph, MUSHROOMDB)
    install_association_graph(graph)                 # raises if doctor fails
    assert {p.name for p in graph.iterdir()} == set(ASSOC_GRAPH_CONTENTS)
    assert (graph / ".mcp.json").is_file() and (graph / ".claude").is_dir()
    assert not (graph / ".gitignore").exists()
    # Idempotent: setup runs it on every invocation, not only the first.
    install_association_graph(graph)
    assert {p.name for p in graph.iterdir()} == set(ASSOC_GRAPH_CONTENTS)


def test_an_association_cell_is_a_fresh_copy_of_its_subject(tmp_path):
    """P/Q/R cells are copies, not worktrees: the subjects are not git repos,
    and a cell that writes scratch files must not hand them to the next one."""
    from subjects import assoc_cell_dir, make_cell_copy
    subject = tmp_path / "subject"
    (subject / "sub").mkdir(parents=True)
    (subject / "README.md").write_text("one")
    (subject / "sub" / "b.json").write_text("two")

    dest = tmp_path / "cells" / "assoc-Q"
    make_cell_copy(subject, dest)
    assert (dest / "README.md").read_text() == "one"
    assert (dest / "sub" / "b.json").read_text() == "two"

    (dest / "scratch.txt").write_text("the agent's leftovers")
    make_cell_copy(subject, dest)
    assert not (dest / "scratch.txt").exists()
    assert not (subject / "scratch.txt").exists()

    assert assoc_cell_dir("R").name == "assoc-R"
