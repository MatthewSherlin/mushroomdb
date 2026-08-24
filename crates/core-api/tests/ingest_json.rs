use core_api::{Direction, FkSkip, GraphDb, GraphError, IngestOptions, Value};
use core_storage::fs::RealFs;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Binding: a raw JSON array of objects ingests, stores props, and auto-declares FK.
#[test]
fn ingest_json_happy_path_auto_fk() {
    let dir = tmp("ingest-json-happy");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "acme", vec![]).unwrap();

    let report = db
        .ingest_json(
            "Person",
            r#"[{"id":"p1","org_id":"acme","name":"ada"}]"#,
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.inserted, 1);
    assert!(report.row_errors.is_empty());
    assert_eq!(
        report.rules_created,
        vec!["auto_fk_person_org_id".to_string()]
    );
    assert!(db.has_node("p1"));
    assert_eq!(db.get_prop("p1", "name"), Some(&Value::Str("ada".into())));
    assert_eq!(
        db.neighbors("p1", "ORG", Direction::Out).unwrap(),
        vec!["acme".to_string()]
    );
}

/// Binding: integral JSON numbers become Int; non-integral become Float.
#[test]
fn ingest_json_int_and_float_coercion() {
    let dir = tmp("ingest-json-nums");
    let mut db = GraphDb::open(&dir).unwrap();

    db.ingest_json(
        "Person",
        r#"[{"id":"p1","age":30,"score":1.5,"ok":true}]"#,
        &IngestOptions::default(),
    )
    .unwrap();

    assert_eq!(db.get_prop("p1", "age"), Some(&Value::Int(30)));
    assert_eq!(db.get_prop("p1", "score"), Some(&Value::Float(1.5)));
    assert_eq!(db.get_prop("p1", "ok"), Some(&Value::Bool(true)));
}

/// Binding: JSON null fields are silently skipped — not stored, not a row error.
#[test]
fn ingest_json_null_fields_are_skipped() {
    let dir = tmp("ingest-json-null");
    let mut db = GraphDb::open(&dir).unwrap();

    let report = db
        .ingest_json(
            "Person",
            r#"[{"id":"p1","name":null,"age":1}]"#,
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.inserted, 1);
    assert!(report.row_errors.is_empty());
    assert_eq!(db.get_prop("p1", "age"), Some(&Value::Int(1)));
    assert_eq!(db.get_prop("p1", "name"), None);
}

/// Binding: nested object (or array-of-objects) is a per-row error; row is skipped.
#[test]
fn ingest_json_nested_object_is_row_error() {
    let dir = tmp("ingest-json-nested");
    let mut db = GraphDb::open(&dir).unwrap();

    let report = db
        .ingest_json(
            "Person",
            r#"[{"id":"p1","meta":{"x":1}},{"id":"p2"},{"id":"p3","items":[{"a":1}]}]"#,
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.inserted, 1);
    assert!(db.has_node("p2"));
    assert!(!db.has_node("p1"));
    assert!(!db.has_node("p3"));
    assert_eq!(report.row_errors.len(), 2);
    assert_eq!(report.row_errors[0].0, 0);
    assert!(
        report.row_errors[0].1.contains("meta"),
        "got {:?}",
        report.row_errors[0]
    );
    assert_eq!(report.row_errors[1].0, 2);
    assert!(
        report.row_errors[1].1.contains("items"),
        "got {:?}",
        report.row_errors[1]
    );
}

/// Binding: `[1, null]` is a per-row error with a distinct mixed/null message.
#[test]
fn ingest_json_array_with_null_is_mixed_element_row_error() {
    let dir = tmp("ingest-json-mixed-arr");
    let mut db = GraphDb::open(&dir).unwrap();

    let report = db
        .ingest_json(
            "Person",
            r#"[{"id":"p1","tags":[1,null]},{"id":"p2"}]"#,
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(report.inserted, 1);
    assert!(db.has_node("p2"));
    assert!(!db.has_node("p1"));
    assert_eq!(report.row_errors.len(), 1);
    assert_eq!(report.row_errors[0].0, 0);
    assert!(
        report.row_errors[0]
            .1
            .contains("mixed or null element in array field tags"),
        "got {:?}",
        report.row_errors[0]
    );
}

/// Binding: parse/shape failures are GraphError::IngestError, not applied.
#[test]
fn ingest_json_top_level_not_array_is_ingest_error() {
    let dir = tmp("ingest-json-shape");
    let mut db = GraphDb::open(&dir).unwrap();

    for payload in ["{}", "not-json", "[1,2]", r#"[{"id":"p1"}, 2]"#] {
        let err = db
            .ingest_json("Person", payload, &IngestOptions::default())
            .unwrap_err();
        assert!(
            matches!(err, GraphError::IngestError { .. }),
            "payload {payload:?} got {err:?}"
        );
        assert!(!db.has_node("p1"));
    }
}

/// Binding: FkSkip exposes field and reason (replaces the old tuple).
#[test]
fn fk_skip_field_and_reason_are_accessible() {
    let dir = tmp("ingest-json-fkskip");
    let mut db = GraphDb::open(&dir).unwrap();

    let report = db
        .ingest_json(
            "Person",
            r#"[{"id":"p1","dept_id":"ghost"}]"#,
            &IngestOptions::default(),
        )
        .unwrap();

    assert_eq!(
        report.skipped_fk_fields,
        vec![FkSkip {
            field: "dept_id".into(),
            reason: "no matching target keys".into(),
        }]
    );
    let skip = &report.skipped_fk_fields[0];
    assert_eq!(skip.field, "dept_id");
    assert_eq!(skip.reason, "no matching target keys");
}

/// Binding: format version lives on GraphDb, not Stats.
#[test]
fn format_version_is_six() {
    assert_eq!(GraphDb::<RealFs>::format_version(), 6);
}
