# Graph Algorithms

mushroomdb ships four built-in graph algorithms that run over the **unified topology** — manual edges you inserted plus derived edges the rule engine maintains. Running PageRank over your derived graph is the showcase: declare rules once, get a ranked graph for free.

## Algorithms

### PageRank

Ranks nodes by importance using the standard damped power-iteration model. Follows directed edges (or treats all edges as undirected) and distributes rank from dangling nodes uniformly.

```rust
let config = PageRankConfig {
    damping: 0.85,
    max_iters: 50,
    tol: 1e-6,
    edge_type: None,       // None = all edge types (manual + derived)
    direction: AlgoDir::Out,
    budget_ms: 5_000,
    weight_prop: None,      // Some("score") to weight mass by an edge property
    min_weight: None,       // drop edges below this resolved weight first
};
let report = db.pagerank(&config);
// report.scores: Vec<(String, f64)> sorted desc, key asc on ties
// report.converged: true if power iteration converged
```

**converged** is always honest: `false` when `max_iters` was reached or the time budget fired before convergence.

**weight_prop / min_weight:** when `weight_prop` is set, a node's out-mass distributes proportionally to that edge property instead of splitting evenly — an edge missing the property (or holding a non-numeric value) falls back to weight `1.0`. `min_weight` drops edges below the threshold before the algorithm runs, whether or not `weight_prop` is set. Both fields default to `None` (existing unweighted behavior, byte-identical).

### Weakly-Connected Components (WCC)

Finds groups of nodes that are reachable from each other when all edges are treated as undirected. Uses union-find with path compression (O(E α(n)) time).

```rust
let config = WccConfig {
    edge_type: None,   // None = all edge types
    budget_ms: 5_000,
    weight_prop: None,  // filter-only: WCC itself is unweighted
    min_weight: None,
};
let report = db.connected_components(&config);
// report.components: Vec<(key, component_id)>
// component_id = smallest key in the component (deterministic)
// sorted by (component_id, key)
```

**v1 note:** Direction is always undirected. All edges are treated symmetrically regardless of how they were inserted.

**weight_prop / min_weight:** WCC's connectivity itself doesn't use edge weight, but `min_weight` still filters — an edge whose resolved weight (via `weight_prop`, default `1.0` when unset or non-numeric) falls below the threshold is dropped before union-find runs, so a weak edge can stop connecting its endpoints.

### Degree Centrality

Counts edges per node (out-degree, in-degree, or both). Thin wrapper over the unified topology — fast, one-shot compute.

```rust
let config = DegreeConfig {
    edge_type: None,
    direction: AlgoDir::Both,  // Out | In | Both
    budget_ms: 5_000,
    weight_prop: None,          // filter-only: degree stays an unweighted count
    min_weight: None,
};
let report = db.degree_centrality(&config);
// report.scores: Vec<(key, u64)> sorted desc, key asc on ties
```

**weight_prop / min_weight:** same filter-only semantics as WCC — an edge below `min_weight` (resolved via `weight_prop`, default `1.0`) isn't counted; the degree itself stays an integer count, not a weighted sum.

### Louvain Communities

Detects communities by greedily maximizing modularity: local moving (each node joins whichever neighboring community raises modularity most) alternating with aggregation (communities collapse into super-nodes), repeated until stable.

```rust
let config = LouvainConfig {
    edge_types: vec![],       // empty = all edge types (union when multiple given)
    weight_prop: None,        // Some("score") to weight edges
    min_weight: None,         // drop edges below this resolved weight first
    resolution: 1.0,          // > 1.0 favors more, smaller communities; < 1.0 fewer, larger
    max_passes: 10,
    max_sweeps: 20,
    budget_ms: 5_000,
    node_label: None,         // restrict membership to one label
};
let report = db.communities(&config);
// report.communities: Vec<Community> sorted size desc, then smallest member key asc
// report.modularity: f64, resolution-adjusted
// report.truncated: true if the time budget fired
```

Each `Community` carries `id` (0-based, assigned by output order), `members` (sorted keys), `internal_weight` (total weight of edges with both endpoints inside, from the original filtered edges), and `cohesion` — `internal_weight / (internal_weight + weight of edges leaving the community)`, `1.0` for a community with no incident edges at all.

**Determinism:** local moving processes nodes in sorted key order at the base level, and that order propagates deterministically through every aggregated level; ties in the modularity-gain comparison keep the lower community id. The same store state always produces the same report, including after a snapshot reopen.

**Budget:** checked once per sweep (not per node). When it fires, `communities` returns the current partition with `truncated: true` — never an error.

**Weight / label restriction:** `edge_types` is a union filter (empty = all); a resolved weight below `min_weight` drops the edge before the algorithm runs, same rule as PageRank/WCC/degree. `node_label` restricts membership to one label — edges touching a node outside the label set are ignored entirely, not just the excluded node.

**Cohesion formula:** for community `C`, `cohesion = internal_weight(C) / (internal_weight(C) + leaving_weight(C))`, where `leaving_weight(C)` is the summed weight of edges with exactly one endpoint in `C`. Both are computed from the original (filtered) edges, not the internal aggregated levels Louvain builds while running.

## When to use degree_centrality vs. a Degree view

| Use case | Tool |
|---|---|
| Offline analysis: rank all nodes by degree once | `degree_centrality` |
| Write top-k scores to a property for querying | `degree_centrality` + `write_scores` |
| Live degree property always available in Cypher queries | Degree materialized view |
| Triggered on every edge insert/delete with no query overhead | Degree materialized view |

A **Degree materialized view** is maintained incrementally — every `insert_edge` / `delete_edge` updates just the affected node's count and stores it as a live property. Use it when you need `WHERE n.out_degree > 5` in a query with zero latency.

`degree_centrality` is a one-shot compute: it scans the whole graph on demand and is **not** persisted unless you call `write_scores`. Use it for batch ranking, one-time exports, or feeding `write_scores`.

## Write-back

```rust
let report = db.pagerank(&PageRankConfig::default());
// Persist the top-ranked scores as a node property.
db.write_scores("pagerank", &report.scores)?;
// Now every node has a "pagerank" property you can query:
// MATCH (n) RETURN n, n.pagerank ORDER BY n.pagerank DESC
```

`write_scores` goes through `write_batch` (crash-atomic, one WAL fsync). It refuses:
- A `prop_name` that is managed by an existing materialized view (`RuleInvalid`).
- A `prop_name` that is itself a view name (`RuleInvalid`).
- Any call on a read-only as-of instance (`ReadOnly`).
- Unknown node keys in the scores list (`KeyNotFound`).

Any rules or materialized views that watch the written property will fire immediately after the batch commits — this is a feature, not a side effect. For example, writing a `pagerank` score property and having a rule that links high-PR nodes will produce derived edges on the next access.

## HTTP API

All three algorithms are accessible via `POST /algo/{pagerank|wcc|degree}`:

```
POST /algo/pagerank
Content-Type: application/json

{
  "damping": 0.85,
  "max_iters": 50,
  "tol": 1e-6,
  "direction": "out",
  "budget_ms": 5000
}
```

```
POST /algo/wcc
Content-Type: application/json

{"edge_type": "FIT", "budget_ms": 5000}
```

```
POST /algo/degree
Content-Type: application/json

{"direction": "both", "budget_ms": 5000}
```

Each endpoint uses `spawn_blocking` with a read lock — reads don't block other reads; `budget_ms` bounds lock-hold time. The response carries `converged` (PageRank) or `truncated` (WCC, degree) flags honestly.

## CLI

```
mushroomdb algo pagerank ./db --top 20
mushroomdb algo wcc ./db --top 50
mushroomdb algo degree ./db --top 20
mushroomdb algo communities ./db --edge-type IMPORTS --edge-type CO_CHANGED --weight-prop score --min-weight 0.3 --top 10
```

`--top N` prints the N highest-ranked results (0 = all). Default is 20.

`algo communities` prints one line per community — id, size, cohesion, and the first 3 members — with `--edge-type` repeatable (empty = all edge types) and `--weight-prop`/`--min-weight` matching the config fields above. The header shows `modularity` and appends `(truncated)` when the time budget fired.

## As-of instances

Algorithms work on read-only as-of instances opened with `GraphDb::open_at`. The topology at the requested commit is used. `write_scores` returns `ReadOnly` on as-of instances.

```rust
let asof = GraphDb::open_at(&dir, 5)?;
let report = asof.pagerank(&PageRankConfig::default()); // works
asof.write_scores("rank", &report.scores)?;            // Err(ReadOnly)
```

## Edge-type filter

Pass `edge_type: Some("FIT".into())` in any config to restrict to one edge type. This is how you run PageRank over only the edges your linking rule created — the "derived graph showcase":

```rust
let config = PageRankConfig {
    edge_type: Some("FIT".into()),
    ..PageRankConfig::default()
};
let report = db.pagerank(&config);
```
