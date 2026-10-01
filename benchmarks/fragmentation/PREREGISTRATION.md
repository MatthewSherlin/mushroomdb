# Fragmentation probe — pre-registration (spec §8.2)

Written 2026-09-30 and committed before the first run. An amendment is dated,
says why, and is made before a run — never after one.

## The question

How much rule accuracy is lost when one entity's properties are split across
several alias nodes instead of living on one? Link-first identity (spec §3.3)
accepts that cost for 0.7. This number decides whether 0.8 builds a canonical
node.

## The world

The association world, day 0: `association/build.py` `world(20260910, 2000)` —
1,400 Talent, 400 Company, 200 Job. The rules are `build.task_rules()` — the
world's nine single-predicate rules; the semantic rule is excluded because
truth.py does not evaluate `VectorSimilar` — plus two conjunctions this probe
adds:

| rule | from → to | predicate | edge |
|---|---|---|---|
| `fit_all_tc` | Talent → Company | All[FieldEqual industry, GeoRadius location 160.9 km, NumericWithin size_bucket 1.0] | `FIT_ALL` |
| `fit_all_tj` | Talent → Job | All[FieldEqual industry, Overlap specialties ≥ 0.15] | `FIT_ALL` |

Every world rule reads one field, so whichever alias holds that field derives
the edge: without a conjunction, fragmentation could not cost a reader that
follows `SAME_AS` anything, and the probe would be blind to the mechanism §3.3
names ("none may clear a threshold the resolved person would").

## Fragmentation

A seeded generator, `random.Random(f"fragmentation-20260910-{fraction}-{k}")`,
picks each Talent with probability `fraction` and splits it into `k` aliases
`key~0 … key~(k-1)`. `~0` is canonical: it is created first, so it holds the
lowest id. Each scalar rule field (`industry`, `location`, `size_bucket`) goes
to one alias, uniformly at random. Each element of a list rule field
(`specialties`, `design_styles`) goes to one alias, uniformly at random. Every
other property stays on `~0`, and every alias carries the `name`.

## Axes

Fragmented fraction ∈ {0.10, 0.25, 0.50} × aliases per entity k ∈ {2, 3, 5}:
nine cells.

## Scoring

Truth is `association/truth.py` `derived_edges` over the unfragmented world —
the engine's predicates transcribed, with a committed cross-check of zero
disagreements. Each cell is scored twice, as recall and precision against truth
after mapping each alias key to its entity:

1. **canonical — the primary score.** Only edges whose source is the canonical
   alias count: what a reader that resolves an entity to its canonical node, and
   does not follow `SAME_AS`, sees.
2. **any-alias — the secondary score.** An edge counts when any alias of its
   source holds it: what a reader following `SAME_AS` sees.

Each is reported over every rule, over the leaf rules, and over the two
conjunctions.

**What each score can show.** A leaf rule's field lands on the canonical alias
with probability 1/k, so canonical recall on leaf rules is close to
1 − fraction × (k−1)/k by construction — about 0.83 at the reference cell. The
canonical score is primary by decision, not because it carries the most
information; the conjunction rows and the any-alias column are where the cost
§3.3 describes shows up, and the summary prints them beside it.

## The decision

The primary number is **canonical recall over every rule at fraction 0.25,
k = 3**. Loss = 1 − that recall.

- **Loss ≤ 0.05 → SMALL.** Link-only is correct indefinitely; §3.3's accepted
  cost is real and cheap.
- **Loss > 0.05 → LARGE.** The canonical node is 0.8's headline, and spec §2's
  "destructive entity merge" non-goal is revisited on evidence.

0.05 is five times the margin that decided 0.6.12's association gate (1.000
against 0.990).

## Pilots run before this was written — disclosed

- **Research pilot, 2026-09-30**, leaf rules only, 25% of Talent split, scale
  2,000, lists not split: canonical recall 0.8801 / 0.8346 / 0.8025 at
  k = 2 / 3 / 5; any-alias recall 1.0000 at every k.
- **Plan pilot, 2026-09-30**, this script at scale 200: reference-cell canonical
  recall 0.8450, loss 0.1550.

Both predict **LARGE**, and the 0.05 threshold was written knowing them. This
is a decision rule stated before the run, not a blind test. What the run adds
is the full curve at scale 2,000 and the conjunction rows no pilot measured at
that scale.

## Procedure

    bindings/python/.venv/bin/python benchmarks/fragmentation/probe.py \
        --out benchmarks/fragmentation/results/$(date -u +%Y%m%dT%H%M%SZ)

The script computes the verdict; nobody judges it. Its `summary.md` is
committed as it came out.
