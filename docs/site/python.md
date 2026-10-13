# Python guide

Install with `pip install mushroomdb`, or build from source:

```text
cd bindings/python
python -m venv .venv
.venv/bin/pip install -U pip maturin
.venv/bin/maturin develop
```

## Open a database

```python
import mushroomdb

db = mushroomdb.GraphDb.open("/path/to/db")
reader = mushroomdb.GraphDb.open("/path/to/db", read_only=True)
```

`GraphDb.open` creates the database directory if it does not already exist — no
manual `mkdir` is required.

A read-write handle takes the store's cross-process write lock and raises
`MushroomBusy` if another one already holds it; `read_only=True` never takes the
lock. See [Concurrency](#concurrency) below.

## Insert nodes

`insert_node(label, key, props, namespace=None)` — same argument order as Rust.
Raises if the key is already live.

```python
db.insert_node("Person", "alice", {"skills": ["graph", "rust"]})
db.insert_node("Org", "acme", {"skills": ["graph", "rust", "search"]})
db.insert_node("Doc", "a1", {"title": "t"}, namespace="tenant-a")
```

`namespace` (v0.6.6) is the namespace the node is created in — set at insert and
immutable, so `set_prop(key, "ns", …)` afterwards raises. Omitted (or `"default"`)
means the `default` namespace and stores nothing. An invalid name raises `ValueError`;
a `props["ns"]` disagreeing with the argument raises too.

## Upsert nodes

`upsert_node(label, key, props)` returns `"inserted"` or `"updated"`.

```python
db.upsert_node("Person", "alice", {"team": "red"})   # "inserted"
db.upsert_node("Person", "alice", {"team": "blue"})  # "updated"
```

On update it writes only the fields present in `props` whose value differs from
the stored one. Fields you do not pass are left untouched, and an unchanged
field produces no WAL record — so rules do not re-fire needlessly. An existing
key under a different label raises `ValueError`: relabelling a node is not an
upsert.

## Set and remove properties

```python
db.set_prop("alice", "team", "green")
db.remove_prop("alice", "team")        # True if removed, False if already absent
db.set_prop("alice", "team", None)     # identical to remove_prop
```

Python has no distinct "null property" and the store has no null `Value`, so
`None` means absent. Removing a field a rule watches retracts the edges that
field derived.

## Delete a node

```python
report = db.delete_node("alice")
# {"manual_edges": 1, "derived_edges": 3}
```

Deletes the node and every incident edge, returning counts of the user-inserted
and rule-derived edges removed. An unknown or already-deleted key raises
`RuntimeError` (`KeyNotFound`).

## Batch ingest

`ingest_batch(nodes, edges=None)` commits every node and edge in a single WAL
frame. Each node is a `{key, label, props}` dict; each edge is a
`{edge_type, src, dst}` dict.

```python
nodes = [
    {"key": f"node-{i}", "label": "Person", "props": {"value": i}}
    for i in range(10000)
]
report = db.ingest_batch(nodes)
# {"inserted": 10000, "edges_inserted": 0, "row_errors": [], ...}
```

A bad edge rejects the entire batch. Keep each call to ≤10,000 nodes: one
giant WAL frame's fsync cost dominates and negates the batching benefit.

## Create a rule

```python
db.create_rule({
    "name": "skill_fit",
    "src_label": "Person",
    "dst_label": "Org",
    "predicate": {"kind": "overlap", "fields": ["skills"], "min": 0.5},
    "edge_type": "FIT",
    "weight_prop": "score",
    "max_edges": None,
    "approximate": False,
})
```

`create_rule` returns `True` when it created the rule. Pass
`if_not_exists=True` to get `False` instead of an exception when a rule of that
name is already registered:

```python
db.create_rule(rule, if_not_exists=True)   # True first time, False after
```

A `"namespace"` key (v0.6.6) scopes the rule to one namespace — it sees only that
namespace's nodes, so every edge it derives stays inside. Omitted is a global rule.

**Predicate shape.** The canonical form is the snake_case one that `explain`
emits, so an explanation round-trips straight back into a new rule:

| `kind` | extra keys |
|---|---|
| `key_match`, `field_equal` | — |
| `overlap`, `vector_similar` | `min` |
| `numeric_within` | `tolerance` |
| `geo_radius` | `km` |
| `all`, `any` | `parts` (a list of nested predicates) |

The field name comes from `fields[0]` (a bare `"field"` string is also
accepted). The Rust-native externally-tagged form still works:
`{"Overlap": {"field": "skills", "min": 0.5}}`, `{"All": [ … ]}`.

```python
why = db.explain("alice", "acme")
clone = {**base_rule, "name": "skill_fit_v2", "predicate": why[0]["predicate"]}
db.create_rule(clone)
```

**`max_edges` semantics:** a missing key fills the predicate default (32 for
scored predicates, 512 for KeyMatch), the same as HTTP `null`. Python `None`
(JSON null) is passed through and leaves `max_edges` as `None` — the Rust
global-budget hatch (1,000,000 per source). Use HTTP `null` or omit the key
if you want the capped default; pass `None` explicitly only if you intend the
uncapped budget.

**Symmetric predicates:** when `src_label == dst_label` (e.g., Person-to-Person
similarity), the rule fires in **both directions** — a property change on any
node triggers candidate scanning from that node as both source and destination.
An undirected Cypher pattern like `MATCH (a)-[:SIMILAR]-(b)` double-counts rows
because both `a→b` and `b→a` edges exist. Use directed patterns
(`MATCH (a)-[:SIMILAR]->(b)`) or `RETURN DISTINCT a, b` to avoid duplicates.

## Query

```python
# params as a dict
result = db.query(
    "MATCH (p:Person)-[r:FIT]->(o:Org) WHERE r.score >= $min RETURN p, o, r.score AS score",
    params={"min": 0.5}
)

# params as a list of (name, value) tuples (back-compat form)
result = db.query(
    "MATCH (p:Person) WHERE p.age > $age RETURN p",
    params=[("age", 30)]
)

# no params
result = db.query("MATCH (p:Person) RETURN p")

for row in result:
    print(row["p"], row["score"])
```

`params` accepts a `dict`, a list of `(str, value)` tuples, or `None`. Values
are passed as Cypher parameters — they are never interpolated into the Cypher
string, so string values are safe against injection regardless of content.

`query_with_params(cypher, params)` is a retained back-compat alias for
`query(cypher, params=params)`.

`role` and `namespace` (v0.6.6) restrict what the query may see:

```python
mine = db.query("MATCH (n) RETURN n", role="a-reader", namespace="tenant-a")
```

`role` answers as one of the store's `roles.json` roles and `namespace` from one
namespace; together they **intersect**, so a role bound to `tenant-a` asked for
`tenant-b` returns `[]` rather than the union. Either argument makes the call a read —
a write statement raises — and an invalid namespace name raises `ValueError`.

`query_write(cypher, params=None)` takes the same three param shapes:

```python
db.query_write(
    "MATCH (n:Person) WHERE key(n) = $k SET n.age = 31 RETURN key(n)",
    {"k": "alice"},
)
```

`n.key` / `n.id` read the node key (a stored property of the same name wins).
`key(n)` / `id(n)` always return the id-map key. `key(n)` is available in both
read queries and write-statement `RETURN` projections. See [query.md](query.md).

The dict keys in each row are the **RETURN aliases** from the query — `p`, `o`,
and `score` in the example above. Bare `RETURN n` yields `{"n": ...}`; `RETURN
n.age AS age` yields `{"age": ...}`.

## Traversal

```python
edges = db.node_edges("alice")   # [{edge_type, src_key, dst_key, derived}, ...]
info  = db.node_info("alice")    # {key, label, props}; None for an unknown key
neighbors = db.neighbors("alice", "KNOWS", "out")   # one hop, list of keys
```

`node_info` returns `None` for an unknown key; `node_edges` raises
`RuntimeError`. The asymmetry mirrors the Rust API (`Option` vs `Result`).

## Vector search

`find_similar(field, vector, label=None, k=10, min=0.8, mask=None, where=None, exact=False)`
returns the `k` nearest nodes to `vector` by cosine similarity on `field`.

Scores are cosine similarity in `[-1, 1]`. The engine keeps `score >= min`
(inclusive). A distance of `1 - sim` is the caller's conversion — the engine
does not speak distance. Convert after the call if you need a strict distance
cut (`keep if 1 - sim < T`); do not bake `T` into `min`.

```python
hits = db.find_similar("embedding", query_vec, k=10, min=0.0)
# [(key, similarity), ...]
```

`min` defaults to `0.8`, on every surface. Through 0.6 the Python default was `0.0`: pass
`min=0.0` if a call relied on it.

**Exact vs approximate.** When no approximate `VectorSimilar` rule covers
`field`, brute `find_similar` is an exact GEMM. HNSW is still the approximate
path — used when such a rule covers the field and `exact` is false.

Two arguments make the call exact, and one does not:

| Argument | Kernel | Exact? |
|---|---|---|
| `exact=True` | GEMM over the candidate set | **Yes** |
| `where={…}` | GEMM over the candidate set | **Yes** — a predicate implies exact |
| `mask=[…]` alone | HNSW, with the beam widened | **No** |
| a `scoped()` handle alone | HNSW, with the beam widened | **No** |

**`mask=` alone is the approximate path.** This is the one that gets misread. A
mask narrows *which nodes may be returned*; it does not change *which kernel
runs*. Under a mask (or a `scoped()` handle) the HNSW beam widens until it has
`k` visible hits, and only if the beam reaches the cap an exact `VectorSimilar`
rule uses (`EF_MAX` = 4,096) does it fall back to an exhaustive masked scan. So
a masked call does not return fewer than `k` while more visible hits exist, and
it is still not guaranteed to have found the true top `k`.

If you need an exhaustive answer over the visible set, say so — pass
`exact=True`, or a `where=` predicate. Both keep the mask and the scope; they
change only the kernel.

```python
hits = db.find_similar("embedding", query_vec, mask=visible)               # approximate
hits = db.find_similar("embedding", query_vec, mask=visible, exact=True)   # exact
```

The engine says this out loud too: the first masked non-exact search of a given
`(field, label)` on an indexed field prints one line to stderr naming the
remedy, and further searches of that shape on that index are silent.

Making a mask or a scope imply `exact` was considered and rejected: it would
silently turn a fast approximate call into an O(n) GEMM for every existing
masked caller.

`exact=True` forces GEMM and does not consult HNSW. Brute results sort
similarity descending, then key ascending; the HNSW sort is unchanged. A
candidate whose embedding is missing, zero-norm, or a different length than the
query is skipped. A zero-norm query returns `[]`.

```python
hits = db.find_similar("embedding", query_vec, exact=True)
hits = db.find_similar(
    "embedding", query_vec, label="Document",
    where={"field": "status", "eq": "published"},
)
```

`where` is the same shape as a role's `visible_where`: exactly one of `eq` or
`in`. A missing property fails. An empty `in` matches nothing. Invalid `where`
(both `eq` and `in`, empty `field`) raises `ValueError` and is never sent into
the engine.

```python
{"field": "resource_scope_id", "eq": "scope-1"}
{"field": "status", "in": ["published", "archived"]}
```

Candidates are `label ∩ mask ∩ where`. `mask` is still a list of keys; it
intersects, never widens. `label` restricts as before.

**`where=` uses the property index only when a `label` accompanies it.** The
index is keyed on `(label, field)`, so a predicate with no label has nothing to
look up. Both conditions must hold for the index path:

1. `label=` is set on the call, **and**
2. `enable_index(label, where["field"])` has been called on the store.

With both, `eq` / `in` resolve through the property index. With either missing —
a predicate and no label, or a label whose `(label, field)` index was never
enabled — the call scans the labelled set, or the whole live set when there is
no label. **That scan is correct, just slower**; the answer is identical either
way, so nothing tells you at runtime which path you took. If a `where=` search
is slower than you expected, this is the first thing to check.

```python
db.enable_index("Document", "status")
hits = db.find_similar(                      # index path
    "embedding", query_vec, label="Document",
    where={"field": "status", "eq": "published"},
)
hits = db.find_similar(                      # correct, but a full live scan
    "embedding", query_vec,
    where={"field": "status", "eq": "published"},
)
```

`where` is a predicate on the **data**, not on the caller — see
[masks.md](masks.md#scoped-one-front-door-for-visibility) for the difference,
and [masks.md](masks.md#narrowing-a-role-by-a-property) for the role-side
predicate, `visible_where`.

`pairwise_similar(keys, field, k=10, min=0.0)` is exact cosine top-k for each
key in `keys`, scored only against that same set. Self-matches are excluded.
It never uses HNSW and does not write edges.

```python
pairs = db.pairwise_similar(["a", "b", "c"], "embedding", k=5, min=0.0)
# [("a", [("b", 0.91), ...]), ("b", [...]), ("c", [])]
```

Treat the result as a map keyed by source: outer order is first-seen packed
keys, not a zip with the input. A packed source with no neighbour above `min`
still appears as `(src, [])`, so "present, nothing similar" is distinct from
"omitted". Unknown keys, missing embeddings, zero-norm and wrong-dimension
vectors are omitted as both query and candidate. Duplicate keys collapse to
first-seen order. Empty `keys` returns `[]`. More than 8192 unique resolved
keys raises. `min` is the same unit and inequality as `find_similar`: cosine
similarity in `[-1, 1]`, kept when `score >= min`. A distance of `1 - sim` is
the caller's conversion here too — the engine does not speak distance on this
surface any more than on `find_similar`. Convert after the call if you need a
distance cut (`keep if 1 - sim < T`); do not bake `T` into `min`.

## Degree

Adjacency is a set: a second `insert_edge` of the same `(edge_type, src, dst)`
returns `False` and does not increase degree. `degree` is unique-neighbour
count, not a stored counter and not a row-count of duplicate pairs.

```python
n = db.degree("alice", edge_type="KNOWS", direction="both")  # out + in sum
ranked = db.degrees(label="Person", direction="both", limit=500)
# [("alice", 12), ("bob", 12), ...]  degree desc, key asc
```

`direction` is `"out"`, `"in"`, or `"both"`. `"both"` is the sum of unique
out-neighbours and unique in-neighbours (a reciprocal pair counts 2 at each
endpoint), not the size of the undirected neighbour set.
`neighbors(..., direction="both")` still raises `ValueError` — that API is one
directed hop.

`degree(key)` raises for an unknown key. `degrees` omits unknown keys;
`degrees(keys=[])` returns `[]`. An unknown `edge_type` yields 0.

`degrees` universe: `keys` if given; else `label`; else all live nodes. `where`
intersects (same dict shape as `find_similar`). `limit` applies after the sort.

This is the read-only subset API. It is not `degree_centrality` (full-graph
ranking, time-budgeted, not bound in Python) and not a Degree materialized view
(a write that maintains a property). See [algorithms.md](algorithms.md).

## Rename a node

```python
db.rename_node("alice", "alice2")
```

Raises `KeyNotFoundError` if the old key does not exist, `DuplicateKeyError` if the
new key is already taken.

## Insert edge with auto-create

```python
result = db.insert_edge_upsert("KNOWS", "alice", "bob", "Person")
# result is a dict: {"nodes_created": 1, "edge_inserted": True}
```

Missing endpoint nodes are created with the given `placeholder_label`. If the edge
already exists, `edge_inserted` is `False` (idempotent).

## Explain

```python
explanation = db.explain("alice", "acme")
# [{"rule", "edge_type", "src_key", "dst_key", "weight", "predicate"}, ...]
```

`predicate` is the snake_case summary shape that `create_rule` accepts verbatim.

## Stats

```python
s = db.stats()
# {"nodes_live": N, "nodes_tombstoned": 0, "edges": E, "history_floor": 0,
#  "namespaces": [{"name": "default", "nodes_live": N}], "rules": [...]}
```

`namespaces` (v0.6.6) lists every namespace with at least one live node, in name
order, always including `default` — one entry on a store that has never named one.

## Concurrency

**One writer at a time across processes.** The store carries an advisory write
lock. A read-write handle holds it for as long as it is open, so opening a
second one anywhere on the machine raises `MushroomBusy` rather than letting two
writers corrupt the store. Nothing was written when it raises, so retrying later
is safe:

```python
from mushroomdb import GraphDb, MushroomBusy

try:
    db = GraphDb.open("./db")
except MushroomBusy:
    ...  # another process is writing
```

**Readers see every commit, after `refresh()`.** A handle does not poll the
store, so another process's commits stay invisible until you ask for them.
`refresh()` applies them in place and returns how many arrived — rules fire and
derived edges appear exactly as on a fresh open, and no `close()` and reopen is
needed:

```python
n = db.refresh()
```

**`read_only=True` never takes the lock.** Such a handle opens immediately even
while a writer holds it, writes nothing to disk, raises `RuntimeError` on any
mutation, and can still `refresh()` to follow the writer:

```python
reader = GraphDb.open("./db", read_only=True)
reader.refresh()
```

A commit another process is midway through writing is left alone and picked up
by the next `refresh()`; a partial write is never an error.

Within one process the handle is guarded by a mutex, so calls from multiple
threads are serialized and safe. They are not isolated transactions: readers
can observe intermediate states while a batch is being applied. See
[concurrency.md](concurrency.md) for the full cross-process model and
[durability.md](durability.md) for the WAL and crash-atomicity guarantees.

## Type stubs

The wheel ships `__init__.pyi` (generated from
`bindings/python/mushroomdb.pyi`) plus a `py.typed` marker, so mypy and
Pyright pick up signatures with no extra configuration. Every method also
carries a docstring and a `__text_signature__`, so `help(mushroomdb.GraphDb)`
is useful at the REPL.

## Snapshot

```python
db.snapshot()  # write V8 (mmap rkyv) snapshot and truncate WAL
```

`snapshot()` writes the current state as a V8 mmap snapshot (12 rkyv sections,
zero-copy open) and then truncates the WAL to a minimal baseline. Faster cold
starts, but as-of history (`open_at`) restarts from that point — commits before
the snapshot are no longer reachable via time travel.

**Keep WAL (Rust API):**

```rust
use core_api::{SnapshotOptions};
db.snapshot_with(SnapshotOptions { keep_wal: true, archive_wal: false })?;
```

`keep_wal: true` writes the V8 snapshot but leaves the WAL intact. `open_at`
can still reach pre-snapshot commits. The WAL replay over the snapshot is
idempotent — no manual recovery is needed. The WAL grows until an explicit
`snapshot()` (with default `keep_wal: false`) truncates it.

**V9 snapshot format:** mmap-able rkyv sections (13, and one more for the full-text index when a pair is declared); zero-copy open via
mmap; no heap allocation for section data. V5–V8 stores are auto-migrated to
V9 on open; a V9 snapshot cannot be opened by a pre-0.6.5 binary, which refuses
it with `snapshot: unsupported version 9`. See [`docs/format-stability.md`](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/format-stability.md) for the
full section table and migration notes. 100k nodes (v0.2, ~10M derived edges):
V8 snapshot open 0.02 s / 31–41 MiB RSS (warm file cache, cold process,
2026-08-28, Apple M4 Pro).

## Exposed surface

Every method the binding exposes, with its signature and docstring, is in the
[Python API](python-api.md) reference, which is generated from the type stub.

`write_batch` is not directly exposed in the Python bindings because Rust
closures capturing `&mut BatchBuilder` do not map naturally to the Python
object model. Use `ingest_batch` (node + edge inserts in one frame) or
`batch_edges` (mixed insert/delete edges in one frame) for atomic Python
writes. Both use the same `WalRecord::Batch` frame and one-fsync guarantee.

The Rust forms — `write_batch`, and the compare-and-set `write_batch_cas` — are on the
[Rust](rust.md#several-writes-as-one) page.

Not yet exposed: `ingest_json` (use `ingest_batch`), auto-FK inference
(declare `KeyMatch` rules explicitly). `rebuild_rule` (per-rule re-derivation)
is exposed; a full `rebuild` is not.
