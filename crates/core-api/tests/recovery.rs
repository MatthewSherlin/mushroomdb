use core_api::{Direction, GraphDb, GraphError, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn basic_write_and_read() {
    let dir = tmp("basic");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "u1", vec![("age".into(), Value::Int(30))])
        .unwrap();
    db.insert_node("Person", "u2", vec![]).unwrap();
    assert!(matches!(
        db.insert_node("Person", "u1", vec![]),
        Err(GraphError::DuplicateKey { .. })
    ));
    assert!(db.insert_edge("KNOWS", "u1", "u2").unwrap());
    assert!(!db.insert_edge("KNOWS", "u1", "u2").unwrap()); // dup edge
    assert!(matches!(
        db.insert_edge("KNOWS", "u1", "ghost"),
        Err(GraphError::KeyNotFound { .. })
    ));
    db.set_prop("u2", "name", Value::Str("bo".into())).unwrap();
    assert_eq!(db.get_prop("u1", "age"), Some(&Value::Int(30)));
    assert_eq!(
        db.neighbors("u1", "KNOWS", Direction::Out).unwrap(),
        vec!["u2"]
    );
    assert_eq!(
        db.neighbors("u2", "KNOWS", Direction::In).unwrap(),
        vec!["u1"]
    );
    assert_eq!(db.node_count(), 2);
    assert_eq!(db.edge_count(), 1);
}

#[test]
fn state_survives_reopen() {
    let dir = tmp("reopen");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Person", "u1", vec![]).unwrap();
        db.insert_node("Person", "u2", vec![("x".into(), Value::Bool(true))])
            .unwrap();
        db.insert_edge("KNOWS", "u1", "u2").unwrap();
    } // drop = crash without shutdown ceremony
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.has_node("u1"));
    assert_eq!(db.get_prop("u2", "x"), Some(&Value::Bool(true)));
    assert_eq!(
        db.neighbors("u1", "KNOWS", Direction::Out).unwrap(),
        vec!["u2"]
    );
    assert_eq!(db.edge_count(), 1);
}
