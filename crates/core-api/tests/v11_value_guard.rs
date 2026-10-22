//! Every write path stores the five 0.10 value types — Date / LocalDateTime /
//! DateTime / Duration / Binary — and moves the store to the V11 snapshot
//! format before the first one reaches the WAL (0.10 TY-1, TY-12, contract C9).
//!
//! A 0.9.x reader cannot decode a frame carrying one and would truncate the
//! WAL there, so the order is the guard: the V11 snapshot is written first,
//! and a 0.9.x reader stops at `snapshot: unsupported version 11` with the WAL
//! untouched. These tests prove it path by path: the write succeeds, the
//! snapshot on disk is V11 and the WAL that was there is still its prefix, and
//! the store reads the same live as after a reopen.
//!
//! A role predicate is the one place a 0.10 value is still refused:
//! `roles.json` is not covered by the snapshot's version stamp.

use mushroomdb::schema::Schema;
use mushroomdb::{IngestOptions, PropPredicate, RealFs, RoleDef, Value};
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
        "{type_name} values cannot be used in a role predicate: roles.json carries no format \
         version, so a 0.9.x reader would report it as corrupt"
    )
}

/// A store with one node, so SET paths have a target.
fn seeded(name: &str) -> (PathBuf, GraphDb) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![("age".into(), Value::Int(30))])
        .unwrap();
    assert!(!db.is_v11());
    (dir, db)
}

/// After a successful write: the store is V11, the WAL that was there before
/// it is still its prefix (the upgrade keeps it), and a reopened store
/// answers `key.field` exactly as the never-closed one does.
fn assert_upgraded_and_stored(
    dir: &Path,
    db: GraphDb,
    wal_before: &[u8],
    key: &str,
    field: &str,
    value: &Value,
) {
    assert!(db.is_v11(), "{key}.{field}: the handle is V11");
    assert_eq!(mushroomdb::snapshot_version_at(dir).unwrap(), Some(11));
    let wal = wal_bytes(dir);
    assert!(
        wal.starts_with(wal_before) && wal.len() > wal_before.len(),
        "the upgrade keeps the WAL and the write is appended to it"
    );
    assert_eq!(db.get_prop(key, field).as_ref(), Some(value), "live");
    let live = db.node_info(key);
    let live_alice = db.node_info("alice");
    let live_count = db.node_count();
    drop(db);
    let reopened = GraphDb::open(dir).unwrap();
    assert!(reopened.is_v11());
    assert_eq!(reopened.node_info(key), live, "reopened");
    assert_eq!(reopened.node_info("alice"), live_alice);
    assert_eq!(reopened.node_count(), live_count);
}

#[test]
fn insert_node_stores_every_v11_type() {
    for (_, value) in v11_values() {
        let (dir, mut db) = seeded("insert-node");
        let before = wal_bytes(&dir);
        db.insert_node("Person", "bob", vec![("born".into(), value.clone())])
            .unwrap();
        assert_upgraded_and_stored(&dir, db, &before, "bob", "born", &value);
    }
}

#[test]
fn set_prop_stores_every_v11_type() {
    for (_, value) in v11_values() {
        let (dir, mut db) = seeded("set-prop");
        let before = wal_bytes(&dir);
        db.set_prop("alice", "born", value.clone()).unwrap();
        assert_upgraded_and_stored(&dir, db, &before, "alice", "born", &value);
    }
}

#[test]
fn set_props_stores_a_v11_value() {
    let (dir, mut db) = seeded("set-props");
    let before = wal_bytes(&dir);
    db.set_props(
        "alice",
        vec![
            ("name".into(), Value::Str("Alice".into())),
            ("born".into(), Value::Date(1)),
        ],
    )
    .unwrap();
    assert_eq!(
        db.get_prop("alice", "name"),
        Some(Value::Str("Alice".into()))
    );
    assert_upgraded_and_stored(&dir, db, &before, "alice", "born", &Value::Date(1));
}

#[test]
fn write_batch_stores_the_whole_batch() {
    let (dir, mut db) = seeded("write-batch");
    let before = wal_bytes(&dir);
    // The V11 value is the last op: the plain ops before it land with it.
    db.write_batch(|b: &mut mushroomdb::BatchBuilder<'_, RealFs>| {
        b.insert_node("Person", "bob", vec![("age".into(), Value::Int(1))]);
        b.insert_edge("KNOWS", "alice", "bob");
        b.set_prop("alice", "blob", Value::Binary(vec![1]));
    })
    .unwrap();
    assert_eq!(db.edge_count(), 1);
    assert_eq!(db.get_prop("bob", "age"), Some(Value::Int(1)));
    assert_upgraded_and_stored(&dir, db, &before, "alice", "blob", &Value::Binary(vec![1]));
}

#[test]
fn ingest_json_stores_a_tagged_value() {
    let (dir, mut db) = seeded("ingest-json");
    let before = wal_bytes(&dir);
    let json = r#"[{"id": "bob", "born": {"$date": "2024-01-15"}}, {"id": "carol"}]"#;
    db.ingest_json("Person", json, &IngestOptions::default())
        .unwrap();
    assert!(db.node_info("carol").is_some());
    assert_upgraded_and_stored(&dir, db, &before, "bob", "born", &Value::Date(19_737));
}

#[test]
fn ingest_rows_store_a_v11_value() {
    let (dir, mut db) = seeded("ingest-rows");
    let before = wal_bytes(&dir);
    let span = Value::Duration {
        months: 1,
        days: 0,
        nanos: 0,
    };
    let row = BTreeMap::from([
        ("id".to_string(), Value::Str("bob".into())),
        ("span".to_string(), span.clone()),
    ]);
    db.ingest("Person", vec![row], &IngestOptions::default())
        .unwrap();
    assert_upgraded_and_stored(&dir, db, &before, "bob", "span", &span);
}

#[test]
fn cypher_set_with_a_v11_parameter_stores_it() {
    let (dir, mut db) = seeded("cypher-set");
    let before = wal_bytes(&dir);
    let params = BTreeMap::from([("d".to_string(), Value::LocalDateTime(5))]);
    db.query_write("MATCH (n:Person) WHERE n.age = 30 SET n.seen = $d", &params)
        .unwrap();
    assert_upgraded_and_stored(&dir, db, &before, "alice", "seen", &Value::LocalDateTime(5));
}

#[test]
fn cypher_merge_with_a_v11_parameter_stores_it() {
    let (dir, mut db) = seeded("cypher-merge");
    let before = wal_bytes(&dir);
    let at = Value::DateTime {
        micros_utc: 0,
        offset_seconds: 0,
    };
    let params = BTreeMap::from([("t".to_string(), at.clone())]);
    db.query_write(
        "MERGE (n:Person {id: 'bob'}) ON CREATE SET n.at = $t",
        &params,
    )
    .unwrap();
    assert_upgraded_and_stored(&dir, db, &before, "bob", "at", &at);
}

/// The upgrade runs before anything is interned: the label and field a V11
/// write introduces replay to the ids the live handle gave them.
#[test]
fn the_upgrade_leaves_interned_names_consistent() {
    let (dir, mut db) = seeded("intern");
    db.insert_node("Fresh", "bob", vec![("fresh_field".into(), Value::Date(0))])
        .unwrap();
    db.set_prop("alice", "other_fresh", Value::Date(0)).unwrap();
    db.insert_node(
        "Later",
        "carol",
        vec![("fresh_field".into(), Value::Int(7))],
    )
    .unwrap();
    let live: Vec<_> = ["alice", "bob", "carol"]
        .iter()
        .map(|k| db.node_info(k))
        .collect();
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    let back: Vec<_> = ["alice", "bob", "carol"]
        .iter()
        .map(|k| reopened.node_info(k))
        .collect();
    assert_eq!(back, live);
    assert_eq!(reopened.node_info("bob").unwrap().label, "Fresh");
    assert_eq!(reopened.node_info("carol").unwrap().label, "Later");
}

#[test]
fn a_query_can_take_and_return_a_v11_parameter_without_upgrading() {
    let (dir, db) = seeded("read-path");
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
    assert!(!db.is_v11());
    assert_eq!(mushroomdb::snapshot_version_at(&dir).unwrap(), None);
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
                .contains("cannot be used in a role predicate"),
            "{err}"
        );
    }
    // The JSON wire forms are not role values either.
    assert!(serde_json::from_str::<PropPredicate>(
        r#"{"field": "born", "eq": {"$date": "2024-01-15"}}"#
    )
    .is_err());
}
