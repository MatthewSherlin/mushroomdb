//! QL-13: temporal functions through the public API. A query may produce a
//! temporal value; storing one is refused until the V11 format lands.

use mushroomdb::{GraphDb, GraphError, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-cypher-temporal-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn wal_len(dir: &std::path::Path) -> u64 {
    std::fs::metadata(dir.join("wal.bin")).map_or(0, |m| m.len())
}

#[test]
fn a_query_returns_temporal_values() {
    let dir = tmp("return");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "T",
        "a",
        vec![("s".into(), Value::Str("2024-03-15".into()))],
    )
    .unwrap();
    let rs = db
        .query(
            "MATCH (n:T) RETURN date(n.s) AS d, date(n.s) + duration({days: 1}) AS next",
            &no_params(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "d"), Some(&Value::Date(19_797)));
    assert_eq!(rs.get(0, "next"), Some(&Value::Date(19_798)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn setting_a_temporal_value_gets_the_named_v11_refusal() {
    let dir = tmp("set");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("T", "a", vec![("x".into(), Value::Int(1))])
        .unwrap();
    let before = wal_len(&dir);
    for expr in [
        "date('2024-01-01')",
        "datetime('2024-01-01T00:00:00Z')",
        "localdatetime('2024-01-01T00:00:00')",
        "duration({days: 1})",
    ] {
        let err = db
            .query_write(&format!("MATCH (n:T) SET n.d = {expr}"), &no_params())
            .unwrap_err();
        match err {
            GraphError::QueryError { detail } => assert!(
                detail.contains("not supported in mushroomdb 0.10 yet")
                    && detail.contains("cannot be stored until the V11 format lands"),
                "{expr}: {detail}"
            ),
            other => panic!("{expr}: expected the V11 refusal, got {other:?}"),
        }
    }
    assert_eq!(wal_len(&dir), before, "a refused write reached the WAL");
    assert_eq!(db.get_prop("a", "d"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_invalid_date_in_set_is_the_evaluated_to_null_refusal() {
    let dir = tmp("null");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("T", "a", vec![("x".into(), Value::Int(1))])
        .unwrap();
    let err = db
        .query_write("MATCH (n:T) SET n.d = date('nope')", &no_params())
        .unwrap_err();
    assert!(err.to_string().contains("evaluated to null"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
