#!/usr/bin/env python3
"""Run the agent benchmark: stock Claude Code (arm A) vs. mushroomdb (arm B).

Each cell is one `claude -p` session in a subject clone, captured as
stream-json so tool calls can be counted and the final `result` event
supplies usage/cost/turns/duration.

Usage:
  python3 run.py --setup-only
  python3 run.py --tasks 1,2 --reps 1
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
MUSHROOMDB = REPO / "target" / "release" / "mushroomdb"
SCRATCH = Path(os.environ.get("TMPDIR", "/tmp")) / "agent-bench"
SUBJECT_A = SCRATCH / "subject"
SUBJECT_B = SCRATCH / "subject-mdb"
SUBJECT_D = SCRATCH / "subject-cli"      # install --delivery cli, no MCP server
EMPTY_MCP = SCRATCH / "empty-mcp.json"

# R2: the second subject repository (see truth_r2.py). One local clone at the
# pinned SHA is the source for the arms' own clones, so a run costs one fetch.
R2_SRC = SCRATCH / "r2-src"
SUBJECT2_A = SCRATCH / "subject2"
SUBJECT2_B = SCRATCH / "subject2-mdb"
SUBJECT2_D = SCRATCH / "subject2-cli"
R2_VENV = SCRATCH / "venv-r2"            # what `python` means in an R2 verify

SUBJECTS = {
    ("A", "R1"): SUBJECT_A, ("B", "R1"): SUBJECT_B,
    ("C", "R1"): SUBJECT_B, ("D", "R1"): SUBJECT_D,
    ("A", "R2"): SUBJECT2_A, ("B", "R2"): SUBJECT2_B,
    ("C", "R2"): SUBJECT2_B, ("D", "R2"): SUBJECT2_D,
}

# One target directory for every cargo invocation of a run — the agent's own
# `cargo test` and the harness's verify command, in every cell. A cold build
# would cost more than the whole session.
CARGO_TARGET = SCRATCH / "target-shared"
CELLS = SCRATCH / "cells"                # per-cell worktrees for change tasks

# What an install leaves in a subject clone. A worktree gets none of it (it is
# untracked and excluded), so a change cell on a graph arm is handed a copy.
INSTALL_ARTIFACTS = (".mcp.json", ".claude", "mushroom-memory")

sys.path.insert(0, str(HERE))
import truth_r2 as TRUTH_R2                      # noqa: E402
from ground_truth import grade, unit_passed      # noqa: E402

# Both arms get exactly these tools. `Bash` is unqualified on purpose: the
# per-command form `Bash(git:*)` denies every pipeline (`git log ... | sort |
# uniq -c`), which is the natural way to answer the co-change and
# most-imported questions, so both arms burned all 12 turns on denials and
# returned nothing. See agent-eval-pilot.md, defect 1.
BASE_TOOLS = [
    "Read",
    "Grep",
    "Glob",
    "Bash",
    "Edit",
    "Write",
]
MCP_TOOL = "mcp__mushroomdb"
CELL_TIMEOUT_S = 900
DEFAULT_MAX_TURNS = 30


# --------------------------------------------------------------------------
# setup
# --------------------------------------------------------------------------


def sh(cmd: list[str], cwd: Path | None = None, timeout: int = 900) -> str:
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    if p.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)} failed ({p.returncode}):\n{p.stderr}")
    return p.stdout


def ensure_binary() -> None:
    if MUSHROOMDB.exists():
        return
    print("building release binary...")
    sh(["cargo", "build", "--release", "-p", "mushroomdb-cli"], cwd=REPO, timeout=3600)
    if not MUSHROOMDB.exists():
        sh(["cargo", "build", "--release"], cwd=REPO, timeout=3600)


def ensure_subject(subject: Path, source: Path, sha: str | None = None) -> None:
    """Clone `source` into `subject` if missing, at `sha` when one is pinned."""
    if not subject.exists():
        print(f"cloning subject -> {subject}")
        sh(["git", "clone", "-q", str(source), str(subject)])
    if sha and sh(["git", "rev-parse", "HEAD"], cwd=subject).strip() != sha:
        sh(["git", "checkout", "-q", sha], cwd=subject)


def install_subject(subject: Path, extra_install_args: list[str],
                    source: Path = REPO, sha: str | None = None) -> None:
    """Clone `subject` if missing, install mushroomdb into it (idempotent),
    hide the install's dirty-tree artifacts, and ingest its store."""
    ensure_subject(subject, source, sha)
    if not (subject / ".claude").exists():
        print(f"installing mushroomdb into {subject.name}")
        print(sh([
            str(MUSHROOMDB), "install", "--project",
            "--platform", "claude-code",
            "--command", str(MUSHROOMDB),
            "--no-prewarm", "--no-git-hooks",
            *extra_install_args,
        ], cwd=subject))
    # The install dirties the working tree (.mcp.json, .claude/, .gitignore).
    # A dirty tree makes the UserPromptSubmit recall hook report the install
    # artifacts instead of the question, so hide them the way a real project
    # would have them committed or ignored -- without adding a commit, which
    # would shift the "last N commits" windows the ground truth depends on.
    exclude = subject / ".git" / "info" / "exclude"
    marker = "mushroom-memory/"
    if marker not in exclude.read_text():
        sh(["git", "checkout", "--", ".gitignore"], cwd=subject)
        with exclude.open("a") as fh:
            fh.write("\n.mcp.json\n.claude/\nmushroom-memory/\n")
    dirty = sh(["git", "status", "--porcelain"], cwd=subject).strip()
    if dirty:
        raise SystemExit(f"{subject.name} clone is not clean after install:\n{dirty}")

    if not (subject / "mushroom-memory").exists():
        print(f"ingesting {subject.name} store")
        print(sh([str(MUSHROOMDB), "ingest-git", "./mushroom-memory", "."],
                 cwd=subject))


def delivery_flag_exists() -> bool:
    """Whether this binary's `install` understands `--delivery`.

    Arm D is defined against a flag that lands later in the plan (§4.5). Until
    it does, asking for it would fail the install, so setup says so and skips
    the arm rather than dying — and starts building it the day the flag ships.
    """
    p = subprocess.run([str(MUSHROOMDB), "install", "--help"],
                       capture_output=True, text=True, timeout=60)
    return "--delivery" in (p.stdout + p.stderr)


def ensure_r2_venv() -> None:
    """The interpreter an R2 change task's `python -m pytest` resolves to.

    Built from the subject's own locked test dependency group, outside the
    clone so no subject tree is dirtied and every per-cell worktree shares it.
    The subject package itself is deliberately not installed: `python -m
    pytest` puts the working directory first on `sys.path`, so a cell tests
    the source the agent just edited, not a copy in site-packages.
    """
    if (R2_VENV / "bin" / "python").exists():
        return
    if shutil.which("uv") is None:
        raise SystemExit("uv is required to build the R2 test venv "
                         "(https://docs.astral.sh/uv/)")
    print(f"building R2 test venv -> {R2_VENV}")
    env = dict(os.environ, UV_PROJECT_ENVIRONMENT=str(R2_VENV))
    p = subprocess.run(
        ["uv", "sync", "--frozen", "--no-install-project",
         "--group", TRUTH_R2.TEST_DEP_GROUP, "--python", "3.12"],
        cwd=R2_SRC, env=env, capture_output=True, text=True, timeout=1800)
    if p.returncode != 0:
        raise SystemExit(f"uv sync failed:\n{p.stderr}")


def setup(force: bool = False, arms: set[str] | None = None) -> None:
    if arms is None:
        arms = {"A", "B", "C", "D"}
    ensure_binary()
    SCRATCH.mkdir(parents=True, exist_ok=True)
    CARGO_TARGET.mkdir(parents=True, exist_ok=True)
    EMPTY_MCP.write_text('{"mcpServers":{}}\n')

    want_d = "D" in arms
    if want_d and not delivery_flag_exists():
        print("arm D: this binary's install has no --delivery flag yet "
              "(§4.5); skipping its subjects")
        want_d = False

    if force:
        dirs = [SUBJECT_A, SUBJECT_B, SUBJECT2_A, SUBJECT2_B]
        if want_d:
            dirs += [SUBJECT_D, SUBJECT2_D]
        for d in dirs:
            shutil.rmtree(d, ignore_errors=True)
    shutil.rmtree(CELLS, ignore_errors=True)

    ensure_subject(SUBJECT_A, REPO)
    # arm A must stay clean: no .mcp.json, no .claude/
    for stray in (SUBJECT_A / ".mcp.json", SUBJECT_A / ".claude"):
        if stray.exists():
            raise SystemExit(f"arm A clone is contaminated: {stray}")

    install_subject(SUBJECT_B, [])

    # Arm D: mushroomdb installed via `--delivery cli` (no MCP server).
    if want_d:
        install_subject(SUBJECT_D, ["--delivery", "cli"])

    # R2: one pinned clone feeds the arms' clones, and the venv the change
    # tasks test in.
    TRUTH_R2.ensure_clone(R2_SRC)
    sha = TRUTH_R2.R2["sha"]
    ensure_subject(SUBJECT2_A, R2_SRC, sha)
    for stray in (SUBJECT2_A / ".mcp.json", SUBJECT2_A / ".claude"):
        if stray.exists():
            raise SystemExit(f"arm A R2 clone is contaminated: {stray}")
    install_subject(SUBJECT2_B, [], source=R2_SRC, sha=sha)
    if want_d:
        install_subject(SUBJECT2_D, ["--delivery", "cli"], source=R2_SRC, sha=sha)
    ensure_r2_venv()

    tasks_path = HERE / "tasks.json"
    head = sh(["git", "rev-parse", "HEAD"], cwd=SUBJECT_A).strip()
    stale = True
    if tasks_path.exists():
        old = json.loads(tasks_path.read_text())
        stale = old.get("head") != head or old.get("head2") != sha
    if force or stale:
        print("regenerating ground truth from the arm A clones")
        sh([sys.executable, str(HERE / "ground_truth.py"),
            "--repo", str(SUBJECT_A), "--repo2", str(SUBJECT2_A),
            "--out", str(tasks_path)])
    else:
        # A pilot writes `min_stock_turns` into tasks.json; rebuilding an
        # unchanged task set would throw that away.
        print("ground truth is current for both subjects; keeping tasks.json")
    print("setup complete")


# --------------------------------------------------------------------------
# running one cell
# --------------------------------------------------------------------------


def child_env(task: dict | None = None) -> dict[str, str]:
    env = dict(os.environ)
    for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT"):
        env.pop(k, None)
    # Every cargo invocation of a run — the agent's own and the harness's —
    # shares one target directory, so only the first pays for a build.
    env["CARGO_TARGET_DIR"] = str(CARGO_TARGET)
    if task is not None and task.get("repo") == "R2":
        # `python` means the subject's own test venv, for the agent and for
        # the verify command alike.
        env["VIRTUAL_ENV"] = str(R2_VENV)
        env["PATH"] = f"{R2_VENV / 'bin'}{os.pathsep}{env.get('PATH', '')}"
    return env


ARM_LABEL = {
    "A": "stock",
    "B": "mushroomdb installed",
    "C": "mushroomdb, /mushroom invoked",
    "D": "mushroomdb, cli delivery (no MCP)",
}


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


# --------------------------------------------------------------------------
# change-and-pass cells: a worktree per cell, tests after the agent exits
# --------------------------------------------------------------------------


def subject_root(arm: str, task: dict) -> Path:
    return SUBJECTS[(arm, task.get("repo", "R1"))]


def changed_files(cwd: Path) -> set[str]:
    """Every tracked file the working tree changed, plus every new file.

    Ignored paths stay out, which is how the install artifacts a graph cell
    was handed do not count as the agent's work.
    """
    out = sh(["git", "status", "--porcelain", "--untracked-files=all"], cwd=cwd)
    files: set[str] = set()
    for line in out.splitlines():
        path = line[3:]
        if not path:
            continue
        if " -> " in path:            # renames report `old -> new`
            path = path.split(" -> ", 1)[1]
        files.add(path.strip().strip('"'))
    return files


def make_worktree(subject: Path, dest: Path) -> Path:
    """A fresh checkout of the subject's HEAD, carrying the install artifacts.

    A worktree gets tracked files only. Arms B, C and D need the MCP config,
    the skill and the store to be in the cell too, or the change tasks would
    measure a stock session wearing another arm's name.
    """
    shutil.rmtree(dest, ignore_errors=True)
    dest.parent.mkdir(parents=True, exist_ok=True)
    sh(["git", "worktree", "prune"], cwd=subject)
    sh(["git", "worktree", "add", "--detach", "-q", str(dest), "HEAD"], cwd=subject)
    for name in INSTALL_ARTIFACTS:
        src = subject / name
        if src.is_dir():
            shutil.copytree(src, dest / name)
        elif src.exists():
            shutil.copy2(src, dest / name)
    return dest


def drop_worktree(subject: Path, dest: Path) -> None:
    try:
        sh(["git", "worktree", "remove", "--force", str(dest)], cwd=subject)
    except Exception:                                   # noqa: BLE001
        shutil.rmtree(dest, ignore_errors=True)
        subprocess.run(["git", "worktree", "prune"], cwd=subject,
                       capture_output=True)


VERIFY_TIMEOUT_S = 900


def run_verify(task: dict, cwd: Path, log: Path) -> int | None:
    """The task's own test command, after the agent has exited. `None` on a
    timeout, which grades the same as a failure."""
    cmd = task["verify"]["cmd"]
    try:
        p = subprocess.run(cmd, cwd=cwd, env=child_env(task), capture_output=True,
                           text=True, timeout=VERIFY_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        log.write_text(f"$ {' '.join(cmd)}\nTIMEOUT after {VERIFY_TIMEOUT_S}s\n")
        return None
    log.write_text(f"$ {' '.join(cmd)}\nrc={p.returncode}\n\n{p.stdout}\n{p.stderr}")
    return p.returncode


def run_cell(task: dict, arm: str, rep: int, outdir: Path) -> dict:
    root = subject_root(arm, task)
    stem = f"task{task['id']:02d}_rep{rep}_arm{arm}"
    worktree = None
    if task.get("verify"):
        worktree = make_worktree(root, CELLS / stem)
        cwd_override = worktree
    else:
        cwd_override = root
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
        drop_worktree(root, worktree)

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
        "answer": answer,
        "answer_source": answer_source,
        "permission_denials": len((res or {}).get("permission_denials") or []),
        "result_subtype": (res or {}).get("subtype"),
        "stream_file": str(stream_path),
    }


# --------------------------------------------------------------------------
# statistics and the pre-registered gate
# --------------------------------------------------------------------------


def bootstrap_ci(xs: list[float], n: int = 2000, seed: int = 0) -> tuple[float, float]:
    """95% percentile bootstrap interval for the mean of `xs`.

    Seeded, so a summary regenerated from the same cells is byte-identical.
    """
    if not xs:
        return (0.0, 0.0)
    rng = random.Random(seed)
    means = sorted(statistics.fmean(rng.choices(xs, k=len(xs))) for _ in range(n))
    return (means[int(0.025 * n)], means[int(0.975 * n) - 1])


GATE_ADOPTION = 0.80


def gate_verdict(rows: list[dict]) -> dict:
    """The spec's §1 gate, evaluated over every cell of a run.

    A graph arm passes when it is at least as correct as stock, costs no more,
    reaches the graph in 80% of its sessions, and never runs out of turns on a
    task stock finished. The best passing arm is the one that passes; among
    several, the most correct, then the cheapest.
    """
    by_arm: dict[str, list[dict]] = {}
    for r in rows:
        by_arm.setdefault(r["arm"], []).append(r)
    stock = by_arm.get("A", [])
    if not stock:
        return {"passed": False, "best_arm": None, "reasons": ["no stock arm"]}

    def mean(rs, k):
        return statistics.fmean(r[k] for r in rs) if rs else 0.0

    stock_ok = {r["task"] for r in stock if r["result_subtype"] == "success"}
    verdicts = []
    for arm, rs in sorted(by_arm.items()):
        if arm == "A":
            continue
        reasons = []
        if mean(rs, "score") < mean(stock, "score"):
            reasons.append(f"{arm}: correctness {mean(rs,'score'):.3f} < stock "
                           f"{mean(stock,'score'):.3f}")
        if mean(rs, "cost_usd") > mean(stock, "cost_usd"):
            reasons.append(f"{arm}: cost {mean(rs,'cost_usd'):.4f} > stock "
                           f"{mean(stock,'cost_usd'):.4f}")
        adoption = sum(1 for r in rs if r["adopted"]) / len(rs)
        if adoption < GATE_ADOPTION:
            reasons.append(f"{arm}: adoption {adoption:.0%} < {GATE_ADOPTION:.0%}")
        turned = [r["task"] for r in rs
                  if r["result_subtype"] == "error_max_turns" and r["task"] in stock_ok]
        if turned:
            reasons.append(f"{arm}: max-turns on tasks {sorted(set(turned))} "
                           "where stock succeeded")
        verdicts.append((arm, reasons, mean(rs, "score"), -mean(rs, "cost_usd")))
    passing = [v for v in verdicts if not v[1]]
    if passing:
        best = max(passing, key=lambda v: (v[2], v[3]))
        return {"passed": True, "best_arm": best[0], "reasons": []}
    best = max(verdicts, key=lambda v: (v[2], v[3])) if verdicts else \
        (None, ["no graph arm"], 0.0, 0.0)
    return {"passed": False, "best_arm": best[0], "reasons": best[1]}


def paired_deltas(rows: list[dict], arm: str, key: str) -> list[float]:
    """One difference per task: this arm's mean minus stock's mean.

    Pairing by task is what makes the interval narrow enough to read: task
    difficulty is the largest source of variance in a cell's numbers.
    """
    out = []
    for t in sorted({r["task"] for r in rows}):
        a = [r[key] for r in rows
             if r["task"] == t and r["arm"] == "A" and r.get(key) is not None]
        b = [r[key] for r in rows
             if r["task"] == t and r["arm"] == arm and r.get(key) is not None]
        if a and b:
            out.append(statistics.fmean(b) - statistics.fmean(a))
    return out


# --------------------------------------------------------------------------
# reporting
# --------------------------------------------------------------------------


def fmt(v, nd=0):
    if v is None:
        return "-"
    if isinstance(v, float):
        return f"{v:.{nd}f}"
    return str(v)


def write_summary(outdir: Path, rows: list[dict], meta: dict,
                  filename: str = "summary.md") -> Path:
    arms = sorted({r["arm"] for r in rows})
    lines: list[str] = []
    lines.append(f"# Agent benchmark run{meta.get('title_suffix', '')}")
    lines.append("")
    if meta.get("note"):
        lines.append(meta["note"])
        lines.append("")
    lines.append(f"- run: `{outdir.name}`")
    lines.append(f"- subject HEAD: `{meta['head_short']}`")
    lines.append(f"- model: sonnet, max-turns {meta.get('max_turns', DEFAULT_MAX_TURNS)}, "
                 f"cell timeout {CELL_TIMEOUT_S}s")
    lines.append(f"- arm A (stock): `{SUBJECT_A}`, no MCP, no project skill")
    lines.append(f"- arm B (installed): `{SUBJECT_B}`, mushroomdb MCP + project "
                 "skill + UserPromptSubmit nudge, plain prompt")
    lines.append("- arm C (invoked): same clone and config as B, prompt prefixed "
                 "with `/mushroom `")
    lines.append(f"- R2 subject: {TRUTH_R2.R2['url']} at tag "
                 f"{TRUTH_R2.R2['tag']} (`{TRUTH_R2.R2['sha'][:12]}`), cloned per arm")
    lines.append(f"- allowed tools, every arm: `{','.join(BASE_TOOLS)}`"
                 f" (+ `{MCP_TOOL}` for B and C)")
    lines.append("- DEVIATION from the original design: `Bash` is unqualified "
                 "rather than the per-command `Bash(git:*)`, `Bash(rg:*)`, ... "
                 "list. That list denies every pipeline, which made the "
                 "co-change and most-imported tasks unanswerable in all arms. "
                 "All arms carry the same deviation.")
    lines.append("")

    # ---- the gate --------------------------------------------------------
    verdict = gate_verdict(rows)
    lines.append("## Gate")
    lines.append("")
    lines.append("The pre-registered §1 gate: correctness >= stock, cost <= stock, "
                 f"adoption >= {GATE_ADOPTION:.0%}, and no max-turns failure on a "
                 "task stock finished.")
    lines.append("")
    lines.append("| | |")
    lines.append("|---|---|")
    lines.append(f"| verdict | **{'PASSED' if verdict['passed'] else 'FAILED'}** |")
    lines.append(f"| best arm | {verdict['best_arm'] or '-'} |")
    lines.append("")
    if verdict["reasons"]:
        lines.append("Why it failed:")
        lines.append("")
        for reason in verdict["reasons"]:
            lines.append(f"- {reason}")
        lines.append("")

    lines.append("## Per cell")
    lines.append("")
    hdr = ("| task | key | arm | rep | in | out | cache read | cache create |"
           " total tok | cache hit | tools | mcp | graph | tool search | turns |"
           " sec | cost $ | score | extra | denials | verify | outcome |")
    lines.append(hdr)
    lines.append("|" + "---|" * 22)
    for r in sorted(rows, key=lambda r: (r["task"], r["rep"], r["arm"])):
        flag = " TIMEOUT" if r["timed_out"] else ""
        outcome = "timeout" if r["timed_out"] else (r.get("result_subtype") or "?")
        verify = "-" if r.get("verify_rc") is None else (
            "pass" if r["verify_rc"] == 0 else f"fail({r['verify_rc']})")
        if r.get("verify_rc") is None and r.get("kind") == "change":
            verify = "no run"
        lines.append(
            f"| {r['task']}{flag} | {r.get('key') or '-'} | {r['arm']} | {r['rep']} |"
            f" {r['input_tokens']} |"
            f" {r['output_tokens']} | {r['cache_read_tokens']} |"
            f" {r['cache_creation_tokens']} | {r['total_tokens']} |"
            f" {fmt(r.get('cache_hit_ratio'), 3)} |"
            f" {r['tool_calls_total']} | {r['mcp_calls']} |"
            f" {r.get('graph_calls', 0)} | {r.get('tool_search_calls', 0)} |"
            f" {fmt(r['num_turns'])} |"
            f" {r['wall_seconds']} | {fmt(r['cost_usd'], 4)} |"
            f" {r['score']:.2f} | {r['wrong_extra']} |"
            f" {r.get('permission_denials', 0)} | {verify} | {outcome} |"
        )
    lines.append("")

    lines.append("## Aggregate (mean per arm)")
    lines.append("")
    metrics = [
        ("total_tokens", "total tokens", 0),
        ("input_tokens", "input tokens", 0),
        ("output_tokens", "output tokens", 0),
        ("cache_read_tokens", "cache read", 0),
        ("cache_creation_tokens", "cache create", 0),
        ("cache_hit_ratio", "cache hit ratio", 3),
        ("tool_calls_total", "tool calls", 2),
        ("mcp_calls", "mcp calls", 2),
        ("graph_calls", "graph calls", 2),
        ("tool_search_calls", "tool search calls", 2),
        ("adopted", "adoption", 2),
        ("num_turns", "turns", 2),
        ("wall_seconds", "seconds", 1),
        ("cost_usd", "cost $", 4),
        ("score", "score", 3),
        ("wrong_extra", "wrong extra", 2),
    ]
    by_arm = {a: [r for r in rows if r["arm"] == a] for a in arms}

    def mean(rs, key):
        vals = [r[key] for r in rs if r.get(key) is not None]
        return statistics.fmean(vals) if vals else None

    def pct(base, val):
        if base is None or val is None:
            return "-"
        if base == 0:
            return "n/a"
        return f"{(val - base) / base * 100:+.1f}%"

    head = "| metric |" + "".join(
        f" arm {a} ({ARM_LABEL.get(a, a)}) |" for a in arms)
    deltas = [a for a in arms if a != "A"]
    head += "".join(f" {a} vs A |" for a in deltas)
    lines.append(head)
    lines.append("|" + "---|" * (1 + len(arms) + len(deltas)))
    for key, label, nd in metrics:
        vals = {a: mean(by_arm[a], key) for a in arms}
        row = f"| {label} |" + "".join(f" {fmt(vals[a], nd)} |" for a in arms)
        row += "".join(f" {pct(vals.get('A'), vals[a])} |" for a in deltas)
        lines.append(row)
    lines.append("")
    lines.append("; ".join(f"arm {a}: {len(by_arm[a])} cells" for a in arms)
                 + f"; timeouts {sum(1 for r in rows if r['timed_out'])}"
                 + f"; errors {sum(1 for r in rows if not r['ok'])}")
    lines.append("")

    # ---- paired deltas with bootstrap intervals ---------------------------
    lines.append("## Deltas vs stock")
    lines.append("")
    lines.append("Paired by task: one difference per task, arm mean minus arm A "
                 "mean. The interval is a 95% percentile bootstrap over those "
                 "differences (2000 resamples, seeded). An interval that does not "
                 "contain 0 is the only kind worth quoting.")
    lines.append("")
    delta_metrics = [
        ("score", "score", 3),
        ("cost_usd", "cost $", 4),
        ("total_tokens", "total tokens", 0),
        ("num_turns", "turns", 2),
        ("cache_hit_ratio", "cache hit ratio", 3),
    ]
    lines.append("| arm | metric | arm mean | stock mean | delta | 95% CI |")
    lines.append("|" + "---|" * 6)
    for a in deltas:
        for key, label, nd in delta_metrics:
            diffs = paired_deltas(rows, a, key)
            lo, hi = bootstrap_ci(diffs)
            delta = statistics.fmean(diffs) if diffs else None
            lines.append(
                f"| {a} | {label} | {fmt(mean(by_arm[a], key), nd)} |"
                f" {fmt(mean(by_arm.get('A', []), key), nd)} |"
                f" {fmt(delta, nd + 1)} | [{lo:.{nd + 1}f}, {hi:.{nd + 1}f}] |")
    lines.append("")

    # ---- per-task correctness -------------------------------------------
    lines.append("## Correctness by task (mean score over reps)")
    lines.append("")
    tasks_seen = sorted({r["task"] for r in rows})
    lines.append("| task |" + "".join(f" arm {a} |" for a in arms) + " best |")
    lines.append("|" + "---|" * (2 + len(arms)))
    for t in tasks_seen:
        vals = {}
        for a in arms:
            rs = [r for r in rows if r["task"] == t and r["arm"] == a]
            vals[a] = statistics.fmean([r["score"] for r in rs]) if rs else None
        present = {a: v for a, v in vals.items() if v is not None}
        best = max(present, key=lambda a: present[a]) if present else "-"
        if present and len({round(v, 4) for v in present.values()}) == 1:
            best = "tie"
        lines.append(
            f"| {t} |" + "".join(
                f" {vals[a]:.2f} |" if vals[a] is not None else " - |"
                for a in arms)
            + f" {best} |")
    means = {a: mean(by_arm[a], "score") for a in arms}
    lines.append("| **mean** |" + "".join(f" **{fmt(means[a], 3)}** |"
                                          for a in arms) + " |")
    lines.append("")

    # ---- tool adoption ---------------------------------------------------
    lines.append("## Tool adoption")
    lines.append("")
    lines.append("Sessions that made at least one MCP call, and which "
                 "mushroomdb tools they used.")
    lines.append("")
    lines.append("| arm | sessions | >=1 mcp call | adoption | mcp calls total |")
    lines.append("|---|---|---|---|---|")
    for a in arms:
        rs = by_arm[a]
        used = [r for r in rs if r["mcp_calls"] > 0]
        share = f"{len(used) / len(rs) * 100:.0f}%" if rs else "-"
        lines.append(f"| {a} | {len(rs)} | {len(used)} | {share} |"
                     f" {sum(r['mcp_calls'] for r in rs)} |")
    lines.append("")
    for a in arms:
        counts: dict[str, int] = {}
        for r in by_arm[a]:
            for name, n in r["tool_calls"].items():
                if name.startswith("mcp__"):
                    counts[name] = counts.get(name, 0) + n
        if not counts:
            lines.append(f"- arm {a}: no MCP tools used")
            continue
        listed = ", ".join(
            f"`{k.replace('mcp__mushroomdb__', '')}` x{v}"
            for k, v in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0])))
        lines.append(f"- arm {a}: {listed}")
    lines.append("")
    lines.append("Tasks where each arm reached for the graph:")
    lines.append("")
    for a in arms:
        hit = sorted({r["task"] for r in by_arm[a] if r["mcp_calls"] > 0})
        lines.append(f"- arm {a}: {hit if hit else 'none'}")
    lines.append("")

    lines.append("## Answers")
    lines.append("")
    for r in sorted(rows, key=lambda r: (r["task"], r["rep"], r["arm"])):
        lines.append(f"### task {r['task']} rep {r['rep']} arm {r['arm']} "
                     f"(score {r['score']:.2f})")
        lines.append("")
        lines.append("```")
        lines.append((r["answer"] or "(no answer)").strip())
        lines.append("```")
        if r["missed"]:
            lines.append(f"missed: {', '.join(r['missed'])}")
        lines.append("")

    path = outdir / filename
    path.write_text("\n".join(lines) + "\n")
    return path


# --------------------------------------------------------------------------
# the pilot: does a stock agent have to work for this answer?
# --------------------------------------------------------------------------

PILOT_MIN_TURNS = 6


def pilot_turns(row: dict, cap: int) -> int:
    """What stock needed for this task.

    A cell that ran out of turns or out of time reports no `num_turns`; it
    spent everything it had, which is the opposite of the "too easy" case the
    floor drops, so it counts as the cap.
    """
    return row["num_turns"] or cap


def apply_pilot(data: dict, rows: list[dict], cap: int,
                run_name: str) -> tuple[dict, dict[int, int], list[tuple[str, int]]]:
    """Stamp `min_stock_turns` on the tasks a stock agent had to work for, and
    drop the ones it answered in fewer than six turns (§3.2)."""
    turns = {r["task"]: pilot_turns(r, cap) for r in rows}
    kept, dropped = [], []
    for t in data["tasks"]:
        if t["id"] not in turns:
            kept.append(t)                       # not piloted this round
        elif turns[t["id"]] >= PILOT_MIN_TURNS:
            kept.append({**t, "min_stock_turns": turns[t["id"]]})
        else:
            dropped.append((t["key"], turns[t["id"]]))
    data = {**data, "tasks": kept,
            "pilot": {"run": run_name, "min_turns": PILOT_MIN_TURNS,
                      "dropped": [{"key": k, "turns": n} for k, n in dropped]}}
    return data, turns, dropped


def pilot(max_turns: int, only: list[int] | None = None) -> int:
    """One stock (arm A) session per task, to size the tasks.

    §3.2's rule: a task a stock Sonnet agent finishes in fewer than six turns
    cannot show a graph win, so it leaves the set. Surviving tasks record what
    stock needed as `min_stock_turns`.
    """
    tasks_path = HERE / "tasks.json"
    data = json.loads(tasks_path.read_text())
    tasks = [t for t in data["tasks"] if only is None or t["id"] in only]
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

    arms = [x.strip() for x in a.arms.split(",")]
    setup(force=a.force_setup, arms={"A"} if a.pilot else set(arms))
    if a.setup_only:
        return 0
    if a.pilot:
        only = None if a.tasks == "all" else [int(x) for x in a.tasks.split(",")]
        return pilot(a.max_turns, only)

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
