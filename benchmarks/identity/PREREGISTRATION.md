# SAME_AS quality gate — pre-registration (spec §8.3)

Written 2026-09-30 and committed before any identity code exists. An
amendment is dated, says why, and is made before a run — never after one.

## What is measured

The identity preset, `memory_schema::memory_identity()`: `Overlap` on each
node's normalised `aliases` list at Jaccard ≥ 0.6, eleven rules — same-label
for Person, Org, Project, Concept, Event and the provisional Entity, and
Entity → each entity label. The set is `benchmarks/identity/labelled.json`:
34 nodes, 21 identities, 561 pairs, 18 positive. Every node is written through
`remember` in list order — `entities[]` when its label is known, `about` when
it is a stub — on a store with the memory defaults and the preset applied.

Two predictions are scored against the labels:

1. **pairwise** — the `SAME_AS` claims, one per unordered pair;
2. **clusters** — co-membership in the complete-linkage identities `analyze`
   reports.

## The floors — each prediction must clear all three

1. **Precision ≥ 0.90.** At most one false link in ten. Spec O-3 settled
   "precision over recall": a false link puts a wrong fact in every answer
   that follows it.
2. **Recall ≥ 0.40.** The rule is designed to miss nickname and formal-name
   aliases (an alias rule cannot know that Chris is Christopher), so this is a
   floor on the categories it is meant to catch, not on every category.
3. **At least 6 true positives.** A rule that links nothing has no false
   positives and would pass floor 1 alone. The set holds five positive pairs
   whose two names are identical (two Matthew Sherlins, two Grace Hoppers, two
   Acme Corps, two Launch 2026s, two Jane Does), so a rule that could only
   match identical names would find five and fail: passing takes a link
   across a difference in spelling.

## Disclosed before the run

The set and the floors were written by the same author, who could compute
every score by hand, and a pilot against the plan-writing prototype gave
**9 true positives, 1 false positive, 9 false negatives on both predictions —
precision 0.900, recall 0.500.** So the gate is expected to pass at the
precision floor with no margin: one more false link fails it. That is
deliberate. This is a regression floor on a disclosed baseline, not a blind
test of an unseen rule, and any change to normalisation, floor or clustering
that adds a false link turns it red.

The one expected false positive is the identical-full-name pair,
`john-smith-nyc` and `john-smith-sf`: two strangers with one name are
indistinguishable to an alias rule. The link is visible, scored and
explainable, and can be undone by changing either node's aliases — which is
link-first's whole argument for accepting it.

## Procedure

    TS=$(date -u +%Y%m%dT%H%M%SZ)
    mkdir -p benchmarks/identity/results/$TS
    cargo run --release -p mushroomdb --example identity_gate -- \
        benchmarks/identity/labelled.json \
        > benchmarks/identity/results/$TS/summary.md

The runner computes the verdict and exits 1 when a floor is missed. Its
`summary.md` is committed as it came out, PASSED or FAILED.
