# Classification of sub-1.0 cells — run 20260911T065400Z

Grading mechanics that matter below (`benchmarks/agent-tasks/ground_truth.py:713-726`): a `set`
check does **naive lowercase substring matching** of every truth/forbid value against the *whole*
answer text — it cannot tell an asserted answer from a value that only appears in the agent's own
shown workings. `SET_PENALTY = 0.25` per forbidden hit, floored at 0.

Seven cells scored below 1.0, out of 180. Five are arm Q (sqlite), two are arm R (graph). Arm P
(files + grep) scored 1.00 on every cell.

## Per-cell classification (7 cells)

| task | key | kind | arm | rep | score | missed | outcome | class | evidence |
|---|---|---|---|---|---|---|---|---|---|
| 5 | assoc-multihop-1 | multihop | Q | 2 | 0.00 | set 27/27 -5 | success | model | Its Python replay builds three *independent* per-relation talent sets (`ind_count`, `spec_count`, `loc_count`) and then rejects a company only `if len(ind_count[ck])<14 or len(spec_count[ck])<14 or len(loc_count[ck])<14` — a per-relation threshold instead of `|ind ∩ spec ∩ loc| >= 14`. It concluded "all 421 remaining companies satisfy the threshold on every one of the three relationship types" and printed the whole company list, so all 27 truth keys hit *and* all 5 forbidden keys hit, floored to 0. No graph tool involved. |
| 13 | assoc-timetravel-1 | timetravel | R | 1 | 0.00 | set 2/8 -5 | success | model | The graph handed it the right answer and it threw it away: `edges_at(talent-000501, at=100, all_of=[IA,SM,LF], label=Company)` returned **exactly the 8-key truth set** (`company-000057/069/129/165/201/237/345/369`), and its own first call, `node_history(talent-000501)`, showed the pivot — `specialties` PropSet at **commit 155**. Instead of mapping 2026-06-19 through `days.json` (never opened) or through that pivot, it probed 503/587/160/300, saw the same late 7-key set, wrote "Stable from commit 160 through 587, so the exact date→commit mapping doesn't matter here", and printed the late set — 2 of 8 truth keys and all 5 forbidden keys. |
| 16 | assoc-timetravel-4 | timetravel | R | 1 | 0.00 | set 0/8 -5 | success | model | Same failure mode without the lucky probe: `node_history(talent-000396)` returned the pivot (`industry` PropSet at **commit 318**), but the agent picked commit **528** with no derivation, checked it only against 587, and reasoned "the two differ (company-000420 was added later), confirming commit 528 correctly captures the 2026-07-14 state" — a difference between two late commits proves nothing about the date. `days.json` was never consulted; no commit near the truth date was ever queried, so 0 of 8 truth keys and all 5 forbidden keys. |
| 6 | assoc-multihop-2 | multihop | Q | 3 | 0.80 | set 8/10 | success | model | Arm Q, no graph. Its own replay printed per-company counts (`company-000013 16 … company-000320 17`) and cut at `>= 16`, emitting 8 companies; `company-000141` and `company-000186` came out under 16 in its reconstruction although reps 1 and 2 of the same arm found them. An arithmetic/state-replay miss on its own side. |
| 14 | assoc-timetravel-2 | timetravel | Q | 1 | 0.83 | set 5/6 | success | model | Arm Q, no graph. `company-000402` genuinely qualifies at 2026-07-12 — sibling arm-Q reps compute it explicitly (`industry True jac 0.25 {'institutional'} dist 25.7`) — but rep 1's hand-rolled day-41 reconstruction never emitted it, and the agent then "verified" only the five keys it already had ("These all check out"), so the miss was never caught. |
| 15 | assoc-timetravel-3 | timetravel | Q | 3 | 0.86 | set 6/7 | success | model | Arm Q, no graph. Same shape: `company-000401` qualifies (reps 1 and 2 compute `0.4, 26.85 km` for it) but rep 3's reconstruction returned only six keys. |
| 11 | assoc-retraction-3 | retraction | Q | 3 | 0.93 | set 13/14 | success | model | Arm Q, no graph. `talent-001444` never appears anywhere in the cell's stream — it was dropped at the "linked by all three today" stage, before the counterfactual was applied, so the 13 keys it did print are all correct and one truth key is simply absent. |

**Classification counts:** tool **0**, model **7**, grader **0**.

No cell in this run is attributable to the graph tool. Every mushroomdb call whose result was
checked returned correct data; the two arm-R losses are both the agent choosing a commit for a date
without deriving it. The grader's naive substring matching did amplify two cells (task 5 rep 2, task
13 rep 1) by scoring the forbidden keys that appear inside a dumped superset — but in both the
underlying answer was already wrong, so neither is a grader cause.

## Per-task-kind summary

**why (tasks 1–4):** **No sub-1.0 cells in any arm.** The `explain_association` payload change
removed the mechanism that cost every why cell in the before run: arm R now answers each why task in
a single tool call, with no raw-property "workings" for the forbid-check to catch.

**multihop (tasks 5–8):** Dominant cause: **model**, and arm-Q-only. Arm R scored 1.00 on all 12
multihop cells. Both losses are in the sqlite baseline, and the worse of the two (task 5 rep 2) is
the same independent-count-instead-of-intersection reasoning bug that hit arm R's Cypher in the
before run — it is a property of how the model writes the query, not of the data surface.

**retraction (tasks 9–12):** Dominant cause: **model**, arm-Q-only. One cell, a single dropped truth
key with no graph involvement. Arm R scored 1.00 on all 12 retraction cells.

**timetravel (tasks 13–16):** Dominant cause: **model**, and specifically **date→commit mapping**.
All four timetravel losses are model: the two arm-R zeroes both picked a commit for the target date
by assertion rather than derivation, while `days.json` and the `node_history` pivot commit — both
available, one of them already returned in the cell's own first call — went unused. The two arm-Q
losses are unrelated reconstruction misses. Nothing here is a tool failure: `edges_at` returned the
exact truth set in task 13 rep 1 when it happened to be asked at a commit on the right side of the
pivot.

**visibility (tasks 17–20):** **No sub-1.0 cells at all**, in either run. Nothing to attribute.

## Before → after, per task kind

Mean score over the four tasks in each kind, from the two runs' "Correctness by task" tables
(before: `20260911T005749Z`; after: `20260911T065400Z`).

| kind | tasks | arm P before → after | arm Q before → after | arm R before → after |
|---|---|---|---|---|
| why | 1–4 | 1.000 → 1.000 | 1.000 → 1.000 | 0.750 → **1.000** |
| multihop | 5–8 | 1.000 → 1.000 | 0.983 → 0.900 | 0.750 → **1.000** |
| retraction | 9–12 | 1.000 → 1.000 | 0.988 → 0.995 | 1.000 → 1.000 |
| timetravel | 13–16 | 1.000 → 1.000 | 0.960 → 0.973 | 0.475 → **0.835** |
| visibility | 17–20 | 1.000 → 1.000 | 1.000 → 1.000 | 1.000 → 1.000 |
| **all** | 1–20 | **1.000 → 1.000** | **0.987 → 0.974** | **0.795 → 0.967** |

Read alongside the cell counts: arm R went from 21 sub-1.0 cells (10 classified tool, 11 model) to
2, both model. The eight max-turns errors in the before run — all arm R, on tasks 5, 8, 14, 15 and
16 — are gone; the after run records 0 timeouts and 0 errors across all 180 cells.

The remaining arm R gap is entirely tasks 13 and 16 (0.67 each), one rep apiece, and both for the
same reason. Arm Q's small decline (0.987 → 0.974) is driven by a single cell, task 5 rep 2, whose
superset dump floors to 0; its other four losses are one-key near-misses.
