//! The `id` property mirrors the node's key (defect ledger row 128). The engine
//! refuses any write that gives a node an `id` other than its key, and any
//! removal of `id`, on every write path; restating the key is accepted, and
//! `rename_node` still moves a matching `id` with the key.

use mushroomdb::{
    BatchOp, GraphError, IngestOptions, OnConflict, Precondition, RealFs, SharedDb, Value,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-key-prop-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn store(name: &str) -> GraphDb {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.insert_node(
        "P",
        "alice",
        vec![("id".into(), s("alice")), ("a".into(), Value::Int(1))],
    )
    .unwrap();
    db
}

fn assert_refused<T: std::fmt::Debug>(what: &str, r: Result<T, GraphError>) {
    match r {
        Err(GraphError::QueryError { detail }) => assert!(
            detail.contains("key property `id`") && detail.contains("rename_node"),
            "{what}: {detail}"
        ),
        other => panic!("{what}: expected the key-property refusal, got {other:?}"),
    }
}

fn untouched(db: &GraphDb, seq: u64) {
    assert_eq!(db.commit_seq(), seq, "a refused write committed");
    assert_eq!(db.get_prop("alice", "id"), Some(s("alice")));
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(1)));
}

#[test]
fn set_prop_and_set_props_refuse_a_different_id_and_accept_the_key() {
    let mut db = store("set");
    let seq = db.commit_seq();
    assert_refused("set_prop", db.set_prop("alice", "id", s("mallory")));
    assert_refused("set_prop int", db.set_prop("alice", "id", Value::Int(7)));
    assert_refused(
        "set_props",
        db.set_props(
            "alice",
            vec![("a".into(), Value::Int(9)), ("id".into(), s("mallory"))],
        ),
    );
    untouched(&db, seq);
    db.set_prop("alice", "id", s("alice")).unwrap();
    db.set_props(
        "alice",
        vec![("id".into(), s("alice")), ("a".into(), Value::Int(2))],
    )
    .unwrap();
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(2)));
    assert_eq!(db.get_prop("alice", "id"), Some(s("alice")));
}

#[test]
fn a_node_with_no_stored_id_may_be_given_its_key_but_nothing_else() {
    let mut db = GraphDb::open(&tmp("no-id")).unwrap();
    db.insert_node("P", "bare", vec![]).unwrap();
    assert_refused("set", db.set_prop("bare", "id", s("other")));
    db.set_prop("bare", "id", s("bare")).unwrap();
    assert_eq!(db.get_prop("bare", "id"), Some(s("bare")));
}

#[test]
fn remove_prop_of_id_is_refused_on_every_path() {
    let mut db = store("remove");
    let seq = db.commit_seq();
    assert_refused("remove_prop", db.remove_prop("alice", "id"));
    assert_refused(
        "batch remove",
        db.write_batch(|b| {
            b.remove_prop("alice", "id");
        }),
    );
    assert_refused(
        "commit_batch",
        db.write_batch_cas(
            vec![],
            vec![BatchOp::RemoveProp {
                key: "alice".into(),
                field: "id".into(),
            }],
        ),
    );
    untouched(&db, seq);
}

#[test]
fn batches_and_cas_refuse_a_different_id() {
    let mut db = store("batch");
    let seq = db.commit_seq();
    assert_refused(
        "batch set_prop",
        db.write_batch(|b| {
            b.set_prop("alice", "a", Value::Int(5));
            b.set_prop("alice", "id", s("mallory"));
        }),
    );
    let last = db.last_changed("alice").unwrap();
    assert_refused(
        "cas",
        db.write_batch_cas(
            vec![Precondition::NodeUnchangedSince {
                key: "alice".into(),
                expected: last,
            }],
            vec![BatchOp::SetProp {
                key: "alice".into(),
                field: "id".into(),
                value: s("mallory"),
            }],
        ),
    );
    untouched(&db, seq);
    db.write_batch(|b| {
        b.set_prop("alice", "id", s("alice"));
        b.set_prop("alice", "a", Value::Int(5));
    })
    .unwrap();
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(5)));
}

#[test]
fn insert_with_an_id_that_is_not_the_key_is_refused() {
    let mut db = GraphDb::open(&tmp("insert")).unwrap();
    assert_refused(
        "insert_node",
        db.insert_node("P", "k", vec![("id".into(), s("other"))]),
    );
    assert_refused(
        "batch insert",
        db.write_batch(|b| {
            b.insert_node("P", "k", vec![("id".into(), Value::Int(3))]);
        }),
    );
    assert!(!db.has_node("k"));
    db.insert_node("P", "k", vec![("id".into(), s("k"))])
        .unwrap();
    assert_eq!(db.get_prop("k", "id"), Some(s("k")));
}

#[test]
fn upsert_replace_refuses_a_different_id_and_keeps_the_stored_one() {
    let mut db = store("replace");
    let seq = db.commit_seq();
    let r = db.write_batch(|b| {
        b.insert_node_on_conflict(
            "P",
            "alice",
            vec![("id".into(), s("mallory")), ("a".into(), Value::Int(9))],
            OnConflict::Replace,
        );
    });
    assert_refused("replace with a different id", r);
    untouched(&db, seq);
    // A replace that leaves `id` out does not remove it.
    db.write_batch(|b| {
        b.insert_node_on_conflict(
            "P",
            "alice",
            vec![("a".into(), Value::Int(3))],
            OnConflict::Replace,
        );
    })
    .unwrap();
    assert_eq!(db.get_prop("alice", "id"), Some(s("alice")));
    assert_eq!(db.get_prop("alice", "a"), Some(Value::Int(3)));
}

#[test]
fn ingest_reports_a_row_whose_id_does_not_match_its_key() {
    let mut db = GraphDb::open(&tmp("ingest")).unwrap();
    // Keyed by `email`: the row's own `id` must be the key or absent.
    let opts = IngestOptions {
        key_field: "email".into(),
        ..IngestOptions::default()
    };
    let row = |email: &str, id: Option<Value>| {
        let mut m: BTreeMap<String, Value> = BTreeMap::new();
        m.insert("email".into(), s(email));
        if let Some(id) = id {
            m.insert("id".into(), id);
        }
        m
    };
    let report = db
        .ingest(
            "U",
            vec![
                row("a@x", Some(Value::Int(7))),
                row("b@x", Some(s("b@x"))),
                row("c@x", None),
                row("d@x", Some(s("someone-else"))),
            ],
            &opts,
        )
        .unwrap();
    let bad: Vec<usize> = report.row_errors.iter().map(|(i, _)| *i).collect();
    assert_eq!(bad, vec![0, 3], "{:?}", report.row_errors);
    assert!(report.row_errors[0].1.contains("does not match the key"));
    assert!(!db.has_node("a@x") && !db.has_node("d@x"));
    assert!(db.has_node("b@x") && db.has_node("c@x"));
    // The default key field `id` is the key, so it always matches.
    let ok = db
        .ingest_json("V", r#"[{"id": "v1", "n": 1}]"#, &IngestOptions::default())
        .unwrap();
    assert!(ok.row_errors.is_empty());
    assert_eq!(db.get_prop("v1", "id"), Some(s("v1")));
}

#[test]
fn rename_still_moves_a_matching_id_with_the_key() {
    let mut db = store("rename");
    db.rename_node("alice", "alicia").unwrap();
    assert_eq!(db.get_prop("alicia", "id"), Some(s("alicia")));
    assert_eq!(db.get_prop("alicia", "a"), Some(Value::Int(1)));
    let mut batch = db.batch();
    batch.rename_node("alicia", "ali");
    batch.commit().unwrap();
    assert_eq!(db.get_prop("ali", "id"), Some(s("ali")));
}

#[test]
fn the_refusal_does_not_apply_on_wal_replay() {
    // Writes are checked, replay never is: a store reopens whatever it holds.
    let dir = tmp("replay");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("P", "k", vec![("id".into(), s("k"))])
            .unwrap();
        let _ = db.set_prop("k", "id", s("nope"));
        db.set_prop("k", "x", Value::Int(1)).unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("k", "id"), Some(s("k")));
    assert_eq!(db.get_prop("k", "x"), Some(Value::Int(1)));
}

#[test]
fn the_shared_handle_and_transactions_refuse_it_too() {
    let dir = tmp("shared");
    let db: SharedDb = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![BatchOp::InsertNode {
        label: "P".into(),
        key: "x".into(),
        props: vec![("id".into(), s("x"))],
    }])
    .unwrap();
    assert!(db
        .submit_batch(vec![BatchOp::SetProp {
            key: "x".into(),
            field: "id".into(),
            value: s("y"),
        }])
        .is_err());
    assert!(db
        .submit_batch(vec![BatchOp::RemoveProp {
            key: "x".into(),
            field: "id".into(),
        }])
        .is_err());
    assert_eq!(
        db.reader().node_info("x").unwrap().props.get("id"),
        Some(&s("x"))
    );
}
