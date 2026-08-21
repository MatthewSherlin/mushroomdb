# Testing

mushroomdb is a young codebase. This page explains what mechanisms are in place to make
"it's new" a manageable risk rather than a reason to avoid it.

---

## Deterministic simulation testing (DST)

The `sim-harness` crate replaces real disk IO with `SimFs`, an in-memory filesystem that
injects crashes at precise byte offsets and Fs-call boundaries. Every test in this section
runs under `SimFs`; none of them touch the filesystem.

### How SimFs works

`SimFs` has two independent crash modes:

**Byte mode** (`SimFs::with_crash_after(n)`) — fires inside `append` once the cumulative
bytes-written counter crosses `n`. The torn `append` writes a prefix and sets a crash latch.
Subsequent `append` and `sync` calls return `Err`; `read` and `write_atomic` are not
affected (this models a power loss mid-WAL-write, not a full storage failure).

**Op mode** (`SimFs::with_crash_after_ops(n)`) — fires on the n-th Fs call (append, sync,
read, write\_atomic combined). A failed `write_atomic` leaves the old file content in place
(rename-never-happened semantics). This closes the snapshot-write and WAL-truncation crash
windows that byte mode does not reach.

After a crash, `surviving_state()` returns a clean `SimFs` with the on-disk bytes the
crash would have left behind, but with the crash latch reset. The recovered `GraphDb` is
opened from that state.

SimFs unit tests (in `crates/sim-harness/src/sim_fs.rs`):

- `crash_tears_the_inflight_append`
- `byte_crash_does_not_block_read_or_write_atomic`
- `op_crash_write_atomic_preserves_old_content`
- `op_crash_blocks_read`
- `op_crashed_stays_crashed`
- `torn_append_does_not_count_as_successful_op`

---

## Byte-offset crash sweeps

These tests run the workload at every crash point from byte 0 through the last byte
written and verify that recovery is always consistent. "Consistent" means: `open_with`
never panics or errors, node data and props are either all present or all absent per
commit, no derived edge references a tombstoned node, and `rebuild_rule` is a no-op.

**`recovery_is_consistent_at_every_crash_offset`**
Runs a 20-node chain workload with a mid-stream snapshot. Sweeps all byte offsets.
Bug class caught: torn WAL commits landing partial node-prop records.

**`recovery_byte_sweep_rules`**
Runs the full rules workload (KM, OV, DUMMY delete, numeric NW/NZ, geo GEO, exact VEC,
approximate VAPPROX with IVF; plus batches, ingest with auto-FK, rebuild\_rule, and
delete\_node of a derived-edge-owning node). Sweeps all byte offsets.
Bug class caught: rule-engine state diverging from node state after mid-stream crash.

**`cypher_write_dst_byte_sweep`**
Runs Cypher write statements (CREATE, SET, multi-node CREATE) through the WAL at every
crash point. Verifies the none-or-all invariant for multi-op Batch frames.
Bug class caught: Cypher writes not sharing WAL durability guarantees with API writes.

**`recovery_delete_heavy_byte_sweep`**
12 nodes, interleaved deletes and prop updates. Verifies no derived edge references a
tombstoned node at any crash offset.
Bug class caught: retraction gaps when a crash lands between delete and rule-fire.

**`write_batch_large_frame_dst_byte_sweep`**
12-op `write_batch` (insert\_node ×5, insert\_edge ×2, set\_prop ×3 including
rule-triggering ones, remove\_prop, delete\_node). At every byte offset, verifies
none-or-all atomicity: batch nodes are either all present or all absent.
Bug class caught: partial batch application after WAL frame torn mid-write.

**`write_batch_composition_sweep`**
write\_batch → snapshot → delete\_node (triggers top-k backfill) → write\_batch.
Verifies consistency across the snapshot boundary and after top-k backfill fires.
Bug class caught: snapshot-boundary interaction with top-k rule state.

**`recovery_byte_sweep_views`**
Materialized view definitions and values across byte-offset crashes.
Bug class caught: view state diverging from node data after partial WAL replay.

**`recovery_byte_sweep_fulltext`** and **`recovery_byte_sweep_fulltext_with_snapshot`**
Fulltext index across byte-offset crashes with and without a mid-stream snapshot.
Bug class caught: inverted index losing postings after a torn WAL write.

All byte-sweep tests also run an op-mode sweep (`recovery_op_sweep_rules`) that injects
crashes at every Fs-call boundary, covering snapshot `write_atomic` and WAL-truncation
`write_atomic` — the two paths byte mode cannot reach.

**Run the byte and op sweep tests:**

```text
CARGO_ENV=1 cargo test -p sim-harness --test crash_recovery -- --nocapture 2>&1 | grep -E "crash points|test result"
```

Or to run just one sweep with full output:

```text
cargo test -p sim-harness --test crash_recovery recovery_byte_sweep_rules -- --nocapture
```

---

## Oracle equivalence (scratch-recompute)

After any sequence of mutations, the incremental engine's derived edge set must equal
a from-scratch brute-force evaluation. This is checked continuously by a proptest suite.

### Rules oracle: `engine_matches_oracle`

`crates/sim-harness/tests/oracle_equivalence.rs` — proptest, 256 cases, sequences up
to 80 ops drawn from: InsertNode, InsertEdge, SetProp, SetF, SetTags, CreateRule
(9 templates: KeyMatch, FieldEqual, Overlap, All, Overlap-shared-etype, NumericWithin ×2,
GeoRadius, VectorSimilar), DeleteRule, DeleteNode, DeleteEdge, RemoveProp, CreateView,
DeleteView, EnableFulltext, DisableFulltext, FulltextSearch, and Batch (2–4 leaf ops).

After every op, the test asserts:

- `db.stats().nodes_live == oracle.node_count()`
- All prop values match across all 256 key slots
- Full edge set (user ∪ derived) matches `oracle.all_edges()`
- Edge weights match `oracle.derived_weights()` to within 1e-9
- `rebuild_rule` is a no-op (re-sweep must equal pre-sweep)
- Fulltext search results match `oracle.scratch_search()` for all indexed fields and queries

Bug class caught: incremental rule maintenance getting out of sync with the desired derived
set after create/delete operations, prop updates, or composed predicates.

**Run the proptest oracle suite:**

```text
cargo test -p sim-harness --test oracle_equivalence engine_matches_oracle
```

To run with more cases (slower, but covers more of the state space):

```text
PROPTEST_CASES=1024 cargo test -p sim-harness --test oracle_equivalence engine_matches_oracle
```

### Top-k oracle: `topk_dst_sweep`

`crates/sim-harness/tests/top_k_oracle.rs` — proptest, verifies that at every quiescent
point the engine's per-source out-neighbor set under a top-k rule equals a scratch
recompute of the k best candidates by score (and by key-order tiebreak for unscored rules).
Three rules run concurrently: FieldEqual k=1, NumericWithin k=3, Any(FieldEqual,
NumericWithin) k=2.

Bug class caught: top-k budget enforcement getting the wrong candidates after
insert-then-evict, backfill, or score change.

**Fixed top-k tests:**

- `topk_dst_insert_evict_and_backfill_sequence` — sequential insert/delete sequence that
  exercises eviction and backfill
- `topk_dst_numeric_k3_score_order` — verifies score ordering under NumericWithin
- `topk_evicted_pair_has_no_explain_entry` — evicted edges must not appear in `explain()`

**Run top-k tests:**

```text
cargo test -p sim-harness --test top_k_oracle
```

### View oracle

Inside `engine_matches_oracle` (proptest): after every op, for every live view, the stored
incremental value is compared to `db.scratch_view_value()`. Degree and Count use exact
equality; Sum and Avg use a 1e-6 epsilon (disclosed f64 accumulation drift).

Bug class caught: view counter diverging from actual topology after edge retraction or
node delete.

### Fulltext oracle

Inside `engine_matches_oracle` (proptest): after every op, `db.search(field, query)` is
compared to `oracle.scratch_search(field, query)` for all indexed fields and query strings.
Scratch search tokenizes and evaluates the boolean query independently.

Bug class caught: postings list diverging from actual node content after enable/disable
or node mutations.

**Fixed fulltext test:**

- `overlap_rule_equality_on_nonempty_derived_sets` (query\_equivalence.rs) — Cypher
  MATCH result set matches the traversal API on a non-empty derived edge set

---

## Cypher ↔ traversal equivalence

`crates/sim-harness/tests/query_equivalence.rs` — proptest. For random small graphs with
an Overlap rule:

- `MATCH (a {k: $key})-[r:T]->(b) RETURN b` row set equals `neighbors` Out
- 1-hop undirected Cypher row set equals `grouped_by_edge_type` bucket

**Run:**

```text
cargo test -p sim-harness --test query_equivalence
```

---

## Torn-write tests (WAL unit layer)

`crates/sim-harness/src/sim_fs.rs` — the torn-write unit tests above verify `SimFs`
itself. The WAL layer is tested through the crash sweeps: every crash in the byte-mode
sweep represents a potential torn WAL frame. The WAL uses CRC checksums per frame;
`recovery_is_consistent_at_every_crash_offset` verifies that a torn frame is detected
and discarded rather than partially applied.

Bug class caught: WAL replaying a record with a valid CRC prefix but a missing or corrupt
payload.

---

## Replay identity tests

**`approximate_wal_replay_identity`**
Builds 20 nodes with 4 vector clusters, creates an approximate (IVF-Flat) rule, captures
the derived edge set, drops and reopens the database (WAL replay), and asserts the edge
set is identical. Same rule + same data → same clusters → same edges.

Bug class caught: IVF state not persisting across a reopen, or non-deterministic cluster
assignment on replay.

**`recovery_byte_sweep_rules`** (also a replay identity test)
At every crash point, `open_with(surviving_state)` re-materializes derived edges from
scratch and the oracle asserts the result matches brute-force evaluation.

**Run replay identity tests:**

```text
cargo test -p sim-harness --test oracle_equivalence approximate_wal_replay_identity
```

---

## Recall floors for approximate mode

`VectorSimilar` with `approximate: true` uses IVF-Flat candidate selection. The recall
floor constants are binding (declared in `crates/sim-harness/src/lib.rs`; do not lower
without explicit sign-off):

```text
APPROX_RECALL_FLOOR_QUIESCED = 0.90   # fully quiesced: all nodes indexed, IVF fitted
APPROX_RECALL_FLOOR_RECOVERY = 0.85   # any crash-recovery or early IVF state
```

Tests:

- **`approximate_recall_above_floor_quiesced`** — 20 2-D unit vectors in 4 clusters;
  asserts per-query recall ≥ 0.90 on a fully built index.
- **`approximate_recall_above_floor_after_rebuild`** — same dataset, asserts recall
  ≥ 0.85 after an explicit `rebuild_rule`.
- **`topk_approx_recall_floor`** — approximate rule inside the top-k oracle sweep;
  asserts recall ≥ 0.90 at every quiescent point.
- In `recovery_byte_sweep_rules`, every crash-recovery state with `vec_approx` live
  asserts recall ≥ `APPROX_RECALL_FLOOR_RECOVERY`.

**Run recall tests:**

```text
cargo test -p sim-harness --test oracle_equivalence approximate_recall
cargo test -p sim-harness --test top_k_oracle topk_approx_recall_floor
```

---

## Running everything

Full Rust gate (required before every commit touching `crates/`):

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --workspace --examples
cargo test --workspace
cargo bench --no-run
```

All five commands must exit 0. The bench step is compile-only.

To run only the sim-harness suite:

```text
cargo test -p sim-harness
```

To run with output (useful for the sweep progress lines):

```text
cargo test -p sim-harness -- --nocapture 2>&1 | grep -E "crash points|recall|test result"
```

---

## What is not yet covered

- **Multi-statement transactions** — not implemented; no test exists or is needed until
  the feature ships.
- **Concurrent writer correctness** — the engine is single-writer by design (`RwLock`
  facade). Race conditions between writers are excluded by the architecture, not tested.
- **Network-layer correctness** — the HTTP and WebSocket layers have golden-shape tests
  in `crates/server/tests/`; they do not yet have fault-injection or load tests.
- **Differential Cypher testing against Neo4j** — the harness in `benchmarks/test_harness.py`
  runs against a live Neo4j instance; it is not part of `cargo test --workspace`. Known
  Cypher gaps are documented in [`docs/site/query.md`](query.md).
