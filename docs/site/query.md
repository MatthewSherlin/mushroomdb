# Cypher Query Reference

mushroomdb supports a Cypher subset for pattern matching, filtering,
aggregation, ordering, and writes (`CREATE`, `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`,
`MERGE`). This page documents the full grammar, with a dedicated section on
variable-length paths and `shortestPath`.

---

## Key idiom — how to filter and read node keys

The node's key lives in the id map, not as a stored property. These spellings
read it (and the label) anywhere a scalar does — `WHERE`, `ORDER BY`, `RETURN`,
an inline pattern filter, and after a grouping `WITH`:

| Spelling | Gives |
|---|---|
| `n.key` / `n.id` | the node's key (a stored property of that name wins) |
| `n.label` | the node's label (stored `label` wins) |
| `key(n)` / `id(n)` | always the id-map key; `id(n)` aliases `key(n)` |
| `labels(n)` | a one-element list holding the label |

`n.key`, `n.id`, and `n.label` are a **fallback**: a stored property of that
name always wins, so a graph that really has an `id` or `key` property keeps
reading it. `key(n)` / `id(n)` always return the id-map key (`id(n)` is the
function alias of `key(n)`: one argument, same errors on a non-node).

`MATCH (n {id: 'alice'})` is **not** the property: it is a key selector
(`ScanKey`) on MATCH/MERGE/CREATE. CREATE may still stamp a stored `id`
property; that node then reads `n.id` from the store, which equals the key.

```cypher
-- filter by key
MATCH (n:Person) WHERE n.id = 'alice' RETURN n.id, n.name
MATCH (n:Person) WHERE n.key = 'alice' RETURN n.key, n.name
MATCH (n:Person {id: 'alice'}) RETURN n.name
MATCH (n:Person) WHERE n = 'alice' RETURN n        -- the older spelling, still correct

-- read key and label
MATCH (n:Person) RETURN n.id, id(n), n.key, n.label AS kind ORDER BY n.key

-- and after a grouping WITH, which used to flatten `c` to a scalar
MATCH (t:Talent)-[:FIT]->(c:Company)
WITH c, count(DISTINCT t) AS n WHERE n >= 2
RETURN key(c), n ORDER BY n DESC
```

A bare `RETURN n` also works: the result row value for a node variable is the
key string, not a node object. In Python, `row["n"]` is `"alice"`, not a dict.

**Performance note.** `WHERE n.key` / `n.id` / `key(n)` / `id(n)` against a
literal or `$param` plans as `ScanKey` (plus a stored-property filter so
stored-wins still holds), not a label scan. `n.label` / `labels(n)` do not:
those still scan the label. Pattern `{id: 'x'}` remains a key lookup.

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
- `{key: 'value'}` — zero or more property equality filters. `{id: 'alice'}` is
  a key lookup (`ScanKey`). The names `key` and `label` fall back to the node's
  own key and label when no stored property of that name exists (see Key idiom
  above)

### Relationship patterns

```
-[r:TYPE]->    right-directed
<-[r:TYPE]-    left-directed
-[r:TYPE]-     undirected (matches either direction)
```

The relationship variable (`r`) is optional.  Omitting the brackets entirely
(`-->`) is also accepted.

### Multiple patterns in one MATCH

Patterns separated by commas inside a single `MATCH` are equivalent to the same
patterns as consecutive `MATCH` clauses — they plan and execute identically.
Sharing a variable between them is a join; sharing none is a cartesian product,
which is the openCypher meaning.

```cypher
-- the intersection: a talent joined to a company by all three types
MATCH (t:Talent)-[:INDUSTRY_ALIGNMENT]->(c:Company),
      (t)-[:SPECIALTY_MATCH]->(c),
      (t)-[:LOCATION_FIT]->(c)
WHERE t.status = 'published'
WITH c, count(DISTINCT t) AS n WHERE n >= 14
RETURN key(c), n ORDER BY n DESC
```

`count(DISTINCT …)` is what makes that correct: three edge types between the
same pair yield three rows per talent under an alternation, and only `DISTINCT`
counts the talent once. A company joined by only two of the three types does not
appear at all.

Comma-separated patterns after `CREATE` are still a named parse error.

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
- **Relationship uniqueness** — within one `MATCH` clause no relationship is
  bound twice (Cypher relationship isomorphism): not twice in one walk, not by
  two hops of a pattern, fixed or variable-length, and not by two
  comma-separated parts of the clause. So a named path never lists a
  relationship twice, and `MATCH (a)-[r]->(b), (c)-[s]->(d)` never binds `r`
  and `s` to the same relationship. Separate clauses do not share the rule:
  `MATCH (a)-[r]->(b) MATCH (c)-[s]->(d)` may bind both to one relationship,
  and so may an `OPTIONAL MATCH` that follows a `MATCH`. A relationship
  variable an earlier clause bound (`MATCH (x)-[r]->(y) MATCH (a)-[r]->(b)`)
  matches that same relationship again; naming one relationship variable for
  two hops of one `MATCH` (`MATCH (a)-[r]->(b)-[r]->(c)`) is the error
  ``relationship variable `r` is used for more than one relationship in the
  same MATCH``. A write driven by the
  match (`MATCH … SET`, `MATCH … DETACH DELETE`) sees the same rows. An
  undirected self-loop is one relationship, walked once. This guarantees
  termination on cyclic graphs.
- **The end node is filtered, the nodes in between are not** — the
  destination's label and inline properties (`(b:Person {active: true})`,
  `{id: $id}`) apply to the node each path ends at.  A path may pass through
  nodes that fail them.  This holds in every hop form, for a hop in the middle
  of a pattern, in `OPTIONAL MATCH`, and with a named path.
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
MATCH (a:N {id: 'alice'}) MATCH (b:N {id: 'bob'})
MATCH shortestPath((a)-[r:T*..5]->(b))
RETURN r.length
```

Bind the two endpoints first: with earlier `MATCH` clauses, as above, or with
an earlier comma-separated part of the same `MATCH`
(`MATCH (a)-[r]->(b), p = shortestPath((a)-[*..5]-(b))`). In the same `MATCH`
the search never uses a relationship the other parts bound, so there it finds
the shortest path that avoids `r`.

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

## Named paths

Bind a whole pattern to a name with `p = …` and return it, or read it with
`.length`, `.nodes` and `.relationships`:

```cypher
MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person)
RETURN p
```

```cypher
MATCH p = (a:Person {id: 'ada'})-[:KNOWS]->(b)-[:WORKS_AT]->(c:Company)
WHERE p.length = 2
RETURN p.nodes
```

A path is a **map** with three fields, so it comes back the same way on every
surface (Rust, HTTP JSON, Python, TypeScript, Arrow, Bolt) as any other map:

| Field | Value |
|---|---|
| `nodes` | the node keys, in walk order |
| `relationships` | one map per relationship: `type`, `start` and `end` (the keys of its stored direction) |
| `length` | the number of relationships |

`p = (a)` is a path of one node with length 0. A path works in `RETURN`,
`WITH`, `WHERE`, `ORDER BY`, `DISTINCT`, `count(p)` and `collect(p)`, and with
`shortestPath`: `MATCH p = shortestPath((a)-[:T*..5]->(b))`. In `OPTIONAL
MATCH` a named path works for fixed-length hops (a pattern that finds nothing
gives a null path); an `OPTIONAL MATCH` with a variable-length hop is a
separate limit that is not lifted here.

- **Masks.** A path through a node the caller cannot see does not exist: the
  whole row is dropped, never returned with that node left out.
- **Ordering.** `ORDER BY p` sorts paths as maps, which puts shorter paths first.
- **A new name.** A path name that is already bound, by an earlier path, a node,
  a relationship or an alias, is an error: ``variable `p` is already bound``.
- **Not a node or a relationship.** `key(p)`, `labels(p)` and `type(p)` are
  errors that name `p`.
- **Storing.** `SET x.r = p` is refused like any bare variable on the right of
  `SET`. A path nested in a list or map (`SET x.r = [p]`) is an ordinary map and
  is stored as one.
- **Not supported.** Named paths in `CREATE` and `MERGE`, and `DELETE p`.
- Relationship properties are not part of a path; match the relationship with
  its own variable to read them.

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

A grouping key that is a **bare variable** stays a node through the group, so
`key(c)`, `c.key` and `c.label` all still read after `WITH c, count(*) AS n`. A
*computed* key (`c.city`, `toLower(c.name)`) becomes a scalar, as it must.

An aggregating `WITH` projects its `RETURN` whether or not a `WHERE` follows it:

```cypher
MATCH (p:Person)-[:WORKS_AT]->(c:Company)
WITH c, COUNT(p) AS n
RETURN key(c), n * 2 AS doubled
```

### A WITH alias is in scope for its own WHERE

An alias a non-aggregate `WITH` introduces is visible to the `WHERE`,
`ORDER BY`, `SKIP` and `LIMIT` that follow it, because the stage projects before
it filters:

```cypher
MATCH (t:Talent)
WITH t, t.years_of_experience AS x
WHERE x > 11
ORDER BY x DESC
RETURN key(t), x
```

An unknown name in that `WHERE` is still an error (``unbound variable `nope` in
WHERE``). A name the `WITH` dropped is accepted and reads null at runtime, so the
predicate is false rather than wrong.

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

## RETURN * and WITH *

`*` stands for every name in scope: the node, path and value variables the
earlier clauses bound, in byte (code-point) order of name, so uppercase names
come before lowercase ones (`Zed`, `alpha`). Further items may follow
it.

```cypher
MATCH (p:Person)-[r:WORKS_AT]->(c:Company) RETURN *
MATCH (p:Person) WITH *, p.age * 2 AS double RETURN p.id, double
MATCH (p:Person) UNWIND p.tags AS t RETURN DISTINCT *
```

The first returns the columns `c` and `p`. Relationship variables, single or
variable-length (`r`, `[rs*1..3]`), are left out, because a relationship is not
returned bare (`RETURN r` is a named error); name what you need beside the
star (`RETURN *, type(r) AS t`). Anonymous nodes and relationships are not
names. A `WITH` replaces what is in scope, so `WITH a RETURN *` returns `a`.
With nothing left the star is an error, `RETURN * / WITH * with no variables in
scope`. `RETURN *` works in the `RETURN` of a write statement (the matched and
created nodes) and after `CALL … YIELD` (the yielded names).

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
- Any expression that evaluates to a list: `UNWIND range(1, 5) AS i`, `UNWIND $items AS x`, `UNWIND tail(p.tags) AS t`

Passing a non-list value (e.g., a plain integer property) is a named error.
Intermediate rows produced by `UNWIND` count against the 1,000,000-row budget.

A map element is read with `.`: `UNWIND $rows AS r RETURN r.id, r.name` returns the
entry, or null when the map has no such key. `MATCH`, `OPTIONAL MATCH`, a second
`UNWIND` and `WITH` may follow an `UNWIND`, each correlated on what is already
bound:

```cypher
UNWIND $ids AS id
MATCH (n:Person {id: id})
RETURN n.name
```

A `MATCH` after a `WHERE` that follows an `UNWIND` starts a new stage, as
`WITH … MATCH` does; a relationship variable is not carried into it. A write
clause after the `UNWIND` makes the statement a write: see
[UNWIND … write clauses](#unwind--write-clauses).

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

### DISTINCT inside an aggregate

`count(DISTINCT x)` and `collect(DISTINCT x)` deduplicate within each group
before aggregating, keyed by the same normalisation grouping uses (so an `Int`
`1` and a `Float` `1.0` are one value).

```cypher
MATCH (t:Talent)-[:FIT|:NEAR]->(c:Company)
RETURN key(c), count(DISTINCT t) AS talents
```

`count(DISTINCT *)` is a named parse error. A variable actually named `distinct`
still parses as an ordinary argument.

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

A sort key is any expression over the variables in scope, not only a column the
`RETURN` projects: a property, arithmetic, a function call, a subscript or a
`CASE`, and a projected alias may appear inside it.

```cypher
MATCH (n:Person)
RETURN n.name
ORDER BY n.score DESC, toLower(n.name)
LIMIT 3
```

Each key takes its own `ASC` (the default) or `DESC`. Null is the largest value, so
nulls sort last ascending and first descending. `RETURN DISTINCT` and a `RETURN` with an aggregate sort only what
they project; any other key is the error `ORDER BY over a non-projected
expression is not allowed with DISTINCT/aggregation`.

---

## Write statements

mushroomdb supports a Cypher write subset.  Every write statement flows through
the same mutation path as the Rust API — the rule engine fires incrementally and
the WAL commits with a single fsync before the response is returned.

Write results are always one row with four columns:

| Column | Meaning |
|---|---|
| `created` | nodes inserted |
| `properties_set` | individual `set_prop` calls applied |
| `deleted` | nodes and relationships deleted |
| `relationships_created` | relationships created (new in 0.10; last, so the first three keep their positions) |

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
- A property value is any expression: a literal, a `$parameter`, a map entry
  of one (`$p.id`), a list or map built from them, arithmetic, a function call
  (`CREATE (:Person {id: $id, name: toUpper($name), tags: [$tag, 'new']})`).
  The node stores the value, typed parameters (`Date`, `DateTime`, …) as their
  type; a parameter whose value is null leaves that property out, and a
  missing parameter is an error naming it.
- `CREATE (n:Person $props)` takes the whole property map from a parameter. A
  null value in the map is left out; a parameter that is not a map is an error
  naming it, and the map must hold a string `id`.
- A statement whose map reads a `$parameter` runs as the per-row form
  ([UNWIND … write](#unwind--write-clauses)) over one row, so it meets the
  same checks and stores what the same statement with the values written as
  literals stores. A literal `null` value (`{p: null}`) is a parse error.

### MATCH … SET

```cypher
MATCH (n:Person {id: 'alice'})
SET n.score = 99
```

- SET RHS must be a literal (integer, float, string, boolean), a `$param`
  reference, or arithmetic (`n.x + 1`); bare property-to-property copy is a named error.
- Multiple SET clauses in one statement are allowed.
- Combined MATCH … SET … RETURN is supported: the write commits, then RETURN
  projects from the post-write view.
- A value that reads a property (`n.count + 1`) reads what the statement has set
  so far: when two matched rows bind the same node (`MATCH (n:C), (m:D) SET
  n.count = n.count + 1`), the count goes up once per row, and in `SET n.v = 10,
  n.w = n.v + 0` the second item reads 10.
- When a SET touches a property that a rule evaluates, the rule engine
  re-evaluates incrementally: derived edges appear or retract within the same
  WAL frame.

### MATCH … REMOVE (property)

```cypher
MATCH (n:Person)
WHERE n.id = 'alice'
REMOVE n.score, n.nickname
```

- `REMOVE n.prop [, m.prop …]` removes properties from the matched nodes.
  Removing a property a node does not have does nothing and is not an error.
- It goes through the same write path as `remove_prop`: rules re-evaluate, so a
  derived edge that depended on the property retracts in the same WAL frame; a
  property a view owns, and `ns` on a node outside `default`, are refused with
  the error `remove_prop` gives.
- `MATCH … REMOVE … RETURN` is supported: the write commits, then RETURN
  projects from the post-write view, where a removed property reads as null.
- The write result's `properties_set` column counts the properties removed.
- `REMOVE n:Label` is refused: `not supported in mushroomdb 0.10: REMOVE n:Label
  (label removal)`. A node has one label in 0.10.
- `REMOVE r.prop` on a relationship, or on a name `MATCH` did not bind, is a
  parse error.
- Inside an interactive transaction `REMOVE` buffers like `SET`, with or
  without `RETURN`; the `RETURN` reads the transaction's state with the
  removal applied.

### `MATCH … SET n = {…}` / `SET n += {…}`

```cypher
MATCH (n:Person) WHERE n.id = 'alice'
SET n += {score: 2.5, nickname: null}

MATCH (n:Person) WHERE n.id = 'alice'
SET n = {name: 'Alice', score: 1}
```

- `SET n += {…}` (merge) sets each listed key and leaves every other property.
  A `null` value removes that property (a no-op when the node lacks it).
- `SET n = {…}` (replace) leaves the node with exactly the listed non-null keys:
  every other user property is removed, and a `null` value is an absent key.
  `SET n = {}` removes them all and keeps the node.
- Never removed by `=`: the key property `id` of a node whose `id` equals its
  key, the namespace `ns`, and a property a view owns. On such a node a map that
  names `id` must name the node's own key; another value, or null, is an error
  (`rename_node` changes a key). On a node whose `id` differs from its key, or
  that has none, `id` is an ordinary property. A map that names a different `ns`, or `+= {ns: null}` on a node
  outside `default`, is `NamespaceImmutable`; a map that names a view-owned
  property, with a value or null, is `ViewPropReadOnly`.
- It goes through the same write path as `SET n.prop = …` and `REMOVE`, in one
  WAL frame: unique constraints apply, rules re-evaluate every set and every
  removal, and one refusal applies nothing.
- Map values are expressions evaluated per matched row (`SET n += {a: n.a + 1,
  b: $p}`). The map is a literal or a `$parameter` holding a map
  (`SET n += $props`), with the same semantics: a null value in the parameter
  removes the key under `+=`. A parameter that is not a map is an error naming
  it, before anything is written. `SET n = m` (a variable) is refused
  (`not supported in mushroomdb 0.10`), as is mixing the map form with
  `SET n.prop = …` in one clause. `n` must be a node. A `MERGE` takes the map
  forms in `ON CREATE SET` / `ON MATCH SET` (see [MERGE](#merge)).
- `SET n.id = …` and `REMOVE n.id` follow the same rule as the map forms, and as
  every other write path: on a node whose `id` equals its key, restating the key
  is accepted and anything else is an error pointing at `rename_node`; on any
  other node `id` is an ordinary property.
- `RETURN` projects the post-write state. `properties_set` counts the
  properties set plus the properties removed.
- Inside an interactive transaction the map forms run as they do auto-commit
  and are buffered until `COMMIT`.

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

- Match-or-create by a single key property. An optional `ns` names the
  namespace the create arm writes into (`MERGE (n:Doc {id: 'x', ns: 'tenant-a'})`).
- A role bound to exactly one namespace stamps that namespace when the
  pattern does not name `ns`. A role bound to two or more must name one.
- `ON CREATE SET` / `ON MATCH SET` apply inside the same write batch as the
  insert-or-skip. Both may appear on one MERGE. Their items are `n.prop = …`,
  `n += <map>` and `n = <map>`, in any mix, with the semantics of
  `SET n += …` and `SET n = …` after `MATCH`: `MERGE (n:Person {id: $id}) ON CREATE SET n
  += $props` creates the node with the parameter's properties, and under `ON
  MATCH SET n += $props` a null value removes that key.
- The key (and `ns`) may be any expression, `$parameter`s included
  (`MERGE (n:Person {id: $id})`); the MERGE matches on the value exactly as it
  does on the same literal. A key that evaluates to null is an error, as is a
  key that is not a string.
- Other extra properties are not supported, and neither is a parameter map in
  the pattern (`MERGE (n:Person $props)`, refused by name): name the key and set
  the rest, `MERGE (n:Person {id: $props.id}) ON CREATE SET n += $props`.

#### MERGE with a relationship

```cypher
MERGE (a:Person {id: 'alice'})-[:KNOWS]->(b:Person {id: 'bob'})
  ON CREATE SET a.met = true
  ON MATCH SET b.seen = true
RETURN a, b
```

- Finds or creates each end node by its key, then finds or creates the edge
  between them. Running the statement again changes nothing: the edge is never
  duplicated, and a write that matches in full logs nothing.
- `ON MATCH SET` runs when both nodes and the edge already existed; otherwise the
  missing nodes and the edge are created and `ON CREATE SET` runs. A `SET` item
  names the start or the end node (`a.x`, `b.x`), never the relationship.
- Everything is one write batch: one commit, rules re-evaluate for every
  assignment, and every write meets the usual checks (constraints, the `id`
  rule, namespaces: both nodes must share one, as for any edge).
- `<-[:T]-` is the same edge written from the other end. The reverse direction,
  another type and another end node are distinct edges.
- A role-scoped write needs create or update scope on both labels and the edge
  type in `create_edge_types`; a hidden end node reads as absent.
- One relationship, with one type and a direction. Undirected, variable-length,
  `[:A|B]`, an untyped relationship, relationship properties and a second
  relationship are refused by name (`not supported in mushroomdb 0.10: …`).
- Inside an interactive transaction it is buffered like the single-node form.

### MATCH … CREATE / MATCH … MERGE

A `MATCH` followed by a `CREATE` or a `MERGE` runs the write once for each row
the `MATCH` finds:

```cypher
MATCH (a:Person {id: 'alice'}), (b:Person {id: 'bob'})
CREATE (a)-[:KNOWS]->(b)
```

```cypher
MATCH (p:Person) WHERE p.score > 10
CREATE (b:Badge {id: 'badge-' + p.id})-[:FOR]->(p)
SET p.badged = true
RETURN b.id
```

```cypher
MATCH (a:Person {id: 'alice'}), (b:Person {id: 'bob'})
MERGE (a)-[:LIKES]->(b)
  ON CREATE SET a.likes = 1
  ON MATCH SET a.likes = a.likes + 1
```

- It is the statement `UNWIND [1] AS _ MATCH … CREATE …`, and runs exactly as
  that does (see [UNWIND … write clauses](#unwind--write-clauses)): the same
  clauses after the `MATCH` (`WHERE`, `OPTIONAL MATCH`, `WITH`, `UNWIND`, then
  `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE` in any order, then `RETURN`), the
  same per-row values, one batch, the same counts and the same role and
  transaction rules. A `MATCH` that finds no row writes nothing.
- A `MERGE` end written `(a)`, a node an earlier clause bound, is that node: the
  `MERGE` finds or creates the relationship (and the other end, if that one has a
  label and key) and never creates `a`. A bound end that is null in a row (an
  `OPTIONAL MATCH` that found nothing) is an error, as is `MERGE (a)` alone.
- It runs inside an interactive transaction too (HTTP `/tx`, Bolt `BEGIN`, and
  so a driver-managed write transaction such as `session.execute_write`), with
  the same results and counts as auto-commit. It reads the transaction's own
  earlier writes, and what it writes stays private to the transaction until
  `COMMIT`.

### UNWIND … write clauses

The bulk-load idiom: the write clauses run once for each row the `UNWIND` (and
any `MATCH`, `WITH` or `WHERE` after it) produces.

```cypher
UNWIND $rows AS r
CREATE (:Person {id: r.id, name: r.name})
```

```cypher
UNWIND $edges AS e
MATCH (a:Person {id: e.from}), (b:Person {id: e.to})
CREATE (a)-[:KNOWS]->(b)
```

```cypher
UNWIND $rows AS r
MERGE (n:Person {id: r.id})
  ON CREATE SET n.created = r.at
  ON MATCH SET n.seen = r.at
```

- **Clauses.** After the read part: `CREATE` (a node, a chain, comma-separated
  patterns, or `(a)` naming a node an earlier clause bound), `MERGE` (a node, or
  a node-relationship-node pattern whose ends may be `(a)` nodes an earlier
  clause bound, with `ON CREATE SET` / `ON MATCH SET`, whose items may be the
  map forms `n += <map>` / `n = <map>`), `CREATE (n:L $props)` (the whole map
  from a parameter),
  `SET n.f = expr [, …]`, `SET n = <map>` and `SET n += <map>` (the map is a
  literal, a `$parameter`, a row or any expression that evaluates to a map),
  `REMOVE n.f`, `DELETE n` and `DETACH DELETE n`, then an optional `RETURN`. A
  value on the right of `=`, or in a property map, is any expression over the
  row: `r.id`, `$tenant`, `toUpper(r.name)`, `r.n + 1`, a literal.
- **One batch.** The whole statement is one WAL frame and one commit, however
  many rows. It is atomic: a failure on any row (a duplicate key, a unique
  constraint, a refused `id` or namespace write, a role scope) writes nothing.
  Every write meets the same checks as a single `CREATE`, `MERGE` or `SET`, and
  rules re-evaluate for every assignment.
- **A row sees the rows before it, and a clause the clause before it.** The
  writes run in order, row by row, and each reads what the statement has written
  so far, node properties included. `UNWIND [1, 1] AS x MERGE (:N {id: x})`
  creates one node. `UNWIND [1, 1] AS x MATCH (n:C {id: 'c'}) SET n.count =
  n.count + 1` adds two, because the second row reads the first row's `count`.
  `SET n.v = 10 SET n.w = n.v` sets `w` to 10. `UNWIND $keys AS k MERGE (n:C {id:
  k}) ON CREATE SET n.count = 1 ON MATCH SET n.count = n.count + 1` counts each
  key as often as it occurs in the list. A `MERGE` after a `CREATE` of the same
  key in the same statement finds it. A `CREATE` of a key that exists, or that an
  earlier row already created, is a `DuplicateKey` error.
- **Reads come first.** The `MATCH`, `WITH` and `WHERE` run once, against the
  state before the statement, so which rows exist and which nodes they bind do
  not change as the writes go: a node a `MATCH` found is still a row after an
  earlier row deleted it, and a row is not added because an earlier row created a
  node the `MATCH` would now find. Only the values written read the statement's
  own writes. A `MATCH`, `UNWIND` or `WITH` after a write clause is refused by
  name. A row whose `MATCH` found no node writes nothing; an `OPTIONAL MATCH`
  that found none skips the `SET`, `REMOVE` and `DELETE` that name it.
- **Null.** A property whose value is null in `CREATE` is left out, so a sparse
  row creates a node without it. `SET n.f = <null>` is an error, as for a single
  `SET`; in a map, a null entry removes the property under `+=` and is an absent
  key under `=`. A `MERGE` key must be a string; a null key is an error.
- **`RETURN`.** One row for each row the writes ran for, in order, after the
  commit. The read part is not run again: each row keeps the nodes and values it
  wrote with, and `RETURN` reads their properties from the committed state, so a
  `SET` of a property the `MATCH` filtered on still returns its row (with the new
  value), `DETACH DELETE n RETURN count(*)` counts the deleted rows, and a
  `CREATE` with a `rand()` key returns the nodes it created. `RETURN n.name`,
  `RETURN count(*)`, `DISTINCT` and `ORDER BY` / `SKIP` / `LIMIT` work. A node the
  statement deleted reads as null. A `RETURN` captures only what it names, so a
  bare `count(*)` carries a row count and nothing else. A relationship variable
  of the read part or of a `MERGE` (`RETURN type(r), r.count`) is found again
  between its two end nodes; a variable-length relationship (`[r:T*1..3]`) in
  such a `RETURN` is refused by name.
- **Counts.** Without `RETURN` the row is `created` (nodes inserted),
  `properties_set` (properties set, plus those removed that existed), `deleted`
  (nodes deleted, plus the relationships a `DETACH DELETE` removed with them,
  counted as `MATCH … DETACH DELETE` counts them) and `relationships_created` (relationships created).
- **Cost.** One pass over the rows; 10,000 `CREATE` rows take about as long as
  `ingest` of the same rows.
- Inside an interactive transaction (and so a Bolt `BEGIN`) it runs as it does
  auto-commit, reading the transaction's own earlier writes; its writes are
  buffered until `COMMIT`. Constraints and the other checks a commit makes run
  at `COMMIT`, and a failure there rejects the whole transaction.
- Under a role-scoped write the `MATCH` is masked, so a hidden node reads as
  absent and nothing is written to it; create and update scope are checked per
  label as for the single statements.
- Not supported: deleting a relationship (`DELETE r`) after `UNWIND`, `REMOVE
  n:Label`, and a map form followed by another `SET` item in the same clause (use
  two `SET` clauses).

---

## Schema statements (DDL)

Indexes and unique constraints can be declared, dropped and listed in Cypher.
Each statement is a whole statement on its own: nothing may come before or
after it (`CREATE INDEX … RETURN 1` is a parse error). Keywords are
case-insensitive, and a name may be backtick-quoted (`` CREATE INDEX `by year` … ``).

```cypher
CREATE INDEX idx_age FOR (n:Person) ON (n.age)
CREATE INDEX FOR (n:Person) ON (n.score)
CREATE INDEX idx_age IF NOT EXISTS FOR (n:Person) ON (n.age)
DROP INDEX idx_age
DROP INDEX idx_age IF EXISTS
SHOW INDEXES

CREATE CONSTRAINT uniq_email FOR (u:User) REQUIRE u.email IS UNIQUE
CREATE CONSTRAINT uniq_email IF NOT EXISTS FOR (u:User) REQUIRE u.email IS UNIQUE
DROP CONSTRAINT uniq_email
DROP CONSTRAINT uniq_email IF EXISTS
SHOW CONSTRAINTS
```

`CREATE INDEX` creates a **range index** on one property of one label, the
index `GraphDb::enable_range_index` declares; with no name it is called
`idx_<Label>_<property>`. `CREATE CONSTRAINT` declares a unique constraint, as
`GraphDb::create_constraint` does, together with the equality index it is
checked against. Equality and full-text indexes are declared through the API,
the schema file or the CLI, not in Cypher.

Creating a name that exists, or dropping one that does not, is an error that
names it. `IF NOT EXISTS` makes a `CREATE` do nothing when the name exists;
an unnamed `CREATE INDEX` also does nothing when the label and property
already have a range index, while a named one over a pair indexed under
another name is an error naming that index. For `CREATE CONSTRAINT`, an
existing constraint on the same label and property also counts. `IF EXISTS`
makes a `DROP` of a missing name do nothing.
A constraint over data that already breaks it is refused, as
`create_constraint` refuses it. A statement that changes the schema answers
with no columns and no rows.

`SHOW INDEXES` lists every index, sorted by name, with the columns `name`,
`type` (`range`, `equality` or `fulltext`), `label`, `property` and `state`
(always `online`: an index is built in full by the statement that declares
it). An equality index is listed as `eq_<Label>_<property>` and a full-text
index as `ft_<Label>_<property>`; `DROP INDEX` drops either by that name.
Those two prefixes are reserved: a range index cannot be named `eq_…` or
`ft_…`, through Cypher or `enable_range_index`. Two pairs can still join to
one listed name (`(A_b, c)` and `(A, b_c)` are both `eq_A_b_c`); `DROP INDEX`
then refuses, listing both, and `disable_index(label, field)` or
`disable_fulltext(label, field)` drops the one meant. The
equality index a constraint is checked against is listed too, and
`DROP INDEX` refuses it, naming the constraint, until the constraint is
dropped. `SHOW CONSTRAINTS` lists `name`, `type` (`unique`), `label` and
`property`, sorted by name.

The first range index or constraint on a store moves it to the V11 snapshot
format, which 0.9.x cannot read, whether it is declared in Cypher or through the
API.

**What reads a range index.** With a range index on `(Label, prop)`, a
`MATCH (n:Label)` whose `WHERE` has one of these as an `AND`-ed part is answered
from the index instead of a scan of the label:

- `n.prop < x`, `n.prop <= x`, `n.prop > x`, `n.prop >= x`, either way round
  (`x < n.prop`), and two of them together as a range
  (`n.prop >= $lo AND n.prop < $hi`);
- `n.prop STARTS WITH x`.

`x` is a literal or a `$parameter`. So is `ORDER BY n.prop [DESC] … LIMIT k`
(with or without `SKIP`) over `MATCH (n:Label)`, with no `WHERE` or with a
`WHERE` made only of the range above on the same property: the index is read
in sort order from the end the sort starts at, and reading stops once the first
`SKIP + LIMIT` rows (and any rows tied with the last of them) are known. Two
cases read more: a descending sort with no `WHERE` also walks the label's nodes
once to find those without the property (they sort first), as does an
ascending one whose first rows reach past the numbers, strings and booleans;
and a descending `STARTS WITH` reads all of the prefix's matches.

The answer is the scan's, row for row and in the same order: integers and
floats compare as numbers (`n.prop > 5.5` finds the integer 10), a string bound
finds only strings, and `Date`, `LocalDateTime` and `DateTime` values range in
their own order. A node without the property is still sorted where `ORDER BY`
puts it (last ascending, first descending). A `Duration`, `Bool`, list or `null`
bound is answered by the scan, which finds nothing for it, since those values
are not ordered. The index is not read for `n.key`, `n.id` or `n.label`, inside
a transaction, through a reader whose recent commits are not yet folded, or
across a relationship (`MATCH (n:L)-[:R]->(m) WHERE n.prop > 1`); those scan,
with the same answer.

**Where they run.** A statement that changes the schema is a write: it runs
through `GraphDb::query_write`, HTTP `/query` and the MCP `query` tool with a
full token, Bolt auto-commit and `mushroomdb query`. It is refused inside a
transaction (`BEGIN … COMMIT`, `Transaction::execute`), on a read-only
(`open_at`) handle, in a time-travel (`as_of`) query, in a masked or namespaced
read, and for a role-bound token (HTTP 403, Bolt `Security.Forbidden`).
`SHOW INDEXES` and `SHOW CONSTRAINTS` are reads and run wherever a read does.
For a role-bound token, a masked read (`mask`, `scoped`) or a namespaced read
they are narrowed: a row is listed only when the caller can see at least one
node of its label, so a scoped caller learns nothing of the schema outside its
scope. A full-access read lists everything.

The words these statements use (`INDEX`, `CONSTRAINT`, `DROP`, `SHOW`,
`UNIQUE`, `REQUIRE`, …) are not reserved: they still name properties, labels
and variables (`MATCH (show:Index) RETURN show.index`).

## Procedures (`CALL`)

`CALL` runs a built-in procedure. It is a statement of its own:

```cypher
CALL db.ping()
CALL db.ping() YIELD success
CALL db.ping() YIELD success AS ok WHERE ok RETURN ok
CALL db.ping() YIELD * RETURN success
```

The full form is `CALL ns.name(args) [YIELD field [AS alias], …] [WHERE …]
[RETURN … [ORDER BY …] [SKIP n] [LIMIT n]]`.

- **Name.** A dotted name with at least two parts; the namespace is every part
  but the last (`db.schema.visualization` is `visualization` in `db.schema`).
  Names are case-sensitive. With no arguments the parentheses may be left out
  (`CALL db.ping`).
- **Arguments.** Any expression that reads no variable: literals, `$parameters`
  and expressions over them (`CALL db.ping($x)`).
- **`YIELD`.** Picks columns and may rename them with `AS`. With no `YIELD`,
  or `YIELD *`, every column the procedure returns is bound under its own name.
  Yielding a column the procedure does not return is an error naming it and
  listing the columns it does return; binding one name twice is an error.
- **`WHERE` and `RETURN`.** Filter and project the yielded names as in any
  read query: expressions, aggregates, `DISTINCT`, `ORDER BY`, `SKIP`, `LIMIT`.
  Without `RETURN` the statement returns the yielded columns.

| Procedure | Arguments | Columns |
|---|---|---|
| `db.ping()` | none | `success` (always `true`) |
| `db.labels()` | none | `label` |
| `db.relationshipTypes()` | none | `relationshipType` |
| `db.propertyKeys()` | none | `propertyKey` |
| `db.indexes()` | none | `name`, `type`, `labelsOrTypes`, `properties`, `state` |
| `db.constraints()` | none | `name`, `type`, `labelsOrTypes`, `properties` |
| `dbms.components()` | none | `name`, `versions`, `edition` |
| `db.schema.visualization()` | none | `nodes`, `relationships` |

Every procedure takes no arguments; passing one is the error `<ns>.<name> takes
no arguments, got <n>`. The catalog procedures list what the caller can see and
nothing else: a role, a mask or a namespace never learns a label, relationship
type, property key, index or constraint that only nodes outside its scope
carry.

- **`db.labels()`, `db.relationshipTypes()`, `db.propertyKeys()`** return one
  column, one row per name, sorted. A label is listed when a visible node has
  it; a relationship type when it joins two visible nodes; a property key when
  it is set on a visible node or on a relationship between visible nodes.
- **`db.indexes()`, `db.constraints()`** return the rows `SHOW INDEXES` and
  `SHOW CONSTRAINTS` return (see [Schema statements](#schema-statements-ddl)), with
  the label and property each in a one-element list under `labelsOrTypes` and
  `properties`, the shape common driver tools read. `type` is the index kind
  (`range`, `equality`, `fulltext`) or `unique`; `state` is always `online`.
- **`dbms.components()`** returns one row: `name` is `mushroomdb`, `versions` is
  a one-element list holding the engine's version, and `edition` is `open`
  (there is one unrestricted edition, so the column carries no tier name).
- **`db.schema.visualization()`** returns one row. `nodes` lists one map
  `{label}` per label; `relationships` lists one map `{type, from, to}` for each
  type, source label and target label that actually occur together. The
  maps are plain values, not graph node and relationship values, so a graph
  browser lists them rather than drawing a schema diagram.

Cost: the procedures read the store once, so they take time in proportion to
the nodes and relationships the caller can see. A relationship type stops its
scan at its first edge between visible nodes, and the node scan for property
keys ends once every key has been found; `db.propertyKeys()` and
`db.schema.visualization()` still read every relationship. Measured on a
release build with 100,000 nodes and 100,000 relationships: `db.labels()` about
0.5 ms, `db.relationshipTypes()` about 0.3 ms, `db.propertyKeys()` and
`db.schema.visualization()` about 14 ms to 15 ms. The index and constraint
procedures read the declared schema and are sub-millisecond.

An unknown procedure is the error `unknown procedure: <ns>.<name>`.

**Where it runs.** A call is a read and writes nothing. It runs on every read
entry point: `GraphDb::query` and `query_write`, a read-only (`open_at`) handle
and `query_at`, `query_masked` and a role-scoped `query_write_authz`, the
lock-free reader, a transaction (`Transaction::execute`, where it sees the
transaction's own writes), HTTP `/query` (with `as_of`, `mask` or `namespace`
too), Bolt `RUN` and the MCP `query` tool. Under a role, a mask or a namespace,
a procedure sees only the nodes the caller can see, the relationships between
them, and the schema rows `SHOW` gives that caller. `query_write_authz` refuses
any statement, a call included, for a role with no write scope, as it refuses
`SHOW`; such a role reads through `query_masked` (HTTP `/query` routes a call
there).

**What is refused.** `CALL` inside a larger query — after `MATCH`, `WITH` or
`UNWIND`, or followed by any clause other than `WHERE` and `RETURN` (a second
`MATCH`, `UNION`) — is the error `not supported in mushroomdb 0.10: CALL inside
a larger query`. A `CALL { … }` subquery is `not supported in mushroomdb 0.10:
CALL subquery`.

`CALL`, `YIELD`, `FOREACH`, `EXISTS` and `LOAD` are not reserved: they still
name properties, labels and variables (`MATCH (call:Yield) RETURN call.exists`).

## Unsupported features

These openCypher forms are recognised and refused with an error that names
them, `not supported in mushroomdb 0.10: <feature>`, rather than an unexpected
token. The error is the whole message (a parse error carries the `parse:`
prefix, the regular-expression operator `lex:`).

| Form | Error message |
|---|---|
| `FOREACH (x IN list \| …)` | `not supported in mushroomdb 0.10: FOREACH` |
| `EXISTS { … }` | `not supported in mushroomdb 0.10: EXISTS { } subquery` |
| `COUNT { … }` | `not supported in mushroomdb 0.10: COUNT { } subquery` |
| `COLLECT { … }` | `not supported in mushroomdb 0.10: COLLECT { } subquery` |
| `CALL { … }`, `CALL (x) { … }` | `not supported in mushroomdb 0.10: CALL subquery` |
| `MATCH … CALL ns.name()`, `CALL ns.name() MATCH …` | `not supported in mushroomdb 0.10: CALL inside a larger query` |
| `CALL` in a subscription (`subscribe_query`) | `not supported in mushroomdb 0.10: CALL here (a procedure call runs only as a read query of its own)` |
| `REMOVE n:Label` | `not supported in mushroomdb 0.10: REMOVE n:Label (label removal)` |
| `SET n:Label` | `not supported in mushroomdb 0.10: SET n:Label (label assignment)` |
| `(n:A:B)`, `(n:A\|B)`, `WHERE n:A:B` | `not supported in mushroomdb 0.10: more than one label (n:A:B, n:A\|B); a node has one label` |
| `WHERE (n)-[:K]->()`, `NOT (n)-->()`, `exists((n)-->())`, `size((n)-->())` | `not supported in mushroomdb 0.10: pattern predicate (a relationship pattern used as an expression)` — test the pattern with `OPTIONAL MATCH (n)-[:K]->(m) WITH n, count(m) AS c WHERE c > 0` |
| `[(n)-->(m) \| m.name]` | `not supported in mushroomdb 0.10: pattern comprehension [(n)-->(m) \| …]` |
| `RETURN n {.name, .age}` | `not supported in mushroomdb 0.10: map projection (n {.key, …})` |
| `n.name =~ 'a.*'` | `not supported in mushroomdb 0.10: regular-expression match (=~)` |
| `LOAD CSV …` | `not supported in mushroomdb 0.10: LOAD CSV` |
| `USE db …` | `not supported in mushroomdb 0.10: USE (database selection)` |
| `allShortestPaths(…)` | `not supported in mushroomdb 0.10: allShortestPaths` |

The other refusals of this release (forms of `MERGE`, `SET` maps, `UNWIND`
writes, `time(…)`) are listed with their features above and under
[Named-error](#named-error).

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

### A leading OPTIONAL MATCH

A query may begin with `OPTIONAL MATCH`. It runs against one row, as `UNWIND [1]
AS _ OPTIONAL MATCH …` does, so it returns the rows it finds, or one row of nulls
when it finds none:

```cypher
OPTIONAL MATCH (n:Person {id: 'nobody'})
RETURN n, n.name
```

returns one row, `null, null`. Clauses may follow it as they follow an `UNWIND`
(`MATCH`, `OPTIONAL MATCH`, `WITH`, `UNWIND`, `WHERE`).

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

A null parameter binds as a null: `None` in Python, JSON `null` over HTTP (top
level, or inside a list or map), `Value::Null` in Rust, matching Bolt. So
`WHERE $p IS NULL` is true for it, and the query does not fail with a missing
parameter.

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
| `toLower(s)` | `String` | `String` | ASCII casefold |
| `toUpper(s)` | `String` | `String` | ASCII casefold |
| `size(x)` | `String` or `List` | `Int` | `size(str)` is bytes; lists are element count |
| `coalesce(a, b, …)` | any | first non-null | never null unless all args are null |
| `type(r)` | `Rel` | `String` | relationship type label |
| `abs(n)` | `Int` or `Float` | same type | absolute value |
| `round(f)` | `Float` | `Float` | rounds to nearest integer as Float |
| `textMatches(s, q)` | `String`/`List`, `String` | `Bool` | per-row scratch full-text; see [`fulltext.md`](fulltext.md) |
| `contains(s, sub)` | `String`, `String` | `Bool` | substring test |
| `startsWith(s, p)` | `String`, `String` | `Bool` | prefix test |
| `endsWith(s, p)` | `String`, `String` | `Bool` | suffix test |
| `toInteger(x)` | `Int`/`Float`/`String` | `Int` | parse/truncate; unparseable → null |
| `toFloat(x)` | `Int`/`Float`/`String` | `Float` | parse; unparseable → null |
| `toString(x)` | scalar | `String` | scalar to text |
| `decay(base, age, halflife)` | `Int`/`Float` ×3 | `Float` | `base * 0.5^(age / halflife)`; error if `halflife <= 0` |
| `key(n)` | node variable | `String` | always the id-map key. Non-node argument is a named error |
| `id(n)` | node variable | `String` | alias of `key(n)` (always the id-map key). `n.id` is stored-wins property access |
| `labels(n)` | node variable | `List` | a one-element list holding the node's label; `n.label` reads it as a scalar |
| `date([x])` | `String` or `Map` | `Date` | `date("2024-03-15")`, `date({year: 2024, month: 3, day: 15})`; no argument is today (UTC). A `LocalDateTime` or `DateTime` argument is cut to its date. Null, invalid or wrong-type input → null. Storing a result moves the store to the V11 format |
| `localdatetime([x])` | `String` or `Map` | `LocalDateTime` | `localdatetime("2024-03-15T10:30:00")`; no argument is now (UTC wall clock). Null, invalid or wrong-type input → null. Storing a result moves the store to the V11 format |
| `datetime([x])` | `String` or `Map` | `DateTime` | `datetime("2024-03-15T10:30:00+05:30")`; the offset is kept (within ±18:00), a string with no offset is read as UTC; no argument is now. Null, invalid or wrong-type input → null. Storing a result moves the store to the V11 format |
| `duration(x)` | `String` or `Map` | `Duration` | `duration({days: 3, hours: 2})` (keys `years`, `months`, `weeks`, `days`, `hours`, `minutes`, `seconds`, `milliseconds`, `microseconds`, `nanoseconds`) or `duration("P1Y2M3DT4H")`. Null, invalid, unknown key or wrong-type input → null. Storing a result moves the store to the V11 format |
| `time(…)`, `localtime(…)` | | | error: `not supported in mushroomdb 0.10: time types` |

### Temporal values

A query can build and return `Date`, `LocalDateTime`, `DateTime` and `Duration`
values with the functions above, and `toString` gives their ISO 8601 text. They
compare with `=`, `<`, `<=`, `>`, `>=` within their own type (a `DateTime` by
instant; a `Date` never equals a `LocalDateTime`), and they sort in `ORDER BY`.
Durations compare for equality only, component by component: a month is not 30
days.

`+` and `-` between a date or date-time and a duration move it, months first
(a day past the end of a shorter month lands on its last day); a `Date` ignores
the duration's time part. Durations add, subtract, and scale with `*` and `/` by
a number. An out-of-range result is null.

Strings must be the extended ISO 8601 form the types print: `datetime` needs a
time (`datetime("2024-03-15")` is null) and `date` takes no time
(`date("2024-03-15T10:00:00")` is null; convert with `date(datetime(…))`).
Compact forms (`20240315`) and named time zones (`[Europe/Paris]`, or a `timezone`
key in a map) are not supported and give null. A map constructor takes
`year` (required), `month`, `day`, and, for `localdatetime` and `datetime`,
`hour`, `minute`, `second`, `millisecond`, `microsecond` and `nanosecond`, all
integers; a `datetime` map is read as UTC. Years outside about ±5.88 million for
`Date`, and ±292 thousand for date-times, give null.

A temporal value can be stored: `SET n.d = date("2024-01-01")` writes a `Date`.
The first such write moves the store to the V11 snapshot format, which 0.9.x
cannot read; see [Format stability](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/format-stability.md).

Aggregations: `count`, `sum`, `avg`, `min`, `max`, and `collect(x)` (gather each
row's value into a list, per group when grouping keys are present). `count` and
`collect` accept `DISTINCT`.

Four statistical aggregates read a numeric property or alias: `stDev(x)` (sample
standard deviation) and `stDevP(x)` (population standard deviation), and
`percentileCont(x, p)` and `percentileDisc(x, p)`. Names are case-insensitive.
Nulls are skipped; ints and floats mix; any other value is a type error naming the
function. `stDev` and `stDevP` are `0.0` with fewer than two non-null values
(including none), and a percentile of no values is null. `p` is a number from 0 to 1, a literal or a
parameter, the same for every row; anything else is an error, even over no rows.
`percentileDisc` is the nearest-rank value (the input value, with its own type);
`percentileCont` interpolates linearly between the two nearest ranks and is always
a float. A percentile holds every value of its group in memory until the group is
done; `stDev` and `stDevP` keep constant state. All four work with grouping keys,
`DISTINCT` and aggregate `WITH`.

`CASE WHEN <cond> THEN <value> … [ELSE <value>] END` is supported anywhere a
scalar expression is (read or write RETURN/WITH/WHERE/SET). `UNION` and `UNION ALL` combine
read queries with matching column names. Relationship patterns accept
type alternation: `(a)-[:A|:B]->(b)`.

Calling an unknown function name returns:

```
unknown function `name`; supported: toLower, toUpper, size, coalesce, type, abs, round, textMatches, contains, startsWith, endsWith, toInteger, toFloat, toString, decay, key, id, labels
```

### Infix string predicates

`STARTS WITH`, `ENDS WITH` and `CONTAINS` are infix spellings of the
`startsWith` / `endsWith` / `contains` functions above, and share their null and
non-string handling: a missing property is null, not an error, so it never matches
(and `NOT c.name CONTAINS 'x'` does not match it either).

```cypher
MATCH (c:Company) WHERE c.name STARTS WITH 'Acme'   RETURN c.name
MATCH (c:Company) WHERE c.name ENDS WITH 'Works'    RETURN c.name
MATCH (c:Company) WHERE NOT c.name CONTAINS 'Design' RETURN c.name
```

`STARTS` or `ENDS` without its `WITH` is a named parse error.

### Comparisons and boolean operators

A comparison (`=`, `<>`, `<`, `<=`, `>`, `>=`), `IS NULL`, `IS NOT NULL`, `IN`,
`STARTS WITH`, `ENDS WITH`, `CONTAINS`, `NOT`, `AND`, `OR` and `XOR` work in every
expression position, not only in `WHERE`: `RETURN` and `WITH` items, `ORDER BY`
keys, list and map elements, a list comprehension's filter and projection, a
`reduce` body, function arguments, `CASE` branches, `SET` values, `CREATE` and
`MERGE` property values (`$param`s included; a variable or property only when
a clause before the `CREATE` / `MERGE` binds it), and an `UNWIND` source.

```cypher
MATCH (n:Person) RETURN n.name, n.age >= 18 AS adult
MATCH (n:Person) WITH n, n.score > 10 AS high WHERE high RETURN n.name
RETURN [x IN [1, 2, 3] | x > 1] AS flags, $p IS NULL AS missing
MATCH (n:Task) SET n.late = n.due < $today AND NOT n.done
```

Precedence, loosest first: `OR`, `XOR`, `AND`, `NOT`, comparison, then `IN`,
`STARTS WITH`, `ENDS WITH`, `CONTAINS`, `IS [NOT] NULL`, then `+ -`, `* / %`, `^`,
unary minus and subscripts. So `NOT a = b` is `NOT (a = b)`, `a OR b AND c` is
`a OR (b AND c)` and `1 + 2 > 2 AND true` is true. Parentheses group either kind
of expression: `(n.age + 1) > 18`, `(a OR b) AND c`. One comparison per operand
pair: `a < b < c` is a parse error. Expressions nest at most 64 levels deep
(parentheses, function arguments, list and map elements and `CASE` branches count
together, and so does a run of `NOT`s); deeper input is refused with a named
parse error, `expression nesting too deep` (or `list or map literal nesting too deep`
for nested literals).

As a value a predicate is three-valued: true, false or null. A comparison or `IN`
with a null side is null (`RETURN null > 1` is null, `1 IN [2, null]` is null);
`NOT null` is null; `null AND false` is false and `null OR true` is true;
`null XOR x` is null. `IS NULL` and `IS NOT NULL` are never null. Only numbers,
strings, temporal values of one type, and binary values order; any other pair
compares as null under `<`, `<=`, `>` and `>=`, so `1 < 'a'`, `false < true` and
`[1] < [2]` are all null (booleans and lists do not order). Values of different
types are unequal (`1 = 'a'` is false).

A non-boolean operand of `NOT`, `AND`, `OR` or `XOR`, or a whole `WHERE` that is a
bare value (`WHERE n.flag`), reads as true unless it is empty or zero (a non-empty
string or list, a non-zero number), and null stays null: `RETURN 1 AND true` is
true, `NOT 0` is true. **Planned:** strict boolean operands, where a non-boolean
operand is a type error as openCypher specifies, are planned for a later release.

`x IN list` takes one list on the right, an additive expression: `IN [a, b]`,
`IN $list`, `IN n.tags`, `IN [1, 2] + [3]`. Each element is compared whole, so
`[1, 2] IN [[1, 2], [3]]` is true and `1 IN [[1]]` is false. It is true when an
element equals `x`; otherwise null if `x`, an element or the list is null; otherwise
false. `null IN []` is false and `null IN [[]]` is null. A written list is read in
order and the scan stops at the first match, so an element after it is never
evaluated.

`WHERE` evaluates its predicate the same three-valued way and keeps the row only
when it is true: false and null both drop it, `NOT` included. So
`WHERE NOT n.missing = 1` drops every row (`NOT null` is null), and
`WHERE NOT (n.age > 18 AND n.missing = 1)` drops an adult (`true AND null` is null)
but keeps a minor (`false AND null` is false). `CASE WHEN` and a list
comprehension's `WHERE` read their condition the same way. Before 0.10, `WHERE`
read a null comparison as false and then applied `NOT`, so it kept those rows. A
predicate used as a value is never turned into an index lookup; only a `WHERE`
predicate is. `SET n.flag = n.score > 10` stores a boolean; a predicate that is
null is the existing `SET value for n.flag evaluated to null` error. An unaliased
predicate column is named `<expr>`.

### Label predicates

`n:Label` is true when the node `n` has that label and false when it has
another; `r:TYPE` does the same for a relationship's type. It is an expression
like any other, in `WHERE`, `RETURN`, `WITH`, `CASE` and the
`RETURN` of a write statement, and it binds tighter than `NOT`, `AND` and the
comparisons:

```cypher
MATCH (n) WHERE n:Person OR n:Company RETURN n.id
MATCH (p:Person) OPTIONAL MATCH (p)-[:WORKS_AT]->(c)
RETURN p.id, c:Company AS employed
MATCH (a)-[r]->(b) WHERE NOT r:BLOCKS RETURN b.id
```

- Three-valued: a null node (an `OPTIONAL MATCH` that found nothing) gives null,
  so `WHERE NOT c:Company` drops that row as well.
- It reads the node's label, not a property: a node that stores a property
  called `label` does not change the answer (`n.label` reads the stored value).
- A node has one label in 0.10, so `n:A:B` and `n:A|B` are refused by name
  (`not supported in mushroomdb 0.10: more than one label …`). On a value that is
  not a node or a relationship it is an error naming the variable.
- It is never turned into an index lookup; the label in the pattern
  (`MATCH (n:Person)`) is what selects nodes by label.
- Where it errors in this build: on a list comprehension's own variable
  (`[x IN nodes WHERE x:A]` — the element is a value; a node variable from
  outside the comprehension works), and in a value of a per-row write (the
  `UNWIND … ` and `MATCH … CREATE/MERGE` statements: `SET n.f = m:L`, a
  `CREATE` or `MERGE` property) when the variable is a node the statement itself
  created or a null node from an `OPTIONAL MATCH`. On a node the `MATCH` found
  it works there, and in a `MATCH … SET` value it works, null included.
  `labels(n)` has the same per-row limit.

### List subscripts

A subscript reads one element of a list-valued property or expression. A
negative index counts from the end.

```cypher
MATCH (n:Place) RETURN n.location[0] AS lat, n.location[1] AS lon
MATCH (n:Place) WHERE n.tags[-1] = 'primary' RETURN key(n)
```

Out of range, a non-list base and a non-integer index all evaluate to `null` —
never an error, and never a match. An unaliased subscript names its own column
(`n.location[0]`), so two subscripts of one list are two distinct columns. A
write-statement `RETURN` evaluates a subscript the same way a read `RETURN` does.

### Examples

```cypher
MATCH (n:Person) RETURN toLower(n.name)
MATCH (n:Person) RETURN size(n.tags)
MATCH (n:Person) RETURN coalesce(n.nickname, n.name)
MATCH (a:Person)-[r]->(b:Person) RETURN type(r)
MATCH (n:Person) RETURN abs(n.score)
MATCH (n:Measurement) RETURN round(n.value)
MATCH (n:Person) RETURN key(n) AS id, labels(n) AS kinds
```

---

## Cypher coverage

Tested against the current binary (2026-08-24, release build, maturin develop --release);
the forms below — node key and label, comma-separated patterns, infix string
predicates, list subscripts, `DISTINCT` aggregates, `WITH` alias scoping — are covered by the
workspace test suite. Classification: **Supported** = executes without error; **Named-error** =
rejected with a clear, actionable message; **Absent** = not implemented (not tested here).

### Supported

| Form | Example |
|---|---|
| `MATCH (n:Label)` | `MATCH (n:Person) RETURN n.name` |
| `MATCH (n {key: val})` inline property filter | `MATCH (n:Person {city: 'Austin'}) RETURN n` |
| `n.key` / `n.id` / `n.label` / `key(n)` / `id(n)` / `labels(n)` | `MATCH (n:Person) WHERE n.id = 'alice' RETURN n.id, id(n), labels(n)` — `n.key`/`n.id`/`n.label` stored-wins; `key(n)`/`id(n)` always the id-map key |
| Comma-separated patterns in one `MATCH` | `MATCH (t)-[:A]->(c), (t)-[:B]->(c) RETURN key(c)` — the same rows as consecutive `MATCH` clauses, except that the parts of one `MATCH` never bind one relationship twice; no shared variable is a product |
| `STARTS WITH` / `ENDS WITH` / `CONTAINS` (infix) | `WHERE c.name STARTS WITH 'Acme'` — missing property is false, not an error |
| Comma-separated patterns in one `MATCH` | `MATCH (t)-[:A]->(c), (t)-[:B]->(c) RETURN key(c)` — identical to consecutive `MATCH` clauses; no shared variable is a product |
| `STARTS WITH` / `ENDS WITH` / `CONTAINS` (infix) | `WHERE c.name STARTS WITH 'Acme'` — missing property is null (never a match, under `NOT` too), not an error |
| List subscript `x[i]`, negative index | `RETURN n.location[0], n.tags[-1]` — out of range / non-list / non-integer index → null |
| `count(DISTINCT x)` / `collect(DISTINCT x)` | `WITH c, count(DISTINCT t) AS n RETURN key(c), n` |
| `WITH … WHERE <alias>` (non-aggregate) | `WITH t, t.age AS x WHERE x > 11 RETURN key(t), x` — the alias is in scope for WHERE, ORDER BY, SKIP and LIMIT |
| Aggregate `WITH` projecting its `RETURN` | `WITH c, COUNT(p) AS n RETURN key(c), n * 2 AS d` — projects with or without a HAVING clause |
| `MATCH (n)-[r:TYPE]->(m)` directed edge | `MATCH (a)-[:KNOWS]->(b) RETURN a` |
| `MATCH (n)-[r:TYPE]-(m)` undirected edge | `MATCH (a)-[:KNOWS]-(b) RETURN a` |
| `MATCH (n)-[r]->(m)` any relationship type | `MATCH (n)-[r]->(m) RETURN type(r)` |
| `WHERE =, <>, <, >, <=, >=` | `WHERE n.age > 18` |
| `WHERE n.prop IN [a, b]` / `IN $list` | `WHERE n.city IN ['Austin', $c]`; `$cities` is `Value::List`; a list element is compared whole (`n.tags IN [[1, 2]]`), see [Comparisons and boolean operators](#comparisons-and-boolean-operators) |
| `RETURN DISTINCT` | `RETURN DISTINCT n.city` — hashed after Project; numeric Int/Float unify like grouping; cap 1,000,000 distinct rows |
| `WHERE AND / OR / XOR` | `WHERE n.a = 1 AND n.b = 2`, `WHERE n.a XOR n.b` |
| Comparisons and boolean operators as values | `RETURN n.age >= 18 AS adult`, `[x IN l \| x > 1]`, `SET n.flag = n.score > 10`, `coalesce(n.a IS NULL, false)`, `CASE WHEN … THEN n.a > 1 END` — every expression position; three-valued (`RETURN null > 1` is null, `NOT null` is null, `null IS NULL` is true); see [Comparisons and boolean operators](#comparisons-and-boolean-operators) |
| `ORDER BY prop ASC / DESC` | `ORDER BY n.name DESC` |
| `SKIP n LIMIT n` | `SKIP 10 LIMIT 5` |
| `$param` in WHERE | `WHERE n.age > $min` |
| `$param` in SET | `SET n.score = $val` |
| `$param.field` (a map parameter's entry) | `WHERE n.id = $p.id`, `CREATE (:P {id: $p.id})` — `$p['id']` too; an absent key is null |
| `COUNT(*)`, `COUNT(var)`, `SUM`, `AVG`, `MIN`, `MAX` (single) | `RETURN COUNT(*), SUM(n.score)` |
| Grouped aggregation (single or multiple keys, multiple aggs) | `RETURN n.city, COUNT(*) AS c ORDER BY c DESC` |
| `WITH` projection + aliasing | `WITH n.name AS nm RETURN nm` |
| `WITH … WHERE` (HAVING) | `WITH city, COUNT(*) AS c WHERE c > 2 RETURN city` |
| `WITH … ORDER BY … LIMIT` | `WITH n, n.age AS a ORDER BY a DESC LIMIT 5 RETURN n.name` |
| `WITH … MATCH` re-entry | `WITH city MATCH (p:Person) WHERE p.city = city RETURN p` |
| `UNWIND list_literal AS x` | `UNWIND [1, 2, 3] AS x RETURN x` |
| `UNWIND n.prop AS x` (list-valued property) | `UNWIND n.tags AS tag RETURN tag` |
| `UNWIND expr AS x` (any list-valued expression) | `UNWIND range(1, 5) AS i RETURN i` |
| `UNWIND alias AS x` (from prior WITH) | `WITH n.tags AS ts UNWIND ts AS t RETURN t` |
| `UNWIND … WHERE` (filter after UNWIND) | `UNWIND [1,2,3] AS x WHERE x > 1 RETURN x` |
| `UNWIND … MATCH` / `OPTIONAL MATCH` / `WITH` (clauses after `UNWIND`) | `UNWIND $ids AS id MATCH (n:Person {id: id}) RETURN n.name` |
| `r.field` on a map element | `UNWIND $rows AS r RETURN r.id, r.name` |
| `UNWIND … CREATE` (bulk load) | `UNWIND $rows AS r CREATE (:Person {id: r.id, name: r.name})` — one batch, atomic |
| `UNWIND … MERGE` | `UNWIND $rows AS r MERGE (n:Person {id: r.id}) ON CREATE SET n.at = r.at` — a row sees the rows before it; node and relationship forms |
| `UNWIND … MATCH … SET` / `SET n += r` / `REMOVE` / `DELETE` / `DETACH DELETE` | `UNWIND $rows AS r MATCH (n {id: r.id}) SET n += r` |
| `OPTIONAL MATCH` (left-outer-join) | `MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN a, b` |
| A leading `OPTIONAL MATCH` | `OPTIONAL MATCH (n:Person {id: 'x'}) RETURN n` — one row of nulls when nothing matches |
| `RETURN *` / `WITH *` | `MATCH (p:Person)-[r:KNOWS]->(q) RETURN *` — the node, path and value names in scope, in byte order of name (here `p`, `q`); see [RETURN * and WITH *](#return--and-with-) |
| Label predicates `n:Label`, `r:TYPE` | `MATCH (n) WHERE n:Person OR n:Company RETURN n.id, n:Person AS person` — see [Label predicates](#label-predicates) |
| `MATCH … CREATE` / `MATCH … MERGE` | `MATCH (a {id: 'x'}), (b {id: 'y'}) CREATE (a)-[:K]->(b)`, `MATCH (a), (b) MERGE (a)-[:K]->(b)` — once per matched row; see [MATCH … CREATE / MATCH … MERGE](#match--create--match--merge) |
| `OPTIONAL MATCH … WHERE` (filter in optional scope) | `OPTIONAL MATCH (a)-[r]->(b) WHERE b.score > 5` |
| Multiple chained `OPTIONAL MATCH` | `OPTIONAL MATCH (a)-[:R]->(b) OPTIONAL MATCH (a)-[:S]->(c)` |
| `OPTIONAL MATCH` + aggregation | `OPTIONAL MATCH (a)-[r]->(b) RETURN a, COUNT(r)` |
| Variable-length `*1..n`, `*n`, `*..n`, bare `*` | `MATCH (a)-[r:T*1..5]->(b) RETURN r.length` |
| `shortestPath((a)-[r*..n]->(b))` | `MATCH (a {id: 'x'}), (b {id: 'y'}) MATCH shortestPath((a)-[r*..5]->(b)) RETURN r.length` |
| Named path `p = (a)-[:T*1..n]->(b)` | `MATCH p = (a)-[:T*1..3]->(b) RETURN p.length, p.nodes` |
| `CREATE (n:L {id: 'x', …})` | `CREATE (n:Person {id: 'alice', age: 30})` |
| `CREATE (n:L {id: 'x', …})` | `CREATE (n:Person {id: 'alice', age: 30})` — a value may be any expression (`{flag: 1 > 0, n: 2 + 3}`); a `MERGE` matches against the evaluated value (`MERGE (n:L {id: 'a' + 'b'})`). A literal-only value that evaluates to null is a named error |
| `$param` in `CREATE` / `MERGE` property maps | `CREATE (:Person {id: $id, name: $p.name})`, `CREATE (n:Person $props)`, `MERGE (n:Person {id: $id}) ON CREATE SET n += $props` — evaluated against the parameters (the per-row form over one row); a null parameter value is left out, a null `MERGE` key is an error |
| `CREATE (a:L {id: 'x'})-[:T]->(b:L {id: 'y'})` | node-edge chain |
| `MATCH … SET n.prop = literal` | `MATCH (n) WHERE n.id = 'x' SET n.score = 99` |
| `MATCH … SET n.prop = $param` | `MATCH (n) WHERE n.id = $id SET n.score = $val` |
| `MATCH … SET … RETURN` | `MATCH (n {id:'a'}) SET n.x = 2 RETURN n.x` — write commits, then RETURN from post-write state. `CASE` and list subscripts (`n.tags[0]`) evaluate the same as a read RETURN; `n.key` / `n.id` / `n.label` use identity fallback |
| `MATCH … SET n += {…}` / `SET n = {…}` | `MATCH (n) WHERE n.id = 'x' SET n += {score: 2, old: null}` — merge (null removes) or replace; `id`, `ns` and view-owned properties are never removed |
| `MATCH … REMOVE n.prop [, …]` | `MATCH (n) WHERE n.id = 'x' REMOVE n.score, n.nickname` — a missing property is a no-op; `RETURN` projects the post-write state |
| `MERGE … ON CREATE SET` / `ON MATCH SET` | `MERGE (n:L {id:'new'}) ON CREATE SET n.born = 1 ON MATCH SET n.hit = 1 RETURN n` |
| `MATCH … DELETE r` (manual edge) | `MATCH (a)-[r:KNOWS]->(b) DELETE r` |
| `MATCH … DETACH DELETE n` | `MATCH (n) WHERE n.id = 'x' DETACH DELETE n` |
| `MATCH … DELETE n` (isolated node) | `MATCH (n:Tmp) WHERE n.id = 'x' DELETE n` |
| `MERGE (n:L {id: 'x'})` (single-key upsert) | `MERGE (n:Person {id: 'alice'})` |
| `MERGE (a:L {id: 'x'})-[:T]->(b:M {id: 'y'})` | `MERGE (a:Person {id: 'alice'})-[:KNOWS]->(b:Person {id: 'bob'})` — find-or-create both nodes and the edge; idempotent; `ON CREATE SET` / `ON MATCH SET` may name `a` or `b` |
| `MERGE (n:L {id: 'x'}) RETURN …` | `MERGE (n:Person {id: 'alice'}) RETURN n` — returns node whether created or matched |
| `CREATE … RETURN …` | `CREATE (n:Person {id: 'alice'}) RETURN n.id, id(n), n.key` — single-statement create + projection |
| `WHERE … IS NULL / IS NOT NULL` | `WHERE n.score IS NULL`, `WHERE b IS NOT NULL` — null-check predicate; composes with AND/OR |
| Boolean literals `true` / `false` (any case) | `WHERE n.active = true`, `WHERE n.active <> false`, `MATCH (n {active: true})`, `SET n.active = false`, `RETURN true AS t` — a node with no such property matches neither `= …` nor `<> …`. `true` and `false` are reserved: naming a variable or an alias either one is a parse error. A property or a map key may still be called that (`n.true`) |
| `null` literal (any case) | `RETURN null AS v`, `WHERE null IS NULL` — evaluates to null; `null = null` is null, so a WHERE drops the row. `null` is reserved like `true` / `false`; `n.null` and a backtick-quoted `` `null` `` key are still names |
| List and map literals | `RETURN [1, n.x, 'a'] AS l, {a: 1, b: [2, 3]} AS m` — elements and values are any expression; `` {`c d`: 1} `` quotes a key. A map's keys come back sorted, not in written order: `keys({b: 1, a: 2})` is `['a', 'b']` |
| Null inside a list or map | `RETURN [1, null] AS l`, `[n.x, n.missing]`, `{a: null}` — the element stays null in every output (Rust `Value::Null`, JSON `null`, Python `None`, TS `null`, Arrow text `[1, null]`). `size([null]) = 1`, `head([null, 1])` is null, `keys({a: null})` is `['a']`. Equality is three-valued: `[1, null] = [1, null]` is null (the row is dropped), `[1, null] = [2, null]` is false. A stored property cannot hold one (see Named-error) |
| List comprehensions | `RETURN [x IN n.tags WHERE x <> 'old' \| x] AS active`, `[x IN [1, 2, 3] \| x * 10]`, `[x IN n.scores WHERE x > 0]` — `WHERE` and `\| expr` are each optional (no `\| expr` keeps the element). Usable in `WHERE`, `RETURN`, `ORDER BY`, `UNWIND` and as a function argument, and they nest. The variable exists only inside the comprehension and shadows an outer variable of the same name there. A null list (or a non-list) gives null; a null element is a real element, so it passes through `\| expr`, and a `WHERE` that is null drops it. Not supported in the `RETURN` of a write statement (see Named-error) |
| `reduce()` | `RETURN reduce(total = 0, x IN n.scores \| total + x) AS sum`, `reduce(s = '', w IN n.words \| s + w)` — `reduce(acc = init, x IN list \| expr)`. `init` and `list` are read in the enclosing scope; `acc` and `x` exist only inside `expr` and shadow outer names there. An empty list gives `init`; a null list (or a non-list) gives null; a null `expr` value makes the accumulator null for the next step. Usable anywhere an expression is, and it nests with itself and with list comprehensions. Not supported in the `RETURN` of a write statement (see Named-error) |
| Query without `MATCH` | `RETURN 1 + 1 AS x`, `UNWIND [1, 2, 3] AS x RETURN x`, `WITH 1 AS a RETURN a` — reads no graph and runs against one empty row; `UNWIND [] AS x` gives no rows |
| Binary arithmetic (`+`, `-`, `*`, `/`) in RETURN, WHERE, SET, function args | `RETURN n.age + 1 AS next`, `WHERE n.score * 2 > 10`, `SET n.x = n.x + 1` — precedence: `*`/`/` over `+`/`-`; parentheses supported; null propagates; integer div by zero is a named error; `+` also joins two strings (`'a' + 'b'`), two lists (`[1] + [2]`), a list and an element (`[1] + 2`, `0 + [1]`), and a number with a string as the number's `toString()` text (`1 + 'a'` is `'1a'`, `'a' + 1.5` is `'a1.5'`); the same in a write statement's `RETURN`; other operators on those types are a type error |
| Scalar functions (`toLower`, `size`, `contains`, `startsWith`, `endsWith`, `toInteger`, `toFloat`, `toString`, `decay`, `key`, …) | `RETURN abs(n.score), toString(n.weight)` |
| `collect(x)` aggregation | `MATCH (c:City)<-[:IN]-(p:Person) RETURN c.name, collect(p.name) AS residents` |
| `stDev` / `stDevP` / `percentileCont` / `percentileDisc` | `MATCH (n:Reading) RETURN n.sensor, stDev(n.value) AS spread, percentileDisc(n.value, 0.95) AS p95` |
| `CASE WHEN … THEN … [ELSE …] END` | `RETURN CASE WHEN n.age >= 65 THEN 'senior' ELSE 'other' END AS band` — also in `MATCH … SET … RETURN` |
| `UNION` / `UNION ALL` | `MATCH (a:A) RETURN a.id AS id UNION MATCH (b:B) RETURN b.id AS id` |
| Multi-relationship-type `[:A\|:B]` | `MATCH (a)-[:KNOWS\|:LIKES]->(b) RETURN b` |
| View-maintained properties queryable like any property | `MATCH (c:City) WHERE c.pop > 1000 RETURN c.name` — `pop` is a degree view maintained incrementally; reads like a stored prop |
| `CREATE INDEX [name] [IF NOT EXISTS] FOR (n:L) ON (n.p)` / `DROP INDEX name [IF EXISTS]` | `CREATE INDEX idx_age FOR (n:Person) ON (n.age)` — a range index; unnamed is `idx_<L>_<p>`; `WHERE` ranges, `STARTS WITH` and `ORDER BY … LIMIT` on it read the index; see [Schema statements](#schema-statements-ddl) |
| `CREATE CONSTRAINT name [IF NOT EXISTS] FOR (n:L) REQUIRE n.p IS UNIQUE` / `DROP CONSTRAINT name [IF EXISTS]` | `CREATE CONSTRAINT uniq_email FOR (u:User) REQUIRE u.email IS UNIQUE` |
| `SHOW INDEXES` / `SHOW CONSTRAINTS` | `SHOW INDEXES` — a read; columns `name`, `type`, `label`, `property`, `state`; a scoped read lists only labels it can see |
| `CALL ns.name(args) [YIELD …] [WHERE …] [RETURN …]` | `CALL db.ping() YIELD success AS ok RETURN ok` — a read statement of its own, on every read entry point; see [Procedures](#procedures-call) |
| `textMatches(n.field, "query")` in WHERE (per-row scratch scan) | `MATCH (a:Article) WHERE textMatches(a.bio, 'rust embedded') RETURN a.key` — correct for any graph size; O(scan) per row. Prefer `db.search()` for large indexed fields. |

### Named-error

Forms rejected with a clear, actionable error message (executor returns a typed error; no silent misbehavior).

| Form | Error message (excerpt) |
|---|---|
| Bare `RETURN r` for a relationship variable | `cannot return relationship variable '…' bare; return its properties (….field) instead` |
| `MERGE (n:L {id: 'x', extra: 2})` (multi-property other than `ns`) | `MERGE supports exactly one key property (got 2)` |
| `MERGE (a)-[:T]-(b)` (undirected), `-[:T*1..3]->`, `-[:A\|B]->`, a second relationship | `not supported in mushroomdb 0.10: MERGE with an undirected relationship` (and its variable-length, alternative-types and more-than-one-relationship siblings) |
| `MERGE (n:L {id: 'x'})` by a role bound to two namespaces | `role-bound token: MERGE create requires the role to name one namespace` |
| `DELETE n` when node has incident edges | `Cannot delete node … because it still has incident edges. Use DETACH DELETE…` |
| `DELETE r` on derived (rule-owned) edge | `cannot delete derived edge; retract via the rule or change the property` |
| `CREATE (a), (b)` comma-separated form | `parse error: unexpected tokens after CREATE pattern (found Comma)` |
| Variable-length `*0..n` (zero-length min) | `zero-length variable-length paths are not supported; minimum hop count is 1` |
| Variable-length `*n..` (unbounded upper) | `variable-length paths are capped at 10 hops` |
| A `MATCH`, `UNWIND` or `WITH` after a write clause of an `UNWIND` statement | `not supported in mushroomdb 0.10: a MATCH, UNWIND or WITH clause after a write clause in the same statement; put the reads first` |
| `UNWIND … MATCH (a)-[r]->(b) DELETE r` | `not supported in mushroomdb 0.10: DELETE of the relationship `r` after UNWIND; delete nodes, or use MATCH … DELETE` |
| `UNWIND … SET n += x` where `x` is not a map | `SET n += <map>: the value must be a map, got …` |
| `UNWIND scalar` (non-list value) | `execute: UNWIND requires a list; got … value for …` |
| Multi-statement / unknown top-level keyword | `parse error: expected MATCH, OPTIONAL MATCH, UNWIND, WITH or RETURN (found …)` |
| `RETURN *` / `WITH *` with no node, path or value name in scope | `RETURN * / WITH * with no variables in scope` |
| `RETURN *` after a `CALL` with no `YIELD` list | `RETURN * after CALL needs a YIELD list; name the columns with YIELD` |
| A label test on a value (`UNWIND [1] AS x RETURN x:L`) | ``x:L: `x` is not a node or a relationship`` |
| Storing a list or map property that holds a null | ``a property value cannot contain null: property `…` of node `…` holds a null`` — `CREATE (:L {id: 'x', p: [1, null]})`, `SET n.p = [1, n.missing]` or a `$param` holding one; nothing is written. `SET n.p = null` keeps its own error (`SET value for n.p evaluated to null`) |
| List comprehension in the `RETURN` of a write statement | `a list comprehension is not supported in the RETURN of a write statement; compute it in a read query, or RETURN the list and filter it afterwards` — refused before anything is written. In a `SET` value or a `WHERE` it works |
| `reduce()` in the `RETURN` of a write statement | `reduce() is not supported in the RETURN of a write statement; compute it in a read query, or RETURN the list and fold it afterwards` — refused before anything is written. In a `SET` value or a `WHERE` it works |
| `shortestPath` with unbound endpoints | `plan: shortestPath: source node … is not bound; bind both endpoints before shortestPath` |
| Unknown function name | `execute: unknown function …; supported: toLower, toUpper, size, coalesce, type, abs, round, textMatches, contains, startsWith, endsWith, toInteger, toFloat, toString, decay, key, id, labels` |
| `STARTS` / `ENDS` without `WITH` | `expected WITH after STARTS` |
| `count(DISTINCT *)` | named parse error — `DISTINCT` needs an expression, not `*` |
| Unknown name in a `WITH … WHERE` | ``unbound variable `nope` in WHERE`` |
| `$param` referenced but not supplied | `execute: missing parameter …` |
| `SET n.prop = n.other` (bare property-to-property copy) | `SET RHS: bare property/variable reference is not supported; use a literal, $parameter, or arithmetic expression` |
| Integer division by zero | `execute: division by zero` |
| Writing to a view-maintained property | `property is managed by view "…" and cannot be written directly` |
| Any write on an `open_at` (as-of) instance | `as-of instances are read-only` |
| A schema statement with another clause (`CREATE INDEX … RETURN 1`) | `parse: … a schema statement is the whole statement; unexpected tokens` |
| `CREATE` / `DROP INDEX` or `CONSTRAINT` inside a transaction | `a schema statement (CREATE/DROP INDEX, CREATE/DROP CONSTRAINT) cannot run inside a transaction; run it on its own, outside BEGIN … COMMIT` |
| `CREATE` / `DROP INDEX` or `CONSTRAINT` by a role-bound token | `role-bound token: schema statements (CREATE/DROP INDEX, CREATE/DROP CONSTRAINT) are not permitted` |
| A range index named `eq_…` or `ft_…` | `index names beginning 'eq_' or 'ft_' are reserved: SHOW INDEXES lists equality and full-text indexes under them; …` |
| `DROP INDEX` of a listed name two indexes share | `'eq_A_b_c' names more than one index: equality (A, b_c), equality (A_b, c); drop the one you mean with disable_index(label, field) or disable_fulltext(label, field)` |
| A composite index or constraint, or a constraint other than `IS UNIQUE` | `an index covers one property; composite indexes are not supported`, `only IS UNIQUE constraints are supported` |
| An unknown procedure, or `YIELD` of a column it does not return | `execute: unknown procedure: <ns>.<name>`, ``execute: YIELD `<field>`: <ns>.<name> returns no such column; it returns: …`` |
| `FOREACH`, `EXISTS { }` / `COUNT { }` / `COLLECT { }` and `CALL { }` subqueries, `CALL` inside a larger query, `SET n:Label`, more than one label, pattern predicates and comprehensions, map projections, `=~`, `LOAD CSV`, `USE`, `allShortestPaths` | `not supported in mushroomdb 0.10: <feature>` — the full list is under [Unsupported features](#unsupported-features) |

### Absent

Forms not implemented and not tested. Behavior is not guaranteed — these may produce an
unexpected-token parse error or other unspecified result.

| Form |
|---|
| Chained comparisons `a < b < c` (a parse error) |
| Composite and full-text index DDL, `IS NOT NULL` / `IS NODE KEY` constraints, `SHOW … YIELD` |

---

## Limitations

| Feature | Status |
|---|---|
| Multi-statement transactions | Supported — BEGIN … COMMIT through `SharedDb::begin_transaction` (and Bolt `BEGIN` / `COMMIT` / `ROLLBACK`, see [Bolt](bolt.md)); snapshot isolation, one atomic commit; every write statement runs inside a transaction with the results and counts it gives auto-commit (`MATCH … CREATE` / `MERGE`, `UNWIND … <write>`, the `SET` map forms and `SET` / `REMOVE … RETURN` included), under HTTP `/tx` and Bolt `BEGIN` alike, so also in driver-managed write transactions; schema statements are refused there |
| Combined MATCH … SET … RETURN | Supported — write commits, then RETURN projects from post-write state |
| SET RHS expressions | Literals, `$param` references, arithmetic expressions (`n.x + 1`, `n.score * 1.5`) and predicates (`n.score > 10`, `NOT n.done`); bare property-to-property copy (`SET n.x = m.y`) is a named error |
| MATCH … DETACH DELETE (node) | Supported — removes node + all edges |
| Bare DELETE on node with edges | Error — use DETACH DELETE |
| MERGE ON CREATE / ON MATCH | Supported in the same MERGE / write batch |
| Grouped aggregation | Supported (multiple keys and multiple aggregates allowed; group count capped at 1,000,000) |
| WITH pipeline stages | Supported — projection, aliasing, WHERE (HAVING, and an alias the stage itself introduced), ORDER BY, LIMIT, and re-entry MATCH |
| Multiple patterns in one MATCH | Supported, comma-separated — as consecutive MATCH clauses, except that the parts of one MATCH never bind one relationship twice. Still a named error after CREATE |
| `n.key` / `n.id` / `key(n)` / `id(n)` equality vs lit/`$param` | ScanKey (plus stored-wins). `n.label` / `labels(n)` are not this fold — they still scan the label |
| UNWIND | Supported — any list-valued expression (list literals, `range(…)`, parameters, list-valued properties, aliases from prior WITH); non-list → named error |
| UNWIND with write clauses | Supported — `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE` per row, one atomic batch; not in a transaction; reads before writes |
| Declaring a unique constraint | Scans the label's existing nodes, so it blocks writers for a time proportional to the label's node count (about 1 to 3 seconds at a million nodes); readers are unaffected. Refused, listing the first 100 conflicting nodes and the total, when two already hold equal values |
| A store whose WAL declares a unique constraint after data that breaks it | Opens normally: replay never checks a record, and a snapshot, a reader's refresh and `open_at` all load it the same way. The duplicates stay as they are. The constraint is in force for new writes: only a value a write changes is judged, so a write that leaves a duplicate's value as it is (another property, or the same value written again) is allowed, and one that gives a node a value another live node holds is refused. To clean up, give all but one holder of each shared value a new value, or delete them; the constraint then holds for the whole label. Only a store written by a build without the create-time scan can be in this state |
| Variable-length paths: max hops | Capped at 10 |
| shortestPath with unbound endpoints | Rejected at planning time |
| Intermediate result budget | 1,000,000 rows (WITH/UNWIND intermediate rows count against this cap) |
