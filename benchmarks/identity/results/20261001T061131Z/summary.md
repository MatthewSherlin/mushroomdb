# SAME_AS quality gate

Labelled set: `benchmarks/identity/labelled.json` — 34 nodes, 561 labelled pairs, 18 of them the same entity. Identity preset: `Overlap` on `aliases` at ≥ 0.6.

## Gate

The pre-registered §8.3 floors, each applied to both predictions:

1. precision ≥ 0.90
2. recall ≥ 0.40
3. at least 6 true positives

| prediction | TP | FP | FN | precision | recall | verdict |
|---|---|---|---|---|---|---|
| pairwise SAME_AS | 9 | 1 | 9 | 0.900 | 0.500 | **PASSED** |
| identity clusters | 9 | 1 | 9 | 0.900 | 0.500 | **PASSED** |

**Verdict: PASSED**

## Every miss

| prediction | kind | a | b | categories |
|---|---|---|---|---|
| pairwise SAME_AS | false positive | `john-smith-nyc` | `john-smith-sf` | negative: identical full name, different person / negative: identical full name, different person |
| pairwise SAME_AS | false negative | `Ada_Lovelace` | `countess-lovelace` | stub spelling the full name / formal name with a declared alias |
| pairwise SAME_AS | false negative | `Matthew_Sherlin` | `matt` | stub spelling the full name / nickname stub |
| pairwise SAME_AS | false negative | `ada-lovelace` | `countess-lovelace` | full name / formal name with a declared alias |
| pairwise SAME_AS | false negative | `amazing-grace` | `ghopper` | longer full name / full name, other key |
| pairwise SAME_AS | false negative | `amazing-grace` | `grace-hopper` | longer full name / full name |
| pairwise SAME_AS | false negative | `anthropic` | `anthropic-pbc` | org, legal suffix / org, legal suffix |
| pairwise SAME_AS | false negative | `chris-lee` | `christopher-lee` | nickname / nickname |
| pairwise SAME_AS | false negative | `matt` | `matthew-sherlin` | nickname stub / full name |
| pairwise SAME_AS | false negative | `matt` | `msherlin` | nickname stub / full name, other key |
| identity clusters | false positive | `john-smith-nyc` | `john-smith-sf` | negative: identical full name, different person / negative: identical full name, different person |
| identity clusters | false negative | `Ada_Lovelace` | `countess-lovelace` | stub spelling the full name / formal name with a declared alias |
| identity clusters | false negative | `Matthew_Sherlin` | `matt` | stub spelling the full name / nickname stub |
| identity clusters | false negative | `ada-lovelace` | `countess-lovelace` | full name / formal name with a declared alias |
| identity clusters | false negative | `amazing-grace` | `ghopper` | longer full name / full name, other key |
| identity clusters | false negative | `amazing-grace` | `grace-hopper` | longer full name / full name |
| identity clusters | false negative | `anthropic` | `anthropic-pbc` | org, legal suffix / org, legal suffix |
| identity clusters | false negative | `chris-lee` | `christopher-lee` | nickname / nickname |
| identity clusters | false negative | `matt` | `matthew-sherlin` | nickname stub / full name |
| identity clusters | false negative | `matt` | `msherlin` | nickname stub / full name, other key |
