#!/usr/bin/env python3
"""What a run's cells add up to: the gate, the intervals, and `summary.md`.

Reads rows — from a live run (`run.py`) or replayed from saved streams
(`rescore.py`) — and never runs anything itself.

Missing-value policy, applied everywhere in this module: a cell that timed out
or died carries `None` for the numbers the `result` event would have supplied
(cost, turns, duration). Such a cell is excluded from the mean of that metric
and from its paired difference; it is never read as a zero, which would make a
crashed arm look cheap. Every section that drops cells says how many it
dropped, and the cells themselves stay in the per-cell table and the counts.
"""

from __future__ import annotations

import random
import statistics
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import truth_r2 as TRUTH_R2                                      # noqa: E402
from ground_truth import unit_passed                             # noqa: E402
from subjects import (ARM_LABEL, BASE_TOOLS, CELL_TIMEOUT_S,     # noqa: E402
                      DEFAULT_MAX_TURNS, MCP_TOOL, SUBJECT_A, SUBJECT_B)


def fmt(v, nd=0):
    if v is None:
        return "-"
    if isinstance(v, float):
        return f"{v:.{nd}f}"
    return str(v)


def values(rows: list[dict], key: str) -> list[float]:
    """The rows that actually have this number. See the module's policy."""
    return [r[key] for r in rows if r.get(key) is not None]


def mean_of(rows: list[dict], key: str) -> float | None:
    vals = values(rows, key)
    return statistics.fmean(vals) if vals else None


def missing(rows: list[dict], key: str) -> int:
    return sum(1 for r in rows if r.get(key) is None)


def bootstrap_ci(xs: list[float], n: int = 2000, seed: int = 0) -> tuple[float, float]:
    """95% percentile bootstrap interval for the mean of `xs`.

    Seeded, so a summary regenerated from the same cells is byte-identical.
    """
    if not xs:
        return (0.0, 0.0)
    rng = random.Random(seed)
    means = sorted(statistics.fmean(rng.choices(xs, k=len(xs))) for _ in range(n))
    return (means[int(0.025 * n)], means[int(0.975 * n) - 1])


def paired_deltas(rows: list[dict], arm: str, key: str) -> list[float]:
    """One difference per task: this arm's mean minus stock's mean.

    Pairing by task is what makes the interval narrow enough to read: task
    difficulty is the largest source of variance in a cell's numbers. A task
    where either side has no usable value contributes no difference.
    """
    out = []
    for t in sorted({r["task"] for r in rows}):
        a = values([r for r in rows if r["task"] == t and r["arm"] == "A"], key)
        b = values([r for r in rows if r["task"] == t and r["arm"] == arm], key)
        if a and b:
            out.append(statistics.fmean(b) - statistics.fmean(a))
    return out


GATE_ADOPTION = 0.80


def gate_verdict(rows: list[dict]) -> dict:
    """The spec's §1 gate, evaluated over every cell of a run.

    A graph arm passes when it is at least as correct as stock **paired by
    task** (§1.1 is a paired statistic, so a run that skipped a task in one arm
    cannot flatter the other), costs no more per cell, reaches the graph in 80%
    of its sessions, and never runs out of turns on a task stock finished. The
    best passing arm is the one that passes; among several, the most correct,
    then the cheapest.
    """
    by_arm: dict[str, list[dict]] = {}
    for r in rows:
        by_arm.setdefault(r["arm"], []).append(r)
    stock = by_arm.get("A", [])
    if not stock:
        return {"passed": False, "best_arm": None, "reasons": ["no stock arm"]}

    stock_ok = {r["task"] for r in stock if r["result_subtype"] == "success"}
    stock_cost = mean_of(stock, "cost_usd")
    verdicts = []
    for arm, rs in sorted(by_arm.items()):
        if arm == "A":
            continue
        reasons = []
        diffs = paired_deltas(rows, arm, "score")
        paired = statistics.fmean(diffs) if diffs else 0.0
        if paired < 0:
            reasons.append(f"{arm}: correctness {paired:+.3f} vs stock, paired "
                           f"over {len(diffs)} task(s)")
        cost = mean_of(rs, "cost_usd")
        if cost is not None and stock_cost is not None and cost > stock_cost:
            reasons.append(f"{arm}: cost {cost:.4f} > stock {stock_cost:.4f}")
        adoption = sum(1 for r in rs if r["adopted"]) / len(rs)
        if adoption < GATE_ADOPTION:
            reasons.append(f"{arm}: adoption {adoption:.0%} < {GATE_ADOPTION:.0%}")
        turned = [r["task"] for r in rs
                  if r["result_subtype"] == "error_max_turns" and r["task"] in stock_ok]
        if turned:
            reasons.append(f"{arm}: max-turns on tasks {sorted(set(turned))} "
                           "where stock succeeded")
        verdicts.append((arm, reasons, paired, -(cost if cost is not None else 0.0)))
    passing = [v for v in verdicts if not v[1]]
    if passing:
        best = max(passing, key=lambda v: (v[2], v[3]))
        return {"passed": True, "best_arm": best[0], "reasons": []}
    best = max(verdicts, key=lambda v: (v[2], v[3])) if verdicts else \
        (None, ["no graph arm"], 0.0, 0.0)
    return {"passed": False, "best_arm": best[0], "reasons": best[1]}


def write_summary(outdir: Path, rows: list[dict], meta: dict,
                  filename: str = "summary.md") -> Path:
    arms = sorted({r["arm"] for r in rows})
    by_arm = {a: [r for r in rows if r["arm"] == a] for a in arms}
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
    lines.append("- a cell that timed out or errored carries no cost, turns or "
                 "duration; it is excluded from those means and from their "
                 "paired differences, never counted as zero. Each section says "
                 "how many cells it dropped.")
    lines.append("")

    # ---- the gate --------------------------------------------------------
    verdict = gate_verdict(rows)
    lines.append("## Gate")
    lines.append("")
    lines.append("The pre-registered §1 gate: correctness >= stock (paired by "
                 f"task), cost <= stock, adoption >= {GATE_ADOPTION:.0%}, and no "
                 "max-turns failure on a task stock finished.")
    lines.append("")
    lines.append("| | |")
    lines.append("|---|---|")
    lines.append(f"| verdict | **{'PASSED' if verdict['passed'] else 'FAILED'}** |")
    lines.append(f"| best arm | {verdict['best_arm'] or '-'} |")
    no_cost = missing(rows, "cost_usd")
    lines.append(f"| cells with no cost recorded | {no_cost} of {len(rows)} |")
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
           " sec | cost $ | score | extra | denials | verify | dirtied | outcome |")
    lines.append(hdr)
    lines.append("|" + "---|" * 23)
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
            f" {r.get('permission_denials', 0)} | {verify} |"
            f" {'yes' if r.get('dirtied') else '-'} | {outcome} |"
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
        vals = {a: mean_of(by_arm[a], key) for a in arms}
        row = f"| {label} |" + "".join(f" {fmt(vals[a], nd)} |" for a in arms)
        row += "".join(f" {pct(vals.get('A'), vals[a])} |" for a in deltas)
        lines.append(row)
    lines.append("")
    lines.append("; ".join(f"arm {a}: {len(by_arm[a])} cells" for a in arms)
                 + f"; timeouts {sum(1 for r in rows if r['timed_out'])}"
                 + f"; errors {sum(1 for r in rows if not r['ok'])}"
                 + f"; cells with no cost {no_cost}")
    lines.append("")

    # ---- paired deltas with bootstrap intervals ---------------------------
    lines.append("## Deltas vs stock")
    lines.append("")
    lines.append("Paired by task: one difference per task, arm mean minus arm A "
                 "mean. The interval is a 95% percentile bootstrap over those "
                 "differences (2000 resamples, seeded). An interval that does not "
                 "contain 0 is the only kind worth quoting. `tasks` is how many "
                 "tasks contributed a difference — a task where either side "
                 "recorded nothing contributes none.")
    lines.append("")
    delta_metrics = [
        ("score", "score", 3),
        ("cost_usd", "cost $", 4),
        ("total_tokens", "total tokens", 0),
        ("num_turns", "turns", 2),
        ("cache_hit_ratio", "cache hit ratio", 3),
    ]
    lines.append("| arm | metric | arm mean | stock mean | delta | 95% CI | tasks |")
    lines.append("|" + "---|" * 7)
    for a in deltas:
        for key, label, nd in delta_metrics:
            diffs = paired_deltas(rows, a, key)
            lo, hi = bootstrap_ci(diffs)
            delta = statistics.fmean(diffs) if diffs else None
            lines.append(
                f"| {a} | {label} | {fmt(mean_of(by_arm[a], key), nd)} |"
                f" {fmt(mean_of(by_arm.get('A', []), key), nd)} |"
                f" {fmt(delta, nd + 1)} | [{lo:.{nd + 1}f}, {hi:.{nd + 1}f}] |"
                f" {len(diffs)} |")
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
    means = {a: mean_of(by_arm[a], "score") for a in arms}
    lines.append("| **mean** |" + "".join(f" **{fmt(means[a], 3)}** |"
                                          for a in arms) + " |")
    lines.append("")

    # ---- every cell that did not score 1.0 (spec §3.4) -------------------
    lines.append("## Sub-1.0 cells")
    lines.append("")
    lines.append("One line per cell that did not score 1.0. `classification` is "
                 "filled in by hand after reading the cell's stream file: "
                 "`tool` (the graph answered badly or not at all), `model` (the "
                 "agent had what it needed and went wrong anyway), or `grader` "
                 "(the answer is right and the check is wrong).")
    lines.append("")
    lines.append("| task | key | arm | rep | score | missed | verify | outcome |"
                 " classification |")
    lines.append("|" + "---|" * 9)
    subs = [r for r in sorted(rows, key=lambda r: (r["score"], r["task"], r["arm"]))
            if r["score"] < 1.0]
    for r in subs:
        outcome = "timeout" if r["timed_out"] else (r.get("result_subtype") or "?")
        verify = "-" if r.get("verify_rc") is None else (
            "pass" if r["verify_rc"] == 0 else f"fail({r['verify_rc']})")
        missed = ", ".join(r.get("missed") or []) or "-"
        if len(missed) > 120:
            missed = missed[:117] + "..."
        lines.append(
            f"| {r['task']} | {r.get('key') or '-'} | {r['arm']} | {r['rep']} |"
            f" {r['score']:.2f} | {missed} | {verify} | {outcome} |  |")
    if not subs:
        lines.append("| - | - | - | - | - | every cell scored 1.0 | - | - |  |")
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


__all__ = ["bootstrap_ci", "fmt", "gate_verdict", "mean_of", "missing",
           "paired_deltas", "unit_passed", "values", "write_summary",
           "GATE_ADOPTION"]
