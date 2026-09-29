# Agent benchmark run

- run: `20260928T195302Z`
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
| verdict | **PASSED** |
| best arm | R |
| arms under the gate | R |
| cells with no cost recorded | 0 of 180 |

## Per cell

| task | key | arm | rep | in | out | cache read | cache create | total tok | cache hit | tools | mcp | graph | tool search | turns | sec | cost $ | score | extra | denials | verify | dirtied | outcome |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | assoc-why-1 | P | 1 | 12 | 1905 | 111831 | 10911 | 124659 | 0.911 | 5 | 0 | 0 | 0 | 6 | 17.4 | 0.0851 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 1 | 10 | 2399 | 150308 | 16035 | 168752 | 0.903 | 5 | 0 | 0 | 0 | 6 | 21.1 | 0.1182 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 1 | 4 | 178 | 38476 | 39134 | 77792 | 0.496 | 1 | 1 | 1 | 0 | 2 | 16.7 | 0.1660 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | P | 2 | 12 | 1921 | 188721 | 9711 | 200365 | 0.951 | 5 | 0 | 0 | 0 | 6 | 17.9 | 0.0958 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 2 | 12 | 2078 | 194384 | 6438 | 202912 | 0.968 | 6 | 0 | 0 | 0 | 7 | 17.7 | 0.0854 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 2 | 4 | 178 | 77610 | 0 | 77792 | 1.000 | 1 | 1 | 1 | 0 | 2 | 17.7 | 0.0173 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | P | 3 | 12 | 1894 | 193247 | 5953 | 201106 | 0.970 | 6 | 0 | 0 | 0 | 7 | 22.6 | 0.0814 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | Q | 3 | 12 | 3226 | 195768 | 7292 | 206298 | 0.964 | 6 | 0 | 0 | 0 | 7 | 52.0 | 0.1006 | 1.00 | 0 | 0 | - | - | success |
| 1 | assoc-why-1 | R | 3 | 4 | 169 | 76952 | 649 | 77774 | 0.992 | 1 | 1 | 1 | 0 | 2 | 16.2 | 0.0197 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 1 | 10 | 1452 | 149167 | 14074 | 164703 | 0.914 | 5 | 0 | 0 | 0 | 6 | 13.3 | 0.1007 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 1 | 8 | 2341 | 120385 | 10004 | 132738 | 0.923 | 4 | 0 | 0 | 0 | 5 | 18.5 | 0.0875 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 1 | 4 | 185 | 72895 | 4700 | 77784 | 0.939 | 1 | 1 | 1 | 0 | 2 | 16.3 | 0.0352 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 2 | 12 | 1778 | 192859 | 5054 | 199703 | 0.974 | 6 | 0 | 0 | 0 | 7 | 16.3 | 0.0766 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 2 | 8 | 1427 | 124327 | 4967 | 130729 | 0.962 | 4 | 0 | 0 | 0 | 5 | 12.6 | 0.0590 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 2 | 4 | 185 | 77595 | 0 | 77784 | 1.000 | 1 | 1 | 1 | 0 | 2 | 19.3 | 0.0174 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | P | 3 | 2 | 32 | 35668 | 6822 | 42524 | 0.839 | 16 | 0 | 0 | 0 | 1 | 54.2 | 0.2637 | 0.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | Q | 3 | 8 | 1438 | 124329 | 5023 | 130798 | 0.961 | 4 | 0 | 0 | 0 | 5 | 13.7 | 0.0594 | 1.00 | 0 | 0 | - | - | success |
| 2 | assoc-why-2 | R | 3 | 4 | 178 | 76952 | 636 | 77770 | 0.992 | 1 | 1 | 1 | 0 | 2 | 16.7 | 0.0197 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 1 | 14 | 1882 | 224888 | 10350 | 237134 | 0.956 | 7 | 0 | 0 | 0 | 8 | 17.0 | 0.1052 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 1 | 10 | 1839 | 155343 | 9887 | 167079 | 0.940 | 5 | 0 | 0 | 0 | 6 | 15.7 | 0.0890 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 1 | 4 | 179 | 72895 | 4741 | 77819 | 0.939 | 1 | 1 | 1 | 0 | 2 | 21.5 | 0.0353 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 2 | 14 | 2354 | 227964 | 6349 | 236681 | 0.973 | 7 | 0 | 0 | 0 | 8 | 20.2 | 0.0946 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 2 | 12 | 2935 | 195366 | 7955 | 206268 | 0.961 | 6 | 0 | 0 | 0 | 7 | 24.1 | 0.1003 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 2 | 4 | 179 | 77636 | 0 | 77819 | 1.000 | 1 | 1 | 1 | 0 | 2 | 17.9 | 0.0173 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | P | 3 | 12 | 1928 | 193466 | 5913 | 201319 | 0.970 | 6 | 0 | 0 | 0 | 7 | 16.8 | 0.0816 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | Q | 3 | 10 | 2104 | 159292 | 6172 | 167578 | 0.963 | 5 | 0 | 0 | 0 | 6 | 17.6 | 0.0776 | 1.00 | 0 | 0 | - | - | success |
| 3 | assoc-why-3 | R | 3 | 4 | 158 | 76952 | 423 | 77537 | 0.995 | 1 | 1 | 1 | 0 | 2 | 15.9 | 0.0187 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 1 | 10 | 2041 | 154638 | 8782 | 165471 | 0.946 | 5 | 0 | 0 | 0 | 6 | 18.3 | 0.0865 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 1 | 8 | 2125 | 120379 | 9456 | 131968 | 0.927 | 4 | 0 | 0 | 0 | 5 | 18.2 | 0.0832 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 1 | 4 | 161 | 72895 | 4442 | 77502 | 0.943 | 1 | 1 | 1 | 0 | 2 | 17.6 | 0.0340 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 2 | 12 | 1883 | 192049 | 5311 | 199255 | 0.973 | 5 | 0 | 0 | 0 | 6 | 16.9 | 0.0785 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 2 | 8 | 1520 | 124345 | 5015 | 130888 | 0.961 | 4 | 0 | 0 | 0 | 5 | 18.2 | 0.0601 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 2 | 4 | 163 | 76952 | 387 | 77506 | 0.995 | 1 | 1 | 1 | 0 | 2 | 16.3 | 0.0186 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | P | 3 | 12 | 1831 | 193410 | 5732 | 200985 | 0.971 | 6 | 0 | 0 | 0 | 7 | 30.6 | 0.0799 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | Q | 3 | 10 | 2562 | 159452 | 6876 | 168900 | 0.959 | 5 | 0 | 0 | 0 | 6 | 20.7 | 0.0850 | 1.00 | 0 | 0 | - | - | success |
| 4 | assoc-why-4 | R | 3 | 4 | 154 | 76952 | 378 | 77488 | 0.995 | 1 | 1 | 1 | 0 | 2 | 16.1 | 0.0185 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 1 | 10 | 1938 | 155882 | 11059 | 168889 | 0.934 | 5 | 0 | 0 | 0 | 6 | 16.0 | 0.0948 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 1 | 12 | 3467 | 192264 | 11908 | 207651 | 0.942 | 6 | 0 | 0 | 0 | 7 | 44.1 | 0.1208 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 1 | 8 | 1320 | 152434 | 6267 | 160029 | 0.961 | 4 | 4 | 4 | 0 | 5 | 31.3 | 0.0688 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 2 | 18 | 3311 | 309377 | 9813 | 322519 | 0.969 | 9 | 0 | 0 | 0 | 10 | 38.0 | 0.1343 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 2 | 12 | 3229 | 198543 | 8307 | 210091 | 0.960 | 6 | 0 | 0 | 0 | 7 | 26.8 | 0.1053 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 2 | 6 | 1032 | 116551 | 1536 | 119125 | 0.987 | 2 | 2 | 2 | 0 | 3 | 45.9 | 0.0398 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | P | 3 | 20 | 3746 | 342615 | 8952 | 355333 | 0.975 | 10 | 0 | 0 | 0 | 11 | 32.2 | 0.1418 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | Q | 3 | 12 | 4675 | 198337 | 9386 | 212410 | 0.955 | 6 | 0 | 0 | 0 | 7 | 40.5 | 0.1240 | 1.00 | 0 | 0 | - | - | success |
| 5 | assoc-multihop-1 | R | 3 | 14 | 1927 | 293169 | 17924 | 313034 | 0.942 | 8 | 8 | 8 | 0 | 9 | 41.3 | 0.1496 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 1 | 10 | 1790 | 154905 | 10815 | 167520 | 0.935 | 5 | 0 | 0 | 0 | 6 | 15.4 | 0.0922 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 1 | 14 | 2887 | 227339 | 11562 | 241802 | 0.952 | 7 | 0 | 0 | 0 | 8 | 25.5 | 0.1206 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 1 | 8 | 1317 | 152469 | 20721 | 174515 | 0.880 | 3 | 3 | 3 | 0 | 4 | 39.3 | 0.1266 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 2 | 12 | 2769 | 193287 | 6195 | 202263 | 0.969 | 6 | 0 | 0 | 0 | 7 | 37.9 | 0.0912 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 2 | 16 | 4011 | 268144 | 7938 | 280109 | 0.971 | 8 | 0 | 0 | 0 | 9 | 37.7 | 0.1255 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 2 | 10 | 1072 | 196148 | 2030 | 199260 | 0.990 | 5 | 5 | 5 | 0 | 6 | 31.2 | 0.0581 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | P | 3 | 12 | 1993 | 192130 | 5796 | 199931 | 0.971 | 6 | 0 | 0 | 0 | 7 | 17.8 | 0.0816 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | Q | 3 | 10 | 2510 | 160080 | 7134 | 169734 | 0.957 | 5 | 0 | 0 | 0 | 6 | 21.1 | 0.0857 | 1.00 | 0 | 0 | - | - | success |
| 6 | assoc-multihop-2 | R | 3 | 36 | 3970 | 1033314 | 35290 | 1072610 | 0.967 | 17 | 17 | 17 | 0 | 18 | 68.2 | 0.3876 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 1 | 12 | 2267 | 189404 | 10239 | 201922 | 0.949 | 6 | 0 | 0 | 0 | 7 | 19.8 | 0.1015 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 1 | 16 | 6243 | 271551 | 15813 | 293623 | 0.945 | 8 | 0 | 0 | 0 | 9 | 49.9 | 0.1800 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 1 | 10 | 1194 | 191787 | 6005 | 198996 | 0.970 | 4 | 4 | 4 | 0 | 5 | 31.8 | 0.0743 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 2 | 12 | 2354 | 194433 | 7057 | 203856 | 0.965 | 6 | 0 | 0 | 0 | 7 | 19.8 | 0.0907 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 2 | 12 | 3968 | 197285 | 8612 | 209877 | 0.958 | 6 | 0 | 0 | 0 | 7 | 32.7 | 0.1136 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 2 | 20 | 2461 | 399207 | 3740 | 405428 | 0.991 | 9 | 9 | 9 | 0 | 10 | 49.9 | 0.1195 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | P | 3 | 16 | 2673 | 266617 | 7319 | 276625 | 0.973 | 8 | 0 | 0 | 0 | 9 | 22.9 | 0.1094 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | Q | 3 | 18 | 3906 | 311529 | 9399 | 324852 | 0.971 | 9 | 0 | 0 | 0 | 10 | 33.2 | 0.1390 | 1.00 | 0 | 0 | - | - | success |
| 7 | assoc-multihop-3 | R | 3 | 8 | 956 | 156271 | 1881 | 159116 | 0.988 | 3 | 3 | 3 | 0 | 4 | 28.8 | 0.0484 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 1 | 12 | 2232 | 190143 | 10952 | 203339 | 0.946 | 6 | 0 | 0 | 0 | 7 | 21.6 | 0.1042 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 1 | 14 | 4402 | 230039 | 14044 | 248499 | 0.942 | 7 | 0 | 0 | 0 | 8 | 33.2 | 0.1462 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 1 | 8 | 857 | 151702 | 5570 | 158137 | 0.965 | 3 | 3 | 3 | 0 | 4 | 29.4 | 0.0612 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 2 | 10 | 1918 | 159241 | 6314 | 167483 | 0.962 | 5 | 0 | 0 | 0 | 6 | 19.1 | 0.0763 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 2 | 12 | 3250 | 195257 | 7076 | 205595 | 0.965 | 6 | 0 | 0 | 0 | 7 | 26.8 | 0.0999 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 2 | 16 | 2203 | 347603 | 17938 | 367760 | 0.951 | 8 | 8 | 8 | 0 | 9 | 88.3 | 0.1633 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | P | 3 | 22 | 5802 | 393719 | 18824 | 418367 | 0.954 | 11 | 0 | 0 | 0 | 12 | 48.5 | 0.2121 | 1.00 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | Q | 3 | 8 | 3274 | 124519 | 6563 | 134364 | 0.950 | 4 | 0 | 0 | 0 | 5 | 28.1 | 0.0839 | 0.78 | 0 | 0 | - | - | success |
| 8 | assoc-multihop-4 | R | 3 | 10 | 1124 | 196252 | 2192 | 199578 | 0.989 | 5 | 5 | 5 | 0 | 6 | 33.6 | 0.0593 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 1 | 12 | 2543 | 190247 | 10504 | 203306 | 0.948 | 5 | 0 | 0 | 0 | 6 | 23.5 | 0.1055 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 1 | 14 | 3284 | 228102 | 12381 | 243781 | 0.949 | 7 | 0 | 0 | 0 | 8 | 28.1 | 0.1280 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 1 | 4 | 3847 | 72968 | 9172 | 85991 | 0.888 | 2 | 2 | 2 | 0 | 3 | 45.2 | 0.0898 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 2 | 12 | 2297 | 193215 | 6389 | 201913 | 0.968 | 5 | 0 | 0 | 0 | 6 | 20.6 | 0.0872 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 2 | 14 | 4854 | 233985 | 10234 | 249087 | 0.958 | 7 | 0 | 0 | 0 | 8 | 41.8 | 0.1363 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 2 | 4 | 3221 | 77098 | 5076 | 85399 | 0.938 | 2 | 2 | 2 | 0 | 3 | 38.7 | 0.0679 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | P | 3 | 12 | 2353 | 197278 | 3365 | 203008 | 0.983 | 5 | 0 | 0 | 0 | 6 | 20.9 | 0.0765 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | Q | 3 | 14 | 2915 | 234613 | 9228 | 246770 | 0.962 | 7 | 0 | 0 | 0 | 8 | 31.9 | 0.1130 | 1.00 | 0 | 0 | - | - | success |
| 9 | assoc-retraction-1 | R | 3 | 10 | 1983 | 201678 | 4641 | 208312 | 0.978 | 5 | 3 | 3 | 0 | 6 | 33.1 | 0.0787 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 1 | 10 | 1769 | 153078 | 9505 | 164362 | 0.942 | 4 | 0 | 0 | 0 | 5 | 18.0 | 0.0863 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 1 | 12 | 4628 | 193045 | 13838 | 211523 | 0.933 | 6 | 0 | 0 | 0 | 7 | 38.2 | 0.1403 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 1 | 8 | 1175 | 155022 | 7634 | 163839 | 0.953 | 4 | 3 | 3 | 0 | 5 | 29.3 | 0.0733 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 2 | 12 | 2519 | 192572 | 7046 | 202149 | 0.965 | 5 | 0 | 0 | 0 | 6 | 22.0 | 0.0919 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 2 | 18 | 4433 | 309135 | 10862 | 324448 | 0.966 | 9 | 0 | 0 | 0 | 10 | 38.4 | 0.1496 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 2 | 10 | 1964 | 202200 | 4888 | 209062 | 0.976 | 5 | 4 | 4 | 0 | 6 | 32.7 | 0.0797 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | P | 3 | 10 | 2447 | 160070 | 6589 | 169116 | 0.960 | 5 | 0 | 0 | 0 | 6 | 19.9 | 0.0829 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | Q | 3 | 12 | 3570 | 197919 | 8134 | 209635 | 0.961 | 6 | 0 | 0 | 0 | 7 | 39.1 | 0.1078 | 1.00 | 0 | 0 | - | - | success |
| 10 | assoc-retraction-2 | R | 3 | 22 | 3051 | 462154 | 6282 | 471509 | 0.987 | 11 | 3 | 3 | 0 | 12 | 42.6 | 0.1481 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 1 | 12 | 2580 | 191353 | 11298 | 205243 | 0.944 | 6 | 0 | 0 | 0 | 7 | 23.3 | 0.1093 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 1 | 14 | 5262 | 230259 | 14339 | 249874 | 0.941 | 7 | 0 | 0 | 0 | 8 | 41.5 | 0.1561 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 1 | 12 | 1615 | 234836 | 8097 | 244560 | 0.967 | 5 | 4 | 4 | 0 | 6 | 33.4 | 0.0955 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 2 | 12 | 2736 | 195407 | 7509 | 205664 | 0.963 | 6 | 0 | 0 | 0 | 7 | 23.0 | 0.0965 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 2 | 14 | 4716 | 233295 | 9203 | 247228 | 0.962 | 7 | 0 | 0 | 0 | 8 | 40.3 | 0.1307 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 2 | 12 | 2356 | 272192 | 19214 | 293774 | 0.934 | 6 | 5 | 5 | 0 | 7 | 40.7 | 0.1549 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | P | 3 | 12 | 2926 | 192943 | 6567 | 202448 | 0.967 | 6 | 0 | 0 | 0 | 7 | 40.5 | 0.0941 | 1.00 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | Q | 3 | 10 | 3290 | 160515 | 8027 | 171842 | 0.952 | 5 | 0 | 0 | 0 | 6 | 26.8 | 0.0971 | 0.93 | 0 | 0 | - | - | success |
| 11 | assoc-retraction-3 | R | 3 | 8 | 1434 | 158639 | 3704 | 163785 | 0.977 | 4 | 3 | 3 | 0 | 5 | 26.7 | 0.0609 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 1 | 10 | 2248 | 155462 | 10836 | 168556 | 0.935 | 5 | 0 | 0 | 0 | 6 | 20.5 | 0.0969 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 1 | 10 | 3281 | 156750 | 11990 | 172031 | 0.929 | 5 | 0 | 0 | 0 | 6 | 27.2 | 0.1121 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 1 | 8 | 1276 | 155027 | 7664 | 163975 | 0.953 | 4 | 3 | 3 | 0 | 5 | 28.6 | 0.0744 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 2 | 12 | 3078 | 194003 | 7521 | 204614 | 0.963 | 6 | 0 | 0 | 0 | 7 | 25.2 | 0.0997 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 2 | 14 | 3965 | 231606 | 8543 | 244128 | 0.964 | 7 | 0 | 0 | 0 | 8 | 34.5 | 0.1202 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 2 | 14 | 2503 | 315395 | 20449 | 338361 | 0.939 | 7 | 6 | 6 | 0 | 8 | 43.2 | 0.1699 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | P | 3 | 12 | 2681 | 191711 | 6102 | 200506 | 0.969 | 5 | 0 | 0 | 0 | 6 | 29.6 | 0.0896 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | Q | 3 | 12 | 3246 | 197079 | 8229 | 208566 | 0.960 | 6 | 0 | 0 | 0 | 7 | 27.7 | 0.1048 | 1.00 | 0 | 0 | - | - | success |
| 12 | assoc-retraction-4 | R | 3 | 10 | 1896 | 201052 | 4395 | 207353 | 0.979 | 5 | 3 | 3 | 0 | 6 | 32.8 | 0.0768 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 1 | 12 | 1772 | 189195 | 10185 | 201164 | 0.949 | 6 | 0 | 0 | 0 | 7 | 20.9 | 0.0963 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 1 | 14 | 4458 | 233398 | 15938 | 253808 | 0.936 | 7 | 0 | 0 | 0 | 8 | 37.0 | 0.1550 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 1 | 4 | 223 | 72921 | 4450 | 77598 | 0.942 | 1 | 1 | 1 | 0 | 2 | 24.6 | 0.0346 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 2 | 12 | 1557 | 187247 | 5065 | 193881 | 0.974 | 6 | 0 | 0 | 0 | 7 | 23.7 | 0.0733 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 2 | 16 | 4288 | 268519 | 9077 | 281900 | 0.967 | 8 | 0 | 0 | 0 | 9 | 36.7 | 0.1329 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 2 | 4 | 219 | 77004 | 350 | 77577 | 0.995 | 1 | 1 | 1 | 0 | 2 | 18.4 | 0.0190 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | P | 3 | 14 | 2222 | 228138 | 6790 | 237164 | 0.971 | 6 | 0 | 0 | 0 | 7 | 25.4 | 0.0950 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | Q | 3 | 10 | 4024 | 160641 | 7950 | 172625 | 0.953 | 5 | 0 | 0 | 0 | 6 | 36.3 | 0.1042 | 1.00 | 0 | 0 | - | - | success |
| 13 | assoc-timetravel-1 | R | 3 | 4 | 207 | 77354 | 0 | 77565 | 1.000 | 1 | 1 | 1 | 0 | 2 | 21.3 | 0.0175 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 1 | 8 | 1956 | 120287 | 9470 | 131721 | 0.927 | 4 | 0 | 0 | 0 | 5 | 17.4 | 0.0815 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 1 | 18 | 3039 | 293329 | 11825 | 308211 | 0.961 | 9 | 0 | 0 | 0 | 10 | 32.0 | 0.1364 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 1 | 4 | 213 | 72921 | 4450 | 77588 | 0.942 | 1 | 1 | 1 | 0 | 2 | 21.4 | 0.0345 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 2 | 14 | 2632 | 228780 | 6598 | 238024 | 0.972 | 6 | 0 | 0 | 0 | 7 | 23.2 | 0.0985 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 2 | 16 | 4468 | 265598 | 9620 | 279702 | 0.965 | 8 | 0 | 0 | 0 | 9 | 40.6 | 0.1363 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 2 | 4 | 200 | 77004 | 340 | 77548 | 0.996 | 1 | 1 | 1 | 0 | 2 | 21.3 | 0.0188 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | P | 3 | 10 | 1613 | 158123 | 5329 | 165075 | 0.967 | 5 | 0 | 0 | 0 | 6 | 14.7 | 0.0691 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | Q | 3 | 14 | 4089 | 232217 | 9450 | 245770 | 0.961 | 7 | 0 | 0 | 0 | 8 | 34.1 | 0.1252 | 1.00 | 0 | 0 | - | - | success |
| 14 | assoc-timetravel-2 | R | 3 | 4 | 210 | 77004 | 356 | 77574 | 0.995 | 1 | 1 | 1 | 0 | 2 | 17.5 | 0.0189 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | P | 1 | 12 | 2224 | 189878 | 9930 | 202044 | 0.950 | 6 | 0 | 0 | 0 | 7 | 24.3 | 0.1000 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 1 | 10 | 2669 | 155450 | 11015 | 169144 | 0.934 | 5 | 0 | 0 | 0 | 6 | 22.8 | 0.1019 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 1 | 4 | 207 | 72921 | 4428 | 77560 | 0.943 | 1 | 1 | 1 | 0 | 2 | 20.3 | 0.0344 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | P | 2 | 10 | 1708 | 158383 | 5637 | 165738 | 0.966 | 5 | 0 | 0 | 0 | 6 | 15.5 | 0.0713 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 2 | 12 | 2845 | 193420 | 6672 | 202949 | 0.967 | 5 | 0 | 0 | 0 | 6 | 28.4 | 0.0938 | 0.86 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 2 | 4 | 204 | 77349 | 0 | 77557 | 1.000 | 1 | 1 | 1 | 0 | 2 | 21.2 | 0.0175 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | P | 3 | 8 | 1775 | 124760 | 6089 | 132632 | 0.953 | 4 | 0 | 0 | 0 | 5 | 15.5 | 0.0671 | 1.00 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | Q | 3 | 12 | 2652 | 194676 | 6961 | 204301 | 0.965 | 6 | 0 | 0 | 0 | 7 | 22.6 | 0.0933 | 0.86 | 0 | 0 | - | - | success |
| 15 | assoc-timetravel-3 | R | 3 | 4 | 201 | 77349 | 0 | 77554 | 1.000 | 1 | 1 | 1 | 0 | 2 | 17.9 | 0.0175 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | P | 1 | 8 | 1562 | 120262 | 9714 | 131546 | 0.925 | 4 | 0 | 0 | 0 | 5 | 17.8 | 0.0785 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 1 | 10 | 3259 | 157381 | 11502 | 172152 | 0.932 | 5 | 0 | 0 | 0 | 6 | 28.1 | 0.1101 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 1 | 4 | 224 | 72921 | 4484 | 77633 | 0.942 | 1 | 1 | 1 | 0 | 2 | 19.0 | 0.0348 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | P | 2 | 12 | 1635 | 191260 | 5983 | 198890 | 0.970 | 6 | 0 | 0 | 0 | 7 | 15.9 | 0.0786 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 2 | 16 | 4868 | 268971 | 9471 | 283326 | 0.966 | 8 | 0 | 0 | 0 | 9 | 59.0 | 0.1404 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 2 | 4 | 212 | 77004 | 387 | 77607 | 0.995 | 1 | 1 | 1 | 0 | 2 | 18.7 | 0.0191 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | P | 3 | 8 | 1592 | 124750 | 5898 | 132248 | 0.955 | 4 | 0 | 0 | 0 | 5 | 13.9 | 0.0645 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | Q | 3 | 16 | 4905 | 271207 | 9583 | 285711 | 0.966 | 8 | 0 | 0 | 0 | 9 | 41.6 | 0.1417 | 1.00 | 0 | 0 | - | - | success |
| 16 | assoc-timetravel-4 | R | 3 | 4 | 232 | 77004 | 407 | 77647 | 0.995 | 1 | 1 | 1 | 0 | 2 | 19.6 | 0.0194 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 1 | 14 | 2518 | 226421 | 10822 | 239775 | 0.954 | 7 | 0 | 0 | 0 | 8 | 24.6 | 0.1138 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 1 | 8 | 2534 | 120500 | 10447 | 133489 | 0.920 | 4 | 0 | 0 | 0 | 5 | 20.8 | 0.0912 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 1 | 6 | 1033 | 112310 | 5493 | 118842 | 0.953 | 2 | 2 | 2 | 0 | 3 | 24.6 | 0.0548 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 2 | 12 | 2516 | 193186 | 5924 | 201638 | 0.970 | 6 | 0 | 0 | 0 | 7 | 22.0 | 0.0875 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 2 | 14 | 5168 | 234433 | 9914 | 249529 | 0.959 | 7 | 0 | 0 | 0 | 8 | 46.0 | 0.1383 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 2 | 6 | 886 | 116307 | 1350 | 118549 | 0.989 | 2 | 2 | 2 | 0 | 3 | 26.8 | 0.0375 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | P | 3 | 14 | 3189 | 232654 | 7843 | 243700 | 0.967 | 7 | 0 | 0 | 0 | 8 | 27.8 | 0.1098 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | Q | 3 | 18 | 5273 | 312574 | 10710 | 328575 | 0.967 | 9 | 0 | 0 | 0 | 10 | 43.3 | 0.1581 | 1.00 | 0 | 0 | - | - | success |
| 17 | assoc-visibility-1 | R | 3 | 4 | 375 | 77014 | 492 | 77885 | 0.994 | 1 | 1 | 1 | 0 | 2 | 17.2 | 0.0211 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 1 | 14 | 2879 | 226209 | 10977 | 240079 | 0.954 | 7 | 0 | 0 | 0 | 8 | 27.0 | 0.1180 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 1 | 10 | 3002 | 154566 | 10674 | 168252 | 0.935 | 5 | 0 | 0 | 0 | 6 | 36.1 | 0.1036 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 1 | 6 | 794 | 112264 | 5532 | 118596 | 0.953 | 2 | 2 | 2 | 0 | 3 | 23.8 | 0.0525 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 2 | 14 | 3431 | 231386 | 7452 | 242283 | 0.969 | 7 | 0 | 0 | 0 | 8 | 31.1 | 0.1104 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 2 | 12 | 3183 | 196479 | 7512 | 207186 | 0.963 | 6 | 0 | 0 | 0 | 7 | 29.1 | 0.1012 | 0.97 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 2 | 4 | 818 | 77014 | 947 | 78783 | 0.988 | 1 | 1 | 1 | 0 | 2 | 28.5 | 0.0274 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | P | 3 | 12 | 2689 | 194309 | 7889 | 204899 | 0.961 | 6 | 0 | 0 | 0 | 7 | 28.5 | 0.0973 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | Q | 3 | 12 | 3672 | 197727 | 8391 | 209802 | 0.959 | 6 | 0 | 0 | 0 | 7 | 30.4 | 0.1099 | 1.00 | 0 | 0 | - | - | success |
| 18 | assoc-visibility-2 | R | 3 | 4 | 756 | 77014 | 885 | 78659 | 0.989 | 1 | 1 | 1 | 0 | 2 | 21.3 | 0.0265 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 1 | 12 | 2813 | 190275 | 10804 | 203904 | 0.946 | 6 | 0 | 0 | 0 | 7 | 23.5 | 0.1094 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 1 | 10 | 3725 | 155824 | 11811 | 171370 | 0.929 | 5 | 0 | 0 | 0 | 6 | 28.2 | 0.1157 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 1 | 6 | 472 | 111809 | 4809 | 117096 | 0.959 | 2 | 2 | 2 | 0 | 3 | 19.9 | 0.0463 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 2 | 10 | 1894 | 159029 | 5947 | 166880 | 0.964 | 5 | 0 | 0 | 0 | 6 | 16.8 | 0.0746 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 2 | 8 | 2188 | 124517 | 6391 | 133104 | 0.951 | 4 | 0 | 0 | 0 | 5 | 21.1 | 0.0724 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 2 | 4 | 696 | 77042 | 763 | 78505 | 0.990 | 1 | 1 | 1 | 0 | 2 | 23.2 | 0.0254 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | P | 3 | 12 | 2412 | 192926 | 6744 | 202094 | 0.966 | 6 | 0 | 0 | 0 | 7 | 37.1 | 0.0897 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | Q | 3 | 10 | 3231 | 160365 | 7454 | 171060 | 0.956 | 5 | 0 | 0 | 0 | 6 | 25.2 | 0.0942 | 1.00 | 0 | 0 | - | - | success |
| 19 | assoc-visibility-3 | R | 3 | 4 | 650 | 77042 | 717 | 78413 | 0.991 | 1 | 1 | 1 | 0 | 2 | 20.5 | 0.0248 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 1 | 16 | 3316 | 263198 | 12268 | 278798 | 0.955 | 8 | 0 | 0 | 0 | 9 | 33.1 | 0.1349 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 1 | 12 | 4569 | 193848 | 13504 | 211933 | 0.935 | 6 | 0 | 0 | 0 | 7 | 35.4 | 0.1385 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 1 | 6 | 478 | 111808 | 4822 | 117114 | 0.959 | 2 | 2 | 2 | 0 | 3 | 19.3 | 0.0464 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 2 | 12 | 2477 | 194617 | 6763 | 203869 | 0.966 | 6 | 0 | 0 | 0 | 7 | 20.9 | 0.0908 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 2 | 12 | 4822 | 197790 | 9523 | 212147 | 0.954 | 6 | 0 | 0 | 0 | 7 | 38.0 | 0.1259 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 2 | 8 | 866 | 155208 | 1203 | 157285 | 0.992 | 3 | 3 | 3 | 0 | 4 | 26.4 | 0.0445 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | P | 3 | 12 | 2284 | 193778 | 6159 | 202233 | 0.969 | 6 | 0 | 0 | 0 | 7 | 20.3 | 0.0863 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | Q | 3 | 10 | 3501 | 159740 | 7508 | 170759 | 0.955 | 5 | 0 | 0 | 0 | 6 | 34.2 | 0.0970 | 1.00 | 0 | 0 | - | - | success |
| 20 | assoc-visibility-4 | R | 3 | 8 | 856 | 155204 | 1165 | 157233 | 0.993 | 3 | 3 | 3 | 0 | 4 | 23.1 | 0.0443 | 1.00 | 0 | 0 | - | - | success |

## Aggregate (mean per arm)

| metric | arm P (files + grep) | arm Q (sqlite) | arm R (graph) | P vs Q | R vs Q |
|---|---|---|---|---|---|
| total tokens | 202552 | 211487 | 155397 | -4.2% | -26.5% |
| input tokens | 12 | 12 | 7 | -1.4% | -40.0% |
| output tokens | 2309 | 3495 | 984 | -33.9% | -71.8% |
| cache read | 192101 | 198467 | 148979 | -3.2% | -24.9% |
| cache create | 8130 | 9513 | 5427 | -14.5% | -42.9% |
| cache hit ratio | 0.956 | 0.953 | 0.964 | +0.4% | +1.2% |
| tool calls | 6.07 | 6.07 | 2.93 | +0.0% | -51.6% |
| mcp calls | 0.00 | 0.00 | 2.62 | n/a | n/a |
| graph calls | 0.00 | 0.00 | 2.62 | n/a | n/a |
| tool search calls | 0.00 | 0.00 | 0.00 | n/a | n/a |
| adoption | 0.00 | 0.00 | 1.00 | n/a | n/a |
| turns | 6.80 | 7.07 | 3.93 | -3.8% | -44.3% |
| seconds | 23.3 | 31.2 | 27.8 | -25.3% | -10.8% |
| cost $ | 0.0979 | 0.1127 | 0.0614 | -13.2% | -45.6% |
| score | 0.983 | 0.990 | 1.000 | -0.7% | +1.0% |
| wrong extra | 0.00 | 0.00 | 0.00 | n/a | n/a |

arm P: 60 cells; arm Q: 60 cells; arm R: 60 cells; timeouts 0; errors 0; cells with no cost 0

## Deltas vs arm Q

Paired by task: one difference per task, arm mean minus arm Q mean. The interval is a 95% percentile bootstrap over those differences (2000 resamples, seeded). An interval that does not contain 0 is the only kind worth quoting. `tasks` is how many tasks contributed a difference — a task where either side recorded nothing contributes none.

| arm | metric | arm mean | arm Q mean | delta | 95% CI | tasks |
|---|---|---|---|---|---|---|
| P | score | 0.983 | 0.990 | -0.0065 | [-0.0471, 0.0203] | 20 |
| P | cost $ | 0.0979 | 0.1127 | -0.01485 | [-0.02699, -0.00008] | 20 |
| P | total tokens | 202552 | 211487 | -8935.3 | [-29788.3, 12589.2] | 20 |
| P | turns | 6.80 | 7.07 | -0.267 | [-0.867, 0.367] | 20 |
| P | cache hit ratio | 0.956 | 0.953 | 0.0037 | [-0.0022, 0.0081] | 20 |
| R | score | 1.000 | 0.990 | 0.0101 | [0.0005, 0.0229] | 20 |
| R | cost $ | 0.0614 | 0.1127 | -0.05136 | [-0.06875, -0.03147] | 20 |
| R | total tokens | 155397 | 211487 | -56089.8 | [-95482.2, -8268.4] | 20 |
| R | turns | 3.93 | 7.07 | -3.133 | [-4.017, -2.133] | 20 |
| R | cache hit ratio | 0.964 | 0.953 | 0.0112 | [-0.0054, 0.0225] | 20 |

## Correctness by task (mean score over reps)

| task | arm P | arm Q | arm R | best |
|---|---|---|---|---|
| 1 | 1.00 | 1.00 | 1.00 | tie |
| 2 | 0.67 | 1.00 | 1.00 | Q |
| 3 | 1.00 | 1.00 | 1.00 | tie |
| 4 | 1.00 | 1.00 | 1.00 | tie |
| 5 | 1.00 | 1.00 | 1.00 | tie |
| 6 | 1.00 | 1.00 | 1.00 | tie |
| 7 | 1.00 | 1.00 | 1.00 | tie |
| 8 | 1.00 | 0.93 | 1.00 | P |
| 9 | 1.00 | 1.00 | 1.00 | tie |
| 10 | 1.00 | 1.00 | 1.00 | tie |
| 11 | 1.00 | 0.98 | 1.00 | P |
| 12 | 1.00 | 1.00 | 1.00 | tie |
| 13 | 1.00 | 1.00 | 1.00 | tie |
| 14 | 1.00 | 1.00 | 1.00 | tie |
| 15 | 1.00 | 0.90 | 1.00 | P |
| 16 | 1.00 | 1.00 | 1.00 | tie |
| 17 | 1.00 | 1.00 | 1.00 | tie |
| 18 | 1.00 | 0.99 | 1.00 | P |
| 19 | 1.00 | 1.00 | 1.00 | tie |
| 20 | 1.00 | 1.00 | 1.00 | tie |
| **mean** | **0.983** | **0.990** | **1.000** | |

## Sub-1.0 cells

One line per cell that did not score 1.0. `classification` is filled in by hand after reading the cell's stream file: `tool` (the graph answered badly or not at all), `model` (the agent had what it needed and went wrong anyway), or `grader` (the answer is right and the check is wrong).

| task | key | arm | rep | score | missed | verify | outcome | classification |
|---|---|---|---|---|---|---|---|---|
| 2 | assoc-why-2 | P | 3 | 0.00 | set 0/5 | - | success |   model |
| 8 | assoc-multihop-4 | Q | 3 | 0.78 | set 7/9 | - | success |   model |
| 15 | assoc-timetravel-3 | Q | 2 | 0.86 | set 6/7 | - | success |   model |
| 15 | assoc-timetravel-3 | Q | 3 | 0.86 | set 6/7 | - | success |   model |
| 11 | assoc-retraction-3 | Q | 3 | 0.93 | set 13/14 | - | success |   model |
| 18 | assoc-visibility-2 | Q | 2 | 0.97 | set 34/35 | - | success |   model |

## Tool adoption

Sessions that made at least one MCP call, and which mushroomdb tools they used.

| arm | sessions | >=1 mcp call | adoption | mcp calls total |
|---|---|---|---|---|
| P | 60 | 0 | 0% | 0 |
| Q | 60 | 0 | 0% | 0 |
| R | 60 | 60 | 100% | 157 |

- arm P: no MCP tools used
- arm Q: no MCP tools used
- arm R: `query` x70, `what_if` x26, `node_edges` x18, `explain_association` x13, `edges_at` x12, `stats` x10, `node_info` x8

Tasks where each arm reached for the graph:

- arm P: none
- arm Q: none
- arm R: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20]

## Answers

### task 1 rep 1 arm P (score 1.00)

```
## Relationships

- INDUSTRY_ALIGNMENT
- SPECIALTY_MATCH
- LOCATION_FIT

## Shared specialties

- hospitality
- residential
```

### task 1 rep 1 arm Q (score 1.00)

```
**Relationship types:**
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
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

hospitality
residential
```

### task 1 rep 2 arm Q (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

**Common specialties:**
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
23.88 km ≤ 160.9 km, so LOCATION_FIT also holds.

Relationships:
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

Common specialties:
hospitality
residential
```

### task 1 rep 3 arm Q (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
LOCATION_FIT

**Shared specialties:**
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

**Common specialties:**
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
`talent-000512` and `company-000054` were never touched by any change in `changes.jsonl`, so the day-0 records stand as final state.

Relationship types:
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

Specialties in common:
multi-family
residential
```

### task 2 rep 2 arm Q (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Common specialties:**
residential
multi-family
```

### task 2 rep 2 arm R (score 1.00)

```
INDUSTRY_ALIGNMENT
MATCHES_DESIGN_STYLE
SPECIALTY_MATCH

multi-family
residential
```

### task 2 rep 3 arm P (score 0.00)

```
The Explore agent's findings confirm my analysis — I've already answered this. No further action needed.
```
missed: set 0/5

### task 2 rep 3 arm Q (score 1.00)

```
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Specialties in common:**
residential
multi-family
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
**Relationship types:**
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

**Common specialties:**
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
**Relationship types:**
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

**Specialties in common:**
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
**Relationship types:**
SPECIALTY_MATCH
LOCATION_FIT
MATCHES_DESIGN_STYLE

**Specialties in common:**
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
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Shared specialties:**
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
Based on the final state (day 89 = day 0, since neither entity appears in the changes history):

**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Shared specialties:**
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
**Relationship types:**
INDUSTRY_ALIGNMENT
SPECIALTY_MATCH
MATCHES_DESIGN_STYLE

**Shared specialties:**
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
Good, matches expected structure. The computation looks correct.

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

### task 5 rep 3 arm P (score 1.00)

```
Good, that confirms base counts (2000 total) and the final counts after inserts/deletes (1411 talents, 421 companies) are plausible. Results verified.

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
The 27 companies satisfying all three relations (INDUSTRY_ALIGNMENT, SPECIALTY_MATCH, LOCATION_FIT) to at least 14 qualifying talents each, after the full history is applied:

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
Good, format matches my parsing. The results look correct.

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
Based on the final state (day 89, all 300 changes applied):

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
Good, my parsing matches the format correctly. Final answer:

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
Exactly 10 companies. Here are the keys:

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
Same result confirmed. Companies linked by all three edge types (INDUSTRY_ALIGNMENT, SPECIALTY_MATCH, LOCATION_FIT) to at least 8 published talents with 15+ years experience:

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
Same result. Here are the qualifying company keys:

company-000141
company-000321
company-000193
company-000006
company-000013
company-000098
company-000112
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

### task 8 rep 3 arm Q (score 0.78)

```
company-000006
company-000013
company-000098
company-000141
company-000193
company-000321
company-000343
```
missed: set 7/9

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
The computation checks out.

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
Good, confirms the parsing was correct. Also verified no `delete_node` affects company-000376 or these talents (script already excludes deleted entities since they're removed from the dict). Final answer:

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
Confirmed. The talents that would no longer be linked by all three:

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
Confirmed: INDUSTRY_ALIGNMENT and LOCATION_FIT depend on `industry`/`location`, not `specialties`, so only the SPECIALTY_MATCH edge is affected. The talents that would no longer be linked by all three:

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
lost_total (258) matches listed (258), so this is the complete lost list — no truncation concerns.

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
Confirms correctly: size_bucket=5 talents lose SIMILAR_SIZE when company drops to bucket 3 (diff becomes 2), size_bucket=4 talents remain linked (diff 1).

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
Good, the full lost/gained lists were captured (290 and 559 respectively, matching totals). Among the 22 talents currently linked by all three edge types, these would lose SIMILAR_SIZE (and thus no longer satisfy all three):

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
All Paris-based, no changes before day 18 for these companies — matches confirmed. Results for `talent-000501` on 2026-06-19:

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
Same result — the fix didn't change the answer since none of those inserted companies matched. Final answer:

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
Based on the reconstructed state on 2026-06-19 (day 18 = day 0 + changes through that date), `talent-000501` (industry: architecture, specialties: civic, institutional) was linked by all three rules to:

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
All checks out. Companies linked to `talent-001026` by all three relationships (`INDUSTRY_ALIGNMENT`, `SPECIALTY_MATCH`, `LOCATION_FIT`) on 2026-07-12:

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
Good, confirmed valid (inserted on day 7). Final answer:

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
Good — that change is dated 2026-07-15, after the target date, so it correctly wasn't applied. Talent-000396's day-0 fields (industry=architecture, specialties, location) were still in effect on 2026-07-14. The script logic is confirmed correct for this entity (no field-name mismatch occurred since no earlier changes existed).

Companies linked by all three relationships on 2026-07-14:

company-000048
company-000120
company-000132
company-000192
company-000288
company-000294
company-000312
company-000360
```

### task 16 rep 1 arm R (score 1.00)

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
All checks confirmed (industry match, Jaccard ≥ 0.15, distance ≤ 160.9 km).

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
Confirmed day 43 = 2026-07-14. The companies linked to `talent-000396` by all three relationships on that date:

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
Good, the logic correctly picked up inserted nodes. Here are the Job keys `talent-000104` is linked to by both `INDUSTRY_ALIGNMENT` and `SPECIALTY_MATCH` (recruiter role sees only Talent/Job, so Company links are excluded):

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
`talent-000104` is linked to these Jobs by both INDUSTRY_ALIGNMENT and SPECIALTY_MATCH:

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
talent-000104 is linked to these entities by both INDUSTRY_ALIGNMENT and SPECIALTY_MATCH:

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
28 out of 89 architecture jobs pass the specialty overlap ≥0.15 threshold — plausible. Final answer:

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
talent-000450 (recruiter view — Job entities only, Company excluded by role) is linked by **both** INDUSTRY_ALIGNMENT and SPECIALTY_MATCH to:

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
Based on the final state (day 89) of the dataset, as `recruiter` (who can only see Talent and Job, not Company — so only Talent–Job pairs count):

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
`talent-000450` is linked by both `INDUSTRY_ALIGNMENT` and `SPECIALTY_MATCH` to:

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

### task 18 rep 2 arm Q (score 0.97)

```
That's fine — the numbers check out given the dataset size. Here are the keys talent-000450 is linked to by both INDUSTRY_ALIGNMENT and SPECIALTY_MATCH (recruiter can only see Talent/Job, so Company links are excluded):

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
```
missed: set 34/35

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
talent-000450 is linked to these entities by **both** INDUSTRY_ALIGNMENT and SPECIALTY_MATCH:

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
Both check out: industry `interior-design` matches, and specialties overlap (`landscape` shared, Jaccard 1/3≈0.33 and 1/2=0.5, both ≥0.15).

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
Data checks out. As `recruiter`, only Talent/Job are visible, so the answer excludes any Company links.

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
All checks confirm industry match (architecture), specialties overlap ≥0.15, and locations within 160.9km of talent-001408.

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
All confirmed matches (recruiter role sees only Talent/Job, so no Company results apply):

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
Under the `recruiter` role, `talent-001408` is linked to these by all three edge types (companies are not visible to this role):

job-000040
job-000046
job-000052
job-000124
job-000155
```

