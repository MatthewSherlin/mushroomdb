//! QL-07: named paths (`MATCH p = (a)-[*1..3]->(b) RETURN p`) answer the same
//! however the store was built or read: live, WAL replay, snapshotted and
//! reopened, one batch or many commits, `open_at`, and under a mask.
//!
//! A path renders as the map `{nodes, relationships, length}` (no new `Value`
//! variant), and a query result carrying one is never storable.

use mushroomdb::{GraphDb, NodeMask, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphdb-ql07-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn none() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

const PEOPLE: [&str; 5] = ["ada", "bob", "cam", "dee", "eve"];

/// A chain ada -> bob -> cam -> dee -> eve, one batch.
fn build_one_batch(dir: &std::path::Path) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    let mut batch = db.batch();
    for key in PEOPLE {
        batch.insert_node("Person", key, vec![]);
    }
    for pair in PEOPLE.windows(2) {
        batch.insert_edge("KNOWS", pair[0], pair[1]);
    }
    batch.commit().unwrap();
    db
}

/// Same graph, one commit per write.
fn build_many_commits(dir: &std::path::Path) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    for key in PEOPLE {
        db.insert_node("Person", key, vec![]).unwrap();
    }
    for pair in PEOPLE.windows(2) {
        db.insert_edge("KNOWS", pair[0], pair[1]).unwrap();
    }
    db
}

const ALL_PATHS: &str = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b:Person) \
                         RETURN a, b, p ORDER BY p.length, a";
const FIXED: &str = "MATCH p = (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) \
                     RETURN p ORDER BY a";

fn rows(db: &GraphDb<core_storage::fs::RealFs>, q: &str) -> Vec<Vec<Option<Value>>> {
    let rs = db.query(q, &none()).unwrap_or_else(|e| panic!("{q}: {e}"));
    (0..rs.len()).map(|i| rs.row(i).to_vec()).collect()
}

fn length_of(row: &[Option<Value>], col: usize) -> i64 {
    match &row[col] {
        Some(Value::Map(m)) => match m["length"] {
            Value::Int(n) => n,
            _ => panic!("length is an int"),
        },
        other => panic!("not a path: {other:?}"),
    }
}

#[test]
fn a_named_path_has_the_pinned_shape() {
    let db = build_one_batch(&tmp("shape"));
    let got = rows(&db, FIXED);
    assert_eq!(got.len(), 3);
    let Some(Value::Map(first)) = &got[0][0] else {
        panic!("a map")
    };
    assert_eq!(
        first.keys().map(String::as_str).collect::<Vec<_>>(),
        ["length", "nodes", "relationships"]
    );
    assert_eq!(first["length"], Value::Int(2));
    assert_eq!(
        first["nodes"],
        Value::List(["ada", "bob", "cam"].map(|k| Value::Str(k.into())).to_vec())
    );
    let Value::List(rels) = &first["relationships"] else {
        panic!("a list")
    };
    assert_eq!(rels.len(), 2);
    assert_eq!(
        rels[0],
        Value::Map(BTreeMap::from([
            ("type".to_owned(), Value::Str("KNOWS".into())),
            ("start".to_owned(), Value::Str("ada".into())),
            ("end".to_owned(), Value::Str("bob".into())),
        ]))
    );
}

#[test]
fn variable_length_paths_count_and_order() {
    let db = build_one_batch(&tmp("varlen"));
    let got = rows(&db, ALL_PATHS);
    // 4 of length 1, 3 of length 2, 2 of length 3.
    let lengths: Vec<i64> = got.iter().map(|r| length_of(r, 2)).collect();
    assert_eq!(lengths, vec![1, 1, 1, 1, 2, 2, 2, 3, 3]);
}

#[test]
fn live_wal_replay_snapshot_reopen_and_commit_shape_agree() {
    let dir = tmp("diff");
    let db = build_one_batch(&dir);
    let live = (rows(&db, ALL_PATHS), rows(&db, FIXED));
    drop(db);

    // Reopened: a WAL replay, never snapshotted.
    let replayed = GraphDb::open(&dir).unwrap();
    assert_eq!(
        (rows(&replayed, ALL_PATHS), rows(&replayed, FIXED)),
        live,
        "WAL replay"
    );

    // Snapshotted, then reopened from the snapshot.
    let mut snapshotting = replayed;
    snapshotting.snapshot().unwrap();
    assert_eq!(
        (rows(&snapshotting, ALL_PATHS), rows(&snapshotting, FIXED)),
        live,
        "snapshotting"
    );
    drop(snapshotting);
    let from_snapshot = GraphDb::open(&dir).unwrap();
    assert_eq!(
        (rows(&from_snapshot, ALL_PATHS), rows(&from_snapshot, FIXED)),
        live,
        "reopened from a snapshot"
    );

    // Many commits instead of one batch.
    let many = build_many_commits(&tmp("diff-many"));
    assert_eq!(
        (rows(&many, ALL_PATHS), rows(&many, FIXED)),
        live,
        "many commits"
    );
}

#[test]
fn a_path_over_an_edge_added_after_the_snapshot_includes_it() {
    let dir = tmp("after-snap");
    let mut db = build_one_batch(&dir);
    db.snapshot().unwrap();
    db.insert_node("Person", "fay", vec![]).unwrap();
    db.insert_edge("KNOWS", "eve", "fay").unwrap();
    let got = rows(
        &db,
        "MATCH p = (a:Person {id: 'ada'})-[:KNOWS*5]->(b) RETURN p.nodes AS n",
    );
    assert_eq!(got.len(), 1);
    let Some(Value::List(nodes)) = &got[0][0] else {
        panic!("a list")
    };
    assert_eq!(nodes.len(), 6);
    assert_eq!(nodes[5], Value::Str("fay".into()));
}

#[test]
fn open_at_an_earlier_commit_sees_only_that_commits_paths() {
    let dir = tmp("open-at");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "ada", vec![]).unwrap();
    db.insert_node("Person", "bob", vec![]).unwrap();
    db.insert_edge("KNOWS", "ada", "bob").unwrap();
    let after_first_edge = 2; // commits are zero-based: node, node, edge
    db.insert_node("Person", "cam", vec![]).unwrap();
    db.insert_edge("KNOWS", "bob", "cam").unwrap();
    let then = GraphDb::open_at(&dir, after_first_edge).unwrap();
    let q = "MATCH p = (a:Person)-[:KNOWS*1..3]->(b) RETURN p.length AS n";
    assert_eq!(rows(&then, q).len(), 1);
    assert_eq!(rows(&db, q).len(), 3);
}

#[test]
fn a_mask_removes_any_path_that_touches_a_hidden_node() {
    let db = build_one_batch(&tmp("mask"));
    let q = "MATCH p = (a:Person)-[:KNOWS*1..4]->(b:Person) RETURN p";
    // `cam` hidden: only ada-bob and dee-eve remain.
    let mask = NodeMask::from_keys(&db, ["ada", "bob", "dee", "eve"]);
    let rs = db.query_masked(q, &none(), &mask).unwrap();
    assert_eq!(rs.len(), 2);
    for i in 0..rs.len() {
        let Some(Value::Map(path)) = rs.get(i, "p") else {
            panic!("a path")
        };
        assert_eq!(path["length"], Value::Int(1));
        let Value::List(nodes) = &path["nodes"] else {
            panic!("nodes")
        };
        assert!(!nodes.contains(&Value::Str("cam".into())), "{nodes:?}");
    }
    // The full mask equals the unmasked answer.
    let all = NodeMask::from_keys(&db, PEOPLE);
    assert_eq!(
        db.query_masked(q, &none(), &all).unwrap().len(),
        db.query(q, &none()).unwrap().len()
    );
}

#[test]
fn a_path_is_refused_as_a_property_value_by_name() {
    // A path is a query result, not a property value: the SET right-hand
    // side takes no bare variable, so a path never reaches the store as one.
    let mut db = build_one_batch(&tmp("unstorable"));
    let err = db
        .query_write(
            "MATCH p = (a:Person {id: 'ada'})-[:KNOWS]->(b) SET a.route = p",
            &none(),
        )
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("bare property/variable reference"), "{text}");
    let after = rows(&db, "MATCH (a:Person {id: 'ada'}) RETURN a.route AS r");
    assert_eq!(after[0][0], None, "nothing was written");
}

#[test]
fn a_named_path_in_a_write_match_binds_and_the_write_still_applies() {
    let mut db = build_one_batch(&tmp("write-match"));
    // The path name binds like any variable; the write still applies to nodes.
    db.query_write(
        "MATCH p = (a:Person {id: 'ada'})-[:KNOWS]->(b) SET b.seen = 1",
        &none(),
    )
    .unwrap();
    let got = rows(&db, "MATCH (n:Person) WHERE n.seen = 1 RETURN n.id AS id");
    assert_eq!(got, vec![vec![Some(Value::Str("bob".into()))]]);
}
