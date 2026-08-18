//! cargo run -p core-api --example predicates_ii
use core_api::{GraphDb, Predicate, RuleDef, Value};

fn list(xs: &[f64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Float).collect())
}

fn main() {
    let dir = std::env::temp_dir().join(format!("graphdb-p7-ex-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).expect("open");
    for (label, key, field, val) in [
        ("Person", "ada", "founded_year", Value::Int(1998)),
        ("Person", "bob", "founded_year", Value::Float(2000.0)),
        ("Office", "paris", "loc", list(&[48.8566, 2.3522])),
        ("Office", "london", "loc", list(&[51.5074, -0.1278])),
        ("Doc", "d1", "emb", list(&[1.0, 0.0])),
        ("Doc", "d2", "emb", list(&[1.0, 0.0])),
    ] {
        db.insert_node(label, key, vec![(field.into(), val)])
            .unwrap();
    }
    let rules = [
        (
            "founded_near",
            "Person",
            Predicate::NumericWithin {
                field: "founded_year".into(),
                tolerance: 3.0,
            },
            "FOUNDED_NEAR",
        ),
        (
            "office_near",
            "Office",
            Predicate::GeoRadius {
                field: "loc".into(),
                km: 400.0,
            },
            "OFFICE_NEAR",
        ),
        (
            "doc_sim",
            "Doc",
            Predicate::VectorSimilar {
                field: "emb".into(),
                min: 0.9,
            },
            "DOC_SIM",
        ),
    ];
    for (name, label, predicate, et) in rules {
        db.create_rule(RuleDef {
            name: name.into(),
            src_label: label.into(),
            dst_label: label.into(),
            predicate,
            edge_type: et.into(),
            weight_prop: Some("score".into()),
            max_edges: None,
        })
        .unwrap();
    }
    println!("nodes={} edges={}", db.node_count(), db.edge_count());
    for (a, b) in [("ada", "bob"), ("paris", "london"), ("d1", "d2")] {
        for e in db.explain(a, b).expect("explain") {
            println!(
                "  {} {}→{} score={:.6}",
                e.rule,
                e.src_key,
                e.dst_key,
                e.weight.unwrap_or(f64::NAN)
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
