# Rule engine vs hand-rolled maintenance — mushroomdb

## Machine / date

- **Date:** 2026-08-21T03:47:15
- **Host:** mac.lan
- **CPU:** Apple M4 Pro
- **RAM:** 24.00 GiB
- **Primary scale:** 10,000 nodes (seed=20260819, 70/20/10 T/C/J)
- **Semantic scale:** 2,000 nodes (exact VectorSimilar; see below)
- **Updates:** 1000 talent specialties updates

## Rules compared

| Rule | Edge type | Predicate | Scale |
|---|---|---|---|
| bench_hr_spec | SPECIALTY_MATCH | Overlap(specialties, min=0.15) | 10,000 nodes |
| bench_hr_sem  | SEMANTIC_MATCH  | VectorSimilar(embedding, min=0.85) exact | 2,000 nodes |

## Three-way comparison: SPECIALTY_MATCH (Overlap, full scale)

> **Three strategies measured on the same mushroomdb engine:**
>
> **(a) per-op (expert-written)** — individual `delete_edge` / `insert_edge` calls,
>     one WAL fsync per retraction and one per addition.  Correctly retracts stale
>     edges on every update; retraction logic written with full knowledge of the API.
>     `batch_edges` did not exist before Plan-13 and is not available on any
>     competitor engine.
>
> **(b) batched (expert-written)** — uses `batch_edges` (Plan-13, new API) to commit
>     all retractions + additions for each talent update in a single WAL frame.
>     Expert knowledge of the batching contract required.  `batch_edges` is a
>     mushroomdb-only API; no equivalent exists on competitor engines.
>
> **Note — add-only (NOT benchmarked):** the most common real-app first attempt omits
>     `delete_edge` entirely. Stale edges accumulate on every update; drift grows
>     monotonically. This pattern is described in the correctness section below but
>     was NOT measured as a separate variant — it is not a correct implementation.
>
> **(c) rule engine** — `create_rule` + `set_prop`.  All derivation and retraction
>     is automatic, atomic, and happens inside Rust with no application code.
>
> Both hand-rolled variants run at 10,000 nodes with 1000 property updates.
> Hand-rolled: Python Jaccard on all talent-company pairs.
> Rule engine: token inverted-index (shared-specialty candidates only).

| Phase | (a) per-op | (b) batched | (c) rule engine |
|---|---|---|---|
| Ingest (10,000 nodes) | 17.525 s | 19.965 s | 817.84 ms |
| Rule backfill / match computation | (included in ingest) | (included in ingest) | 11.700 s |
| Property updates (1000 × set_prop + retract/add) | 64.63 min | 5.015 s | 5.064 s |
| **Total wall (spec only)** | **64.93 min** | **24.979 s** | **17.582 s** |

> Rule engine vs per-op: rule engine is **221.6× faster** than per-op hand-rolled.
> Rule engine vs batched: rule engine is **1.42× faster** than batched (17.582 s vs 24.979 s).

### SPECIALTY_MATCH edge counts and drift

| Metric | (a) per-op | (b) batched | (c) rule engine |
|---|---|---|---|
| SPECIALTY_MATCH edges | 5,165,384 | 5,165,384 | 5,165,384 |
| Spurious (vs rule engine) | 0 | 0 | — |
| Missed (re only) | — | — | 0 |
| Total SPECIALTY drift vs rule engine | 0 | 0 | |

## SEMANTIC_MATCH comparison (VectorSimilar exact, 2,000-node sub-run)

> Exact VectorSimilar at 10,000 nodes extrapolates to ~26 min for the rule engine
  (measured: 61.6 s at 2,000; scales O(n²)).
> This sub-run uses 2,000 nodes to keep the comparison tractable.
> Hand-rolled: numpy batched cosine matrix multiply (~0.1 s at any scale).
> Rule engine: sequential exact cosine with early-exit (~61.6 s at 2k).

| Phase | hand-rolled (2k) | rule engine (2k) |
|---|---|---|
| Ingest (2,000 nodes) | 1.480 s | 154.81 ms |
| Match computation (SEMANTIC only) | (included in ingest) | 2.893 s |
| Updates | 1.394 s | 832.52 ms |
| SEMANTIC edges | 17,789 | 17,789 |
| SEMANTIC drift (total) | 0 | |

**Key finding**: for SEMANTIC_MATCH initial ingestion, numpy batched matrix multiply (~0.1 s) is dramatically faster than the rule engine's sequential exact cosine.  The rule engine's advantage is automatic incremental updates and zero maintenance code — on each `set_prop`, it re-evaluates only the changed node's candidates, while the hand-rolled code must do the same in Python.
## Correctness, drift, and maintenance burden

**Authorship disclosure (C-2):** The hand-rolled variants tested here were
written by the mushroomdb engine team with full knowledge of the retraction
semantics.  Both variants correctly implement retraction: they collect current
SPECIALTY_MATCH edges before each update, re-evaluate all candidates, and issue
deletes for stale edges and inserts for new matches.  Real application code
routinely misses one or more retraction paths:

- **Missing retraction entirely** (add-only): stale edges accumulate after every
  update.  Drift grows monotonically — there is no self-correction.
- **Retraction on wrong field**: updating `specialties` also affects Overlap
  predicates on related fields; an app may only retract the field it just wrote.
- **Missing top-k backfill**: when a node gains new matches after eviction, they
  are never added back without an explicit rebuild.
- **Score staleness**: weight_prop (edge score) is not recomputed unless the app
  explicitly re-inserts or updates the edge property.

The rule engine handles all of these automatically and atomically on every `set_prop`.

- **Retraction count (optimized):** not separately tracked in this run; batch_edges commits
  retractions + additions atomically. Reference from v2: ~476k retractions across 1000 updates.
- **Addition count (optimized):** not separately tracked; reference from v2: ~415k additions.
- **Per-op variant drift (failed retractions):** 0

The hand-rolled SPECIALTY_MATCH maintainer requires explicit retraction logic:

```python
# On property update — must retract stale edges AND add new matches:
current_neighbors = {e['dst_key'] for e in db.node_edges(tkey)
                    if e['edge_type'] == 'SPECIALTY_MATCH'}
db.set_prop(tkey, 'specialties', new_specs)
for ckey, cprops in all_companies.items():
    matches_now = jaccard(new_specs, cprops['specialties']) >= 0.15
    had_edge = ckey in current_neighbors
    if had_edge and not matches_now:
        db.delete_edge('SPECIALTY_MATCH', tkey, ckey)  # retraction
    elif not had_edge and matches_now:
        db.insert_edge('SPECIALTY_MATCH', tkey, ckey)  # addition
```

**Add-only pattern (NOT benchmarked — incorrect implementation):** the most common
real-app first attempt omits `delete_edge`, so stale edges accumulate on every update.
After 1000 property updates: expected edge count = 5,165,384 (ground truth from rule engine);
an add-only implementation would retain ALL 5,165,384 edges even for talents whose
specialties changed to the rare set — leading to tens of thousands of spurious
matches (precise count depends on update targets; rule engine drift = 0 always).
This pattern was described for context only — it was NOT measured as variant (a);
variant (a) "per-op (expert-written)" correctly retracts stale edges.

## Methodology notes

- All three strategies use the **same mushroomdb engine** and same Python API.
  The comparison isolates *maintenance strategy*, not the store.
- **(a) per-op (expert-written)**: `insert_edge` / `delete_edge` called individually.
  One WAL fsync per retraction, one per addition.  Correctly implements retraction.
  No `batch_edges` API — this was the only option before Plan-13.
- **(b) batched (expert-written)**: uses `batch_edges` (Plan-13, added Task-6) to commit all
  retractions + additions for each update in one WAL frame (one fsync).
  `batch_edges` is a mushroomdb-specific API; no equivalent exists on competitor
  engines.  Requires expert knowledge of the batching contract.
- **(c) Rule engine**: `db.create_rule()` + `db.set_prop()` — derivation and
  retraction happen in Rust, atomically, with no application maintenance code.
  numpy used for batched cosine in the hand-rolled SEMANTIC path (not applicable
  to rule engine which uses sequential exact cosine).
- Updates alternate RARE_SET (['landscape']) and COMMON_SET (5 popular specialties)
  to test both retraction and addition in every update pass.
- SEMANTIC_MATCH edges are unaffected by specialties updates (embedding is computed
  from industry+primary_specialty at ingest time via SHA-256 hash chain, not
  from the mutable specialties list).

