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

/// What a write that holds a null is refused with (QL-17).
const NULL_PROPERTY: &str = "a property value cannot contain null";

fn wal_len(dir: &std::path::Path) -> u64 {
    std::fs::metadata(dir.join("wal.bin")).map_or(0, |m| m.len())
}

#[test]
fn a_null_element_in_a_set_literal_is_an_error_and_writes_nothing() {
    let (mut db, dir) = open("setnull");
    let wal_before = wal_len(&dir);
    let err = db
        .query_write(
            "MATCH (n:T) WHERE n.id = 'a' SET n.tags = [1, n.nope]",
            &no_params(),
        )
        .expect_err("a stored list cannot hold null");
    assert!(err.to_string().contains(NULL_PROPERTY), "{err}");
    let rs = db
        .query("MATCH (n:T {id: 'a'}) RETURN n.tags AS t", &no_params())
        .unwrap();
    assert_eq!(rs.get(0, "t"), None);
    assert_eq!(wal_len(&dir), wal_before);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── QL-17: null literal, null elements, queries without MATCH ───────────────

#[test]
fn a_null_element_is_returned_from_the_read_path() {
    let (db, dir) = open("readnull");
    let rs = db
        .query(
            "MATCH (n:T {id: 'a'}) RETURN [n.x, n.missing] AS l, {a: null} AS m, null AS v",
            &no_params(),
        )
        .unwrap();
    assert_eq!(
        rs.get(0, "l"),
        Some(&Value::List(vec![Value::Int(7), Value::Null]))
    );
    assert_eq!(
        rs.get(0, "m"),
        Some(&Value::Map(BTreeMap::from([(
            "a".to_string(),
            Value::Null
        )])))
    );
    assert_eq!(rs.row(0)[2], None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_null_element_is_returned_from_a_write_statement_return() {
    let (mut db, dir) = open("writeret");
    let rs = db
        .query_write(
            "MATCH (n:T) WHERE n.id = 'a' SET n.y = 1 RETURN [n.x, n.missing, null] AS l",
            &no_params(),
        )
        .unwrap();
    assert_eq!(
        rs.get(0, "l"),
        Some(&Value::List(vec![Value::Int(7), Value::Null, Value::Null]))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn queries_without_match_run_against_one_empty_row() {
    let (db, dir) = open("nomatch");
    let rs = db.query("RETURN 1 + 1 AS x", &no_params()).unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs.get(0, "x"), Some(&Value::Int(2)));
    let rs = db
        .query("UNWIND [1, 2, 3] AS x RETURN x", &no_params())
        .unwrap();
    assert_eq!(rs.len(), 3);
    let rs = db.query("WITH 1 AS a RETURN a", &no_params()).unwrap();
    assert_eq!(rs.get(0, "a"), Some(&Value::Int(1)));
    let rs = db.query("UNWIND [] AS x RETURN x", &no_params()).unwrap();
    assert_eq!(rs.len(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn create_with_a_null_list_element_is_refused_and_leaves_no_trace() {
    let (mut db, dir) = open("createnull");
    let wal_before = wal_len(&dir);
    let seq_before = db.commit_seq();
    let err = db
        .query_write("CREATE (:L {id: 'b', p: [1, null]})", &no_params())
        .expect_err("a stored list cannot hold null");
    assert!(err.to_string().contains(NULL_PROPERTY), "{err}");
    assert!(err.to_string().contains("`p`"), "{err}");
    assert_eq!(db.commit_seq(), seq_before);
    assert_eq!(wal_len(&dir), wal_before);
    assert!(db.node_info("b").is_none());
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.node_info("b").is_none());
    assert_eq!(db.commit_seq(), seq_before);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_write_path_refuses_a_null_at_the_choke_point() {
    let (mut db, dir) = open("chokepoint");
    let wal_before = wal_len(&dir);
    let with_null = Value::List(vec![Value::Int(1), Value::Null]);
    let nested = Value::Map(BTreeMap::from([(
        "k".to_string(),
        Value::List(vec![Value::Null]),
    )]));

    let err = db.set_prop("a", "p", with_null.clone()).unwrap_err();
    assert!(err.to_string().contains(NULL_PROPERTY), "{err}");
    let err = db.set_prop("a", "p", Value::Null).unwrap_err();
    assert!(err.to_string().contains(NULL_PROPERTY), "{err}");
    let err = db
        .insert_node("T", "b", vec![("p".into(), nested.clone())])
        .unwrap_err();
    assert!(err.to_string().contains(NULL_PROPERTY), "{err}");

    // A batch is refused whole: the clean op beside the null is not written.
    let mut batch = db.batch();
    batch.set_prop("a", "ok", Value::Int(1));
    batch.set_prop("a", "p", with_null.clone());
    let err = batch.commit().unwrap_err();
    assert!(err.to_string().contains(NULL_PROPERTY), "{err}");

    // A collection mixing a format-V11 type and a null: both refusals sit
    // at the same choke point, and either one stops the write.
    let mixed = Value::List(vec![Value::Date(19_737), Value::Null]);
    let err = db.set_prop("a", "p", mixed).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains(NULL_PROPERTY) || msg.contains("Date values cannot be stored"),
        "{msg}"
    );

    // A parameter that holds a null.
    let params = BTreeMap::from([("l".to_string(), with_null)]);
    let err = db
        .query_write("MATCH (n:T) WHERE n.id = 'a' SET n.p = $l", &params)
        .unwrap_err();
    assert!(err.to_string().contains(NULL_PROPERTY), "{err}");

    assert_eq!(wal_len(&dir), wal_before);
    let check = |db: &GraphDb<core_storage::fs::RealFs>| {
        assert!(db.node_info("b").is_none());
        assert_eq!(db.get_prop("a", "p"), None);
        assert_eq!(db.get_prop("a", "ok"), None);
        assert_eq!(db.get_prop("a", "x"), Some(Value::Int(7)));
    };
    check(&db);
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    check(&db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn null_as_a_whole_property_value_keeps_its_existing_errors() {
    let (mut db, dir) = open("toplevelnull");
    // SET to null is the existing "evaluated to null" refusal.
    let err = db
        .query_write("MATCH (n:T) WHERE n.id = 'a' SET n.x = null", &no_params())
        .unwrap_err();
    assert!(err.to_string().contains("evaluated to null"), "{err}");
    // CREATE with a null literal property is still a parse error.
    let err = db
        .query_write("CREATE (:L {id: 'c', p: null})", &no_params())
        .unwrap_err();
    assert!(err.to_string().contains("cannot be null"), "{err}");
    assert_eq!(db.get_prop("a", "x"), Some(Value::Int(7)));
    assert!(db.node_info("c").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The serde JSON form of every `Value` variant (external tagging, as the
/// `/subscribe` stream carries rows). Pinned so a reorder or rename fails here
/// as well as in core-storage's bincode pin.
#[test]
fn value_serde_json_form_is_pinned() {
    let cases: Vec<(Value, &str)> = vec![
        (Value::Int(-7), r#"{"Int":-7}"#),
        (Value::Float(1.5), r#"{"Float":1.5}"#),
        (Value::Str("s".into()), r#"{"Str":"s"}"#),
        (Value::Bool(true), r#"{"Bool":true}"#),
        (Value::List(vec![Value::Int(1)]), r#"{"List":[{"Int":1}]}"#),
        (
            Value::Map(BTreeMap::from([("k".to_string(), Value::Int(2))])),
            r#"{"Map":{"k":{"Int":2}}}"#,
        ),
        (Value::Date(19_737), r#"{"Date":19737}"#),
        (
            Value::LocalDateTime(1_705_322_096_789_012),
            r#"{"LocalDateTime":1705322096789012}"#,
        ),
        (
            Value::DateTime {
                micros_utc: 1_705_302_296_789_012,
                offset_seconds: 19_800,
            },
            r#"{"DateTime":{"micros_utc":1705302296789012,"offset_seconds":19800}}"#,
        ),
        (
            Value::Duration {
                months: 14,
                days: 3,
                nanos: 14_706_789_000_000,
            },
            r#"{"Duration":{"months":14,"days":3,"nanos":14706789000000}}"#,
        ),
        (Value::Binary(vec![0xDE, 0xAD]), r#"{"Binary":[222,173]}"#),
        (Value::Null, r#""Null""#),
    ];
    for (v, json) in cases {
        assert_eq!(serde_json::to_string(&v).unwrap(), json);
        let back: Value = serde_json::from_str(json).unwrap();
        assert_eq!(back, v);
    }
}

/// QL-17 through the TX-6 reader: a lock-free snapshot evaluates null
/// elements, three-valued comparisons and a MATCH-less query the same way.
#[test]
fn the_reader_snapshot_evaluates_nulls_the_same_way() {
    let (db, dir) = open("reader");
    let snap = db.reader();
    let rs = snap
        .query("MATCH (n:T) RETURN [n.x, n.missing] AS l", &no_params())
        .unwrap();
    assert_eq!(
        rs.get(0, "l"),
        Some(&Value::List(vec![Value::Int(7), Value::Null]))
    );
    let rs = snap
        .query(
            "MATCH (n:T) WHERE n.x <> null RETURN n.x AS x LIMIT 5",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.len(), 0);
    let rs = snap
        .query("UNWIND [1, null] AS x RETURN x", &no_params())
        .unwrap();
    assert_eq!(rs.len(), 2);
    assert_eq!(rs.row(1)[0], None);
    let _ = std::fs::remove_dir_all(&dir);
}
