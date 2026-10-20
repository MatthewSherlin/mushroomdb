//! List and map literals in Cypher expressions, evaluated end to end through
//! the read path, the write path, and a reopened store.

use mushroomdb::{GraphDb, Value};
use std::collections::BTreeMap;

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn open(tag: &str) -> (GraphDb<core_storage::fs::RealFs>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("graphdb-literals-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("T", "a", vec![("x".into(), Value::Int(7))])
        .unwrap();
    (db, dir)
}

#[test]
fn literals_evaluate_on_the_read_path() {
    let (db, dir) = open("read");
    let rs = db
        .query(
            "MATCH (n:T {id: 'a'}) RETURN [n.x, n.x + 1] AS l, {v: n.x, w: [1, 2]} AS m, head([5, 6]) AS h",
            &no_params(),
        )
        .unwrap();
    assert_eq!(
        rs.get(0, "l"),
        Some(&Value::List(vec![Value::Int(7), Value::Int(8)]))
    );
    assert_eq!(
        rs.get(0, "m"),
        Some(&Value::Map(BTreeMap::from([
            ("v".to_string(), Value::Int(7)),
            (
                "w".to_string(),
                Value::List(vec![Value::Int(1), Value::Int(2)])
            ),
        ])))
    );
    assert_eq!(rs.get(0, "h"), Some(&Value::Int(5)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_takes_list_and_map_expressions_and_survives_a_reopen() {
    let (mut db, dir) = open("set");
    db.query_write(
        "MATCH (n:T) WHERE n.id = 'a' SET n.tags = [n.x, 2], n.meta = {k: 'v', n: n.x + 1}",
        &no_params(),
    )
    .unwrap();
    let check = |db: &GraphDb<core_storage::fs::RealFs>| {
        let rs = db
            .query(
                "MATCH (n:T {id: 'a'}) RETURN n.tags AS t, n.meta AS m",
                &no_params(),
            )
            .unwrap();
        assert_eq!(
            rs.get(0, "t"),
            Some(&Value::List(vec![Value::Int(7), Value::Int(2)]))
        );
        assert_eq!(
            rs.get(0, "m"),
            Some(&Value::Map(BTreeMap::from([
                ("k".to_string(), Value::Str("v".into())),
                ("n".to_string(), Value::Int(8)),
            ])))
        );
    };
    check(&db);
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    check(&db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_null_element_in_a_set_literal_is_an_error_and_writes_nothing() {
    let (mut db, dir) = open("setnull");
    let err = db
        .query_write(
            "MATCH (n:T) WHERE n.id = 'a' SET n.tags = [1, n.nope]",
            &no_params(),
        )
        .expect_err("a list cannot hold null");
    assert!(err.to_string().contains("null"), "{err}");
    let rs = db
        .query("MATCH (n:T {id: 'a'}) RETURN n.tags AS t", &no_params())
        .unwrap();
    assert_eq!(rs.get(0, "t"), None);
    let _ = std::fs::remove_dir_all(&dir);
}
