# Linking rules

A linking rule is a schema object that declares when mushroomdb should create
an edge between two nodes. Once declared, the rule fires automatically on every
`insert_node` and `set_prop` call — incrementally, not in batch.

Derived edges carry rule provenance (visible via `explain` and the UI why
panel) and are retracted automatically when the properties that matched them
change.

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
    pub approximate: bool,     // IVF-Flat approximate mode (VectorSimilar only)
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

**Score:** none (the `weight_prop` field is ignored; explain shows `weight=none`).

---

### 2. FieldEqual

Matches when a named field on the source node exactly equals the same
field on the destination node. Comparison is on any scalar `ValueKey`
(string, int, float, bool), not strings only.

```rust
Predicate::FieldEqual { field: "industry".into() }
```

**Score:** 1.0 when the fields match; the edge is not written when they differ.

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
path from a full scan to IVF-Flat (Inverted File Index):

```rust
RuleDef {
    predicate: Predicate::VectorSimilar { field: "embedding".into(), min: 0.85, dims: 1536 },
    approximate: true,
    max_edges: Some(1000),
    ..
}
```

**How it works:** at rule creation and rebuild time, k-means is fit over
the destination-side vectors (k = ceil(√n), clamped to [4, 1024], 12
iterations, seed derived from the rule name). On each lookup, P =
max(1, ⌈k/16⌉) nearest centroids are probed. Centroid assignments are
updated on insert; drift accumulates until the next `rebuild` re-fits.

**Trade-offs:**

| Property | Exact (`approximate: false`) | Approximate (`approximate: true`) |
|---|---|---|
| Candidates | All vectors with the right label | Members of P nearest clusters |
| Per-query edge recall | 1.00 (exact) | ≥ 0.90 quiesced; ≥ 0.85 post-rebuild |
| Determinism | Yes | Yes — same rule + data → same clusters |
| WAL replay | Identical | Identical (clusters re-fit on replay) |

Measured at 5k nodes, dim 1536: exact ~12 min backfill, approximate
~17 s. Per-query recall measured at 0.991 (uncapped 5k probe).

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
   "src": "person-01", "dst": "proj-01", "weight": null},
  {"rule": "skill_fit", "edge_type": "FIT",
   "src": "person-01", "dst": "proj-01", "weight": 1.0}
]
```

The UI why panel renders this as human-readable arithmetic for each
predicate kind.

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
  8.25 min; IVF-Flat re-derivation dominates. V5 snapshots persist derived edges and IVF centroids —
  `open_with` from a V5 snapshot takes 8.7 s at 100k (snapshot write cost: 25 s). Call `snapshot()`
  before close to avoid re-derivation on next open.
