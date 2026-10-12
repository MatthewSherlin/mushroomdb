# Vector search

A vector is an ordinary property: a list of numbers on a node. The store compares vectors; it
never computes them. You bring the embeddings, from whatever model you use, and write them as
you would any other property.

## Two things you can do with a vector

- **Declare a rule.** A `VectorSimilar` rule links every pair of nodes whose vectors are similar
  enough, with a derived edge that is kept current on every write.
- **Search.** `find_similar` returns the nodes whose vector is nearest a query vector.

They are independent. A search needs no rule, and a rule needs no search.

## A rule that links similar nodes

The demo store has one: `similar_interests` links people whose `embedding` vectors have a cosine
similarity of at least 0.8.

```sh
mushroomdb demo ./db
mushroomdb query ./db "MATCH (p:Person {id: 'person-01'})-[r:SIMILAR]->(q:Person) RETURN q, r.score AS score ORDER BY score DESC, q LIMIT 3"
```

```text
columns: q, score
q=person-11  score=1.0
q=person-21  score=1.0
q=person-09  score=0.8
```

Nobody wrote those edges. Change a person's `embedding` and theirs are re-derived in the same
write: new ones added, the ones that fell under 0.8 retracted.

Declaring one, in Python:

```python
from mushroomdb import GraphDb

db = GraphDb.open("./vecdb")
db.insert_node("Item", "a", {"embedding": [1.0, 0.0]})
db.insert_node("Item", "b", {"embedding": [0.9, 0.1]})
db.insert_node("Item", "c", {"embedding": [-1.0, 0.0]})

db.create_rule({
    "name": "similar_items",
    "src_label": "Item",
    "dst_label": "Item",
    "predicate": {"VectorSimilar": {"field": "embedding", "min": 0.8}},
    "edge_type": "SIMILAR",
    "weight_prop": "score",
})

print(db.neighbors("a", "SIMILAR", "out"))   # ['b']
```

The same rule over HTTP is the same object, sent to [`POST /rules`](http.md#post-rules).

What the rule does:

- Similarity is cosine. It is the only metric.
- `min` is the threshold, above 0 and at most 1. The similarity is stored on the edge under the
  rule's `weight_prop`.
- With no `max_edges`, the rule keeps the 32 best matches for each source node. Set `max_edges`
  to keep more or fewer.
- A rule between a label and itself links in both directions: `a` to `b`, and `b` to `a`.
- Two vectors of different lengths, a list with something in it that is not a number, and a
  vector of all zeros produce no edge. They produce no error either, so a field that was
  written with the wrong dimension is silent: check a node you expect to be linked.

## Searching

| | What it does | Python | MCP | HTTP |
|---|---|---|---|---|
| Find similar | The `k` nodes nearest a query vector | `find_similar` | `find_similar` | `POST /find_similar` |
| Pairwise similar | Among keys you name, which are most alike | `pairwise_similar` | `pairwise_similar` | |
| Hybrid | Full-text and vector results in one ranking | `search_hybrid` | `hybrid_search` | |

Continuing the example above:

```python
db.find_similar("embedding", [1.0, 0.0], label="Item", k=3)
# [('a', 1.0), ('b', 0.9938837346736189)]

db.find_similar("embedding", [1.0, 0.0], label="Item", k=3, min=-1.0)
# [('a', 1.0), ('b', 0.9938837346736189), ('c', -1.0)]
```

- `k` defaults to 10.
- **`min` defaults to 0.8, on every surface.** A search that names no `min` returns only close
  matches, and can return none. Pass a lower `min` to see everything ranked, as the second call
  does.
- A score is a cosine similarity, from -1 to 1. If you need a distance, it is `1 - score`.
- `pairwise_similar` compares only the keys you give it, exactly, and writes nothing.
- Over MCP, `find_similar` has a second form: give it a `key` and an `edge_type` and it reads
  that node's derived edges instead of searching.

Every argument is in the [Python API](python-api.md), [MCP tools](mcp-tools.md) and
[HTTP API](http.md#post-find_similar) references.

## Exact and approximate

By default a search compares the query with every vector it may return. That is exact, and it
takes time in proportion to the number of nodes.

A search uses an index only when a rule over that field was declared with `approximate: true`.
`has_vector_rule(field)` says whether one was. The demo's rule is not approximate, and neither
is the one in the example above, so searches on both are exact scans.

When there is an index:

- `exact=True` sets it aside and scans.
- A `where` filter sets it aside too: a filtered search is always exact.
- A mask, or a scoped handle, does **not**. The search stays approximate. It widens until it has
  `k` hits the caller may see, and falls back to an exact scan of the visible nodes when
  widening is not enough.

Approximate means what it says. The index finds near neighbours quickly; it does not prove they
are the nearest. Use `exact=True` where a missed neighbour would matter.

## The index

The index is an HNSW graph, built in the engine with no outside dependency.

| Parameter | Default |
|---|---|
| `m` | 16 |
| `m0` | 64 |
| `ef_construction` | 200 |
| `ef_search` | 400 |

They apply to the whole store and are set only through an environment variable, read once when
the process starts: `MUSHROOMDB_HNSW_PARAMS=m,m0,ef_construction,ef_search`. There is no setting
for one rule.

A rule created over more than 2,048 vectors builds its index in slices. `create_rule` returns
before the edges exist, and they arrive in later commits. To have the build finished before
traffic arrives:

```sh no-run
mushroomdb build-index ./db
```

The index is saved in the snapshot, so a store opened from a snapshot does not build it again.

## What has been measured

Every pull request is held to a recall floor. On 5,000 vectors of dimension 1536, the ten
nearest neighbours the index returns must include at least 0.90 of the true ten for every query
and 0.95 on average. It measures 1.00 and 1.00.

That is the easy case: vectors drawn at random, on a fixed seed. On clustered vectors, with
clusters wider than the index's neighbour lists, the floor is 0.70 for the worst query and 0.95
on average, and it measures 0.80 and 0.995.

The index keeps its own 32-bit copy of each vector, about 6.7 kB a node at dimension 1536,
beside the 64-bit values the node stores as a property. The largest index built so far is
50,000 vectors. [Benchmarks](benchmarks.md) and [Known limits](limits.md) have the rest.

## Hybrid search

Hybrid search runs a full-text search and a vector search and fuses the two rankings. It takes
the top `4 × k` from each side and scores a node by the sum of `1 / (60 + rank)` over the sides
it appears in.

- The text field has to be declared full-text, with `enable_fulltext(label, field)` or in a
  schema. If it is not, the text side returns nothing.
- An empty vector makes it a text-only search.
- It takes no `exact` argument and no filter.

[Full-text search](fulltext.md) covers the text side.

## What it does not do

- One metric, cosine.
- No embedding model: vectors come from you.
- No quantisation: the index holds 32-bit floats.
- A filter on a search is one equality or membership test on one field, and makes it exact.
- Index parameters belong to the store, not to a rule.
