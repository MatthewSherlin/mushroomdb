//! `SharedDb::from_locked` wraps a plain read-write handle — one that already
//! holds the store's cross-process write lock — into a `SharedDb`, so a binding
//! that opened with the lock can run interactive transactions without giving
//! the lock up. The lock stays held for as long as any clone of the result is
//! alive, and `Busy` semantics for every other opener are unchanged.

use mushroomdb::{GraphError, OpenOptions, RealFs, SharedDb, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;

type GraphDb = mushroomdb::GraphDb<RealFs>;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-from-locked-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

#[test]
fn the_cross_process_lock_is_held_for_the_wrapped_handles_lifetime() {
    let dir = tmp("lock");
    let db = GraphDb::open(&dir).unwrap();
    let shared = SharedDb::from_locked(db).unwrap();

    // Held before any transaction, during one, and after its commit.
    assert!(matches!(GraphDb::open(&dir), Err(GraphError::Busy { .. })));
    let mut tx = shared.begin_transaction();
    tx.execute("CREATE (n:P {id: 'a'})", &no_params()).unwrap();
    assert!(matches!(GraphDb::open(&dir), Err(GraphError::Busy { .. })));
    tx.commit().unwrap();
    assert!(matches!(GraphDb::open(&dir), Err(GraphError::Busy { .. })));

    // A clone keeps it alive; the last drop releases it.
    let clone = shared.clone();
    drop(shared);
    assert!(matches!(GraphDb::open(&dir), Err(GraphError::Busy { .. })));
    drop(clone);
    let reopened = GraphDb::open(&dir).unwrap();
    let rs = reopened
        .query("MATCH (n:P) RETURN key(n) AS k", &no_params())
        .unwrap();
    assert_eq!(rs.len(), 1);
}

#[test]
fn state_written_before_the_wrap_is_visible_to_a_transaction() {
    let dir = tmp("before");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "seed", vec![]).unwrap();
    let shared = SharedDb::from_locked(db).unwrap();
    let mut tx = shared.begin_transaction();
    let rs = tx
        .execute("MATCH (n:P) RETURN key(n) AS k", &no_params())
        .unwrap();
    assert_eq!(rs.len(), 1);
}

#[test]
fn a_read_only_handle_cannot_be_wrapped() {
    let dir = tmp("ro");
    drop(GraphDb::open(&dir).unwrap());
    let opts = OpenOptions {
        read_only: true,
        ..OpenOptions::default()
    };
    let ro = GraphDb::open_with_options(&dir, opts).unwrap();
    assert!(matches!(
        SharedDb::from_locked(ro),
        Err(GraphError::ReadOnly)
    ));
}

#[test]
fn a_snapshots_masked_write_is_the_typed_masked_read_only_error() {
    // Both masked entry points refuse a write with the same variant, so a
    // caller matches the variant, never the message.
    let dir = tmp("masked");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "a", vec![]).unwrap();
    let mask = mushroomdb::NodeMask::from_ids([]);
    let on_db = db
        .query_masked("CREATE (n:P {id: 'x'})", &no_params(), &mask)
        .unwrap_err();
    let on_snapshot = db
        .reader()
        .query_masked("CREATE (n:P {id: 'x'})", &no_params(), &mask)
        .unwrap_err();
    assert!(matches!(on_db, GraphError::MaskedReadOnly));
    assert!(
        matches!(on_snapshot, GraphError::MaskedReadOnly),
        "{on_snapshot:?}"
    );
    assert_eq!(on_db.to_string(), on_snapshot.to_string());
}
