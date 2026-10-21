# Cypher Query Reference

mushroomdb supports a Cypher subset for pattern matching, filtering,
aggregation, ordering, and writes (`CREATE`, `SET`, `DELETE`, `DETACH DELETE`,
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
MATCH (a:N {id: 'alice'}) MATCH (b:N {id: 'bob'})
MATCH shortestPath((a)-[r:T*..5]->(b))
RETURN r.length
```

Bind the two endpoints with **separate** `MATCH` clauses. Comma-separated
patterns in a single `MATCH` (`MATCH (a), (b)`) are not supported and fail to
parse.

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

- SET RHS must be a literal (integer, float, string, boolean), a `$param`
  reference, or arithmetic (`n.x + 1`); bare property-to-property copy is a named error.
- Multiple SET clauses in one statement are allowed.
- Combined MATCH … SET … RETURN is supported: the write commits, then RETURN
  projects from the post-write view.
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

- Match-or-create by a single key property. An optional `ns` names the
  namespace the create arm writes into (`MERGE (n:Doc {id: 'x', ns: 'tenant-a'})`).
- A role bound to exactly one namespace stamps that namespace when the
  pattern does not name `ns`. A role bound to two or more must name one.
- `ON CREATE SET` / `ON MATCH SET` apply inside the same write batch as the
  insert-or-skip. Both may appear on one MERGE.
- Other extra properties are not supported.

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
| `date([x])` | `String` or `Map` | `Date` | `date("2024-03-15")`, `date({year: 2024, month: 3, day: 15})`; no argument is today (UTC). A `LocalDateTime` or `DateTime` argument is cut to its date. Null, invalid or wrong-type input → null. A result cannot be stored until the V11 format ships |
| `localdatetime([x])` | `String` or `Map` | `LocalDateTime` | `localdatetime("2024-03-15T10:30:00")`; no argument is now (UTC wall clock). Null, invalid or wrong-type input → null. A result cannot be stored until the V11 format ships |
| `datetime([x])` | `String` or `Map` | `DateTime` | `datetime("2024-03-15T10:30:00+05:30")`; the offset is kept (within ±18:00), a string with no offset is read as UTC; no argument is now. Null, invalid or wrong-type input → null. A result cannot be stored until the V11 format ships |
| `duration(x)` | `String` or `Map` | `Duration` | `duration({days: 3, hours: 2})` (keys `years`, `months`, `weeks`, `days`, `hours`, `minutes`, `seconds`, `milliseconds`, `microseconds`, `nanoseconds`) or `duration("P1Y2M3DT4H")`. Null, invalid, unknown key or wrong-type input → null. A result cannot be stored until the V11 format ships |
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

Until the V11 format ships, a temporal value cannot be stored: `SET n.d =
date("2024-01-01")` is refused with a `not supported in mushroomdb 0.10 yet`
error naming the type.

Aggregations: `count`, `sum`, `avg`, `min`, `max`, and `collect(x)` (gather each
row's value into a list, per group when grouping keys are present). `count` and
`collect` accept `DISTINCT`.

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
non-string handling: a missing property is `false`, not an error.

```cypher
MATCH (c:Company) WHERE c.name STARTS WITH 'Acme'   RETURN c.name
MATCH (c:Company) WHERE c.name ENDS WITH 'Works'    RETURN c.name
MATCH (c:Company) WHERE NOT c.name CONTAINS 'Design' RETURN c.name
```

`STARTS` or `ENDS` without its `WITH` is a named parse error.

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
| Comma-separated patterns in one `MATCH` | `MATCH (t)-[:A]->(c), (t)-[:B]->(c) RETURN key(c)` — identical to consecutive `MATCH` clauses; no shared variable is a product |
| `STARTS WITH` / `ENDS WITH` / `CONTAINS` (infix) | `WHERE c.name STARTS WITH 'Acme'` — missing property is false, not an error |
| List subscript `x[i]`, negative index | `RETURN n.location[0], n.tags[-1]` — out of range / non-list / non-integer index → null |
| `count(DISTINCT x)` / `collect(DISTINCT x)` | `WITH c, count(DISTINCT t) AS n RETURN key(c), n` |
| `WITH … WHERE <alias>` (non-aggregate) | `WITH t, t.age AS x WHERE x > 11 RETURN key(t), x` — the alias is in scope for WHERE, ORDER BY, SKIP and LIMIT |
| Aggregate `WITH` projecting its `RETURN` | `WITH c, COUNT(p) AS n RETURN key(c), n * 2 AS d` — projects with or without a HAVING clause |
| `MATCH (n)-[r:TYPE]->(m)` directed edge | `MATCH (a)-[:KNOWS]->(b) RETURN a` |
| `MATCH (n)-[r:TYPE]-(m)` undirected edge | `MATCH (a)-[:KNOWS]-(b) RETURN a` |
| `MATCH (n)-[r]->(m)` any relationship type | `MATCH (n)-[r]->(m) RETURN type(r)` |
| `WHERE =, <>, <, >, <=, >=` | `WHERE n.age > 18` |
| `WHERE n.prop IN [a, b]` / `IN $list` | `WHERE n.city IN ['Austin', $c]`; `$cities` is `Value::List` |
| `RETURN DISTINCT` | `RETURN DISTINCT n.city` — hashed after Project; numeric Int/Float unify like grouping; cap 1,000,000 distinct rows |
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
| `MATCH … SET … RETURN` | `MATCH (n {id:'a'}) SET n.x = 2 RETURN n.x` — write commits, then RETURN from post-write state. `CASE` and list subscripts (`n.tags[0]`) evaluate the same as a read RETURN; `n.key` / `n.id` / `n.label` use identity fallback |
| `MERGE … ON CREATE SET` / `ON MATCH SET` | `MERGE (n:L {id:'new'}) ON CREATE SET n.born = 1 ON MATCH SET n.hit = 1 RETURN n` |
| `MATCH … DELETE r` (manual edge) | `MATCH (a)-[r:KNOWS]->(b) DELETE r` |
| `MATCH … DETACH DELETE n` | `MATCH (n) WHERE n.id = 'x' DETACH DELETE n` |
| `MATCH … DELETE n` (isolated node) | `MATCH (n:Tmp) WHERE n.id = 'x' DELETE n` |
| `MERGE (n:L {id: 'x'})` (single-key upsert) | `MERGE (n:Person {id: 'alice'})` |
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
| `CASE WHEN … THEN … [ELSE …] END` | `RETURN CASE WHEN n.age >= 65 THEN 'senior' ELSE 'other' END AS band` — also in `MATCH … SET … RETURN` |
| `UNION` / `UNION ALL` | `MATCH (a:A) RETURN a.id AS id UNION MATCH (b:B) RETURN b.id AS id` |
| Multi-relationship-type `[:A\|:B]` | `MATCH (a)-[:KNOWS\|:LIKES]->(b) RETURN b` |
| View-maintained properties queryable like any property | `MATCH (c:City) WHERE c.pop > 1000 RETURN c.name` — `pop` is a degree view maintained incrementally; reads like a stored prop |
| `textMatches(n.field, "query")` in WHERE (per-row scratch scan) | `MATCH (a:Article) WHERE textMatches(a.bio, 'rust embedded') RETURN a.key` — correct for any graph size; O(scan) per row. Prefer `db.search()` for large indexed fields. |

### Named-error

Forms rejected with a clear, actionable error message (executor returns a typed error; no silent misbehavior).

| Form | Error message (excerpt) |
|---|---|
| Bare `RETURN r` for a relationship variable | `cannot return relationship variable '…' bare; return its properties (….field) instead` |
| `MERGE (n:L {id: 'x', extra: 2})` (multi-property other than `ns`) | `MERGE supports exactly one key property (got 2)` |
| `MERGE (n:L {id: 'x'})` by a role bound to two namespaces | `role-bound token: MERGE create requires the role to name one namespace` |
| `DELETE n` when node has incident edges | `Cannot delete node … because it still has incident edges. Use DETACH DELETE…` |
| `DELETE r` on derived (rule-owned) edge | `cannot delete derived edge; retract via the rule or change the property` |
| `CREATE (a), (b)` comma-separated form | `parse error: unexpected tokens after CREATE pattern (found Comma)` |
| Variable-length `*0..n` (zero-length min) | `zero-length variable-length paths are not supported; minimum hop count is 1` |
| Variable-length `*n..` (unbounded upper) | `variable-length paths are capped at 10 hops` |
| `UNWIND scalar` (non-list value) | `execute: UNWIND requires a list; got … value for …` |
| Multi-statement / unknown top-level keyword | `parse error: expected MATCH, UNWIND, WITH or RETURN (found …)` |
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

### Absent

Forms not implemented and not tested. Behavior is not guaranteed — these may produce an
unexpected-token parse error or other unspecified result.

| Form |
|---|
| Pattern comprehension `[(n)-[r]->(m) | m.prop]` |
| `EXISTS { … }` subquery predicate |
| `CALL` procedure invocations |
| Schema operations (`CREATE INDEX`, `CREATE CONSTRAINT`) |

---

## Limitations

| Feature | Status |
|---|---|
| Multi-statement transactions | Not supported (one write statement per query in v1) |
| Combined MATCH … SET … RETURN | Supported — write commits, then RETURN projects from post-write state |
| SET RHS expressions | Literals, `$param` references, and arithmetic expressions (`n.x + 1`, `n.score * 1.5`); bare property-to-property copy (`SET n.x = m.y`) is a named error |
| MATCH … DETACH DELETE (node) | Supported — removes node + all edges |
| Bare DELETE on node with edges | Error — use DETACH DELETE |
| MERGE ON CREATE / ON MATCH | Supported in the same MERGE / write batch |
| Grouped aggregation | Supported (multiple keys and multiple aggregates allowed; group count capped at 1,000,000) |
| WITH pipeline stages | Supported — projection, aliasing, WHERE (HAVING, and an alias the stage itself introduced), ORDER BY, LIMIT, and re-entry MATCH |
| Multiple patterns in one MATCH | Supported, comma-separated — identical to consecutive MATCH clauses. Still a named error after CREATE |
| `n.key` / `n.id` / `key(n)` / `id(n)` equality vs lit/`$param` | ScanKey (plus stored-wins). `n.label` / `labels(n)` are not this fold — they still scan the label |
| UNWIND | Supported — list literals, list-valued properties, and scalar aliases from prior WITH; non-list → named error |
| Variable-length paths: max hops | Capped at 10 |
| shortestPath with unbound endpoints | Rejected at planning time |
| Intermediate result budget | 1,000,000 rows (WITH/UNWIND intermediate rows count against this cap) |
