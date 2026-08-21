# Graph Algorithms

mushroomdb ships three built-in graph algorithms that run over the **unified topology** — manual edges you inserted plus derived edges the rule engine maintains. Running PageRank over your derived graph is the showcase: declare rules once, get a ranked graph for free.

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
};
let report = db.pagerank(&config);
// report.scores: Vec<(String, f64)> sorted desc, key asc on ties
// report.converged: true if power iteration converged
```

**converged** is always honest: `false` when `max_iters` was reached or the time budget fired before convergence.

### Weakly-Connected Components (WCC)

Finds groups of nodes that are reachable from each other when all edges are treated as undirected. Uses union-find with path compression (O(E α(n)) time).

```rust
let config = WccConfig {
    edge_type: None,   // None = all edge types
    budget_ms: 5_000,
};
let report = db.connected_components(&config);
// report.components: Vec<(key, component_id)>
// component_id = smallest key in the component (deterministic)
// sorted by (component_id, key)
```

**v1 note:** Direction is always undirected. All edges are treated symmetrically regardless of how they were inserted.

### Degree Centrality

Counts edges per node (out-degree, in-degree, or both). Thin wrapper over the unified topology — fast, one-shot compute.

```rust
let config = DegreeConfig {
    edge_type: None,
    direction: AlgoDir::Both,  // Out | In | Both
    budget_ms: 5_000,
};
let report = db.degree_centrality(&config);
// report.scores: Vec<(key, u64)> sorted desc, key asc on ties
```

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
```

`--top N` prints the N highest-ranked results (0 = all). Default is 20.

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
