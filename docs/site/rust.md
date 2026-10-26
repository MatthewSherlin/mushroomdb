# Rust

mushroomdb is a Rust library first. The command line, the two servers and the Python module are
built on the crate this page describes.

```sh no-run
cargo add mushroomdb
```

The crate is installed and imported as `mushroomdb`: you write
`use mushroomdb::GraphDb;`. It needs Rust 1.92 or later. Every public item is on
[docs.rs](https://docs.rs/mushroomdb); this page is the handful you start with.

The examples here are not written for the page. Each is part of a program under
[`crates/core-api/examples/`](https://github.com/MatthewSherlin/mushroomdb/tree/main/crates/core-api/examples)
that is built and run on every pull request.

## The shortest program

Nodes, one rule, a query, and why two nodes are linked. From `embed_basics.rs`, which has the
`use` lines and the small `skills` helper around it:

```rust
let mut db = GraphDb::open(&dir)?;

db.insert_node(
    "Org",
    "acme",
    vec![("skills".into(), skills(&["graph", "rust", "search"]))],
)?;
db.insert_node(
    "Person",
    "ada",
    vec![("skills".into(), skills(&["graph", "rust"]))],
)?;

// Link every Person to every Org whose skills overlap theirs by at least half.
db.create_rule(RuleDef {
    name: "skill_fit".into(),
    src_label: "Person".into(),
    dst_label: "Org".into(),
    predicate: Predicate::Overlap {
        field: "skills".into(),
        min: 0.5,
    },
    edge_type: "FIT".into(),
    weight_prop: Some("score".into()),
    max_edges: None,
    approximate: false,
    via_label: None,
    via_edge: None,
    via_dir: None,
    namespace: None,
})?;

// The rule derived the edge; nobody inserted it.
let rows = db.query(
    "MATCH (p:Person)-[r:FIT]->(o:Org) RETURN p, o, r.score AS score",
    &BTreeMap::new(),
)?;
assert_eq!(rows.len(), 1);

// Why are they linked? The rule, the edge type and the score.
let why = db.explain("ada", "acme")?;
assert_eq!(why[0].rule, "skill_fit");

// Change the property and the edge is retracted in the same write.
db.set_prop("ada", "skills", skills(&["cooking"]))?;
assert!(db.explain("ada", "acme")?.is_empty());
```

- `GraphDb::open` creates the directory if it is not there, and holds the store's write lock
  for as long as the handle lives.
- Properties are a `Vec<(String, Value)>`. A `Value` is `Int`, `Float`, `Str`, `Bool`, `List`
  or `Map`.
- `RuleDef` has no default: every field is written out, as above. `max_edges: None` means no
  limit on the edges from one source node.
- `query` takes the statement and its parameters, and returns a `ResultSet`, read with `len()`,
  `columns()` and `get(row, column)`.

## Opening a store

| To | Call |
|---|---|
| Read and write, one handle | `GraphDb::open(dir)` |
| Read only, without taking the write lock | `GraphDb::open_with_options(dir, OpenOptions { read_only: true, ..Default::default() })` |
| Read the store as it was at a past commit | `GraphDb::open_at(dir, commit)` |
| Share one store between threads | `SharedDb::open(dir)` |

A read-only handle refuses every write with `GraphError::ReadOnly`, and opens at once even while
another process is writing. `OpenOptions` has two other fields, `auto_migrate` and `repair_wal`,
both true unless you say otherwise.

## Several writes as one

From `embed_batch.rs`:

```rust
// One WAL frame, one fsync: after a crash, all of it is there or none.
db.write_batch(|batch| {
    batch
        .insert_node(
            "Person",
            "ada",
            vec![("name".into(), Value::Str("Ada".into()))],
        )
        .insert_node(
            "Person",
            "grace",
            vec![("name".into(), Value::Str("Grace".into()))],
        )
        .insert_edge("KNOWS", "ada", "grace");
})?;

// Compare-and-set: write only if `ada` has not changed since we read it.
let seen = db.last_changed("ada").expect("ada exists");
db.set_prop("ada", "name", Value::Str("Ada L.".into()))?; // someone else writes first

let refused = db.write_batch_cas(
    vec![Precondition::NodeUnchangedSince {
        key: "ada".into(),
        expected: seen,
    }],
    vec![BatchOp::SetProp {
        key: "ada".into(),
        field: "name".into(),
        value: Value::Str("A.".into()),
    }],
);
assert!(matches!(refused, Err(GraphError::CasConflict { .. })));
```

- A batch is checked as a whole and then written as one entry in the log with one fsync. After a
  crash, all of it is there or none of it is.
- It is not isolated. A reader in the same process may see a batch part-applied.
- `write_batch_cas` takes its writes as a `Vec<BatchOp>`, not a closure. When a precondition
  fails it returns `GraphError::CasConflict`, which names the key, and writes nothing.
- Create a rule in a batch of its own.

**Error semantics — validate-then-apply:** the closure queues ops without
touching the database. `commit` validates every op before writing anything.
If op N fails validation (duplicate key, unknown key, rule-owned edge) the
entire batch is rejected: no WAL bytes written, no in-memory state changes.

**Crash atomicity, not isolation:** on WAL replay after a crash, a torn
`Batch` frame applies none of its ops. However, while applying a committed
batch in memory, concurrent readers may observe intermediate states. For
isolation, use an interactive transaction; see [Transactions](transactions.md).

The following ops are supported inside `write_batch`:

| Method | Description |
|---|---|
| `b.insert_node(label, key, props)` | Insert a new node |
| `b.insert_edge(edge_type, src, dst)` | Insert a user-owned edge |
| `b.set_prop(key, field, value)` | Set or overwrite a property |
| `b.remove_prop(key, field)` | Remove a property (no-op if absent) |
| `b.delete_edge(edge_type, src, dst)` | Delete a user-owned edge |
| `b.delete_node(key)` | Delete a node and all its incident edges |
| `b.create_rule(def)` | Register a linking rule |
| `b.delete_rule(name)` | Remove a linking rule |

The method-chaining `db.batch()` builder is equivalent; `write_batch` is a
convenience wrapper that auto-commits.

**Precondition types:**

| Precondition | Fails when |
|---|---|
| `NodeUnchangedSince { key, expected }` | `last_changed(key)` is not `expected`. A node that does not exist counts as 0 |
| `NodeAbsent { key }` | Node exists (for insert-only semantics) |

**Touch definition:** `last_changed` is updated on `InsertNode`, `SetProp`,
`RemoveProp`, `InsertEdge` (both endpoints), `DeleteEdge` (both endpoints),
`DeleteNode` (entry removed; `last_changed` returns `None` thereafter). History
markers and rule-management records do not update `last_changed`. Values are
persisted in V8 snapshot section 11 (LAST_CHANGE) and survive restarts.

`SharedDb::submit_batch_cas` gives the same guarantee to a shared handle.

## One store, several threads

From `embed_shared.rs`:

```rust
let db = SharedDb::open(&dir)?;

// Writers on any thread; concurrent batches share one fsync.
let writers: Vec<_> = (0..4)
    .map(|n| {
        let db = db.clone();
        std::thread::spawn(move || {
            db.submit_batch(vec![BatchOp::InsertNode {
                label: "Job".into(),
                key: format!("job-{n}"),
                props: vec![("n".into(), Value::Int(n))],
            }])
        })
    })
    .collect();
for writer in writers {
    writer.join().expect("writer thread")?;
}

// Readers take a guard, never wait on each other, and see every commit.
let rows = db
    .read()
    .query("MATCH (j:Job) RETURN j", &BTreeMap::new())?;
assert_eq!(rows.len(), 4);
```

- `SharedDb` is `Clone`, `Send` and `Sync`. It takes the cross-process lock for each write, not
  for its lifetime, so other processes can write between yours.
- `read()` returns a guard. Do not hold one while calling the same handle again.
- `write()` waits up to two seconds for the lock. If it does not get it, it still returns a
  guard, and every write through that guard fails with `Busy`. `submit_batch` returns when the
  group it was written with has been fsynced.
- `reader()` returns a `ReaderSnapshot`: a view that does not move while you hold it.

[Concurrency](concurrency.md) has the model across processes.

## Durability

`FsyncPolicy` is `Strict`, `Batched` or `Relaxed`. `Strict` is the default: every commit is
fsynced before the call returns. Change it with `db.set_fsync_policy(..)`.

`db.snapshot()` writes the whole store as one file, so that the next open does not replay the
log. [Durability](durability.md) has what a crash can and cannot lose.

## Errors

A call that can fail returns `mushroomdb::Result<T>`. The variants of `GraphError` worth matching
on:

| Variant | Means |
|---|---|
| `KeyNotFound`, `DuplicateKey` | The key is not there; the key is taken |
| `ReadOnly` | A write through a read-only handle |
| `Busy` | Another process is writing right now. Nothing was written; retrying is always safe |
| `CasConflict` | A precondition of a compare-and-set batch did not hold |
| `QueryError` | The statement did not parse, plan or run; the message says which |
| `CommitOutOfRange` | A read at a commit the store no longer holds, or has not reached |
| `RuleOwned` | A write to an edge a rule owns |

## The rest

- **Events.** `subscribe_rule` and its relatives return a `Subscription` you poll for what rules
  did. [Subscriptions](subscriptions.md)
- **Schema as a value.** `Schema` holds rules, views, indexes and roles; `apply_schema` applies
  it and says what changed.
- **Derived properties.** [Views](views.md)
- **Graph algorithms.** [Algorithms](algorithms.md)
- **History.** [Time travel](timetravel.md)
- **Roles and scopes.** [Access control](masks.md)
