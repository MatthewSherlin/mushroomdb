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

A node's key is not a property, so `n.key` does not resolve. Use the `key(n)`
scalar function to project or filter on it. `node_info` returns the key too.

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

A handle does not poll the store, so another process's commits stay invisible
until you ask for them. `refresh()` applies them in place and returns how many
arrived — no `close()` and reopen:

```python
n = db.refresh()   # rules fire and derived edges appear, as on a fresh open
```

Readers never take the lock. `read_only=True` opens immediately even while a
writer holds it, writes nothing to disk, raises `RuntimeError` on any mutation,
and can still `refresh()` to follow the writer:

```python
reader = GraphDb.open("./db", read_only=True)
reader.refresh()
```

A commit another process is midway through writing is left alone and picked up
by the next `refresh()`; a partial write is never an error.

Within one process the handle is guarded by a mutex, so calls from multiple
threads are serialized and safe. They are not isolated transactions: readers
can observe intermediate states while a batch is being applied.

## Type stubs

The wheel ships `__init__.pyi` and a `py.typed` marker, so mypy and Pyright
pick up signatures without extra configuration.

Full documentation, the rules tour, and benchmarks live in the
[main repository](https://github.com/MatthewSherlin/mushroomdb).
