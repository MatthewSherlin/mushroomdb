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
