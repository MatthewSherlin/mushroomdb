# Cypher Query Reference

mushroomdb supports a read-only Cypher subset for pattern matching, filtering,
aggregation, and ordering.  This page documents the full grammar, with a
dedicated section on variable-length paths and `shortestPath`.

---

## Basic patterns

```cypher
MATCH (n:Person)-[:KNOWS]->(m:Person)
WHERE n.name = 'Alice'
RETURN m.name
ORDER BY m.name
LIMIT 10
```

### Node patterns

```
(var:Label {key: 'value'})
```

- `var` — optional binding name
- `:Label` — optional label filter
- `{key: 'value'}` — zero or more property equality filters

### Relationship patterns

```
-[r:TYPE]->    right-directed
<-[r:TYPE]-    left-directed
-[r:TYPE]-     undirected (matches either direction)
```

The relationship variable (`r`) is optional.  Omitting the brackets entirely
(`-->`) is also accepted.

---

## Variable-length paths

Add `*min..max` inside the relationship brackets to match paths spanning
multiple hops:

```cypher
MATCH (a:N)-[r:T*1..5]->(b:N)
RETURN b
```

### Range syntax

| Syntax      | Meaning                           |
|-------------|-----------------------------------|
| `*`         | 1 to 10 hops (bare star default)  |
| `*n`        | Exactly n hops                    |
| `*min..max` | min to max hops (both inclusive)  |
| `*..max`    | 1 to max hops                     |

**Hard cap: max hops ≤ 10.**  Patterns with an unbounded upper end (`*min..`)
or with `max > 10` are rejected at parse time with the error:

```
variable-length paths are capped at 10 hops
```

### Semantics

- **BFS expansion** — each hop expands all neighbours of the current frontier.
- **Per-path edge uniqueness** — a relationship may not appear twice within the
  same path (Cypher relationship isomorphism).  This guarantees termination on
  cyclic graphs.
- **Intermediate-row budget** — if the output row count reaches 1,000,000 the
  query is aborted with an error naming the limit.  Use tighter ranges or a
  `WHERE` filter to stay within budget on dense graphs.

### Executor routing

Variable-length plans always use the *staged* execution path regardless of a
`LIMIT` clause.  `LIMIT` is applied after materialisation.

### Accessing path length

The relationship variable bound in a `*min..max` pattern exposes a virtual
`.length` field:

```cypher
MATCH (a:N)-[r:T*1..5]->(b:N)
RETURN r.length
```

No other fields on `r` are available for variable-length relationships.

---

## shortestPath

`shortestPath` finds the shortest directed path between two **already-bound**
nodes:

```cypher
MATCH (a:N {id: 'alice'}), (b:N {id: 'bob'})
MATCH shortestPath((a)-[r:T*..5]->(b))
RETURN r.length
```

- Both endpoints must be bound before the `shortestPath` clause.  Unbound
  forms are rejected at planning time.
- BFS stops at the first path found (shortest hop count).
- The `*..max` range cap still applies (max ≤ 10).
- Returns zero rows when no path exists within the hop limit.

---

## Aggregation

```cypher
MATCH (a:N)-[:T]->(b:N)
RETURN COUNT(*)

MATCH (a:N)-[r:RATED]->(b:N)
RETURN AVG(r.score)
```

Supported functions: `COUNT(*)`, `COUNT(var)`, `SUM(var.field)`,
`AVG(var.field)`, `MIN(var.field)`, `MAX(var.field)`.

Grouped aggregation (`RETURN a, COUNT(*)`) is not supported.

---

## ORDER BY / SKIP / LIMIT

```cypher
MATCH (n:Person)-[:KNOWS]->(m:Person)
RETURN m.name
ORDER BY m.name DESC
SKIP 10
LIMIT 5
```

---

## Limitations

| Feature | Status |
|---|---|
| Write clauses (CREATE / SET / DELETE) | Not supported |
| Multi-statement transactions | Not supported |
| Grouped aggregation | Not supported |
| Variable-length paths: max hops | Capped at 10 |
| shortestPath with unbound endpoints | Rejected at planning time |
| Intermediate result budget | 1,000,000 rows |
