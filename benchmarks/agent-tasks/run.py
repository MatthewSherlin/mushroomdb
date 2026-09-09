#!/usr/bin/env python3
"""Run the agent benchmark: stock Claude Code (arm A) vs. mushroomdb.

Each cell is one `claude -p` session in a subject clone, captured as
stream-json so tool calls can be counted and the final `result` event
supplies usage/cost/turns/duration. Change-and-pass cells run in a worktree of
their arm's clone and are graded on what they edited and whether the task's own
tests pass afterwards.

`subjects.py` owns the clones and worktrees; `report.py` owns the summary.

Usage:
  python3 run.py --setup-only
  python3 run.py --pilot
  python3 run.py --tasks 1,2 --reps 1
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from ground_truth import grade, unit_passed                      # noqa: E402
from report import write_summary                                 # noqa: E402
from subjects import (BASE_TOOLS, CELL_TIMEOUT_S,                # noqa: E402
                      DEFAULT_MAX_TURNS, EMPTY_MCP, MCP_TOOL, SUBJECT_A,
                      SUBJECT_B, SUBJECT_D, cell_worktree, changed_files,
                      child_env, drop_worktree, make_worktree, restore_subject,
                      setup, subject_root)


# --------------------------------------------------------------------------
# running one cell
# --------------------------------------------------------------------------


def cell_command(arm: str, prompt: str, max_turns: int = DEFAULT_MAX_TURNS,
                 cwd_override: Path | None = None) -> tuple[list[str], Path]:
    if arm == "A":
        cwd, mcp_config, tools = SUBJECT_A, str(EMPTY_MCP), list(BASE_TOOLS)
    elif arm == "D":
        cwd, mcp_config, tools = SUBJECT_D, str(EMPTY_MCP), list(BASE_TOOLS)
    else:
        # B and C share the arm B clone; C differs only by invoking the
        # project skill with its own trigger.
        cwd, mcp_config, tools = SUBJECT_B, ".mcp.json", list(BASE_TOOLS) + [MCP_TOOL]
        if arm == "C":
            prompt = "/mushroom " + prompt
    if cwd_override is not None:
        cwd = cwd_override
    cmd = [
        "claude", "-p", prompt,
        "--model", "sonnet",
        "--max-turns", str(max_turns),
        "--output-format", "stream-json",
        "--verbose",
        "--mcp-config", mcp_config,
        "--strict-mcp-config",
        "--setting-sources", "project",
        "--allowedTools", ",".join(tools),
    ]
    return cmd, cwd


GRAPH_BASH_RE = re.compile(r"^\s*(\S*/)?mushroomdb\s")   # the command itself, bare or by path; not an argument


def _is_graph_call(name: str, inp: dict) -> bool:
    if name.startswith(MCP_TOOL):
        return True
    return name == "Bash" and bool(GRAPH_BASH_RE.search(str(inp.get("command", ""))))


def parse_stream(path: Path) -> dict:
    tool_calls: dict[str, int] = {}
    result_event = None
    init_event = None
    last_text = ""
    tool_search = 0
    graph = 0
    for line in path.read_text(errors="replace").splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        t = ev.get("type")
        if t == "system" and ev.get("subtype") == "init":
            init_event = ev
        elif t == "result":
            result_event = ev
        msg = ev.get("message")
        if not isinstance(msg, dict):
            continue
        content = msg.get("content")
        if isinstance(content, list):
            for block in content:
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "tool_use":
                    name = block.get("name", "?")
                    tool_calls[name] = tool_calls.get(name, 0) + 1
                    if name == "ToolSearch":
                        tool_search += 1
                    if _is_graph_call(name, block.get("input") or {}):
                        graph += 1
                elif t == "assistant" and block.get("type") == "text":
                    txt = (block.get("text") or "").strip()
                    if txt:
                        last_text = txt
    return {
        "tool_calls": tool_calls,
        "result": result_event,
        "init": init_event,
        "last_assistant_text": last_text,
        "tool_search_calls": tool_search,
        "graph_calls": graph,
    }


def usage_of(result_event: dict | None) -> dict:
    u = (result_event or {}).get("usage") or {}
    tin = int(u.get("input_tokens") or 0)
    tout = int(u.get("output_tokens") or 0)
    cr = int(u.get("cache_read_input_tokens") or 0)
    cc = int(u.get("cache_creation_input_tokens") or 0)
    ctx = tin + cr + cc
    ratio = (cr / ctx) if ctx else 0.0
    return {
        "input_tokens": tin,
        "output_tokens": tout,
        "cache_read_tokens": cr,
        "cache_creation_tokens": cc,
        "total_tokens": tin + tout + cr + cc,
        "cache_hit_ratio": round(ratio, 4),
    }


VERIFY_TIMEOUT_S = 900
VERIFY_MISSING_RC = 127          # the shell's "command not found", same meaning


def run_verify(task: dict, cwd: Path, log: Path) -> int | None:
    """The task's own test command, after the agent has exited.

    `None` on a timeout, which grades the same as a failure. A command that is
    not installed grades as 127 rather than crashing the run: a missing
    toolchain is a failed cell, not a failed benchmark.
    """
    cmd = task["verify"]["cmd"]
    try:
        p = subprocess.run(cmd, cwd=cwd, env=child_env(task), capture_output=True,
                           text=True, timeout=VERIFY_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        log.write_text(f"$ {' '.join(cmd)}\nTIMEOUT after {VERIFY_TIMEOUT_S}s\n")
        return None
    except (FileNotFoundError, PermissionError) as e:
        log.write_text(f"$ {' '.join(cmd)}\nrc={VERIFY_MISSING_RC}\n\n{e}\n")
        return VERIFY_MISSING_RC
    log.write_text(f"$ {' '.join(cmd)}\nrc={p.returncode}\n\n{p.stdout}\n{p.stderr}")
    return p.returncode


def run_cell(task: dict, arm: str, rep: int, outdir: Path) -> dict:
    root = subject_root(arm, task)
    stem = f"task{task['id']:02d}_rep{rep}_arm{arm}"
    worktree = make_worktree(root, cell_worktree(arm, task)) \
        if task.get("verify") else None
    try:
        return _run_cell_in(task, arm, rep, outdir, stem, root, worktree)
    finally:
        # A crash between the checkout and the grading must not leave a
        # worktree behind: the next cell's `git worktree add` would fail on
        # the same path and the run would stop.
        if worktree is not None:
            drop_worktree(root, worktree)


def _run_cell_in(task: dict, arm: str, rep: int, outdir: Path, stem: str,
                 root: Path, worktree: Path | None) -> dict:
    cwd_override = worktree if worktree is not None else root
    cmd, cwd = cell_command(
        arm, task["full_prompt"], task.get("max_turns", DEFAULT_MAX_TURNS),
        cwd_override)
    stream_path = outdir / f"{stem}.stream.jsonl"
    err_path = outdir / f"{stem}.stderr.txt"
    started = time.time()
    timed_out = False
    rc = None
    with stream_path.open("w") as so, err_path.open("w") as se:
        proc = subprocess.Popen(cmd, cwd=cwd, stdout=so, stderr=se,
                                env=child_env(task))
        try:
            rc = proc.wait(timeout=CELL_TIMEOUT_S)
        except subprocess.TimeoutExpired:
            timed_out = True
            proc.kill()
            proc.wait()
    wall = time.time() - started

    parsed = parse_stream(stream_path)
    res = parsed["result"]
    answer = (res or {}).get("result") or ""
    answer_source = "result"
    if not answer:
        # max-turns / error runs carry no `result` string; fall back to the
        # last assistant text so a partial answer still gets graded.
        answer = parsed["last_assistant_text"]
        answer_source = "last_assistant_text" if answer else "none"
    usage = usage_of(res)
    tools = parsed["tool_calls"]
    mcp_calls = sum(v for k, v in tools.items() if k.startswith("mcp__"))

    diff_files: set[str] | None = None
    verify_rc: int | None = None
    verify_seconds = None
    if worktree is not None:
        diff_files = changed_files(worktree)
        v_started = time.time()
        verify_rc = run_verify(task, worktree, outdir / f"{stem}.verify.txt")
        verify_seconds = round(time.time() - v_started, 1)
        dirtied = bool(diff_files)
    else:
        # Every cell may Edit and Write. A non-change cell that did so was
        # working in the clone the next cell gets, so put it back and say it
        # happened.
        dirtied = restore_subject(root)

    graded = grade(task, answer, diff_files, verify_rc)
    return {
        "task": task["id"],
        "key": task.get("key"),
        "kind": task.get("kind"),
        "subject": task.get("repo", "R1"),
        "arm": arm,
        "rep": rep,
        "ok": (not timed_out) and rc == 0 and res is not None,
        "timed_out": timed_out,
        "returncode": rc,
        "wall_seconds": round(wall, 1),
        "duration_ms": (res or {}).get("duration_ms"),
        "num_turns": (res or {}).get("num_turns"),
        "cost_usd": (res or {}).get("total_cost_usd"),
        "is_error": (res or {}).get("is_error"),
        **usage,
        "tool_calls_total": sum(tools.values()),
        "mcp_calls": mcp_calls,
        "tool_calls": tools,
        "tool_search_calls": parsed["tool_search_calls"],
        "graph_calls": parsed["graph_calls"],
        "adopted": parsed["graph_calls"] >= 1,
        "score": graded["score"],
        "wrong_extra": graded["wrong_extra"],
        "missed": [u["fact"] for u in graded["units"] if not unit_passed(u)],
        "verify_rc": verify_rc,
        "verify_seconds": verify_seconds,
        "diff_files": sorted(diff_files) if diff_files is not None else None,
        "dirtied": dirtied,
        "answer": answer,
        "answer_source": answer_source,
        "permission_denials": len((res or {}).get("permission_denials") or []),
        "result_subtype": (res or {}).get("subtype"),
        "stream_file": str(stream_path),
    }


# --------------------------------------------------------------------------
# the pilot: does a stock agent have to work for this answer?
# --------------------------------------------------------------------------

PILOT_MIN_TURNS = 6


def task_fingerprint(task: dict) -> str:
    """What a pilot measurement is about: the prompt, the truth, the tests.

    A stamp is kept only while these are unchanged. Everything else about a
    task — its id, its key, bookkeeping added later — can move without making
    a measured turn count wrong.
    """
    payload = json.dumps({
        "full_prompt": task.get("full_prompt"),
        "truth": task.get("truth"),
        "verify": task.get("verify"),
    }, sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()[:16]


def pilot_turns(row: dict, cap: int) -> int:
    """What stock needed for this task.

    A cell that ran out of turns or out of time reports no `num_turns`; it
    spent everything it had, which is the opposite of the "too easy" case the
    floor drops, so it counts as the cap.
    """
    return row["num_turns"] or cap


def already_sized(task: dict) -> bool:
    """Whether this exact task already carries a measurement of its own."""
    return (task.get("min_stock_turns") is not None
            and task.get("pilot_fingerprint") == task_fingerprint(task))


def apply_pilot(data: dict, rows: list[dict], cap: int,
                run_name: str) -> tuple[dict, dict[int, int], list[tuple[str, int]]]:
    """Stamp `min_stock_turns` on the tasks a stock agent had to work for, and
    drop the ones it answered in fewer than six turns (§3.2).

    Tasks this round did not run keep the stamp they have, and `pilot.rounds`
    is appended to: the provenance of the set is cumulative, and a later round
    that re-sizes two tasks must not erase what the first round measured.
    """
    turns = {r["task"]: pilot_turns(r, cap) for r in rows}
    kept, dropped = [], []
    for t in data["tasks"]:
        if t["id"] not in turns:
            kept.append(t)                       # not piloted this round
        elif turns[t["id"]] >= PILOT_MIN_TURNS:
            kept.append({**t, "min_stock_turns": turns[t["id"]],
                         "pilot_fingerprint": task_fingerprint(t)})
        else:
            dropped.append((t["key"], turns[t["id"]]))
    rounds = list(data.get("pilot", {}).get("rounds", []))
    rounds.append({
        "run": run_name,
        "tasks": sorted(turns),
        "dropped_under_the_floor": [k for k, _ in dropped],
    })
    data = {**data, "tasks": kept,
            "pilot": {"run": run_name, "min_turns": PILOT_MIN_TURNS,
                      "dropped": [{"key": k, "turns": n} for k, n in dropped],
                      "rounds": rounds}}
    return data, turns, dropped


def pilot(max_turns: int, only: list[int] | None = None) -> int:
    """One stock (arm A) session per task, to size the tasks.

    §3.2's rule: a task a stock Sonnet agent finishes in fewer than six turns
    cannot show a graph win, so it leaves the set. Surviving tasks record what
    stock needed as `min_stock_turns`, and a task whose prompt, truth and test
    command are unchanged since it was measured is not measured again.
    """
    tasks_path = HERE / "tasks.json"
    data = json.loads(tasks_path.read_text())
    asked = [t for t in data["tasks"] if only is None or t["id"] in only]
    tasks = [t for t in asked if not already_sized(t)]
    skipped = [t for t in asked if already_sized(t)]
    if skipped:
        print("already sized, not re-running: "
              + ", ".join(f"{t['key']} ({t['min_stock_turns']})" for t in skipped))
    if not tasks:
        print("nothing to pilot")
        return 0

    ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    outdir = HERE / "results" / f"{ts}-pilot"
    outdir.mkdir(parents=True, exist_ok=True)

    rows: list[dict] = []
    for i, t in enumerate(tasks, start=1):
        print(f"[{i}/{len(tasks)}] pilot {t['key']} ({t['kind']}) ...", flush=True)
        row = run_cell({**t, "max_turns": max_turns}, "A", 1, outdir)
        rows.append(row)
        (outdir / "pilot.json").write_text(json.dumps(rows, indent=2) + "\n")
        print(f"    turns {row['num_turns']} score {row['score']:.2f} "
              f"cost ${row['cost_usd'] or 0:.4f} {row['wall_seconds']}s"
              f" [{row.get('result_subtype')}]", flush=True)

    data, turns, dropped = apply_pilot(data, rows, max_turns, outdir.name)
    tasks_path.write_text(json.dumps(data, indent=2) + "\n")

    print("\n| task | kind | stock turns | kept |")
    for r in rows:
        n = turns[r["task"]]
        print(f"| {r.get('key')} | {r.get('kind')} | {n} |"
              f" {'kept' if n >= PILOT_MIN_TURNS else 'DROPPED'} |")
    total = sum(r["cost_usd"] or 0 for r in rows)
    print(f"\npilot cost ${total:.2f}; {len(data['tasks'])} tasks in tasks.json"
          f"; dropped {dropped if dropped else 'none'}")
    return 0


# --------------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", default="all")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--arms", default="A,B,C,D")
    ap.add_argument("--max-turns", type=int, default=DEFAULT_MAX_TURNS)
    ap.add_argument("--setup-only", action="store_true")
    ap.add_argument("--force-setup", action="store_true")
    ap.add_argument("--pilot", action="store_true",
                    help="arm A only, one rep, to size the tasks (§3.2)")
    a = ap.parse_args()

    asked = [x.strip() for x in a.arms.split(",") if x.strip()]
    provisioned = setup(force=a.force_setup,
                        arms={"A"} if a.pilot else set(asked))
    if a.setup_only:
        return 0
    if a.pilot:
        only = None if a.tasks == "all" else [int(x) for x in a.tasks.split(",")]
        return pilot(a.max_turns, only)

    # An arm with no subject has no cells: scheduling it would fail inside the
    # first `Popen` and take the run with it.
    arms = [x for x in asked if x in provisioned]
    missing = [x for x in asked if x not in provisioned]
    if missing:
        print(f"skipping arm(s) {','.join(missing)}: not provisioned")
    if not arms:
        raise SystemExit("no requested arm is available")

    data = json.loads((HERE / "tasks.json").read_text())
    wanted = (
        [t["id"] for t in data["tasks"]]
        if a.tasks == "all"
        else [int(x) for x in a.tasks.split(",")]
    )
    tasks = [{**t, "max_turns": a.max_turns}
             for t in data["tasks"] if t["id"] in wanted]

    ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    outdir = HERE / "results" / ts
    outdir.mkdir(parents=True, exist_ok=True)
    (outdir / "tasks.json").write_text(json.dumps(data, indent=2) + "\n")

    # interleaved: A1,B1,A2,B2,... per rep, per task
    cells = [(t, arm, rep)
             for rep in range(1, a.reps + 1)
             for t in tasks
             for arm in arms]

    rows: list[dict] = []
    for i, (t, arm, rep) in enumerate(cells, start=1):
        print(f"[{i}/{len(cells)}] task {t['id']} arm {arm} rep {rep} ...",
              flush=True)
        row = run_cell(t, arm, rep, outdir)
        rows.append(row)
        (outdir / "cells.json").write_text(json.dumps(rows, indent=2) + "\n")
        print(f"    score {row['score']:.2f} tokens {row['total_tokens']} "
              f"tools {row['tool_calls_total']} ({row['mcp_calls']} mcp) "
              f"{row['wall_seconds']}s"
              f"{' TIMEOUT' if row['timed_out'] else ''}", flush=True)

    path = write_summary(
        outdir, rows, {"head_short": data["head_short"], "max_turns": a.max_turns})
    print(f"\nsummary: {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
