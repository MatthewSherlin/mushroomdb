# Agent benchmark run

- run: `20260910T050239Z`
- subject HEAD: `c61ac91`
- model: sonnet, max-turns 30, cell timeout 900s
- arm A (stock): no MCP server, no project skill, no hooks; plain prompt
- arm E (mushroomdb installed + grep redirect): `mushroomdb install --intercept-grep` — arm B's install plus the PreToolUse redirect from `Grep` to `explore`; plain prompt
- arm F (mushroomdb cli delivery + grep redirect): `mushroomdb install --delivery cli --intercept-grep` — arm D's install plus that redirect; plain prompt
- R2 subject: https://github.com/fastapi/fastapi at tag 0.141.1 (`95f8322ee1dc`), cloned per arm
- allowed tools, every arm: `Read,Grep,Glob,Bash,Edit,Write` (+ `mcp__mushroomdb` for E)
- DEVIATION from the original design: `Bash` is unqualified rather than the per-command `Bash(git:*)`, `Bash(rg:*)`, ... list. That list denies every pipeline, which made the co-change and most-imported tasks unanswerable in all arms. All arms carry the same deviation.
- a cell that timed out or errored carries no cost, turns or duration; it is excluded from those means and from their paired differences, never counted as zero. Each section says how many cells it dropped.

## Gate

The pre-registered §1 gate: correctness >= stock (paired by task), cost <= stock, adoption >= 80%, and no max-turns failure on a task stock finished.

| | |
|---|---|
| verdict | **FAILED** |
| best arm | F |
| cells with no cost recorded | 0 of 60 |

Why it failed:

- F: adoption 10% < 80%

## Per cell

| task | key | arm | rep | in | out | cache read | cache create | total tok | cache hit | tools | mcp | graph | tool search | turns | sec | cost $ | score | extra | denials | verify | dirtied | outcome |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | r1-change-1 | A | 1 | 14 | 1350 | 214670 | 8191 | 224225 | 0.963 | 6 | 0 | 0 | 0 | 7 | 21.3 | 0.0904 | 1.00 | 0 | 0 | pass | yes | success |
| 1 | r1-change-1 | E | 1 | 30 | 4021 | 680912 | 35421 | 720384 | 0.951 | 14 | 1 | 1 | 1 | 15 | 71.6 | 0.3181 | 1.00 | 0 | 2 | pass | yes | success |
| 1 | r1-change-1 | F | 1 | 16 | 1719 | 276202 | 20287 | 298224 | 0.931 | 7 | 0 | 2 | 0 | 8 | 35.4 | 0.1536 | 1.00 | 0 | 1 | pass | yes | success |
| 2 | r1-change-2 | A | 1 | 60 | 9219 | 1416899 | 41425 | 1467603 | 0.972 | 30 | 0 | 0 | 0 | 31 | 141.3 | 0.5426 | 0.00 | 0 | 0 | fail(101) | yes | error_max_turns |
| 2 | r1-change-2 | E | 1 | 60 | 8285 | 1437787 | 34088 | 1480220 | 0.977 | 30 | 0 | 0 | 0 | 31 | 125.3 | 0.5069 | 0.00 | 0 | 0 | fail(101) | yes | error_max_turns |
| 2 | r1-change-2 | F | 1 | 60 | 9093 | 1397086 | 33721 | 1439960 | 0.976 | 30 | 0 | 0 | 0 | 31 | 120.9 | 0.5054 | 0.00 | 0 | 0 | fail(101) | yes | error_max_turns |
| 3 | r1-change-3 | A | 1 | 60 | 9321 | 1810684 | 47493 | 1867558 | 0.974 | 32 | 0 | 0 | 0 | 31 | 122.0 | 0.6466 | 1.00 | 2 | 0 | pass | yes | error_max_turns |
| 3 | r1-change-3 | E | 1 | 60 | 8439 | 1544986 | 36681 | 1590166 | 0.977 | 32 | 0 | 0 | 0 | 31 | 119.2 | 0.5402 | 1.00 | 2 | 1 | pass | yes | error_max_turns |
| 3 | r1-change-3 | F | 1 | 60 | 8098 | 1394563 | 35225 | 1437946 | 0.975 | 32 | 0 | 0 | 0 | 31 | 103.1 | 0.5009 | 1.00 | 2 | 1 | pass | yes | error_max_turns |
| 4 | r1-blast-1 | A | 1 | 8 | 4991 | 118564 | 11069 | 134632 | 0.915 | 27 | 0 | 0 | 0 | 28 | 37.7 | 0.1190 | 1.00 | 0 | 0 | - | - | success |
| 4 | r1-blast-1 | E | 1 | 8 | 628 | 123740 | 8938 | 133314 | 0.933 | 3 | 1 | 1 | 1 | 4 | 12.3 | 0.0668 | 1.00 | 0 | 1 | - | - | success |
| 4 | r1-blast-1 | F | 1 | 6 | 528 | 90546 | 7964 | 99044 | 0.919 | 2 | 0 | 1 | 0 | 3 | 10.5 | 0.0553 | 1.00 | 0 | 1 | - | - | success |
| 5 | r1-blast-2 | A | 1 | 8 | 5329 | 126559 | 19634 | 151530 | 0.866 | 15 | 0 | 0 | 0 | 16 | 64.2 | 0.1582 | 0.95 | 0 | 0 | - | - | success |
| 5 | r1-blast-2 | E | 1 | 12 | 3526 | 212912 | 25174 | 241624 | 0.894 | 12 | 0 | 0 | 0 | 13 | 35.7 | 0.1786 | 1.00 | 0 | 0 | - | - | success |
| 5 | r1-blast-2 | F | 1 | 6 | 1258 | 93823 | 11613 | 106700 | 0.890 | 2 | 0 | 0 | 0 | 3 | 14.8 | 0.0778 | 1.00 | 0 | 0 | - | - | success |
| 6 | r1-localize-1 | A | 1 | 44 | 16959 | 949630 | 29386 | 996019 | 0.970 | 38 | 0 | 0 | 0 | 22 | 379.0 | 0.9161 | 1.00 | 0 | 0 | - | - | success |
| 6 | r1-localize-1 | E | 1 | 8 | 954 | 124556 | 12919 | 138437 | 0.906 | 3 | 0 | 0 | 0 | 4 | 18.3 | 0.0861 | 1.00 | 0 | 0 | - | - | success |
| 6 | r1-localize-1 | F | 1 | 30 | 4727 | 760449 | 33894 | 799100 | 0.957 | 16 | 0 | 0 | 0 | 17 | 65.0 | 0.3350 | 1.00 | 0 | 0 | - | - | success |
| 7 | r1-localize-2 | A | 1 | 24 | 2304 | 413543 | 17818 | 433689 | 0.959 | 11 | 0 | 0 | 0 | 12 | 42.3 | 0.1782 | 1.00 | 0 | 0 | - | - | success |
| 7 | r1-localize-2 | E | 1 | 14 | 1599 | 230223 | 11646 | 243482 | 0.952 | 6 | 0 | 0 | 0 | 7 | 19.7 | 0.1086 | 0.67 | 0 | 0 | - | - | success |
| 7 | r1-localize-2 | F | 1 | 12 | 1380 | 195889 | 11806 | 209087 | 0.943 | 5 | 0 | 0 | 0 | 6 | 23.4 | 0.1002 | 1.00 | 0 | 0 | - | - | success |
| 8 | r1-navigate-1 | A | 1 | 22 | 2041 | 367782 | 13456 | 383301 | 0.965 | 10 | 0 | 0 | 0 | 11 | 26.9 | 0.1489 | 0.80 | 0 | 0 | - | - | success |
| 8 | r1-navigate-1 | E | 1 | 24 | 2643 | 447900 | 20412 | 470979 | 0.956 | 11 | 1 | 1 | 1 | 12 | 44.8 | 0.1977 | 0.80 | 0 | 1 | - | - | success |
| 8 | r1-navigate-1 | F | 1 | 26 | 2436 | 466567 | 23889 | 492918 | 0.951 | 12 | 0 | 0 | 0 | 13 | 47.6 | 0.2133 | 0.80 | 0 | 1 | - | - | success |
| 9 | r1-navigate-2 | A | 1 | 2 | 105 | 30684 | 2249 | 33040 | 0.932 | 12 | 0 | 0 | 0 | 1 | 59.1 | 0.1939 | 1.00 | 0 | 0 | - | - | success |
| 9 | r1-navigate-2 | E | 1 | 14 | 1466 | 233571 | 13007 | 248058 | 0.947 | 7 | 0 | 0 | 0 | 8 | 18.5 | 0.1134 | 1.00 | 0 | 0 | - | - | success |
| 9 | r1-navigate-2 | F | 1 | 8 | 752 | 124507 | 8855 | 134122 | 0.933 | 5 | 0 | 0 | 0 | 6 | 10.2 | 0.0679 | 1.00 | 0 | 0 | - | - | success |
| 10 | r1-history-1 | A | 1 | 22 | 3889 | 355601 | 10433 | 369945 | 0.971 | 10 | 0 | 0 | 0 | 11 | 43.0 | 0.1529 | 1.00 | 0 | 0 | - | - | success |
| 10 | r1-history-1 | E | 1 | 10 | 2092 | 158550 | 8736 | 169388 | 0.948 | 4 | 0 | 0 | 0 | 5 | 32.4 | 0.0876 | 1.00 | 0 | 0 | - | - | success |
| 10 | r1-history-1 | F | 1 | 14 | 3153 | 226050 | 9740 | 238957 | 0.959 | 6 | 0 | 0 | 0 | 7 | 154.0 | 0.1157 | 1.00 | 0 | 0 | - | - | success |
| 11 | r2-change-1 | A | 1 | 22 | 7881 | 416248 | 19931 | 444082 | 0.954 | 10 | 0 | 0 | 0 | 11 | 80.0 | 0.2431 | 1.00 | 0 | 0 | pass | yes | success |
| 11 | r2-change-1 | E | 1 | 18 | 7832 | 349527 | 27288 | 384665 | 0.927 | 8 | 0 | 0 | 0 | 9 | 77.4 | 0.2574 | 1.00 | 0 | 0 | pass | yes | success |
| 11 | r2-change-1 | F | 1 | 34 | 8399 | 697373 | 28866 | 734672 | 0.960 | 16 | 0 | 0 | 0 | 17 | 100.8 | 0.3390 | 1.00 | 0 | 0 | pass | yes | success |
| 12 | r2-change-2 | A | 1 | 60 | 19209 | 1556412 | 51269 | 1626950 | 0.968 | 30 | 0 | 0 | 0 | 31 | 236.5 | 0.7097 | 1.00 | 0 | 0 | pass | yes | error_max_turns |
| 12 | r2-change-2 | E | 1 | 60 | 26655 | 1908418 | 60024 | 1995157 | 0.970 | 30 | 0 | 0 | 0 | 31 | 306.9 | 0.8884 | 0.67 | 0 | 1 | pass | yes | error_max_turns |
| 12 | r2-change-2 | F | 1 | 60 | 32235 | 1945962 | 63643 | 2041900 | 0.968 | 30 | 0 | 0 | 0 | 31 | 380.0 | 0.9662 | 1.00 | 0 | 0 | pass | yes | error_max_turns |
| 13 | r2-change-3 | A | 1 | 44 | 5641 | 889486 | 24400 | 919571 | 0.973 | 21 | 0 | 0 | 0 | 22 | 64.4 | 0.3332 | 1.00 | 0 | 0 | pass | yes | success |
| 13 | r2-change-3 | E | 1 | 30 | 4445 | 616464 | 21196 | 642135 | 0.967 | 14 | 0 | 0 | 0 | 15 | 54.6 | 0.2526 | 1.00 | 0 | 0 | pass | yes | success |
| 13 | r2-change-3 | F | 1 | 32 | 7460 | 703547 | 26256 | 737295 | 0.964 | 15 | 0 | 0 | 0 | 16 | 90.6 | 0.3204 | 1.00 | 0 | 0 | pass | yes | success |
| 14 | r2-blast-1 | A | 1 | 12 | 3420 | 203664 | 16066 | 223162 | 0.927 | 12 | 0 | 0 | 0 | 13 | 30.9 | 0.1403 | 1.00 | 0 | 0 | - | - | success |
| 14 | r2-blast-1 | E | 1 | 14 | 2925 | 242953 | 20196 | 266088 | 0.923 | 17 | 1 | 1 | 1 | 18 | 32.2 | 0.1587 | 1.00 | 0 | 1 | - | - | success |
| 14 | r2-blast-1 | F | 1 | 16 | 3953 | 289290 | 20905 | 314164 | 0.933 | 17 | 0 | 0 | 0 | 19 | 43.6 | 0.1810 | 1.00 | 0 | 1 | - | - | success |
| 15 | r2-blast-2 | A | 1 | 10 | 1495 | 163592 | 15405 | 180502 | 0.914 | 6 | 0 | 0 | 0 | 7 | 36.8 | 0.1104 | 1.00 | 0 | 0 | - | - | success |
| 15 | r2-blast-2 | E | 1 | 14 | 3366 | 237077 | 17224 | 257681 | 0.932 | 15 | 1 | 1 | 1 | 16 | 43.7 | 0.1500 | 1.00 | 0 | 1 | - | - | success |
| 15 | r2-blast-2 | F | 1 | 16 | 3241 | 298365 | 22547 | 324169 | 0.930 | 16 | 0 | 0 | 0 | 17 | 44.3 | 0.1823 | 1.00 | 0 | 1 | - | - | success |
| 16 | r2-localize-1 | A | 1 | 20 | 2238 | 315997 | 9998 | 328253 | 0.969 | 9 | 0 | 0 | 0 | 10 | 38.5 | 0.1267 | 1.00 | 0 | 0 | - | - | success |
| 16 | r2-localize-1 | E | 1 | 12 | 1046 | 189542 | 8995 | 199595 | 0.955 | 5 | 0 | 0 | 0 | 6 | 15.2 | 0.0844 | 1.00 | 0 | 0 | - | - | success |
| 16 | r2-localize-1 | F | 1 | 16 | 1548 | 262290 | 10885 | 274739 | 0.960 | 7 | 0 | 0 | 0 | 8 | 26.7 | 0.1115 | 1.00 | 0 | 0 | - | - | success |
| 17 | r2-localize-2 | A | 1 | 18 | 2486 | 311536 | 16741 | 330781 | 0.949 | 9 | 0 | 0 | 0 | 10 | 30.4 | 0.1552 | 0.75 | 0 | 0 | - | - | success |
| 17 | r2-localize-2 | E | 1 | 36 | 15730 | 853885 | 39943 | 909594 | 0.955 | 17 | 0 | 0 | 0 | 18 | 188.7 | 0.4879 | 1.00 | 0 | 0 | - | - | success |
| 17 | r2-localize-2 | F | 1 | 16 | 3943 | 351300 | 31149 | 386408 | 0.918 | 8 | 0 | 0 | 0 | 9 | 56.8 | 0.2343 | 1.00 | 0 | 0 | - | - | success |
| 18 | r2-navigate-1 | A | 1 | 8 | 1725 | 117314 | 6309 | 125356 | 0.949 | 7 | 0 | 0 | 0 | 8 | 25.0 | 0.0671 | 1.00 | 0 | 0 | - | - | success |
| 18 | r2-navigate-1 | E | 1 | 14 | 1721 | 224935 | 8733 | 235403 | 0.963 | 6 | 0 | 0 | 0 | 7 | 23.4 | 0.0972 | 1.00 | 0 | 0 | - | - | success |
| 18 | r2-navigate-1 | F | 1 | 6 | 798 | 90628 | 7544 | 98976 | 0.923 | 2 | 0 | 0 | 0 | 3 | 12.3 | 0.0563 | 1.00 | 0 | 0 | - | - | success |
| 19 | r2-navigate-2 | A | 1 | 10 | 1303 | 150016 | 7229 | 158558 | 0.954 | 6 | 0 | 0 | 0 | 7 | 21.5 | 0.0730 | 0.80 | 0 | 0 | - | - | success |
| 19 | r2-navigate-2 | E | 1 | 10 | 708 | 156730 | 8294 | 165742 | 0.950 | 4 | 0 | 0 | 0 | 5 | 14.1 | 0.0716 | 0.80 | 0 | 0 | - | - | success |
| 19 | r2-navigate-2 | F | 1 | 10 | 911 | 158462 | 8869 | 168252 | 0.947 | 4 | 0 | 0 | 0 | 5 | 14.6 | 0.0763 | 0.80 | 0 | 0 | - | - | success |
| 20 | r2-history-1 | A | 1 | 60 | 12934 | 1114890 | 24072 | 1151956 | 0.979 | 30 | 0 | 0 | 2 | 31 | 555.1 | 0.4498 | 0.11 | 0 | 0 | - | - | error_max_turns |
| 20 | r2-history-1 | E | 1 | 18 | 3639 | 298449 | 11168 | 313274 | 0.964 | 8 | 0 | 0 | 0 | 9 | 41.3 | 0.1408 | 0.89 | 1 | 0 | - | - | success |
| 20 | r2-history-1 | F | 1 | 14 | 5206 | 228338 | 12093 | 245651 | 0.950 | 6 | 0 | 0 | 0 | 7 | 65.6 | 0.1461 | 0.89 | 1 | 0 | - | - | success |

## Aggregate (mean per arm)

| metric | arm A (stock) | arm E (mushroomdb installed + grep redirect) | arm F (mushroomdb cli delivery + grep redirect) | E vs A | F vs A |
|---|---|---|---|---|---|
| total tokens | 577536 | 540269 | 529114 | -6.5% | -8.4% |
| input tokens | 26 | 23 | 23 | -11.7% | -13.3% |
| output tokens | 5692 | 5086 | 5042 | -10.6% | -11.4% |
| cache read | 552189 | 513656 | 502562 | -7.0% | -9.0% |
| cache create | 19629 | 21504 | 21488 | +9.6% | +9.5% |
| cache hit ratio | 0.951 | 0.947 | 0.944 | -0.4% | -0.7% |
| tool calls | 16.55 | 12.30 | 11.90 | -25.7% | -28.1% |
| mcp calls | 0.00 | 0.25 | 0.00 | n/a | n/a |
| graph calls | 0.00 | 0.25 | 0.15 | n/a | n/a |
| tool search calls | 0.10 | 0.25 | 0.00 | +150.0% | -100.0% |
| adoption | 0.00 | 0.25 | 0.10 | n/a | n/a |
| turns | 16.00 | 13.20 | 12.85 | -17.5% | -19.7% |
| seconds | 102.8 | 64.8 | 71.0 | -37.0% | -30.9% |
| cost $ | 0.2778 | 0.2397 | 0.2369 | -13.7% | -14.7% |
| score | 0.871 | 0.891 | 0.924 | +2.4% | +6.2% |
| wrong extra | 0.10 | 0.15 | 0.15 | +50.0% | +50.0% |

arm A: 20 cells; arm E: 20 cells; arm F: 20 cells; timeouts 0; errors 10; cells with no cost 0

## Deltas vs stock

Paired by task: one difference per task, arm mean minus arm A mean. The interval is a 95% percentile bootstrap over those differences (2000 resamples, seeded). An interval that does not contain 0 is the only kind worth quoting. `tasks` is how many tasks contributed a difference — a task where either side recorded nothing contributes none.

| arm | metric | arm mean | stock mean | delta | 95% CI | tasks |
|---|---|---|---|---|---|---|
| E | score | 0.891 | 0.871 | 0.0206 | [-0.0667, 0.1247] | 20 |
| E | cost $ | 0.2397 | 0.2778 | -0.03811 | [-0.15580, 0.05352] | 20 |
| E | total tokens | 540269 | 577536 | -37266.3 | [-210890.2, 113867.9] | 20 |
| E | turns | 13.20 | 16.00 | -2.800 | [-7.150, 1.100] | 20 |
| E | cache hit ratio | 0.947 | 0.951 | -0.0038 | [-0.0134, 0.0046] | 20 |
| F | score | 0.924 | 0.871 | 0.0539 | [0.0000, 0.1442] | 20 |
| F | cost $ | 0.2369 | 0.2778 | -0.04084 | [-0.12442, 0.02743] | 20 |
| F | total tokens | 529114 | 577536 | -48421.4 | [-178933.5, 60215.4] | 20 |
| F | turns | 12.85 | 16.00 | -3.150 | [-7.350, 0.450] | 20 |
| F | cache hit ratio | 0.944 | 0.951 | -0.0067 | [-0.0132, -0.0001] | 20 |

## Correctness by task (mean score over reps)

| task | arm A | arm E | arm F | best |
|---|---|---|---|---|
| 1 | 1.00 | 1.00 | 1.00 | tie |
| 2 | 0.00 | 0.00 | 0.00 | tie |
| 3 | 1.00 | 1.00 | 1.00 | tie |
| 4 | 1.00 | 1.00 | 1.00 | tie |
| 5 | 0.95 | 1.00 | 1.00 | E |
| 6 | 1.00 | 1.00 | 1.00 | tie |
| 7 | 1.00 | 0.67 | 1.00 | A |
| 8 | 0.80 | 0.80 | 0.80 | tie |
| 9 | 1.00 | 1.00 | 1.00 | tie |
| 10 | 1.00 | 1.00 | 1.00 | tie |
| 11 | 1.00 | 1.00 | 1.00 | tie |
| 12 | 1.00 | 0.67 | 1.00 | A |
| 13 | 1.00 | 1.00 | 1.00 | tie |
| 14 | 1.00 | 1.00 | 1.00 | tie |
| 15 | 1.00 | 1.00 | 1.00 | tie |
| 16 | 1.00 | 1.00 | 1.00 | tie |
| 17 | 0.75 | 1.00 | 1.00 | E |
| 18 | 1.00 | 1.00 | 1.00 | tie |
| 19 | 0.80 | 0.80 | 0.80 | tie |
| 20 | 0.11 | 0.89 | 0.89 | E |
| **mean** | **0.871** | **0.891** | **0.924** | |

## Sub-1.0 cells

One line per cell that did not score 1.0. `classification` is filled in by hand after reading the cell's stream file: `tool` (the graph answered badly or not at all), `model` (the agent had what it needed and went wrong anyway), or `grader` (the answer is right and the check is wrong).

| task | key | arm | rep | score | missed | verify | outcome | classification |
|---|---|---|---|---|---|---|---|---|
| 2 | r1-change-2 | A | 1 | 0.00 | tests pass | fail(101) | error_max_turns |  |
| 2 | r1-change-2 | E | 1 | 0.00 | tests pass | fail(101) | error_max_turns |  |
| 2 | r1-change-2 | F | 1 | 0.00 | tests pass | fail(101) | error_max_turns |  |
| 20 | r2-history-1 | A | 1 | 0.11 | fastapi/openapi/utils.py, fastapi/_compat/v2.py, fastapi/applications.py, any_of>=1, author<tiangolo@gmail.com>, 23e4... | - | error_max_turns |  |
| 7 | r1-localize-2 | E | 1 | 0.67 | any_of>=1 | - | success |  |
| 12 | r2-change-2 | E | 1 | 0.67 | tests/test_sse.py | pass | error_max_turns |  |
| 17 | r2-localize-2 | A | 1 | 0.75 | stream_item_type | - | success |  |
| 8 | r1-navigate-1 | A | 1 | 0.80 | any_of>=1 | - | success |  |
| 8 | r1-navigate-1 | E | 1 | 0.80 | any_of>=1 | - | success |  |
| 8 | r1-navigate-1 | F | 1 | 0.80 | any_of>=1 | - | success |  |
| 19 | r2-navigate-2 | A | 1 | 0.80 | fastapi/routing.py:1329 | - | success |  |
| 19 | r2-navigate-2 | E | 1 | 0.80 | fastapi/routing.py:1329 | - | success |  |
| 19 | r2-navigate-2 | F | 1 | 0.80 | fastapi/routing.py:1329 | - | success |  |
| 20 | r2-history-1 | E | 1 | 0.89 | 5d5666bec | - | success |  |
| 20 | r2-history-1 | F | 1 | 0.89 | 5d5666bec | - | success |  |
| 5 | r1-blast-2 | A | 1 | 0.95 | any_of>=1 | - | success |  |

## Tool adoption

Sessions that made at least one MCP call, and which mushroomdb tools they used.

| arm | sessions | >=1 mcp call | adoption | mcp calls total |
|---|---|---|---|---|
| A | 20 | 0 | 0% | 0 |
| E | 20 | 5 | 25% | 5 |
| F | 20 | 0 | 0% | 0 |

- arm A: no MCP tools used
- arm E: `explore` x5
- arm F: no MCP tools used

Tasks where each arm reached for the graph:

- arm A: none
- arm E: [1, 4, 8, 14, 15]
- arm F: none

## Answers

### task 1 rep 1 arm A (score 1.00)

```
All 12 tests pass. Renamed `cap_lines` to `cap_line_count` throughout `render.rs` (definition, all 6 call sites, the doc-comment link, and the unit test), and confirmed no other files in the Rust workspace reference it. The docs/llms-full.txt mentions are outside the crate code and weren't touched. Changes are left uncommitted in the working tree.
```

### task 1 rep 1 arm E (score 1.00)

```
Renamed `cap_lines` → `cap_line_count` across its definition, all 7 call sites, the doc-comment link, and the unit test (name + body) in `crates/core-api/src/repograph/render.rs` — it was the only file referencing the symbol. All 12 tests in `repograph::render` pass.
```

### task 1 rep 1 arm F (score 1.00)

```
All tests pass. Renamed `cap_lines` → `cap_line_count` throughout `crates/core-api/src/repograph/render.rs` — the definition, all 8 call sites, the doc-comment link, and the unit test (both function name and its assertions). No other files referenced it. Edits are left uncommitted in the working tree.
```

### task 2 rep 1 arm A (score 0.00)

```
Now update the existing tests referencing `Command::Owners` and the usage line assertion.
```
missed: tests pass

### task 2 rep 1 arm E (score 0.00)

```
Good, all covered. Now let's view the `parse_args_table` test function itself, to add owners cases in same style as map's `--json` spellings.
```
missed: tests pass

### task 2 rep 1 arm F (score 0.00)

```
Now update main.rs dispatch.
```
missed: tests pass

### task 3 rep 1 arm A (score 1.00)

```
The tree-sitter error was a transient race in the shared target directory (from concurrent builds by other sessions) — it didn't recur here. The specified test command passes.

Let's confirm the broader workspace still compiles cleanly to make sure no other caller was missed.
```

### task 3 rep 1 arm E (score 1.00)

```
All caught. Now let's build and run the specified test.
```

### task 3 rep 1 arm F (score 1.00)

```
This failure is unrelated to my change — a shared build-cache race with `tree-sitter`. Let's retry.
```

### task 4 rep 1 arm A (score 1.00)

```
Files under `crates/*/src/` that call `repograph::render::sanitize` (the one in `render.rs`) and would need attention if its signature changed:

- crates/core-api/src/repograph/why.rs
- crates/core-api/src/repograph/recall.rs
- crates/core-api/src/repograph/owners.rs
- crates/core-api/src/repograph/mod.rs (re-exports it)
- crates/core-api/src/repograph/map.rs
- crates/core-api/src/repograph/impact.rs
- crates/core-api/src/repograph/context.rs
- crates/server/src/mcp_tasks.rs
- crates/cli/src/recall.rs

Not affected: `crates/code-extract/src/lang/rust.rs` and `crates/code-extract/src/lang/mod.rs` only mention "sanitize" in doc comments (unrelated to this function), and `crates/cli/src/export.rs` defines/uses its own local `sanitize_id_component`, not this one.
```

### task 4 rep 1 arm E (score 1.00)

```
Files under `crates/*/src/` that call `sanitize` (crates/core-api/src/repograph/render.rs#sanitize) and would need attention if its signature changed:

- crates/core-api/src/repograph/render.rs
- crates/core-api/src/repograph/context.rs
- crates/core-api/src/repograph/why.rs
- crates/core-api/src/repograph/map.rs
- crates/core-api/src/repograph/impact.rs
- crates/core-api/src/repograph/owners.rs
- crates/core-api/src/repograph/recall.rs
- crates/server/src/mcp_tasks.rs
- crates/cli/src/recall.rs
```

### task 4 rep 1 arm F (score 1.00)

```
crates/core-api/src/repograph/render.rs
crates/core-api/src/repograph/context.rs
crates/core-api/src/repograph/why.rs
crates/core-api/src/repograph/map.rs
crates/core-api/src/repograph/impact.rs
crates/core-api/src/repograph/owners.rs
crates/core-api/src/repograph/recall.rs
crates/cli/src/recall.rs
crates/server/src/mcp_tasks.rs
```

### task 5 rep 1 arm A (score 0.95)

```
`pub fn node_ref` — `crates/core-api/src/db.rs:6882`. Callers under `crates/*/src/` (excluding db.rs itself):

- `crates/core-api/src/repograph/map.rs` — `repo_map`
- `crates/core-api/src/repograph/facts.rs` — `str_prop` (also `int_prop`, `list_prop`, `label_of`)
- `crates/core-api/src/repograph/recall.rs` — `recall_digest`
- `crates/core-api/src/ingest.rs` — `infer_auto_fk`
- `crates/cli/src/structure.rs` — `plan_file` (also `diff_prop`)
- `crates/cli/src/ingest_git.rs` — `ingest_unit` (also `legacy_name_counts`, `run_ingest_git`, `write_marker`, `marker_of`)
- `crates/cli/src/recall.rs` — `run_recall`
- `crates/server/src/mcp_tasks.rs` — `project_repo`
- `crates/server/src/mcp.rs` — `tool_neighborhood`
- `crates/server/src/http.rs` — `neighborhood`
```
missed: any_of>=1

### task 5 rep 1 arm E (score 1.00)

```
So: mcp.rs line 444 → `tool_neighborhood` (398-457); http.rs line 1451 → `neighborhood` (1369-1470); repograph/recall.rs line 367 → `recall_digest` (291-488); repograph/map.rs line 244 → `repo_map` (193-...).

Callers of `pub fn node_ref` (`crates/core-api/src/db.rs`), one function per file under `crates/*/src/`:

- `crates/core-api/src/repograph/map.rs` — `repo_map`
- `crates/core-api/src/repograph/facts.rs` — `str_prop`
- `crates/core-api/src/repograph/recall.rs` — `recall_digest`
- `crates/core-api/src/ingest.rs` — `infer_auto_fk`
- `crates/server/src/mcp.rs` — `tool_neighborhood`
- `crates/server/src/mcp_tasks.rs` — `project_repo`
- `crates/server/src/http.rs` — `neighborhood`
- `crates/cli/src/recall.rs` — `nudge_root`
- `crates/cli/src/structure.rs` — `plan_file`
- `crates/cli/src/ingest_git.rs` — `write_marker`
```

### task 5 rep 1 arm F (score 1.00)

```
crates/core-api/src/repograph/map.rs — repo_map
crates/core-api/src/repograph/recall.rs — recall_digest
crates/core-api/src/repograph/facts.rs — str_prop (also label_of)
crates/core-api/src/ingest.rs — infer_auto_fk
crates/cli/src/structure.rs — plan_file (also diff_prop)
crates/cli/src/ingest_git.rs — legacy_name_counts (also run_ingest_git, write_marker, ingest_unit)
crates/cli/src/recall.rs — nudge_root (also stale_concepts_describing)
crates/server/src/mcp_tasks.rs — project_repo
crates/server/src/mcp.rs — tool_neighborhood
crates/server/src/http.rs — neighborhood
```

### task 6 rep 1 arm A (score 1.00)

```
**Where the actual defect is:** the server side is correct — `crates/server/src/http.rs` maps `GraphError::Busy` to HTTP 503 + `Retry-After: 1` specifically so standard retry middleware treats it as retryable (see the comment at line 674). The failure happens one layer up, in the TypeScript SDK, which throws away that signal before any client-side retry logic ever sees it.

- File: `clients/typescript/src/client.ts`
- Function that builds the reply the retry layer actually sees: `fetchJson` (line 134) — constructs `new MushroomError(detail)` from just the JSON `error` string
- Function that routes the error into it: also `fetchJson` — the `if (!resp.ok)` check inside it is what catches the response and hands it to the `throw` (there's no separate router; unlike the server's split `graph_err`→`busy_response`, this SDK does both jobs in one function)

Root cause: `fetchJson` discards `resp.status` (503) and the `Retry-After` header, leaving `MushroomError` (`clients/typescript/src/types.ts:456`) with only a `detail: string`. A retry layer built on top of this client has no status code or retryable flag to inspect, so it can't distinguish this transient lock-busy condition from a permanent 4xx and gives up.
```

### task 6 rep 1 arm E (score 1.00)

```
- **File:** `crates/server/src/http.rs`
- **Builds the reply:** `busy_response` (line 676) — returns `503 Service Unavailable` with a `Retry-After: 1` header for `GraphError::Busy`.
- **Routes the error into it:** `graph_err` (line 685) — matches `GraphError::Busy { .. }` and calls `busy_response(busy.to_string())`.
```

### task 6 rep 1 arm F (score 1.00)

```
- **File:** `crates/server/src/http.rs`
- **Builds the reply:** `busy_response` (crates/server/src/http.rs:676) — constructs the 503 + `Retry-After: 1` response for `GraphError::Busy`.
- **Routes the error into it:** `graph_err` (crates/server/src/http.rs:685) — matches `GraphError::Busy { .. }` and dispatches to `busy_response`.
```

### task 7 rep 1 arm A (score 1.00)

```
File: `crates/core-rules/src/def.rs`

Function that decides safety: `predicate_contains_keymatch` (def.rs:226) — returns true when a `KeyMatch` (FK match) appears anywhere in the predicate tree, including under `Any`/OR, meaning the candidate index cannot safely narrow.

Function that consults it: `compute_desired_via` in `crates/core-rules/src/engine.rs` (engine.rs:712) — `let indexed = index.filter(|_| !predicate_contains_keymatch(&def.predicate));`
```

### task 7 rep 1 arm E (score 0.67)

```
This confirms it: the fast-path check (`is_keymatch_rooted`) says `Any` (OR) is never KeyMatch-rooted, so an OR predicate with a `KeyMatch` branch loses index narrowing safety — and `predicate_contains_keymatch` is the actual gate that decides whether to fall back to the full candidate set.

- File: `crates/core-rules/src/def.rs`
- Deciding function: `is_keymatch_rooted` (def.rs:207) — determines whether the FK/index fast path applies (returns `false` for `Any`)
- Consulting function: `predicate_contains_keymatch` (def.rs:226), used at `crates/core-rules/src/engine.rs:712` to decide whether to skip the index and fall back to the full candidate set
```
missed: any_of>=1

### task 7 rep 1 arm F (score 1.00)

```
The decision function is `Predicate::Any` (OR) matched recursively — `parts.iter().any(predicate_contains_keymatch)` means any `KeyMatch` in an `Any` branch marks the whole predicate unsafe to narrow.

- File: `crates/core-rules/src/def.rs`
- Deciding function: `predicate_contains_keymatch` (def.rs:226)
- Consuming function: `compute_desired_via` (`crates/core-rules/src/engine.rs:606`), which calls it at line 712 to gate whether the `RuleIndex` is used (`indexed = index.filter(|_| !predicate_contains_keymatch(&def.predicate))`), falling back to the full unnarrowed candidate scan when true.
```

### task 8 rep 1 arm A (score 0.80)

```
- Bound declared: `crates/cli/src/lib.rs:70` — `pub const AUTO_SNAPSHOT_RETENTION: u32 = 8;`
- Applied to a store by: `snapshot_automatically` (`crates/cli/src/lib.rs:81`), via `db.set_wal_archive_retention(Some(AUTO_SNAPSHOT_RETENTION))`
- Caller: `crates/cli/src/ingest_git.rs:1110` (also `crates/cli/src/lib.rs:106`)
```
missed: any_of>=1

### task 8 rep 1 arm E (score 0.80)

```
Caller: `crates/cli/src/ingest_git.rs:1110`.

- Bound: `crates/cli/src/lib.rs:70` — `pub const AUTO_SNAPSHOT_RETENTION: u32 = 8;`
- Applying function: `crates/cli/src/lib.rs:81` — `snapshot_automatically(db: &mut WriteGuard<'_>)`, which calls `db.set_wal_archive_retention(Some(AUTO_SNAPSHOT_RETENTION))`
- Caller of `snapshot_automatically`: `crates/cli/src/ingest_git.rs:1110`
```
missed: any_of>=1

### task 8 rep 1 arm F (score 0.80)

```
- **Bound declared:** `crates/cli/src/lib.rs:70` — `pub const AUTO_SNAPSHOT_RETENTION: u32 = 8;`
- **Applying function:** `set_wal_archive_retention` (`crates/core-api/src/db.rs:8694`), which sets `wal_archive_retention` on the `GraphDb`; the actual pruning happens in `snapshot_with` (`crates/core-api/src/db.rs:9527`, retention check at line 9826).
- **Caller:** `snapshot_automatically` in `crates/cli/src/lib.rs:81` (calls `db.set_wal_archive_retention(Some(AUTO_SNAPSHOT_RETENTION))` at line 82).
```
missed: any_of>=1

### task 9 rep 1 arm A (score 1.00)

```
- `tools_list(all: bool)` — `crates/server/src/mcp.rs:913`
- Flag: `--all-tools`
- Parsed at `crates/cli/src/lib.rs:1834` (`let all_tools = args.contains(&"--all-tools");`, inside `parse_mcp`)
```

### task 9 rep 1 arm E (score 1.00)

```
- Function that builds the listing and decides how much to show: `tools_list(all: bool)` at `crates/server/src/mcp.rs:913`.
- Command-line switch for the full listing: `--all-tools`.
- Parsed in `crates/cli/src/lib.rs:1834` (inside `parse_mcp`, `args.contains(&"--all-tools")`).
```

### task 9 rep 1 arm F (score 1.00)

```
- Listing function: `tools_list` — `crates/server/src/mcp.rs:913`
- Full listing switch: `--all-tools`
- Parsed at: `crates/cli/src/lib.rs:1834`
```

### task 10 rep 1 arm A (score 1.00)

```
**Top 5 co-changed files with `crates/cli/src/install.rs` (last 300 commits):**
1. `crates/cli/tests/install.rs` — 18
2. `docs/site/skill.md` — 14
3. `crates/cli/src/lib.rs` — 9
4. `docs/site/quickstart.md` — 7
5. `README.md` — 6

**Most recent joint commit, top three:**
- `crates/cli/tests/install.rs`: `145d580`
- `docs/site/skill.md`: `52c02ba`
- `crates/cli/src/lib.rs`: `ca3d484`

**Closest file (`crates/cli/tests/install.rs`) — top author by commit count:** Matthew Sherlin (23/23 commits touching that file).
```

### task 10 rep 1 arm E (score 1.00)

```
Top 5 co-changed files with `crates/cli/src/install.rs` (last 300 commits):

1. `crates/cli/tests/install.rs` — 18 commits
2. `docs/site/skill.md` — 14 commits
3. `crates/cli/src/lib.rs` — 9 commits
4. `docs/site/quickstart.md` — 7 commits
5. `docs/site/ingest-git.md` — 6 commits

Most recent joint commit for the top three:
- `crates/cli/tests/install.rs` → `145d580`
- `docs/site/skill.md` → `52c02ba`
- `crates/cli/src/lib.rs` → `ca3d484`

Closest overall: `crates/cli/tests/install.rs`. Top author (by commits touching it, all history): Matthew Sherlin — 23 commits (sole author).
```

### task 10 rep 1 arm F (score 1.00)

```
**Top 5 co-changed files with `crates/cli/src/install.rs`** (last 300 commits):
1. `crates/cli/tests/install.rs` — 18
2. `docs/site/skill.md` — 14
3. `crates/cli/src/lib.rs` — 9
4. `docs/site/quickstart.md` — 7
5. tie at 6: `docs/site/ingest-git.md`, `README.md`, `crates/cli/src/main.rs`

**Most recent joint commit, for the top 3:**
- `crates/cli/tests/install.rs` — `145d580a`
- `docs/site/skill.md` — `52c02ba7`
- `crates/cli/src/lib.rs` — `ca3d4848`

**Closest overall (`crates/cli/tests/install.rs`) — top author by commit count touching that file:** Matthew Sherlin (23 commits).
```

### task 11 rep 1 arm A (score 1.00)

```
All 11 tests pass. Summary of changes:

- `fastapi/dependencies/models.py`: split each `lru_cache`-decorated classifier into an uncached implementation plus a module-level cached wrapper variable, and added `set_callable_classification_cache_size(size)` which rebuilds all three caches (fresh, empty `lru_cache` instances) and reassigns the globals — the existing `_is_gen_callable`/`_is_async_gen_callable`/`_is_coroutine_callable` wrappers look up those globals dynamically, so they automatically use the new caches.
- `tests/test_dependency_models.py`: added `test_set_callable_classification_cache_size`, which sets the size to 1, classifies two different callables, and asserts `cache_info()` shows `maxsize == 1` and `currsize == 1`, resetting to 4096 afterward so other tests' assumptions still hold.
```

### task 11 rep 1 arm E (score 1.00)

```
All 11 tests pass. Summary of changes:

- `fastapi/dependencies/models.py:` split each of the three `@lru_cache`-wrapped classifiers into an uncached function plus a module-level cached wrapper, and added `set_callable_classification_cache_size(size)` which rebuilds all three `lru_cache` wrappers (fresh, empty) at the given size and updates `_CALLABLE_CLASSIFICATION_CACHE_SIZE`. Since `_is_gen_callable`/`_is_async_gen_callable`/`_is_coroutine_callable` look up the `*_cached` names as globals at call time, they automatically pick up the rebuilt caches.
- `tests/test_dependency_models.py`: added `test_set_callable_classification_cache_size`, which sets the cache size to 1, classifies two distinct callables, and asserts `cache_info().currsize == 1` (restoring the default of 4096 afterward so it doesn't affect other tests).
```

### task 11 rep 1 arm F (score 1.00)

```
Changes are in place and tests pass:

- `fastapi/dependencies/models.py`: split each `_is_*_callable_cached` into an uncached implementation plus a module-level `lru_cache`-wrapped binding, and added `set_callable_classification_cache_size(size)` which rebuilds all three caches at the new size (fresh/empty, since `lru_cache` can't be resized in place). The existing wrapper functions (`_is_gen_callable`, `_is_async_gen_callable`, `_is_coroutine_callable`) resolve the cached function via module globals at call time, so they automatically pick up the rebuilt caches.
- `tests/test_dependency_models.py`: added `test_set_callable_classification_cache_size`, which sets the size to 1, classifies two distinct callables, and asserts `cache_info()` shows `maxsize == 1` and `currsize == 1`, restoring the default (4096) in a `finally` block.

`python -m pytest tests/test_dependency_models.py -q -x` → 11 passed.
```

### task 12 rep 1 arm A (score 1.00)

```
Now let's run the test suite.
```

### task 12 rep 1 arm E (score 0.67)

```
Now let's rerun the full test_sse.py suite to confirm the openapi introspection issue is fixed.
```
missed: tests/test_sse.py

### task 12 rep 1 arm F (score 1.00)

```
All passing. Let's also run a quick full-suite smoke check to be safe, given the routing.py refactor touches a central file.
```

### task 13 rep 1 arm A (score 1.00)

```
All 6 tests pass. Summary of changes:

- `fastapi/openapi/docs.py`: added `redoc_ui_parameters: dict[str, Any] | None = None` to `get_redoc_html`, rendering each key/value as an HTML attribute on the `<redoc>` element (via a new `_redoc_ui_parameters_html` helper, HTML-escaped).
- `fastapi/applications.py`: added a matching `redoc_ui_parameters` constructor argument, stored as `self.redoc_ui_parameters`, and passed through in the built-in `/redoc` route handler.
- `tests/test_local_docs.py`: added `test_redoc_ui_parameters_in_generated_redoc` verifying a passed parameter reaches the rendered HTML.

Working tree is left with these edits uncommitted, as requested.
```

### task 13 rep 1 arm E (score 1.00)

```
All 6 tests pass. Summary of changes:

- `fastapi/openapi/docs.py`: `get_redoc_html` now accepts `redoc_ui_parameters: dict[str, Any] | None = None`, whose keys/values are written as attributes on the rendered `<redoc>` element.
- `fastapi/applications.py`: `FastAPI.__init__` gains a matching `redoc_ui_parameters` argument, stored as `self.redoc_ui_parameters` and passed through by the built-in `/redoc` route.
- `tests/test_local_docs.py`: added `test_redoc_ui_parameters_in_generated_redoc`, verifying a passed parameter reaches the rendered HTML.
```

### task 13 rep 1 arm F (score 1.00)

```
Works as expected. Summary of changes:

- `fastapi/openapi/docs.py`: added `_html_safe_attr` helper, a `redoc_ui_parameters: dict[str, Any] | None = None` argument on `get_redoc_html`, and code that writes each key/value as an HTML attribute on the `<redoc>` element.
- `fastapi/applications.py`: added a `redoc_ui_parameters` constructor argument (stored as `self.redoc_ui_parameters`), and passed it through in the `/redoc` route's call to `get_redoc_html`.
- `tests/test_local_docs.py`: added `test_redoc_ui_parameters_in_generated_redoc` verifying a passed parameter reaches the rendered HTML.

`python -m pytest tests/test_local_docs.py -q -x` passes (6 tests). Changes are left uncommitted in the working tree.
```

### task 14 rep 1 arm A (score 1.00)

```
`lenient_issubclass` is defined in `fastapi/_compat/shared.py`. Other files that import/call it:

fastapi/_compat/__init__.py — re-exports it (`from .shared import lenient_issubclass as lenient_issubclass`); no functions call it directly.
fastapi/_compat/v2.py — get_model_fields, get_flat_models_from_annotation (also get_flat_models_from_field)
fastapi/routing.py — get_request_handler, _populate_api_route_state
fastapi/openapi/utils.py — _get_openapi_operation_parameters, get_openapi_path
fastapi/dependencies/utils.py — analyze_param, add_non_field_param_to_dependency (also _get_flat_fields_from_params, request_params_to_args, is_union_of_base_models, _should_embed_body_fields, request_body_to_args)
```

### task 14 rep 1 arm E (score 1.00)

```
Based on the mushroomdb caller list for `fastapi/_compat/shared.py#lenient_issubclass`:

fastapi/dependencies/utils.py — `analyze_param`, `add_non_field_param_to_dependency`
fastapi/_compat/shared.py — `_annotation_is_complex`, `is_uploadfile_or_nonable_uploadfile_annotation`
fastapi/_compat/v2.py — `get_flat_models_from_annotation`, `get_model_fields`
fastapi/routing.py — `_populate_api_route_state`, `get_request_handler`
fastapi/openapi/utils.py — `get_openapi_path`, `_get_openapi_operation_parameters`
```

### task 14 rep 1 arm F (score 1.00)

```
Based on the mushroomdb call-site data for `lenient_issubclass` (fastapi/_compat/shared.py:47), here are the other files under `fastapi/` with call sites and the functions containing them:

fastapi/dependencies/utils.py — `add_non_field_param_to_dependency`, `_get_flat_fields_from_params` (also `analyze_param`)
fastapi/_compat/v2.py — `get_model_fields`, `get_flat_models_from_annotation` (also `get_flat_models_from_field`)
fastapi/routing.py — `get_request_handler`, `_populate_api_route_state`
fastapi/openapi/utils.py — `_get_openapi_operation_parameters`, `get_openapi_path`
```

### task 15 rep 1 arm A (score 1.00)

```
Files under `fastapi/` (excluding `encoders.py` itself, where it's self-recursive) that call `jsonable_encoder` and would need review:

fastapi/routing.py — `serialize_response`
fastapi/routing.py — `run_endpoint_function`
fastapi/exception_handlers.py — `request_validation_exception_handler`
fastapi/exception_handlers.py — `websocket_request_validation_exception_handler`
fastapi/openapi/utils.py — `_get_openapi_security_definitions`
fastapi/openapi/utils.py — `_get_openapi_operation_parameters`
fastapi/openapi/utils.py — `get_openapi_operation_request_body`
fastapi/openapi/utils.py — `get_openapi`
fastapi/openapi/docs.py — `get_swagger_ui_html`
```

### task 15 rep 1 arm E (score 1.00)

```
Confirmed. Here's the answer:

fastapi/openapi/utils.py — `_get_openapi_security_definitions`, `_get_openapi_operation_parameters`, `get_openapi_operation_request_body`, `get_openapi`
fastapi/routing.py — `serialize_response`, `app` (inner handler in `get_request_handler`), `_serialize_sse_item` (nested in that same `app`)
fastapi/exception_handlers.py — `request_validation_exception_handler`, `websocket_request_validation_exception_handler`
fastapi/openapi/docs.py — `get_swagger_ui_html`
```

### task 15 rep 1 arm F (score 1.00)

```
Both lines 172 and 186 fall inside `get_swagger_ui_html` (40–197). Here's the answer:

- `fastapi/openapi/utils.py` — `_get_openapi_security_definitions`, `_get_openapi_operation_parameters`, `get_openapi_operation_request_body`, `get_openapi`
- `fastapi/routing.py` — `serialize_response`, and `_serialize_data`/`_serialize_sse_item` (nested closures inside `get_request_handler`)
- `fastapi/exception_handlers.py` — `request_validation_exception_handler`, `websocket_request_validation_exception_handler`
- `fastapi/openapi/docs.py` — `get_swagger_ui_html`
```

### task 16 rep 1 arm A (score 1.00)

```
Confirmed: `_get_multidict_value` is where `field_annotation_is_sequence` decides `getlist` vs `get`, so a nested-annotated element type causing this to return `False` explains the bug (repeated key collapses to single value, no error).

**Answers:**

- File: `fastapi/_compat/shared.py`
- Predicates: `field_annotation_is_sequence` (line 64) and `field_annotation_is_scalar_sequence` (line 113)
- Functions elsewhere in `fastapi/dependencies/utils.py` that query them while analyzing a parameter:
  - `analyze_param` (line 381) — calls `field_annotation_is_scalar_sequence` at line 543 to validate a `Query` parameter's type
  - `_get_multidict_value` (line 749) — calls `field_annotation_is_sequence` at lines 755 and 769 to decide whether to pull the value via `getlist` (sequence) or `get` (scalar)
```

### task 16 rep 1 arm E (score 1.00)

```
Line 543 is in `analyze_param` (381-547), and lines 755/769 are in `_get_multidict_value` (749-780).

**Answer:**

- File: `fastapi/_compat/shared.py`
- Predicates: `field_annotation_is_sequence` (line 64) and `field_annotation_is_scalar_sequence` (line 113)
- Callers during parameter analysis, both in `fastapi/dependencies/utils.py`:
  - `analyze_param` (line 543, via `field_annotation_is_scalar_sequence`)
  - `_get_multidict_value` (lines 755/769, via `field_annotation_is_sequence`)
```

### task 16 rep 1 arm F (score 1.00)

```
File: `fastapi/_compat/shared.py`
Predicates: `field_annotation_is_sequence` (line 64), `field_annotation_is_scalar_sequence` (line 113)
Callers in `fastapi/dependencies/utils.py`: `analyze_param` (uses `field_annotation_is_scalar_sequence`, line 543) and `_get_multidict_value` (uses `field_annotation_is_sequence`, lines 755 and 769)
```

### task 17 rep 1 arm A (score 0.75)

```
This is called by `APIRoute.__init__`. Confirmed — the flow answers the question fully.

- **File:** `fastapi/routing.py`
- **Attribute:** `stream_item_field` (a `ModelField | None` on `APIRoute`, holding the declared stream item type)
- **Function that sets it:** `_populate_api_route_state`
- **Function that calls it:** `APIRoute.__init__`

This explains the bug: `_populate_api_route_state` (and thus `stream_item_field`) only runs in `APIRoute.__init__`, so a route created directly (mounted on the app) gets it set — but when a route is copied/re-added via `include_router`, that state isn't recreated the same way, so `stream_item_field` ends up `None` and the item type silently drops out of validation/serialization.
```
missed: stream_item_type

### task 17 rep 1 arm E (score 1.00)

```
- File: `fastapi/routing.py`
- Attribute: `stream_item_type` (route.stream_item_type)
- Function that sets it: `_populate_api_route_state`
- Function that calls it: `APIRoute.__init__` (also called by `_EffectiveRouteContext.from_api_route`, the path used when a route is carried through `include_router`)
```

### task 17 rep 1 arm F (score 1.00)

```
- **File:** `fastapi/routing.py`
- **Attribute:** `stream_item_type` (on the route/`_APIRouteLike` state, e.g. `APIRoute.stream_item_type` / `_EffectiveRouteContext.stream_item_type`)
- **Setter function:** `_populate_api_route_state`
- **Caller:** `_EffectiveRouteContext.from_api_route` (the include-router path that rebuilds route state when a route is mounted through another `APIRouter`; `APIRoute.__init__` is the other caller, used for a route registered directly on the app).
```

### task 18 rep 1 arm A (score 1.00)

```
Trace (FastAPI OpenAPI generation):

- **Path entry builder**: `get_openapi_path()` — `fastapi/openapi/utils.py:311`
- **Field → JSON-Schema fragment helper**: `get_schema_from_model_field()` — imported into `utils.py` from `fastapi/_compat/__init__.py:35`, but actually **defined** in `fastapi/_compat/v2.py:254`
- **Collects all models' definitions for the whole doc**: `get_definitions()` — `fastapi/_compat/v2.py:285` (called once from `get_openapi()` at `utils.py:623`)
- **Prefix constant the fragments reference each other by**: `REF_PREFIX` (`"#/components/schemas/"`) — `fastapi/openapi/constants.py:2`
```

### task 18 rep 1 arm E (score 1.00)

```
All confirmed. Here's the trace:

- **Per-path entry builder**: `get_openapi_path()` — `fastapi/openapi/utils.py:311`
- **Field → JSON-Schema fragment helper**: `get_schema_from_model_field()`, called from `get_openapi_path` — imported via `fastapi/_compat` but actually defined in `fastapi/_compat/v2.py:254`
- **Collects every model's definitions once**: `get_definitions()`, called from `get_openapi()` at `fastapi/openapi/utils.py:623` — also defined in `fastapi/_compat/v2.py:285`
- **Ref-prefix constant**: `REF_PREFIX = "#/components/schemas/"` — `fastapi/openapi/constants.py:2`
```

### task 18 rep 1 arm F (score 1.00)

```
- **`get_openapi_path`** — `fastapi/openapi/utils.py:311` — builds one path's entry in the OpenAPI document.
- **`get_schema_from_model_field`** — called from `fastapi/openapi/utils.py` (e.g. line 460 for the response model), but actually defined in `fastapi/_compat/v2.py:254` (imported into `utils.py` via `fastapi/_compat/__init__.py`).
- **`get_definitions`** — defined in `fastapi/_compat/v2.py:285`, called once in `fastapi/openapi/utils.py:623` to collect every model's schema definitions for the whole document.
- **`REF_PREFIX`** — `fastapi/openapi/constants.py:2` (`"#/components/schemas/"`), the prefix used in `$ref` fragments so schemas can reference each other.
```

### task 19 rep 1 arm A (score 0.80)

```
This is the `include_router` sub-route copy logic (an included router's routes not explicitly setting `response_class` fall back to the parent's default).

- Function: `get_value_or_default`, defined in `fastapi/utils.py:121`
- Applied to the response class at `fastapi/routing.py:1457`
- Sentinel type: `DefaultPlaceholder` (in `fastapi/datastructures.py:153`; created via the `Default()` helper)
```
missed: fastapi/routing.py:1329

### task 19 rep 1 arm E (score 0.80)

```
Function: `get_value_or_default`, defined in `fastapi/utils.py:121`.

Applied to the response class at `fastapi/routing.py:1457-1460` (inside the merge logic that resolves an included router's route's `response_class`).

Sentinel type: `DefaultPlaceholder` (from `fastapi/datastructures.py`, created via the `Default()` helper).
```
missed: fastapi/routing.py:1329

### task 19 rep 1 arm F (score 0.80)

```
- Function: `get_value_or_default` in `fastapi/utils.py:121`
- Applied at: `fastapi/routing.py:1457-1461` (inside `include_router`, resolving `response_class=get_value_or_default(route.response_class, include_context.included_router.default_response_class, include_context.default_response_class)`)
- Sentinel type: `DefaultPlaceholder`
```
missed: fastapi/routing.py:1329

### task 20 rep 1 arm A (score 0.11)

```
All three confirmed. Now let's get the top author for `fastapi/dependencies/utils.py` (the closest / most frequent co-change file).
```
missed: fastapi/openapi/utils.py, fastapi/_compat/v2.py, fastapi/applications.py, any_of>=1, author<tiangolo@gmail.com>, 23e4a1043, 027082950, 5d5666bec

### task 20 rep 1 arm E (score 0.89)

```
**Top 5 files co-changed with `fastapi/routing.py`** (last 2000 commits; 49 commits touched routing.py):
1. `fastapi/dependencies/utils.py` — 16 commits
2. `fastapi/openapi/utils.py` — 15 commits
3. `fastapi/applications.py` — 11 commits
4. `fastapi/_compat/v2.py` — 11 commits
5. `pyproject.toml` — 9 commits

**Most recent joint commit (routing.py + file), for the top 3:**
- `fastapi/dependencies/utils.py` — `23e4a1043` (2026-07-27)
- `fastapi/openapi/utils.py` — `027082950` (2026-07-24)
- `fastapi/applications.py` — `5429fed84` (2026-07-29)

**Closest of all: `fastapi/applications.py`** (2026-07-29, `5429fed84`)
Top author by commit count on that file: **Sebastián Ramírez — 54 commits**
```
missed: 5d5666bec

### task 20 rep 1 arm F (score 0.89)

```
Top 5 files co-changed with `fastapi/routing.py` (last 2000 commits):

1. fastapi/dependencies/utils.py — 16
2. fastapi/openapi/utils.py — 15
3. fastapi/applications.py — 11
4. fastapi/_compat/v2.py — 11
5. pyproject.toml — 9 (tied with fastapi/encoders.py at 9; pyproject.toml's most recent shared commit is more recent)

Most recent commit touching `fastapi/routing.py` together with each of the top 3:
- fastapi/dependencies/utils.py: 23e4a10
- fastapi/openapi/utils.py: 0270829
- fastapi/applications.py: 5429fed

Closest (most recent) of the three: fastapi/applications.py (5429fed, 2026-07-29).
Top author of fastapi/applications.py by commit count: Sebastián Ramírez (54 commits).
```
missed: 5d5666bec

