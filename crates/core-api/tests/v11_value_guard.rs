//! Until the V11 format lands, no Date / LocalDateTime / DateTime / Duration /
//! Binary value may reach the WAL (0.10 TY-1).
//!
//! A 0.9.x reader cannot decode a frame carrying one and stops replaying there,
//! silently losing every later commit, while the store carries no stamp telling
//! it to refuse. So every write path refuses such a value before anything is
//! appended, and these tests prove it path by path: the write fails with the
//! refusal, `wal.bin` is byte-for-byte unchanged, and the store reads the same
//! live as after a reopen.
//!
//! Reads are not refused: a query can still take a typed parameter and return
//! it.

use mushroomdb::schema::Schema;
use mushroomdb::{GraphError, IngestOptions, PropPredicate, RealFs, RoleDef, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "graphdb-v11-guard-{}-{}-{}",
        name,
        std::process::id(),
        nanos,
    ))
}

fn wal_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("wal.bin")).unwrap_or_default()
}

/// One example of each format-V11 type, plus one nested in a list.
fn v11_values() -> Vec<(&'static str, Value)> {
    vec![
        ("Date", Value::Date(19_737)),
        ("LocalDateTime", Value::LocalDateTime(1_705_322_096_789_012)),
        (
            "DateTime",
            Value::DateTime {
                micros_utc: 1_705_302_296_789_012,
                offset_seconds: 19_800,
            },
        ),
        (
            "Duration",
            Value::Duration {
                months: 14,
                days: 3,
                nanos: 1,
            },
        ),
        ("Binary", Value::Binary(vec![0xDE, 0xAD])),
        ("Date", Value::List(vec![Value::Int(1), Value::Date(0)])),
    ]
}

fn expected_refusal(type_name: &str) -> String {
    format!(
        "not supported in mushroomdb 0.10 yet: {type_name} values cannot be stored until the \
         V11 format lands"
    )
}

fn assert_refused(result: Result<impl std::fmt::Debug, GraphError>, type_name: &str) {
    match result {
        Err(GraphError::QueryError { detail }) => assert_eq!(detail, expected_refusal(type_name)),
        other => panic!("expected the V11 refusal for {type_name}, got {other:?}"),
    }
}

/// A store with one node, so SET paths have a target.
fn seeded(name: &str) -> (PathBuf, GraphDb) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![("age".into(), Value::Int(30))])
        .unwrap();
    (dir, db)
}

/// After a refused write: nothing appended, the store unchanged, and a
/// reopened store answers exactly as the never-closed one does.
fn assert_nothing_written(dir: &Path, db: GraphDb, wal_before: &[u8]) {
    assert_eq!(
        wal_bytes(dir),
        wal_before,
        "a refused write appended to the WAL"
    );
    let live = db.node_info("alice").expect("alice survives");
    assert_eq!(live.props.get("age"), Some(&Value::Int(30)));
    assert!(db.node_info("bob").is_none());
    let live_count = db.node_count();
    drop(db);
    let reopened = GraphDb::open(dir).unwrap();
    assert_eq!(reopened.node_info("alice"), Some(live));
    assert_eq!(reopened.node_count(), live_count);
}

#[test]
fn insert_node_refuses_every_v11_type() {
    for (type_name, value) in v11_values() {
        let (dir, mut db) = seeded("insert-node");
        let before = wal_bytes(&dir);
        assert_refused(
            db.insert_node("Person", "bob", vec![("born".into(), value)]),
            type_name,
        );
        assert_nothing_written(&dir, db, &before);
    }
}

#[test]
fn set_prop_refuses_every_v11_type() {
    for (type_name, value) in v11_values() {
        let (dir, mut db) = seeded("set-prop");
        let before = wal_bytes(&dir);
        assert_refused(db.set_prop("alice", "born", value), type_name);
        assert_nothing_written(&dir, db, &before);
    }
}

#[test]
fn set_props_refuses_a_v11_value() {
    let (dir, mut db) = seeded("set-props");
    let before = wal_bytes(&dir);
    assert_refused(
        db.set_props(
            "alice",
            vec![
                ("name".into(), Value::Str("Alice".into())),
                ("born".into(), Value::Date(1)),
            ],
        ),
        "Date",
    );
    assert_nothing_written(&dir, db, &before);
}

#[test]
fn write_batch_refuses_the_whole_batch() {
    let (dir, mut db) = seeded("write-batch");
    let before = wal_bytes(&dir);
    // The V11 value is the last op: the plain ops before it must not land.
    let result = db.write_batch(|b: &mut mushroomdb::BatchBuilder<'_, RealFs>| {
        b.insert_node("Person", "bob", vec![("age".into(), Value::Int(1))]);
        b.insert_edge("KNOWS", "alice", "bob");
        b.set_prop("alice", "blob", Value::Binary(vec![1]));
    });
    assert_refused(result, "Binary");
    assert_nothing_written(&dir, db, &before);
}

#[test]
fn ingest_json_refuses_a_tagged_value() {
    let (dir, mut db) = seeded("ingest-json");
    let before = wal_bytes(&dir);
    let json = r#"[{"id": "bob", "born": {"$date": "2024-01-15"}}, {"id": "carol"}]"#;
    assert_refused(
        db.ingest_json("Person", json, &IngestOptions::default()),
        "Date",
    );
    assert_nothing_written(&dir, db, &before);
}

#[test]
fn ingest_rows_refuse_a_v11_value() {
    let (dir, mut db) = seeded("ingest-rows");
    let before = wal_bytes(&dir);
    let row = BTreeMap::from([
        ("id".to_string(), Value::Str("bob".into())),
        (
            "span".to_string(),
            Value::Duration {
                months: 1,
                days: 0,
                nanos: 0,
            },
        ),
    ]);
    assert_refused(
        db.ingest("Person", vec![row], &IngestOptions::default()),
        "Duration",
    );
    assert_nothing_written(&dir, db, &before);
}

#[test]
fn cypher_set_with_a_v11_parameter_is_refused() {
    let (dir, mut db) = seeded("cypher-set");
    let before = wal_bytes(&dir);
    let params = BTreeMap::from([("d".to_string(), Value::LocalDateTime(5))]);
    assert_refused(
        db.query_write("MATCH (n:Person) WHERE n.age = 30 SET n.seen = $d", &params),
        "LocalDateTime",
    );
    assert_nothing_written(&dir, db, &before);
}

#[test]
fn cypher_merge_with_a_v11_parameter_is_refused() {
    let (dir, mut db) = seeded("cypher-merge");
    let before = wal_bytes(&dir);
    let params = BTreeMap::from([(
        "t".to_string(),
        Value::DateTime {
            micros_utc: 0,
            offset_seconds: 0,
        },
    )]);
    assert_refused(
        db.query_write(
            "MERGE (n:Person {id: 'bob'}) ON CREATE SET n.at = $t",
            &params,
        ),
        "DateTime",
    );
    assert_nothing_written(&dir, db, &before);
}

#[test]
fn a_refused_write_leaves_no_interned_name_behind() {
    // The refused insert names a label and a field nothing else uses. If either
    // were interned without its `Intern` record reaching the WAL, the next
    // successful write would log ids that replay does not reproduce.
    let (dir, mut db) = seeded("intern-rollback");
    assert!(db
        .insert_node("Fresh", "bob", vec![("fresh_field".into(), Value::Date(0))])
        .is_err());
    assert!(db.set_prop("alice", "other_fresh", Value::Date(0)).is_err());
    db.insert_node(
        "Later",
        "carol",
        vec![("fresh_field".into(), Value::Int(7))],
    )
    .unwrap();
    db.set_prop("alice", "other_fresh", Value::Int(8)).unwrap();
    let live_carol = db.node_info("carol").unwrap();
    let live_alice = db.node_info("alice").unwrap();
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.node_info("carol"), Some(live_carol));
    assert_eq!(reopened.node_info("alice"), Some(live_alice));
    assert_eq!(
        reopened.node_info("carol").unwrap().label,
        "Later",
        "carol's label must replay as written"
    );
}

#[test]
fn a_refused_write_does_not_poison_the_handle() {
    let (dir, mut db) = seeded("not-poisoned");
    assert!(db.set_prop("alice", "x", Value::Binary(vec![])).is_err());
    db.set_prop("alice", "x", Value::Str("ok".into())).unwrap();
    db.snapshot().unwrap();
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(
        reopened.node_info("alice").unwrap().props.get("x"),
        Some(&Value::Str("ok".into()))
    );
}

#[test]
fn a_query_can_still_take_and_return_a_v11_parameter() {
    let (_dir, db) = seeded("read-path");
    for (_, value) in v11_values() {
        let params = BTreeMap::from([("d".to_string(), value.clone())]);
        let rs = db
            .query("MATCH (n:Person) RETURN $d AS d", &params)
            .unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs.row(0)[0].as_ref(), Some(&value));
    }
    // A typed parameter also filters without error; nothing stored matches.
    let params = BTreeMap::from([("d".to_string(), Value::Date(0))]);
    let rs = db
        .query("MATCH (n:Person) WHERE n.age = $d RETURN n.age", &params)
        .unwrap();
    assert_eq!(rs.len(), 0);
}

fn role_with(pred: PropPredicate) -> RoleDef {
    RoleDef {
        name: "dated".into(),
        keys: vec![],
        labels: vec!["Person".into()],
        visible_where: Some(pred),
        namespaces: None,
        write: None,
    }
}

#[test]
fn a_role_predicate_refuses_a_v11_value() {
    let dir = tmp("role-predicate");
    let mut db = GraphDb::open(&dir).unwrap();
    let pred = PropPredicate {
        field: "born".into(),
        eq: Some(Value::Date(19_737)),
        in_: None,
    };
    let schema = Schema {
        roles: vec![role_with(pred)],
        ..Default::default()
    };
    let err = db.apply_schema(&schema).unwrap_err().to_string();
    assert!(err.contains(&expected_refusal("Date")), "{err}");
    assert!(!dir.join("roles.json").exists(), "nothing persisted");
}

#[test]
fn roles_json_with_a_v11_value_does_not_parse() {
    // The serde-tagged form exists as soon as the variant does; it must not
    // become a way to write a 0.9.x-unreadable roles.json.
    for j in [
        r#"{"field": "born", "eq": {"Date": 19737}}"#,
        r#"{"field": "born", "in": [{"Str": "x"}, {"Binary": [1]}]}"#,
    ] {
        let err = serde_json::from_str::<PropPredicate>(j).unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot be stored until the V11 format lands"),
            "{err}"
        );
    }
    // The JSON wire forms are not role values either.
    assert!(serde_json::from_str::<PropPredicate>(
        r#"{"field": "born", "eq": {"$date": "2024-01-15"}}"#
    )
    .is_err());
}
