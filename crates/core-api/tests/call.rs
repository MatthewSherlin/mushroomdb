//! `CALL ns.name(args) [YIELD …]` (0.10 QL-12) is a read: every read entry
//! point answers it — the handle, `query_write`, an as-of handle and
//! `query_at`, a masked read, a role-scoped write, the lock-free reader and a
//! transaction — with the same rows, and the openCypher forms this release
//! refuses are named on all of them.

use mushroomdb::{
    schema::Schema, GraphError, RealFs, ResultSet, RoleDef, SharedDb, Value, WriteScope,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-call-{name}-{}-{nanos}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

const PING: &str = "CALL db.ping() YIELD success AS ok RETURN ok";

/// The answer `PING` must give everywhere.
fn assert_ping(rs: Result<ResultSet, GraphError>, at: &str) {
    let rs = rs.unwrap_or_else(|e| panic!("{at}: {e}"));
    assert_eq!(rs.columns(), ["ok"], "{at}");
    assert_eq!(rs.len(), 1, "{at}");
    assert_eq!(rs.get(0, "ok"), Some(&Value::Bool(true)), "{at}");
}

fn role_store(dir: &std::path::Path) -> GraphDb {
    let mut db = GraphDb::open(dir).unwrap();
    db.apply_schema(&Schema {
        fulltext: vec![],
        indexes: vec![],
        rules: vec![],
        views: vec![],
        roles: vec![RoleDef {
            name: "writer".into(),
            keys: vec![],
            labels: vec!["P".into()],
            visible_where: None,
            namespaces: None,
            write: Some(WriteScope {
                create_labels: vec!["P".into()],
                update_labels: vec!["P".into()],
                delete_labels: vec!["P".into()],
                create_edge_types: vec![],
                delete_edge_types: vec![],
                update_edge_types: vec![],
            }),
        }],
    })
    .unwrap();
    db.insert_node("P", "p1", vec![]).unwrap();
    db
}

#[test]
fn every_read_entry_point_runs_a_call() {
    let dir = tmp("entry");
    let mut db = role_store(&dir);
    assert_ping(db.query(PING, &p()), "query");
    assert_ping(db.query_write(PING, &p()), "query_write");
    let mask = db.mask_for_role("writer").unwrap();
    assert_ping(db.query_masked(PING, &p(), &mask), "query_masked");
    assert_ping(
        db.query_write_authz("writer", PING, &p()),
        "query_write_authz",
    );
    let snap = db.reader();
    assert_ping(snap.query(PING, &p()), "reader query");
    assert_ping(snap.query_masked(PING, &p(), &mask), "reader query_masked");
    let commit = db.commit_seq() - 1;
    assert_ping(db.query_at(commit, PING, &p()), "query_at");
    let past = GraphDb::open_at(&dir, commit).unwrap();
    assert!(past.is_read_only());
    assert_ping(past.query(PING, &p()), "as-of handle");
    drop(db);

    let shared = SharedDb::open(&tmp("tx")).unwrap();
    let mut tx = shared.begin_transaction();
    assert_ping(tx.execute(PING, &p()), "transaction");
    tx.execute("CREATE (n:P {id: 'x'})", &p()).unwrap();
    assert_ping(tx.execute(PING, &p()), "transaction after a write");
    tx.commit().unwrap();
    assert_ping(shared.reader().query(PING, &p()), "shared reader");
}

#[test]
fn a_call_writes_nothing() {
    let mut db = GraphDb::open(&tmp("nowrite")).unwrap();
    let before = db.commit_seq();
    db.query_write(PING, &p()).unwrap();
    db.query("CALL db.ping()", &p()).unwrap();
    assert_eq!(db.commit_seq(), before);
}

#[test]
fn call_errors_carry_their_stage() {
    let db = GraphDb::open(&tmp("errors")).unwrap();
    let err = db
        .query("CALL unknown.proc()", &p())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("execute: unknown procedure: unknown.proc"),
        "{err}"
    );
    let err = db
        .query("CALL db.ping() YIELD nope", &p())
        .unwrap_err()
        .to_string();
    assert!(err.contains("`nope`"), "{err}");
    let err = db.query("CALL ping()", &p()).unwrap_err().to_string();
    assert!(err.contains("parse:"), "{err}");
    let p_x = BTreeMap::from([("x".to_string(), Value::Int(1))]);
    let err = db.query("CALL db.ping($x)", &p_x).unwrap_err().to_string();
    assert!(err.contains("db.ping takes no arguments, got 1"), "{err}");
}

#[test]
fn refused_forms_are_named_on_the_read_and_write_entry_points() {
    let mut db = GraphDb::open(&tmp("refused")).unwrap();
    let ns = "not supported in mushroomdb 0.10: ";
    for (q, want) in [
        ("CALL { MATCH (n) RETURN n }", "CALL subquery"),
        (
            "MATCH (n) CALL db.ping() RETURN n",
            "CALL inside a larger query",
        ),
        ("FOREACH (x IN [1,2] | CREATE (n:X {id:x}))", "FOREACH"),
        (
            "MATCH (n) WHERE EXISTS { (n)-[:K]->() } RETURN n",
            "EXISTS { } subquery",
        ),
    ] {
        let err = db.query(q, &p()).unwrap_err().to_string();
        assert!(err.contains(&format!("{ns}{want}")), "query {q}: {err}");
        let err = db.query_write(q, &p()).unwrap_err().to_string();
        assert!(
            err.contains(&format!("{ns}{want}")),
            "query_write {q}: {err}"
        );
    }
    for (q, want) in [
        ("MATCH (n) REMOVE n:Label", "REMOVE n:Label (label removal)"),
        ("MATCH (n) SET n:Label", "SET n:Label (label assignment)"),
        ("MATCH (n) FOREACH (x IN [1] | SET n.a = x)", "FOREACH"),
    ] {
        let err = db.query_write(q, &p()).unwrap_err().to_string();
        assert!(
            err.contains(&format!("{ns}{want}")),
            "query_write {q}: {err}"
        );
    }
}

#[test]
fn a_subscription_refuses_a_call_by_name() {
    let mut db = GraphDb::open(&tmp("sub")).unwrap();
    let err = db
        .subscribe_query("CALL db.ping()")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not supported in mushroomdb 0.10: CALL here"),
        "{err}"
    );
}

#[test]
fn a_call_is_not_a_write_query() {
    for q in [
        "CALL db.ping()",
        "CALL db.ping() YIELD success RETURN success",
    ] {
        assert!(!mushroomdb::is_write_query(q).unwrap(), "{q}");
    }
}
