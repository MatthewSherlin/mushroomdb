#!/usr/bin/env python3
"""Re-grade a completed run's stream files. No sessions are re-run.

Answers, tool calls and usage come back out of the saved stream-json;
timing comes from the original cells.json. Only the grader changes.

Writes `cells-corrected.json` and `summary-corrected.md` beside the
originals, which are left untouched.

Usage: python3 rescore.py --run results/<timestamp> [--tasks tasks.json]

`--rerender` is the other, smaller mode: it rewrites `summary.md` in place
from the run's own `cells.json`, with no grading of any kind, so a wording fix
in `report.py` reaches a committed summary without re-running or re-scoring
anything. Every number outside the provenance block is asserted unchanged
first, and the old file is put back if one moved.
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

# ── re-render: the same cells, the same numbers, a newer `report.py` ─────────

TITLE_RE = re.compile(r"^# Agent benchmark run(.*)$", re.M)
HEAD_RE = re.compile(r"^- subject HEAD: `([^`]+)`", re.M)
SUITE_RE = re.compile(r"^- suite: (\S+)$", re.M)
BASELINE_RE = re.compile(r"^- baseline arm: ([A-Z])\b", re.M)
DIGEST_RE = re.compile(r"^- world digest: `([^`]+)`", re.M)
TURNS_RE = re.compile(r"^- model: .*max-turns (\d+)", re.M)
NUM_RE = re.compile(r"\d+(?:\.\d+)?")


def summary_meta(md: str) -> dict:
    """The `write_summary` meta a committed summary was rendered with.

    Read back off the summary itself rather than out of `tasks.json`, which
    has moved on since: a re-render must reproduce the run that happened, not
    the run the current task set would describe.

    The suite and the baseline arm are read back too, and they decide the gate
    variant `write_summary` describes: rendering an `association` summary under
    the `code` defaults would print the wrong gate and compute the wrong
    verdict, which is the one thing a re-render must not do. Whichever of the
    provenance lines a suite carries — `subject HEAD` for `code`, `world
    digest` for `association` — is the one required.
    """
    title = TITLE_RE.search(md)
    turns = TURNS_RE.search(md)
    suite = SUITE_RE.search(md)
    baseline = BASELINE_RE.search(md)
    if not (title and turns):
        raise SystemExit("rerender: summary.md is missing its title or "
                         "max-turns line — cannot reproduce its meta")
    meta: dict = {"max_turns": int(turns.group(1))}
    if suite:
        meta["suite"] = suite.group(1)
    if baseline:
        meta["baseline"] = baseline.group(1)
    if meta.get("suite") == "association":
        digest = DIGEST_RE.search(md)
        if not digest:
            raise SystemExit("rerender: an association summary.md is missing "
                             "its world-digest line")
        meta["world_digest"] = digest.group(1)
        # `graph_arms` is not on the page in words, but "arms under the gate"
        # is: a re-render gates exactly the arms the run gated.
        gated = re.search(r"^\| arms under the gate \| (.+?) \|$", md, re.M)
        if gated and gated.group(1).strip() != "-":
            meta["graph_arms"] = [a.strip() for a in
                                  gated.group(1).split(",")]
    else:
        head = HEAD_RE.search(md)
        if not head:
            raise SystemExit("rerender: summary.md is missing its HEAD line — "
                             "cannot reproduce its meta")
        meta["head_short"] = head.group(1)
    if title.group(1):
        meta["title_suffix"] = title.group(1)
    # Anything between the title and the provenance block is the run's note,
    # carried through verbatim.
    body = md.split("\n", 1)[1]
    note = body.split("\n- run: ", 1)[0].strip("\n")
    if note.strip():
        meta["note"] = note
    return meta


def numbers(md: str) -> list[str]:
    """Every number in a summary except the ones in its provenance block.

    The provenance block — the `- ` bullets above the first `##` heading — is
    exactly what a re-render is allowed to change; a digit anywhere else came
    out of a cell and must survive untouched.
    """
    out: list[str] = []
    in_provenance = True
    for line in md.splitlines():
        if line.startswith("## "):
            in_provenance = False
        if in_provenance and line.startswith("- "):
            continue
        out.extend(NUM_RE.findall(line))
    return out


def is_subsequence(small: list[str], large: list[str]) -> int | None:
    """`None` when every element of `small` appears in `large` in order,
    else the index in `small` of the first one that does not.

    A re-render may *add* numbers — a gate leg that used to go unreported now
    prints its interval — but it may never change or drop one, because every
    number outside the provenance block came out of a cell that nothing
    re-ran.
    """
    it = iter(large)
    for i, want in enumerate(small):
        if not any(got == want for got in it):
            return i
    return None


def rerender(run_dir: Path) -> Path:
    """Rewrite `run_dir/summary.md` from `run_dir/cells.json`, numbers intact."""
    path = run_dir / "summary.md"
    before = path.read_text()
    rows = json.loads((run_dir / "cells.json").read_text())
    write_summary(run_dir, rows, summary_meta(before), filename="summary.md")
    after = path.read_text()
    old, new = numbers(before), numbers(after)
    first = is_subsequence(old, new)
    if first is not None:
        path.write_text(before)          # leave the committed file as it was
        raise SystemExit(
            f"rerender: {path} would change or drop a number "
            f"({len(old)} before, {len(new)} after; first unmatched at "
            f"{first}: {old[first:first + 3]}) — "
            "re-render aborted and the file restored")
    return path


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
    ap.add_argument("--rerender", action="store_true",
                    help="rewrite summary.md in place from cells.json, with "
                         "no grading and no numeric change")
    a = ap.parse_args()

    run_dir = Path(a.run).resolve()
    if a.rerender:
        print(f"re-rendered {rerender(run_dir)} (no cell re-graded)")
        return 0
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
