//! The `id` property of a node whose `id` equals its key (defect ledger row 128).
//! The engine refuses, on every write path, a write that sets such a node's `id`
//! to another value and any removal of it; restating the key is accepted, and
//! `rename_node` still moves a matching `id` with the key. A node whose `id`
//! differs from its key, or that has none (a store imported with another
//! `key_field`), treats `id` as an ordinary property.

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
fn a_node_whose_id_is_not_its_key_treats_id_as_an_ordinary_property() {
    let mut db = GraphDb::open(&tmp("no-id")).unwrap();
    db.insert_node("P", "bare", vec![]).unwrap();
    db.insert_node("P", "foreign", vec![("id".into(), Value::Int(7))])
        .unwrap();
    db.set_prop("bare", "id", s("other")).unwrap();
    assert_eq!(db.get_prop("bare", "id"), Some(s("other")));
    db.set_prop("foreign", "id", s("x")).unwrap();
    assert_eq!(db.get_prop("foreign", "id"), Some(s("x")));
    assert!(db.remove_prop("foreign", "id").unwrap());
    assert!(db.remove_prop("bare", "id").unwrap());
    db.write_batch(|b| {
        b.set_prop("bare", "id", s("again"));
        b.remove_prop("foreign", "id");
    })
    .unwrap();
    assert_eq!(db.get_prop("bare", "id"), Some(s("again")));
    // Giving it its key makes it a convention node, and it is protected from then on.
    db.set_prop("bare", "id", s("bare")).unwrap();
    assert_refused("now protected", db.set_prop("bare", "id", s("other")));
    assert_refused("now protected", db.remove_prop("bare", "id"));
}

#[test]
fn a_frame_sees_its_own_writes_when_judging_whether_id_is_the_key() {
    let mut db = GraphDb::open(&tmp("in-frame")).unwrap();
    // Inserted with id == key in the frame: protected within the same frame.
    assert_refused(
        "insert then set",
        db.write_batch(|b| {
            b.insert_node("P", "k", vec![("id".into(), s("k"))]);
            b.set_prop("k", "id", s("other"));
        }),
    );
    // Inserted with another id: ordinary within the frame.
    db.write_batch(|b| {
        b.insert_node("P", "f", vec![("id".into(), s("not-f"))]);
        b.set_prop("f", "id", s("also-not-f"));
    })
    .unwrap();
    assert_eq!(db.get_prop("f", "id"), Some(s("also-not-f")));
    // Set to the key and then away in one frame: refused at the second write.
    assert_refused(
        "set key then away",
        db.write_batch(|b| {
            b.set_prop("f", "id", s("f"));
            b.set_prop("f", "id", s("g"));
        }),
    );
    assert_eq!(db.get_prop("f", "id"), Some(s("also-not-f")));
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
fn insert_stores_whatever_id_the_row_has() {
    let mut db = GraphDb::open(&tmp("insert")).unwrap();
    db.insert_node("P", "k", vec![("id".into(), s("other"))])
        .unwrap();
    db.write_batch(|b| {
        b.insert_node("P", "j", vec![("id".into(), Value::Int(3))]);
        b.insert_node("P", "m", vec![("id".into(), s("m"))]);
    })
    .unwrap();
    assert_eq!(db.get_prop("k", "id"), Some(s("other")));
    assert_eq!(db.get_prop("j", "id"), Some(Value::Int(3)));
    // Only the node whose id equals its key got the convention.
    assert_refused("m is protected", db.set_prop("m", "id", s("z")));
    db.set_prop("k", "id", s("z")).unwrap();
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
    // A node whose id is not its key loses it like any omitted property.
    db.insert_node("P", "bob", vec![("id".into(), Value::Int(9))])
        .unwrap();
    db.write_batch(|b| {
        b.insert_node_on_conflict(
            "P",
            "bob",
            vec![("a".into(), Value::Int(1))],
            OnConflict::Replace,
        );
    })
    .unwrap();
    assert_eq!(db.get_prop("bob", "id"), None);
}

#[test]
fn ingest_with_another_key_field_and_an_id_column_succeeds() {
    let mut db = GraphDb::open(&tmp("ingest")).unwrap();
    let opts = IngestOptions {
        key_field: "sku".into(),
        ..IngestOptions::default()
    };
    let row = |sku: &str, id: Option<Value>| {
        let mut m: BTreeMap<String, Value> = BTreeMap::new();
        m.insert("sku".into(), s(sku));
        if let Some(id) = id {
            m.insert("id".into(), id);
        }
        m
    };
    let report = db
        .ingest(
            "Item",
            vec![
                row("a", Some(Value::Int(7))),
                row("b", Some(s("b"))),
                row("c", None),
                row("d", Some(s("someone-else"))),
            ],
            &opts,
        )
        .unwrap();
    assert!(report.row_errors.is_empty(), "{:?}", report.row_errors);
    assert_eq!(db.get_prop("a", "id"), Some(Value::Int(7)));
    assert_eq!(db.get_prop("d", "id"), Some(s("someone-else")));
    assert_eq!(db.get_prop("c", "id"), None);
    // The rule applies per node: `d` is unprotected, `b` has the convention.
    db.set_prop("d", "id", s("changed")).unwrap();
    db.set_prop("a", "id", Value::Int(8)).unwrap();
    assert_refused("b", db.set_prop("b", "id", s("changed")));
    assert_refused("b", db.remove_prop("b", "id"));
    // The default key field is `id`, so every row has the convention.
    db.ingest_json("V", r#"[{"id": "v1", "n": 1}]"#, &IngestOptions::default())
        .unwrap();
    assert_refused("v1", db.set_prop("v1", "id", s("x")));
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
