use core_api::{GraphDb, MaskMode, MaskedNodeResult, NodeMask, Value};
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

// ── Restricted-stub mask mode (Task 1: KB-hardening) ─────────────────────────

#[test]
fn stub_default_mode_is_omit() {
    let dir = tmp("stub-default");
    let db = GraphDb::open(&dir).unwrap();
    let mask = NodeMask::from_keys(&db, std::iter::empty::<&str>());
    assert_eq!(
        mask.mode(),
        MaskMode::Omit,
        "from_keys must default to Omit"
    );

    let mask2 = NodeMask::from_ids(std::iter::empty::<u32>());
    assert_eq!(
        mask2.mode(),
        MaskMode::Omit,
        "from_ids must default to Omit"
    );
}

#[test]
fn stub_with_mode_sets_stub() {
    let dir = tmp("stub-with-mode");
    let db = GraphDb::open(&dir).unwrap();
    let mask = NodeMask::from_keys(&db, std::iter::empty::<&str>()).with_mode(MaskMode::Stub);
    assert_eq!(mask.mode(), MaskMode::Stub);
}

#[test]
fn stub_intersect_always_produces_omit() {
    let dir = tmp("stub-intersect");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    let mask_a = NodeMask::from_keys(&db, ["a"]).with_mode(MaskMode::Stub);
    let mask_b = NodeMask::from_keys(&db, ["a"]).with_mode(MaskMode::Stub);
    let intersected = mask_a.intersect(&mask_b);
    assert_eq!(
        intersected.mode(),
        MaskMode::Omit,
        "intersect must always produce Omit mode (role-path invariant)"
    );
}

/// `node_info_masked` on a key that does not exist in the DB returns `None`
/// in both modes (caller should 404).
#[test]
fn stub_node_info_absent_key_is_none_in_both_modes() {
    let dir = tmp("stub-info-absent");
    let db = GraphDb::open(&dir).unwrap();

    let mask_omit = NodeMask::from_keys(&db, std::iter::empty::<&str>());
    assert!(
        db.node_info_masked("ghost", &mask_omit).is_none(),
        "absent key must be None in Omit mode"
    );

    let mask_stub = NodeMask::from_keys(&db, std::iter::empty::<&str>()).with_mode(MaskMode::Stub);
    assert!(
        db.node_info_masked("ghost", &mask_stub).is_none(),
        "absent key must be None in Stub mode"
    );
}

/// In Omit mode, `node_info_masked` on a hidden key returns `None` (same as not found).
#[test]
fn stub_node_info_hidden_key_omit_mode_returns_none() {
    let dir = tmp("stub-info-omit");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "alice", vec![]).unwrap();
    db.insert_node("P", "bob", vec![]).unwrap();

    let mask = NodeMask::from_keys(&db, ["alice"]); // bob is hidden, mode=Omit
    assert!(
        db.node_info_masked("bob", &mask).is_none(),
        "hidden key must be None in Omit mode"
    );
}

/// In Stub mode, `node_info_masked` on a hidden key returns `Some(Restricted)` — not None.
/// Existence is disclosed; no label or props are returned.
#[test]
fn stub_node_info_hidden_key_stub_mode_returns_restricted() {
    let dir = tmp("stub-info-stub");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "alice", vec![]).unwrap();
    db.insert_node("P", "bob", vec![("score".into(), Value::Int(42))])
        .unwrap();

    let mask = NodeMask::from_keys(&db, ["alice"]).with_mode(MaskMode::Stub);
    let result = db.node_info_masked("bob", &mask);
    assert_eq!(
        result,
        Some(MaskedNodeResult::Restricted),
        "hidden key in Stub mode must return Restricted, not None"
    );
}

/// In Stub mode, `node_info_masked` on a visible key returns the full `NodeInfo`.
#[test]
fn stub_node_info_visible_key_returns_visible() {
    let dir = tmp("stub-info-visible");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "alice", vec![]).unwrap();

    let mask = NodeMask::from_keys(&db, ["alice"]).with_mode(MaskMode::Stub);
    match db.node_info_masked("alice", &mask) {
        Some(MaskedNodeResult::Visible(info)) => assert_eq!(info.key, "alice"),
        other => panic!("expected Visible(alice), got {other:?}"),
    }
}

/// In Stub mode, `node_edges_masked` includes edges whose far endpoint is hidden.
/// The far endpoint is marked `*_restricted: true`; no label or props leak.
#[test]
fn stub_node_edges_includes_hidden_neighbor_as_stub() {
    let dir = tmp("stub-edges-stub");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "alice", vec![]).unwrap();
    db.insert_node("P", "bob", vec![]).unwrap();
    db.insert_node("P", "carol", vec![]).unwrap();
    db.insert_edge("KNOWS", "alice", "bob").unwrap();
    db.insert_edge("KNOWS", "alice", "carol").unwrap();

    // Only alice and carol visible; bob is hidden.
    let mask = NodeMask::from_keys(&db, ["alice", "carol"]).with_mode(MaskMode::Stub);
    let edges = db.node_edges_masked("alice", &mask).unwrap();

    // Both edges must be present: alice→carol (visible) and alice→bob (stub).
    assert_eq!(
        edges.len(),
        2,
        "both edges must appear in stub mode: {edges:?}"
    );

    let bob_edge = edges
        .iter()
        .find(|e| e.dst_key == "bob")
        .expect("alice→bob edge must be present as stub");
    assert!(
        bob_edge.dst_restricted,
        "bob endpoint must be marked restricted"
    );
    assert!(!bob_edge.src_restricted, "alice src must not be restricted");

    let carol_edge = edges
        .iter()
        .find(|e| e.dst_key == "carol")
        .expect("alice→carol edge must be present");
    assert!(!carol_edge.dst_restricted, "carol must not be restricted");
}

/// In Omit mode, `node_edges_masked` excludes edges to hidden endpoints.
#[test]
fn stub_node_edges_omit_mode_excludes_hidden_neighbor() {
    let dir = tmp("stub-edges-omit");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "alice", vec![]).unwrap();
    db.insert_node("P", "bob", vec![]).unwrap();
    db.insert_node("P", "carol", vec![]).unwrap();
    db.insert_edge("KNOWS", "alice", "bob").unwrap();
    db.insert_edge("KNOWS", "alice", "carol").unwrap();

    let mask = NodeMask::from_keys(&db, ["alice", "carol"]); // bob hidden, Omit mode
    let edges = db.node_edges_masked("alice", &mask).unwrap();
    assert_eq!(
        edges.len(),
        1,
        "Omit mode must exclude edge to hidden bob: {edges:?}"
    );
    assert_eq!(edges[0].dst_key, "carol");
}

/// Cypher query results are byte-identical in Omit and Stub modes.
/// The mode flag does not change which nodes the query engine sees.
#[test]
fn stub_cypher_query_identical_in_both_modes() {
    let dir = tmp("stub-query");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("P", "alice", vec![]).unwrap();
    db.insert_node("P", "bob", vec![]).unwrap();
    db.insert_node("P", "carol", vec![]).unwrap();
    db.insert_edge("KNOWS", "alice", "bob").unwrap();
    db.insert_edge("KNOWS", "alice", "carol").unwrap();

    let p = no_params();
    let cypher = "MATCH (a:P)-[r:KNOWS]->(b:P) RETURN b.id";

    let mask_omit = NodeMask::from_keys(&db, ["alice", "carol"]);
    let mask_stub = NodeMask::from_keys(&db, ["alice", "carol"]).with_mode(MaskMode::Stub);

    let rs_omit = db.query_masked(cypher, &p, &mask_omit).unwrap();
    let rs_stub = db.query_masked(cypher, &p, &mask_stub).unwrap();

    assert_eq!(
        rs_omit.len(),
        rs_stub.len(),
        "Cypher query must return same row count in Omit and Stub modes"
    );
}

/// BFS neighborhood does not expand through a hidden node in either mode.
///
/// Graph: A -HOP-> B (hidden) -HOP-> C (visible)
/// Mask:  {A, C}
///
/// In Omit mode: BFS returns nothing (B is invisible, so A→C path is broken).
/// In Stub mode: BFS returns B as a stub (depth 1) but NOT C — B is not used
/// as a frontier node, so the BFS never discovers C.
#[test]
fn stub_bfs_does_not_expand_through_hidden_node_in_either_mode() {
    let dir = tmp("stub-bfs");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap(); // will be hidden
    db.insert_node("N", "c", vec![]).unwrap();
    db.insert_edge("HOP", "a", "b").unwrap();
    db.insert_edge("HOP", "b", "c").unwrap();

    // Omit mode: B is hidden → A cannot reach C, BFS returns empty.
    let mask_omit = NodeMask::from_keys(&db, ["a", "c"]);
    let rs_omit = db
        .neighborhood_masked("a", 2, None, core_api::Dir::Out, &mask_omit)
        .unwrap();
    assert_eq!(
        rs_omit.len(),
        0,
        "Omit mode: BFS must return 0 visible nodes (C unreachable)"
    );

    // Stub mode: B appears as a stub at depth 1, but BFS does NOT expand from B,
    // so C does not appear in the result.
    let mask_stub = NodeMask::from_keys(&db, ["a", "c"]).with_mode(MaskMode::Stub);
    let rs_stub = db
        .neighborhood_masked("a", 2, None, core_api::Dir::Out, &mask_stub)
        .unwrap();

    let has_c = (0..rs_stub.len()).any(|i| {
        rs_stub
            .row(i)
            .first()
            .and_then(|v| v.as_ref())
            .and_then(|v| {
                if let Value::Str(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            == Some("c")
    });
    assert!(
        !has_c,
        "Stub mode: C must not appear — BFS must not expand through hidden B"
    );

    let has_b_stub = (0..rs_stub.len()).any(|i| {
        let row = rs_stub.row(i);
        let key_is_b = row.first().and_then(|v| v.as_ref()).and_then(|v| {
            if let Value::Str(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }) == Some("b");
        let label_is_null = row.get(1).and_then(|v| v.as_ref()).is_none();
        key_is_b && label_is_null
    });
    assert!(
        has_b_stub,
        "Stub mode: B must appear as a stub row (key=b, label=null)"
    );
}
