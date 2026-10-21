//! QL-09: `UNWIND` accepts any expression that evaluates to a list.

use mushroomdb::{GraphDb, Value};
use std::collections::BTreeMap;

fn open(tag: &str) -> (GraphDb<core_storage::fs::RealFs>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("graphdb-ql09-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "T",
        "a",
        vec![(
            "tags".into(),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
        )],
    )
    .unwrap();
    (db, dir)
}

fn column(db: &GraphDb<core_storage::fs::RealFs>, q: &str, col: &str) -> Vec<Value> {
    let rs = db.query(q, &BTreeMap::new()).unwrap();
    (0..rs.len())
        .map(|i| rs.get(i, col).cloned().unwrap_or(Value::Null))
        .collect()
}

fn ints(v: &[i64]) -> Vec<Value> {
    v.iter().map(|i| Value::Int(*i)).collect()
}

#[test]
fn unwind_a_range_call() {
    let (db, dir) = open("range");
    assert_eq!(
        column(&db, "UNWIND range(1, 5) AS i RETURN i", "i"),
        ints(&[1, 2, 3, 4, 5])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unwind_a_literal_list_still_works() {
    let (db, dir) = open("lit");
    assert_eq!(
        column(&db, "UNWIND [1, 2, 3] AS x RETURN x", "x"),
        ints(&[1, 2, 3])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unwind_null_and_an_empty_range_yield_no_rows() {
    let (db, dir) = open("empty");
    assert!(column(&db, "UNWIND null AS x RETURN x", "x").is_empty());
    assert!(column(&db, "UNWIND range(1, 0) AS i RETURN i", "i").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unwind_keeps_a_null_element_as_a_row() {
    let (db, dir) = open("nullelem");
    assert_eq!(
        column(&db, "UNWIND [1, null] AS x RETURN x", "x"),
        vec![Value::Int(1), Value::Null]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unwind_a_function_of_a_property_and_a_property() {
    let (db, dir) = open("concat");
    assert_eq!(
        column(
            &db,
            "MATCH (n:T {id: 'a'}) UNWIND tail(n.tags) AS x RETURN x",
            "x"
        ),
        ints(&[2])
    );
    assert_eq!(
        column(
            &db,
            "MATCH (n:T {id: 'a'}) UNWIND n.tags AS x RETURN x",
            "x"
        ),
        ints(&[1, 2])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unwind_a_parameter_list() {
    let (db, dir) = open("param");
    let mut params = BTreeMap::new();
    params.insert("items".to_string(), Value::List(ints(&[7, 8])));
    let rs = db.query("UNWIND $items AS x RETURN x", &params).unwrap();
    assert_eq!(rs.len(), 2);
    assert_eq!(rs.get(1, "x"), Some(&Value::Int(8)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unwind_a_non_list_is_an_error() {
    let (db, dir) = open("scalar");
    let err = db
        .query("UNWIND 1 + 1 AS x RETURN x", &BTreeMap::new())
        .unwrap_err()
        .to_string();
    assert!(err.contains("UNWIND requires a list; got Int"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
