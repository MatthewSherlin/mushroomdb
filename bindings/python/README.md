# mushroomdb (Python)

Python bindings for [mushroomdb](https://github.com/MatthewSherlin/mushroomdb) —
the embedded graph database where edges are declared, not inserted.

```python
import mushroomdb

db = mushroomdb.GraphDb.open("./db")
db.insert_node("Org", "org-01", {"founded_year": 2010})
```

`GraphDb.open` creates the directory if it does not exist. The handle is also a
context manager, so `with mushroomdb.GraphDb.open("./db") as db:` closes on exit.

## Writing nodes

```python
db.insert_node("Person", "alice", {"team": "red"})   # raises if 'alice' exists
db.upsert_node("Person", "alice", {"team": "blue"})  # "inserted" or "updated"
db.set_prop("alice", "team", "green")
db.set_prop("alice", "team", None)                   # same as remove_prop
db.remove_prop("alice", "team")                      # False if already absent
report = db.delete_node("alice")                     # {"manual_edges", "derived_edges"}
```

`upsert_node` writes only the fields you pass whose value differs from the
stored one. Fields you omit are left alone, and unchanged fields produce no WAL
record, so rules do not re-fire needlessly. An existing key under a different
label raises `ValueError` — relabelling is not an upsert.

## Querying

`query` and `query_write` both accept parameters as a `dict`, as a list of
`(name, value)` tuples, or not at all. Parameters are bound, never interpolated
into the Cypher string.

```python
rows = db.query(
    "MATCH (n:Person) WHERE n.age > $min RETURN key(n) AS id",
    {"min": 18},
)
db.query_write(
    "MATCH (n:Person) WHERE key(n) = $k SET n.age = 31 RETURN key(n)",
    {"k": "alice"},
)
```

`n.key` / `n.id` read the node key (a stored property of the same name wins).
`key(n)` / `id(n)` always return the id-map key. `node_info` returns the key too.

## Scoping: who may see what

Bind the scope to the **handle**, not to the call. `scoped()` returns a
read-only child that applies the scope to every read:

```python
reader = db.scoped(role="reader-a")
tenant = db.scoped(namespace="tenant-a", keys=visible_ids)
```

It shares the parent's store and mutex — not a second open, no second lock —
and refuses every write. Legs intersect, so `scoped()` on a scoped handle
narrows further and can never widen; an unknown `role` raises at `scoped()`,
not on the first read. `refresh()` is allowed: it writes nothing.

Every read obeys one contract: **the subject is checked first, so a key outside
the scope is indistinguishable from a key that does not exist.** `node_info`
answers `None`, `node_edges` raises `KeyNotFound`, `degree` counts only visible
neighbours, `find_similar` scores only visible candidates. Hidden is absent,
never "hidden".

The per-call arguments are the same idea, narrowed for one call rather than for
a caller:

| Argument | `scoped()` form |
|---|---|
| `role=` (a role from `roles.json`) | `scoped(role=…)` |
| `namespace=` | `scoped(namespace=…)` |
| `mask=` (an explicit key allow-list) | `scoped(keys=…)` |
| `visible_where` in `roles.json` | carried inside `role=` |

None of them is deprecated — they are the right tool when the narrowing really
does belong to one call.

**`where=` is not one of them.** It is a predicate on the *data*, not on the
caller: an argument this call chose, filtering a result set the caller was
already entitled to see. A role's `visible_where` takes the same dict shape and
is the opposite thing — part of who the caller is, not passed by them. Do not
use `where=` as an access control. It composes with a scope, intersecting.

`db.roles()` lists what `roles.json` defines. It raises `Corrupt` if that file
was corrupt at open, rather than answering `[]` as an unrestricted store does.

Full detail: [masks.md](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/site/masks.md).

## Vector search

Scores are cosine similarity in `[-1, 1]` (`score >= min`). A distance of
`1 - sim` is the caller's conversion — on `pairwise_similar` too. Convert after
the call rather than baking a distance threshold into `min`.

**`mask=` alone is the approximate path.** `exact=True` and a `where=`
predicate each force an exact GEMM brute-force. A `mask=` allow-list — and a
`scoped()` handle — do not: they narrow which nodes may be returned without
changing which kernel runs, so the answer stays approximate whenever an HNSW
rule covers the field. Under a mask the beam widens until it has `k` visible
hits, so the result is never short while more visible hits exist, but it is
still not guaranteed to be the true top `k`. For an exhaustive answer over the
visible set, pass `exact=True` **alongside** the mask.

```python
hits = db.find_similar("embedding", query_vec, mask=visible)              # approximate
hits = db.find_similar("embedding", query_vec, mask=visible, exact=True)  # exact
```

When no approximate rule covers the field — check with
`has_vector_rule("embedding")` — every call is already an exact brute-force
scan.

**`min` defaults to `0.8`** — here, in the MCP `find_similar` tool and on HTTP
`POST /find_similar`. Through 0.6 this binding and HTTP defaulted to `0.0`: a
call that never named `min` now drops every hit below `0.8`, and nothing
raises. Pass `min=0.0` for the old behaviour. `pairwise_similar` still defaults
to `0.0`.

**`where=` uses the property index only when a `label` accompanies it.** The
index is keyed on `(label, field)`, so both `label=` and a prior
`enable_index(label, field)` are needed for the index path. Without them the
call scans the labelled set, or the whole live set when there is no label —
correct, just slower, and nothing tells you at runtime which path you took.

```python
db.enable_index("Document", "status")
hits = db.find_similar(                                 # index path
    "embedding", query_vec, k=10, min=0.0,
    label="Document",
    where={"field": "status", "eq": "published"},
)
pairs = db.pairwise_similar(["a", "b", "c"], "embedding", k=5)
# exact per-key top-k among `keys`; self excluded; no HNSW
```

## Degree

Adjacency is a set; `degree` is unique-neighbour count (`"both"` is out+in
sum). Distinct from `degree_centrality` and from Degree views.

```python
db.degree("alice", direction="both")
db.degrees(keys=["alice", "bob"], limit=10)  # degree desc, key asc
```

## Rules

```python
db.create_rule({
    "name": "same_team",
    "src_label": "Person",
    "dst_label": "Person",
    "predicate": {"kind": "field_equal", "fields": ["team"]},
    "edge_type": "SAME_TEAM",
    "weight_prop": None,
})
```

The **canonical predicate shape is snake_case** — `{"kind": ..., "fields": [...]}`
plus whatever numeric knob the kind takes (`min`, `tolerance`, `km`, or `parts`
for `all`/`any`). This is exactly the shape `explain` emits, so an explanation
round-trips straight back into a new rule:

```python
why = db.explain("alice", "bob")
clone = {**base, "name": "same_team_clone", "predicate": why[0]["predicate"]}
db.create_rule(clone)
```

| kind | extra keys |
|---|---|
| `key_match`, `field_equal` | — |
| `overlap`, `vector_similar` | `min` |
| `numeric_within` | `tolerance` |
| `geo_radius` | `km` |
| `all`, `any` | `parts` (a list of nested predicates) |

The Rust-native externally-tagged form is still accepted:
`{"FieldEqual": {"field": "team"}}`, `{"Overlap": {"field": "skills", "min": 0.5}}`.

`create_rule` returns `True` when it created the rule. Pass
`if_not_exists=True` to get `False` instead of an exception when a rule of that
name is already registered.

### Listing, deleting, rebuilding, proposing

```python
db.rules()                    # every rule, as dicts create_rule accepts
db.delete_rule("same_team")   # and every edge it derived
db.rebuild_rule("same_team")  # the way out of a tripped rule
for s in db.suggest_rules()["suggestions"]:
    print(s["name"], s["est_edges"], s["rationale"])
    # db.create_rule(s["create_rule_args"])  — nothing is created until you do
```

`suggest_rules()` never proposes a rule over a field the store writes for
itself, and each proposal's `create_rule_args` creates the same rule here as
it does through the MCP `create_rule` tool.

## Full-text search

```python
db.enable_fulltext("Doc", "body", if_not_exists=True)
db.search("body", 'graph OR "property index"', k=10)   # [(key, score), ...]
db.fulltext_pairs()                                    # [("Doc", "body")]
db.is_fulltext_enabled("Doc", "body")                  # True
db.disable_fulltext("Doc", "body")                     # the index and its postings
```

`search` is keyed by field alone: two labels indexed on the same field name
are searched together. Every declared pair is rebuilt when the store opens.

## Graph algorithms

```python
db.pagerank(edge_type="CITES")["scores"][:10]
db.connected_components()["components"]         # [(key, component), ...]
db.degree_centrality(direction="in")["scores"]  # not degree() / degrees()
db.communities(edge_types=["CITES"])["communities"]
```

`budget_ms` defaults to `0` — no time limit — so the same store gives the
same answer; pass one to bound a call and read `converged` / `truncated`.
`degree()` and `degrees()` are a different thing: the degree of the keys you
name, which can be scoped and filtered.

**None of these runs on a `scoped()` handle.** The algorithms, `search`,
`fulltext_pairs`, `rules` and `suggest_rules` answer about the whole store and
take no mask, so each raises `ValueError` there rather than return an answer
computed over nodes the scope hides.

## Concurrency

**One writer at a time across processes; readers see commits after `refresh()`.**
The store carries an advisory write lock. A read-write handle holds it for as
long as it is open, so opening a second one anywhere on the machine raises
`MushroomBusy` rather than letting two writers corrupt the store:

```python
from mushroomdb import GraphDb, MushroomBusy

try:
    db = GraphDb.open("./db")
except MushroomBusy:
    ...  # another process is writing; nothing was changed, retry later
```

Note where that refusal lands: **a plain handle takes the lock at `open`**, so
`MushroomBusy` is raised by `GraphDb.open`, not by the first write. Nothing was
written, so retrying later is always safe.

A handle does not poll the store, so another process's commits stay invisible
until you ask for them. `refresh()` applies them in place and returns how many
arrived — no `close()` and reopen:

```python
n = db.refresh()   # rules fire and derived edges appear, as on a fresh open
```

A `refresh()` that finds nothing new costs two filesystem metadata calls and an
integer compare, reading no file contents at all, so polling on an interval is
cheap. Open a reader once and refresh it; do not reopen per request.

Readers never take the lock. `read_only=True` opens immediately even while a
writer holds it, writes nothing to disk, raises `ReadOnly` on any mutation, and
can still `refresh()` to follow the writer:

```python
reader = GraphDb.open("./db", read_only=True)
reader.refresh()
```

A commit another process is midway through writing is left alone and picked up
by the next `refresh()`; a partial write is never an error.

**Keep a store that several processes open on a local disk.** Advisory locks
over a network filesystem — NFS, SMB, EFS — are not supported, and both the
lock and the staleness check fail silently there. The supported arrangements,
with a worked disposable-sidecar example, are in
[multiprocess.md](https://github.com/MatthewSherlin/mushroomdb/blob/main/docs/site/multiprocess.md).

Within one process the handle is guarded by a mutex, so calls from multiple
threads are serialized and safe. They are not isolated transactions: readers
can observe intermediate states while a batch is being applied.

## Type stubs

The wheel ships `__init__.pyi` and a `py.typed` marker, so mypy and Pyright
pick up signatures without extra configuration.

Full documentation, the rules tour, and benchmarks live in the
[main repository](https://github.com/MatthewSherlin/mushroomdb).
