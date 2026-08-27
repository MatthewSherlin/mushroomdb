use core_api::{GraphDb, NodeMask, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-mask-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn no_params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

#[test]
fn masked_query_hides_nodes_and_their_edges() {
    let dir = tmp("basic");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "alice", vec![]).unwrap();
    db.insert_node("P", "bob", vec![]).unwrap();
    db.insert_node("P", "carol", vec![]).unwrap();
    db.insert_edge("KNOWS", "alice", "bob").unwrap();
    db.insert_edge("KNOWS", "alice", "carol").unwrap();

    let mask = NodeMask::from_keys(&db, ["alice", "bob"]);
    assert_eq!(mask.len(), 2);
    let p = no_params();

    // Label scan sees only masked nodes.
    let rs = db
        .query_masked("MATCH (n:P) RETURN n.id", &p, &mask)
        .unwrap();
    assert_eq!(rs.len(), 2, "label scan should return 2 rows");

    // Neighbor expansion cannot reach carol (edge alice->carol is hidden).
    let rs = db
        .query_masked(
            "MATCH (a:P)-[r:KNOWS]->(b:P) RETURN b.id",
            &p,
            &mask,
        )
        .unwrap();
    assert_eq!(rs.len(), 1, "only alice->bob should be visible");

    // Key lookup on hidden node binds nothing.
    let rs = db
        .query_masked("MATCH (n {id: 'carol'}) RETURN n.id", &p, &mask)
        .unwrap();
    assert_eq!(rs.len(), 0, "carol should not be found by key lookup");

    // Unmasked query sees all three nodes.
    let rs = db.query("MATCH (n:P) RETURN n.id", &p).unwrap();
    assert_eq!(rs.len(), 3, "unmasked query must see all nodes");
}

#[test]
fn masked_write_is_rejected() {
    let dir = tmp("write-reject");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "x", vec![]).unwrap();
    let mask = NodeMask::from_keys(&db, ["x"]);
    let p = no_params();

    let err = db
        .query_masked("CREATE (n:P {id: 'new'})", &p, &mask)
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("masked queries are read-only"),
        "error should mention read-only: {msg}"
    );
}

#[test]
fn empty_mask_hides_all_nodes() {
    let dir = tmp("empty-mask");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "alice", vec![]).unwrap();
    db.insert_node("P", "bob", vec![]).unwrap();

    let mask = NodeMask::from_keys(&db, std::iter::empty());
    assert!(mask.is_empty());
    let p = no_params();

    let rs = db
        .query_masked("MATCH (n:P) RETURN n.id", &p, &mask)
        .unwrap();
    assert_eq!(rs.len(), 0, "empty mask hides all nodes");
}

#[test]
fn unknown_keys_in_mask_are_ignored() {
    let dir = tmp("unknown-keys");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "alice", vec![]).unwrap();

    // "ghost" does not exist; mask should contain only alice's id.
    let mask = NodeMask::from_keys(&db, ["alice", "ghost"]);
    assert_eq!(mask.len(), 1);
    let p = no_params();

    let rs = db
        .query_masked("MATCH (n:P) RETURN n.id", &p, &mask)
        .unwrap();
    assert_eq!(rs.len(), 1);
}
