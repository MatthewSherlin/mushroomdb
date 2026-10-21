//! QL-14: `reduce()` through the read and write paths.

use mushroomdb::{GraphDb, Value};
use std::collections::BTreeMap;

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn open(tag: &str) -> (GraphDb<core_storage::fs::RealFs>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("graphdb-reduce-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "P",
        "a",
        vec![(
            "scores".into(),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        )],
    )
    .unwrap();
    (db, dir)
}

#[test]
fn reduce_reads_a_stored_list() {
    let (db, dir) = open("read");
    let rs = db
        .query(
            "MATCH (n:P) RETURN reduce(acc = 0, x IN n.scores | acc + x) AS total",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "total"), Some(&Value::Int(6)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_value_may_be_a_reduce() {
    let (mut db, dir) = open("set");
    db.query_write(
        "MATCH (n:P) WHERE n.id = 'a' SET n.total = reduce(acc = 0, x IN n.scores | acc + x)",
        &no_params(),
    )
    .unwrap();
    let rs = db
        .query("MATCH (n:P {id: 'a'}) RETURN n.total AS t", &no_params())
        .unwrap();
    assert_eq!(rs.get(0, "t"), Some(&Value::Int(6)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_reduce_in_a_write_return_is_refused_and_writes_nothing() {
    let (mut db, dir) = open("writeret");
    for ret in [
        "reduce(acc = 0, x IN [1, 2] | acc + x)",
        "1 + reduce(acc = 0, x IN [1, 2] | acc + x)",
    ] {
        let err = db
            .query_write(
                &format!("MATCH (n:P) WHERE n.id = 'a' SET n.y = 1 RETURN {ret} AS r"),
                &no_params(),
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("reduce() is not supported in the RETURN of a write statement"),
            "{err}"
        );
    }
    let rs = db
        .query("MATCH (n:P {id: 'a'}) RETURN n.y AS y", &no_params())
        .unwrap();
    assert_eq!(rs.get(0, "y"), None, "the refused statement must not write");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The write-statement RETURN has its own arithmetic evaluator; `+` must
/// mean the same there as in a read query.
#[test]
fn plus_joins_in_a_write_return_as_it_does_in_a_read() {
    let (mut db, dir) = open("plus");
    let exprs = [
        ("'a' + 'b'", Value::Str("ab".into())),
        ("1 + 'a'", Value::Str("1a".into())),
        ("'a' + 1.5", Value::Str("a1.5".into())),
        ("[1] + [2]", Value::List(vec![Value::Int(1), Value::Int(2)])),
        ("[1] + 2", Value::List(vec![Value::Int(1), Value::Int(2)])),
        ("0 + [1]", Value::List(vec![Value::Int(0), Value::Int(1)])),
    ];
    for (expr, want) in exprs {
        let read = db
            .query(&format!("MATCH (n:P) RETURN {expr} AS r"), &no_params())
            .unwrap();
        let write = db
            .query_write(
                &format!("MATCH (n:P) WHERE n.id = 'a' SET n.y = 1 RETURN {expr} AS r"),
                &no_params(),
            )
            .unwrap_or_else(|e| panic!("`{expr}` in a write RETURN failed: {e}"));
        assert_eq!(read.get(0, "r"), Some(&want), "{expr} (read)");
        assert_eq!(write.get(0, "r"), Some(&want), "{expr} (write)");
    }
    let err = db
        .query_write(
            "MATCH (n:P) WHERE n.id = 'a' SET n.y = 1 RETURN 'a' - 1 AS r",
            &no_params(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("must be numeric"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
