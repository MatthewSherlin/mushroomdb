# Full-Text Search (full-text-lite)

mushroomdb ships an incrementally-maintained inverted index for keyword search over node properties. It is designed for graph-native use cases: finding nodes by text content, ranking by match quality, and combining results with graph traversal — not for replacing a dedicated search engine.

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

Both declarations are WAL-logged and replayed on open; the index is rebuilt from scratch on every open from the WAL-declared pairs, so no snapshot format changes are required.

## Searching

```rust
// Returns Vec<(key, match_count)> sorted by match_count DESC, then key ASC.
let results = db.search("bio", "rust embedded")?;
```

Match count is the number of OR-groups satisfied by the node (see query syntax below). Nodes that satisfy more groups rank higher.

## Query syntax

| Form | Meaning |
|------|---------|
| `word` | exact token match (lowercased alphanumeric) |
| `word*` | prefix match (trailing `*`) |
| `a b` | AND: both tokens must appear |
| `a OR b` | OR: either group must match |
| `a b OR c*` | `(a AND b) OR (prefix c)` |

`AND` is the default; the keyword `AND` may be written explicitly or omitted.
`OR` is case-insensitive. Empty or whitespace-only queries return no results.

**Mid-token `*`:** A `*` that is not at the trailing position of a word (e.g., `"ru*st"`) is NOT a prefix operator, and the two sides treat it differently: in **queries**, non-alphanumeric characters inside a word are stripped, so `"ru*st"` searches for the exact term `rust`; in **indexed document text**, any non-alphanumeric character splits tokens, so a stored value `"ru*st"` indexes as the two tokens `ru` and `st`. Use a trailing `*` (`"ru*"`) for prefix matching.

## Tokenization

Tokens are extracted by splitting on non-alphanumeric characters and lowercasing each character. Unicode alphanumeric characters are accepted. Stemming, phrase search, and stop-word removal are not supported in v1.

Examples:

- `"Rust embedded"` → `["rust", "embedded"]`
- `"graph-database"` → `["graph", "database"]`
- `"Hello, World!"` → `["hello", "world"]`

## Cypher WHERE

`textMatches(n.field, "query")` can be used directly in a WHERE clause:

```cypher
MATCH (a:Article)
WHERE textMatches(a.bio, 'rust embedded')
RETURN a.key, a.bio
```

This performs a per-row scratch scan (O(scan) per row) and is correct for small graphs. For large indexed fields, prefer `db.search()` which uses the inverted index (O(tokens)).

## What this is not

mushroomdb full-text-lite is not a replacement for PostgreSQL FTS, Elasticsearch, or Typesense. It has no stemming, no phrase search, no BM25 scoring, no language-specific analysis, no field boosting, and no distributed sharding. It is a simple inverted index for graph-native search: lightweight, zero external dependencies, and incrementally maintained alongside your graph mutations.

For production full-text requirements, index from mushroomdb into a dedicated search engine via the subscription API and query both in parallel.

## Memory model

The index uses O(unique\_tokens × avg\_postings\_per\_token) memory per indexed field. There is no cap in v1. Very large free-text fields (e.g. full article bodies) may consume significant memory; prefer indexing short summary fields.

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

On open, `rebuild_all()` corrects any drift accumulated during incremental WAL replay. This ensures crash safety at every WAL offset.
