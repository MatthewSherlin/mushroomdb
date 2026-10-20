//! `keys(r)` / `properties(r)` against an edge whose properties live partly in
//! the snapshot base and partly in the overlay. Whatever the store's history,
//! they must report exactly what `get_edge_prop` (the `EdgePropsView::get` path)
//! reports. The only edge property a user edge can carry is the insert count of
//! an opted-in store, which is what lets one field be written twice and so be
//! shadowed.

use mushroomdb::{GraphDb, RealFs, Value};
use std::collections::BTreeMap;

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-edge-props-fns-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn store(name: &str) -> (std::path::PathBuf, Db) {
    let dir = tmp(name);
    let mut db = GraphDb::open(&dir).unwrap();
    db.enable_multiplicity().unwrap();
    for k in ["a", "b"] {
        db.insert_node("N", k, vec![]).unwrap();
    }
    (dir, db)
}

/// `properties(r)` and `keys(r)` for the one `E` edge from `a` to `b`; `None`
/// when the edge is not reachable.
fn read(db: &Db) -> Option<(BTreeMap<String, Value>, Vec<Value>)> {
    let rs = db
        .query(
            "MATCH (a:N {id: 'a'})-[r:E]->(b:N {id: 'b'}) RETURN properties(r), keys(r)",
            &BTreeMap::new(),
        )
        .unwrap();
    if rs.is_empty() {
        return None;
    }
    assert_eq!(rs.len(), 1);
    let row = rs.row(0);
    let Some(Value::Map(props)) = row[0].clone() else {
        panic!("properties(r) is not a map: {:?}", row[0]);
    };
    let Some(Value::List(keys)) = row[1].clone() else {
        panic!("keys(r) is not a list: {:?}", row[1]);
    };
    Some((props, keys))
}

/// What the point-lookup path says the edge carries.
fn expected(db: &Db) -> BTreeMap<String, Value> {
    db.get_edge_prop("E", "a", "b", "count")
        .map(|v| BTreeMap::from([("count".to_string(), v)]))
        .unwrap_or_default()
}

/// The edge as `properties(r)` / `keys(r)` see it must be the edge as
/// `get_edge_prop` sees it. `want`: `None` = edge gone, `Some(None)` = edge
/// with no properties, `Some(Some(n))` = edge with insert count `n`.
fn check(db: &Db, want: Option<Option<i64>>, what: &str) {
    let want_props: BTreeMap<String, Value> = want
        .flatten()
        .map(|n| BTreeMap::from([("count".to_string(), Value::Int(n))]))
        .unwrap_or_default();
    assert_eq!(expected(db), want_props, "{what}: get_edge_prop");
    match (want, read(db)) {
        (None, None) => {}
        (Some(_), Some((props, keys))) => {
            assert_eq!(props, want_props, "{what}: properties(r)");
            let want_keys: Vec<Value> = want_props.keys().cloned().map(Value::Str).collect();
            assert_eq!(keys, want_keys, "{what}: keys(r)");
        }
        (want, got) => panic!("{what}: wanted {want:?}, read {got:?}"),
    }
}

fn reopen(dir: &std::path::Path, db: Db) -> Db {
    drop(db);
    GraphDb::open(dir).unwrap()
}

#[test]
fn a_base_only_edge() {
    let (dir, mut db) = store("base-only");
    db.insert_edge("E", "a", "b").unwrap();
    db.insert_edge("E", "a", "b").unwrap();
    check(&db, Some(Some(2)), "live, before snapshot");
    db.snapshot().unwrap();
    check(&db, Some(Some(2)), "after snapshot");
    let db = reopen(&dir, db);
    check(&db, Some(Some(2)), "after reopen");
}

#[test]
fn an_overlay_field_shadowing_a_base_field() {
    let (dir, mut db) = store("shadow");
    db.insert_edge("E", "a", "b").unwrap();
    db.insert_edge("E", "a", "b").unwrap();
    db.snapshot().unwrap();
    db.insert_edge("E", "a", "b").unwrap();
    check(&db, Some(Some(3)), "overlay over base");
    let mut db = reopen(&dir, db);
    check(&db, Some(Some(3)), "overlay over base, reopened");
    db.snapshot().unwrap();
    check(&db, Some(Some(3)), "folded into a new base");
}

#[test]
fn a_tombstoned_edge() {
    let (dir, mut db) = store("tombstone");
    db.insert_edge("E", "a", "b").unwrap();
    db.insert_edge("E", "a", "b").unwrap();
    db.snapshot().unwrap();
    assert!(db.delete_edge("E", "a", "b").unwrap());
    check(&db, None, "deleted after snapshot");
    let db = reopen(&dir, db);
    check(&db, None, "deleted after snapshot, reopened");
}

#[test]
fn an_edge_removed_and_set_again_after_a_snapshot() {
    let (dir, mut db) = store("reset");
    db.insert_edge("E", "a", "b").unwrap();
    db.insert_edge("E", "a", "b").unwrap();
    db.snapshot().unwrap();
    assert!(db.delete_edge("E", "a", "b").unwrap());
    assert!(db.insert_edge("E", "a", "b").unwrap());
    // The new edge is a first insert, so it has no count; the base's old 2
    // must not come back through the base fallback.
    check(&db, Some(None), "removed and re-set");
    let mut db = reopen(&dir, db);
    check(&db, Some(None), "removed and re-set, reopened");
    db.snapshot().unwrap();
    check(&db, Some(None), "removed and re-set, then snapshotted");
}
