//! Open a store, declare one scored Overlap rule, query the edges it created,
//! and explain why two nodes are linked.
//!
//! ```text
//! cargo run -p core-api --example quickstart
//! ```

use core_api::{GraphDb, Predicate, RuleDef, Value};
use std::collections::BTreeMap;

fn tags(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

fn fmt_value(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            let s = format!("{f}");
            if s.contains('.') || s.contains('e') || s.contains('E') {
                s
            } else {
                format!("{s}.0")
            }
        }
        Value::Str(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::List(xs) => {
            let inner: Vec<String> = xs.iter().map(fmt_value).collect();
            format!("[{}]", inner.join(", "))
        }
    }
}

fn fmt_cell(cell: Option<&Value>) -> String {
    match cell {
        None => "null".into(),
        Some(v) => fmt_value(v),
    }
}

fn main() {
    let dir = std::env::temp_dir().join(format!("graphdb-quickstart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    println!("== open ==");
    let mut db = GraphDb::open(&dir).expect("open database in temp dir");
    println!("store: temp dir");

    db.insert_node(
        "Org",
        "acme",
        vec![("skills".into(), tags(&["graph", "rust", "search"]))],
    )
    .expect("insert org acme");
    db.insert_node(
        "Org",
        "beta",
        vec![("skills".into(), tags(&["sales", "ops"]))],
    )
    .expect("insert org beta");

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
    })
    .expect("create scored Overlap rule");

    // Inserts after create_rule fire the rule immediately.
    db.insert_node(
        "Person",
        "ada",
        vec![("skills".into(), tags(&["graph", "rust", "search"]))],
    )
    .expect("insert person ada");
    db.insert_node(
        "Person",
        "bob",
        vec![("skills".into(), tags(&["graph", "rust"]))],
    )
    .expect("insert person bob");
    db.insert_node("Person", "cara", vec![("skills".into(), tags(&["sales"]))])
        .expect("insert person cara");

    println!("\n== graph ==");
    println!(
        "nodes: {}  edges: {}  (derived FIT from skill_fit)",
        db.node_count(),
        db.edge_count()
    );

    let mut params = BTreeMap::new();
    params.insert("min".into(), Value::Float(0.5));
    let rs = db
        .query(
            "\
MATCH (p:Person)-[r:FIT]->(o:Org)
WHERE r.score >= $min
RETURN p, o, r.score AS score
ORDER BY score DESC, p",
            &params,
        )
        .expect("query FIT edges filtered by rule score");

    println!("\n== query ==");
    println!("columns: {}", rs.columns().join(", "));
    for i in 0..rs.len() {
        let cells: Vec<String> = rs
            .columns()
            .iter()
            .map(|c| format!("{c}={}", fmt_cell(rs.get(i, c))))
            .collect();
        println!("  {}", cells.join("  "));
    }

    let ada = db.node_ref("ada").expect("ada exists");
    println!("\n== grouped_by_edge_type (ada) ==");
    for (etype, nbrs) in ada.grouped_by_edge_type() {
        println!("  {etype}: {}", nbrs.join(", "));
    }

    println!("\n== explain (ada, acme) ==");
    for e in db.explain("ada", "acme").expect("explain ada/acme") {
        let weight = e
            .weight
            .map(|w| fmt_value(&Value::Float(w)))
            .unwrap_or_else(|| "none".into());
        println!(
            "  rule={}  type={}  {}→{}  weight={}",
            e.rule, e.edge_type, e.src_key, e.dst_key, weight
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
