# Rule engine vs hand-rolled maintenance — mushroomdb

## Machine / date

- **Date:** 2026-08-21T02:02:10
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

## SPECIALTY_MATCH comparison (Overlap, full scale)

> Both sides run at 10,000 nodes with 1000 property updates.
> Hand-rolled: Python Jaccard on all talent-company pairs. 
> Rule engine: token inverted-index (shared-specialty candidates only).

| Phase | hand-rolled | rule engine |
|---|---|---|
| Ingest (10,000 nodes) | 1.04 min | 9.203 s |
| Rule backfill / match computation | (included in ingest) | 1.17 min |
| Property updates (1000 × set_prop + retract/add) | 17.060 s | 9.777 s |
| **Total wall (spec only)** | **1.33 min** | **1.49 min** |

> Rule engine 0.9× faster than hand-rolled for SPECIALTY_MATCH.

### SPECIALTY_MATCH edge counts and drift

| Metric | hand-rolled | rule engine |
|---|---|---|
| SPECIALTY_MATCH edges | 5,165,384 | 5,165,384 |
| Spurious (hr only) | 0 | — |
| Missed (re only) | — | 0 |
| Total SPECIALTY drift | 0 | |

## SEMANTIC_MATCH comparison (VectorSimilar exact, 2,000-node sub-run)

> Exact VectorSimilar at 10,000 nodes extrapolates to ~26 min for the rule engine
  (measured: 61.6 s at 2,000; scales O(n²)).
> This sub-run uses 2,000 nodes to keep the comparison tractable.
> Hand-rolled: numpy batched cosine matrix multiply (~0.1 s at any scale).
> Rule engine: sequential exact cosine with early-exit (~61.6 s at 2k).

| Phase | hand-rolled (2k) | rule engine (2k) |
|---|---|---|
| Ingest (2,000 nodes) | 4.093 s | 1.828 s |
| Match computation (SEMANTIC only) | (included in ingest) | 1.05 min |
| Updates | 1.725 s | 1.174 s |
| SEMANTIC edges | 17,789 | 17,789 |
| SEMANTIC drift (total) | 0 | |

**Key finding**: for SEMANTIC_MATCH initial ingestion, numpy batched matrix multiply (~0.1 s) is dramatically faster than the rule engine's sequential exact cosine.  The rule engine's advantage is automatic incremental updates and zero maintenance code — on each `set_prop`, it re-evaluates only the changed node's candidates, while the hand-rolled code must do the same in Python.
## Correctness and maintenance burden

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

A naive implementation that only adds edges (no retraction) accumulates
stale matches after every property update.  The rule engine handles
retraction automatically and atomically on every `set_prop`.

- **Hand-rolled retraction count:** 476,178 retractions across 1000 updates
- **Hand-rolled addition count:**   415,466 additions across 1000 updates

## Methodology notes

- Both sides use the **same mushroomdb engine** and same Python API.
  The comparison isolates *maintenance strategy*, not the store.
- Hand-rolled: `insert_edge` / `delete_edge` / `ingest_batch` called from Python.
  numpy used for batched cosine matrix multiply (vectorizes the O(n²) computation).
- Rule engine: `db.create_rule()` + `db.set_prop()` — all derivation and
  retraction is automatic, atomic, and happens in Rust.
- Updates alternate RARE_SET (['landscape']) and COMMON_SET (5 popular specialties)
  to test both retraction and addition in every update pass.
- SEMANTIC_MATCH edges are unaffected by specialties updates (embedding is computed
  from industry+primary_specialty at ingest time via SHA-256 hash chain, not
  from the mutable specialties list).

