# Linking rules

A linking rule is a schema object that declares when mushroomdb should create
an edge between two nodes. Once declared, the rule fires automatically on every
`insert_node` and `set_prop` call — incrementally, not in batch.

Derived edges carry rule provenance (visible via `explain` and the UI why
panel) and are retracted automatically when the properties that matched them
change.

![Rule fire, retraction, and explain arithmetic on the demo store](../assets/rule-fire-explain.gif)

Reproduce the GIF above with `vhs scripts/rule-fire-explain.tape` — it shows
the FIT edges before and after a `SET` that retracts one and fires another,
then calls `explain` on the edge that survives.

---

## Declaring a rule

Rules are declared with `GraphDb::create_rule` (Rust), the `POST /rules`
endpoint (HTTP), the MCP `create_rule` tool, or `db.create_rule()` (Python
bindings). The schema is `RuleDef`:

```rust
pub struct RuleDef {
    pub name: String,          // unique rule identifier
    pub src_label: String,     // source node label
    pub dst_label: String,     // destination node label
    pub predicate: Predicate,  // one of the six kinds below
    pub edge_type: String,     // edge type written to the graph
    pub weight_prop: Option<String>, // if Some, score stored as this edge prop
    pub max_edges: Option<usize>,    // cap on derived edges per rule (recommended)
    pub approximate: bool,     // HNSW approximate mode (VectorSimilar only)
}
```

All label, edge-type, and field names are arbitrary user strings. The engine
has no reserved names.

---

## Six predicate kinds

### 1. KeyMatch

Matches when a field on the source node equals the destination node's key.
This is the FK-style predicate, and it also runs automatically on `*_id`
fields via auto-FK inference at ingest time.

```rust
Predicate::KeyMatch { field: "org_id".into() }
```

**Demo example:** `auto_fk_person_org_id` — every Person whose `org_id`
field matches an Org's key gets a `ORG` edge to that Org. After the demo,
30 such edges exist (one per Person).

**Score:** 1.0 when the field matches the destination key.

---

### 2. FieldEqual

Matches when a named field on the source node exactly equals the same
field on the destination node. Comparison is on any scalar `ValueKey`
(string, int, float, bool), not strings only.

```rust
Predicate::FieldEqual { field: "industry".into() }
```

**Score:** 1.0 when the fields match; the edge is not written when they differ.

Scores are stored on the edge under `weight_prop` (MCP default `weight`);
`explain` reports the score even for rules that store none. Via-hop rules are
the exception: they score over the via set, so `explain` reports their stored
score only.

For a rule declared with no `weight_prop` at all (only reachable via the Rust
API — the MCP `create_rule` tool always defaults it to `"weight"`), `explain`
still reports the recomputed score (1.0 for `KeyMatch`/`FieldEqual`), but the
`EdgeFired` subscription event's `weight` field (`Option<f64>`) is absent
(omitted) from the JSON payload — because it only looks up the stored prop,
not the predicate.

**Example:** two Talent nodes both with `industry = "architecture"` get a
`INDUSTRY_ALIGNMENT` edge between them (score 1.0).

---

### 3. Overlap

Computes the Jaccard coefficient between two list-valued fields and writes
an edge when the coefficient meets a minimum threshold.

```rust
Predicate::Overlap { field: "skills".into(), min: 0.5 }
```

**Score:** Jaccard value, stored on the edge via `weight_prop`.

**Demo example:** `skill_fit` — Person.skills vs Project.skills, min 0.5.
After the demo:

```text
  p=person-01  proj=proj-01  score=1.0   (full overlap)
  p=person-01  proj=proj-02  score=0.5   (partial)
  p=person-01  proj=proj-20  score=0.5   (partial)
```

90 FIT edges total across 30 People and 20 Projects.

---

### 4. NumericWithin

Matches when the absolute difference between a numeric field on the source
and the same field on the destination is within a tolerance. Score is
`1 - |Δ| / tolerance`, so exact matches score 1.0 and the boundary scores 0.0.

```rust
Predicate::NumericWithin { field: "founded_year".into(), tolerance: 2.0 }
```

**Demo example:** `founded_within` — Org.founded_year, tolerance 2. An Org
founded in 2010 and one in 2011 score `1 - 1/2 = 0.5`. 34 FOUNDED_WITHIN
edges in the demo dataset.

---

### 5. GeoRadius

Matches when the Haversine distance between two `[lat, lon]` fields is
within a radius (in km). Score is `1 - distance / radius`.

```rust
Predicate::GeoRadius { field: "office".into(), km: 50.0 }
```

**Demo example:** `nearby_office` — Org.office, 50 km. Uses real city
coordinates. 16 NEARBY edges in the demo dataset.

**Field format:** the field must contain a two-element numeric list
`[latitude, longitude]` in decimal degrees.

---

### 6. VectorSimilar

Computes cosine similarity between two fixed-dimension float array fields
and writes an edge when similarity meets a minimum threshold.

```rust
Predicate::VectorSimilar { field: "embedding".into(), min: 0.8 }
```

**Score:** cosine similarity, stored on the edge via `weight_prop`.

**Demo example:** `similar_interests` — Person.embedding, dim 8, min 0.8.
114 edges in the demo dataset.

**Scale note:** the exact (full-scan) path is O(n²) in the number of
nodes carrying the field. At 5k nodes and dim 1536 the exact backfill
takes about 12 minutes. Use the approximate mode below for large vector
sets.

---

## Composing predicates — All and Any

### All — require every branch

Require multiple conditions on the same edge by wrapping predicates in
`All`:

```rust
Predicate::All(vec![
    Predicate::FieldEqual { field: "region".into() },
    Predicate::Overlap { field: "tags".into(), min: 0.3 },
])
```

The edge is written only when every sub-predicate fires. The score is the
**minimum** of the individual sub-predicate scores (`score.min(part_score)`
per part — verified in `crates/core-rules/src/def.rs`, test
`all_takes_min_score_and_requires_every_part`).

### Any — require at least one branch

Match when at least one branch fires by wrapping predicates in `Any`:

```rust
Predicate::Any(vec![
    Predicate::Overlap { field: "tags".into(), min: 0.3 },
    Predicate::NumericWithin { field: "founded_year".into(), tolerance: 2.0 },
])
```

The edge is written when **any** sub-predicate fires. The score is the
**maximum** of the satisfied branches' scores (`score.max(branch_score)` —
verified in `crates/core-rules/src/def.rs`, test
`any_score_is_max_when_both_branches_match`).

### Nesting

`All` and `Any` nest freely:

```rust
Predicate::All(vec![
    Predicate::FieldEqual { field: "region".into() },
    Predicate::Any(vec![
        Predicate::Overlap { field: "skills".into(), min: 0.3 },
        Predicate::NumericWithin { field: "founded_year".into(), tolerance: 2.0 },
    ]),
])
```

The combined score follows the two conventions above: `All` takes the
**minimum** over its branches' scores; `Any` takes the **maximum**. In the
example above the combined score is `min(1.0, max(overlap_score,
numeric_score))`.

**Depth cap:** nesting depth is capped at 4. `validate()` returns a named
error (`"predicate nesting depth N exceeds cap of 4"`) when the limit is
exceeded.

---

## Approximate mode (VectorSimilar only)

Setting `approximate: true` on a VectorSimilar rule switches the candidate
path from a full scan to in-tree HNSW (Hierarchical Navigable Small World):

```rust
RuleDef {
    predicate: Predicate::VectorSimilar { field: "embedding".into(), min: 0.85, dims: 1536 },
    approximate: true,
    max_edges: Some(1000),
    ..
}
```

**How it works:** at rule creation time, an HNSW graph is built over the
destination-side vectors (cosine similarity, in-tree implementation — no
external ANN dependency). Each node insertion incrementally updates the HNSW
graph. On each lookup, the HNSW index returns the approximate k nearest
neighbors. IVF-Flat centroids are also maintained as a fallback; the primary
candidate path is HNSW. The HNSW graph is persisted in V7 snapshots alongside
IVF centroids so WAL replay and snapshot open avoid full re-fitting.

**Trade-offs:**

| Property | Exact (`approximate: false`) | Approximate (`approximate: true`) |
|---|---|---|
| Candidates | All vectors with the right label | HNSW approximate k-NN |
| Per-query edge recall | 1.00 (exact) | min 0.90, mean 0.998 (5k/dim 1536, fixed-seed probe) |
| Determinism | Yes | Yes — same rule + data → same graph |
| WAL replay | Identical | Identical (HNSW rebuilt on replay) |

Measured at 5k nodes, dim 1536: exact ~12 min backfill. Approximate
backfill time is substantially faster — the IVF-Flat-era measurement was
~17 s; HNSW timing at this scale is not separately published (flag: unsure).

Use approximate mode when backfill latency matters more than perfect
recall. Do not use it when completeness is required (safety-critical
graph closure, compliance checks).

The `explain` endpoint marks approximate edges with `"approximate": true`
in the predicate summary.

---

## Provenance and explain

Every derived edge records which rule produced it. Query it via:

```text
GET /explain?a=person-01&b=proj-01
```

Response:

```json
[
  {"rule": "auto_fk_person_project_id", "edge_type": "PROJECT",
   "src": "person-01", "dst": "proj-01", "weight": 1.0},
  {"rule": "skill_fit", "edge_type": "FIT",
   "src": "person-01", "dst": "proj-01", "weight": 1.0}
]
```

The UI why panel renders this as human-readable arithmetic for each
predicate kind.

Each entry also carries `via_edge`: the edge type a via-hop rule hops over,
or `null` for a plain two-node rule.

---

## Chaining

A via-hop rule reads edges. Those edges can themselves be rule-derived, so
one write cascades: setting `File.top_author_id` refires the FK rule that
owns `TOP_AUTHOR`, and the new `TOP_AUTHOR` edge immediately refires every
via-hop rule that hops over `TOP_AUTHOR` — `KNOWS`, say. All of it lands in
the same commit. Retraction chains the same way: an edge that disappears
takes the edges derived from it with it — including when the edge disappears
because you deleted the rule that owned it, or removed one of its endpoints.

The rules of the cascade:

- **Depth 4.** A write chains at most four levels past the rule that fired
  first. A longer chain stops there, leaving the levels beyond it at their
  previous values, and no later write repairs them — a write that touches the
  root chains four levels again and never reaches the fifth. The cap is
  `MAX_CHAIN_DEPTH`, exported from the crate root. When a write hits it with
  work still pending, `stats().chain_truncations` goes up; a non-zero value
  there means some derived edges are stale and the rule chain needs
  shortening or splitting. The counter is not persisted, so it starts at zero
  on every open and is re-accumulated by replay.
- **Once per rule and source, per level.** Within one chain level each
  `(rule, source node)` pair is recomputed at most once. Every edge a level
  consumes was already written before that level began, and a recompute is a
  full re-evaluation of the source's desired edge set rather than a patch, so
  a second pass at the same level could only repeat itself. Across levels the
  guard is released: a rule that ran at one level still refires at the next if
  another rule wrote an edge it hops over. Each such chained recompute of a
  via-hop rule increments that rule's `stats().rules[].fires` counter the same
  as a top-level fire, so a rule that sits deep in a chain reports more fires
  than the number of writes that triggered it.
- **Deterministic.** Derived edges are consumed in the order they were
  written and rules are visited in name order, so a chain produces the same
  result every time. WAL replay runs the identical hooks, so a reopened store
  reproduces the identical chained edges — and the identical truncations.
- **Nothing derives onto a node being deleted.** A delete retracts that
  node's derived edges and chains those retractions, but the node is excluded
  from every role a rule could put it in while the delete is in flight, so the
  chain cannot hand it a new edge on the way out.
- **No cycles.** See below.

### Cycles are rejected at rule creation

A rule set where `A` hops over an edge `B` writes, and `B` hops over an edge
`A` writes, would refire itself forever; the depth cap would silently cut it
short at an arbitrary point. So `create_rule` rejects it outright:

```text
RuleInvalid: rule chain cycle: KNOWS -> TOP_AUTHOR -> KNOWS
```

The path names the edge types around the loop. A rule whose `via_edge`
equals its own `edge_type` is rejected the same way. Rules created earlier in
the same batch count too, so a batch cannot assemble a cycle one rule at a
time and have both halves accepted.

Views are not part of this. A rule cannot read a view, so a view computed
from derived edges never feeds another rule and cannot close a cycle.
View-fed rules remain designed, not shipped.

---

## Backfill and max_edges

When a rule is declared on a graph that already has nodes, mushroomdb
backfills — it evaluates the predicate against all existing node pairs.
For rules without a `max_edges` cap this is O(|src_nodes| × |dst_nodes|)
pair evaluations.

### Top-k per-source semantics (`max_edges: Some(k)`)

Setting `max_edges: Some(k)` gives the rule **per-source top-k semantics**:
each source node derives at most *k* outgoing edges, keeping only the
best-scoring destinations:

- Scored predicates (`NumericWithin`, `VectorSimilar`): destinations ranked
  by score DESC, then destination key ASC as a tiebreak.
- Unscored predicates (`KeyMatch`, `FieldEqual`): destinations ranked by
  destination key ASC.

When a new candidate beats the current k-th score it is inserted and the
weakest existing edge is evicted. When a destination node is removed,
affected sources automatically backfill from the next-best candidate.

**Always set `max_edges` on high-fanout rules at large scale.** A
`FieldEqual` rule on 70k Talent × 20k Company nodes with `max_edges: Some(5)`
produces at most 5 edges per source — evaluation stays linear in |src_nodes|.

After a rule is declared, incremental firing on each new write is cheap
(sub-millisecond for most predicates) because only the changed node's
field wakes the rule.

> **Ceiling note:** `max_edges: Some(k)` is the *only* ceiling for per-source
> rules — there is no additional defensive cap. A rule with `k = 1_000_000`
> on a graph with 100k source nodes can create up to 10¹¹ edges during
> backfill. Choose k relative to the expected candidate-set size per source.

### Global-budget semantics (`max_edges: None`)

When `max_edges` is `None`, the rule uses a global runaway guard:
materialization stops after `DEFAULT_MAX_EDGES` (1 000 000) total edges and
the rule is marked `tripped`. No further edges are added until the rule is
rebuilt with nodes removed. Use `max_edges: Some(k)` instead when you want
per-source cardinality control.

> **Breaking change (pre-alpha, since 2026-08-20):** Prior to this release,
> `max_edges: Some(k)` meant "stop at k total edges globally (global-budget
> semantics, same as `None` but with a lower cap)." It now means "keep the
> k best-matching destinations per source node (per-source top-k semantics)."
> Any snapshot written before this change that contains a rule with
> `max_edges: Some(k)` will silently switch to per-source top-k semantics
> on the next open — which may cause up to `src_count × k` edges to be added
> on the next write where previously the rule was frozen. The `tripped` flag
> loaded from old snapshots is ignored by the new code path.
> No migration is provided (pre-alpha policy).

---

## Known limitations

- Two-hop Cypher joins over dense derived-edge sets hit a 1M-row
  intermediate-result cap before `LIMIT` is applied. Use the traversal
  API (`node_edges` / `neighborhood`) for multi-hop lookups on large
  graphs. LIMIT pushdown is on the roadmap.
- WAL-only cold-start re-fires all rules from node data. At 100k nodes (9 rules), re-open takes
  ~8.16 min; ANN index (HNSW) re-fitting dominates. V5/V6 snapshots persist derived edges and IVF
  centroids; V7 snapshots additionally persist HNSW blobs — `open_with` from a V6 snapshot takes
  8.88 s at 100k (V6 snapshot write cost: 22.5 s; V7 numbers not yet separately published). Call
  `snapshot()` before close to avoid re-derivation on next open.
