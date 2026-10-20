//! `%` and `^` in Cypher expressions, evaluated end to end.

use mushroomdb::{GraphDb, Value};
use std::collections::BTreeMap;

fn eval(db: &GraphDb<core_storage::fs::RealFs>, expr: &str) -> Option<Value> {
    let cypher = format!("MATCH (n:T {{id: 'a'}}) RETURN {expr} AS v");
    let rs = db
        .query(&cypher, &BTreeMap::new())
        .unwrap_or_else(|e| panic!("{cypher}: {e}"));
    assert_eq!(rs.len(), 1, "{cypher}");
    rs.row(0)[0].clone()
}

#[test]
fn modulo_and_power_evaluate_with_the_documented_precedence() {
    let dir = std::env::temp_dir().join(format!("graphdb-arith-ops-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "T",
        "a",
        vec![("x".into(), Value::Int(7)), ("y".into(), Value::Int(3))],
    )
    .unwrap();

    assert_eq!(eval(&db, "7 % 3"), Some(Value::Int(1)));
    assert_eq!(eval(&db, "n.x % n.y"), Some(Value::Int(1)));
    assert_eq!(eval(&db, "10 % 4 * 2"), Some(Value::Int(4)));
    assert_eq!(eval(&db, "7.5 % 2"), Some(Value::Float(1.5)));
    assert_eq!(eval(&db, "2 ^ 8"), Some(Value::Float(256.0)));
    assert_eq!(eval(&db, "-2 ^ 2"), Some(Value::Float(4.0)));
    assert_eq!(eval(&db, "2 ^ 3 ^ 2"), Some(Value::Float(64.0)));
    assert_eq!(eval(&db, "2 * 3 ^ 2"), Some(Value::Float(18.0)));
    assert_eq!(eval(&db, "1 + 2 ^ 3 % 5"), Some(Value::Float(4.0)));
    assert_eq!(eval(&db, "n.x ^ 2"), Some(Value::Float(49.0)));
    assert_eq!(eval(&db, "n.missing % 2"), None);

    let err = db
        .query(
            "MATCH (n:T {id: 'a'}) RETURN n.x % 0 AS v",
            &BTreeMap::new(),
        )
        .expect_err("integer modulo by zero is an error");
    assert!(err.to_string().contains("division by zero"), "{err}");
}
