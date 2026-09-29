# Classification of sub-1.0 cells — run 20260928T195302Z

Grading mechanics that matter below (`benchmarks/agent-tasks/ground_truth.py:713-726`): a `set`
check does **naive lowercase substring matching** of every truth/forbid value against the answer
text — which is the cell's final `result` payload, falling back to the last assistant message
(`run.py:306-312`). `SET_PENALTY = 0.25` per forbidden hit, floored at 0.

Six cells scored below 1.0, out of 180. **Five are arm Q (sqlite), one is arm P (files + grep).
Arm R (graph) scored 1.00 on every one of its 60 cells** — the first run in which it did.

## Per-cell classification (6 cells)

| task | key | kind | arm | rep | score | missed | outcome | class | evidence |
|---|---|---|---|---|---|---|---|---|---|
| 2 | assoc-why-2 | why | P | 3 | 0.00 | set 0/5 | success | model | **It had the whole answer and did not say it.** All five truth values appear in the cell's own stream — `INDUSTRY_ALIGNMENT` ×18, `SPECIALTY_MATCH` ×18, `MATCHES_DESIGN_STYLE` ×11, `multi-family` ×25, `residential` ×25 — but the graded text is the final `result`, and that reads in full: *"The Explore agent's findings confirm my analysis — I've already answered this. No further action needed."* It had spawned an `Agent` subagent (16 tool calls, 1 turn) and treated the earlier turn as the answer. A delivery failure, not a reasoning one; the same task scored 1.00 in arm P reps 1 and 2. |
| 8 | assoc-multihop-4 | multihop | Q | 3 | 0.78 | set 7/9 | success | model | `company-000112` and `company-000405` **never appear anywhere in the cell stream** (0 occurrences each), so its SQL reconstruction excluded them before any output. Both are genuinely in the truth: arm R rep 3 names each 6 times and scores 1.00. No graph involved — arm Q has `world.sqlite` and no MCP server. |
| 15 | assoc-timetravel-3 | timetravel | Q | 2 | 0.86 | set 6/7 | success | model | `company-000401` **never appears in the stream** (0 occurrences). Arm R rep 2 names it 4 times and scores 1.00. |
| 15 | assoc-timetravel-3 | timetravel | Q | 3 | 0.86 | set 6/7 | success | model | Identical answer to rep 2, same key absent, same 0 occurrences. **This exact miss — task 15, arm Q, `company-000401` — also occurred in run `20260911T065400Z`**, two weeks and one engine apart. It is a reproducible property of how the model reconstructs a past day from `world.sqlite` and its changes log, not run-to-run noise. |
| 11 | assoc-retraction-3 | retraction | Q | 3 | 0.93 | set 13/14 | success | model | `talent-001444` **never appears in the stream** (0 occurrences); the 13 keys it printed are all correct and the fourteenth is simply absent from its reconstruction. Arm R rep 3 names it 8 times. **Also a repeat of `20260911T065400Z`** — same task, same arm, same missing key. |
| 18 | assoc-visibility-2 | visibility | Q | 2 | 0.97 | set 34/35 | success | model | `job-000203` **never appears in the stream** (0 occurrences); 34 of 35 printed, the missing one is the last in sorted order. Arm R rep 2 names it 4 times. |

**Classification counts:** tool **0**, model **6**, grader **0**.

## What the evidence rules out

**Not the grader.** Every key a baseline missed was produced and asserted by arm R in the *same
rep*, so each truth value is reachable and correct. No sub-1.0 cell is a case of a right answer
scored wrong.

**Not the graph.** Arm R did not lose a single cell. Five of the six losses are in an arm with no
MCP server at all, and the sixth (arm P) is a `claude -p` answer-delivery failure with no data
surface involved.

**A note on the one 0.00.** Task 2 arm P rep 3 scoring zero while holding the complete answer is
worth separating from the other five: it says something about the harness reading only the final
`result` payload, not about the files-and-grep surface. Treated as `model` because the answer *as
delivered* was empty, which is what the gate measures, but it is not a reasoning error and should
not be read as one.

## Per-task-kind summary

**why (tasks 1–4):** one sub-1.0 cell, arm P, the delivery failure above. Arms Q and R both 1.00
across all 12 cells.

**multihop (tasks 5–8):** one sub-1.0 cell, arm Q rep 3. Arm R 1.00 on all 12.

**retraction (tasks 9–12):** one sub-1.0 cell, arm Q rep 3, repeating the 11 Sep miss exactly.
Arm R 1.00 on all 12.

**timetravel (tasks 13–16):** two sub-1.0 cells, both arm Q, both the same key on the same task.
**Arm R scored 1.00 on all 12 timetravel cells, each in a single MCP call at ~77.6k tokens.** In
run `20260925T200950Z` this kind was the gate's only failure — `assoc-timetravel-4` at 0.88 across
three reps — and that loss was a date-resolution defect in the engine, fixed before this run: the
sidecar recorded a commit counter where a WAL frame index was needed, so `2026-07-14` resolved to
frame 167 instead of 317, answering with a graph 23 simulated days stale. With the fix,
`2026-07-14` resolves to 317 and all four tasks answer correctly in every rep.

**visibility (tasks 17–20):** one sub-1.0 cell, arm Q rep 2. Arm R 1.00 on all 12.

## The one pattern worth naming

Four of the five arm-Q losses miss keys that **never appear anywhere in the cell's own stream** —
the reconstruction dropped them before printing, so the agent had no opportunity to notice. Three
of those four miss the *last* key in sorted order (`talent-001444`, `job-000203`, and
`company-000401` in task 15). Two of them reproduce exactly across two independent runs. That is a
stable property of replaying a change log in SQL by hand, and it is the cost the graph arm avoids
by asking one question of a surface that already maintains the answer.
