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
        .query_masked("MATCH (a:P)-[r:KNOWS]->(b:P) RETURN b.id", &p, &mask)
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

/// Variable-length path test: the only route from `a` to `c` passes through
/// hidden intermediate node `b`.  With the mask excluding `b`, no path exists.
///
/// Graph: a -HOP-> b -HOP-> c
/// Mask:  {a, c}  (b is hidden)
/// Query: MATCH (x:N)-[r:HOP*1..2]->(y:N) RETURN y.id
///   unmasked → 3 rows (a→b, a→c via 2-hop, b→c)
///   masked   → 0 rows (b is non-existent, so no path can use it)
#[test]
fn masked_var_expand_blocks_hidden_intermediate() {
    let dir = tmp("var-expand");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    db.insert_node("N", "c", vec![]).unwrap();
    db.insert_edge("HOP", "a", "b").unwrap();
    db.insert_edge("HOP", "b", "c").unwrap();

    let p = no_params();

    // Sanity check: unmasked sees 3 pairs (a→b, b→c via 1-hop; a→c via 2-hop).
    let rs = db
        .query("MATCH (x:N)-[r:HOP*1..2]->(y:N) RETURN y.id", &p)
        .unwrap();
    assert_eq!(rs.len(), 3, "unmasked var-expand should return 3 rows");

    // Mask excludes b: the a→b hop vanishes, and since a→c requires b as an
    // intermediate, a→c also vanishes.  b→c vanishes because b itself is hidden.
    let mask = NodeMask::from_keys(&db, ["a", "c"]);
    let rs = db
        .query_masked("MATCH (x:N)-[r:HOP*1..2]->(y:N) RETURN y.id", &p, &mask)
        .unwrap();
    assert_eq!(
        rs.len(),
        0,
        "masked var-expand must return 0 rows when the only path passes through a hidden node"
    );
}

/// ShortestPath test: path from `a` to `d` must pass through hidden `b`.
/// With the mask excluding `b`, no shortest path exists.
///
/// Graph: a -T-> b -T-> c -T-> d
/// Mask:  {a, c, d}  (b is hidden)
/// shortestPath(a → d, max 5 hops) unmasked → depth 3 row
///                                  masked   → 0 rows (b blocks the path)
#[test]
fn masked_shortest_path_blocks_hidden_intermediate() {
    let dir = tmp("shortest-path");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    db.insert_node("N", "c", vec![]).unwrap();
    db.insert_node("N", "d", vec![]).unwrap();
    db.insert_edge("T", "a", "b").unwrap();
    db.insert_edge("T", "b", "c").unwrap();
    db.insert_edge("T", "c", "d").unwrap();

    let p = no_params();
    let q = "MATCH (a:N {id: 'a'}) MATCH (d:N {id: 'd'}) \
             MATCH shortestPath((a)-[r:T*..5]->(d)) RETURN r.length";

    // Sanity: unmasked finds the path at depth 3.
    let rs = db.query(q, &p).unwrap();
    assert_eq!(rs.len(), 1, "unmasked shortestPath should find a→b→c→d");

    // Mask excludes b: a→b→c→d requires traversing hidden b, so no path.
    let mask = NodeMask::from_keys(&db, ["a", "c", "d"]);
    let rs = db.query_masked(q, &p, &mask).unwrap();
    assert_eq!(
        rs.len(),
        0,
        "masked shortestPath must return 0 rows when the only path passes through a hidden node"
    );
}
