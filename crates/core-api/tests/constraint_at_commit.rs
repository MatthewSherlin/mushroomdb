//! Unique constraints at COMMIT (0.10 TX-21).
//!
//! The check lives at the one function every WAL append passes through (the
//! dense rewrite, contract C2), so an interactive transaction and a concurrent
//! writer meet it with no call of their own. These tests hold that: a
//! transaction's buffered statements are not judged when they are buffered —
//! the commit is — a refused commit leaves no WAL bytes and nothing visible,
//! racing writers on one value produce exactly one winner, and reopening a
//! store keeps the constraint binding (grandfathered duplicates, which replay
//! must not refuse, are held in `constraints.rs`).

use mushroomdb::{BatchOp, GraphError, RealFs, SharedDb, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "mushroomdb-txcc-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn wal_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("wal.bin")).unwrap_or_default()
}

/// A store with `UNIQUE (User, email)` declared.
fn constrained(name: &str) -> (PathBuf, SharedDb) {
    let dir = tmp(name);
    let db = SharedDb::open(&dir).unwrap();
    db.write()
        .create_constraint("uniq_email", "User", "email")
        .unwrap();
    (dir, db)
}

fn user_count(db: &SharedDb) -> usize {
    let rs = db
        .reader()
        .query("MATCH (n:User) RETURN n.id AS id", &p())
        .unwrap();
    rs.len()
}

fn is_violation<T: std::fmt::Debug>(r: &Result<T, GraphError>) -> bool {
    matches!(r, Err(GraphError::ConstraintViolation { .. }))
}

fn insert_user(key: &str, email: &str) -> BatchOp {
    BatchOp::InsertNode {
        label: "User".into(),
        key: key.into(),
        props: vec![("email".into(), Value::Str(email.into()))],
    }
}

#[test]
fn two_concurrent_transactions_inserting_one_value_commit_exactly_once() {
    let (dir, db) = constrained("two-tx");
    // Both begin before either commits, so neither one's snapshot holds the
    // other's node and each buffers its insert without complaint.
    let mut txs = Vec::new();
    for key in ["alice", "bob"] {
        let mut tx = db.begin_transaction();
        tx.execute(
            &format!("CREATE (n:User {{id: '{key}', email: 'same@x'}})"),
            &p(),
        )
        .expect("a buffered statement is not judged against other writers");
        txs.push(tx);
    }
    let barrier = Arc::new(Barrier::new(txs.len()));
    let handles: Vec<_> = txs
        .into_iter()
        .map(|tx| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                tx.commit()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    let losses = results.iter().filter(|r| is_violation(r)).count();
    assert_eq!((wins, losses), (1, 1), "{results:?}");
    assert_eq!(user_count(&db), 1);

    // The refusal is not a WAL record: the store reopens to the same one node.
    drop(db);
    let reopened = SharedDb::open(&dir).unwrap();
    assert_eq!(user_count(&reopened), 1);
    assert_eq!(reopened.read().constraints().len(), 1);
}

#[test]
fn a_transaction_is_judged_at_commit_not_when_a_statement_is_buffered() {
    let (dir, db) = constrained("at-commit");
    db.submit_batch(vec![insert_user("alice", "a@x")]).unwrap();
    let wal_before = wal_bytes(&dir);
    let frames_before = mushroomdb::wal_commit_count_at(&dir).unwrap();

    let mut tx = db.begin_transaction();
    tx.execute("CREATE (n:User {id: 'ok', email: 'fine@x'})", &p())
        .unwrap();
    tx.execute("CREATE (n:User {id: 'bob', email: 'a@x'})", &p())
        .expect("the violating statement buffers; the commit decides");
    // The transaction reads its own buffered writes, violation included.
    let rs = tx
        .execute("MATCH (n:User) RETURN n.id AS id", &p())
        .unwrap();
    assert_eq!(rs.len(), 3);
    // Nothing outside sees any of it yet.
    assert_eq!(user_count(&db), 1);

    let err = tx.commit();
    assert!(is_violation(&err), "{err:?}");

    // All-or-nothing: the clean statement did not land either, and no WAL
    // record was written.
    assert_eq!(user_count(&db), 1);
    assert!(db.reader().node_info("ok").is_none());
    assert!(db.reader().node_info("bob").is_none());
    assert_eq!(wal_bytes(&dir), wal_before);
    assert_eq!(
        mushroomdb::wal_commit_count_at(&dir).unwrap(),
        frames_before
    );

    // The store is still writable, and the value is still taken.
    db.submit_batch(vec![insert_user("carol", "c@x")]).unwrap();
    assert!(is_violation(
        &db.submit_batch(vec![insert_user("dave", "a@x")])
    ));
}

#[test]
fn a_transaction_that_collides_with_a_later_committed_write_is_refused() {
    let (_dir, db) = constrained("late-holder");
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (n:User {id: 'bob', email: 'x@x'})", &p())
        .unwrap();
    // Another writer takes the value after the transaction began.
    db.submit_batch(vec![insert_user("alice", "x@x")]).unwrap();
    let err = tx.commit();
    assert!(is_violation(&err), "{err:?}");
    assert!(db.reader().node_info("bob").is_none());
    assert_eq!(user_count(&db), 1);
}

#[test]
fn a_transaction_set_that_frees_and_retakes_a_value_commits() {
    let (_dir, db) = constrained("swap");
    db.submit_batch(vec![insert_user("alice", "a@x")]).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("MATCH (n:User {id: 'alice'}) SET n.email = 'moved@x'", &p())
        .unwrap();
    tx.execute("CREATE (n:User {id: 'bob', email: 'a@x'})", &p())
        .unwrap();
    tx.commit()
        .expect("one frame that frees a value may hand it on");
    assert_eq!(user_count(&db), 2);
}

#[test]
fn racing_query_writes_on_one_value_have_one_winner() {
    let (dir, db) = constrained("race-query-write");
    const WRITERS: usize = 6;
    let barrier = Arc::new(Barrier::new(WRITERS));
    let handles: Vec<_> = (0..WRITERS)
        .map(|i| {
            let db = db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                db.write()
                    .query_write(
                        &format!("CREATE (n:User {{id: 'u{i}', email: 'race@x'}})"),
                        &BTreeMap::new(),
                    )
                    .map(|_| ())
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    let losses = results.iter().filter(|r| is_violation(r)).count();
    assert_eq!((wins, losses), (1, WRITERS - 1), "{results:?}");
    assert_eq!(user_count(&db), 1);
    drop(db);
    assert_eq!(user_count(&SharedDb::open(&dir).unwrap()), 1);
}

#[test]
fn racing_submit_batches_on_one_value_have_one_winner() {
    let (_dir, db) = constrained("race-submit");
    const WRITERS: usize = 6;
    let barrier = Arc::new(Barrier::new(WRITERS));
    let handles: Vec<_> = (0..WRITERS)
        .map(|i| {
            let db = db.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                db.submit_batch(vec![insert_user(&format!("u{i}"), "race@x")])
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    let losses = results.iter().filter(|r| is_violation(r)).count();
    assert_eq!((wins, losses), (1, WRITERS - 1), "{results:?}");
    assert_eq!(user_count(&db), 1);
}

#[test]
fn racing_transactions_on_distinct_values_all_commit() {
    let (_dir, db) = constrained("race-distinct");
    const WRITERS: usize = 6;
    let txs: Vec<_> = (0..WRITERS)
        .map(|i| {
            let mut tx = db.begin_transaction();
            tx.execute(
                &format!("CREATE (n:User {{id: 'u{i}', email: 'e{i}@x'}})"),
                &p(),
            )
            .unwrap();
            tx
        })
        .collect();
    let handles: Vec<_> = txs
        .into_iter()
        .map(|tx| std::thread::spawn(move || tx.commit()))
        .collect();
    for h in handles {
        h.join().unwrap().expect("distinct values never conflict");
    }
    assert_eq!(user_count(&db), WRITERS);
}

#[test]
fn reopen_and_snapshot_keep_the_constraint_binding() {
    let (dir, db) = constrained("replay");
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (n:User {id: 'alice', email: 'a@x'})", &p())
        .unwrap();
    tx.commit().unwrap();
    assert!(is_violation(
        &db.submit_batch(vec![insert_user("bob", "a@x")])
    ));
    drop(db);

    // Reopen from the WAL alone, then again after a snapshot.
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.node_count(), 1);
    assert_eq!(reopened.constraints().len(), 1);
    drop(reopened);
    let mut again = GraphDb::open(&dir).unwrap();
    again.snapshot().unwrap();
    drop(again);
    let last = SharedDb::open(&dir).unwrap();
    assert_eq!(user_count(&last), 1);
    // And the constraint still binds after all that.
    assert!(is_violation(
        &last.submit_batch(vec![insert_user("eve", "a@x")])
    ));
}
