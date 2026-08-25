# Phase 4 — Linking moat Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make linking hard to copy: real ANN, agent recall that does not require a pre-derived `SIMILAR` edge, one 3-node rule form, and incremental Cypher result subscriptions.

**Architecture:** May start after G1 (IVF cosine-normalize exists). Prefer after G3 so HNSW sits on packed vectors. Three PRs: in-tree HNSW → 3-node rules → `subscribe_query`.

**Tech Stack:** In-tree HNSW in `crates/core-rules/src/hnsw.rs`. **No usearch / instant-distance / hnsw_rs crate** unless a spike proves in-tree recall < 0.90 at 5k/1536 and Matthew approves a dependency.

**Spec:** `docs/superpowers/specs/2026-08-25-best-graph-db.md` §4 Gate G4, §5 A2/A4/D15/D16

## Global Constraints

- G1 required (IVF metric fix + default top-k). G3 recommended.
- `approximate: true` keeps working; implementation switches from IVF-Flat to HNSW. IVF code stays as fallback behind `#[cfg]` or `RuleDef` later flag — **default approximate path becomes HNSW**.
- Per-query recall on the 5k/1536 dogfood probe: **min ≥ 0.90**, mean ≥ 0.95. Document if a rebuild is required after bulk ingest.
- 3-node rules are a **new predicate/rule form**, not Cypher-in-rules. Generality Guarantee still holds.
- `subscribe_query` supports a documented subset: `MATCH (n:Label) WHERE … RETURN n` and `MATCH (a)-[r:TYPE]->(b) RETURN a,b,r` — not full Cypher.
- MCP `find_similar` must return neighbors for a **query vector** even when no `SIMILAR` edges exist yet (compute ANN, optional to materialize).

---

## File map

| File | Role |
|---|---|
| `crates/core-rules/src/hnsw.rs` | new; graph of `u32` node ids, cosine distance on normalized vecs |
| `crates/core-rules/src/index.rs` | `CandidateSpec::Hnsw { field }` |
| `crates/core-rules/src/engine.rs` | wire HNSW candidates; persist graph in snapshot export |
| `crates/core-storage/src/snapshot.rs` | V7/V8 blob for HNSW (append field on SnapshotState — **version bump if positional bincode**; Phase 3 V7 should have used a versioned map. If Phase 3 landed a packed format, add a named section `hnsw`. If Phase 4 runs **before** Phase 3, append `hnsw_bytes: BTreeMap<String, Vec<u8>>` to `SnapshotState` and bump snapshot version — pre-1.0 break is allowed.) |
| `crates/core-rules/src/def.rs` | `Predicate::Via` or `RuleDef.path` for 3-node |
| `crates/server/src/mcp.rs` | `find_similar` accepts `vector` and `field` |
| `crates/core-api/src/subscription.rs` | `subscribe_query` |
| `crates/server/src/subscribe.rs` | WS message `{"type":"query_sub", "cypher":"..."}` |

---

### Task 1: In-tree HNSW + MCP `find_similar`

**Files:** as above.
**Interfaces:**

```rust
pub struct HnswIndex { /* M=16, ef_construction=64, ef_search=64 */ }
impl HnswIndex {
    pub fn insert(&mut self, id: u32, v: &[f64]);
    pub fn remove(&mut self, id: u32);
    pub fn search(&self, q: &[f64], k: usize) -> Vec<(u32, f64)>; // id, cosine
}
```

`candidate_spec_approx` for `VectorSimilar` → `CandidateSpec::Hnsw { field }` returning the `k = max(max_edges, 64)` nearest dst ids.

MCP:

```
find_similar arguments:
  key?: string          # existing: neighbors of key via edge_type
  vector?: number[]     # new: ANN query
  field?: string        # default "embedding"
  label?: string
  k?: number            # default 10
  min?: number          # default 0.8
  edge_type?: string    # default SIMILAR; ignored when vector is set
```

If `vector` is set, do **not** require existing edges.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn hnsw_recalls_near_duplicate() {
    // 200 random unit vecs dim 32; query = vec[7] + tiny noise; search k=1 returns 7
}

#[test]
fn find_similar_vector_without_edges() {
    // MCP or GraphDb API: insert 3 nodes with embeddings, no rule; query vector of node 0
    // returns node 0 (or skip self) and the closer of {1,2}
}
```

Add `GraphDb::find_similar_vector(&self, field, label, q: &[f64], k, min) -> Vec<(String, f64)>` so MCP stays thin.

- [ ] **Step 2: Run to fail** — no `hnsw.rs`.

- [ ] **Step 3: Implement** standard HNSW (Malkov & Yashunin). Seed RNG with FNV of rule name for deterministic rebuild (WAL replay identity). Persist levels + neighbors in snapshot.

Measure 5k/1536 recall in `dogfood/` or a criterion bench; fail CI if min < 0.90 on the **fixed seed** probe (not random each run).

- [ ] **Step 4: Tests + dogfood probe**

- [ ] **Step 5: Commit** `feat: in-tree HNSW approximate vectors; find_similar by vector`

---

### Task 2: 3-node rules

**Files:** `crates/core-rules/src/def.rs`, `engine.rs`, `crates/core-api/tests/rules.rs`

**Interfaces:** v1 form only:

```rust
pub struct RuleDef {
    // existing fields...
    /// Optional intermediate hop. When set, src matches `via_label` via
    /// `via_edge`, then the existing `predicate` is evaluated between
    /// **via node** and **dst** (not src and dst). Derived edge is still
    /// src → dst with `edge_type`.
    pub via_label: Option<String>,
    pub via_edge: Option<String>,
    pub via_dir: Option<core_storage::Direction>,
}
```

Appended fields, `#[serde(default)]` for JSON; bincode positional — pre-1.0 break for `CreateRule` bytes. Acceptable. Document: reopen from snapshot V7+ only, or bump WAL `CreateRule` payload version inside `def_bytes`.

Semantics: for each src, expand `via_edge` one hop to via-nodes with `via_label`; run `predicate` between via and dst (FieldEqual, Overlap, …); fire src→dst if any via satisfies. Score = max over via. Top-k still per src.

Example: Person -[:WORKS_AT]-> Org, Org.industry equals Project.industry → Person-[:FIT]->Project.

- [ ] **Step 1: Failing fixture test** with 2 people, 1 org, 2 projects; only the matching industry project gets FIT.

- [ ] **Step 2: Fail** — extra fields ignored / validate rejects.

- [ ] **Step 3: Implement.** `validate`: `via_label`, `via_edge` both Some or both None. Incremental: on src change, re-expand via; on via-node prop change, find srcs that hop to it (reverse via_edge) then recompute; on dst change, existing dst path.

- [ ] **Step 4:** oracle: extend `sim-harness/src/oracle.rs` to model via (or exclude via rules from the 2-node oracle and add a dedicated `via_oracle.rs` DST). **Must have an oracle.** Dedicated file is fine.

- [ ] **Step 5: Commit** `feat: 3-node via-hop linking rules`

---

### Task 3: `subscribe_query`

**Files:** `crates/core-api/src/subscription.rs`, `crates/server/src/subscribe.rs`, `crates/core-query`

**Interfaces:**

```rust
impl GraphDb<F> {
    pub fn subscribe_query(&self, cypher: &str) -> Result<Subscription> {
        // parse+plan; reject if plan is not in the allowlist:
        //   ScanLabel/ScanKey + optional Filter + Project
        //   ScanKey/ScanLabel + Expand + Project
        // Re-run the plan after each commit; diff rows by serialized key;
        // emit DbEvent::QueryRowAdded / QueryRowRemoved { columns, row }
    }
}
```

Not differential dataflow. Re-execute on commit with the 1M cap. Document: "full re-run per commit; use LIMIT."

WS: client sends `{"cypher":"MATCH (n:Person) RETURN n.id"}` in addition to `{"rules":[...]}`.

- [ ] **Step 1: Failing test** insert node → subscriber receives added row; delete → removed.

- [ ] **Step 2: Fail**

- [ ] **Step 3: Implement** allowlist in `plan.rs` (`fn is_subscribable(ops: &[PlanOp]) -> bool`). On each `distribute_events`, if any query subs, re-execute. Slow but correct. Phase 5 can replace with DD.

- [ ] **Step 4: tests in `crates/core-api/tests/events.rs` and `crates/server/tests/subscribe.rs`

- [ ] **Step 5: Commit** `feat: subscribe_query for allowlisted Cypher`

---

## G4 gate

- 5k/1536 ANN min recall ≥ 0.90 (recorded)
- MCP `find_similar` with `vector` works with zero derived edges
- via-hop fixture + oracle green
- `subscribe_query` round-trip test green
- `cargo test --workspace` green
