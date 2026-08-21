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

**Minimum hop count ≥ 1.**  Zero-length paths (`*0`, `*0..N`) are not
supported and are rejected at parse time with the error:

```
zero-length variable-length paths are not supported; minimum hop count is 1
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
- A minimum hop count greater than 1 (e.g., `*2..5`) is rejected at planning
  time with the error `"shortestPath does not support a minimum hop count"`.
  Use a plain variable-length pattern if you need a minimum.
- BFS stops at the first path found (shortest hop count).
- **Tie behavior:** When two or more paths reach the destination at the same
  BFS depth, exactly one row is returned.  The choice is arbitrary (internal
  storage order) and is not guaranteed to be stable across re-opens or inserts.
- The `*..max` range cap still applies (max ≤ 10).
- Returns zero rows when no path exists within the hop limit.

---

## WITH pipelines

`WITH` turns the executor into a multi-stage pipeline: each clause projects,
filters, sorts, and/or limits the working set before passing rows into the
next stage.

### Basic projection and aliasing

```cypher
MATCH (p:Person)
WITH p, p.city AS city
RETURN city
ORDER BY city
```

### Aggregation with HAVING (WHERE after WITH)

```cypher
MATCH (p:Person)
WITH p.city AS city, COUNT(*) AS cnt
WHERE cnt > 2
RETURN city, cnt
ORDER BY cnt DESC
```

The `WHERE` clause after an aggregating `WITH` acts as a HAVING filter —
evaluated against the group result, not the raw matched rows.

### ORDER BY and LIMIT inside WITH

```cypher
MATCH (p:Person)
WITH p, p.age AS age
ORDER BY age DESC
LIMIT 5
RETURN p.name AS name, age
```

### Chained stages with re-entry MATCH

```cypher
MATCH (p:Person)
WITH p.city AS city, COUNT(*) AS cnt
WHERE cnt > 1
MATCH (p2:Person)
WHERE p2.city = city
RETURN p2.name AS name, city
```

Each `WITH` stage produces a new working set.  A subsequent `MATCH` clause
joins against that set — use `WHERE` to correlate.

---

## UNWIND

`UNWIND` expands a list into one row per element.  Null or empty lists produce
zero rows.

```cypher
MATCH (p:Person)
UNWIND [1, 2, 3] AS x
RETURN p.name AS name, x
```

List source forms:
- Literal list: `UNWIND [1, 2, 'a'] AS x`
- Property containing a list value: `UNWIND p.tags AS tag`
- A named alias from a prior `WITH`: `UNWIND items AS item`

Passing a non-list value (e.g., a plain integer property) is a named error.
Intermediate rows produced by `UNWIND` count against the 1,000,000-row budget.

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

### Grouped aggregation

One or more non-aggregate RETURN items act as group keys; one or more
aggregate functions are computed per group:

```cypher
MATCH (n:Person)-[:KNOWS]->(m:Person)
RETURN n.city, COUNT(*) AS friends
ORDER BY friends DESC
LIMIT 5
```

- Multiple group keys and multiple aggregates are supported in the same
  RETURN clause.
- `NULL` group-key values group together (openCypher semantics).
- Empty input with no group-key items yields exactly one row (openCypher
  semantics: `COUNT=0`, other aggregates `null`).
- Group count is capped at 1,000,000 distinct keys per query.
- `ORDER BY` + `LIMIT` sort the finished group table (top-k groups), not
  the input rows; the LIMIT push-down optimisation is intentionally
  disabled for grouped queries.
- Multiple aggregates without any group-key items (e.g. `RETURN COUNT(*),
  SUM(r.score)`) is also supported and returns exactly one row.
- Numeric group-key values are compared as floats: `1` (integer) and `1.0`
  (float) land in the same group.  **Caveat:** integers whose absolute value
  exceeds 2^53 may be incorrectly unified due to float-representation
  rounding.

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

## Write statements

mushroomdb supports a Cypher write subset.  Every write statement flows through
the same mutation path as the Rust API — the rule engine fires incrementally and
the WAL commits with a single fsync before the response is returned.

Write results are always one row with three columns:

| Column | Meaning |
|---|---|
| `created` | nodes inserted |
| `properties_set` | individual `set_prop` calls applied |
| `deleted` | edges deleted |

### CREATE

```cypher
CREATE (n:Person {id: 'alice', score: 1})
```

- The `id` property is required and becomes the node key.
- Multiple nodes with an edge between them use chain syntax (comma-separated
  lists are not supported — attempting them returns a named parse error):
  ```cypher
  CREATE (a:Org {id: 'acme'})-[:MEMBER]->(b:Person {id: 'bob'})
  ```

### MATCH … SET

```cypher
MATCH (n:Person {id: 'alice'})
SET n.score = 99
```

- SET RHS must be a literal (integer, float, string, boolean); expression RHS is
  rejected with a named error in v1.
- Multiple SET clauses in one statement are allowed.
- Combined MATCH … SET … RETURN is not supported; use a separate MATCH … RETURN
  query after the write.
- When a SET touches a property that a rule evaluates, the rule engine
  re-evaluates incrementally: derived edges appear or retract within the same
  WAL frame.

### MATCH … DELETE (edge)

```cypher
MATCH (a:Org)-[r:MEMBER]->(b:Person)
DELETE r
```

- Only manual (user-inserted) edges may be deleted.
- Attempting to delete a derived (rule-owned) edge returns:
  `cannot delete derived edge; retract via the rule or change the property`

### MATCH … DETACH DELETE (node)

```cypher
MATCH (n:Person) WHERE n.id = 'alice'
DETACH DELETE n
```

Deletes the node and all incident edges (manual and derived). Derived edges
are retracted via the rule engine; top-k backfill fires automatically for any
source whose top-k slot was occupied by the removed node.

### MATCH … DELETE (isolated node)

```cypher
MATCH (n:Person) WHERE n.id = 'solo'
DELETE n
```

Bare `DELETE n` on a node variable succeeds when the node has no incident
edges (openCypher semantics).  If any edges remain the executor returns:
`Cannot delete node … because it still has incident edges. Use DETACH DELETE…`

### MERGE

```cypher
MERGE (n:Person {id: 'alice'})
```

- Match-or-create by a single key property only.
- ON CREATE SET / ON MATCH SET clauses are not supported.
- Multi-property maps are not supported.

---

## Limitations

| Feature | Status |
|---|---|
| Multi-statement transactions | Not supported (one write statement per query in v1) |
| Combined MATCH … SET … RETURN | Not supported; use two queries |
| SET RHS expressions | Literals only in v1 |
| MATCH … DETACH DELETE (node) | Supported — removes node + all edges |
| Bare DELETE on node with edges | Error — use DETACH DELETE |
| MERGE ON CREATE / ON MATCH | Not supported |
| Grouped aggregation | Supported (multiple keys and multiple aggregates allowed; group count capped at 1,000,000) |
| WITH pipeline stages | Supported — projection, aliasing, WHERE (HAVING), ORDER BY, LIMIT, and re-entry MATCH |
| UNWIND | Supported — list literals, list-valued properties, and scalar aliases from prior WITH; non-list → named error |
| Variable-length paths: max hops | Capped at 10 |
| shortestPath with unbound endpoints | Rejected at planning time |
| Intermediate result budget | 1,000,000 rows (WITH/UNWIND intermediate rows count against this cap) |
