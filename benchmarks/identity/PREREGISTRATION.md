# SAME_AS quality gate — pre-registration (spec §8.3)

Written 2026-09-30 and committed before any identity code exists. An
amendment is dated, says why, and is made before a run — never after one.

Corrected 2026-10-01, before any run and before any identity code: the
rationale of floor 3 is restated truthfully and the Alex nodes' expected
behaviour is disclosed. No floor, label or number changed.

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
3. **At least 6 true positives.** Floor 1 alone would pass a rule that links
   nothing, which has no false positives; this floor says the rule must link
   something. It is a backstop and does not bind in practice: recall ≥ 0.40 of
   18 positives already implies at least 8 true positives, so floor 2 is the
   binding one. It is kept, unchanged, as a statement of intent.

   A rule that matched only identical names is caught by floor 1, not floor 3.
   Six positive pairs carry an identical name on both sides (two Matthew
   Sherlins, two Grace Hoppers, two Acme Corps, two Launch 2026s, two Jane Does,
   and `countess-lovelace`, which declares the alias "Ada Lovelace", against
   `ada-lovelace`), so such a rule finds six. It would also link the two John
   Smiths and the two nodes named only "Alex", so its precision is 6 of 8,
   0.75, and it fails floor 1.

## Disclosed before the run

The set and the floors were written by the same author, who could compute
every score by hand, and a pilot against the plan-writing prototype gave
**9 true positives, 1 false positive, 9 false negatives on both predictions —
precision 0.900, recall 0.500.** So the gate is expected to pass at the
precision floor with no margin: one more false link fails it. That is
deliberate. This is a regression floor on a disclosed baseline, not a blind
test of an unseen rule, and any change to normalisation, floor or clustering
that adds a false link turns it red.

The Alex nodes are expected not to link, by arithmetic under the OD-1
normalisation (the key lowercased, the name in canonical form, each word of a
name of two or more words). `alex-1` holds {alex-1, alex} and `alex-2` holds
{alex-2, alex}: Jaccard 1/3. The `alex` stub holds {alex}: 1/2 against either,
and 1/4 against `alex-chen` or `alex-kim`, each of which holds four aliases.
`alex-1` against `alex-chen` is 1/5. `alex-chen` against `alex-kim` shares
only {alex}: 1/7. The highest is 0.5, below 0.6, so no Alex pair is an
expected false positive. The John Smith pair, by contrast, holds
{john-smith-nyc, john smith, john, smith} against {john-smith-sf, john smith,
john, smith}: 3/5 = 0.6, which reaches the floor and links.

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
