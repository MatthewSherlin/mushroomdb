# Full-Text Search

mushroomdb ships an incrementally-maintained inverted index for keyword search over
node properties. It is designed for graph-native use cases: finding nodes by text
content, ranking by match quality, and combining results with graph traversal — not
for replacing a dedicated search engine.

## Enabling an index

Full-text indexing is opt-in, per `(label, field)` pair:

```rust
db.enable_fulltext("Article", "bio")?;
db.enable_fulltext("Article", "title")?;
```

Call `disable_fulltext` to drop the index and stop maintaining it:

```rust
db.disable_fulltext("Article", "bio")?;
```

Both declarations are WAL-logged and replayed on open; the index is rebuilt from
scratch on every open from the WAL-declared pairs, so no snapshot format changes are
required.

## Searching

```rust
// Returns Vec<(key, f64)> sorted by BM25 score DESC, then key ASC.
let results = db.search("bio", "rust embedded")?;
```

The score is a BM25 relevance score (see Scoring below). Nodes that match more terms
and more query groups rank higher.

## Query syntax (v2, shipped 2026-08-29)

| Form | Meaning |
|------|---------|
| `word` | stemmed term match (Snowball EN) |
| `"two words"` | phrase — both tokens must appear adjacent (stemmed adjacency) |
| `-word` | negation — excludes matching documents |
| `word*` | prefix match (trailing `*`; unstemmed, literal prefix) |
| `a b` | AND: both atoms must match |
| `a OR b` | OR: either group must match |
| `a b OR c*` | `(a AND b) OR (prefix c)` |

`AND` is the default within a group; the keyword `AND` may also be written
explicitly. `OR` separates groups.

### Stemming

All term queries go through the Snowball English stemmer before lookup. Index
tokens are stemmed the same way at write time, so a query for `"databases"` matches
documents containing `"database"` or `"databases"` (both stem to `"databas"`). Prefix
queries (`word*`) are matched on the **unstemmed** literal prefix so that `"databas*"`
returns results whose text starts with `"databas"` before any stemming is applied.

### Phrase matching

A phrase query (`"graph database"`) checks that the stemmed forms of both tokens
appear at adjacent positions within a single document field. Adjacent means
sequential token-stream positions — no gap. Phrases never match across list-element
boundaries: a field stored as `["graph theory", "database design"]` does not satisfy
`"graph database"` because the tokens come from different list elements.

### Negation

A leading `-` excludes all documents that contain the negated term. Negation applies
to any atom: `-word`, `-prefix*`, or `"-phrase"`.

A query consisting entirely of negation (e.g., `-foo` with no positive terms) returns
**no results** — there is no baseline to subtract from. A group that contains only
negated atoms contributes nothing to the final score even when combined with other OR
groups (e.g., `"graph OR -database"` scores as if the second group were absent).

### Tokenization

Index tokens: extracted by splitting on non-alphanumeric characters, lowercasing, then
applying the Snowball English stemmer. Unicode alphanumeric characters are accepted.

Examples (raw → indexed tokens):

- `"databases"` → `["databas"]` (Snowball EN stem)
- `"graph-database"` → `["graph", "databas"]`
- `"Hello, World!"` → `["hello", "world"]`

Query terms: lowercased and stemmed before lookup (except prefix queries, which are
matched on the raw prefix).

## Scoring

BM25 (Okapi BM25) with parameters:

- k1 = 1.2
- b  = 0.75

IDF = ln((N − df(t) + 0.5) / (df(t) + 0.5) + 1) where N is the total document
count for the indexed field.

TF normalisation = tf × (k1 + 1) / (tf + k1 × (1 − b + b × dl / avg_dl))

Score = sum of BM25 contributions across matched OR-groups.

These are the constants used by the implementation as of 2026-08-29. No comparative
benchmark of BM25 tuning across workloads has been run.

## Cypher WHERE

`textMatches(n.field, "query")` can be used directly in a WHERE clause:

```cypher
MATCH (a:Article)
WHERE textMatches(a.bio, 'rust embedded')
RETURN a.key, a.bio
```

This performs a per-row scratch scan (O(scan) per row) and is correct for small
graphs. For large indexed fields, prefer `db.search()` which uses the inverted index
(O(tokens)).

Cypher `textMatches` results use omit mode — hidden nodes are omitted regardless of
the `stub_hidden` setting.

## What this is not

mushroomdb full-text is not a replacement for PostgreSQL FTS, Elasticsearch, or
Typesense. It has no field boosting, no distributed sharding, and no custom
tokenizer pipeline beyond Snowball EN. It is a lightweight inverted index for
graph-native search: zero external runtime dependencies, incrementally maintained
alongside graph mutations.

For production full-text requirements at high volume, index from mushroomdb into a
dedicated search engine via the subscription API and query both in parallel.

## Memory model

The index stores per-token position lists (Vec<u32> per posting). Memory scales with
total token occurrences and token repetition within documents. For a typical corpus
of 10k documents averaging 100 tokens each, the v2 index uses roughly 1.25–3× more
memory than a v1 (position-free) index, depending on token repetition. There is no
cap. Very large free-text fields may consume significant memory; prefer indexing short
summary fields.

## Incremental maintenance

The index is maintained incrementally on every mutation:

| Operation | Behavior |
|-----------|----------|
| `insert_node` | Indexes all enabled fields for the node's label |
| `set_prop` | Removes old tokens, adds new tokens |
| `remove_prop` | Removes tokens for that field |
| `delete_node` | Removes all tokens for the node |
| `enable_fulltext` | Backfills all existing live nodes of that label |
| `disable_fulltext` | Drops the field's postings (if no other label still indexes it) |

On open, `rebuild_all()` corrects any drift accumulated during incremental WAL
replay. This ensures crash safety at every WAL offset.
