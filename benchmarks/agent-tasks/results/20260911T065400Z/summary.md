# Agent benchmark run

- run: `20260911T065400Z`
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

- R: correctness -0.007 vs arm Q, paired over 20 task(s)
- R: correctness -0.007 is below arm(s) P (+0.026)
- R: cost interval [-0.0581, +0.0115] vs arm Q is not entirely below zero

## Per cell

| task | key | arm | rep | in | out | cache read | cache create | total tok | cache hit | tools | mcp | graph | tool search | turns | sec | cost $ | score | extra | denials | verify | dirtied | outcome |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | assoc-why-1 | P | 1 | 12 | 1893 | 182211 | 15016 | 199132 | 0.924 | 6 | 0 | 0 | 0 | 7 | 18.3 | 0.1166 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 1 | 10 | 2390 | 148758 | 15354 | 166512 | 0.906 | 5 | 0 | 0 | 0 | 6 | 27.7 | 0.1162 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 1 | 4 | 136 | 36781 | 37164 | 74085 | 0.497 | 1 | 1 | 1 | 0 | 2 | 4.3 | 0.1574 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | P | 2 | 12 | 1963 | 190606 | 5780 | 198361 | 0.971 | 5 | 0 | 0 | 0 | 6 | 20.0 | 0.0820 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 2 | 10 | 1987 | 157805 | 6260 | 166062 | 0.962 | 6 | 0 | 0 | 0 | 7 | 23.7 | 0.0776 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 2 | 4 | 136 | 73945 | 0 | 74085 | 1.000 | 1 | 1 | 1 | 0 | 2 | 4.0 | 0.0162 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | P | 3 | 12 | 2193 | 191378 | 6144 | 199727 | 0.969 | 6 | 0 | 0 | 0 | 7 | 21.0 | 0.0859 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 3 | 10 | 2230 | 157922 | 6514 | 166676 | 0.960 | 6 | 0 | 0 | 0 | 7 | 20.8 | 0.0811 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 3 | 4 | 167 | 73562 | 647 | 74380 | 0.991 | 1 | 1 | 1 | 0 | 2 | 7.1 | 0.0190 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 1 | 10 | 1409 | 152742 | 8270 | 162431 | 0.949 | 4 | 0 | 0 | 0 | 5 | 22.3 | 0.0788 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 1 | 8 | 1465 | 119362 | 8953 | 129788 | 0.930 | 4 | 0 | 0 | 0 | 5 | 19.5 | 0.0754 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 1 | 4 | 143 | 68402 | 5526 | 74075 | 0.925 | 1 | 1 | 1 | 0 | 2 | 6.5 | 0.0372 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 2 | 10 | 1194 | 156434 | 4508 | 162146 | 0.972 | 4 | 0 | 0 | 0 | 5 | 13.1 | 0.0624 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 2 | 10 | 1969 | 157858 | 6394 | 166231 | 0.961 | 5 | 0 | 0 | 0 | 6 | 18.2 | 0.0780 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 2 | 4 | 185 | 73562 | 643 | 74394 | 0.991 | 1 | 1 | 1 | 0 | 2 | 6.2 | 0.0191 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 3 | 10 | 2042 | 156450 | 5100 | 163602 | 0.968 | 5 | 0 | 0 | 0 | 6 | 19.8 | 0.0732 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 3 | 8 | 1519 | 123085 | 4989 | 129601 | 0.961 | 4 | 0 | 0 | 0 | 5 | 15.3 | 0.0609 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 3 | 4 | 143 | 73928 | 0 | 74075 | 1.000 | 1 | 1 | 1 | 0 | 2 | 5.4 | 0.0162 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 1 | 12 | 2220 | 187704 | 9811 | 199747 | 0.950 | 6 | 0 | 0 | 0 | 7 | 43.6 | 0.1001 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 1 | 10 | 2091 | 154332 | 10133 | 166566 | 0.938 | 5 | 0 | 0 | 0 | 6 | 20.0 | 0.0934 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 1 | 4 | 147 | 68402 | 5572 | 74125 | 0.925 | 1 | 1 | 1 | 0 | 2 | 6.6 | 0.0374 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 2 | 10 | 2205 | 161110 | 2235 | 165560 | 0.986 | 7 | 0 | 0 | 0 | 8 | 20.3 | 0.0643 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 2 | 10 | 1819 | 162257 | 1799 | 165885 | 0.989 | 5 | 0 | 0 | 0 | 6 | 18.7 | 0.0590 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 2 | 4 | 137 | 73562 | 402 | 74105 | 0.995 | 1 | 1 | 1 | 0 | 2 | 5.0 | 0.0177 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 3 | 12 | 2157 | 191429 | 6047 | 199645 | 0.969 | 6 | 0 | 0 | 0 | 7 | 27.8 | 0.0852 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 3 | 14 | 2646 | 228170 | 7272 | 238102 | 0.969 | 7 | 0 | 0 | 0 | 8 | 24.9 | 0.1023 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 3 | 4 | 137 | 73964 | 0 | 74105 | 1.000 | 1 | 1 | 1 | 0 | 2 | 4.1 | 0.0162 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 1 | 12 | 2037 | 189467 | 9902 | 201418 | 0.950 | 6 | 0 | 0 | 0 | 7 | 21.0 | 0.0990 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 1 | 10 | 2088 | 154447 | 10190 | 166735 | 0.938 | 5 | 0 | 0 | 0 | 6 | 19.1 | 0.0936 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 1 | 4 | 164 | 68402 | 5548 | 74118 | 0.925 | 1 | 1 | 1 | 0 | 2 | 6.5 | 0.0375 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 2 | 12 | 1817 | 191408 | 5745 | 198982 | 0.971 | 6 | 0 | 0 | 0 | 7 | 26.4 | 0.0806 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 2 | 8 | 2108 | 123117 | 5026 | 130259 | 0.961 | 4 | 0 | 0 | 0 | 5 | 26.3 | 0.0669 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 2 | 4 | 152 | 73562 | 376 | 74094 | 0.995 | 1 | 1 | 1 | 0 | 2 | 4.0 | 0.0177 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 3 | 16 | 2618 | 262697 | 7418 | 272749 | 0.973 | 10 | 0 | 0 | 0 | 11 | 24.4 | 0.1095 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 3 | 10 | 2187 | 158218 | 6390 | 166805 | 0.961 | 5 | 0 | 0 | 0 | 6 | 32.5 | 0.0802 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 3 | 4 | 142 | 73562 | 366 | 74074 | 0.995 | 1 | 1 | 1 | 0 | 2 | 4.1 | 0.0176 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 1 | 10 | 2227 | 154230 | 10320 | 166787 | 0.937 | 5 | 0 | 0 | 0 | 6 | 20.2 | 0.0955 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 1 | 10 | 2976 | 154855 | 11489 | 169330 | 0.931 | 5 | 0 | 0 | 0 | 6 | 31.3 | 0.1078 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 1 | 4 | 415 | 68448 | 5765 | 74632 | 0.922 | 1 | 1 | 1 | 0 | 2 | 9.9 | 0.0409 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 2 | 14 | 4712 | 244382 | 15843 | 264951 | 0.939 | 7 | 0 | 0 | 0 | 8 | 49.8 | 0.1605 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 2 | 22 | 10551 | 409406 | 17802 | 437781 | 0.958 | 11 | 0 | 0 | 0 | 12 | 109.9 | 0.2597 | 0.00 | 5 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 2 | 4 | 409 | 73654 | 554 | 74621 | 0.993 | 1 | 1 | 1 | 0 | 2 | 9.9 | 0.0210 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 3 | 12 | 2340 | 192923 | 7039 | 202314 | 0.965 | 6 | 0 | 0 | 0 | 7 | 22.2 | 0.0913 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 3 | 14 | 3278 | 232509 | 8085 | 243886 | 0.966 | 7 | 0 | 0 | 0 | 8 | 29.6 | 0.1127 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 3 | 4 | 418 | 73654 | 563 | 74639 | 0.992 | 1 | 1 | 1 | 0 | 2 | 9.6 | 0.0212 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 1 | 10 | 1864 | 154769 | 10673 | 167316 | 0.935 | 5 | 0 | 0 | 0 | 6 | 23.6 | 0.0934 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 1 | 14 | 3014 | 225340 | 11577 | 239945 | 0.951 | 7 | 0 | 0 | 0 | 8 | 36.3 | 0.1226 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 1 | 4 | 338 | 68448 | 5637 | 74427 | 0.924 | 1 | 1 | 1 | 0 | 2 | 8.9 | 0.0396 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 2 | 12 | 1494 | 190005 | 6235 | 197746 | 0.968 | 6 | 0 | 0 | 0 | 7 | 21.8 | 0.0790 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 2 | 14 | 4409 | 230967 | 8115 | 243505 | 0.966 | 7 | 0 | 0 | 0 | 8 | 52.1 | 0.1239 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 2 | 4 | 330 | 73654 | 424 | 74412 | 0.994 | 1 | 1 | 1 | 0 | 2 | 7.0 | 0.0197 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 3 | 10 | 2024 | 156770 | 5983 | 164787 | 0.963 | 5 | 0 | 0 | 0 | 6 | 25.4 | 0.0766 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 3 | 8 | 2422 | 123155 | 6313 | 131898 | 0.951 | 4 | 0 | 0 | 0 | 5 | 22.5 | 0.0752 | 0.80 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 3 | 4 | 322 | 73654 | 417 | 74397 | 0.994 | 1 | 1 | 1 | 0 | 2 | 7.1 | 0.0196 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 1 | 10 | 1755 | 153418 | 9603 | 164786 | 0.941 | 5 | 0 | 0 | 0 | 6 | 23.1 | 0.0878 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 1 | 8 | 2405 | 119488 | 10122 | 132023 | 0.922 | 4 | 0 | 0 | 0 | 5 | 21.5 | 0.0895 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 1 | 4 | 375 | 68448 | 5705 | 74532 | 0.923 | 1 | 1 | 1 | 0 | 2 | 7.2 | 0.0403 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 2 | 12 | 2314 | 192009 | 6612 | 200947 | 0.967 | 6 | 0 | 0 | 0 | 7 | 33.1 | 0.0891 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 2 | 16 | 4465 | 268232 | 9518 | 282231 | 0.966 | 8 | 0 | 0 | 0 | 9 | 40.8 | 0.1375 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 2 | 4 | 374 | 73654 | 497 | 74529 | 0.993 | 1 | 1 | 1 | 0 | 2 | 7.6 | 0.0205 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 3 | 12 | 2472 | 192096 | 6412 | 200992 | 0.968 | 6 | 0 | 0 | 0 | 7 | 29.3 | 0.0899 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 3 | 12 | 3109 | 194517 | 7006 | 204644 | 0.965 | 6 | 0 | 0 | 0 | 7 | 29.0 | 0.0992 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 3 | 4 | 395 | 73654 | 519 | 74572 | 0.993 | 1 | 1 | 1 | 0 | 2 | 11.4 | 0.0208 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 1 | 8 | 1949 | 119483 | 9498 | 130938 | 0.926 | 4 | 0 | 0 | 0 | 5 | 16.9 | 0.0825 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 1 | 8 | 3223 | 119480 | 11033 | 133744 | 0.915 | 4 | 0 | 0 | 0 | 5 | 27.0 | 0.1014 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 1 | 4 | 376 | 68448 | 5674 | 74502 | 0.923 | 1 | 1 | 1 | 0 | 2 | 7.6 | 0.0402 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 2 | 10 | 1848 | 157440 | 5981 | 165279 | 0.963 | 5 | 0 | 0 | 0 | 6 | 25.3 | 0.0750 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 2 | 10 | 2538 | 158419 | 7136 | 168103 | 0.957 | 5 | 0 | 0 | 0 | 6 | 22.3 | 0.0867 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 2 | 4 | 320 | 73654 | 411 | 74389 | 0.994 | 1 | 1 | 1 | 0 | 2 | 6.9 | 0.0196 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 3 | 16 | 2262 | 266075 | 7275 | 275628 | 0.973 | 8 | 0 | 0 | 0 | 9 | 22.5 | 0.1061 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 3 | 26 | 7135 | 469682 | 14113 | 490956 | 0.971 | 13 | 0 | 0 | 0 | 14 | 67.3 | 0.2229 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 3 | 4 | 319 | 73654 | 409 | 74386 | 0.994 | 1 | 1 | 1 | 0 | 2 | 9.7 | 0.0196 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 1 | 12 | 2489 | 188192 | 10222 | 200915 | 0.948 | 6 | 0 | 0 | 0 | 7 | 25.9 | 0.1046 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 1 | 12 | 4739 | 191701 | 12961 | 209413 | 0.937 | 6 | 0 | 0 | 0 | 7 | 45.8 | 0.1387 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 1 | 10 | 3583 | 186562 | 24000 | 214155 | 0.886 | 5 | 5 | 5 | 0 | 6 | 39.6 | 0.1692 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 2 | 10 | 2034 | 157585 | 6426 | 166055 | 0.961 | 5 | 0 | 0 | 0 | 6 | 20.9 | 0.0787 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 2 | 16 | 3433 | 268772 | 3761 | 275982 | 0.986 | 8 | 0 | 0 | 0 | 9 | 39.7 | 0.1043 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 2 | 10 | 4789 | 250123 | 26182 | 281104 | 0.905 | 25 | 25 | 25 | 0 | 26 | 60.6 | 0.2027 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 3 | 10 | 1899 | 158427 | 5806 | 166142 | 0.965 | 5 | 0 | 0 | 0 | 6 | 20.5 | 0.0750 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 3 | 14 | 4083 | 233355 | 9446 | 246898 | 0.961 | 7 | 0 | 0 | 0 | 8 | 49.9 | 0.1264 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 3 | 18 | 5511 | 422743 | 22830 | 451102 | 0.949 | 9 | 5 | 5 | 0 | 10 | 59.8 | 0.2310 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 1 | 12 | 3719 | 188232 | 10797 | 202760 | 0.946 | 6 | 0 | 0 | 0 | 7 | 41.2 | 0.1192 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 1 | 14 | 4538 | 227135 | 13967 | 245654 | 0.942 | 7 | 0 | 0 | 0 | 8 | 39.3 | 0.1478 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 1 | 8 | 2505 | 174091 | 24153 | 200757 | 0.878 | 4 | 4 | 4 | 0 | 5 | 31.3 | 0.1565 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 2 | 12 | 2197 | 192299 | 6617 | 201125 | 0.967 | 6 | 0 | 0 | 0 | 7 | 28.9 | 0.0880 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 2 | 12 | 3733 | 197187 | 9733 | 210665 | 0.953 | 6 | 0 | 0 | 0 | 7 | 35.7 | 0.1168 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 2 | 6 | 2604 | 126135 | 19574 | 148319 | 0.866 | 3 | 3 | 3 | 0 | 4 | 23.7 | 0.1296 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 3 | 12 | 2877 | 194725 | 8193 | 205807 | 0.960 | 6 | 0 | 0 | 0 | 7 | 33.9 | 0.1016 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 3 | 14 | 4514 | 233338 | 10197 | 248063 | 0.958 | 7 | 0 | 0 | 0 | 8 | 57.9 | 0.1337 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 3 | 10 | 4396 | 234580 | 21072 | 260058 | 0.917 | 5 | 5 | 5 | 0 | 6 | 47.8 | 0.1752 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 1 | 10 | 2056 | 154201 | 10810 | 167077 | 0.934 | 5 | 0 | 0 | 0 | 6 | 20.3 | 0.0958 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 1 | 14 | 3854 | 227259 | 12520 | 243647 | 0.948 | 7 | 0 | 0 | 0 | 8 | 33.7 | 0.1352 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 1 | 6 | 2083 | 120813 | 26354 | 149256 | 0.821 | 3 | 3 | 3 | 0 | 4 | 18.3 | 0.1504 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 2 | 10 | 2129 | 157808 | 6495 | 166442 | 0.960 | 5 | 0 | 0 | 0 | 6 | 30.1 | 0.0800 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 2 | 12 | 4729 | 194636 | 9405 | 208782 | 0.954 | 6 | 0 | 0 | 0 | 7 | 41.1 | 0.1250 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 2 | 4 | 4143 | 73692 | 6223 | 84062 | 0.922 | 2 | 2 | 2 | 0 | 3 | 32.5 | 0.0811 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 3 | 14 | 3120 | 228908 | 7483 | 239525 | 0.968 | 7 | 0 | 0 | 0 | 8 | 46.4 | 0.1081 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 3 | 14 | 3047 | 227303 | 7661 | 238025 | 0.967 | 7 | 0 | 0 | 0 | 8 | 41.1 | 0.1077 | 0.93 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 3 | 4 | 2553 | 73692 | 6039 | 82288 | 0.924 | 2 | 2 | 2 | 0 | 3 | 21.1 | 0.0644 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 1 | 10 | 2770 | 155021 | 10786 | 168587 | 0.935 | 5 | 0 | 0 | 0 | 6 | 33.5 | 0.1030 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 1 | 10 | 3976 | 155591 | 12593 | 172170 | 0.925 | 5 | 0 | 0 | 0 | 6 | 32.3 | 0.1224 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 1 | 10 | 2932 | 202719 | 25175 | 230836 | 0.889 | 8 | 8 | 8 | 0 | 9 | 31.2 | 0.1706 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 2 | 12 | 2933 | 193075 | 7341 | 203361 | 0.963 | 6 | 0 | 0 | 0 | 7 | 27.7 | 0.0984 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 2 | 12 | 3689 | 195127 | 9186 | 208014 | 0.955 | 6 | 0 | 0 | 0 | 7 | 31.4 | 0.1138 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 2 | 8 | 1367 | 152444 | 3679 | 157498 | 0.976 | 4 | 3 | 3 | 0 | 5 | 17.7 | 0.0589 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 3 | 10 | 2234 | 158014 | 6943 | 167201 | 0.958 | 5 | 0 | 0 | 0 | 6 | 27.6 | 0.0828 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 3 | 16 | 4475 | 272744 | 5569 | 282804 | 0.980 | 8 | 0 | 0 | 0 | 9 | 39.4 | 0.1227 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 3 | 4 | 2174 | 73692 | 6416 | 82286 | 0.920 | 2 | 2 | 2 | 0 | 3 | 21.5 | 0.0622 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 1 | 12 | 2019 | 187730 | 10111 | 199872 | 0.949 | 6 | 0 | 0 | 0 | 7 | 26.1 | 0.0993 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 1 | 10 | 2731 | 153887 | 10283 | 166911 | 0.937 | 5 | 0 | 0 | 0 | 6 | 26.0 | 0.1004 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 1 | 18 | 5243 | 435796 | 27135 | 468192 | 0.941 | 11 | 11 | 11 | 0 | 12 | 69.2 | 0.2482 | 0.00 | 5 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 2 | 8 | 1653 | 123516 | 5725 | 130902 | 0.956 | 4 | 0 | 0 | 0 | 5 | 16.8 | 0.0652 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 2 | 16 | 3373 | 260032 | 8455 | 271876 | 0.969 | 8 | 0 | 0 | 0 | 9 | 33.5 | 0.1207 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 2 | 18 | 2300 | 350799 | 8187 | 361304 | 0.977 | 10 | 6 | 6 | 0 | 11 | 38.4 | 0.1259 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 3 | 12 | 2368 | 192835 | 6744 | 201959 | 0.966 | 6 | 0 | 0 | 0 | 7 | 21.7 | 0.0904 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 3 | 12 | 2684 | 193856 | 7027 | 203579 | 0.965 | 6 | 0 | 0 | 0 | 7 | 26.6 | 0.0948 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 3 | 24 | 6598 | 606308 | 28118 | 641048 | 0.956 | 15 | 13 | 13 | 0 | 16 | 98.6 | 0.2998 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 1 | 12 | 3751 | 189277 | 11177 | 204217 | 0.944 | 6 | 0 | 0 | 0 | 7 | 34.5 | 0.1212 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 1 | 18 | 4238 | 289762 | 12168 | 306186 | 0.960 | 8 | 0 | 0 | 0 | 9 | 51.1 | 0.1502 | 0.83 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 1 | 22 | 2347 | 554834 | 33174 | 590377 | 0.944 | 11 | 7 | 7 | 0 | 13 | 37.5 | 0.2672 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 2 | 12 | 2537 | 191646 | 6242 | 200437 | 0.968 | 6 | 0 | 0 | 0 | 7 | 31.5 | 0.0898 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 2 | 16 | 5023 | 267417 | 10351 | 282807 | 0.963 | 8 | 0 | 0 | 0 | 9 | 42.1 | 0.1463 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 2 | 18 | 1878 | 393029 | 22464 | 417389 | 0.946 | 8 | 6 | 6 | 0 | 9 | 30.4 | 0.1873 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 3 | 16 | 2740 | 256270 | 6505 | 265531 | 0.975 | 7 | 0 | 0 | 0 | 8 | 162.8 | 0.1058 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 3 | 14 | 4341 | 228252 | 8434 | 241041 | 0.964 | 7 | 0 | 0 | 0 | 8 | 42.4 | 0.1239 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 3 | 20 | 2581 | 464167 | 28827 | 495595 | 0.942 | 9 | 5 | 5 | 1 | 11 | 37.9 | 0.2340 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | P | 1 | 14 | 2884 | 223577 | 10887 | 237362 | 0.954 | 7 | 0 | 0 | 0 | 8 | 27.1 | 0.1183 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 1 | 10 | 2272 | 153858 | 10399 | 166539 | 0.937 | 5 | 0 | 0 | 0 | 6 | 24.4 | 0.0962 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 1 | 18 | 2441 | 417617 | 28250 | 448326 | 0.937 | 9 | 7 | 7 | 0 | 10 | 37.1 | 0.2210 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | P | 2 | 10 | 1800 | 157810 | 5834 | 165454 | 0.964 | 5 | 0 | 0 | 0 | 6 | 18.2 | 0.0740 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 2 | 12 | 3008 | 193365 | 7354 | 203739 | 0.963 | 6 | 0 | 0 | 0 | 7 | 26.9 | 0.0993 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 2 | 16 | 2736 | 408862 | 40119 | 451733 | 0.911 | 10 | 7 | 7 | 0 | 12 | 37.4 | 0.2696 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | P | 3 | 16 | 2426 | 260787 | 6728 | 269957 | 0.975 | 7 | 0 | 0 | 0 | 8 | 35.4 | 0.1045 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 3 | 10 | 2722 | 158376 | 6954 | 168062 | 0.958 | 5 | 0 | 0 | 0 | 6 | 26.0 | 0.0878 | 0.86 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 3 | 14 | 1493 | 269562 | 7090 | 278159 | 0.974 | 7 | 5 | 5 | 0 | 8 | 31.7 | 0.0972 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | P | 1 | 14 | 2007 | 221935 | 10852 | 234808 | 0.953 | 6 | 0 | 0 | 0 | 7 | 21.5 | 0.1090 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 1 | 20 | 4250 | 329470 | 13151 | 346891 | 0.962 | 9 | 0 | 0 | 0 | 10 | 40.7 | 0.1622 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 1 | 12 | 2122 | 251098 | 23463 | 276695 | 0.914 | 6 | 6 | 6 | 0 | 7 | 34.2 | 0.1653 | 0.00 | 5 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | P | 2 | 12 | 2191 | 191065 | 6129 | 199397 | 0.969 | 5 | 0 | 0 | 0 | 6 | 22.6 | 0.0858 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 2 | 16 | 3359 | 258854 | 7552 | 269781 | 0.972 | 7 | 0 | 0 | 0 | 8 | 31.9 | 0.1167 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 2 | 20 | 3945 | 490573 | 26952 | 521490 | 0.948 | 11 | 7 | 7 | 0 | 13 | 54.7 | 0.2454 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | P | 3 | 12 | 2446 | 191830 | 6157 | 200445 | 0.969 | 6 | 0 | 0 | 0 | 7 | 29.2 | 0.0886 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 3 | 14 | 2849 | 232099 | 11197 | 246159 | 0.954 | 7 | 0 | 0 | 0 | 8 | 29.1 | 0.1208 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 3 | 24 | 4715 | 603666 | 28201 | 636606 | 0.955 | 11 | 8 | 8 | 0 | 12 | 85.8 | 0.2807 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 1 | 10 | 2563 | 153993 | 10268 | 166834 | 0.937 | 5 | 0 | 0 | 0 | 6 | 22.8 | 0.0986 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 1 | 16 | 5088 | 268715 | 17905 | 291724 | 0.938 | 8 | 0 | 0 | 0 | 9 | 52.1 | 0.1774 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 1 | 4 | 471 | 68433 | 5993 | 74901 | 0.919 | 1 | 1 | 1 | 0 | 2 | 6.8 | 0.0424 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 2 | 14 | 2128 | 225494 | 5713 | 233349 | 0.975 | 7 | 0 | 0 | 0 | 8 | 22.2 | 0.0903 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 2 | 14 | 5010 | 234243 | 9256 | 248523 | 0.962 | 7 | 0 | 0 | 0 | 8 | 51.8 | 0.1351 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 2 | 6 | 701 | 110855 | 1135 | 112697 | 0.990 | 2 | 2 | 2 | 0 | 3 | 9.5 | 0.0337 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 3 | 16 | 3628 | 264157 | 7578 | 275379 | 0.972 | 8 | 0 | 0 | 0 | 9 | 39.4 | 0.1206 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 3 | 12 | 4260 | 195247 | 8619 | 208138 | 0.958 | 6 | 0 | 0 | 0 | 7 | 38.0 | 0.1172 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 3 | 4 | 438 | 73624 | 697 | 74763 | 0.991 | 1 | 1 | 1 | 0 | 2 | 5.9 | 0.0219 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 1 | 14 | 3446 | 223694 | 11035 | 238189 | 0.953 | 7 | 0 | 0 | 0 | 8 | 33.6 | 0.1245 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 1 | 10 | 3081 | 155651 | 10964 | 169706 | 0.934 | 5 | 0 | 0 | 0 | 6 | 28.1 | 0.1069 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 1 | 4 | 538 | 68433 | 6035 | 75010 | 0.919 | 1 | 1 | 1 | 0 | 2 | 7.0 | 0.0432 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 2 | 16 | 3264 | 264258 | 7866 | 275404 | 0.971 | 8 | 0 | 0 | 0 | 9 | 31.5 | 0.1181 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 2 | 16 | 6344 | 272611 | 11146 | 290117 | 0.961 | 8 | 0 | 0 | 0 | 9 | 61.3 | 0.1637 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 2 | 30 | 4387 | 590723 | 6729 | 601869 | 0.989 | 14 | 14 | 14 | 0 | 15 | 53.3 | 0.1890 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 3 | 12 | 2652 | 195613 | 10596 | 208873 | 0.949 | 6 | 0 | 0 | 0 | 7 | 23.4 | 0.1091 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 3 | 12 | 2832 | 192967 | 6969 | 202780 | 0.965 | 7 | 0 | 0 | 0 | 8 | 26.4 | 0.0959 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 3 | 4 | 509 | 73624 | 815 | 74952 | 0.989 | 1 | 1 | 1 | 0 | 2 | 9.4 | 0.0231 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 1 | 10 | 1958 | 153195 | 9271 | 164434 | 0.943 | 5 | 0 | 0 | 0 | 6 | 29.6 | 0.0884 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 1 | 14 | 4666 | 227216 | 13019 | 244915 | 0.946 | 7 | 0 | 0 | 0 | 8 | 38.7 | 0.1453 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 1 | 4 | 329 | 68447 | 5623 | 74403 | 0.924 | 1 | 1 | 1 | 0 | 2 | 6.0 | 0.0395 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 2 | 12 | 3105 | 193255 | 7171 | 203543 | 0.964 | 6 | 0 | 0 | 0 | 7 | 40.3 | 0.0995 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 2 | 10 | 2751 | 158496 | 7009 | 168266 | 0.958 | 5 | 0 | 0 | 0 | 6 | 22.8 | 0.0884 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 2 | 4 | 272 | 73652 | 368 | 74296 | 0.995 | 1 | 1 | 1 | 0 | 2 | 4.9 | 0.0189 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 3 | 12 | 2525 | 193171 | 7040 | 202748 | 0.965 | 6 | 0 | 0 | 0 | 7 | 26.7 | 0.0932 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 3 | 10 | 2994 | 158429 | 7113 | 168546 | 0.957 | 5 | 0 | 0 | 0 | 6 | 25.6 | 0.0912 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 3 | 4 | 306 | 73652 | 403 | 74365 | 0.995 | 1 | 1 | 1 | 0 | 2 | 12.7 | 0.0194 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 1 | 12 | 2309 | 188686 | 10279 | 201286 | 0.948 | 6 | 0 | 0 | 0 | 7 | 29.6 | 0.1031 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 1 | 14 | 4051 | 229744 | 12619 | 246428 | 0.948 | 7 | 0 | 0 | 0 | 8 | 35.9 | 0.1381 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 1 | 4 | 305 | 68447 | 5582 | 74338 | 0.924 | 1 | 1 | 1 | 0 | 2 | 7.0 | 0.0391 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 2 | 12 | 2104 | 191340 | 6013 | 199469 | 0.970 | 6 | 0 | 0 | 0 | 7 | 19.9 | 0.0845 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 2 | 12 | 4535 | 195834 | 9340 | 209721 | 0.954 | 6 | 0 | 0 | 0 | 7 | 51.2 | 0.1230 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 2 | 4 | 308 | 73652 | 408 | 74372 | 0.994 | 1 | 1 | 1 | 0 | 2 | 5.1 | 0.0195 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 3 | 14 | 3110 | 229817 | 8276 | 241217 | 0.965 | 7 | 0 | 0 | 0 | 8 | 31.8 | 0.1113 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 3 | 18 | 3460 | 302242 | 8266 | 313986 | 0.973 | 9 | 0 | 0 | 0 | 10 | 35.4 | 0.1292 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 3 | 4 | 391 | 73652 | 494 | 74541 | 0.993 | 1 | 1 | 1 | 0 | 2 | 8.4 | 0.0206 | 1.00 | 0 | 0 | - | - | success |

## Aggregate (mean per arm)

| metric | arm P (files + grep) | arm Q (sqlite) | arm R (graph) | P vs Q | R vs Q |
|---|---|---|---|---|---|
| total tokens | 200034 | 221063 | 186215 | -9.5% | -15.8% |
| input tokens | 12 | 13 | 8 | -6.8% | -35.5% |
| output tokens | 2385 | 3546 | 1536 | -32.7% | -56.7% |
| cache read | 189645 | 208103 | 174157 | -8.9% | -16.3% |
| cache create | 7993 | 9402 | 10513 | -15.0% | +11.8% |
| cache hit ratio | 0.959 | 0.955 | 0.945 | +0.4% | -1.0% |
| tool calls | 5.90 | 6.38 | 4.00 | -7.6% | -37.3% |
| mcp calls | 0.00 | 0.00 | 3.42 | n/a | n/a |
| graph calls | 0.00 | 0.00 | 3.42 | n/a | n/a |
| tool search calls | 0.00 | 0.00 | 0.02 | n/a | n/a |
| adoption | 0.00 | 0.00 | 1.00 | n/a | n/a |
| turns | 6.90 | 7.38 | 5.07 | -6.5% | -31.4% |
| seconds | 29.1 | 35.2 | 21.7 | -17.3% | -38.4% |
| cost $ | 0.0949 | 0.1158 | 0.0923 | -18.1% | -20.3% |
| score | 1.000 | 0.974 | 0.967 | +2.7% | -0.7% |
| wrong extra | 0.00 | 0.08 | 0.17 | -100.0% | +100.0% |

arm P: 60 cells; arm Q: 60 cells; arm R: 60 cells; timeouts 0; errors 0; cells with no cost 0

## Deltas vs arm Q

Paired by task: one difference per task, arm mean minus arm Q mean. The interval is a 95% percentile bootstrap over those differences (2000 resamples, seeded). An interval that does not contain 0 is the only kind worth quoting. `tasks` is how many tasks contributed a difference — a task where either side recorded nothing contributes none.

| arm | metric | arm mean | arm Q mean | delta | 95% CI | tasks |
|---|---|---|---|---|---|---|
| P | score | 1.000 | 0.974 | 0.0264 | [0.0040, 0.0639] | 20 |
| P | cost $ | 0.0949 | 0.1158 | -0.02094 | [-0.02861, -0.01260] | 20 |
| P | total tokens | 200034 | 221063 | -21029.5 | [-38685.4, -2746.6] | 20 |
| P | turns | 6.90 | 7.38 | -0.483 | [-0.933, 0.017] | 20 |
| P | cache hit ratio | 0.959 | 0.955 | 0.0040 | [0.0017, 0.0063] | 20 |
| R | score | 0.967 | 0.974 | -0.0070 | [-0.0711, 0.0483] | 20 |
| R | cost $ | 0.0923 | 0.1158 | -0.02355 | [-0.05815, 0.01148] | 20 |
| R | total tokens | 186215 | 221063 | -34848.5 | [-96315.1, 33715.3] | 20 |
| R | turns | 5.07 | 7.38 | -2.317 | [-4.000, -0.483] | 20 |
| R | cache hit ratio | 0.945 | 0.955 | -0.0096 | [-0.0275, 0.0056] | 20 |

## Correctness by task (mean score over reps)

| task | arm P | arm Q | arm R | best |
|---|---|---|---|---|
| 1 | 1.00 | 1.00 | 1.00 | tie |
| 2 | 1.00 | 1.00 | 1.00 | tie |
| 3 | 1.00 | 1.00 | 1.00 | tie |
| 4 | 1.00 | 1.00 | 1.00 | tie |
| 5 | 1.00 | 0.67 | 1.00 | P |
| 6 | 1.00 | 0.93 | 1.00 | P |
| 7 | 1.00 | 1.00 | 1.00 | tie |
| 8 | 1.00 | 1.00 | 1.00 | tie |
| 9 | 1.00 | 1.00 | 1.00 | tie |
| 10 | 1.00 | 1.00 | 1.00 | tie |
| 11 | 1.00 | 0.98 | 1.00 | P |
| 12 | 1.00 | 1.00 | 1.00 | tie |
| 13 | 1.00 | 1.00 | 0.67 | P |
| 14 | 1.00 | 0.94 | 1.00 | P |
| 15 | 1.00 | 0.95 | 1.00 | P |
| 16 | 1.00 | 1.00 | 0.67 | P |
| 17 | 1.00 | 1.00 | 1.00 | tie |
| 18 | 1.00 | 1.00 | 1.00 | tie |
| 19 | 1.00 | 1.00 | 1.00 | tie |
| 20 | 1.00 | 1.00 | 1.00 | tie |
| **mean** | **1.000** | **0.974** | **0.967** | |

## Sub-1.0 cells

One line per cell that did not score 1.0. `classification` is filled in by hand after reading the cell's stream file: `tool` (the graph answered badly or not at all), `model` (the agent had what it needed and went wrong anyway), or `grader` (the answer is right and the check is wrong).

| task | key | arm | rep | score | missed | verify | outcome | classification |
|---|---|---|---|---|---|---|---|---|
| 5 | assoc-multihop-1 | Q | 2 | 0.00 | set 27/27 -5 | - | success |  |
| 13 | assoc-timetravel-1 | R | 1 | 0.00 | set 2/8 -5 | - | success |  |
| 16 | assoc-timetravel-4 | R | 1 | 0.00 | set 0/8 -5 | - | success |  |
| 6 | assoc-multihop-2 | Q | 3 | 0.80 | set 8/10 | - | success |  |
| 14 | assoc-timetravel-2 | Q | 1 | 0.83 | set 5/6 | - | success |  |
| 15 | assoc-timetravel-3 | Q | 3 | 0.86 | set 6/7 | - | success |  |
| 11 | assoc-retraction-3 | Q | 3 | 0.93 | set 13/14 | - | success |  |

## Tool adoption

Sessions that made at least one MCP call, and which mushroomdb tools they used.

| arm | sessions | >=1 mcp call | adoption | mcp calls total |
|---|---|---|---|---|
| P | 60 | 0 | 0% | 0 |
| Q | 60 | 0 | 0% | 0 |
| R | 60 | 60 | 100% | 205 |

- arm P: no MCP tools used
- arm Q: no MCP tools used
- arm R: `query` x61, `explain_association` x36, `edges_at` x21, `node_info` x19, `node_edges` x15, `edge_history` x13, `what_if` x13, `node_history` x12, `stats` x8, `recall` x7

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
**Relationship types (talent-000141 ↔ company-000153):**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

**Shared specialties:**
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
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

**Shared specialties:**
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
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

**Common specialties:**
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
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Specialties in common:**
multi-family
residential
```

### task 2 rep 1 arm Q (score 1.00)

```
Based on day‑0 data (unchanged by any later history for either entity):

**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Shared specialties:**
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
**Relationships:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Shared specialties:**
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
`talent-000512` and `company-000054` are both `industry: architecture`, and their `specialties` and `design_styles` sets clear the Jaccard thresholds (size and location don't).

**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Common specialties:**
multi-family
residential
```

### task 2 rep 3 arm Q (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Common specialties:**
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
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

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
**Relationship types:**
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

**Shared specialties:**
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

commercial
multi-family
```

### task 3 rep 3 arm P (score 1.00)

```
**Relationships:**
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

**Common specialties:**
multi-family
commercial
```

### task 3 rep 3 arm Q (score 1.00)

```
Relationships between talent-001202 and company-000014:

SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

Common specialties:

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
Neither entity was ever touched by the change history, so day-0 field values (shown above) are also the final values.

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
**Relationship types**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Shared specialties**
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
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Shared specialties:**
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

### task 5 rep 2 arm Q (score 0.00)

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
missed: set 27/27 -5

### task 5 rep 2 arm R (score 1.00)

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
Good, format confirmed. The results are verified.

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

### task 6 rep 3 arm Q (score 0.80)

```
company-000013
company-000039
company-000056
company-000098
company-000135
company-000230
company-000243
company-000320
```
missed: set 8/10

### task 6 rep 3 arm R (score 1.00)

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
Good, the field names match my assumptions (`specialties`, `location` as `[lat, lon]`). The computation is verified.

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
company-000141
company-000321
company-000193
company-000343
company-000405
company-000013
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
company-000141
company-000321
company-000193
company-000343
company-000405
company-000013
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
This checks out. The final answer (companies linked by all three relationships to ≥9 qualifying talents):

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
company-000141
company-000321
company-000193
company-000343
company-000405
company-000013
company-000112
company-000098
company-000006
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
Good, format matches my parsing. The results are verified.

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
Talents no longer linked to company-000239 by all three edge types after the specialties change:

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
Good, that confirms the working directory was correct.

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
No changes recorded for that talent — day-0 data stands unchanged, consistent with the script. The computation looks solid.

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
This confirms my calculation. The talents that would lose SPECIALTY_MATCH (and thus no longer satisfy all three) are:

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
This confirms the results. The talents linked by all three relationships to `company-000376` that would lose the `SPECIALTY_MATCH` link under the new specialties:

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
Good, normalization matches. The talents that would drop out are:

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
This matches expectations — talents with size_bucket=5 lose the link (|5-3|=2>1), while size_bucket=4 ones remain (|4-3|=1).

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

### task 11 rep 3 arm Q (score 0.93)

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
```
missed: set 13/14

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
Confirmed: bucket-1 talents drop out (|1−3|=2>1), bucket-2 talents remain (|2−3|=1≤1).

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
This is consistent with day-0 data unchanged for these talents. The final answer:

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
`talent-000501` at 2026-06-19 (day 18) was linked to these companies by all three relationships:

company-000057
company-000069
company-000129
company-000165
company-000201
company-000237
company-000345
company-000369
```

### task 13 rep 1 arm R (score 0.00)

```
Stable from commit 160 through 587, so the exact date→commit mapping doesn't matter here as long as 2026-06-19 falls after commit 155 (which it does under any reasonable mapping, since it's well before the dataset's end date 2026-09-11).

company-000069
company-000093
company-000165
company-000249
company-000297
company-000309
company-000393
```
missed: set 2/8 -5

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
Good, the script correctly handles inserts/deletes/set_prop up to 2026-06-19. The results stand.

**Companies linked to talent-000501 by all three relationships on 2026-06-19:**

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
That change happens on day 19 (after our target date), so day-0 state applies for talent-000501 on 2026-06-19. Results confirmed.

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

### task 14 rep 1 arm Q (score 0.83)

```
These all check out (industry match, specialty Jaccard ≥0.15, within 160.9 km). Companies linked to `talent-001026` by all three relations on 2026-07-12:

company-000042
company-000066
company-000138
company-000246
company-000354
```
missed: set 5/6

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
All six checks confirm.

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
Confirmed day 42 = 2026-07-13.

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
Good — that change is on day 43, after our target date, confirming day-0 talent data is correct for 2026-07-13. The industry flips the next day, which is exactly why the question specifies "not the end of the history."

company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
company-000401
```

### task 15 rep 3 arm Q (score 0.86)

```
company-000017
company-000101
company-000281
company-000293
company-000329
company-000389
```
missed: set 6/7

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
Looks consistent (no changes recorded, day-0 state applies). The results:

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
Good, the two differ (company-000420 was added later), confirming commit 528 correctly captures the 2026-07-14 state.

company-000012
company-000084
company-000144
company-000180
company-000204
company-000228
company-000276
company-000300
company-000324
company-000396
```
missed: set 0/8 -5

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
400 - 1 + 11 = 410. That matches.

company-000048
company-000120
company-000132
company-000192
company-000288
company-000294
company-000312
company-000360
```

### task 16 rep 2 arm R (score 1.00)

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

### task 16 rep 3 arm R (score 1.00)

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

### task 17 rep 1 arm P (score 1.00)

```
Same result. Now output only these keys.

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
talent-000104 is linked to (as a recruiter can see, via Job entities only) the following, by **both** INDUSTRY_ALIGNMENT and SPECIALTY_MATCH:

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
As recruiter, only Job entities are visible (not Company). Filtering to job-* keys:

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
Output confirms the computation. Here are the job keys linked to `talent-000104` by both `INDUSTRY_ALIGNMENT` and `SPECIALTY_MATCH` in the final state:

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
As recruiter, only Job entities are visible (Company is not). Filtering to those:

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
These check out. Since `recruiter` can only see Talent and Job entities (not Company), Company links are excluded.

talent-000104 is linked to these Job entities by both INDUSTRY_ALIGNMENT and SPECIALTY_MATCH (final state):

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
Confirmed correct (industry both `architecture`, shared specialty `educational`, Jaccard = 1/4 ≥ 0.15).

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
job-000002 (jaccard 1/3=0.33), job-000009 (1/2=0.5), job-000203 (1/2=0.5) — checks out.

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
35 jobs out of ~200 is plausible. Final answer — jobs linked to talent-000450 by both `INDUSTRY_ALIGNMENT` and `SPECIALTY_MATCH`:

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
Applying every change through day 89, `talent-000450` (industry: interior-design; specialties: industrial, landscape) is linked to these entities by both `INDUSTRY_ALIGNMENT` and `SPECIALTY_MATCH` (companies are excluded — the `recruiter` role only sees Talent and Job):

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
This matches the Job subset from the earlier `node_edges` result (with `key(a)` filter instead of `key(a) = 'talent-000450'` on the node itself, which had a parser bug). This is the recruiter-visible set (Company entities are excluded since recruiter cannot see Company).

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
Good, format matches my parsing. The 35 jobs found are correct — these are the Job entities linked to `talent-000450` by both `INDUSTRY_ALIGNMENT` and `SPECIALTY_MATCH` at the final state, visible to the `recruiter` role (Company-linked matches are excluded since recruiters can't see Company entities).

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
All checks confirmed (industry match, specialties Jaccard ≥ 0.15, distance ≤ 160.9 km).

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
As `recruiter`, I can only see Talent and Job entities (not Company). Filtering to those:

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
All checks confirm the matches (industry_alignment via `architecture`, specialty overlap ≥0.15, and location within 160.9 km).

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
Good — locations reflect the final state (relocations moved job-000046 and job-000155 near Miami). Final answer:

job-000040
job-000046
job-000052
job-000124
job-000155
```

### task 20 rep 3 arm Q (score 1.00)

```
These all reflect final-state changes correctly since our replay processed them in seq order. The results are confirmed.

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

