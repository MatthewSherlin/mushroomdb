# Phase 3 — Storage physics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace HashMap topology and HashMap columns with a layout that can hit 100k snapshot open **< 1 s** and make a 10M-node RAM budget plausible. Freeze a versioned on-disk schema so 1.0 can stop breaking files.

**Architecture:** Three sequential PRs: columns → topology → snapshot/WAL versioning. `GraphView` / `GraphMut` keep the same method names (`neighbors`, `prop`, `set`) so query and rules do not rewrite. **Do not start until Gate G2.**

**Tech Stack:** Existing crates only. Snapshot encoding may add `rkyv` **only if** a spike in Task 1 proves zstd+bincode cannot break 1 s at 100k. Default: custom packed CSR image + zstd, no rkyv. Spike is throwaway.

**Spec:** `docs/superpowers/specs/2026-08-25-best-graph-db.md` §4 Gate G3, §5 C12, §6 honesty overlay

## Global Constraints

- Gate G2 must be green.
- No dual-replica epoch readers in this phase (still HashMap-free first).
- IDs remain dense `u32`, never reused. Allocation that would wrap returns `GraphError::Corrupt { detail: "id space exhausted" }` (or a new `IdSpaceExhausted` variant appended to `GraphError` — append only).
- WAL discriminants stay append-only. New snapshot magic version **7**. V6 remains readable.
- DST crash sweeps must pass on the new format.
- Dogfood 100k shape (`dogfood/results/scale-100k.md`) is the G3 bench: snapshot open < 1 s, RSS recorded.

---

## File map

| File | Role |
|---|---|
| `crates/core-storage/src/columns.rs` | `Vec` + null bitmap + interned strings |
| `crates/core-storage/src/interner.rs` | shared string intern for column Str |
| `crates/core-storage/src/topology.rs` | typed CSR + per-vertex unsorted insert buffer (Sortledton-style) |
| `crates/core-storage/src/snapshot.rs` | V7 packed image |
| `crates/core-storage/src/wal.rs` | dense-id records **appended** as new variants; old string records still decode |
| `crates/core-storage/src/idmap.rs` | checked `u32` alloc |
| `crates/core-api/src/db.rs` | use new methods; WAL write prefers dense-id variants for `SetProp`/`InsertEdge` after intern |

---

### Task 1: Physical columns

**Files:**
- Modify: `crates/core-storage/src/columns.rs`
- Modify: `crates/core-query` ColumnHandle users (keep `ColumnHandle::get`)
- Test: `crates/core-storage/src/columns.rs` unit tests; `crates/core-api/tests/mutations.rs`

**Interfaces:**
- Consumes: `Value`, `Interner`
- Produces: `ColumnStore` public methods **unchanged**: `set`, `get`, `column`, `remove`, `remove_all`, `fields`. Internals: per-field typed arrays where all live values share a tag; mixed-type fields stay a spill `HashMap<u32, Value>` (document as slow path).

- [ ] **Step 1: Write a failing bench/test that pin the API**

Keep existing column tests. Add:

```rust
#[test]
fn str_column_does_not_clone_on_get() {
    let mut c = ColumnStore::new();
    c.set(0, "name", Value::Str("ada".into()));
    assert_eq!(c.get(0, "name"), Some(&Value::Str("ada".into())));
}
```

This still passes on HashMap — the real pin is RSS in Task 4. For Task 1, add:

```rust
#[test]
fn mixed_type_column_round_trips() {
    c.set(0, "x", Value::Int(1));
    c.set(1, "x", Value::Str("a".into()));
    assert!(matches!(c.get(0, "x"), Some(Value::Int(1))));
    assert!(matches!(c.get(1, "x"), Some(Value::Str(_))));
}
```

- [ ] **Step 2: Implement Vec+bitmap for homogeneous Int/Float/Bool/Str.** Str uses intern ids (`u32`) plus intern table owned by `ColumnStore` **or** the existing `Interner` passed in. Prefer `ColumnStore` owning a private intern so `GraphView` signatures stay the same.

- [ ] **Step 3: Tests** `cargo test -p mushroomdb-storage` and `cargo test -p mushroomdb-core-api --test mutations`

- [ ] **Step 4: Commit** `feat: columnar property store with typed vecs and interned strings`

---

### Task 2: Physical topology (CSR + insert buffer)

**Files:**
- Modify: `crates/core-storage/src/topology.rs`
- Test: existing topology unit tests (sorted neighbors, dual inn/out, remove)

**Interfaces:**
- Public methods **unchanged**: `add_edge`, `remove_edge`, `neighbors` → `&[u32]`, `degree`, `etypes`, `edge_count`.
- Internals: per `(etype, dir, vertex)` a frozen sorted block plus an unsorted delta buffer. `neighbors()` returns a merged view. **Problem:** today's `&[u32]` cannot merge two slices. **Lock:** change `neighbors` to return `impl Iterator<Item = u32>` **or** fill a thread-local/scratch `Vec` and return `&[u32]` from a `Topology::neighbors_buf(&mut self, …)` — that mutates. Query is `&self`.

**Chosen:** `neighbors` returns `Cow<[u32]>` — borrowed when the delta buffer is empty (scan path), owned merged vec when dirty. Update `core-query` `expand` and `core-rules` to take `Cow` or `as_ref()`. This is the one signature break in Phase 3; it is internal to the workspace (not Python/HTTP).

- [ ] **Step 1: Write the failing compile by changing the signature in a test helper, then implement merge.**

Existing tests `edges_are_typed_directed_sorted_deduped` must still see sorted unique neighbors.

- [ ] **Step 2: Implement Sortledton-style: on `add_edge`, push to unsorted buffer; when buffer len > 32, merge+sort into the frozen block.**

- [ ] **Step 3:** `cargo test --workspace` — query_equivalence and oracle must pass (neighbor order is part of Cypher determinism — **merged output must be sorted**).

- [ ] **Step 4: Commit** `feat: typed CSR topology with sorted insert buffers`

---

### Task 3: V7 snapshot + dense-id WAL + id overflow

**Files:**
- Modify: `crates/core-storage/src/snapshot.rs` `VERSION = 7`
- Modify: `crates/core-storage/src/wal.rs` new variants at the **end**:

```rust
InsertNodeId { label: u32, key: String, props: Vec<(u32, Value)> }, // still has key string once
SetPropId { id: u32, field: u32, value: Value },
InsertEdgeId { etype: u32, src: u32, dst: u32 },
```

Old string variants remain for decode. Live `log_then_apply` writes the `*Id` variants after intern.

- Modify: `idmap.rs` `get_or_insert` returns `Result<u32>` **or** panics converted to `GraphError` at the `GraphDb` layer. Prefer:

```rust
pub fn try_insert(&mut self, key: &str) -> Result<u32, GraphError>
```

and keep `get_or_insert` for tests that cannot hit 2^32.

- Test: golden V6 still opens; new V7 golden fixture; crash_recovery op sweep; `open` at 100k measured in `dogfood/` (record in `dogfood/results/scale-100k.md`)

- [ ] **Step 1: Failing test** `decode(v6_bytes)` still works after VERSION=7 default encode.

- [ ] **Step 2: Implement V7 as: magic+u16=7 + zstd(crc + packed CSR + packed columns + bincode of leftover: rule_defs, provenance, ivf, views).** mmap the packed topology on open if the OS allows (`memmap2` is a **new crate** — ask before adding). **Default without new crate:** read into owned CSR vecs (still far faster than HashMap bincode). Revisit mmap as a follow-up PR inside this phase if `memmap2` is approved.

- [ ] **Step 3:** DST + golden fixtures.

- [ ] **Step 4: Commit** `feat: snapshot V7 packed CSR/columns; dense-id WAL variants`

---

## G3 gate

Record in `dogfood/results/scale-100k.md` (same machine note as existing runs):

| Metric | Bar |
|---|---|
| V7 snapshot open, 100k matching shape | < 1 s |
| RSS after open | write the number; must be **below** HashMap 4.7 GiB ingest RSS or snapshot-open 12.7 GiB peak |
| `cargo test --workspace` | pass |

Do not start wait-list completeness (multi-label, clustering) here.
