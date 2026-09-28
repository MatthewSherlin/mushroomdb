# Agent benchmark run

- run: `20260925T200950Z`
- suite: association
- baseline arm: Q (sqlite)
- world digest: `f2b689ba52c80241`
- model: sonnet, max-turns 30, cell timeout 900s
- arm P (files + grep): the world as `entities/*.json`, `changes.jsonl`, `roles.json` and a README describing the rules; no MCP server; plain prompt
- arm Q (sqlite): the same world as `world.sqlite` (`sqlite3` on PATH) with the same README and its schema; no MCP server; plain prompt
- arm R (graph): the same world as a mushroomdb store — `install --delivery mcp --db ./world.mushroomdb` into a directory holding nothing else, so the store is the only data path; plain prompt
- allowed tools, every arm: `Read,Grep,Glob,Bash,Edit,Write` (+ `mcp__mushroomdb` for R)
- every cell runs in a fresh copy of its arm's subject directory; nothing is carried from one cell to the next.
- a cell that timed out or errored carries no cost, turns or duration; it is excluded from those means and from their paired differences, never counted as zero. Each section says how many cells it dropped.

## Gate

The pre-registered §1 gate: correctness >= arm Q (paired by task), at or above every other arm's paired mean (a tie passes), cost <= arm Q, the 95% cost interval vs arm Q lies entirely below zero, no max-turns failure on a task arm Q finished.

Correctness carries no interval requirement: the baseline arm Q saturated it in the pilot (1.00 on every task), so cost is the discriminator (§1, amended 2026-09-11).

Adoption is recorded below, not gated: in the graph arm the store is the agent's only data path (§1).

| | |
|---|---|
| verdict | **FAILED** |
| best arm | R |
| arms under the gate | R |
| cells with no cost recorded | 0 of 180 |

Why it failed:

- R: correctness -0.006 vs arm Q, paired over 20 task(s)
- R: correctness -0.006 is below arm(s) P (+0.000)

## Per cell

| task | key | arm | rep | in | out | cache read | cache create | total tok | cache hit | tools | mcp | graph | tool search | turns | sec | cost $ | score | extra | denials | verify | dirtied | outcome |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | assoc-why-1 | P | 1 | 8 | 912 | 70994 | 12407 | 84321 | 0.851 | 3 | 0 | 0 | 0 | 4 | 9.0 | 0.0730 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 1 | 8 | 711 | 69614 | 10626 | 80959 | 0.868 | 3 | 0 | 0 | 0 | 4 | 8.0 | 0.0636 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 1 | 4 | 267 | 46099 | 5193 | 51563 | 0.899 | 2 | 2 | 2 | 0 | 3 | 16.6 | 0.0327 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | P | 2 | 8 | 898 | 76564 | 6801 | 84271 | 0.918 | 3 | 0 | 0 | 0 | 4 | 9.0 | 0.0515 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 2 | 8 | 703 | 75191 | 5030 | 80932 | 0.937 | 3 | 0 | 0 | 0 | 4 | 7.6 | 0.0422 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 2 | 4 | 248 | 51292 | 0 | 51544 | 1.000 | 2 | 2 | 2 | 0 | 3 | 16.2 | 0.0127 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | P | 3 | 8 | 984 | 76569 | 6976 | 84537 | 0.916 | 3 | 0 | 0 | 0 | 4 | 9.4 | 0.0531 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 3 | 8 | 682 | 75191 | 5054 | 80935 | 0.937 | 3 | 0 | 0 | 0 | 4 | 8.0 | 0.0421 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 3 | 4 | 279 | 51292 | 0 | 51575 | 1.000 | 2 | 2 | 2 | 0 | 3 | 17.0 | 0.0131 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 1 | 8 | 975 | 72910 | 9722 | 83615 | 0.882 | 3 | 0 | 0 | 0 | 4 | 9.5 | 0.0632 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 1 | 8 | 717 | 72292 | 8261 | 81278 | 0.897 | 3 | 0 | 0 | 0 | 4 | 7.6 | 0.0547 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 1 | 4 | 258 | 46099 | 5178 | 51539 | 0.899 | 2 | 2 | 2 | 0 | 3 | 17.2 | 0.0325 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 2 | 8 | 1012 | 76811 | 5792 | 83623 | 0.930 | 3 | 0 | 0 | 0 | 4 | 9.8 | 0.0487 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 2 | 8 | 696 | 75191 | 4982 | 80877 | 0.938 | 3 | 0 | 0 | 0 | 4 | 7.5 | 0.0419 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 2 | 4 | 307 | 50080 | 30379 | 80770 | 0.622 | 3 | 3 | 3 | 0 | 4 | 16.7 | 0.1346 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 3 | 8 | 916 | 76569 | 6828 | 84321 | 0.918 | 3 | 0 | 0 | 0 | 4 | 8.8 | 0.0518 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 3 | 8 | 660 | 75191 | 4957 | 80816 | 0.938 | 3 | 0 | 0 | 0 | 4 | 7.1 | 0.0415 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 3 | 6 | 414 | 77514 | 30121 | 108055 | 0.720 | 4 | 4 | 4 | 0 | 5 | 18.3 | 0.1401 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 1 | 8 | 1009 | 72999 | 9814 | 83830 | 0.881 | 3 | 0 | 0 | 0 | 4 | 10.9 | 0.0640 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 1 | 8 | 711 | 71317 | 8875 | 80911 | 0.889 | 3 | 0 | 0 | 0 | 4 | 9.8 | 0.0569 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 1 | 4 | 319 | 46099 | 6200 | 52622 | 0.881 | 2 | 2 | 2 | 0 | 3 | 18.2 | 0.0372 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 2 | 8 | 950 | 76872 | 5969 | 83799 | 0.928 | 3 | 0 | 0 | 0 | 4 | 10.3 | 0.0488 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 2 | 8 | 782 | 75191 | 4981 | 80962 | 0.938 | 3 | 0 | 0 | 0 | 4 | 9.5 | 0.0428 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 2 | 4 | 341 | 50080 | 2238 | 52663 | 0.957 | 2 | 2 | 2 | 0 | 3 | 16.8 | 0.0224 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 3 | 8 | 1025 | 76569 | 6895 | 84497 | 0.917 | 3 | 0 | 0 | 0 | 4 | 10.6 | 0.0532 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 3 | 8 | 748 | 74619 | 4409 | 79784 | 0.944 | 3 | 0 | 0 | 0 | 4 | 9.3 | 0.0401 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 3 | 4 | 265 | 50080 | 1238 | 51587 | 0.976 | 2 | 2 | 2 | 0 | 3 | 16.7 | 0.0176 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 1 | 8 | 979 | 72999 | 9701 | 83687 | 0.883 | 3 | 0 | 0 | 0 | 4 | 9.4 | 0.0632 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 1 | 8 | 739 | 72430 | 8384 | 81561 | 0.896 | 3 | 0 | 0 | 0 | 4 | 8.1 | 0.0554 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 1 | 4 | 267 | 46099 | 4695 | 51065 | 0.907 | 2 | 2 | 2 | 0 | 3 | 17.5 | 0.0307 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 2 | 8 | 1017 | 76564 | 6865 | 84454 | 0.918 | 3 | 0 | 0 | 0 | 4 | 9.4 | 0.0530 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 2 | 8 | 724 | 76304 | 4515 | 81551 | 0.944 | 3 | 0 | 0 | 0 | 4 | 7.5 | 0.0406 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 2 | 4 | 266 | 50794 | 0 | 51064 | 1.000 | 2 | 2 | 2 | 0 | 3 | 16.9 | 0.0128 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 3 | 8 | 889 | 76810 | 5636 | 83343 | 0.931 | 3 | 0 | 0 | 0 | 4 | 9.3 | 0.0468 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 3 | 8 | 724 | 76137 | 4343 | 81212 | 0.946 | 3 | 0 | 0 | 0 | 4 | 7.7 | 0.0399 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 3 | 4 | 247 | 50080 | 941 | 51272 | 0.982 | 2 | 2 | 2 | 0 | 3 | 16.5 | 0.0163 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 1 | 8 | 1146 | 73253 | 9945 | 84352 | 0.880 | 3 | 0 | 0 | 0 | 4 | 10.2 | 0.0659 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 1 | 14 | 1776 | 142712 | 10999 | 155501 | 0.928 | 6 | 0 | 0 | 0 | 7 | 15.8 | 0.0903 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 1 | 10 | 885 | 124394 | 5836 | 131125 | 0.955 | 5 | 5 | 5 | 0 | 6 | 26.6 | 0.0571 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 2 | 10 | 1711 | 104238 | 8360 | 114319 | 0.926 | 4 | 0 | 0 | 0 | 5 | 14.3 | 0.0714 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 2 | 10 | 1338 | 98827 | 6269 | 106444 | 0.940 | 4 | 0 | 0 | 0 | 5 | 12.5 | 0.0582 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 2 | 8 | 681 | 102445 | 983 | 104117 | 0.990 | 4 | 4 | 4 | 0 | 5 | 22.9 | 0.0312 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 3 | 8 | 1117 | 78178 | 6848 | 86151 | 0.919 | 3 | 0 | 0 | 0 | 4 | 9.8 | 0.0542 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 3 | 8 | 1288 | 77178 | 5891 | 84365 | 0.929 | 3 | 0 | 0 | 0 | 4 | 11.4 | 0.0519 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 3 | 8 | 719 | 102558 | 1095 | 104380 | 0.989 | 4 | 4 | 4 | 0 | 5 | 23.3 | 0.0321 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 1 | 8 | 1063 | 73253 | 9617 | 83941 | 0.884 | 3 | 0 | 0 | 0 | 4 | 9.9 | 0.0638 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 1 | 10 | 1365 | 94921 | 9655 | 105951 | 0.908 | 4 | 0 | 0 | 0 | 5 | 13.2 | 0.0713 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 1 | 12 | 734 | 150523 | 5792 | 157061 | 0.963 | 5 | 5 | 5 | 0 | 6 | 24.3 | 0.0606 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 2 | 8 | 1022 | 78711 | 7173 | 86914 | 0.916 | 3 | 0 | 0 | 0 | 4 | 9.7 | 0.0547 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 2 | 14 | 1579 | 146874 | 6853 | 155320 | 0.955 | 6 | 0 | 0 | 0 | 7 | 14.6 | 0.0726 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 2 | 10 | 720 | 128946 | 1079 | 130755 | 0.992 | 5 | 5 | 5 | 0 | 6 | 23.8 | 0.0373 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 3 | 12 | 1435 | 124961 | 6763 | 133171 | 0.949 | 5 | 0 | 0 | 0 | 6 | 13.7 | 0.0664 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 3 | 8 | 1124 | 77178 | 5763 | 84073 | 0.930 | 3 | 0 | 0 | 0 | 4 | 10.4 | 0.0497 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 3 | 6 | 410 | 76358 | 446 | 77220 | 0.994 | 2 | 2 | 2 | 0 | 3 | 21.8 | 0.0212 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 1 | 8 | 1049 | 74753 | 11122 | 86932 | 0.870 | 3 | 0 | 0 | 0 | 4 | 9.6 | 0.0699 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 1 | 8 | 1225 | 72929 | 9393 | 83555 | 0.886 | 3 | 0 | 0 | 0 | 4 | 11.2 | 0.0644 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 1 | 10 | 749 | 124396 | 5892 | 131047 | 0.955 | 5 | 5 | 5 | 0 | 6 | 26.0 | 0.0560 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 2 | 8 | 1075 | 77970 | 6514 | 85567 | 0.923 | 3 | 0 | 0 | 0 | 4 | 10.2 | 0.0524 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 2 | 8 | 1280 | 77178 | 5869 | 84335 | 0.929 | 3 | 0 | 0 | 0 | 4 | 11.6 | 0.0517 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 2 | 12 | 1025 | 155625 | 1497 | 158159 | 0.990 | 5 | 5 | 5 | 0 | 6 | 26.2 | 0.0474 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 3 | 12 | 2412 | 125641 | 7997 | 136062 | 0.940 | 5 | 0 | 0 | 0 | 6 | 18.0 | 0.0813 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 3 | 8 | 1188 | 76849 | 5436 | 83481 | 0.934 | 3 | 0 | 0 | 0 | 4 | 10.9 | 0.0490 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 3 | 12 | 1075 | 155891 | 1877 | 158855 | 0.988 | 7 | 7 | 7 | 0 | 8 | 29.0 | 0.0495 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 1 | 10 | 1065 | 96011 | 9925 | 107011 | 0.906 | 4 | 0 | 0 | 0 | 5 | 10.7 | 0.0696 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 1 | 8 | 1205 | 73258 | 9692 | 84163 | 0.883 | 3 | 0 | 0 | 0 | 4 | 11.3 | 0.0655 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 1 | 14 | 1402 | 177901 | 6555 | 185872 | 0.964 | 6 | 6 | 6 | 0 | 7 | 31.7 | 0.0758 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 2 | 8 | 998 | 78672 | 7130 | 86808 | 0.917 | 3 | 0 | 0 | 0 | 4 | 10.2 | 0.0543 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 2 | 8 | 1201 | 77178 | 5768 | 84155 | 0.930 | 3 | 0 | 0 | 0 | 4 | 10.5 | 0.0505 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 2 | 6 | 552 | 76202 | 1359 | 78119 | 0.982 | 3 | 3 | 3 | 0 | 4 | 39.0 | 0.0262 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 3 | 8 | 1027 | 78703 | 7176 | 86914 | 0.916 | 3 | 0 | 0 | 0 | 4 | 9.3 | 0.0547 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 3 | 8 | 1180 | 76849 | 5332 | 83369 | 0.935 | 3 | 0 | 0 | 0 | 4 | 10.6 | 0.0485 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 3 | 12 | 1261 | 155752 | 1762 | 158787 | 0.989 | 6 | 6 | 6 | 0 | 7 | 27.2 | 0.0508 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 1 | 8 | 926 | 72567 | 10322 | 83823 | 0.875 | 3 | 0 | 0 | 0 | 4 | 10.7 | 0.0651 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 1 | 8 | 1220 | 73403 | 10064 | 84695 | 0.879 | 3 | 0 | 0 | 0 | 4 | 12.2 | 0.0672 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 1 | 4 | 688 | 46172 | 23957 | 70821 | 0.658 | 3 | 3 | 3 | 0 | 4 | 20.8 | 0.1120 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 2 | 8 | 1043 | 77093 | 5731 | 83875 | 0.931 | 3 | 0 | 0 | 0 | 4 | 11.2 | 0.0488 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 2 | 8 | 1253 | 75483 | 6239 | 82983 | 0.924 | 3 | 0 | 0 | 0 | 4 | 13.1 | 0.0526 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 2 | 4 | 679 | 70129 | 0 | 70812 | 1.000 | 3 | 3 | 3 | 0 | 4 | 20.1 | 0.0208 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 3 | 8 | 1056 | 76548 | 6483 | 84095 | 0.922 | 3 | 0 | 0 | 0 | 4 | 10.8 | 0.0518 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 3 | 10 | 1582 | 100476 | 6720 | 108788 | 0.937 | 4 | 0 | 0 | 0 | 5 | 17.0 | 0.0628 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 3 | 4 | 779 | 50226 | 19927 | 70936 | 0.716 | 3 | 3 | 3 | 0 | 4 | 20.9 | 0.0976 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 1 | 8 | 1013 | 70961 | 8793 | 80775 | 0.890 | 3 | 0 | 0 | 0 | 4 | 9.9 | 0.0595 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 1 | 8 | 1545 | 73042 | 10607 | 85202 | 0.873 | 3 | 0 | 0 | 0 | 4 | 14.0 | 0.0725 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 1 | 4 | 740 | 46177 | 23251 | 70172 | 0.665 | 3 | 3 | 3 | 0 | 4 | 21.5 | 0.1096 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 2 | 8 | 1058 | 74895 | 4940 | 80901 | 0.938 | 3 | 0 | 0 | 0 | 4 | 10.6 | 0.0453 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 2 | 8 | 1267 | 77072 | 6020 | 84367 | 0.927 | 3 | 0 | 0 | 0 | 4 | 11.8 | 0.0522 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 2 | 4 | 746 | 69428 | 0 | 70178 | 1.000 | 3 | 3 | 3 | 0 | 4 | 20.3 | 0.0214 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 3 | 8 | 995 | 74885 | 5025 | 80913 | 0.937 | 3 | 0 | 0 | 0 | 4 | 9.6 | 0.0450 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 3 | 8 | 1263 | 76665 | 5697 | 83633 | 0.931 | 3 | 0 | 0 | 0 | 4 | 11.7 | 0.0508 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 3 | 4 | 760 | 69428 | 0 | 70192 | 1.000 | 3 | 3 | 3 | 0 | 4 | 20.3 | 0.0215 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 1 | 8 | 1328 | 73162 | 9822 | 84320 | 0.882 | 3 | 0 | 0 | 0 | 4 | 13.3 | 0.0672 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 1 | 10 | 1469 | 96446 | 10583 | 108508 | 0.901 | 4 | 0 | 0 | 0 | 5 | 13.3 | 0.0763 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 1 | 4 | 643 | 46164 | 9980 | 56791 | 0.822 | 2 | 2 | 2 | 0 | 3 | 20.7 | 0.0556 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 2 | 8 | 1166 | 75579 | 5058 | 81811 | 0.937 | 3 | 0 | 0 | 0 | 4 | 11.2 | 0.0470 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 2 | 14 | 2002 | 180864 | 17713 | 200593 | 0.911 | 6 | 0 | 0 | 0 | 7 | 18.7 | 0.1271 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 2 | 4 | 700 | 50210 | 20854 | 71768 | 0.707 | 3 | 3 | 3 | 0 | 4 | 20.0 | 0.1005 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 3 | 8 | 973 | 77833 | 6302 | 85116 | 0.925 | 3 | 0 | 0 | 0 | 4 | 9.2 | 0.0505 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 3 | 10 | 1442 | 101119 | 7409 | 109980 | 0.932 | 4 | 0 | 0 | 0 | 5 | 13.3 | 0.0643 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 3 | 4 | 634 | 56144 | 0 | 56782 | 1.000 | 2 | 2 | 2 | 0 | 3 | 19.8 | 0.0176 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 1 | 8 | 1016 | 73895 | 10411 | 85330 | 0.876 | 3 | 0 | 0 | 0 | 4 | 9.8 | 0.0666 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 1 | 12 | 1662 | 120454 | 10730 | 132858 | 0.918 | 5 | 0 | 0 | 0 | 6 | 15.2 | 0.0837 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 1 | 4 | 663 | 46164 | 10048 | 56879 | 0.821 | 2 | 2 | 2 | 0 | 3 | 20.9 | 0.0561 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 2 | 8 | 953 | 77833 | 6392 | 85186 | 0.924 | 3 | 0 | 0 | 0 | 4 | 9.5 | 0.0507 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 2 | 8 | 959 | 75280 | 5443 | 81690 | 0.932 | 3 | 0 | 0 | 0 | 4 | 9.6 | 0.0464 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 2 | 4 | 644 | 56212 | 0 | 56860 | 1.000 | 2 | 2 | 2 | 0 | 3 | 19.8 | 0.0177 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 3 | 8 | 999 | 77833 | 6456 | 85296 | 0.923 | 3 | 0 | 0 | 0 | 4 | 10.0 | 0.0514 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 3 | 12 | 1935 | 126174 | 7667 | 135788 | 0.943 | 5 | 0 | 0 | 0 | 6 | 17.9 | 0.0753 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 3 | 4 | 660 | 56212 | 0 | 56876 | 1.000 | 2 | 2 | 2 | 0 | 3 | 19.8 | 0.0179 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 1 | 8 | 909 | 70502 | 8221 | 79640 | 0.895 | 3 | 0 | 0 | 0 | 4 | 9.2 | 0.0561 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 1 | 10 | 1580 | 93436 | 9627 | 104653 | 0.906 | 4 | 0 | 0 | 0 | 5 | 16.0 | 0.0730 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 1 | 4 | 216 | 50494 | 0 | 50714 | 1.000 | 1 | 1 | 1 | 0 | 2 | 17.3 | 0.0123 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 2 | 8 | 1023 | 74422 | 4328 | 79781 | 0.945 | 3 | 0 | 0 | 0 | 4 | 9.7 | 0.0424 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 2 | 8 | 1363 | 75285 | 5821 | 82477 | 0.928 | 3 | 0 | 0 | 0 | 4 | 14.3 | 0.0520 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 2 | 4 | 216 | 50494 | 0 | 50714 | 1.000 | 1 | 1 | 1 | 0 | 2 | 16.6 | 0.0123 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 3 | 8 | 1038 | 74219 | 4334 | 79599 | 0.945 | 3 | 0 | 0 | 0 | 4 | 10.0 | 0.0426 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 3 | 10 | 1429 | 96497 | 6393 | 104329 | 0.938 | 4 | 0 | 0 | 0 | 5 | 15.0 | 0.0592 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 3 | 4 | 216 | 50494 | 0 | 50714 | 1.000 | 1 | 1 | 1 | 0 | 2 | 16.2 | 0.0123 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 1 | 8 | 1080 | 70371 | 8133 | 79592 | 0.896 | 3 | 0 | 0 | 0 | 4 | 10.8 | 0.0574 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 1 | 10 | 1406 | 92793 | 9312 | 103521 | 0.909 | 4 | 0 | 0 | 0 | 5 | 14.6 | 0.0699 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 1 | 4 | 206 | 50483 | 0 | 50693 | 1.000 | 1 | 1 | 1 | 0 | 2 | 16.8 | 0.0122 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 2 | 8 | 1003 | 74217 | 4414 | 79642 | 0.944 | 3 | 0 | 0 | 0 | 4 | 10.0 | 0.0425 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 2 | 8 | 1310 | 75285 | 5659 | 82262 | 0.930 | 3 | 0 | 0 | 0 | 4 | 13.0 | 0.0508 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 2 | 4 | 206 | 50483 | 0 | 50693 | 1.000 | 1 | 1 | 1 | 0 | 2 | 16.3 | 0.0122 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 3 | 8 | 963 | 74215 | 4506 | 79692 | 0.943 | 3 | 0 | 0 | 0 | 4 | 9.4 | 0.0425 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 3 | 8 | 1109 | 75118 | 5548 | 81783 | 0.931 | 3 | 0 | 0 | 0 | 4 | 12.4 | 0.0483 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 3 | 4 | 206 | 50483 | 0 | 50693 | 1.000 | 1 | 1 | 1 | 0 | 2 | 16.4 | 0.0122 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | P | 1 | 8 | 978 | 70502 | 8356 | 79844 | 0.894 | 3 | 0 | 0 | 0 | 4 | 9.2 | 0.0573 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 1 | 8 | 1271 | 70765 | 8990 | 81034 | 0.887 | 3 | 0 | 0 | 0 | 4 | 13.7 | 0.0628 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 1 | 4 | 211 | 50489 | 0 | 50704 | 1.000 | 1 | 1 | 1 | 0 | 2 | 17.1 | 0.0122 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | P | 2 | 8 | 855 | 74689 | 4718 | 80270 | 0.941 | 3 | 0 | 0 | 0 | 4 | 8.4 | 0.0424 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 2 | 8 | 1210 | 75285 | 13214 | 89717 | 0.851 | 3 | 0 | 0 | 0 | 4 | 12.2 | 0.0800 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 2 | 4 | 211 | 50489 | 0 | 50704 | 1.000 | 1 | 1 | 1 | 0 | 2 | 16.5 | 0.0122 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | P | 3 | 8 | 918 | 74689 | 4625 | 80240 | 0.942 | 3 | 0 | 0 | 0 | 4 | 9.2 | 0.0426 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 3 | 8 | 1231 | 76454 | 5845 | 83538 | 0.929 | 3 | 0 | 0 | 0 | 4 | 13.5 | 0.0510 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 3 | 6 | 382 | 75543 | 718 | 76649 | 0.991 | 2 | 2 | 2 | 0 | 3 | 18.7 | 0.0218 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | P | 1 | 8 | 1016 | 70670 | 8549 | 80243 | 0.892 | 3 | 0 | 0 | 0 | 4 | 10.4 | 0.0585 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 1 | 10 | 1230 | 92780 | 9188 | 103208 | 0.910 | 4 | 0 | 0 | 0 | 5 | 13.6 | 0.0676 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 1 | 4 | 211 | 50489 | 0 | 50704 | 1.000 | 1 | 1 | 1 | 0 | 2 | 16.9 | 0.0122 | 0.88 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | P | 2 | 8 | 848 | 74405 | 4308 | 79569 | 0.945 | 3 | 0 | 0 | 0 | 4 | 8.9 | 0.0406 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 2 | 8 | 1188 | 74665 | 5077 | 80938 | 0.936 | 3 | 0 | 0 | 0 | 4 | 11.8 | 0.0471 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 2 | 4 | 211 | 50489 | 0 | 50704 | 1.000 | 1 | 1 | 1 | 0 | 2 | 16.5 | 0.0122 | 0.88 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | P | 3 | 10 | 1095 | 95837 | 4681 | 101623 | 0.953 | 4 | 0 | 0 | 0 | 5 | 11.2 | 0.0489 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 3 | 10 | 1458 | 98618 | 6342 | 106428 | 0.940 | 4 | 0 | 0 | 0 | 5 | 15.5 | 0.0597 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 3 | 4 | 211 | 50489 | 0 | 50704 | 1.000 | 1 | 1 | 1 | 0 | 2 | 16.1 | 0.0122 | 0.88 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 1 | 8 | 899 | 73034 | 9489 | 83430 | 0.885 | 3 | 0 | 0 | 0 | 4 | 9.1 | 0.0616 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 1 | 12 | 1594 | 121283 | 10703 | 133592 | 0.919 | 6 | 0 | 0 | 0 | 7 | 14.0 | 0.0830 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 1 | 4 | 299 | 46130 | 4429 | 50862 | 0.912 | 1 | 1 | 1 | 0 | 2 | 16.8 | 0.0299 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 2 | 8 | 867 | 76938 | 5400 | 83213 | 0.934 | 3 | 0 | 0 | 0 | 4 | 8.7 | 0.0457 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 2 | 10 | 1394 | 101076 | 6445 | 108925 | 0.940 | 5 | 0 | 0 | 0 | 6 | 12.9 | 0.0600 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 2 | 4 | 344 | 50142 | 418 | 50908 | 0.992 | 1 | 1 | 1 | 0 | 2 | 16.7 | 0.0151 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 3 | 8 | 875 | 76938 | 5561 | 83382 | 0.932 | 3 | 0 | 0 | 0 | 4 | 8.5 | 0.0464 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 3 | 10 | 1144 | 100147 | 6150 | 107451 | 0.942 | 4 | 0 | 0 | 0 | 5 | 11.0 | 0.0561 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 3 | 4 | 481 | 50142 | 1129 | 51756 | 0.978 | 2 | 2 | 2 | 0 | 3 | 17.7 | 0.0194 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 1 | 10 | 1303 | 96022 | 10072 | 107407 | 0.905 | 4 | 0 | 0 | 0 | 5 | 11.8 | 0.0725 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 1 | 10 | 1265 | 96166 | 10171 | 107612 | 0.904 | 4 | 0 | 0 | 0 | 5 | 12.1 | 0.0726 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 1 | 4 | 530 | 46130 | 5300 | 51964 | 0.897 | 2 | 2 | 2 | 0 | 3 | 17.9 | 0.0357 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 2 | 10 | 1280 | 101344 | 6672 | 109306 | 0.938 | 4 | 0 | 0 | 0 | 5 | 11.3 | 0.0598 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 2 | 10 | 1208 | 99956 | 6283 | 107457 | 0.941 | 4 | 0 | 0 | 0 | 5 | 11.4 | 0.0572 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 2 | 4 | 572 | 51430 | 0 | 52006 | 1.000 | 2 | 2 | 2 | 0 | 3 | 18.6 | 0.0160 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 3 | 8 | 975 | 76938 | 5680 | 83601 | 0.931 | 3 | 0 | 0 | 0 | 4 | 9.4 | 0.0479 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 3 | 10 | 1325 | 100188 | 6331 | 107854 | 0.941 | 5 | 0 | 0 | 0 | 6 | 11.9 | 0.0586 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 3 | 4 | 533 | 50142 | 1322 | 52001 | 0.974 | 2 | 2 | 2 | 0 | 3 | 17.7 | 0.0207 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 1 | 8 | 845 | 73835 | 10235 | 84923 | 0.878 | 3 | 0 | 0 | 0 | 4 | 9.3 | 0.0642 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 1 | 10 | 1281 | 95678 | 10176 | 107145 | 0.904 | 4 | 0 | 0 | 0 | 5 | 12.1 | 0.0727 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 1 | 6 | 627 | 71861 | 5029 | 77523 | 0.934 | 3 | 3 | 3 | 0 | 4 | 19.2 | 0.0408 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 2 | 8 | 847 | 77753 | 6319 | 84927 | 0.925 | 3 | 0 | 0 | 0 | 4 | 8.4 | 0.0493 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 2 | 14 | 1629 | 147143 | 6590 | 155376 | 0.957 | 6 | 0 | 0 | 0 | 7 | 15.0 | 0.0721 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 2 | 6 | 402 | 75597 | 643 | 76648 | 0.992 | 2 | 2 | 2 | 0 | 3 | 17.4 | 0.0217 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 3 | 8 | 930 | 76874 | 5396 | 83208 | 0.934 | 3 | 0 | 0 | 0 | 4 | 9.2 | 0.0463 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 3 | 14 | 1701 | 147368 | 6776 | 155859 | 0.956 | 6 | 0 | 0 | 0 | 7 | 15.8 | 0.0736 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 3 | 6 | 417 | 75939 | 316 | 76678 | 0.996 | 2 | 2 | 2 | 0 | 3 | 17.9 | 0.0206 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 1 | 8 | 873 | 73835 | 10063 | 84779 | 0.880 | 3 | 0 | 0 | 0 | 4 | 8.5 | 0.0638 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 1 | 10 | 1511 | 95948 | 10248 | 107717 | 0.903 | 4 | 0 | 0 | 0 | 5 | 14.2 | 0.0753 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 1 | 6 | 613 | 71868 | 5024 | 77511 | 0.935 | 3 | 3 | 3 | 0 | 4 | 18.9 | 0.0406 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 2 | 8 | 857 | 77753 | 6327 | 84945 | 0.925 | 3 | 0 | 0 | 0 | 4 | 9.0 | 0.0494 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 2 | 10 | 1211 | 99103 | 5767 | 106091 | 0.945 | 4 | 0 | 0 | 0 | 5 | 11.5 | 0.0550 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 2 | 4 | 436 | 50170 | 638 | 51248 | 0.987 | 2 | 2 | 2 | 0 | 3 | 17.4 | 0.0170 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 3 | 10 | 1526 | 99698 | 6144 | 107378 | 0.942 | 4 | 0 | 0 | 0 | 5 | 13.4 | 0.0598 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 3 | 14 | 1513 | 147638 | 6641 | 155806 | 0.957 | 6 | 0 | 0 | 0 | 7 | 14.5 | 0.0712 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 3 | 4 | 465 | 50808 | 0 | 51277 | 1.000 | 2 | 2 | 2 | 0 | 3 | 18.4 | 0.0148 | 1.00 | 0 | 0 | - | - | success |

## Aggregate (mean per arm)

| metric | arm P (files + grep) | arm Q (sqlite) | arm R (graph) | P vs Q | R vs Q |
|---|---|---|---|---|---|
| total tokens | 87469 | 100506 | 73578 | -13.0% | -26.8% |
| input tokens | 8 | 9 | 5 | -11.3% | -42.6% |
| output tokens | 1050 | 1242 | 507 | -15.4% | -59.1% |
| cache read | 79173 | 91763 | 68842 | -13.7% | -25.0% |
| cache create | 7237 | 7492 | 4222 | -3.4% | -43.6% |
| cache hit ratio | 0.915 | 0.923 | 0.943 | -0.8% | +2.2% |
| tool calls | 3.17 | 3.75 | 2.53 | -15.6% | -32.4% |
| mcp calls | 0.00 | 0.00 | 2.53 | n/a | n/a |
| graph calls | 0.00 | 0.00 | 2.53 | n/a | n/a |
| tool search calls | 0.00 | 0.00 | 0.00 | n/a | n/a |
| adoption | 0.00 | 0.00 | 1.00 | n/a | n/a |
| turns | 4.17 | 4.75 | 3.53 | -12.3% | -25.6% |
| seconds | 10.2 | 12.2 | 19.8 | -16.6% | +62.3% |
| cost $ | 0.0553 | 0.0608 | 0.0357 | -9.0% | -41.2% |
| score | 1.000 | 1.000 | 0.994 | +0.0% | -0.6% |
| wrong extra | 0.00 | 0.00 | 0.00 | n/a | n/a |

arm P: 60 cells; arm Q: 60 cells; arm R: 60 cells; timeouts 0; errors 0; cells with no cost 0

## Deltas vs arm Q

Paired by task: one difference per task, arm mean minus arm Q mean. The interval is a 95% percentile bootstrap over those differences (2000 resamples, seeded). An interval that does not contain 0 is the only kind worth quoting. `tasks` is how many tasks contributed a difference — a task where either side recorded nothing contributes none.

| arm | metric | arm mean | arm Q mean | delta | 95% CI | tasks |
|---|---|---|---|---|---|---|
| P | score | 1.000 | 1.000 | 0.0000 | [0.0000, 0.0000] | 20 |
| P | cost $ | 0.0553 | 0.0608 | -0.00545 | [-0.01063, -0.00043] | 20 |
| P | total tokens | 87469 | 100506 | -13037.2 | [-21574.1, -5327.8] | 20 |
| P | turns | 4.17 | 4.75 | -0.583 | [-0.917, -0.283] | 20 |
| P | cache hit ratio | 0.915 | 0.923 | -0.0074 | [-0.0135, -0.0009] | 20 |
| R | score | 0.994 | 1.000 | -0.0063 | [-0.0187, 0.0000] | 20 |
| R | cost $ | 0.0357 | 0.0608 | -0.02501 | [-0.03528, -0.01242] | 20 |
| R | total tokens | 73578 | 100506 | -26928.2 | [-42234.8, -9714.0] | 20 |
| R | turns | 3.53 | 4.75 | -1.217 | [-1.900, -0.500] | 20 |
| R | cache hit ratio | 0.943 | 0.923 | 0.0200 | [-0.0121, 0.0467] | 20 |

## Correctness by task (mean score over reps)

| task | arm P | arm Q | arm R | best |
|---|---|---|---|---|
| 1 | 1.00 | 1.00 | 1.00 | tie |
| 2 | 1.00 | 1.00 | 1.00 | tie |
| 3 | 1.00 | 1.00 | 1.00 | tie |
| 4 | 1.00 | 1.00 | 1.00 | tie |
| 5 | 1.00 | 1.00 | 1.00 | tie |
| 6 | 1.00 | 1.00 | 1.00 | tie |
| 7 | 1.00 | 1.00 | 1.00 | tie |
| 8 | 1.00 | 1.00 | 1.00 | tie |
| 9 | 1.00 | 1.00 | 1.00 | tie |
| 10 | 1.00 | 1.00 | 1.00 | tie |
| 11 | 1.00 | 1.00 | 1.00 | tie |
| 12 | 1.00 | 1.00 | 1.00 | tie |
| 13 | 1.00 | 1.00 | 1.00 | tie |
| 14 | 1.00 | 1.00 | 1.00 | tie |
| 15 | 1.00 | 1.00 | 1.00 | tie |
| 16 | 1.00 | 1.00 | 0.88 | P |
| 17 | 1.00 | 1.00 | 1.00 | tie |
| 18 | 1.00 | 1.00 | 1.00 | tie |
| 19 | 1.00 | 1.00 | 1.00 | tie |
| 20 | 1.00 | 1.00 | 1.00 | tie |
| **mean** | **1.000** | **1.000** | **0.994** | |

## Sub-1.0 cells

One line per cell that did not score 1.0. `classification` is filled in by hand after reading the cell's stream file: `tool` (the graph answered badly or not at all), `model` (the agent had what it needed and went wrong anyway), or `grader` (the answer is right and the check is wrong).

| task | key | arm | rep | score | missed | verify | outcome | classification |
|---|---|---|---|---|---|---|---|---|
| 16 | assoc-timetravel-4 | R | 1 | 0.88 | set 7/8 | - | success |  |
| 16 | assoc-timetravel-4 | R | 2 | 0.88 | set 7/8 | - | success |  |
| 16 | assoc-timetravel-4 | R | 3 | 0.88 | set 7/8 | - | success |  |

## Tool adoption

Sessions that made at least one MCP call, and which mushroomdb tools they used.

| arm | sessions | >=1 mcp call | adoption | mcp calls total |
|---|---|---|---|---|
| P | 60 | 0 | 0% | 0 |
| Q | 60 | 0 | 0% | 0 |
| R | 60 | 60 | 100% | 152 |

- arm P: no MCP tools used
- arm Q: no MCP tools used
- arm R: `query` x58, `node_edges` x25, `stats` x19, `edges_at` x13, `explain_association` x12, `what_if` x12, `node_info` x11, `edge_history` x2

Tasks where each arm reached for the graph:

- arm P: none
- arm Q: none
- arm R: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]

## Answers

### task 1 rep 1 arm P (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

hospitality
residential
```

### task 1 rep 1 arm Q (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

hospitality
residential
```

### task 1 rep 1 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
LOCATION_FIT
SPECIALTY_MATCH

hospitality
residential
```

### task 1 rep 2 arm P (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

hospitality
residential
```

### task 1 rep 2 arm Q (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

hospitality
residential
```

### task 1 rep 2 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
LOCATION_FIT
SPECIALTY_MATCH

hospitality
residential
```

### task 1 rep 3 arm P (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

hospitality
residential
```

### task 1 rep 3 arm Q (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

hospitality
residential
```

### task 1 rep 3 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
LOCATION_FIT
SPECIALTY_MATCH

hospitality
residential
```

### task 2 rep 1 arm P (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

multi-family
residential
```

### task 2 rep 1 arm Q (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

multi-family
residential
```

### task 2 rep 1 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

multi-family
residential
```

### task 2 rep 2 arm P (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

multi-family
residential
```

### task 2 rep 2 arm Q (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

multi-family
residential
```

### task 2 rep 2 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

multi-family
residential
```

### task 2 rep 3 arm P (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

multi-family
residential
```

### task 2 rep 3 arm Q (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

multi-family
residential
```

### task 2 rep 3 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

multi-family
residential
```

### task 3 rep 1 arm P (score 1.00)

```
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

multi-family
commercial
```

### task 3 rep 1 arm Q (score 1.00)

```
LOCATION_FIT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

Common specialties:
multi-family
commercial
```

### task 3 rep 1 arm R (score 1.00)

```
LOCATION_FIT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

commercial
multi-family
```

### task 3 rep 2 arm P (score 1.00)

```
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

commercial
multi-family
```

### task 3 rep 2 arm Q (score 1.00)

```
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

multi-family
commercial
```

### task 3 rep 2 arm R (score 1.00)

```
LOCATION_FIT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

commercial
multi-family
```

### task 3 rep 3 arm P (score 1.00)

```
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

multi-family
commercial
```

### task 3 rep 3 arm Q (score 1.00)

```
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

multi-family
commercial
```

### task 3 rep 3 arm R (score 1.00)

```
LOCATION_FIT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

commercial
multi-family
```

### task 4 rep 1 arm P (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

civic
institutional
```

### task 4 rep 1 arm Q (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

civic
institutional
```

### task 4 rep 1 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

civic
institutional
```

### task 4 rep 2 arm P (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

civic
institutional
```

### task 4 rep 2 arm Q (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

civic
institutional
```

### task 4 rep 2 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

civic
institutional
```

### task 4 rep 3 arm P (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

civic
institutional
```

### task 4 rep 3 arm Q (score 1.00)

```
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

civic
institutional
```

### task 4 rep 3 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

civic
institutional
```

### task 5 rep 1 arm P (score 1.00)

```
company-000010
company-000013
company-000018
company-000026
company-000039
company-000056
company-000071
company-000098
company-000111
company-000112
company-000127
company-000135
company-000141
company-000159
company-000186
company-000193
company-000230
company-000243
company-000291
company-000320
company-000343
company-000361
company-000373
company-000381
company-000399
company-000405
company-000414
```

### task 5 rep 1 arm Q (score 1.00)

```
company-000010
company-000013
company-000018
company-000026
company-000039
company-000056
company-000071
company-000098
company-000111
company-000112
company-000127
company-000135
company-000141
company-000159
company-000186
company-000193
company-000230
company-000243
company-000291
company-000320
company-000343
company-000361
company-000373
company-000381
company-000399
company-000405
company-000414
```

### task 5 rep 1 arm R (score 1.00)

```
company-000320
company-000098
company-000013
company-000056
company-000039
company-000135
company-000243
company-000141
company-000186
company-000230
company-000193
company-000361
company-000343
company-000071
company-000405
company-000159
company-000414
company-000111
company-000112
company-000291
company-000127
company-000399
company-000381
company-000373
company-000018
company-000010
company-000026
```

### task 5 rep 2 arm P (score 1.00)

```
company-000010
company-000013
company-000018
company-000026
company-000039
company-000056
company-000071
company-000098
company-000111
company-000112
company-000127
company-000135
company-000141
company-000159
company-000186
company-000193
company-000230
company-000243
company-000291
company-000320
company-000343
company-000361
company-000373
company-000381
company-000399
company-000405
company-000414
```

### task 5 rep 2 arm Q (score 1.00)

```
company-000010
company-000013
company-000018
company-000026
company-000039
company-000056
company-000071
company-000098
company-000111
company-000112
company-000127
company-000135
company-000141
company-000159
company-000186
company-000193
company-000230
company-000243
company-000291
company-000320
company-000343
company-000361
company-000373
company-000381
company-000399
company-000405
company-000414
```

### task 5 rep 2 arm R (score 1.00)

```
company-000010
company-000013
company-000018
company-000026
company-000039
company-000056
company-000071
company-000098
company-000111
company-000112
company-000127
company-000135
company-000141
company-000159
company-000186
company-000193
company-000230
company-000243
company-000291
company-000320
company-000343
company-000361
company-000373
company-000381
company-000399
company-000405
company-000414
```

### task 5 rep 3 arm P (score 1.00)

```
company-000010
company-000013
company-000018
company-000026
company-000039
company-000056
company-000071
company-000098
company-000111
company-000112
company-000127
company-000135
company-000141
company-000159
company-000186
company-000193
company-000230
company-000243
company-000291
company-000320
company-000343
company-000361
company-000373
company-000381
company-000399
company-000405
company-000414
```

### task 5 rep 3 arm Q (score 1.00)

```
company-000010
company-000013
company-000018
company-000026
company-000039
company-000056
company-000071
company-000098
company-000111
company-000112
company-000127
company-000135
company-000141
company-000159
company-000186
company-000193
company-000230
company-000243
company-000291
company-000320
company-000343
company-000361
company-000373
company-000381
company-000399
company-000405
company-000414
```

### task 5 rep 3 arm R (score 1.00)

```
company-000010
company-000013
company-000018
company-000026
company-000039
company-000056
company-000071
company-000098
company-000111
company-000112
company-000127
company-000135
company-000141
company-000159
company-000186
company-000193
company-000230
company-000243
company-000291
company-000320
company-000343
company-000361
company-000373
company-000381
company-000399
company-000405
company-000414
```

### task 6 rep 1 arm P (score 1.00)

```
company-000013
company-000039
company-000056
company-000098
company-000135
company-000141
company-000186
company-000230
company-000243
company-000320
```

### task 6 rep 1 arm Q (score 1.00)

```
company-000013
company-000039
company-000056
company-000098
company-000135
company-000141
company-000186
company-000230
company-000243
company-000320
```

### task 6 rep 1 arm R (score 1.00)

```
company-000320
company-000098
company-000013
company-000056
company-000039
company-000135
company-000243
company-000141
company-000186
company-000230
```

### task 6 rep 2 arm P (score 1.00)

```
company-000013
company-000039
company-000056
company-000098
company-000135
company-000141
company-000186
company-000230
company-000243
company-000320
```

### task 6 rep 2 arm Q (score 1.00)

```
company-000013
company-000039
company-000056
company-000098
company-000135
company-000141
company-000186
company-000230
company-000243
company-000320
```

### task 6 rep 2 arm R (score 1.00)

```
company-000320
company-000098
company-000013
company-000056
company-000039
company-000135
company-000243
company-000141
company-000186
company-000230
```

### task 6 rep 3 arm P (score 1.00)

```
company-000013
company-000039
company-000056
company-000098
company-000135
company-000141
company-000186
company-000230
company-000243
company-000320
```

### task 6 rep 3 arm Q (score 1.00)

```
company-000013
company-000039
company-000056
company-000098
company-000135
company-000141
company-000186
company-000230
company-000243
company-000320
```

### task 6 rep 3 arm R (score 1.00)

```
company-000013
company-000056
company-000320
company-000039
company-000135
company-000243
company-000141
company-000186
company-000098
company-000230
```

### task 7 rep 1 arm P (score 1.00)

```
company-000006
company-000010
company-000013
company-000026
company-000098
company-000102
company-000103
company-000112
company-000127
company-000141
company-000193
company-000196
company-000230
company-000265
company-000321
company-000340
company-000343
company-000390
company-000405
company-000410
```

### task 7 rep 1 arm Q (score 1.00)

```
company-000006
company-000010
company-000013
company-000026
company-000098
company-000102
company-000103
company-000112
company-000127
company-000141
company-000193
company-000196
company-000230
company-000265
company-000321
company-000340
company-000343
company-000390
company-000405
company-000410
```

### task 7 rep 1 arm R (score 1.00)

```
company-000006
company-000010
company-000013
company-000026
company-000098
company-000102
company-000103
company-000112
company-000127
company-000141
company-000193
company-000196
company-000230
company-000265
company-000321
company-000340
company-000343
company-000390
company-000405
company-000410
```

### task 7 rep 2 arm P (score 1.00)

```
company-000006
company-000010
company-000013
company-000026
company-000098
company-000102
company-000103
company-000112
company-000127
company-000141
company-000193
company-000196
company-000230
company-000265
company-000321
company-000340
company-000343
company-000390
company-000405
company-000410
```

### task 7 rep 2 arm Q (score 1.00)

```
company-000006
company-000010
company-000013
company-000026
company-000098
company-000102
company-000103
company-000112
company-000127
company-000141
company-000193
company-000196
company-000230
company-000265
company-000321
company-000340
company-000343
company-000390
company-000405
company-000410
```

### task 7 rep 2 arm R (score 1.00)

```
company-000006
company-000010
company-000013
company-000026
company-000098
company-000102
company-000103
company-000112
company-000127
company-000141
company-000193
company-000196
company-000230
company-000265
company-000321
company-000340
company-000343
company-000390
company-000405
company-000410
```

### task 7 rep 3 arm P (score 1.00)

```
company-000006
company-000010
company-000013
company-000026
company-000098
company-000102
company-000103
company-000112
company-000127
company-000141
company-000193
company-000196
company-000230
company-000265
company-000321
company-000340
company-000343
company-000390
company-000405
company-000410
```

### task 7 rep 3 arm Q (score 1.00)

```
company-000006
company-000010
company-000013
company-000026
company-000098
company-000102
company-000103
company-000112
company-000127
company-000141
company-000193
company-000196
company-000230
company-000265
company-000321
company-000340
company-000343
company-000390
company-000405
company-000410
```

### task 7 rep 3 arm R (score 1.00)

```
company-000006
company-000010
company-000013
company-000026
company-000098
company-000102
company-000103
company-000112
company-000127
company-000141
company-000193
company-000196
company-000230
company-000265
company-000321
company-000340
company-000343
company-000390
company-000405
company-000410
```

### task 8 rep 1 arm P (score 1.00)

```
company-000006
company-000013
company-000098
company-000112
company-000141
company-000193
company-000321
company-000343
company-000405
```

### task 8 rep 1 arm Q (score 1.00)

```
company-000006
company-000013
company-000098
company-000112
company-000141
company-000193
company-000321
company-000343
company-000405
```

### task 8 rep 1 arm R (score 1.00)

```
company-000343
company-000141
company-000321
company-000405
company-000013
company-000193
company-000112
company-000098
company-000006
```

### task 8 rep 2 arm P (score 1.00)

```
company-000006
company-000013
company-000098
company-000112
company-000141
company-000193
company-000321
company-000343
company-000405
```

### task 8 rep 2 arm Q (score 1.00)

```
company-000006
company-000013
company-000098
company-000112
company-000141
company-000193
company-000321
company-000343
company-000405
```

### task 8 rep 2 arm R (score 1.00)

```
company-000343
company-000141
company-000321
company-000405
company-000013
company-000193
company-000112
company-000098
company-000006
```

### task 8 rep 3 arm P (score 1.00)

```
company-000006
company-000013
company-000098
company-000112
company-000141
company-000193
company-000321
company-000343
company-000405
```

### task 8 rep 3 arm Q (score 1.00)

```
company-000006
company-000013
company-000098
company-000112
company-000141
company-000193
company-000321
company-000343
company-000405
```

### task 8 rep 3 arm R (score 1.00)

```
company-000006
company-000013
company-000098
company-000112
company-000141
company-000193
company-000321
company-000343
company-000405
```

### task 9 rep 1 arm P (score 1.00)

```
talent-000083
talent-000143
talent-000227
talent-000329
talent-000335
talent-000479
talent-000671
talent-000767
talent-000851
talent-000923
talent-001163
talent-001175
talent-001199
talent-001259
talent-001331
```

### task 9 rep 1 arm Q (score 1.00)

```
talent-000083
talent-000143
talent-000227
talent-000329
talent-000335
talent-000479
talent-000671
talent-000767
talent-000851
talent-000923
talent-001163
talent-001175
talent-001199
talent-001259
talent-001331
```

### task 9 rep 1 arm R (score 1.00)

```
talent-000083
talent-000143
talent-000227
talent-000329
talent-000335
talent-000479
talent-000671
talent-000767
talent-000851
talent-000923
talent-001163
talent-001175
talent-001199
talent-001259
talent-001331
```

### task 9 rep 2 arm P (score 1.00)

```
talent-000083
talent-000143
talent-000227
talent-000329
talent-000335
talent-000479
talent-000671
talent-000767
talent-000851
talent-000923
talent-001163
talent-001175
talent-001199
talent-001259
talent-001331
```

### task 9 rep 2 arm Q (score 1.00)

```
talent-000083
talent-000143
talent-000227
talent-000329
talent-000335
talent-000479
talent-000671
talent-000767
talent-000851
talent-000923
talent-001163
talent-001175
talent-001199
talent-001259
talent-001331
```

### task 9 rep 2 arm R (score 1.00)

```
talent-000083
talent-000143
talent-000227
talent-000329
talent-000335
talent-000479
talent-000671
talent-000767
talent-000851
talent-000923
talent-001163
talent-001175
talent-001199
talent-001259
talent-001331
```

### task 9 rep 3 arm P (score 1.00)

```
talent-000083
talent-000143
talent-000227
talent-000329
talent-000335
talent-000479
talent-000671
talent-000767
talent-000851
talent-000923
talent-001163
talent-001175
talent-001199
talent-001259
talent-001331
```

### task 9 rep 3 arm Q (score 1.00)

```
talent-000083
talent-000143
talent-000227
talent-000329
talent-000335
talent-000479
talent-000671
talent-000767
talent-000851
talent-000923
talent-001163
talent-001175
talent-001199
talent-001259
talent-001331
```

### task 9 rep 3 arm R (score 1.00)

```
talent-000083
talent-000143
talent-000227
talent-000329
talent-000335
talent-000479
talent-000671
talent-000767
talent-000851
talent-000923
talent-001163
talent-001175
talent-001199
talent-001259
talent-001331
```

### task 10 rep 1 arm P (score 1.00)

```
talent-000112
talent-000184
talent-000424
talent-000736
talent-000832
talent-000988
talent-001024
talent-001048
talent-001252
talent-001300
talent-001360
talent-001444
```

### task 10 rep 1 arm Q (score 1.00)

```
talent-000112
talent-000184
talent-000424
talent-000736
talent-000832
talent-000988
talent-001024
talent-001048
talent-001252
talent-001300
talent-001360
talent-001444
```

### task 10 rep 1 arm R (score 1.00)

```
talent-000112
talent-000184
talent-000424
talent-000736
talent-000832
talent-000988
talent-001024
talent-001048
talent-001252
talent-001300
talent-001360
talent-001444
```

### task 10 rep 2 arm P (score 1.00)

```
talent-000112
talent-000184
talent-000424
talent-000736
talent-000832
talent-000988
talent-001024
talent-001048
talent-001252
talent-001300
talent-001360
talent-001444
```

### task 10 rep 2 arm Q (score 1.00)

```
talent-000112
talent-000184
talent-000424
talent-000736
talent-000832
talent-000988
talent-001024
talent-001048
talent-001252
talent-001300
talent-001360
talent-001444
```

### task 10 rep 2 arm R (score 1.00)

```
talent-000112
talent-000184
talent-000424
talent-000736
talent-000832
talent-000988
talent-001024
talent-001048
talent-001252
talent-001300
talent-001360
talent-001444
```

### task 10 rep 3 arm P (score 1.00)

```
talent-000112
talent-000184
talent-000424
talent-000736
talent-000832
talent-000988
talent-001024
talent-001048
talent-001252
talent-001300
talent-001360
talent-001444
```

### task 10 rep 3 arm Q (score 1.00)

```
talent-000112
talent-000184
talent-000424
talent-000736
talent-000832
talent-000988
talent-001024
talent-001048
talent-001252
talent-001300
talent-001360
talent-001444
```

### task 10 rep 3 arm R (score 1.00)

```
talent-000112
talent-000184
talent-000424
talent-000736
talent-000832
talent-000988
talent-001024
talent-001048
talent-001252
talent-001300
talent-001360
talent-001444
```

### task 11 rep 1 arm P (score 1.00)

```
talent-000124
talent-000184
talent-000244
talent-000304
talent-000424
talent-000544
talent-000664
talent-000724
talent-000784
talent-000964
talent-001024
talent-001204
talent-001324
talent-001444
```

### task 11 rep 1 arm Q (score 1.00)

```
talent-000124
talent-000184
talent-000244
talent-000304
talent-000424
talent-000544
talent-000664
talent-000724
talent-000784
talent-000964
talent-001024
talent-001204
talent-001324
talent-001444
```

### task 11 rep 1 arm R (score 1.00)

```
talent-000124
talent-000184
talent-000244
talent-000304
talent-000424
talent-000544
talent-000664
talent-000724
talent-000784
talent-000964
talent-001024
talent-001204
talent-001324
talent-001444
```

### task 11 rep 2 arm P (score 1.00)

```
talent-000124
talent-000184
talent-000244
talent-000304
talent-000424
talent-000544
talent-000664
talent-000724
talent-000784
talent-000964
talent-001024
talent-001204
talent-001324
talent-001444
```

### task 11 rep 2 arm Q (score 1.00)

```
talent-000124
talent-000184
talent-000244
talent-000304
talent-000424
talent-000544
talent-000664
talent-000724
talent-000784
talent-000964
talent-001024
talent-001204
talent-001324
talent-001444
```

### task 11 rep 2 arm R (score 1.00)

```
talent-000124
talent-000184
talent-000244
talent-000304
talent-000424
talent-000544
talent-000664
talent-000724
talent-000784
talent-000964
talent-001024
talent-001204
talent-001324
talent-001444
```

### task 11 rep 3 arm P (score 1.00)

```
talent-000124
talent-000184
talent-000244
talent-000304
talent-000424
talent-000544
talent-000664
talent-000724
talent-000784
talent-000964
talent-001024
talent-001204
talent-001324
talent-001444
```

### task 11 rep 3 arm Q (score 1.00)

```
talent-000124
talent-000184
talent-000244
talent-000304
talent-000424
talent-000544
talent-000664
talent-000724
talent-000784
talent-000964
talent-001024
talent-001204
talent-001324
talent-001444
```

### task 11 rep 3 arm R (score 1.00)

```
talent-000124
talent-000184
talent-000244
talent-000304
talent-000424
talent-000544
talent-000664
talent-000724
talent-000784
talent-000964
talent-001024
talent-001204
talent-001324
talent-001444
```

### task 12 rep 1 arm P (score 1.00)

```
talent-000055
talent-000175
talent-000235
talent-000475
talent-000715
talent-000775
talent-001015
talent-001135
talent-001195
talent-001200
```

### task 12 rep 1 arm Q (score 1.00)

```
talent-000055
talent-000175
talent-000235
talent-000475
talent-000715
talent-000775
talent-001015
talent-001135
talent-001195
talent-001200
```

### task 12 rep 1 arm R (score 1.00)

```
talent-000055
talent-000175
talent-000235
talent-000475
talent-000715
talent-000775
talent-001015
talent-001135
talent-001195
talent-001200
```

### task 12 rep 2 arm P (score 1.00)

```
talent-000055
talent-000175
talent-000235
talent-000475
talent-000715
talent-000775
talent-001015
talent-001135
talent-001195
talent-001200
```

### task 12 rep 2 arm Q (score 1.00)

```
talent-000055
talent-000175
talent-000235
talent-000475
talent-000715
talent-000775
talent-001015
talent-001135
talent-001195
talent-001200
```

### task 12 rep 2 arm R (score 1.00)

```
talent-000055
talent-000175
talent-000235
talent-000475
talent-000715
talent-000775
talent-001015
talent-001135
talent-001195
talent-001200
```

### task 12 rep 3 arm P (score 1.00)

```
talent-000055
talent-000175
talent-000235
talent-000475
talent-000715
talent-000775
talent-001015
talent-001135
talent-001195
talent-001200
```

### task 12 rep 3 arm Q (score 1.00)

```
talent-000055
talent-000175
talent-000235
talent-000475
talent-000715
talent-000775
talent-001015
talent-001135
talent-001195
talent-001200
```

### task 12 rep 3 arm R (score 1.00)

```
talent-000055
talent-000175
talent-000235
talent-000475
talent-000715
talent-000775
talent-001015
talent-001135
talent-001195
talent-001200
```

### task 13 rep 1 arm P (score 1.00)

```
company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 13 rep 1 arm Q (score 1.00)

```
company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 13 rep 1 arm R (score 1.00)

```
company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 13 rep 2 arm P (score 1.00)

```
company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 13 rep 2 arm Q (score 1.00)

```
company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 13 rep 2 arm R (score 1.00)

```
company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 13 rep 3 arm P (score 1.00)

```
company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 13 rep 3 arm Q (score 1.00)

```
company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 13 rep 3 arm R (score 1.00)

```
company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 14 rep 1 arm P (score 1.00)

```
company-000042
company-000066
company-000138
company-000246
company-000354
company-000402
```

### task 14 rep 1 arm Q (score 1.00)

```
company-000042
company-000066
company-000138
company-000246
company-000354
company-000402
```

### task 14 rep 1 arm R (score 1.00)

```
company-000042
company-000066
company-000138
company-000246
company-000354
company-000402
```

### task 14 rep 2 arm P (score 1.00)

```
company-000042
company-000066
company-000138
company-000246
company-000354
company-000402
```

### task 14 rep 2 arm Q (score 1.00)

```
company-000042
company-000066
company-000138
company-000246
company-000354
company-000402
```

### task 14 rep 2 arm R (score 1.00)

```
company-000042
company-000066
company-000138
company-000246
company-000354
company-000402
```

### task 14 rep 3 arm P (score 1.00)

```
company-000042
company-000066
company-000138
company-000246
company-000354
company-000402
```

### task 14 rep 3 arm Q (score 1.00)

```
company-000042
company-000066
company-000138
company-000246
company-000354
company-000402
```

### task 14 rep 3 arm R (score 1.00)

```
company-000042
company-000066
company-000138
company-000246
company-000354
company-000402
```

### task 15 rep 1 arm P (score 1.00)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
company-000401
```

### task 15 rep 1 arm Q (score 1.00)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
company-000401
```

### task 15 rep 1 arm R (score 1.00)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
company-000401
```

### task 15 rep 2 arm P (score 1.00)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
company-000401
```

### task 15 rep 2 arm Q (score 1.00)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
company-000401
```

### task 15 rep 2 arm R (score 1.00)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
company-000401
```

### task 15 rep 3 arm P (score 1.00)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
company-000401
```

### task 15 rep 3 arm Q (score 1.00)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
company-000401
```

### task 15 rep 3 arm R (score 1.00)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
company-000401
```

### task 16 rep 1 arm P (score 1.00)

```
company-000048
company-000120
company-000132
company-000192
company-000288
company-000294
company-000312
company-000360
```

### task 16 rep 1 arm Q (score 1.00)

```
company-000048
company-000120
company-000132
company-000192
company-000288
company-000294
company-000312
company-000360
```

### task 16 rep 1 arm R (score 0.88)

```
company-000048
company-000120
company-000132
company-000192
company-000288
company-000312
company-000360
```
missed: set 7/8

### task 16 rep 2 arm P (score 1.00)

```
company-000048
company-000120
company-000132
company-000192
company-000288
company-000294
company-000312
company-000360
```

### task 16 rep 2 arm Q (score 1.00)

```
company-000048
company-000120
company-000132
company-000192
company-000288
company-000294
company-000312
company-000360
```

### task 16 rep 2 arm R (score 0.88)

```
company-000048
company-000120
company-000132
company-000192
company-000288
company-000312
company-000360
```
missed: set 7/8

### task 16 rep 3 arm P (score 1.00)

```
company-000048
company-000120
company-000132
company-000192
company-000288
company-000294
company-000312
company-000360
```

### task 16 rep 3 arm Q (score 1.00)

```
company-000048
company-000120
company-000132
company-000192
company-000288
company-000294
company-000312
company-000360
```

### task 16 rep 3 arm R (score 0.88)

```
company-000048
company-000120
company-000132
company-000192
company-000288
company-000312
company-000360
```
missed: set 7/8

### task 17 rep 1 arm P (score 1.00)

```
job-000003
job-000031
job-000040
job-000041
job-000051
job-000052
job-000057
job-000063
job-000064
job-000066
job-000076
job-000078
job-000080
job-000093
job-000102
job-000108
job-000115
job-000117
job-000128
job-000142
job-000150
job-000154
job-000163
job-000180
job-000184
job-000188
job-000193
job-000206
```

### task 17 rep 1 arm Q (score 1.00)

```
job-000003
job-000031
job-000040
job-000041
job-000051
job-000052
job-000057
job-000063
job-000064
job-000066
job-000076
job-000078
job-000080
job-000093
job-000102
job-000108
job-000115
job-000117
job-000128
job-000142
job-000150
job-000154
job-000163
job-000180
job-000184
job-000188
job-000193
job-000206
```

### task 17 rep 1 arm R (score 1.00)

```
job-000003
job-000031
job-000040
job-000041
job-000051
job-000052
job-000057
job-000063
job-000064
job-000066
job-000076
job-000078
job-000080
job-000093
job-000102
job-000108
job-000115
job-000117
job-000128
job-000142
job-000150
job-000154
job-000163
job-000180
job-000184
job-000188
job-000193
job-000206
```

### task 17 rep 2 arm P (score 1.00)

```
job-000003
job-000031
job-000040
job-000041
job-000051
job-000052
job-000057
job-000063
job-000064
job-000066
job-000076
job-000078
job-000080
job-000093
job-000102
job-000108
job-000115
job-000117
job-000128
job-000142
job-000150
job-000154
job-000163
job-000180
job-000184
job-000188
job-000193
job-000206
```

### task 17 rep 2 arm Q (score 1.00)

```
job-000003
job-000031
job-000040
job-000041
job-000051
job-000052
job-000057
job-000063
job-000064
job-000066
job-000076
job-000078
job-000080
job-000093
job-000102
job-000108
job-000115
job-000117
job-000128
job-000142
job-000150
job-000154
job-000163
job-000180
job-000184
job-000188
job-000193
job-000206
```

### task 17 rep 2 arm R (score 1.00)

```
job-000003
job-000031
job-000040
job-000041
job-000051
job-000052
job-000057
job-000063
job-000064
job-000066
job-000076
job-000078
job-000080
job-000093
job-000102
job-000108
job-000115
job-000117
job-000128
job-000142
job-000150
job-000154
job-000163
job-000180
job-000184
job-000188
job-000193
job-000206
```

### task 17 rep 3 arm P (score 1.00)

```
job-000003
job-000031
job-000040
job-000041
job-000051
job-000052
job-000057
job-000063
job-000064
job-000066
job-000076
job-000078
job-000080
job-000093
job-000102
job-000108
job-000115
job-000117
job-000128
job-000142
job-000150
job-000154
job-000163
job-000180
job-000184
job-000188
job-000193
job-000206
```

### task 17 rep 3 arm Q (score 1.00)

```
job-000003
job-000031
job-000040
job-000041
job-000051
job-000052
job-000057
job-000063
job-000064
job-000066
job-000076
job-000078
job-000080
job-000093
job-000102
job-000108
job-000115
job-000117
job-000128
job-000142
job-000150
job-000154
job-000163
job-000180
job-000184
job-000188
job-000193
job-000206
```

### task 17 rep 3 arm R (score 1.00)

```
job-000003
job-000031
job-000040
job-000041
job-000051
job-000052
job-000057
job-000063
job-000064
job-000066
job-000076
job-000078
job-000080
job-000093
job-000102
job-000108
job-000115
job-000117
job-000128
job-000142
job-000150
job-000154
job-000163
job-000180
job-000184
job-000188
job-000193
job-000206
```

### task 18 rep 1 arm P (score 1.00)

```
job-000002
job-000009
job-000014
job-000015
job-000017
job-000019
job-000034
job-000037
job-000049
job-000070
job-000086
job-000090
job-000099
job-000104
job-000105
job-000106
job-000112
job-000125
job-000126
job-000130
job-000135
job-000138
job-000139
job-000143
job-000149
job-000152
job-000158
job-000164
job-000165
job-000169
job-000176
job-000177
job-000178
job-000199
job-000203
```

### task 18 rep 1 arm Q (score 1.00)

```
job-000002
job-000009
job-000014
job-000015
job-000017
job-000019
job-000034
job-000037
job-000049
job-000070
job-000086
job-000090
job-000099
job-000104
job-000105
job-000106
job-000112
job-000125
job-000126
job-000130
job-000135
job-000138
job-000139
job-000143
job-000149
job-000152
job-000158
job-000164
job-000165
job-000169
job-000176
job-000177
job-000178
job-000199
job-000203
```

### task 18 rep 1 arm R (score 1.00)

```
job-000002
job-000009
job-000014
job-000015
job-000017
job-000019
job-000034
job-000037
job-000049
job-000070
job-000086
job-000090
job-000099
job-000104
job-000105
job-000106
job-000112
job-000125
job-000126
job-000130
job-000135
job-000138
job-000139
job-000143
job-000149
job-000152
job-000158
job-000164
job-000165
job-000169
job-000176
job-000177
job-000178
job-000199
job-000203
```

### task 18 rep 2 arm P (score 1.00)

```
job-000002
job-000009
job-000014
job-000015
job-000017
job-000019
job-000034
job-000037
job-000049
job-000070
job-000086
job-000090
job-000099
job-000104
job-000105
job-000106
job-000112
job-000125
job-000126
job-000130
job-000135
job-000138
job-000139
job-000143
job-000149
job-000152
job-000158
job-000164
job-000165
job-000169
job-000176
job-000177
job-000178
job-000199
job-000203
```

### task 18 rep 2 arm Q (score 1.00)

```
job-000002
job-000009
job-000014
job-000015
job-000017
job-000019
job-000034
job-000037
job-000049
job-000070
job-000086
job-000090
job-000099
job-000104
job-000105
job-000106
job-000112
job-000125
job-000126
job-000130
job-000135
job-000138
job-000139
job-000143
job-000149
job-000152
job-000158
job-000164
job-000165
job-000169
job-000176
job-000177
job-000178
job-000199
job-000203
```

### task 18 rep 2 arm R (score 1.00)

```
job-000002
job-000009
job-000014
job-000015
job-000017
job-000019
job-000034
job-000037
job-000049
job-000070
job-000086
job-000090
job-000099
job-000104
job-000105
job-000106
job-000112
job-000125
job-000126
job-000130
job-000135
job-000138
job-000139
job-000143
job-000149
job-000152
job-000158
job-000164
job-000165
job-000169
job-000176
job-000177
job-000178
job-000199
job-000203
```

### task 18 rep 3 arm P (score 1.00)

```
job-000002
job-000009
job-000014
job-000015
job-000017
job-000019
job-000034
job-000037
job-000049
job-000070
job-000086
job-000090
job-000099
job-000104
job-000105
job-000106
job-000112
job-000125
job-000126
job-000130
job-000135
job-000138
job-000139
job-000143
job-000149
job-000152
job-000158
job-000164
job-000165
job-000169
job-000176
job-000177
job-000178
job-000199
job-000203
```

### task 18 rep 3 arm Q (score 1.00)

```
job-000002
job-000009
job-000014
job-000015
job-000017
job-000019
job-000034
job-000037
job-000049
job-000070
job-000086
job-000090
job-000099
job-000104
job-000105
job-000106
job-000112
job-000125
job-000126
job-000130
job-000135
job-000138
job-000139
job-000143
job-000149
job-000152
job-000158
job-000164
job-000165
job-000169
job-000176
job-000177
job-000178
job-000199
job-000203
```

### task 18 rep 3 arm R (score 1.00)

```
job-000002
job-000009
job-000014
job-000015
job-000017
job-000019
job-000034
job-000037
job-000049
job-000070
job-000086
job-000090
job-000099
job-000104
job-000105
job-000106
job-000112
job-000125
job-000126
job-000130
job-000135
job-000138
job-000139
job-000143
job-000149
job-000152
job-000158
job-000164
job-000165
job-000169
job-000176
job-000177
job-000178
job-000199
job-000203
```

### task 19 rep 1 arm P (score 1.00)

```
job-000045
job-000069
job-000153
job-000189
```

### task 19 rep 1 arm Q (score 1.00)

```
job-000045
job-000069
job-000153
job-000189
```

### task 19 rep 1 arm R (score 1.00)

```
job-000045
job-000069
job-000153
job-000189
```

### task 19 rep 2 arm P (score 1.00)

```
job-000045
job-000069
job-000153
job-000189
```

### task 19 rep 2 arm Q (score 1.00)

```
job-000045
job-000069
job-000153
job-000189
```

### task 19 rep 2 arm R (score 1.00)

```
job-000045
job-000069
job-000153
job-000189
```

### task 19 rep 3 arm P (score 1.00)

```
job-000045
job-000069
job-000153
job-000189
```

### task 19 rep 3 arm Q (score 1.00)

```
job-000045
job-000069
job-000153
job-000189
```

### task 19 rep 3 arm R (score 1.00)

```
job-000045
job-000069
job-000153
job-000189
```

### task 20 rep 1 arm P (score 1.00)

```
job-000040
job-000046
job-000052
job-000124
job-000155
```

### task 20 rep 1 arm Q (score 1.00)

```
job-000040
job-000046
job-000052
job-000124
job-000155
```

### task 20 rep 1 arm R (score 1.00)

```
job-000040
job-000046
job-000052
job-000124
job-000155
```

### task 20 rep 2 arm P (score 1.00)

```
job-000040
job-000046
job-000052
job-000124
job-000155
```

### task 20 rep 2 arm Q (score 1.00)

```
job-000040
job-000046
job-000052
job-000124
job-000155
```

### task 20 rep 2 arm R (score 1.00)

```
job-000040
job-000046
job-000052
job-000124
job-000155
```

### task 20 rep 3 arm P (score 1.00)

```
job-000040
job-000046
job-000052
job-000124
job-000155
```

### task 20 rep 3 arm Q (score 1.00)

```
job-000040
job-000046
job-000052
job-000124
job-000155
```

### task 20 rep 3 arm R (score 1.00)

```
job-000040
job-000046
job-000052
job-000124
job-000155
```

