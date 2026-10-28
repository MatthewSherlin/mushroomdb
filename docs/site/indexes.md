# Property indexes

Equality indexes answer exact scalar lookups. Range indexes also answer
ordered bounds, string prefixes and eligible ordered top-k queries. Both
maintain postings as data changes; declaring one preserves query results.

| Index | Declare through | Query uses |
|---|---|---|
| Equality | `enable_index`, schema file, HTTP | exact equality and compound equality intersection |
| Range | Cypher `CREATE INDEX`, `enable_range_index`, HTTP | equality, ordered bounds, `STARTS WITH`, eligible `ORDER BY … LIMIT` |

Indexable scalars are Int, Float, Str, Bool, Date, LocalDateTime, DateTime and
Binary. Lists, maps and Duration have no index key. Binary equality is
supported; Binary range-query acceleration is outside the advertised
guarantee. See [Property value types](types.md).

## Declare and inspect

A named or anonymous Cypher index is a **range index**:

```sh
mushroomdb query index-demo "CREATE INDEX by_age FOR (n:Person) ON (n.age)"
mushroomdb query index-demo "CREATE INDEX FOR (n:Person) ON (n.score)"
mushroomdb schema indexes index-demo
```

```text
name             | type  | label  | property | state
by_age           | range | Person | age      | online
idx_Person_score | range | Person | score    | online
```

The equivalent listing is `SHOW INDEXES`; its columns are `name`, `type`,
`label`, `property` and `state`. Type is `range`, `equality` or `fulltext`;
state is always `online`, since declaration builds the index synchronously.
Rows are sorted by name.

```cypher
CREATE INDEX by_age IF NOT EXISTS FOR (n:Person) ON (n.age)
SHOW INDEXES
DROP INDEX by_age IF EXISTS
```

Without a name, the default is `idx_<Label>_<property>` and redeclaring that
label/property pair is a no-op. A named duplicate is an error unless `IF NOT
EXISTS` applies. `DROP INDEX name` refuses a missing name; `IF EXISTS` makes
that a no-op. Each schema statement is the whole statement, with no trailing
`RETURN`. See [Cypher DDL](query.md#schema-statements-ddl) for collisions and
scope rules.

For an equality index, retain the existing schema-file form:

```json
{ "indexes": [["Person", "city"], ["Company", "status"]] }
```

Or use the Rust API on an open `db`:

```rust
db.enable_index("Person", "city")?;
db.enable_range_index("by_age", "Person", "age")?;
```

[CLI schema commands](cli.md#schema) list and apply schemas. The
[HTTP schema reference](http.md#schema-indexes-and-constraints) documents the
actual list/create/delete routes and accepted bodies. Equality and full-text
indexes are not created by Cypher `CREATE INDEX`.

## Equality predicates

A declared equality or range index can answer a non-`id` property-map lookup
such as `MATCH (n:Person {city:'London'})` or a single `WHERE n.city = 'London'`.
Compound equalities on the same scan variable can intersect indexed node-ID
lists; unindexed fields remain residual filters. With no eligible index the
query scans. The `id` pseudo-property uses the identity map instead.
Cross-expand pushdown is not supported.

## Range predicates and ordering

With a range index on `(Person, age)`, these use supported predicate syntax:

```cypher
MATCH (n:Person) WHERE n.age >= 30 AND n.age < 50 RETURN n
MATCH (n:Person) WHERE n.age < 50 RETURN n
MATCH (n:Person) WHERE n.age <= 50 RETURN n
MATCH (n:Person) WHERE n.age > 30 RETURN n
MATCH (n:Person) RETURN n.age ORDER BY n.age DESC LIMIT 10
```

Either side may hold the property (`30 < n.age`); bounds may be literals or
parameters. A Str range index also supports `n.name STARTS WITH 'Ad'`.
`BETWEEN` is not supported: use two bounds joined by `AND`.

Eligible `ORDER BY n.prop [DESC] … LIMIT` (with optional `SKIP`) has no WHERE,
or only range/prefix bounds on that same property. It reads from the ordered
index and stops once the required rows and all boundary ties are known. Tie
order and null placement match the scan: missing values sort last ascending,
first descending. Unfiltered DESC also walks the label once to find missing
values. An ascending query that reaches beyond numbers, strings and booleans
can also walk the label; descending `STARTS WITH` reads all prefix matches.

Temporal bounds range within their own type; Int and Float compare as
numbers. Bool inequality bounds use a scan, where `false < true`; the range
index is not read for them. Duration, list or null bounds also fall back to
the scan, but have no ordered matches. A range declaration does not create
an ordered Duration key or change those comparisons.

Transactions, readers with an unfolded recent-commit layer, pseudo-properties
`key`/`id`/`label`, and cross-relationship query shapes may scan. Declaring a
range index is not a promise that every query avoids scanning. The complete
eligibility rules are in [the query reference](query.md#schema-statements-ddl).

## Persistence and ownership

Declarations survive WAL replay and snapshots. Postings rebuild from live
properties on open. Equality-only declarations preserve the older format;
the first range-index declaration opts the store into sticky V11. Removing
the last range index does not downgrade it. See
[Format stability](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/format-stability.md#v11-is-opt-in-and-sticky).

An equality index used by a [unique constraint](constraints.md) cannot be
dropped while the constraint owns it. Dropping the constraint leaves that
index in place. `SHOW INDEXES` names equality indexes `eq_<Label>_<property>`
and full-text indexes `ft_<Label>_<property>`; those prefixes are reserved for
those kinds and cannot name a range index.

## Relationship to the vector index

A property index answers equality; the vector index answers nearness. It is not
declared — it is built by a [VectorSimilar rule](rules.md#6-vectorsimilar) over
its two sides. Its shape, the `MUSHROOMDB_HNSW_PARAMS` override, its memory per
indexed vector and the recall its gates assert are documented under
[Vector index parameters](rules.md#vector-index-parameters).

From 0.6.6 **both** kinds of VectorSimilar rule take their candidates from it: an
`approximate: true` rule in one beam pass at `k`, an `approximate: false` rule by
widening the beam — doubling from `ef_search` (400 by default) up to a ceiling of
4,096 — and then re-scoring every candidate exactly, which is why its scores are
exact and its candidate set carries a committed recall floor of 0.98.

For an exact rule the index only ever *narrows* the candidates when a beam comes
back full with its worst hit below `min`, which is the one case that proves what
the beam left out. A beam that comes back short of its own width (layer 0 need
not be one connected component), or that reaches the ceiling with its worst
hit still at or above `min`, falls back to every vector on that side — so a dense
cluster above `min` costs a scan and never costs recall. The same path serves an
`All([VectorSimilar, …])` rule, whose candidates are that set intersected with
the other conjuncts' index lookups. `MUSHROOMDB_VECTOR_SCAN=1` restores the
O(n²) full scan for a caller who needs every pair above `min` provably found.

Query-time `find_similar` is a different path from rule derivation: brute
`find_similar` is exact GEMM, HNSW is still the approximate path, and
`exact=True` (or a `where=` predicate) forces GEMM.

## Relationship to fulltext

Use a **property index** for exact scalar equality (`city = 'austin'`). Use a
[fulltext index](fulltext.md) for tokenized text search (BM25, stemming, phrase
and prefix queries) over free-text fields.
