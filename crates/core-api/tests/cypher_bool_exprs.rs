//! QL-BX: a comparison or boolean expression is a value in a write statement:
//! a SET value, a per-row CREATE property, and a write statement's RETURN.
//! A stored boolean is an ordinary `Value::Bool`; a null predicate follows the
//! existing null rules.

use mushroomdb::{GraphDb, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-cypher-bool-exprs-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

/// `a` scores 12, `b` scores 5, `c` has no score.
fn open(tag: &str) -> (GraphDb<core_storage::fs::RealFs>, std::path::PathBuf) {
    let dir = tmp(tag);
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "a", vec![("score".into(), Value::Int(12))])
        .unwrap();
    db.insert_node("P", "b", vec![("score".into(), Value::Int(5))])
        .unwrap();
    db.insert_node("P", "c", vec![]).unwrap();
    (db, dir)
}

fn prop(db: &GraphDb<core_storage::fs::RealFs>, key: &str, field: &str) -> Option<Value> {
    db.get_prop(key, field)
}

#[test]
fn set_value_may_be_a_comparison_and_is_stored_as_a_bool() {
    let (mut db, dir) = open("set-cmp");
    db.query_write(
        "MATCH (n:P) WHERE n.score IS NOT NULL SET n.flag = n.score > 10",
        &no_params(),
    )
    .unwrap();
    assert_eq!(prop(&db, "a", "flag"), Some(Value::Bool(true)));
    assert_eq!(prop(&db, "b", "flag"), Some(Value::Bool(false)));
    // Never closed and reopened agree.
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(prop(&db, "a", "flag"), Some(Value::Bool(true)));
    assert_eq!(prop(&db, "b", "flag"), Some(Value::Bool(false)));
    let rs = db
        .query(
            "MATCH (n:P) WHERE n.flag = true RETURN n.key AS k",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "k"), Some(&Value::Str("a".into())));
}

#[test]
fn a_set_value_reads_an_earlier_item_through_not() {
    let (mut db, _dir) = open("set-not");
    db.query_write(
        "MATCH (n:P {id: 'a'}) SET n.flag = n.score > 10, n.off = NOT n.flag",
        &no_params(),
    )
    .unwrap();
    assert_eq!(prop(&db, "a", "flag"), Some(Value::Bool(true)));
    assert_eq!(prop(&db, "a", "off"), Some(Value::Bool(false)));
    db.query_write(
        "MATCH (n:P {id: 'b'}) SET n.both = (n.score < 10 AND true) XOR false",
        &no_params(),
    )
    .unwrap();
    assert_eq!(prop(&db, "b", "both"), Some(Value::Bool(true)));
}

#[test]
fn a_null_predicate_as_a_set_value_is_the_existing_null_error() {
    let (mut db, _dir) = open("set-null");
    let err = db
        .query_write(
            "MATCH (n:P {id: 'c'}) SET n.flag = n.score > 10",
            &no_params(),
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("SET value for n.flag evaluated to null"),
        "{err}"
    );
    assert_eq!(prop(&db, "c", "flag"), None);
}

#[test]
fn a_write_return_projects_a_predicate_three_valued() {
    let (mut db, _dir) = open("write-return");
    let rs = db
        .query_write(
            "MATCH (n:P {id: 'a'}) SET n.score = 20 RETURN n.score > 15 AS big, n.missing > 1 AS u, NOT n.score IN [1, 2] AS out",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "big"), Some(&Value::Bool(true)));
    assert_eq!(rs.get(0, "u"), None);
    assert_eq!(rs.get(0, "out"), Some(&Value::Bool(true)));
}

#[test]
fn per_row_writes_take_predicates() {
    let (mut db, _dir) = open("per-row");
    db.query_write(
        "UNWIND [1, 2, 3] AS x CREATE (:Q {id: toString(x), big: x > 1})",
        &no_params(),
    )
    .unwrap();
    assert_eq!(prop(&db, "1", "big"), Some(Value::Bool(false)));
    assert_eq!(prop(&db, "3", "big"), Some(Value::Bool(true)));
    db.query_write(
        "UNWIND ['a', 'b'] AS k MATCH (n:P {id: k}) SET n.top = n.score >= 12 OR k = 'b'",
        &no_params(),
    )
    .unwrap();
    assert_eq!(prop(&db, "a", "top"), Some(Value::Bool(true)));
    assert_eq!(prop(&db, "b", "top"), Some(Value::Bool(true)));
}

#[test]
fn a_write_where_drops_a_row_whose_not_is_null() {
    let (mut db, _dir) = open("write-where-not");
    // `c` has no score: `NOT (null > 10)` is null, so `c` is not written.
    db.query_write(
        "MATCH (n:P) WHERE NOT n.score > 10 SET n.low = true",
        &no_params(),
    )
    .unwrap();
    assert_eq!(prop(&db, "a", "low"), None);
    assert_eq!(prop(&db, "b", "low"), Some(Value::Bool(true)));
    assert_eq!(prop(&db, "c", "low"), None);
    // A write statement's RETURN reads CASE WHEN three-valued too.
    let rs = db
        .query_write(
            "MATCH (n:P {id: 'a'}) SET n.seen = 1 RETURN CASE WHEN NOT n.missing = 1 THEN 't' ELSE 'f' END AS r",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "r"), Some(&Value::Str("f".into())));
}
