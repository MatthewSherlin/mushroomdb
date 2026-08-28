use core_api::{GraphDb, Value};
use std::collections::BTreeMap;

fn m(pairs: &[(&str, Value)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn map_value_roundtrips_through_wal_and_snapshot() {
    let dir = tmp("map-roundtrip");
    let nested = m(&[
        ("city", Value::Str("berlin".into())),
        (
            "scores",
            Value::List(vec![Value::Int(1), m(&[("deep", Value::Bool(true))])]),
        ),
    ]);
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![("meta".into(), nested.clone())])
            .unwrap();
    }
    // WAL replay
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("a", "meta"), Some(nested.clone()));
    drop(db);
    // snapshot (V7 pack) roundtrip
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.snapshot().unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("a", "meta"), Some(nested.clone()));
}

#[test]
fn map_equality_in_cypher_where() {
    let dir = tmp("map-cypher-eq");
    let mut db = GraphDb::open(&dir).unwrap();
    let meta = m(&[("k", Value::Int(1))]);
    db.insert_node("N", "a", vec![("meta".into(), meta.clone())])
        .unwrap();
    db.insert_node("N", "b", vec![("meta".into(), m(&[("k", Value::Int(2))]))])
        .unwrap();
    let mut params = BTreeMap::new();
    params.insert("m".to_string(), meta);
    let rs = db
        .query("MATCH (n:N) WHERE n.meta = $m RETURN n.meta", &params)
        .unwrap();
    assert_eq!(rs.len(), 1);
}
