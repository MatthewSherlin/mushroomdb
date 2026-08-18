# Reader concurrency decision

**Date:** 2026-08-19
**Machine:** Apple M4 Pro (12 cores), 24 GB RAM, macOS 15.7.3 (24G419), arm64
**Rust:** rustc 1.92.0, cargo 1.92.0
**HEAD:** `4625386` (post-T4)
**Harness:** `cargo bench -p core-bench --bench engine -- read_contention_*`
(criterion 0.8.2, sample_size=12). All three rows are `run_contention` so
spawn + barrier sit in both numerator and denominator.

## Criterion (binding)

Swap `std::sync::RwLock` → `parking_lot::RwLock` behind the existing
`SharedDb::read()` / `write()` seam **only if**

1. read-path degradation under `read_contention_16r1w` exceeds **25% vs**
   `read_contention_1r0w`, **and**
2. the writer does not starve readers pathologically.

Otherwise: no code change. `parking_lot` is pre-approved for this swap only.

The T1 inheritance names the ratio
`median(16r1w) / median(1r0w)`. Both benches call `run_contention`, so
thread-spawn and the start barrier cancel in that quotient. They do **not**
cancel the 16 `set_prop` WAL-fsyncs that only the `*r1w` rows perform.

## Measured (this HEAD, this session)

| Bench | Median ± MAD | Shape |
|---|---|---|
| `read_contention_1r0w` | 40.632 µs ± 0.221 µs | `run_contention(db, 1, 16, 0)` |
| `read_contention_4r1w` | 69.499 ms ± 3.724 ms | `run_contention(db, 4, 16, 16)` |
| `read_contention_16r1w` | 76.674 ms ± 6.938 ms | `run_contention(db, 16, 16, 16)` |

**Binding ratio** `16r1w / 1r0w` = **1887×** (76.674 ms / 40.632 µs).

That number is not read-path degradation. `1r0w` is one reader doing 16
depth-1 neighborhoods (~1.15 µs each) plus spawn/join. `16r1w` is the same
reader loop **plus 16 exclusive `set_prop` calls** (each a WAL fsync, same
order as `rule_incremental_fire` ~4 ms) while readers spin until `stop`.
16 × ~4 ms ≈ 64–80 ms — the whole `*r1w` wave.

Same writer burst, more readers:

| | Ratio | vs 25% |
|---|---|---|
| `16r1w / 4r1w` | 1.103× (**+10.3%**) | under |
| `16r1w / 1r0w` | 1887× | writer work, not lock tax |

Adding twelve readers to an identical 16-write burst costs +10.3%. That is
the closest committed pair that holds writer work fixed. It does not clear
25%.

## std `RwLock` behavior observed

- `read()` / `write()` are exclusive of each other. The lock-discipline test
  `reader_during_write_observes_before_or_after_only` pins readers see 0 or
  BATCH, never a torn in-memory count.
- A reader holds the guard only for one `node_ref` + `neighborhood` (~1 µs)
  and drops it before the next acquire. The writer holds the guard for the
  whole `set_prop` (WAL + rule fire).
- Readers are not pathologically starved: every `16r1w` iteration joins;
  `concurrent_readers_sum_stats_while_writer_inserts` records samples during
  a 50-insert burst. The extra `while !stop` loop in the bench is the
  harness keeping the lock hot, not evidence that readers fail to run.
- The writer is not starved either: 76.674 ms / 16 writes ≈ 4.8 ms/write,
  in the same band as a solo `set_prop`. `16r1w` is only +10.3% vs `4r1w`.
- Server: guards are scoped and dropped before every `.await`.
  `crates/server` denies `clippy::await_holding_lock`. `std::sync::RwLock`
  read guards are `Send`; the deny is what keeps the never-across-await
  pattern, not the lock brand.

`std::sync::RwLock` parks the caller's thread. Under the tokio multi-thread
runtime that is a worker-thread tax (already documented on `router`), not
a measured 25% hit on this 10k graph.

## Decision

**No swap.** Leave `std::sync::RwLock`. Zero code change.

The binding ratio is 1887× because the numerator includes 16 fsyncs and the
denominator does not. Read-path degradation with writer work held fixed is
+10.3% (`16r1w` vs `4r1w`), below the 25% bar. `parking_lot` cannot remove
WAL fsync and is not justified.

Epoch snapshot readers stay the post-launch upgrade behind the same seam.

## Epoch-readers sketch (post-launch)

`SharedDb` already returns `impl Deref<Target = GraphDb<RealFs>>` from
`read()` and `impl DerefMut<…>` from `write()`. Callers (HTTP, MCP, CLI,
benches) never name the lock type. Both designs below fit that signature.

### Left-right dual replica

Two live `GraphDb` replicas. Readers `Deref` the published side. The writer
applies the mutation to the inactive side, then flips an `AtomicPtr` /
`ArcSwap<GraphDb>` so new `read()` calls see the new replica.

| | |
|---|---|
| Reads | Lock-free. No park on the tokio worker. |
| Writes | Applied twice (once per replica) or applied once and copied. |
| Memory | 2× the in-memory graph. |
| Storage | Current mutable `GraphDb` is enough — no immutability rewrite. |
| Consistency | A `read()` guard pins one replica for its lifetime, same as today. |

Cost is honest: 2× RAM on a design that already targets ~5–15 GB at 10M
nodes, and 2× CPU on every write. Fits pre-1.0 storage as it exists.

### ArcSwap snapshot

Readers load `Arc<ImmutableGraph>`. The writer builds the next snapshot and
`ArcSwap`s it. Cheap only if storage is already a persistent / CoW image.

That is the spec end-state: Sortledton adjacency + mmap'd zero-copy
snapshots. Today's `IdMap` / `Topology` / `ColumnStore` mutate in place; an
ArcSwap here would clone the world per write. **Not** a pre-1.0 swap.

### Recommendation

1. **Pre-1.0:** keep `std::sync::RwLock`. Numbers do not support
   `parking_lot`. If multi-client HTTP starts parking worker threads, wrap
   `read()`/`write()` in `tokio::task::spawn_blocking` (already noted on
   `router`) — that is a runtime placement fix, not a lock swap.
2. **When lock-free reads are required and storage is still mutable:**
   left-right dual replica. Same `read()` / `write()` types; 2× memory is
   the fee.
3. **Spec destination:** ArcSwap (or equivalent epoch publish) of an mmap'd
   snapshot once Sortledton + mmap land. That is the single-writer /
   epoch-reader model in the design spec.

**Sortledton + mmap snapshots are deferred post-launch.** Pre-1.0 keeps the
current in-memory topology + WAL + file snapshots. Acceptable against the
spec's "design for 10M, first workloads ~10k" clause.

## Lock discipline (unchanged)

No swap landed, so the server was not re-wired. Still required, still green:

- `#![deny(clippy::await_holding_lock)]` on `crates/server`
- HTTP / MCP take the guard, call core-api, drop the guard, then `.await`
- `core-api` lock-discipline tests (`clone_shares_state`,
  `concurrent_readers_sum_stats_while_writer_inserts`,
  `reader_during_write_observes_before_or_after_only`)
