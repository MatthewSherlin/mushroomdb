# Materialized Property Views

Materialized property views are per-node derived properties maintained
incrementally as the graph changes.  They behave exactly like user-written
node properties in queries (scan, filter, project, group-by) but are computed
automatically — no triggers, no manual refresh.

## Concepts

A **view** binds a synthetic property name (`view_prop`) to a computation
over a node's neighbors:

| Source type | Computes |
|---|---|
| `Degree` | Count of edges of a given type in a given direction |
| `NeighborAgg` / Sum | Sum of a neighbor property |
| `NeighborAgg` / Avg | Average of a neighbor property |
| `NeighborAgg` / Min | Minimum of a neighbor property |
| `NeighborAgg` / Max | Maximum of a neighbor property |
| `NeighborAgg` / Count | Count of neighbors that have the property |

`NeighborAgg` Count skips neighbors missing the named property (`props.get`
is `None`). It is not Degree: an incident edge whose endpoint has no value
for `prop` does not increment Count. Degree still counts every incident
edge of that type and direction.

Views are label-scoped: only nodes with the declared `label` carry the
synthetic property.  Views cover both user-inserted edges and rule-derived
edges.

## Quick Start

```rust
use core_api::{AggFn, Direction, GraphDb, Value, ViewDef, ViewSource};

let mut db = GraphDb::open(&dir)?;

// Track how many people live in each city.
db.create_view(ViewDef {
    name: "city_population".into(),
    label: "City".into(),
    view_prop: "pop".into(),
    source: ViewSource::Degree {
        edge_type: "LIVES_IN".into(),
        direction: Direction::In,
    },
})?;

// Average score of residents.
db.create_view(ViewDef {
    name: "city_avg_score".into(),
    label: "City".into(),
    view_prop: "avg_score".into(),
    source: ViewSource::NeighborAgg {
        edge_type: "LIVES_IN".into(),
        direction: Direction::In,
        agg: AggFn::Avg,
        prop: "score".into(),
    },
})?;

// Read like any property.
let pop = db.get_prop("london", "pop");

// Query using view props in Cypher.
let results = db.query(
    "MATCH (c:City) WHERE c.pop >= 1000 RETURN c.name, c.avg_score",
    &Default::default()
)?;
```

## API

### `create_view(def: ViewDef) -> Result<()>`

Register a new view.  Backfills values for all existing nodes with the
matching label.  Fails if:
- A view with the same name already exists.
- `view_prop` is already owned by another view.
- `view_prop` collides with an existing real node property.

### `delete_view(name: &str) -> Result<()>`

Remove a view and delete its values from all nodes.  Fails if the view
does not exist.

### `views() -> Vec<ViewDef>`

Snapshot of all registered view definitions.

## Incremental Maintenance

| Event | Views updated |
|---|---|
| Edge inserted | Degree / NeighborAgg for src and/or dst |
| Edge deleted | Same |
| Derived edge fired (rule) | Same |
| Derived edge retracted (rule) | Same |
| Node property changed | NeighborAgg views that read that property |
| Node deleted | Neighbor views decremented; own values removed |
| `create_view` | Backfill all matching nodes |
| `delete_view` | Values removed from all matching nodes |

### MIN / MAX retraction cost

When a Degree or NeighborAgg Min/Max view loses the edge whose endpoint
held the current extreme value, a full scan over remaining neighbors is
required (O(degree)).  No auxiliary sorted structure is maintained in v1.
For workloads where this is a bottleneck, use Sum + Count and compute Avg
/ Min / Max in the query layer, or maintain a heap externally.

## Write Guard

Writing directly to a view-managed property name returns
`GraphError::ViewPropReadOnly { view_name }`:

```rust
// Assuming "pop" is managed by "city_population":
let err = db.set_prop("london", "pop", Value::Int(999));
// err == Err(ViewPropReadOnly { view_name: "city_population" })
```

## Breaking Change: Format Version 5

> **Pre-alpha notice.** Snapshot format version 5 (introduced with materialized
> views) is incompatible with version 4.  A binary built from this commit
> refuses to open a V4 snapshot with a clear error:
> `snapshot: unsupported version 4 — V4 snapshot is no longer supported; re-snapshot with a V5 binary`.
>
> **Rebuild procedure:** delete `snapshot.bin` and re-open the database; the
> WAL is replayed from scratch and a fresh V5 snapshot is written on the next
> `snapshot()` call.

## Persistence

View **definitions** are WAL-persisted as `CreateView` / `DeleteView`
records (discriminants 10 / 11).  They also survive snapshots via a
`view_defs` field in the snapshot state (format version 5).

View **values** are NOT stored in snapshots — they are recomputed from
topo + props on every open.  This matches the rebuild-on-open pattern
used by candidate indexes and keeps snapshot writes simple.

> **Disk-space note.** Snapshots store the raw edge list including all derived
> edges.  At 100k nodes with a dense rule (10.5M derived edges) a V5 snapshot
> is approximately 2.2 GiB on disk.  Plan accordingly if you call `snapshot()`
> on large graphs with high-fanout rules.

## Subscriptions

View-value updates do not emit subscription events in v1.  Only
user-write WAL records and rule fire/retract deltas generate `DbEvent`s.

## DST Oracle

`GraphDb::scratch_view_value(key, view_name)` recomputes a view value
from scratch (O(degree)) without reading the stored column.  Use it to
verify correctness:

```rust
let live = db.get_prop("london", "pop");
let scratch = db.scratch_view_value("london", "city_population");
assert_eq!(live, scratch.as_ref());
```
