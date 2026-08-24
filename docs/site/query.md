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

- SET RHS must be a literal (integer, float, string, boolean) or a `$param`
  reference; expression RHS is rejected with a named error.
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

## OPTIONAL MATCH

`OPTIONAL MATCH` applies left-outer-join semantics.  Rows that satisfy the
outer `MATCH` always appear in the result; if the optional pattern finds no
match, its bindings are `null`.

```cypher
MATCH (a:Person)
OPTIONAL MATCH (a)-[:KNOWS]->(b)
RETURN a, COUNT(b)
```

Edgeless nodes return `COUNT(b) = 0` — the outer row is never dropped.

### WHERE inside OPTIONAL MATCH

A `WHERE` clause inside an `OPTIONAL MATCH` filters the optional candidates,
not the outer row:

```cypher
MATCH (a:Person)
OPTIONAL MATCH (a)-[:KNOWS]->(b) WHERE b.score > 5
RETURN a, b
```

If no `b` passes the filter, `b` is `null` for that outer row.

### Chained OPTIONAL MATCHes

Multiple `OPTIONAL MATCH` clauses may follow one `MATCH`:

```cypher
MATCH (a:Person)
OPTIONAL MATCH (a)-[:FRIEND]->(b)
OPTIONAL MATCH (a)-[:COLLEAGUE]->(c)
RETURN a, b, c
```

Each clause is independent; a miss in one does not affect the others.

### Composes with aggregation

`OPTIONAL MATCH` composes correctly with grouped aggregation and WITH pipelines.

---

## Query parameters

Parameters substitute `$name` placeholders with values supplied at query time.
They are safe (the value is never interpreted as Cypher) and avoid string
interpolation in your application code.

### Rust API

```rust
// Convenience wrapper — takes a slice of (&str, Value) pairs.
let rs = db.query_with_params(
    "MATCH (n:Person) WHERE n.age > $min RETURN n",
    &[("min", Value::Int(18))],
)?;

// Low-level form — BTreeMap<String, Value>.
use std::collections::BTreeMap;
let mut params = BTreeMap::new();
params.insert("min".to_string(), Value::Int(18));
let rs = db.query("MATCH (n:Person) WHERE n.age > $min RETURN n", &params)?;
```

Parameters also work in `SET` clauses:

```cypher
MATCH (n:Person) WHERE n.age = 30 SET n.age = $newage
```

### HTTP API

```json
POST /query
{
  "cypher": "MATCH (n:Person) WHERE n.age > $min RETURN n",
  "params": { "min": 18 }
}
```

### Error: unknown parameter

If `$name` appears in the query but is not supplied, the executor returns:

```
missing parameter `name`
```

---

## Scalar functions

Scalar functions are available in `WHERE` expressions and `RETURN` / `WITH`
projections.  **Null propagation:** if any argument is `null`, the result is
`null` — except `coalesce`, which skips nulls.

| Function | Input | Output | Notes |
|---|---|---|---|
| `toLower(s)` | `String` | `String` | ASCII + Unicode lower-case |
| `toUpper(s)` | `String` | `String` | ASCII + Unicode upper-case |
| `size(x)` | `String` or `List` | `Int` | character count or element count |
| `coalesce(a, b, …)` | any | first non-null | never null unless all args are null |
| `type(r)` | `Rel` | `String` | relationship type label |
| `abs(n)` | `Int` or `Float` | same type | absolute value |
| `round(f)` | `Float` | `Float` | rounds to nearest integer as Float |

Calling an unknown function name returns:

```
unknown function `name`; supported: toLower, toUpper, size, coalesce, type, abs, round
```

### Examples

```cypher
MATCH (n:Person) RETURN toLower(n.name)
MATCH (n:Person) RETURN size(n.tags)
MATCH (n:Person) RETURN coalesce(n.nickname, n.name)
MATCH (a:Person)-[r]->(b:Person) RETURN type(r)
MATCH (n:Person) RETURN abs(n.score)
MATCH (n:Measurement) RETURN round(n.value)
```

---

## Cypher coverage

Tested against the current binary (2026-08-21, release build, maturin develop --release).
Classification: **Supported** = executes without error; **Named-error** = rejected with a
clear, actionable message; **Absent** = not implemented (not tested here).

### Supported

| Form | Example |
|---|---|
| `MATCH (n:Label)` | `MATCH (n:Person) RETURN n.name` |
| `MATCH (n {key: val})` inline property filter | `MATCH (n:Person {city: 'Austin'}) RETURN n` |
| `MATCH (n)-[r:TYPE]->(m)` directed edge | `MATCH (a)-[:KNOWS]->(b) RETURN a` |
| `MATCH (n)-[r:TYPE]-(m)` undirected edge | `MATCH (a)-[:KNOWS]-(b) RETURN a` |
| `MATCH (n)-[r]->(m)` any relationship type | `MATCH (n)-[r]->(m) RETURN type(r)` |
| `WHERE =, <>, <, >, <=, >=` | `WHERE n.age > 18` |
| `WHERE AND / OR` | `WHERE n.a = 1 AND n.b = 2` |
| `ORDER BY prop ASC / DESC` | `ORDER BY n.name DESC` |
| `SKIP n LIMIT n` | `SKIP 10 LIMIT 5` |
| `$param` in WHERE | `WHERE n.age > $min` |
| `$param` in SET | `SET n.score = $val` |
| `COUNT(*)`, `COUNT(var)`, `SUM`, `AVG`, `MIN`, `MAX` (single) | `RETURN COUNT(*), SUM(n.score)` |
| Grouped aggregation (single or multiple keys, multiple aggs) | `RETURN n.city, COUNT(*) AS c ORDER BY c DESC` |
| `WITH` projection + aliasing | `WITH n.name AS nm RETURN nm` |
| `WITH … WHERE` (HAVING) | `WITH city, COUNT(*) AS c WHERE c > 2 RETURN city` |
| `WITH … ORDER BY … LIMIT` | `WITH n, n.age AS a ORDER BY a DESC LIMIT 5 RETURN n.name` |
| `WITH … MATCH` re-entry | `WITH city MATCH (p:Person) WHERE p.city = city RETURN p` |
| `UNWIND list_literal AS x` | `UNWIND [1, 2, 3] AS x RETURN x` |
| `UNWIND n.prop AS x` (list-valued property) | `UNWIND n.tags AS tag RETURN tag` |
| `UNWIND alias AS x` (from prior WITH) | `WITH n.tags AS ts UNWIND ts AS t RETURN t` |
| `UNWIND … WHERE` (filter after UNWIND) | `UNWIND [1,2,3] AS x WHERE x > 1 RETURN x` |
| `OPTIONAL MATCH` (left-outer-join) | `MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN a, b` |
| `OPTIONAL MATCH … WHERE` (filter in optional scope) | `OPTIONAL MATCH (a)-[r]->(b) WHERE b.score > 5` |
| Multiple chained `OPTIONAL MATCH` | `OPTIONAL MATCH (a)-[:R]->(b) OPTIONAL MATCH (a)-[:S]->(c)` |
| `OPTIONAL MATCH` + aggregation | `OPTIONAL MATCH (a)-[r]->(b) RETURN a, COUNT(r)` |
| Variable-length `*1..n`, `*n`, `*..n`, bare `*` | `MATCH (a)-[r:T*1..5]->(b) RETURN r.length` |
| `shortestPath((a)-[r*..n]->(b))` | `MATCH (a {id: 'x'}), (b {id: 'y'}) MATCH shortestPath((a)-[r*..5]->(b)) RETURN r.length` |
| `CREATE (n:L {id: 'x', …})` | `CREATE (n:Person {id: 'alice', age: 30})` |
| `CREATE (a:L {id: 'x'})-[:T]->(b:L {id: 'y'})` | node-edge chain |
| `MATCH … SET n.prop = literal` | `MATCH (n) WHERE n.id = 'x' SET n.score = 99` |
| `MATCH … SET n.prop = $param` | `MATCH (n) WHERE n.id = $id SET n.score = $val` |
| `MATCH … DELETE r` (manual edge) | `MATCH (a)-[r:KNOWS]->(b) DELETE r` |
| `MATCH … DETACH DELETE n` | `MATCH (n) WHERE n.id = 'x' DETACH DELETE n` |
| `MATCH … DELETE n` (isolated node) | `MATCH (n:Tmp) WHERE n.id = 'x' DELETE n` |
| `MERGE (n:L {id: 'x'})` (single-key upsert) | `MERGE (n:Person {id: 'alice'})` |
| `MERGE (n:L {id: 'x'}) RETURN …` | `MERGE (n:Person {id: 'alice'}) RETURN n` — returns node whether created or matched |
| `CREATE … RETURN …` | `CREATE (n:Person {id: 'alice'}) RETURN n.id AS id` — single-statement create + projection |
| `WHERE … IS NULL / IS NOT NULL` | `WHERE n.score IS NULL`, `WHERE b IS NOT NULL` — null-check predicate; composes with AND/OR |
| Binary arithmetic (`+`, `-`, `*`, `/`) in RETURN, WHERE, SET, function args | `RETURN n.age + 1 AS next`, `WHERE n.score * 2 > 10`, `SET n.x = n.x + 1` — precedence: `*`/`/` over `+`/`-`; parentheses supported; null propagates; integer div by zero is a named error |
| `toLower`, `toUpper`, `size`, `coalesce`, `type`, `abs`, `round` | `RETURN abs(n.score), round(n.weight)` |
| View-maintained properties queryable like any property | `MATCH (c:City) WHERE c.pop > 1000 RETURN c.name` — `pop` is a degree view maintained incrementally; reads like a stored prop |
| `textMatches(n.field, "query")` in WHERE (per-row scratch scan) | `MATCH (a:Article) WHERE textMatches(a.bio, 'rust embedded') RETURN a.key` — correct for any graph size; O(scan) per row. Prefer `db.search()` for large indexed fields. |

### Named-error

Forms rejected with a clear, actionable error message (executor returns a typed error; no silent misbehavior).

| Form | Error message (excerpt) |
|---|---|
| `MERGE … ON CREATE SET / ON MATCH SET` | `ON CREATE SET / ON MATCH SET are not supported in MERGE (v1 limitation)` |
| `MERGE (n:L {id: 'x', extra: 2})` (multi-property) | `MERGE supports exactly one key property (got 2)` |
| `DELETE n` when node has incident edges | `Cannot delete node … because it still has incident edges. Use DETACH DELETE…` |
| `DELETE r` on derived (rule-owned) edge | `cannot delete derived edge; retract via the rule or change the property` |
| `MATCH … SET … RETURN` in one statement | `parse error: expected RETURN (found Set)` |
| `CREATE (a), (b)` comma-separated form | `parse error: unexpected tokens after CREATE pattern (found Comma)` |
| Variable-length `*0..n` (zero-length min) | `zero-length variable-length paths are not supported; minimum hop count is 1` |
| Variable-length `*n..` (unbounded upper) | `variable-length paths are capped at 10 hops` |
| `UNWIND` without preceding `MATCH` | `parse error: expected MATCH (found Unwind)` |
| `UNWIND scalar` (non-list value) | `execute: UNWIND requires a list; got … value for …` |
| Multi-statement / unknown top-level keyword | `parse error: expected MATCH (found …)` |
| `shortestPath` with unbound endpoints | `plan: shortestPath: source node … is not bound; bind both endpoints before shortestPath` |
| `shortestPath` with endpoints bound via comma-sep `MATCH (a), (b)` | `parse error: unexpected tokens after CREATE pattern (found Comma)` — comma-separated MATCH is not supported; use sequential `MATCH (a) MATCH (b)` forms |
| Unknown function name | `execute: unknown function …; supported: toLower, toUpper, size, coalesce, type, abs, round` |
| `$param` referenced but not supplied | `execute: missing parameter …` |
| `SET n.prop = n.other` (bare property-to-property copy) | `SET RHS: bare property/variable reference is not supported; use a literal, $parameter, or arithmetic expression` |
| Integer division by zero | `execute: division by zero` |
| Writing to a view-maintained property | `property is managed by view "…" and cannot be written directly` |
| Any write on an `open_at` (as-of) instance | `as-of instances are read-only` |

### Absent

Forms not implemented and not tested. Behavior is not guaranteed — these may produce an
unexpected-token parse error or other unspecified result.

| Form |
|---|
| `CASE WHEN … THEN … ELSE … END` expressions |
| Pattern comprehension `[(n)-[r]->(m) | m.prop]` |
| List comprehension `[x IN list WHERE …]` |
| `EXISTS { … }` subquery predicate |
| `CALL` procedure invocations |
| Schema operations (`CREATE INDEX`, `CREATE CONSTRAINT`) |

---

## Limitations

| Feature | Status |
|---|---|
| Multi-statement transactions | Not supported (one write statement per query in v1) |
| Combined MATCH … SET … RETURN | Not supported; use two queries |
| SET RHS expressions | Literals, `$param` references, and arithmetic expressions (`n.x + 1`, `n.score * 1.5`); bare property-to-property copy (`SET n.x = m.y`) is a named error |
| MATCH … DETACH DELETE (node) | Supported — removes node + all edges |
| Bare DELETE on node with edges | Error — use DETACH DELETE |
| MERGE ON CREATE / ON MATCH | Not supported |
| Grouped aggregation | Supported (multiple keys and multiple aggregates allowed; group count capped at 1,000,000) |
| WITH pipeline stages | Supported — projection, aliasing, WHERE (HAVING), ORDER BY, LIMIT, and re-entry MATCH |
| UNWIND | Supported — list literals, list-valued properties, and scalar aliases from prior WITH; non-list → named error |
| Variable-length paths: max hops | Capped at 10 |
| shortestPath with unbound endpoints | Rejected at planning time |
| Intermediate result budget | 1,000,000 rows (WITH/UNWIND intermediate rows count against this cap) |
