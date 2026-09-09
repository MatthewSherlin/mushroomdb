#!/usr/bin/env python3
"""Re-grade a completed run's stream files. No sessions are re-run.

Answers, tool calls and usage come back out of the saved stream-json;
timing comes from the original cells.json. Only the grader changes.

Writes `cells-corrected.json` and `summary-corrected.md` beside the
originals, which are left untouched.

Usage: python3 rescore.py --run results/<timestamp> [--tasks tasks.json]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from ground_truth import grade, unit_passed  # noqa: E402
from report import write_summary  # noqa: E402
from run import parse_stream, usage_of  # noqa: E402

NAME_RE = re.compile(r"task(\d+)_rep(\d+)_arm([A-Z])\.stream\.jsonl$")


def rescore(run_dir: Path, tasks_path: Path) -> tuple[list[dict], list[dict]]:
    data = json.loads(tasks_path.read_text())
    tasks = {t["id"]: t for t in data["tasks"]}

    original = {}
    old_path = run_dir / "cells.json"
    if old_path.exists():
        for r in json.loads(old_path.read_text()):
            original[(r["task"], r["arm"], r["rep"])] = r

    rows: list[dict] = []
    for stream in sorted(run_dir.glob("task*_rep*_arm*.stream.jsonl")):
        m = NAME_RE.search(stream.name)
        if not m:
            continue
        tid, rep, arm = int(m.group(1)), int(m.group(2)), m.group(3)
        task = tasks.get(tid)
        if task is None:
            print(f"  skip {stream.name}: task {tid} not in {tasks_path.name}")
            continue

        parsed = parse_stream(stream)
        res = parsed["result"]
        answer = (res or {}).get("result") or ""
        answer_source = "result"
        if not answer:
            answer = parsed["last_assistant_text"]
            answer_source = "last_assistant_text" if answer else "none"

        tools = parsed["tool_calls"]
        prev = original.get((tid, arm, rep), {})
        # A change task cannot be re-graded from a stream: its score is the
        # diff and the test run, neither of which is replayable. Keep what the
        # live run recorded.
        graded = (grade(task, answer) if task.get("kind") != "change"
                  else {"score": prev.get("score", 0.0), "units": [],
                        "wrong_extra": prev.get("wrong_extra", 0)})
        rows.append({
            "task": tid,
            "arm": arm,
            "rep": rep,
            "ok": prev.get("ok", res is not None),
            "timed_out": prev.get("timed_out", False),
            "returncode": prev.get("returncode"),
            "wall_seconds": prev.get("wall_seconds", 0.0),
            "duration_ms": (res or {}).get("duration_ms"),
            "num_turns": (res or {}).get("num_turns"),
            "cost_usd": (res or {}).get("total_cost_usd"),
            "is_error": (res or {}).get("is_error"),
            **usage_of(res),
            "tool_calls_total": sum(tools.values()),
            "mcp_calls": sum(v for k, v in tools.items() if k.startswith("mcp__")),
            "tool_calls": tools,
            "tool_search_calls": parsed["tool_search_calls"],
            "graph_calls": parsed["graph_calls"],
            "adopted": parsed["graph_calls"] >= 1,
            "verify_rc": prev.get("verify_rc"),
            "kind": task.get("kind"),
            "key": task.get("key"),
            "score": graded["score"],
            "wrong_extra": graded["wrong_extra"],
            "missed": [u["fact"] for u in graded["units"] if not unit_passed(u)],
            "matched_variants": [
                {"fact": u["fact"], "used": u["matched"]}
                for u in graded["units"] if u.get("matched")
            ],
            "answer": answer,
            "answer_source": answer_source,
            "permission_denials": len((res or {}).get("permission_denials") or []),
            "result_subtype": (res or {}).get("subtype"),
            "score_before": prev.get("score"),
            "stream_file": str(stream),
        })

    changed = [
        r for r in rows
        if r["score_before"] is not None
        and abs(r["score"] - r["score_before"]) > 1e-9
    ]
    return rows, changed


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", required=True)
    ap.add_argument("--tasks", default=str(HERE / "tasks.json"))
    a = ap.parse_args()

    run_dir = Path(a.run).resolve()
    rows, changed = rescore(run_dir, Path(a.tasks).resolve())
    print(f"re-scored {len(rows)} cells from {run_dir}")

    (run_dir / "cells-corrected.json").write_text(json.dumps(rows, indent=2) + "\n")

    variants: dict[str, set[str]] = {}
    for r in rows:
        for mv in r["matched_variants"]:
            if mv["fact"].startswith("author<"):
                variants.setdefault(mv["fact"], set()).add(mv["used"])

    note_lines = [
        "**Re-scored with the corrected grader. Sessions were not re-run; "
        "the same 60 stream files were re-graded.** The original "
        "`summary.md` and `cells.json` are kept beside this file.",
        "",
        "Two grader fixes:",
        "",
        "1. **SHA prefix matching.** A commit cited as any unambiguous hex "
        f"prefix of at least 6 characters now matches, in either direction. "
        "The old grader required the exact 7-character short SHA as a "
        "substring, so a correct answer written as `9cfdbf` scored zero.",
        "2. **Author name variants.** An author fact is now the person behind "
        "an email, and any git author name recorded for that email is "
        "accepted. The variant the answer actually used is recorded per cell "
        "in `cells-corrected.json` under `matched_variants`.",
        "",
    ]
    if variants:
        note_lines.append("Author name variants accepted in this run:")
        note_lines.append("")
        for fact, used in sorted(variants.items()):
            note_lines.append(f"- `{fact}`: {', '.join(sorted(used))}")
        note_lines.append("")
    if changed:
        note_lines.append(f"Cells whose score changed ({len(changed)}):")
        note_lines.append("")
        note_lines.append("| cell | before | after |")
        note_lines.append("|---|---|---|")
        for r in sorted(changed, key=lambda r: (r["arm"], r["task"], r["rep"])):
            note_lines.append(
                f"| arm {r['arm']} task {r['task']} rep {r['rep']} |"
                f" {r['score_before']:.2f} | {r['score']:.2f} |")
    else:
        note_lines.append("No cell changed score.")

    head = json.loads(Path(a.tasks).read_text())["head_short"]
    path = write_summary(
        run_dir, rows,
        {"head_short": head,
         "title_suffix": " (corrected grader)",
         "note": "\n".join(note_lines)},
        filename="summary-corrected.md",
    )
    print(f"corrected summary: {path}")
    for r in sorted(changed, key=lambda r: (r["arm"], r["task"], r["rep"])):
        print(f"  arm {r['arm']} task {r['task']} rep {r['rep']}: "
              f"{r['score_before']:.2f} -> {r['score']:.2f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
