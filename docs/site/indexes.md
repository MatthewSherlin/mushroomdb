# Property (equality) indexes

An equality index makes `MATCH (n:Label {field: value})` a direct lookup
instead of a full scan of every node with that label. It is the right tool for
selective property filters — `WHERE`-style equality on an id, email, city,
status, or any scalar field — once a label holds more than a few thousand nodes.

Only **scalar** values are indexable (`Int`, `Float`, `Str`, `Bool`). List- and
map-valued properties are skipped: a list has no single equality key.

## Declaring an index

Via a schema file (idempotent — re-applying is a no-op):

```json
{ "indexes": [["Person", "city"], ["Company", "status"]] }
```

```bash
mushroomdb schema apply mydb.db schema.json
# → created index:Person.city
```

Or from the embedded API:

```rust
db.enable_index("Person", "city")?;
// db.disable_index("Person", "city")?;
// db.is_index_enabled("Person", "city");
```

Over HTTP, apply a schema through `POST /query` / the schema endpoint the same
way you declare rules or fulltext fields.

## Semantics

- **Maintained incrementally.** Every insert, `SET`, and delete updates the
  index; you never rebuild it by hand.
- **Rebuilt on open.** The *declaration* is persisted in the WAL (and the
  snapshot baseline); the postings are rebuilt from live data when the store
  opens — so an index survives a snapshot and reopen with no format migration.
- **Used automatically.** The planner rewrites a single non-`id` equality in a
  MATCH property map to an indexed lookup. If the field is not indexed the query
  still runs, just as a scan — declaring an index never changes results, only
  speed.
- **Which query shapes.** Today the fast path covers exactly one equality in the
  inline property map, e.g. `MATCH (n:Person {city: 'austin'})` or with a
  parameter `MATCH (n:Person {city: $c})`. Compound filters and `WHERE`-clause
  equality fall back to a scan.

## Relationship to fulltext

Use a **property index** for exact scalar equality (`city = 'austin'`). Use a
[fulltext index](fulltext.md) for tokenized text search (BM25, stemming, phrase
and prefix queries) over free-text fields.
