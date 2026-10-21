//! QL-15: list comprehensions through the write path.

use mushroomdb::{GraphDb, Value};
use std::collections::BTreeMap;

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn open(tag: &str) -> (GraphDb<core_storage::fs::RealFs>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("graphdb-listcomp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("T", "a", vec![("x".into(), Value::Int(7))])
        .unwrap();
    (db, dir)
}

#[test]
fn a_comprehension_reads_on_the_read_path() {
    let (db, dir) = open("read");
    let rs = db
        .query(
            "MATCH (n:T {id: 'a'}) RETURN [v IN range(1, n.x) WHERE v > 5 | v * 2] AS l",
            &no_params(),
        )
        .unwrap();
    assert_eq!(
        rs.get(0, "l"),
        Some(&Value::List(vec![Value::Int(12), Value::Int(14)]))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_value_may_be_a_comprehension() {
    let (mut db, dir) = open("set");
    db.query_write(
        "MATCH (n:T) WHERE n.id = 'a' SET n.l = [v IN range(1, n.x) WHERE v > 5]",
        &no_params(),
    )
    .unwrap();
    let rs = db
        .query("MATCH (n:T {id: 'a'}) RETURN n.l AS l", &no_params())
        .unwrap();
    assert_eq!(
        rs.get(0, "l"),
        Some(&Value::List(vec![Value::Int(6), Value::Int(7)]))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The RETURN of a write statement is projected after the commit, so a
/// comprehension there is refused by name *before* anything is written.
#[test]
fn a_comprehension_in_a_write_return_is_refused_and_writes_nothing() {
    let (mut db, dir) = open("writeret");
    let err = db
        .query_write(
            "MATCH (n:T) WHERE n.id = 'a' SET n.y = 1 RETURN [v IN [1, 2] | v] AS l",
            &no_params(),
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("list comprehension is not supported in the RETURN of a write statement"),
        "{err}"
    );
    let rs = db
        .query("MATCH (n:T {id: 'a'}) RETURN n.y AS y", &no_params())
        .unwrap();
    assert_eq!(rs.get(0, "y"), None, "the refused statement must not write");
    // Nested inside another expression is refused the same way.
    let err = db
        .query_write(
            "MATCH (n:T) WHERE n.id = 'a' SET n.y = 1 RETURN size([v IN [1, 2] | v]) AS l",
            &no_params(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("write statement"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
