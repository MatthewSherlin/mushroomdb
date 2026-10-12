//! The smallest embedding of mushroomdb: nodes, one rule, a query, and why.
//! Quoted on docs/site/rust.md. Run: cargo run -p mushroomdb --example embed_basics

use core_api::{GraphDb, Predicate, RuleDef, Value};
use std::collections::BTreeMap;

fn skills(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

fn main() -> core_api::Result<()> {
    let dir = std::env::temp_dir().join(format!("mushroomdb-embed-basics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // docs:start
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
    // docs:end

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
