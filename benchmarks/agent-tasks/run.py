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


def setup(force: bool = False, arms: set[str] | None = None) -> None:
    if arms is None:
        arms = {"A", "B", "C", "D"}
    ensure_binary()
    SCRATCH.mkdir(parents=True, exist_ok=True)
    EMPTY_MCP.write_text('{"mcpServers":{}}\n')

    if force:
        dirs = [SUBJECT_A, SUBJECT_B]
        if "D" in arms:
            dirs.append(SUBJECT_D)
        for d in dirs:
            shutil.rmtree(d, ignore_errors=True)

    if not SUBJECT_A.exists():
        print(f"cloning arm A subject -> {SUBJECT_A}")
        sh(["git", "clone", "-q", str(REPO), str(SUBJECT_A)])
    # arm A must stay clean: no .mcp.json, no .claude/
    for stray in (SUBJECT_A / ".mcp.json", SUBJECT_A / ".claude"):
        if stray.exists():
            raise SystemExit(f"arm A clone is contaminated: {stray}")

    if not SUBJECT_B.exists():
        print(f"cloning arm B subject -> {SUBJECT_B}")
        sh(["git", "clone", "-q", str(REPO), str(SUBJECT_B)])
    if not (SUBJECT_B / ".mcp.json").exists():
        print("installing mushroomdb into arm B clone")
        print(sh([
            str(MUSHROOMDB), "install", "--project",
            "--platform", "claude-code",
            "--command", str(MUSHROOMDB),
            "--no-prewarm", "--no-git-hooks",
        ], cwd=SUBJECT_B))
    # The install dirties the working tree (.mcp.json, .claude/, .gitignore).
    # A dirty tree makes the UserPromptSubmit recall hook report the install
    # artifacts instead of the question, so hide them the way a real project
    # would have them committed or ignored -- without adding a commit, which
    # would shift the "last N commits" windows the ground truth depends on.
    exclude = SUBJECT_B / ".git" / "info" / "exclude"
    marker = "mushroom-memory/"
    if marker not in exclude.read_text():
        sh(["git", "checkout", "--", ".gitignore"], cwd=SUBJECT_B)
        with exclude.open("a") as fh:
            fh.write("\n.mcp.json\n.claude/\nmushroom-memory/\n")
    dirty = sh(["git", "status", "--porcelain"], cwd=SUBJECT_B).strip()
    if dirty:
        raise SystemExit(f"arm B clone is not clean after install:\n{dirty}")

    if not (SUBJECT_B / "mushroom-memory").exists():
        print("ingesting arm B store")
        print(sh([str(MUSHROOMDB), "ingest-git", "./mushroom-memory", "."],
                 cwd=SUBJECT_B))

    # Arm D: mushroomdb installed via `--delivery cli` (no MCP server). That
    # flag doesn't exist on the binary yet (it lands in a later task), so
    # this whole block only runs when D is actually requested.
    if "D" in arms:
        if not SUBJECT_D.exists():
            print(f"cloning arm D subject -> {SUBJECT_D}")
            sh(["git", "clone", "-q", str(REPO), str(SUBJECT_D)])
        if not (SUBJECT_D / ".claude").exists():
            print("installing mushroomdb (cli delivery) into arm D clone")
            print(sh([
                str(MUSHROOMDB), "install", "--project",
                "--platform", "claude-code",
                "--command", str(MUSHROOMDB),
                "--no-prewarm", "--no-git-hooks",
                "--delivery", "cli",
            ], cwd=SUBJECT_D))
        exclude_d = SUBJECT_D / ".git" / "info" / "exclude"
        if marker not in exclude_d.read_text():
            sh(["git", "checkout", "--", ".gitignore"], cwd=SUBJECT_D)
            with exclude_d.open("a") as fh:
                fh.write("\n.mcp.json\n.claude/\nmushroom-memory/\n")
        dirty_d = sh(["git", "status", "--porcelain"], cwd=SUBJECT_D).strip()
        if dirty_d:
            raise SystemExit(f"arm D clone is not clean after install:\n{dirty_d}")
        if not (SUBJECT_D / "mushroom-memory").exists():
            print("ingesting arm D store")
            print(sh([str(MUSHROOMDB), "ingest-git", "./mushroom-memory", "."],
                     cwd=SUBJECT_D))

    print("regenerating ground truth from arm A clone")
    sh([sys.executable, str(HERE / "ground_truth.py"),
        "--repo", str(SUBJECT_A), "--out", str(HERE / "tasks.json")])
    print("setup complete")


# --------------------------------------------------------------------------
# running one cell
# --------------------------------------------------------------------------


def child_env() -> dict[str, str]:
    env = dict(os.environ)
    for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT"):
        env.pop(k, None)
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


def run_cell(task: dict, arm: str, rep: int, outdir: Path) -> dict:
    cmd, cwd = cell_command(
        arm, task["full_prompt"], task.get("max_turns", DEFAULT_MAX_TURNS))
    stream_path = outdir / f"task{task['id']:02d}_rep{rep}_arm{arm}.stream.jsonl"
    err_path = outdir / f"task{task['id']:02d}_rep{rep}_arm{arm}.stderr.txt"
    started = time.time()
    timed_out = False
    rc = None
    with stream_path.open("w") as so, err_path.open("w") as se:
        proc = subprocess.Popen(cmd, cwd=cwd, stdout=so, stderr=se, env=child_env())
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

    sys.path.insert(0, str(HERE))
    from ground_truth import grade  # noqa: E402

    graded = grade(task, answer) if answer else {"score": 0.0, "units": [],
                                                 "wrong_extra": 0}
    return {
        "task": task["id"],
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
        "missed": [u["fact"] for u in graded["units"] if u["got"] < 1.0],
        "answer": answer,
        "answer_source": answer_source,
        "permission_denials": len((res or {}).get("permission_denials") or []),
        "result_subtype": (res or {}).get("subtype"),
        "stream_file": str(stream_path),
    }


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
    lines.append(f"- allowed tools, every arm: `{','.join(BASE_TOOLS)}`"
                 f" (+ `{MCP_TOOL}` for B and C)")
    lines.append("- DEVIATION from the original design: `Bash` is unqualified "
                 "rather than the per-command `Bash(git:*)`, `Bash(rg:*)`, ... "
                 "list. That list denies every pipeline, which made the "
                 "co-change and most-imported tasks unanswerable in all arms. "
                 "All arms carry the same deviation.")
    lines.append("")

    lines.append("## Per cell")
    lines.append("")
    hdr = ("| task | arm | rep | in | out | cache read | cache create | total tok |"
           " tools | mcp | turns | sec | cost $ | score | extra | denials |"
           " outcome |")
    lines.append(hdr)
    lines.append("|" + "---|" * 17)
    for r in sorted(rows, key=lambda r: (r["task"], r["rep"], r["arm"])):
        flag = " TIMEOUT" if r["timed_out"] else ""
        outcome = "timeout" if r["timed_out"] else (r.get("result_subtype") or "?")
        lines.append(
            f"| {r['task']}{flag} | {r['arm']} | {r['rep']} | {r['input_tokens']} |"
            f" {r['output_tokens']} | {r['cache_read_tokens']} |"
            f" {r['cache_creation_tokens']} | {r['total_tokens']} |"
            f" {r['tool_calls_total']} | {r['mcp_calls']} | {fmt(r['num_turns'])} |"
            f" {r['wall_seconds']} | {fmt(r['cost_usd'], 4)} |"
            f" {r['score']:.2f} | {r['wrong_extra']} |"
            f" {r.get('permission_denials', 0)} | {outcome} |"
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
        ("tool_calls_total", "tool calls", 2),
        ("mcp_calls", "mcp calls", 2),
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


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", default="all")
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--arms", default="A,B,C,D")
    ap.add_argument("--max-turns", type=int, default=DEFAULT_MAX_TURNS)
    ap.add_argument("--setup-only", action="store_true")
    ap.add_argument("--force-setup", action="store_true")
    a = ap.parse_args()

    arms = [x.strip() for x in a.arms.split(",")]
    setup(force=a.force_setup, arms=set(arms))
    if a.setup_only:
        return 0

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
