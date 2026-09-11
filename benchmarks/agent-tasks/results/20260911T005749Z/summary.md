# Agent benchmark run

- run: `20260911T005749Z`
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

- R: correctness -0.191 vs arm Q, paired over 20 task(s)
- R: correctness -0.191 is below arm(s) P (+0.013)
- R: cost 0.5089 > arm Q 0.1257
- R: cost interval [+0.2522, +0.5253] vs arm Q is not entirely below zero
- R: max-turns on tasks [5, 8, 14, 15, 16] where arm Q succeeded

## Per cell

| task | key | arm | rep | in | out | cache read | cache create | total tok | cache hit | tools | mcp | graph | tool search | turns | sec | cost $ | score | extra | denials | verify | dirtied | outcome |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | assoc-why-1 | P | 1 | 12 | 2383 | 182342 | 15079 | 199816 | 0.924 | 6 | 0 | 0 | 0 | 7 | 47.5 | 0.1217 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 1 | 12 | 1993 | 183383 | 15549 | 200937 | 0.922 | 6 | 0 | 0 | 0 | 7 | 32.9 | 0.1199 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 1 | 10 | 494 | 142096 | 40323 | 182923 | 0.779 | 5 | 3 | 3 | 2 | 6 | 80.5 | 0.1947 | 0.25 | 3 | 0 | - | - | success |
| 1 | assoc-why-1 | P | 2 | 12 | 2049 | 187606 | 9641 | 199308 | 0.951 | 6 | 0 | 0 | 0 | 7 | 29.1 | 0.0977 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 2 | 10 | 2171 | 154047 | 10002 | 166230 | 0.939 | 5 | 0 | 0 | 0 | 6 | 233.8 | 0.0936 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 2 | 8 | 457 | 116682 | 34728 | 151875 | 0.771 | 4 | 3 | 3 | 1 | 5 | 22.9 | 0.1668 | 0.25 | 3 | 0 | - | - | success |
| 1 | assoc-why-1 | P | 3 | 12 | 2311 | 186512 | 9578 | 198413 | 0.951 | 5 | 0 | 0 | 0 | 6 | 22.8 | 0.0998 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 3 | 10 | 2141 | 154027 | 9885 | 166063 | 0.940 | 5 | 0 | 0 | 0 | 6 | 31.6 | 0.0929 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 3 | 10 | 543 | 147570 | 34969 | 183092 | 0.808 | 5 | 3 | 3 | 2 | 6 | 23.4 | 0.1748 | 0.25 | 3 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 1 | 10 | 1339 | 152727 | 8244 | 162320 | 0.949 | 4 | 0 | 0 | 0 | 5 | 50.6 | 0.0780 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 1 | 8 | 1451 | 119374 | 8815 | 129648 | 0.931 | 5 | 0 | 0 | 0 | 6 | 58.5 | 0.0748 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 1 | 8 | 456 | 116893 | 34928 | 152285 | 0.770 | 4 | 3 | 3 | 1 | 5 | 44.9 | 0.1677 | 0.25 | 3 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 2 | 10 | 1631 | 153339 | 8657 | 163637 | 0.947 | 5 | 0 | 0 | 0 | 6 | 17.3 | 0.0827 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 2 | 8 | 1632 | 119414 | 8759 | 129813 | 0.932 | 4 | 0 | 0 | 0 | 5 | 23.0 | 0.0764 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 2 | 6 | 504 | 86163 | 34903 | 121576 | 0.712 | 4 | 3 | 3 | 1 | 5 | 68.1 | 0.1619 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 3 | 10 | 1711 | 153250 | 8527 | 163498 | 0.947 | 5 | 0 | 0 | 0 | 6 | 23.4 | 0.0830 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 3 | 8 | 1627 | 119356 | 8744 | 129735 | 0.932 | 4 | 0 | 0 | 0 | 5 | 18.5 | 0.0762 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 3 | 10 | 534 | 147556 | 35057 | 183157 | 0.808 | 5 | 3 | 3 | 2 | 6 | 19.3 | 0.1751 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 1 | 14 | 1843 | 222639 | 10117 | 234613 | 0.957 | 7 | 0 | 0 | 0 | 8 | 74.5 | 0.1046 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 1 | 10 | 2065 | 154209 | 10015 | 166299 | 0.939 | 5 | 0 | 0 | 0 | 6 | 30.9 | 0.0927 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 1 | 10 | 551 | 147594 | 35026 | 183181 | 0.808 | 5 | 3 | 3 | 2 | 6 | 24.7 | 0.1752 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 2 | 12 | 1658 | 187679 | 9420 | 198769 | 0.952 | 6 | 0 | 0 | 0 | 7 | 80.7 | 0.0929 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 2 | 10 | 2048 | 154006 | 10069 | 166133 | 0.939 | 5 | 0 | 0 | 0 | 6 | 108.0 | 0.0927 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 2 | 8 | 466 | 116897 | 34859 | 152230 | 0.770 | 4 | 3 | 3 | 1 | 5 | 27.3 | 0.1675 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 3 | 12 | 2381 | 187623 | 9833 | 199849 | 0.950 | 6 | 0 | 0 | 0 | 7 | 35.3 | 0.1018 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 3 | 10 | 1636 | 153994 | 9790 | 165430 | 0.940 | 5 | 0 | 0 | 0 | 6 | 22.8 | 0.0874 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 3 | 10 | 559 | 147624 | 35034 | 183227 | 0.808 | 5 | 3 | 3 | 2 | 6 | 23.9 | 0.1753 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 1 | 12 | 1739 | 186800 | 8945 | 197496 | 0.954 | 5 | 0 | 0 | 0 | 6 | 59.0 | 0.0916 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 1 | 10 | 1850 | 153885 | 9651 | 165396 | 0.941 | 5 | 0 | 0 | 0 | 6 | 40.9 | 0.0890 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 1 | 10 | 544 | 147589 | 34983 | 183126 | 0.808 | 5 | 3 | 3 | 2 | 6 | 20.2 | 0.1749 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 2 | 12 | 2126 | 187613 | 9754 | 199505 | 0.951 | 6 | 0 | 0 | 0 | 7 | 27.2 | 0.0989 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 2 | 12 | 2085 | 189059 | 10125 | 201281 | 0.949 | 6 | 0 | 0 | 0 | 7 | 28.7 | 0.1003 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 2 | 10 | 538 | 147583 | 34977 | 183108 | 0.808 | 5 | 3 | 3 | 2 | 6 | 24.6 | 0.1748 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 3 | 10 | 1791 | 154086 | 9706 | 165593 | 0.941 | 5 | 0 | 0 | 0 | 6 | 22.0 | 0.0887 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 3 | 12 | 1889 | 188416 | 9967 | 200284 | 0.950 | 6 | 0 | 0 | 0 | 7 | 18.4 | 0.0976 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 3 | 10 | 502 | 147589 | 34975 | 183076 | 0.808 | 5 | 3 | 3 | 2 | 6 | 19.1 | 0.1745 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 1 | 12 | 2428 | 188022 | 10399 | 200861 | 0.948 | 6 | 0 | 0 | 0 | 7 | 69.1 | 0.1046 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 1 | 14 | 4859 | 228430 | 13673 | 246976 | 0.944 | 7 | 0 | 0 | 0 | 8 | 98.7 | 0.1501 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 1 | 30 | 3713 | 522481 | 13124 | 539348 | 0.975 | 15 | 10 | 10 | 1 | 16 | 231.4 | 0.1942 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 2 | 10 | 2142 | 155547 | 11497 | 169196 | 0.931 | 5 | 0 | 0 | 0 | 6 | 45.0 | 0.0997 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 2 | 14 | 3223 | 226530 | 11265 | 241032 | 0.953 | 7 | 0 | 0 | 0 | 8 | 133.4 | 0.1237 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 2 | 60 | 10099 | 1436972 | 39777 | 1486908 | 0.973 | 32 | 22 | 22 | 2 | 31 | 141.4 | 0.5476 | 0.00 | 0 | 0 | - | - | error_max_turns |
| 5 | assoc-multihop-1 | P | 3 | 12 | 2053 | 189687 | 11310 | 203062 | 0.944 | 6 | 0 | 0 | 0 | 7 | 27.5 | 0.1048 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 3 | 8 | 2343 | 119486 | 10174 | 132011 | 0.921 | 4 | 0 | 0 | 0 | 5 | 20.7 | 0.0891 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 3 | 50 | 6607 | 1151205 | 38940 | 1196802 | 0.967 | 32 | 25 | 25 | 2 | 33 | 127.9 | 0.4522 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 1 | 12 | 2001 | 189219 | 9954 | 201186 | 0.950 | 6 | 0 | 0 | 0 | 7 | 31.6 | 0.0988 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 1 | 8 | 2347 | 119501 | 10151 | 132007 | 0.922 | 4 | 0 | 0 | 0 | 5 | 34.7 | 0.0891 | 0.80 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 1 | 52 | 8068 | 986342 | 19306 | 1013768 | 0.981 | 26 | 21 | 21 | 1 | 27 | 369.4 | 0.3553 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 2 | 10 | 1834 | 153708 | 10024 | 165576 | 0.939 | 5 | 0 | 0 | 0 | 6 | 29.4 | 0.0903 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 2 | 14 | 3921 | 226122 | 11822 | 241879 | 0.950 | 7 | 0 | 0 | 0 | 8 | 34.5 | 0.1328 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 2 | 38 | 7095 | 736253 | 27724 | 771110 | 0.964 | 23 | 20 | 20 | 1 | 24 | 104.9 | 0.3292 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 3 | 10 | 2132 | 154569 | 10245 | 166956 | 0.938 | 5 | 0 | 0 | 0 | 6 | 20.5 | 0.0943 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 3 | 14 | 4370 | 226079 | 12348 | 242811 | 0.948 | 7 | 0 | 0 | 0 | 8 | 44.1 | 0.1394 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 3 | 34 | 4766 | 566697 | 12174 | 583671 | 0.979 | 16 | 13 | 13 | 1 | 17 | 79.0 | 0.2098 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 1 | 10 | 2414 | 155341 | 11342 | 169107 | 0.932 | 5 | 0 | 0 | 0 | 6 | 38.1 | 0.1017 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 1 | 16 | 3879 | 262094 | 12540 | 278529 | 0.954 | 8 | 0 | 0 | 0 | 9 | 107.4 | 0.1425 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 1 | 30 | 4763 | 563885 | 18821 | 587499 | 0.968 | 15 | 10 | 10 | 1 | 16 | 247.6 | 0.2358 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 2 | 12 | 2627 | 188098 | 10201 | 200938 | 0.949 | 6 | 0 | 0 | 0 | 7 | 36.6 | 0.1058 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 2 | 14 | 4874 | 229394 | 13836 | 248118 | 0.943 | 7 | 0 | 0 | 0 | 8 | 145.5 | 0.1511 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 2 | 34 | 6197 | 619718 | 34511 | 660460 | 0.947 | 25 | 21 | 21 | 1 | 26 | 82.9 | 0.3240 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 3 | 12 | 2342 | 188269 | 10063 | 200686 | 0.949 | 6 | 0 | 0 | 0 | 7 | 31.3 | 0.1024 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 3 | 12 | 3111 | 190317 | 12088 | 205528 | 0.940 | 6 | 0 | 0 | 0 | 7 | 32.7 | 0.1186 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 3 | 32 | 7799 | 861467 | 60992 | 930290 | 0.934 | 26 | 21 | 21 | 1 | 27 | 115.5 | 0.4943 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 1 | 10 | 1529 | 152233 | 9322 | 163094 | 0.942 | 5 | 0 | 0 | 0 | 6 | 110.4 | 0.0842 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 1 | 12 | 4368 | 191313 | 12868 | 208561 | 0.937 | 6 | 0 | 0 | 0 | 7 | 86.7 | 0.1345 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 1 | 40 | 9457 | 884317 | 53109 | 946923 | 0.943 | 28 | 25 | 25 | 1 | 29 | 123.1 | 0.4839 | 0.00 | 5 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 2 | 18 | 6262 | 313442 | 16912 | 336634 | 0.949 | 9 | 0 | 0 | 0 | 10 | 145.6 | 0.1941 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 2 | 16 | 3901 | 262863 | 12158 | 278938 | 0.956 | 8 | 0 | 0 | 0 | 9 | 49.0 | 0.1414 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 2 | 36 | 4767 | 727217 | 30023 | 762043 | 0.960 | 20 | 17 | 17 | 1 | 21 | 65.9 | 0.3133 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 3 | 12 | 1763 | 189499 | 11216 | 202490 | 0.944 | 6 | 0 | 0 | 0 | 7 | 17.1 | 0.1015 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 3 | 16 | 4771 | 266273 | 14431 | 285491 | 0.949 | 8 | 0 | 0 | 0 | 9 | 53.2 | 0.1598 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 3 | 60 | 10834 | 1194448 | 33324 | 1238666 | 0.973 | 30 | 24 | 24 | 1 | 31 | 172.5 | 0.4806 | 0.00 | 0 | 0 | - | - | error_max_turns |
| 9 | assoc-retraction-1 | P | 1 | 10 | 2448 | 154310 | 10339 | 167107 | 0.937 | 5 | 0 | 0 | 0 | 6 | 69.1 | 0.0978 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 1 | 12 | 3611 | 192616 | 12338 | 208577 | 0.940 | 6 | 0 | 0 | 0 | 7 | 81.3 | 0.1251 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 1 | 34 | 6645 | 803907 | 30859 | 841445 | 0.963 | 19 | 15 | 15 | 1 | 20 | 171.0 | 0.3507 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 2 | 8 | 1826 | 119329 | 9586 | 130749 | 0.926 | 4 | 0 | 0 | 0 | 5 | 25.2 | 0.0816 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 2 | 12 | 4311 | 190853 | 12808 | 207984 | 0.937 | 6 | 0 | 0 | 0 | 7 | 40.0 | 0.1336 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 2 | 40 | 8763 | 1090597 | 49783 | 1149183 | 0.956 | 23 | 15 | 15 | 1 | 24 | 112.1 | 0.5050 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 3 | 10 | 2219 | 154888 | 11413 | 168530 | 0.931 | 5 | 0 | 0 | 0 | 6 | 35.6 | 0.1000 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 3 | 10 | 3274 | 154331 | 11162 | 168777 | 0.932 | 6 | 0 | 0 | 0 | 7 | 38.0 | 0.1094 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 3 | 30 | 9051 | 712922 | 344444 | 1066447 | 0.674 | 38 | 32 | 32 | 1 | 39 | 126.6 | 1.6109 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 1 | 12 | 2234 | 188671 | 10443 | 201360 | 0.948 | 6 | 0 | 0 | 0 | 7 | 55.4 | 0.1030 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 1 | 12 | 3523 | 192749 | 12126 | 208410 | 0.941 | 6 | 0 | 0 | 0 | 7 | 102.4 | 0.1234 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 1 | 48 | 18730 | 1724276 | 90114 | 1833168 | 0.950 | 26 | 24 | 24 | 2 | 27 | 476.1 | 0.8927 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 2 | 12 | 2687 | 187807 | 10288 | 200794 | 0.948 | 6 | 0 | 0 | 0 | 7 | 36.7 | 0.1067 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 2 | 12 | 5090 | 190868 | 12583 | 208553 | 0.938 | 6 | 0 | 0 | 0 | 7 | 55.4 | 0.1406 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 2 | 42 | 11894 | 1356757 | 377797 | 1746490 | 0.782 | 45 | 39 | 39 | 1 | 46 | 184.3 | 1.9016 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 3 | 10 | 2003 | 153979 | 10569 | 166561 | 0.936 | 5 | 0 | 0 | 0 | 6 | 30.3 | 0.0942 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 3 | 10 | 3420 | 155011 | 12195 | 170636 | 0.927 | 5 | 0 | 0 | 0 | 6 | 28.5 | 0.1151 | 0.92 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 3 | 18 | 7815 | 411899 | 370852 | 790584 | 0.526 | 31 | 27 | 27 | 1 | 32 | 97.8 | 1.6440 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 1 | 14 | 3642 | 225962 | 12410 | 242028 | 0.948 | 7 | 0 | 0 | 0 | 8 | 66.9 | 0.1324 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 1 | 14 | 3392 | 225428 | 11620 | 240454 | 0.951 | 7 | 0 | 0 | 0 | 8 | 55.7 | 0.1266 | 0.93 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 1 | 36 | 5395 | 841106 | 28653 | 875190 | 0.967 | 25 | 24 | 24 | 1 | 26 | 94.3 | 0.3369 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 2 | 14 | 3607 | 226799 | 12583 | 243003 | 0.947 | 6 | 0 | 0 | 0 | 7 | 50.0 | 0.1329 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 2 | 28 | 5859 | 503298 | 17271 | 526456 | 0.967 | 14 | 0 | 0 | 0 | 15 | 84.7 | 0.2295 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 2 | 42 | 19032 | 1298036 | 71650 | 1388760 | 0.948 | 24 | 22 | 22 | 1 | 25 | 248.1 | 0.7366 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 3 | 14 | 2512 | 224843 | 11442 | 238811 | 0.952 | 7 | 0 | 0 | 0 | 8 | 38.0 | 0.1170 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 3 | 18 | 4419 | 297399 | 12875 | 314711 | 0.958 | 9 | 0 | 0 | 0 | 10 | 53.2 | 0.1563 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 3 | 38 | 6462 | 1107166 | 60192 | 1173858 | 0.948 | 19 | 18 | 18 | 1 | 20 | 105.4 | 0.5269 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 1 | 12 | 3106 | 189543 | 11108 | 203769 | 0.945 | 6 | 0 | 0 | 0 | 7 | 147.6 | 0.1145 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 1 | 10 | 3275 | 155151 | 11778 | 170214 | 0.929 | 5 | 0 | 0 | 0 | 6 | 40.8 | 0.1120 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 1 | 36 | 13009 | 879510 | 49586 | 942141 | 0.947 | 23 | 16 | 16 | 1 | 24 | 436.1 | 0.5044 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 2 | 12 | 3069 | 188455 | 10399 | 201935 | 0.948 | 6 | 0 | 0 | 0 | 7 | 30.1 | 0.1111 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 2 | 14 | 4015 | 228511 | 12679 | 245219 | 0.947 | 7 | 0 | 0 | 0 | 8 | 63.7 | 0.1377 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 2 | 36 | 5380 | 858983 | 42614 | 907013 | 0.953 | 18 | 13 | 13 | 1 | 19 | 99.4 | 0.3961 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 3 | 12 | 2826 | 187924 | 10418 | 201180 | 0.947 | 6 | 0 | 0 | 0 | 7 | 32.1 | 0.1086 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 3 | 16 | 4386 | 262729 | 13161 | 280292 | 0.952 | 8 | 0 | 0 | 0 | 9 | 54.4 | 0.1502 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 3 | 46 | 26095 | 1396305 | 65469 | 1487915 | 0.955 | 26 | 24 | 24 | 2 | 27 | 256.2 | 0.8022 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 1 | 10 | 1628 | 152576 | 9048 | 163262 | 0.944 | 4 | 0 | 0 | 0 | 5 | 24.7 | 0.0841 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 1 | 12 | 3752 | 190091 | 11963 | 205818 | 0.941 | 6 | 0 | 0 | 0 | 7 | 147.2 | 0.1245 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 1 | 36 | 21331 | 893298 | 91094 | 1005759 | 0.907 | 36 | 24 | 24 | 3 | 38 | 318.2 | 0.7564 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 2 | 10 | 1547 | 152106 | 9334 | 162997 | 0.942 | 5 | 0 | 0 | 0 | 6 | 17.9 | 0.0843 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 2 | 14 | 4539 | 229166 | 12988 | 246707 | 0.946 | 7 | 0 | 0 | 0 | 8 | 46.8 | 0.1443 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 2 | 56 | 32615 | 1734330 | 92236 | 1859237 | 0.950 | 131 | 119 | 119 | 2 | 133 | 384.1 | 1.0421 | 0.88 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 3 | 10 | 2230 | 155134 | 10609 | 167983 | 0.936 | 5 | 0 | 0 | 0 | 6 | 26.7 | 0.0969 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 3 | 16 | 3276 | 254709 | 11641 | 269642 | 0.956 | 7 | 0 | 0 | 0 | 8 | 38.4 | 0.1314 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 3 | 46 | 17322 | 1120583 | 70358 | 1208309 | 0.941 | 55 | 43 | 43 | 2 | 57 | 265.6 | 0.6789 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 1 | 8 | 1802 | 119192 | 9143 | 130145 | 0.929 | 4 | 0 | 0 | 0 | 5 | 17.5 | 0.0795 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 1 | 20 | 4440 | 330886 | 13012 | 348358 | 0.962 | 9 | 0 | 0 | 0 | 10 | 52.0 | 0.1638 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 1 | 60 | 44559 | 1811800 | 103225 | 1959644 | 0.946 | 65 | 49 | 49 | 3 | 67 | 564.2 | 1.2210 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 2 | 12 | 2057 | 188748 | 9752 | 200569 | 0.951 | 6 | 0 | 0 | 0 | 7 | 25.0 | 0.0984 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 2 | 14 | 3089 | 221278 | 10754 | 235135 | 0.954 | 6 | 0 | 0 | 0 | 7 | 29.0 | 0.1193 | 0.83 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 2 | 60 | 16276 | 1504552 | 50362 | 1571250 | 0.968 | 30 | 20 | 20 | 3 | 31 | 225.2 | 0.6652 | 0.00 | 0 | 0 | - | - | error_max_turns |
| 14 | assoc-timetravel-2 | P | 3 | 12 | 2161 | 187070 | 9636 | 198879 | 0.951 | 5 | 0 | 0 | 0 | 6 | 27.8 | 0.0987 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 3 | 16 | 3195 | 261164 | 13494 | 277869 | 0.951 | 8 | 0 | 0 | 0 | 9 | 31.6 | 0.1393 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 3 | 50 | 29092 | 1441451 | 87371 | 1557964 | 0.943 | 37 | 26 | 26 | 3 | 38 | 373.8 | 0.9288 | 0.83 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | P | 1 | 14 | 2626 | 223093 | 10535 | 236268 | 0.955 | 7 | 0 | 0 | 0 | 8 | 33.5 | 0.1141 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 1 | 12 | 2357 | 185503 | 10481 | 198353 | 0.947 | 6 | 0 | 0 | 0 | 7 | 180.0 | 0.1037 | 0.86 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 1 | 60 | 17463 | 1566809 | 70791 | 1655123 | 0.957 | 44 | 31 | 31 | 3 | 31 | 221.2 | 0.7713 | 0.00 | 0 | 0 | - | - | error_max_turns |
| 15 | assoc-timetravel-3 | P | 2 | 12 | 2557 | 187689 | 10048 | 200306 | 0.949 | 6 | 0 | 0 | 0 | 7 | 30.0 | 0.1044 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 2 | 10 | 2369 | 154056 | 10329 | 166764 | 0.937 | 5 | 0 | 0 | 0 | 6 | 25.0 | 0.0970 | 0.86 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 2 | 60 | 18573 | 1894861 | 76423 | 1989917 | 0.961 | 45 | 36 | 36 | 2 | 31 | 278.8 | 0.8705 | 0.00 | 0 | 0 | - | - | error_max_turns |
| 15 | assoc-timetravel-3 | P | 3 | 14 | 2288 | 222275 | 10417 | 234994 | 0.955 | 6 | 0 | 0 | 0 | 7 | 32.4 | 0.1101 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 3 | 16 | 3809 | 262319 | 13964 | 280108 | 0.949 | 8 | 0 | 0 | 0 | 9 | 68.8 | 0.1475 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 3 | 46 | 26556 | 1461950 | 88224 | 1576776 | 0.943 | 62 | 51 | 51 | 2 | 64 | 329.5 | 0.9109 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | P | 1 | 10 | 2029 | 155203 | 10065 | 167307 | 0.939 | 5 | 0 | 0 | 0 | 6 | 61.9 | 0.0927 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 1 | 16 | 2563 | 252988 | 10091 | 265658 | 0.962 | 7 | 0 | 0 | 0 | 8 | 28.5 | 0.1177 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 1 | 60 | 7755 | 1528711 | 52284 | 1588810 | 0.967 | 30 | 20 | 20 | 3 | 31 | 131.5 | 0.5925 | 0.00 | 0 | 0 | - | - | error_max_turns |
| 16 | assoc-timetravel-4 | P | 2 | 14 | 2396 | 222453 | 10206 | 235069 | 0.956 | 7 | 0 | 0 | 0 | 8 | 24.9 | 0.1104 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 2 | 18 | 4090 | 297670 | 13189 | 314967 | 0.958 | 8 | 0 | 0 | 0 | 9 | 48.1 | 0.1543 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 2 | 60 | 19860 | 1688582 | 72734 | 1781236 | 0.959 | 41 | 30 | 30 | 3 | 31 | 315.2 | 0.8274 | 0.00 | 0 | 0 | - | - | error_max_turns |
| 16 | assoc-timetravel-4 | P | 3 | 16 | 2065 | 244666 | 15024 | 261771 | 0.942 | 7 | 0 | 0 | 0 | 8 | 30.3 | 0.1308 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 3 | 14 | 4782 | 227067 | 13645 | 245508 | 0.943 | 7 | 0 | 0 | 0 | 8 | 41.5 | 0.1489 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 3 | 60 | 9508 | 1393285 | 40415 | 1443268 | 0.972 | 30 | 13 | 19 | 3 | 31 | 164.8 | 0.5355 | 0.00 | 0 | 0 | - | - | error_max_turns |
| 17 | assoc-visibility-1 | P | 1 | 14 | 2238 | 223608 | 10356 | 236216 | 0.956 | 7 | 0 | 0 | 0 | 8 | 228.6 | 0.1096 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 1 | 10 | 3477 | 155295 | 11573 | 170355 | 0.931 | 5 | 0 | 0 | 0 | 6 | 59.7 | 0.1132 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 1 | 18 | 3756 | 289826 | 13781 | 307381 | 0.955 | 10 | 2 | 2 | 1 | 11 | 64.8 | 0.1507 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 2 | 12 | 2821 | 189804 | 10741 | 203378 | 0.946 | 6 | 0 | 0 | 0 | 7 | 39.0 | 0.1102 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 2 | 10 | 3130 | 154515 | 10625 | 168280 | 0.936 | 5 | 0 | 0 | 0 | 6 | 36.2 | 0.1058 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 2 | 42 | 7085 | 879457 | 31107 | 917691 | 0.966 | 25 | 19 | 19 | 2 | 26 | 96.2 | 0.3713 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 3 | 16 | 3040 | 263362 | 11863 | 278281 | 0.957 | 8 | 0 | 0 | 0 | 9 | 40.1 | 0.1316 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 3 | 20 | 4190 | 334953 | 12433 | 351596 | 0.964 | 10 | 0 | 0 | 0 | 11 | 46.1 | 0.1597 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 3 | 52 | 9594 | 1096243 | 33285 | 1139174 | 0.971 | 26 | 19 | 19 | 2 | 27 | 120.4 | 0.4484 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 1 | 16 | 3448 | 261750 | 11678 | 276892 | 0.957 | 8 | 0 | 0 | 0 | 9 | 100.5 | 0.1347 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 1 | 12 | 3819 | 190592 | 11698 | 206121 | 0.942 | 6 | 0 | 0 | 0 | 7 | 83.0 | 0.1242 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 1 | 36 | 5746 | 669475 | 28317 | 703574 | 0.959 | 17 | 12 | 12 | 2 | 18 | 80.1 | 0.3047 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 2 | 18 | 3544 | 294188 | 12135 | 309885 | 0.960 | 9 | 0 | 0 | 0 | 10 | 37.1 | 0.1439 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 2 | 18 | 5791 | 308298 | 14617 | 328724 | 0.955 | 9 | 0 | 0 | 0 | 10 | 66.7 | 0.1792 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 2 | 16 | 3650 | 251197 | 12828 | 267691 | 0.951 | 10 | 2 | 2 | 1 | 11 | 50.6 | 0.1381 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 3 | 12 | 2747 | 188410 | 10552 | 201721 | 0.947 | 6 | 0 | 0 | 0 | 7 | 33.8 | 0.1085 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 3 | 16 | 6020 | 270276 | 14851 | 291163 | 0.948 | 8 | 0 | 0 | 0 | 9 | 68.1 | 0.1748 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 3 | 36 | 6949 | 790300 | 32627 | 829912 | 0.960 | 20 | 16 | 16 | 1 | 21 | 103.5 | 0.3581 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 1 | 12 | 2349 | 188589 | 10243 | 201193 | 0.948 | 6 | 0 | 0 | 0 | 7 | 30.8 | 0.1033 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 1 | 10 | 3878 | 155341 | 11660 | 170889 | 0.930 | 5 | 0 | 0 | 0 | 6 | 33.8 | 0.1176 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 1 | 44 | 6857 | 885640 | 32783 | 925324 | 0.964 | 25 | 18 | 18 | 2 | 26 | 101.2 | 0.3769 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 2 | 12 | 2428 | 187593 | 9984 | 200017 | 0.949 | 6 | 0 | 0 | 0 | 7 | 32.4 | 0.1029 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 2 | 10 | 3274 | 155215 | 11352 | 169851 | 0.932 | 5 | 0 | 0 | 0 | 6 | 38.3 | 0.1103 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 2 | 34 | 4006 | 678598 | 25517 | 708155 | 0.964 | 19 | 18 | 18 | 1 | 20 | 78.9 | 0.2779 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 3 | 14 | 3121 | 221184 | 10189 | 234508 | 0.956 | 7 | 0 | 0 | 0 | 8 | 39.8 | 0.1173 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 3 | 20 | 4311 | 338514 | 12884 | 355729 | 0.963 | 10 | 0 | 0 | 0 | 11 | 46.3 | 0.1635 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 3 | 60 | 7582 | 1589902 | 46252 | 1643796 | 0.972 | 34 | 28 | 28 | 2 | 35 | 106.8 | 0.5789 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 1 | 12 | 2636 | 188333 | 10270 | 201251 | 0.948 | 6 | 0 | 0 | 0 | 7 | 23.9 | 0.1062 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 1 | 12 | 3289 | 190572 | 11879 | 205752 | 0.941 | 6 | 0 | 0 | 0 | 7 | 143.0 | 0.1196 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 1 | 36 | 6199 | 828681 | 32927 | 867843 | 0.962 | 21 | 17 | 17 | 1 | 22 | 352.4 | 0.3595 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 2 | 12 | 2563 | 188507 | 10328 | 201410 | 0.948 | 6 | 0 | 0 | 0 | 7 | 41.9 | 0.1058 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 2 | 14 | 3556 | 226176 | 12092 | 241838 | 0.949 | 7 | 0 | 0 | 0 | 8 | 40.1 | 0.1303 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 2 | 42 | 6157 | 929695 | 31678 | 967572 | 0.967 | 20 | 19 | 19 | 1 | 21 | 87.6 | 0.3743 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 3 | 12 | 2049 | 188485 | 9668 | 200214 | 0.951 | 6 | 0 | 0 | 0 | 7 | 31.8 | 0.0980 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 3 | 12 | 4037 | 191617 | 12374 | 208040 | 0.939 | 6 | 0 | 0 | 0 | 7 | 33.9 | 0.1293 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 3 | 32 | 4362 | 678444 | 25791 | 708629 | 0.963 | 16 | 12 | 12 | 1 | 17 | 65.3 | 0.2825 | 1.00 | 0 | 0 | - | - | success |

## Aggregate (mean per arm)

| metric | arm P (files + grep) | arm Q (sqlite) | arm R (graph) | P vs Q | R vs Q |
|---|---|---|---|---|---|
| total tokens | 202216 | 226711 | 935776 | -10.8% | +312.8% |
| input tokens | 12 | 13 | 36 | -8.4% | +173.4% |
| output tokens | 2384 | 3469 | 9357 | -31.3% | +169.7% |
| cache read | 189273 | 211313 | 865866 | -10.4% | +309.8% |
| cache create | 10547 | 11915 | 60517 | -11.5% | +407.9% |
| cache hit ratio | 0.946 | 0.944 | 0.909 | +0.3% | -3.6% |
| tool calls | 5.88 | 6.52 | 25.17 | -9.7% | +286.2% |
| mcp calls | 0.00 | 0.00 | 19.63 | n/a | n/a |
| graph calls | 0.00 | 0.00 | 19.73 | n/a | n/a |
| tool search calls | 0.00 | 0.00 | 1.62 | n/a | n/a |
| adoption | 0.00 | 0.00 | 1.00 | n/a | n/a |
| turns | 6.88 | 7.52 | 25.55 | -8.4% | +239.9% |
| seconds | 45.7 | 60.3 | 159.1 | -24.3% | +163.8% |
| cost $ | 0.1050 | 0.1257 | 0.5089 | -16.5% | +304.7% |
| score | 1.000 | 0.987 | 0.795 | +1.4% | -19.4% |
| wrong extra | 0.00 | 0.00 | 0.28 | n/a | n/a |

arm P: 60 cells; arm Q: 60 cells; arm R: 60 cells; timeouts 0; errors 8; cells with no cost 0

## Deltas vs arm Q

Paired by task: one difference per task, arm mean minus arm Q mean. The interval is a 95% percentile bootstrap over those differences (2000 resamples, seeded). An interval that does not contain 0 is the only kind worth quoting. `tasks` is how many tasks contributed a difference — a task where either side recorded nothing contributes none.

| arm | metric | arm mean | arm Q mean | delta | 95% CI | tasks |
|---|---|---|---|---|---|---|
| P | score | 1.000 | 0.987 | 0.0135 | [0.0026, 0.0262] | 20 |
| P | cost $ | 0.1050 | 0.1257 | -0.02074 | [-0.02766, -0.01322] | 20 |
| P | total tokens | 202216 | 226711 | -24494.9 | [-43121.9, -6537.7] | 20 |
| P | turns | 6.88 | 7.52 | -0.633 | [-1.133, -0.183] | 20 |
| P | cache hit ratio | 0.946 | 0.944 | 0.0024 | [-0.0010, 0.0057] | 20 |
| R | score | 0.795 | 0.987 | -0.1914 | [-0.3420, -0.0699] | 20 |
| R | cost $ | 0.5089 | 0.1257 | 0.38314 | [0.25218, 0.52528] | 20 |
| R | total tokens | 935776 | 226711 | 709065.6 | [498965.9, 914119.2] | 20 |
| R | turns | 25.55 | 7.52 | 18.033 | [11.483, 25.667] | 20 |
| R | cache hit ratio | 0.909 | 0.944 | -0.0344 | [-0.0682, -0.0036] | 20 |

## Correctness by task (mean score over reps)

| task | arm P | arm Q | arm R | best |
|---|---|---|---|---|
| 1 | 1.00 | 1.00 | 0.25 | P |
| 2 | 1.00 | 1.00 | 0.75 | P |
| 3 | 1.00 | 1.00 | 1.00 | tie |
| 4 | 1.00 | 1.00 | 1.00 | tie |
| 5 | 1.00 | 1.00 | 0.67 | P |
| 6 | 1.00 | 0.93 | 1.00 | P |
| 7 | 1.00 | 1.00 | 1.00 | tie |
| 8 | 1.00 | 1.00 | 0.33 | P |
| 9 | 1.00 | 1.00 | 1.00 | tie |
| 10 | 1.00 | 0.97 | 1.00 | P |
| 11 | 1.00 | 0.98 | 1.00 | P |
| 12 | 1.00 | 1.00 | 1.00 | tie |
| 13 | 1.00 | 1.00 | 0.96 | P |
| 14 | 1.00 | 0.94 | 0.61 | P |
| 15 | 1.00 | 0.90 | 0.33 | P |
| 16 | 1.00 | 1.00 | 0.00 | P |
| 17 | 1.00 | 1.00 | 1.00 | tie |
| 18 | 1.00 | 1.00 | 1.00 | tie |
| 19 | 1.00 | 1.00 | 1.00 | tie |
| 20 | 1.00 | 1.00 | 1.00 | tie |
| **mean** | **1.000** | **0.987** | **0.795** | |

## Sub-1.0 cells

One line per cell that did not score 1.0. `classification` is filled in by hand after reading the cell's stream file: `tool` (the graph answered badly or not at all), `model` (the agent had what it needed and went wrong anyway), or `grader` (the answer is right and the check is wrong).

| task | key | arm | rep | score | missed | verify | outcome | classification |
|---|---|---|---|---|---|---|---|---|
| 5 | assoc-multihop-1 | R | 2 | 0.00 | set 0/27 | - | error_max_turns |  |
| 8 | assoc-multihop-4 | R | 1 | 0.00 | set 9/9 -5 | - | success |  |
| 8 | assoc-multihop-4 | R | 3 | 0.00 | set 0/9 | - | error_max_turns |  |
| 14 | assoc-timetravel-2 | R | 2 | 0.00 | set 0/6 | - | error_max_turns |  |
| 15 | assoc-timetravel-3 | R | 1 | 0.00 | set 0/7 | - | error_max_turns |  |
| 15 | assoc-timetravel-3 | R | 2 | 0.00 | set 0/7 | - | error_max_turns |  |
| 16 | assoc-timetravel-4 | R | 1 | 0.00 | set 0/8 | - | error_max_turns |  |
| 16 | assoc-timetravel-4 | R | 2 | 0.00 | set 0/8 | - | error_max_turns |  |
| 16 | assoc-timetravel-4 | R | 3 | 0.00 | set 0/8 | - | error_max_turns |  |
| 1 | assoc-why-1 | R | 1 | 0.25 | set 5/5 -3 | - | success |  |
| 1 | assoc-why-1 | R | 2 | 0.25 | set 5/5 -3 | - | success |  |
| 1 | assoc-why-1 | R | 3 | 0.25 | set 5/5 -3 | - | success |  |
| 2 | assoc-why-2 | R | 1 | 0.25 | set 5/5 -3 | - | success |  |
| 6 | assoc-multihop-2 | Q | 1 | 0.80 | set 8/10 | - | success |  |
| 14 | assoc-timetravel-2 | Q | 2 | 0.83 | set 5/6 | - | success |  |
| 14 | assoc-timetravel-2 | R | 3 | 0.83 | set 5/6 | - | success |  |
| 15 | assoc-timetravel-3 | Q | 1 | 0.86 | set 6/7 | - | success |  |
| 15 | assoc-timetravel-3 | Q | 2 | 0.86 | set 6/7 | - | success |  |
| 13 | assoc-timetravel-1 | R | 2 | 0.88 | set 7/8 | - | success |  |
| 10 | assoc-retraction-2 | Q | 3 | 0.92 | set 11/12 | - | success |  |
| 11 | assoc-retraction-3 | Q | 1 | 0.93 | set 13/14 | - | success |  |

## Tool adoption

Sessions that made at least one MCP call, and which mushroomdb tools they used.

| arm | sessions | >=1 mcp call | adoption | mcp calls total |
|---|---|---|---|---|
| P | 60 | 0 | 0% | 0 |
| Q | 60 | 0 | 0% | 0 |
| R | 60 | 60 | 100% | 1178 |

- arm P: no MCP tools used
- arm Q: no MCP tools used
- arm R: `query` x612, `edge_history` x222, `node_info` x146, `was_linked` x52, `node_edges` x40, `node_history` x38, `explain_association` x30, `stats` x29, `neighborhood` x4, `recall` x4, `mcp__mushroomdb` x1

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
23.88 km ≤ 160.9 km, so LOCATION_FIT holds too.

**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

**Shared specialties:**
hospitality
residential
```

### task 1 rep 1 arm R (score 0.25)

```
Talent specialties: hospitality, residential, retail
Company specialties: commercial, hospitality, residential, single-family

INDUSTRY_ALIGNMENT
LOCATION_FIT
SPECIALTY_MATCH

hospitality
residential
```
missed: set 5/5 -3

### task 1 rep 2 arm P (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

**Common specialties:**
hospitality
residential
```

### task 1 rep 2 arm Q (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

**Specialties in common:**
hospitality
residential
```

### task 1 rep 2 arm R (score 0.25)

```
Talent specialties: hospitality, residential, retail
Company specialties: commercial, hospitality, residential, single-family

Common specialties: hospitality, residential

INDUSTRY_ALIGNMENT
LOCATION_FIT
SPECIALTY_MATCH

hospitality
residential
```
missed: set 5/5 -3

### task 1 rep 3 arm P (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

**Specialties in common:**
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

### task 1 rep 3 arm R (score 0.25)

```
Talent specialties: hospitality, residential, retail
Company specialties: commercial, hospitality, residential, single-family

Relationship types:
INDUSTRY_ALIGNMENT
LOCATION_FIT
SPECIALTY_MATCH

Common specialties:
hospitality
residential
```
missed: set 5/5 -3

### task 2 rep 1 arm P (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Shared specialties:**
multi-family
residential
```

### task 2 rep 1 arm Q (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Common specialties:**
residential
multi-family
```

### task 2 rep 1 arm R (score 0.25)

```
Talent specialties: healthcare, multi-family, residential, large-scale
Company specialties: residential, civic, multi-family

INDUSTRY_ALIGNMENT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

multi-family
residential
```
missed: set 5/5 -3

### task 2 rep 2 arm P (score 1.00)

```
`talent-000512` and `company-000054` both have `industry: "architecture"`, share `specialties` values `residential` and `multi-family` (Jaccard 2/5 = 0.4 ≥ 0.15), and share `design_styles` value `industrial` (Jaccard 1/4 = 0.25 ≥ 0.2).

INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

residential
multi-family
```

### task 2 rep 2 arm Q (score 1.00)

```
**Relationships:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Shared specialties:**
multi-family
residential
```

### task 2 rep 2 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

residential
multi-family
```

### task 2 rep 3 arm P (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Common specialties:**
residential
multi-family
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
MATCHES_DESIGN_STYLE
LOCATION_FIT

multi-family
commercial
```

### task 3 rep 1 arm Q (score 1.00)

```
## Relationships between `talent-001202` and `company-000014`

- SPECIALTY_MATCH
- LOCATION_FIT
- MATCHES_DESIGN_STYLE

## Specialties in common

- multi-family
- commercial
```

### task 3 rep 1 arm R (score 1.00)

```
LOCATION_FIT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

multi-family
commercial
```

### task 3 rep 2 arm P (score 1.00)

```
26.26 km ≤ 160.9 km, so LOCATION_FIT holds too.

Relationships derived between talent-001202 and company-000014:

SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

Common specialties:
multi-family
commercial
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

multi-family
commercial
```

### task 3 rep 3 arm P (score 1.00)

```
Relationships (talent-001202 ↔ company-000014):

SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

Common specialties:
multi-family
commercial
```

### task 3 rep 3 arm Q (score 1.00)

```
26.26 km ≤ 160.9 km, so LOCATION_FIT also holds.

Neither entity appears in the changes table, so day-89 state equals day-0 state.

**Relationships:**
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

**Shared specialties:**
multi-family
commercial
```

### task 3 rep 3 arm R (score 1.00)

```
LOCATION_FIT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

multi-family
commercial
```

### task 4 rep 1 arm P (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Specialties in common:**
civic
institutional
```

### task 4 rep 1 arm Q (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Shared specialties:**
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
Relationships:
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

Common specialties:
civic
institutional
```

### task 4 rep 2 arm Q (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Common specialties:**
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

institutional
civic
```

### task 4 rep 3 arm Q (score 1.00)

```
Neither entity has any changes in history, so day-0 fields are final.

Relationships:
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

Common specialties:
civic
institutional
```

### task 4 rep 3 arm R (score 1.00)

```
Relationship types:
INDUSTRY_ALIGNMENT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

Common specialties:
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
company-000320
company-000098
company-000013
company-000039
company-000056
company-000135
company-000141
company-000186
company-000230
company-000243
company-000071
company-000111
company-000112
company-000159
company-000193
company-000291
company-000343
company-000361
company-000405
company-000414
company-000010
company-000018
company-000026
company-000127
company-000373
company-000381
company-000399
```

### task 5 rep 2 arm Q (score 1.00)

```
Results computed after applying the full 300-change history (final state). Companies linked by all three relations to ≥14 eligible talents (published, years_of_experience ≥ 10):

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

### task 5 rep 2 arm R (score 0.00)

```
(no answer)
```
missed: set 0/27

### task 5 rep 3 arm P (score 1.00)

```
Good, format matches my script's assumptions (key always present, node present for insert). The script correctly filtered by prefix and applied changes in order. Result stands:

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

### task 6 rep 1 arm P (score 1.00)

```
company-000320
company-000098
company-000013
company-000039
company-000056
company-000135
company-000141
company-000186
company-000230
company-000243
```

### task 6 rep 1 arm Q (score 0.80)

```
company-000098
company-000320
company-000013
company-000039
company-000056
company-000135
company-000230
company-000243
```
missed: set 8/10

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
company-000320
company-000098
company-000013
company-000039
company-000056
company-000135
company-000141
company-000186
company-000230
company-000243
```

### task 6 rep 2 arm R (score 1.00)

```
Good, edges are unique per pair/type. The answer set (n≥16) is confirmed as the top 10 rows.

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
Structure matches my script's assumptions. Final answer — companies linked by all three relationships to at least 16 qualifying talents:

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
Companies linked by all three relationship types (INDUSTRY_ALIGNMENT, SPECIALTY_MATCH, LOCATION_FIT) to ≥16 published talents with 10+ years experience:

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

### task 7 rep 1 arm P (score 1.00)

```
company-000141
company-000321
company-000193
company-000006
company-000013
company-000098
company-000112
company-000343
company-000405
company-000010
company-000026
company-000102
company-000103
company-000127
company-000196
company-000230
company-000265
company-000340
company-000390
company-000410
```

### task 7 rep 1 arm Q (score 1.00)

```
Good — the insert_node format uses `location` and `specialties`/`design_styles` directly (not `_json` suffixed), which my script already handles correctly since it checks `rec.get('specialties')` fallback. The script's logic matches the data format. Results confirmed.

**company-000006**
**company-000010**
**company-000013**
**company-000026**
**company-000098**
**company-000102**
**company-000103**
**company-000112**
**company-000127**
**company-000141**
**company-000193**
**company-000196**
**company-000230**
**company-000265**
**company-000321**
**company-000340**
**company-000343**
**company-000390**
**company-000405**
**company-000410**
```

### task 7 rep 1 arm R (score 1.00)

```
All 11 are distinct talents, confirming no duplicate-edge inflation. The results stand.

Companies linked by all three (INDUSTRY_ALIGNMENT, SPECIALTY_MATCH, LOCATION_FIT) to at least 8 published talents with 15+ years of experience:

```
company-000141
company-000321
company-000193
company-000343
company-000405
company-000013
company-000112
company-000098
company-000006
company-000103
company-000127
company-000102
company-000010
company-000265
company-000196
company-000340
company-000230
company-000410
company-000026
company-000390
```
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
The results changed after fixing the location bug for inserted nodes — this second run is correct. Final answer:

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
Based on companies linked via all three relationship types (INDUSTRY_ALIGNMENT, SPECIALTY_MATCH, LOCATION_FIT) to the same ≥8 published talents with 15+ years of experience:

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
Good, format confirmed correct. My script correctly parses `location` field directly from node_json.

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

### task 8 rep 1 arm R (score 0.00)

```
company-000000
company-000001
company-000002
company-000003
company-000005
company-000006
company-000007
company-000008
company-000009
company-000010
company-000011
company-000012
company-000013
company-000014
company-000015
company-000016
company-000017
company-000018
company-000019
company-000020
company-000021
company-000022
company-000023
company-000024
company-000025
company-000026
company-000027
company-000028
company-000029
company-000030
company-000031
company-000032
company-000033
company-000034
company-000035
company-000036
company-000037
company-000038
company-000039
company-000040
company-000041
company-000042
company-000043
company-000044
company-000045
company-000046
company-000047
company-000048
company-000049
company-000050
company-000051
company-000052
company-000053
company-000054
company-000055
company-000056
company-000057
company-000058
company-000059
company-000060
company-000061
company-000062
company-000063
company-000064
company-000065
company-000066
company-000067
company-000068
company-000069
company-000070
company-000071
company-000072
company-000073
company-000074
company-000075
company-000076
company-000077
company-000078
company-000079
company-000080
company-000081
company-000082
company-000083
company-000084
company-000085
company-000086
company-000087
company-000088
company-000089
company-000090
company-000091
company-000092
company-000093
company-000094
company-000095
company-000096
company-000097
company-000098
company-000099
company-000100
company-000101
company-000102
company-000103
company-000104
company-000105
company-000106
company-000107
company-000108
company-000109
company-000110
company-000111
company-000112
company-000113
company-000114
company-000115
company-000116
company-000117
company-000118
company-000119
company-000120
company-000121
company-000122
company-000123
company-000124
company-000125
company-000126
company-000127
company-000128
company-000129
company-000130
company-000131
company-000132
company-000133
company-000134
company-000135
company-000136
company-000137
company-000138
company-000139
company-000140
company-000141
company-000142
company-000143
company-000144
company-000145
company-000146
company-000147
company-000148
company-000149
company-000150
company-000151
company-000152
company-000153
company-000154
company-000155
company-000156
company-000157
company-000158
company-000159
company-000160
company-000161
company-000162
company-000163
company-000164
company-000165
company-000166
company-000167
company-000168
company-000169
company-000170
company-000171
company-000172
company-000173
company-000174
company-000175
company-000176
company-000177
company-000178
company-000179
company-000180
company-000181
company-000182
company-000183
company-000184
company-000185
company-000186
company-000187
company-000188
company-000189
company-000190
company-000191
company-000192
company-000193
company-000194
company-000195
company-000196
company-000197
company-000198
company-000199
company-000200
company-000201
company-000202
company-000203
company-000204
company-000205
company-000206
company-000207
company-000208
company-000209
company-000210
company-000211
company-000212
company-000213
company-000214
company-000215
company-000216
company-000217
company-000218
company-000219
company-000220
company-000221
company-000222
company-000223
company-000224
company-000225
company-000226
company-000227
company-000228
company-000229
company-000230
company-000231
company-000232
company-000233
company-000234
company-000235
company-000236
company-000237
company-000238
company-000239
company-000240
company-000241
company-000242
company-000243
company-000244
company-000245
company-000246
company-000247
company-000248
company-000249
company-000250
company-000251
company-000252
company-000253
company-000254
company-000255
company-000256
company-000257
company-000258
company-000259
company-000260
company-000261
company-000262
company-000263
company-000264
company-000265
company-000266
company-000267
company-000268
company-000269
company-000270
company-000271
company-000272
company-000273
company-000274
company-000275
company-000276
company-000277
company-000278
company-000279
company-000280
company-000281
company-000282
company-000283
company-000284
company-000285
company-000286
company-000287
company-000288
company-000289
company-000290
company-000291
company-000292
company-000293
company-000294
company-000295
company-000296
company-000297
company-000298
company-000299
company-000300
company-000301
company-000302
company-000303
company-000304
company-000305
company-000306
company-000307
company-000308
company-000309
company-000310
company-000311
company-000312
company-000313
company-000314
company-000315
company-000316
company-000317
company-000318
company-000319
company-000320
company-000321
company-000322
company-000323
company-000324
company-000325
company-000326
company-000327
company-000328
company-000329
company-000330
company-000331
company-000332
company-000333
company-000334
company-000335
company-000336
company-000337
company-000338
company-000339
company-000340
company-000341
company-000342
company-000343
company-000344
company-000345
company-000346
company-000347
company-000348
company-000349
company-000350
company-000351
company-000352
company-000353
company-000354
company-000355
company-000356
company-000357
company-000358
company-000359
company-000360
company-000361
company-000362
company-000363
company-000364
company-000365
company-000366
company-000367
company-000368
company-000369
company-000370
company-000371
company-000372
company-000373
company-000374
company-000375
company-000376
company-000377
company-000378
company-000379
company-000380
company-000381
company-000382
company-000383
company-000384
company-000385
company-000386
company-000387
company-000388
company-000389
company-000390
company-000391
company-000392
company-000393
company-000394
company-000395
company-000396
company-000397
company-000398
company-000399
company-000400
company-000401
company-000402
company-000403
company-000404
company-000405
company-000406
company-000407
company-000408
company-000409
company-000410
company-000411
company-000412
company-000413
company-000414
company-000415
company-000416
company-000417
company-000418
company-000419
company-000420
company-000421
```
missed: set 9/9 -5

### task 8 rep 2 arm P (score 1.00)

```
All companies exist in the final state. The result is confirmed.

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

### task 8 rep 3 arm R (score 0.00)

```
That works too. So the issue was `WITH c, count(...)` after that. Let's try grouping now.
```
missed: set 0/9

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
These all retain overlap (institutional/civic/hospitality) with the new specialty list, so they stay linked. The 12 talents that lose the SPECIALTY_MATCH link are:

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

### task 10 rep 3 arm Q (score 0.92)

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
```
missed: set 11/12

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

### task 11 rep 1 arm Q (score 0.93)

```
Based on the final (post-history) state, company-000244 has `size_bucket=5` and is linked by all three relationships to 21 talents. Changing its `size_bucket` to 3 only affects `SIMILAR_SIZE` (tolerance 1, so it now requires talent `size_bucket` in {2,3,4}). The talents with `size_bucket=5` would drop out:

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
```
missed: set 13/14

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
Good, company-000244 was never changed in history, confirming its final state matches day-0. The results are final.

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
This confirms the logic: talents with `size_bucket=5` fall out of range (need |5-3|<=1, i.e., false), while `size_bucket=4` remain (|4-3|=1). The 14 talents that would no longer be linked by all three:

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
Confirms current diff=1 (bucket 5 vs 4) gives SIMILAR_SIZE with score 0.00 (edge boundary), rule is `numeric_within on size_bucket +/- 1`. With new size_bucket=3, talents at bucket 5 (diff=2) drop out of the ±1 window while bucket-4 talents (diff=1) remain.

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
Talents that would no longer be linked by all three (INDUSTRY_ALIGNMENT, SIMILAR_SIZE, LOCATION_FIT) if company-000235's size_bucket became 3:

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
Same result after fixing the bug — none of the location changes affected these companies or the talent record.

company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 13 rep 2 arm R (score 0.88)

```
company-000057
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```
missed: set 7/8

### task 13 rep 3 arm P (score 1.00)

```
All checks out.

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
All confirmed: SPECIALTY_MATCH was added at commit 6 and retracted at commit 156 (right after the 2026-06-19 snapshot at commit 148), consistent with talent-000501's specialties changing at commit 155. INDUSTRY_ALIGNMENT and LOCATION_FIT were added at commits 2 and 10 respectively and never retracted for these companies.

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
Good — that change is on day 42, after our target day 41, so it doesn't affect the 2026-07-12 state.

talent-001026 was linked to the following companies by all three relationships on 2026-07-12:

company-000042
company-000066
company-000138
company-000246
company-000354
company-000402
```

### task 14 rep 1 arm R (score 1.00)

```
`talent-001026`'s properties (industry, specialties) never changed in this dataset — only its `location` changed, at commit 311 (2026-07-13), moving it out of Seattle. On 2026-07-12 (commit 302) it was still in Seattle, so I checked every company in the Seattle-metro cluster for having all three edge types active at that commit.

Companies linked to `talent-001026` by `INDUSTRY_ALIGNMENT`, `SPECIALTY_MATCH`, and `LOCATION_FIT` on 2026-07-12:

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

### task 14 rep 2 arm Q (score 0.83)

```
Good, field names confirmed. Companies linked to `talent-001026` by all three relationships on 2026-07-12:

company-000042
company-000066
company-000138
company-000246
company-000354
```
missed: set 5/6

### task 14 rep 2 arm R (score 0.00)

```
It saved to a file; let me process it with Python via Bash.
```
missed: set 0/6

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

### task 14 rep 3 arm R (score 0.83)

```
All confirmed added at commit 312 (after our target commit 302), so none qualify for 2026-07-12. Final answer — companies linked by all three edge types on 2026-07-12:

company-000066
company-000138
company-000246
company-000354
company-000402
```
missed: set 5/6

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

### task 15 rep 1 arm Q (score 0.86)

```
Day 42 = 2026-07-13, confirmed. The matches:

company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
```
missed: set 6/7

### task 15 rep 1 arm R (score 0.00)

```
Now let's get key, specialties, and location together properly.
```
missed: set 0/7

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

### task 15 rep 2 arm Q (score 0.86)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
```
missed: set 6/7

### task 15 rep 2 arm R (score 0.00)

```
There's a `days.json` mapping file. Let me check it along with the README.
```
missed: set 0/7

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
Confirms exactly the same 7 companies using current specialty/location data directly, matching the was_linked verification at commit 312. No other architecture companies qualify.

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
Sorted, so the computation is correct.

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
Day 43 confirmed to correspond to 2026-07-14.

company-000048
company-000120
company-000132
company-000192
company-000288
company-000294
company-000312
company-000360
```

### task 16 rep 1 arm R (score 0.00)

```
Found the mapping. Let's get the commit for 2026-07-14.
```
missed: set 0/8

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
Good, that confirms the relocation happened before the target date, which matches my computed match. The results check out.

**Companies:**
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
```

### task 16 rep 2 arm R (score 0.00)

```
There's a `days.json` file — that's likely the commit-to-date mapping.
```
missed: set 0/8

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
All checks confirm. Companies linked to `talent-000396` by all three relationships (INDUSTRY_ALIGNMENT, SPECIALTY_MATCH, LOCATION_FIT) as of 2026-07-14:

company-000048
company-000120
company-000132
company-000192
company-000288
company-000294
company-000312
company-000360
```

### task 16 rep 3 arm R (score 0.00)

```
Now let me check days.json for the commit index corresponding to 2026-07-14.
```
missed: set 0/8

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
Good, that confirms the parsing was correct (`location` key present as expected). Also confirmed no `talent-000104` insert/delete occurred (it's a day-0 entity, still present).

**talent-000104 final state:** industry=`architecture`, specialties=`[single-family, educational]`.

Jobs linked by both INDUSTRY_ALIGNMENT and SPECIALTY_MATCH:

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
28 out of 89 architecture jobs also clearing the specialty overlap threshold checks out. Here are the keys:

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
talent-000104's specialties are `["single-family", "educational"]` and industry is `architecture`. Jobs linked by both INDUSTRY_ALIGNMENT and SPECIALTY_MATCH (industry equal, specialties Jaccard ≥ 0.15):

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
Confirms the recruiter role can't see Company nodes at all — only the 28 Job nodes are valid to name.

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
Confirmed. As recruiter, `talent-000104` is linked by both INDUSTRY_ALIGNMENT and SPECIALTY_MATCH to these Jobs:

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
Final job count check aside, the computation applied all 300 changes in order to day-0 state and evaluated the two rules against the resulting talent-000450 record and every surviving Job. Since the `recruiter` role only sees Talent and Job (not Company), only Job-linked matches are nameable.

**talent-000450 is linked by both INDUSTRY_ALIGNMENT and SPECIALTY_MATCH to:**

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
talent-000450 is linked to both `INDUSTRY_ALIGNMENT` and `SPECIALTY_MATCH` (recruiter-visible entities only):

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
These look consistent (industry matches, specialties overlap with talent-000450's `["industrial", "landscape"]`).

talent-000450 is linked to these Job entities by both INDUSTRY_ALIGNMENT and SPECIALTY_MATCH:

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
Filtering to the `recruiter` role, which can only see `Talent` and `Job` entities (not `Company`), the entities linked to `talent-000450` by both `INDUSTRY_ALIGNMENT` and `SPECIALTY_MATCH` are:

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
Based on the final state (after all 300 changes), recruiters see only Talent and Job entities — Company links exist but aren't visible to this role. `talent-000450` (industry: `interior-design`, specialties: `industrial`, `landscape`) is linked to these Jobs by both INDUSTRY_ALIGNMENT and SPECIALTY_MATCH:

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
Confirmed by query, matching my manual check.

job-000040
job-000046
job-000052
job-000124
job-000155
```

### task 20 rep 3 arm P (score 1.00)

```
Good, the working directory matched, so the results are valid. As recruiter, only Job entities are visible alongside Talent (Company is not), so only Talent–Job links can be named.

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

