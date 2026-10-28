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

#[test]
fn plain_create_and_merge_property_maps_take_expressions() {
    let (mut db, dir) = open("plain-create-merge");
    db.query_write(
        "CREATE (n:T {id: 'x', flag: 1 > 0, total: 2 + 3, tags: [1 = 1, null IS NULL]})",
        &no_params(),
    )
    .unwrap();
    assert_eq!(prop(&db, "x", "flag"), Some(Value::Bool(true)));
    assert_eq!(prop(&db, "x", "total"), Some(Value::Int(5)));
    assert_eq!(
        prop(&db, "x", "tags"),
        Some(Value::List(vec![Value::Bool(true), Value::Bool(true)]))
    );
    // MERGE matches against the evaluated key: 'x' already exists.
    let rs = db
        .query_write(
            "MERGE (n:T {id: 'x' + ''}) ON MATCH SET n.hit = 1 RETURN n.flag AS f",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "f"), Some(&Value::Bool(true)));
    assert_eq!(prop(&db, "x", "hit"), Some(Value::Int(1)));
    // And creates under the evaluated key when it is new.
    db.query_write("MERGE (n:T {id: 'y' + 'z'})", &no_params())
        .unwrap();
    assert!(db.has_node("yz"));
    // A computed value that is null, or that reads a variable, is a named error.
    let err = db
        .query_write("CREATE (n:T {id: 'w', v: 1 > null})", &no_params())
        .unwrap_err()
        .to_string();
    assert!(err.contains("null"), "{err}");
    assert!(!db.has_node("w"));
    // Never closed and reopened agree.
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(prop(&db, "x", "flag"), Some(Value::Bool(true)));
    assert!(db.has_node("yz"));
}

/// QL-BX WHERE timing at 100k nodes: the generic filter, the bounded (fused)
/// path with IN, and a NOT over properties half the nodes lack. Prints the
/// median of 9 runs per query; compare two builds by hand. Release only:
/// `cargo test --release -p mushroomdb --test cypher_bool_exprs -- --ignored --nocapture`.
#[test]
#[ignore = "measurement by hand: prints WHERE timings at 100k nodes (release, --ignored), nothing asserted"]
fn bench_where_predicates_at_100k() {
    let dir = tmp("bench-where");
    let mut db = GraphDb::open(&dir).unwrap();
    {
        let mut batch = db.batch();
        for i in 0..100_000i64 {
            let mut props = vec![
                ("a".into(), Value::Int(i % 100)),
                ("b".into(), Value::Int(i % 1000)),
                ("c".into(), Value::Int(i % 7)),
            ];
            if i % 2 == 0 {
                props.push(("d".into(), Value::Int(i % 3)));
            }
            batch.insert_node("L", &format!("k{i}"), props);
        }
        batch.commit().unwrap();
    }
    let queries = [
        "MATCH (n:L) WHERE n.a > 10 AND n.b < 500 OR NOT n.c = 3 RETURN count(*) AS c",
        "MATCH (n:L) WHERE n.a >= 50 AND n.b <> 7 AND n.c IN [1, 2, 4] RETURN n.a LIMIT 100000",
        "MATCH (n:L) WHERE NOT (n.d = 1 AND n.a > 10) OR NOT n.d IN [0, 2] RETURN count(*) AS c",
    ];
    for q in queries {
        let mut runs = Vec::new();
        for _ in 0..9 {
            let t = std::time::Instant::now();
            let rs = db.query(q, &no_params()).unwrap();
            std::hint::black_box(rs.len());
            runs.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        runs.sort_by(|a, b| a.total_cmp(b));
        println!("TIMING median={:.2}ms q={q}", runs[4]);
    }
}
