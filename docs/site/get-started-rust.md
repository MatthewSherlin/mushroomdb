# Rust quickstart

Embed a graph in your process: create Alice and Bob, connect them, and query
who knows whom. You need Rust 1.92 or later.

```sh no-run
cargo new mushroomdb-quickstart
cd mushroomdb-quickstart
cargo add mushroomdb
```

Replace `src/main.rs` with this complete program:

```rust
//! A plain graph: two people, a directed relationship, and an exact query.
//! cargo run -p mushroomdb --example graph_quickstart --release -- /fresh/path

use mushroomdb::{GraphDb, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let dir = args.next().map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join(format!(
            "mushroomdb-graph-quickstart-{}",
            std::process::id()
        ))
    });
    if args.next().is_some() {
        return Err("usage: graph_quickstart [fresh-store-path]".into());
    }
    // Refuse an existing path; never delete somebody else's store.
    std::fs::create_dir(&dir)?;
    let mut db = GraphDb::open(&dir)?;
    db.insert_node(
        "Person",
        "alice",
        vec![("name".into(), Value::Str("Alice".into()))],
    )?;
    db.insert_node(
        "Person",
        "bob",
        vec![("name".into(), Value::Str("Bob".into()))],
    )?;
    db.insert_edge("KNOWS", "alice", "bob")?;
    let rows = db.query(
        "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.name, b.name",
        &BTreeMap::new(),
    )?;
    assert_eq!(rows.columns(), &["a.name".to_owned(), "b.name".to_owned()]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows.get(0, "a.name"), Some(&Value::Str("Alice".into())));
    assert_eq!(rows.get(0, "b.name"), Some(&Value::Str("Bob".into())));
    assert_eq!((db.node_count(), db.edge_count()), (2, 1));
    println!("Alice knows Bob");
    drop(db);
    println!("Store retained at {}", dir.display());
    Ok(())
}
```

```sh no-run
cargo run
```

It asserts the exact row and prints `Alice knows Bob`. Each run creates a
fresh temporary directory and prints its path; the store is retained after
close. An optional argument chooses a fresh path. Existing paths are refused.
The same program is [built and run in CI](https://github.com/MatthewSherlin/mushroomdb/blob/release/0.10/crates/core-api/examples/graph_quickstart.rs).

## What's next

- [Rust reference](rust.md): batching, shared handles and errors.
- [Agent memory](quickstart.md): the memory API when your application needs it.
- [Deployment](deployment.md): run a persistent service.
