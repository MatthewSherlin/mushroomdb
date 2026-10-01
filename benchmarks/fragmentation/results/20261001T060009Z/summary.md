# Fragmentation probe (spec §8.2)

World: association, seed 20260910, scale 2000, day 0 — 2000 nodes, 1264424 truth edges over 11 rules (2 conjunctive, added by this probe).

## Decision

Pre-registered in `benchmarks/fragmentation/PREREGISTRATION.md`: the primary score is recall of the edges the canonical alias holds, over every rule, at 25% of Talent split into 3 aliases. A loss of at most 0.05 is SMALL — link-only stays; above it is LARGE — the canonical node is 0.8's headline.

| reference cell | canonical recall | loss | verdict |
|---|---|---|---|
| 0.25 × 3 | 0.8324 | 0.1676 | **LARGE** |

## The curve

Recall / precision. `canonical` counts only edges the canonical alias holds; `any-alias` counts an edge when any alias of the source holds it — what a reader following `SAME_AS` would see. `leaf` is the world's nine single-predicate rules, `all` the two conjunctions.

| fraction | aliases | canonical every | canonical leaf | canonical all | any-alias every | any-alias leaf | any-alias all |
|---|---|---|---|---|---|---|---|
| 0.10 | 2 | 0.9525 / 0.9976 | 0.9537 / 0.9976 | 0.9257 / 0.9983 | 0.9974 / 0.9954 | 0.9998 / 0.9954 | 0.9462 / 0.9966 |
| 0.10 | 3 | 0.9318 / 0.9976 | 0.9327 / 0.9975 | 0.9107 / 0.9989 | 0.9968 / 0.9935 | 0.9998 / 0.9933 | 0.9301 / 0.9974 |
| 0.10 | 5 | 0.9282 / 0.9989 | 0.9290 / 0.9988 | 0.9107 / 0.9997 | 0.9966 / 0.9945 | 0.9999 / 0.9943 | 0.9231 / 0.9989 |
| 0.25 | 2 | 0.8790 / 0.9938 | 0.8815 / 0.9937 | 0.8244 / 0.9958 | 0.9944 / 0.9895 | 0.9996 / 0.9894 | 0.8786 / 0.9924 |
| 0.25 | 3 | 0.8324 / 0.9948 | 0.8350 / 0.9946 | 0.7757 / 0.9986 | 0.9924 / 0.9870 | 0.9996 / 0.9867 | 0.8319 / 0.9945 |
| 0.25 | 5 | 0.7841 / 0.9961 | 0.7857 / 0.9960 | 0.7484 / 0.9991 | 0.9907 / 0.9832 | 0.9998 / 0.9827 | 0.7884 / 0.9955 |
| 0.50 | 2 | 0.7479 / 0.9858 | 0.7535 / 0.9857 | 0.6219 / 0.9882 | 0.9881 / 0.9785 | 0.9990 / 0.9784 | 0.7470 / 0.9806 |
| 0.50 | 3 | 0.6705 / 0.9869 | 0.6757 / 0.9867 | 0.5537 / 0.9933 | 0.9847 / 0.9731 | 0.9993 / 0.9729 | 0.6599 / 0.9822 |
| 0.50 | 5 | 0.6180 / 0.9899 | 0.6212 / 0.9897 | 0.5468 / 0.9966 | 0.9832 / 0.9700 | 0.9996 / 0.9695 | 0.6183 / 0.9886 |
