//! Unique degree / typed incidence readout (sidecar exact-reads Task 4).

use core_api::{AlgoDir, Direction, GraphDb, GraphError, PropPredicate, Value};
use core_storage::fs::RealFs;

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-degree-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn open(name: &str) -> (std::path::PathBuf, Db) {
    let dir = tmp(name);
    let db = GraphDb::open(&dir).unwrap();
    (dir, db)
}

fn insert_n(db: &mut Db, keys: &[&str]) {
    for k in keys {
        db.insert_node("N", k, vec![]).unwrap();
    }
}

fn pred_eq(field: &str, value: &str) -> PropPredicate {
    PropPredicate {
        field: field.into(),
        eq: Some(Value::Str(value.into())),
        in_: None,
    }
}

/// After N unique edges of one type out of `a`, degree equals neighbors len.
#[test]
fn degree_matches_neighbors_len() {
    let (_dir, mut db) = open("matches-nbrs");
    insert_n(&mut db, &["a", "n0", "n1", "n2", "n3", "n4"]);
    for i in 0..5 {
        assert!(db.insert_edge("E", "a", &format!("n{i}")).unwrap());
    }
    let deg = db.degree("a", Some("E"), AlgoDir::Out).unwrap();
    let nbrs = db.neighbors("a", "E", Direction::Out).unwrap();
    assert_eq!(deg as usize, nbrs.len());
    assert_eq!(deg, 5);
}

/// Second insert of the same triple is a no-op; unique degree is unchanged.
#[test]
fn duplicate_edge_does_not_increase_degree() {
    let (_dir, mut db) = open("dup-edge");
    insert_n(&mut db, &["a", "b"]);
    assert!(
        db.insert_edge("E", "a", "b").unwrap(),
        "first insert of (E,a,b) must land"
    );
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 1);
    assert!(
        !db.insert_edge("E", "a", "b").unwrap(),
        "second insert of the same triple must return false"
    );
    assert_eq!(
        db.degree("a", Some("E"), AlgoDir::Out).unwrap(),
        1,
        "adjacency is a set; duplicate insert must not increase degree"
    );
}

/// Two components: degrees(keys=[left]) is correct without the right-hand edges.
#[test]
fn degrees_subset_does_not_require_full_graph_walk() {
    let (_dir, mut db) = open("subset");
    insert_n(&mut db, &["a", "b", "x", "y"]);
    assert!(db.insert_edge("E", "a", "b").unwrap());
    assert!(db.insert_edge("E", "x", "y").unwrap());
    let keys = vec!["a".to_string()];
    let got = db
        .degrees(Some(&keys), None, None, Some("E"), AlgoDir::Out, None)
        .unwrap();
    assert_eq!(got, vec![("a".to_string(), 1)]);
}

/// Cap 2 returns the two highest unique degrees, key-asc on ties.
#[test]
fn degrees_where_limit_orders_desc() {
    let (_dir, mut db) = open("where-limit");
    for (k, n_out, scope) in [
        ("a", 3usize, "keep"),
        ("b", 3, "keep"),
        ("c", 1, "keep"),
        ("z", 9, "drop"),
    ] {
        db.insert_node(
            "Document",
            k,
            vec![("scope".into(), Value::Str(scope.into()))],
        )
        .unwrap();
        for i in 0..n_out {
            let dst = format!("{k}-d{i}");
            db.insert_node("Document", &dst, vec![]).unwrap();
            assert!(db.insert_edge("E", k, &dst).unwrap());
        }
    }
    let pred = pred_eq("scope", "keep");
    let got = db
        .degrees(
            None,
            Some("Document"),
            Some(&pred),
            Some("E"),
            AlgoDir::Out,
            Some(2),
        )
        .unwrap();
    assert_eq!(
        got,
        vec![("a".to_string(), 3), ("b".to_string(), 3)],
        "limit 2: highest unique degrees, key-asc on ties; z filtered by where"
    );
}

#[test]
fn degree_unknown_key_is_key_not_found() {
    let (_dir, mut db) = open("unknown-key");
    insert_n(&mut db, &["a"]);
    match db.degree("ghost", None, AlgoDir::Out) {
        Err(GraphError::KeyNotFound { key }) => assert_eq!(key, "ghost"),
        other => panic!("expected KeyNotFound, got {other:?}"),
    }
}

#[test]
fn degrees_omits_unknown_keys() {
    let (_dir, mut db) = open("omit-unknown");
    insert_n(&mut db, &["a", "b"]);
    assert!(db.insert_edge("E", "a", "b").unwrap());
    let keys = vec!["ghost".to_string(), "a".to_string(), "missing".to_string()];
    let got = db
        .degrees(Some(&keys), None, None, Some("E"), AlgoDir::Out, None)
        .unwrap();
    assert_eq!(got, vec![("a".to_string(), 1)]);
}

#[test]
fn degrees_empty_keys_returns_empty() {
    let (_dir, mut db) = open("empty-keys");
    insert_n(&mut db, &["a", "b"]);
    assert!(db.insert_edge("E", "a", "b").unwrap());
    let empty: [String; 0] = [];
    let got = db
        .degrees(Some(&empty), None, None, None, AlgoDir::Both, None)
        .unwrap();
    assert!(got.is_empty());
}

/// Reciprocal pair A→B and B→A: Both is 2 at each endpoint, not 1 (sum, not union).
#[test]
fn degree_direction_both_is_sum() {
    let (_dir, mut db) = open("both-sum");
    insert_n(&mut db, &["a", "b"]);
    assert!(db.insert_edge("E", "a", "b").unwrap());
    assert!(db.insert_edge("E", "b", "a").unwrap());
    assert_eq!(db.degree("a", Some("E"), AlgoDir::Out).unwrap(), 1);
    assert_eq!(db.degree("a", Some("E"), AlgoDir::In).unwrap(), 1);
    assert_eq!(
        db.degree("a", Some("E"), AlgoDir::Both).unwrap(),
        2,
        "Both must sum out+in (reciprocal pair counts 2), not neighbour-set union"
    );
    assert_eq!(db.degree("b", Some("E"), AlgoDir::Both).unwrap(), 2);
}

#[test]
fn degree_unknown_edge_type_is_zero() {
    let (_dir, mut db) = open("unknown-etype");
    insert_n(&mut db, &["a", "b"]);
    assert!(db.insert_edge("E", "a", "b").unwrap());
    assert_eq!(db.degree("a", Some("NOPE"), AlgoDir::Out).unwrap(), 0);
    assert_eq!(db.degree("a", Some("NOPE"), AlgoDir::Both).unwrap(), 0);
}

#[test]
fn degrees_invalid_where_is_query_error() {
    let (_dir, mut db) = open("bad-where");
    insert_n(&mut db, &["a"]);
    let pred = PropPredicate {
        field: "scope".into(),
        eq: Some(Value::Str("a".into())),
        in_: Some(vec![Value::Str("b".into())]),
    };
    let err = db
        .degrees(None, None, Some(&pred), None, AlgoDir::Both, None)
        .expect_err("both eq and in must be refused");
    match err {
        GraphError::QueryError { detail } => {
            assert!(
                detail.contains("where"),
                "QueryError must use validate_named(\"where\"), got {detail}"
            );
            assert!(
                detail.contains("both"),
                "QueryError must name both eq and in, got {detail}"
            );
        }
        other => panic!("expected QueryError, got {other:?}"),
    }
}
