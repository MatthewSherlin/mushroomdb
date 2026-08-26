use core_api::{Dir, Direction, GraphDb, GraphError, Predicate, ResultSet, RuleDef, Stats, Value};
use std::collections::{BTreeMap, BTreeSet};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn tags(xs: &[&str]) -> Value {
    Value::List(xs.iter().map(|s| Value::Str((*s).into())).collect())
}

fn overlap_rule(name: &str, etype: &str) -> RuleDef {
    RuleDef {
        name: name.into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: etype.into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

fn wal_len(dir: &std::path::Path) -> u64 {
    std::fs::metadata(dir.join("wal.bin")).unwrap().len()
}

#[test]
fn remove_prop_retracts_overlap_edges_both_ways_and_indexes_stay_clean() {
    let dir = tmp("mut-overlap-retract");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.create_rule(overlap_rule("rel", "REL")).unwrap();
    assert_eq!(db.edge_count(), 2);
    let ex = db.explain("a", "b").unwrap();
    assert_eq!(ex.len(), 2);
    assert!(ex.iter().all(|e| e.weight == Some(1.0)));
    assert_eq!(db.neighbors("a", "REL", Direction::Out).unwrap(), vec!["b"]);
    assert_eq!(db.neighbors("b", "REL", Direction::Out).unwrap(), vec!["a"]);

    assert!(db.remove_prop("a", "tags").unwrap());
    assert_eq!(db.get_prop("a", "tags"), None);
    assert_eq!(db.edge_count(), 0);
    assert!(db.explain("a", "b").unwrap().is_empty());
    assert!(db.neighbors("a", "REL", Direction::Out).unwrap().is_empty());
    assert!(db.neighbors("b", "REL", Direction::Out).unwrap().is_empty());

    // a is no longer in the token index: a new matching node links only to b.
    db.insert_node("A", "c", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    assert_eq!(db.edge_count(), 2);
    assert_eq!(db.neighbors("b", "REL", Direction::Out).unwrap(), vec!["c"]);
    assert!(db.neighbors("a", "REL", Direction::Out).unwrap().is_empty());
    assert!(db.explain("a", "c").unwrap().is_empty());

    // Re-adding the prop re-links both directions (indexes were cleaned).
    db.set_prop("a", "tags", tags(&["x"])).unwrap();
    assert_eq!(db.edge_count(), 6); // a,b,c pairwise, both directions
    assert_eq!(
        db.neighbors("a", "REL", Direction::Out).unwrap(),
        vec!["b", "c"]
    );
    let ex_ab = db.explain("a", "b").unwrap();
    assert_eq!(ex_ab.len(), 2);
    assert!(ex_ab.iter().all(|e| e.weight == Some(1.0)));
}

#[test]
fn delete_user_edge_leaves_same_etype_derived_edge() {
    let dir = tmp("mut-delete-user-edge");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "u1", vec![]).unwrap();
    db.insert_node("A", "u2", vec![]).unwrap();
    db.insert_edge("KNOWS", "u1", "u2").unwrap();
    db.insert_node("A", "d1", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "d2", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.create_rule(overlap_rule("knows", "KNOWS")).unwrap();
    assert_eq!(db.edge_count(), 3); // user u1→u2 + derived d1↔d2

    assert!(db.delete_edge("KNOWS", "u1", "u2").unwrap());
    assert!(db
        .neighbors("u1", "KNOWS", Direction::Out)
        .unwrap()
        .is_empty());
    assert_eq!(
        db.neighbors("d1", "KNOWS", Direction::Out).unwrap(),
        vec!["d2"]
    );
    assert_eq!(
        db.neighbors("d2", "KNOWS", Direction::Out).unwrap(),
        vec!["d1"]
    );
    assert_eq!(db.edge_count(), 2);
}

#[test]
fn delete_rule_owned_edge_returns_rule_owned() {
    let dir = tmp("mut-rule-owned");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.create_rule(overlap_rule("rel", "REL")).unwrap();
    match db.delete_edge("REL", "a", "b") {
        Err(GraphError::RuleOwned { detail }) => {
            assert!(
                detail.contains("delete") || detail.contains("change"),
                "RuleOwned detail must guide the user to delete or change the rule: {detail}"
            );
            assert!(
                !detail.contains("or a live rule would re-derive it"),
                "provenance-owned path must not use the would_derive suffix: {detail}"
            );
        }
        other => panic!("expected RuleOwned, got {other:?}"),
    }
    assert_eq!(db.edge_count(), 2); // derived edges untouched
}

#[test]
fn absent_remove_prop_and_delete_edge_are_false_and_unlogged() {
    let dir = tmp("mut-absent-nolog");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("name".into(), Value::Str("ada".into()))])
        .unwrap();
    db.insert_node("A", "b", vec![]).unwrap();
    db.insert_edge("E", "a", "b").unwrap();

    let before = wal_len(&dir);
    assert!(!db.remove_prop("a", "missing").unwrap());
    assert_eq!(wal_len(&dir), before);
    assert!(!db.delete_edge("E", "b", "a").unwrap()); // reverse not present
    assert_eq!(wal_len(&dir), before);
    assert!(!db.delete_edge("NOPE", "a", "b").unwrap());
    assert_eq!(wal_len(&dir), before);

    assert!(db.remove_prop("a", "name").unwrap());
    let after_remove = wal_len(&dir);
    assert!(after_remove > before);
    assert!(!db.remove_prop("a", "name").unwrap());
    assert_eq!(wal_len(&dir), after_remove);

    assert!(db.delete_edge("E", "a", "b").unwrap());
    let after_delete = wal_len(&dir);
    assert!(after_delete > after_remove);
    assert!(!db.delete_edge("E", "a", "b").unwrap());
    assert_eq!(wal_len(&dir), after_delete);
}

#[test]
fn unknown_keys_are_key_not_found() {
    let dir = tmp("mut-unknown-key");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![]).unwrap();
    assert!(matches!(
        db.remove_prop("ghost", "x"),
        Err(GraphError::KeyNotFound { .. })
    ));
    assert!(matches!(
        db.delete_edge("E", "ghost", "a"),
        Err(GraphError::KeyNotFound { .. })
    ));
    assert!(matches!(
        db.delete_edge("E", "a", "ghost"),
        Err(GraphError::KeyNotFound { .. })
    ));
}

#[test]
fn crash_window_replays_remove_prop_and_delete_edge_idempotently() {
    let dir = tmp("mut-crash-window");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
        db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
        db.insert_node("A", "u1", vec![]).unwrap();
        db.insert_node("A", "u2", vec![]).unwrap();
        db.insert_edge("KNOWS", "u1", "u2").unwrap();
        db.create_rule(overlap_rule("rel", "REL")).unwrap();
        assert!(db.remove_prop("a", "tags").unwrap());
        assert!(db.delete_edge("KNOWS", "u1", "u2").unwrap());
        assert_eq!(db.edge_count(), 0);
        assert_eq!(db.get_prop("a", "tags"), None);

        let pre_snap_wal = std::fs::read(dir.join("wal.bin")).unwrap();
        db.snapshot().unwrap();
        // Crash between snapshot write and WAL truncation: restore the
        // pre-snapshot WAL so reopen replays RemoveProp/DeleteEdge over the
        // already-applied snapshot (field/edge already gone).
        std::fs::write(dir.join("wal.bin"), &pre_snap_wal).unwrap();
    }
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("a", "tags"), None);
    assert_eq!(db.get_prop("b", "tags"), Some(&tags(&["x"])));
    assert_eq!(db.edge_count(), 0);
    assert!(db
        .neighbors("u1", "KNOWS", Direction::Out)
        .unwrap()
        .is_empty());
    assert!(db.explain("a", "b").unwrap().is_empty());
    // Incremental firing still works: restoring a's tags re-links.
    db.set_prop("a", "tags", tags(&["x"])).unwrap();
    assert_eq!(db.edge_count(), 2);
}

#[test]
fn reopen_replays_deletions_identically() {
    let dir = tmp("mut-reopen");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
        db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
        db.insert_node("A", "u1", vec![]).unwrap();
        db.insert_node("A", "u2", vec![]).unwrap();
        db.insert_edge("KNOWS", "u1", "u2").unwrap();
        db.create_rule(overlap_rule("rel", "REL")).unwrap();
        assert!(db.remove_prop("a", "tags").unwrap());
        assert!(db.delete_edge("KNOWS", "u1", "u2").unwrap());
        assert_eq!(db.edge_count(), 0);
        assert_eq!(db.get_prop("a", "tags"), None);
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("a", "tags"), None);
    assert_eq!(db.get_prop("b", "tags"), Some(&tags(&["x"])));
    assert_eq!(db.edge_count(), 0);
    assert!(db
        .neighbors("u1", "KNOWS", Direction::Out)
        .unwrap()
        .is_empty());
    assert!(db.explain("a", "b").unwrap().is_empty());
    assert_eq!(db.rules().len(), 1);
}

fn keys_of(rs: &ResultSet, col: &str) -> BTreeSet<String> {
    (0..rs.len())
        .map(|i| match rs.get(i, col) {
            Some(Value::Str(k)) => k.clone(),
            other => panic!("expected Str in {col}: {other:?}"),
        })
        .collect()
}

fn out_pairs(
    db: &GraphDb<core_storage::fs::RealFs>,
    keys: &[&str],
    etype: &str,
) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for k in keys {
        if !db.has_node(k) {
            continue;
        }
        for n in db.neighbors(k, etype, Direction::Out).unwrap() {
            out.insert(((*k).to_string(), n));
        }
    }
    out
}

#[test]
fn delete_node_retracts_rule_edges_both_sides_and_partners_relink() {
    let dir = tmp("del-retract-relink");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.create_rule(overlap_rule("rel", "REL")).unwrap();
    assert_eq!(db.edge_count(), 2);
    assert_eq!(db.neighbors("a", "REL", Direction::Out).unwrap(), vec!["b"]);
    assert_eq!(db.neighbors("b", "REL", Direction::Out).unwrap(), vec!["a"]);
    let ex = db.explain("a", "b").unwrap();
    assert_eq!(ex.len(), 2);

    db.delete_node("a").unwrap();
    assert!(!db.has_node("a"));
    assert!(db.node_ref("a").is_none());
    assert_eq!(db.get_prop("a", "tags"), None);
    assert_eq!(db.edge_count(), 0);
    assert!(matches!(
        db.explain("a", "b"),
        Err(GraphError::KeyNotFound { .. })
    ));
    assert!(db.neighbors("b", "REL", Direction::Out).unwrap().is_empty());

    // Indexes still hold b: a new matching node links only to b, not the ghost a.
    db.insert_node("A", "c", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    assert_eq!(db.edge_count(), 2);
    assert_eq!(db.neighbors("b", "REL", Direction::Out).unwrap(), vec!["c"]);
    assert_eq!(db.neighbors("c", "REL", Direction::Out).unwrap(), vec!["b"]);
    assert_eq!(db.explain("b", "c").unwrap().len(), 2);
}

#[test]
fn delete_node_sweeps_user_edges_and_props() {
    let dir = tmp("del-user-sweep");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "A",
        "a",
        vec![
            ("name".into(), Value::Str("ada".into())),
            ("age".into(), Value::Int(36)),
        ],
    )
    .unwrap();
    db.insert_node("A", "u", vec![]).unwrap();
    db.insert_node("A", "v", vec![]).unwrap();
    assert!(db.insert_edge("KNOWS", "a", "u").unwrap());
    assert!(db.insert_edge("LIKES", "v", "a").unwrap());

    db.delete_node("a").unwrap();
    assert!(!db.has_node("a"));
    assert_eq!(db.edge_count(), 0);
    assert!(db
        .neighbors("u", "KNOWS", Direction::In)
        .unwrap()
        .is_empty());
    assert!(db
        .neighbors("v", "LIKES", Direction::Out)
        .unwrap()
        .is_empty());
}

#[test]
fn delete_node_unknown_and_tombstoned_are_key_not_found() {
    let dir = tmp("del-unknown");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![]).unwrap();
    let before = wal_len(&dir);
    assert!(matches!(
        db.delete_node("ghost"),
        Err(GraphError::KeyNotFound { .. })
    ));
    assert_eq!(wal_len(&dir), before);

    db.delete_node("a").unwrap();
    let after = wal_len(&dir);
    assert!(after > before);
    assert!(matches!(
        db.delete_node("a"),
        Err(GraphError::KeyNotFound { .. })
    ));
    assert_eq!(wal_len(&dir), after);
}

#[test]
fn delete_node_reinsert_is_fresh_identity_with_no_ghosts() {
    let dir = tmp("del-reinsert");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "u", vec![]).unwrap();
    db.insert_edge("KNOWS", "a", "u").unwrap();
    db.create_rule(overlap_rule("rel", "REL")).unwrap();
    assert_eq!(db.edge_count(), 3); // a↔b derived + a→u user

    db.delete_node("a").unwrap();
    db.insert_node("A", "a", vec![]).unwrap();
    assert!(db.has_node("a"));
    assert_eq!(db.get_prop("a", "tags"), None);
    assert!(db.neighbors("a", "REL", Direction::Out).unwrap().is_empty());
    assert!(db
        .neighbors("a", "KNOWS", Direction::Out)
        .unwrap()
        .is_empty());
    assert!(db.neighbors("b", "REL", Direction::Out).unwrap().is_empty());
    assert!(db
        .neighbors("u", "KNOWS", Direction::In)
        .unwrap()
        .is_empty());
    assert!(db.explain("a", "b").unwrap().is_empty());

    // Fresh identity can grow its own derived edges; no leftover link to u.
    db.set_prop("a", "tags", tags(&["x"])).unwrap();
    assert_eq!(db.neighbors("a", "REL", Direction::Out).unwrap(), vec!["b"]);
    assert!(db
        .neighbors("a", "KNOWS", Direction::Out)
        .unwrap()
        .is_empty());
}

#[test]
fn rebuild_rule_after_delete_is_noop() {
    let dir = tmp("del-rebuild-noop");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "c", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.create_rule(overlap_rule("rel", "REL")).unwrap();
    db.delete_node("a").unwrap();

    let before_count = db.edge_count();
    let before = out_pairs(&db, &["a", "b", "c"], "REL");
    assert_eq!(before_count, 2); // b↔c only
    db.rebuild_rule("rel").unwrap();
    assert_eq!(db.edge_count(), before_count);
    assert_eq!(out_pairs(&db, &["a", "b", "c"], "REL"), before);
}

#[test]
fn crash_window_replays_delete_node_idempotently() {
    let dir = tmp("del-crash-window");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
        db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
        db.insert_node("A", "u", vec![]).unwrap();
        db.insert_edge("KNOWS", "a", "u").unwrap();
        db.create_rule(overlap_rule("rel", "REL")).unwrap();
        db.delete_node("a").unwrap();
        assert!(!db.has_node("a"));
        assert_eq!(db.edge_count(), 0);

        let pre_snap_wal = std::fs::read(dir.join("wal.bin")).unwrap();
        db.snapshot().unwrap();
        // Crash between snapshot write and WAL truncation: restore the
        // pre-snapshot WAL so reopen replays DeleteNode over a snapshot
        // that already tombstoned the key.
        std::fs::write(dir.join("wal.bin"), &pre_snap_wal).unwrap();
    }
    let mut db = GraphDb::open(&dir).unwrap();
    assert!(!db.has_node("a"));
    assert!(db.has_node("b"));
    assert_eq!(db.edge_count(), 0);
    assert!(db.neighbors("b", "REL", Direction::Out).unwrap().is_empty());
    assert!(db
        .neighbors("u", "KNOWS", Direction::In)
        .unwrap()
        .is_empty());
    // Indexes survived replay: a new match still links to b.
    db.insert_node("A", "c", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    assert_eq!(db.neighbors("b", "REL", Direction::Out).unwrap(), vec!["c"]);
}

#[test]
fn cypher_scan_traversal_grouped_explain_ignore_deleted_node() {
    let dir = tmp("del-read-paths");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "u", vec![]).unwrap();
    db.insert_edge("KNOWS", "b", "u").unwrap();
    db.create_rule(overlap_rule("rel", "REL")).unwrap();
    db.delete_node("a").unwrap();

    let labeled = db.query("MATCH (n:A) RETURN n", &BTreeMap::new()).unwrap();
    assert_eq!(
        keys_of(&labeled, "n"),
        BTreeSet::from(["b".into(), "u".into()])
    );

    let unlabeled = db.query("MATCH (n) RETURN n", &BTreeMap::new()).unwrap();
    assert_eq!(
        keys_of(&unlabeled, "n"),
        BTreeSet::from(["b".into(), "u".into()])
    );
    assert!(!keys_of(&unlabeled, "n").contains("a"));

    let hop = db
        .query("MATCH (x:A)-[:REL]->(y) RETURN x, y", &BTreeMap::new())
        .unwrap();
    assert!(hop.is_empty(), "no REL edges remain after deleting a");

    let b = db.node_ref("b").expect("b live");
    let grouped = b.grouped_by_edge_type();
    assert!(!grouped.values().flatten().any(|k| k == "a"));
    assert_eq!(
        grouped.get("KNOWS").cloned().unwrap_or_default(),
        vec!["u".to_string()]
    );

    let nb = b.neighborhood(2, None, Dir::Both);
    let nb_keys: BTreeSet<String> = (0..nb.len())
        .map(|i| match nb.get(i, "key") {
            Some(Value::Str(k)) => k.clone(),
            other => panic!("expected key: {other:?}"),
        })
        .collect();
    assert!(!nb_keys.contains("a"));
    assert!(nb_keys.contains("u"));

    assert!(db.node_ref("a").is_none());
    assert!(matches!(
        db.explain("a", "b"),
        Err(GraphError::KeyNotFound { key }) if key == "a"
    ));
}

/// Helper: FieldEqual rule with top-k per-source cap.
fn topk_eq_rule(k: u64) -> RuleDef {
    RuleDef {
        name: "eq".into(),
        src_label: "N".into(),
        dst_label: "N".into(),
        predicate: Predicate::FieldEqual { field: "k".into() },
        edge_type: "EQ".into(),
        weight_prop: None,
        max_edges: Some(k),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

// Keep const_eq_rule/insert_const_nodes for the delete_rule_drops_rule_stats test.
fn const_eq_rule(k: u64) -> RuleDef {
    topk_eq_rule(k)
}

fn insert_const_nodes(db: &mut GraphDb<core_storage::fs::RealFs>, start: usize, end: usize) {
    for i in start..end {
        db.insert_node(
            "N",
            &format!("n{i}"),
            vec![("k".into(), Value::Str("const".into()))],
        )
        .unwrap();
    }
}

/// Top-k FieldEqual: 5 nodes all sharing k="const", top-k=2 per source.
/// Each src gets exactly 2 dsts (the 2 smallest keys that aren't self).
/// Stats must survive WAL replay and snapshot+reopen; tripped must always
/// be false for top-k rules.
#[test]
fn topk_field_equal_cap_per_source_and_stats_survive_recovery() {
    let dir = tmp("topk-stats-replay");
    let live;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.create_rule(topk_eq_rule(2)).unwrap();

        // Insert 5 nodes with keys n0..n4 (alphabetical order = insertion order).
        for i in 0..5usize {
            db.insert_node(
                "N",
                &format!("n{i}"),
                vec![("k".into(), Value::Str("const".into()))],
            )
            .unwrap();
        }

        let s = db.stats();
        assert_eq!(s.nodes_live, 5);
        assert_eq!(s.edges, 10, "5 nodes × top-2 each = 10 edges");
        assert_eq!(GraphDb::<core_storage::fs::RealFs>::format_version(), 7);
        assert_eq!(s.rules.len(), 1);
        assert_eq!(s.rules[0].name, "eq");
        assert_eq!(s.rules[0].edges, 10);
        // Top-k rules never set the tripped latch.
        assert!(!s.rules[0].tripped, "top-k rules must never set tripped");
        assert!(s.rules[0].fires > 0);

        // Additional inserts succeed and update the top-k normally.
        db.insert_node("N", "n5", vec![("k".into(), Value::Str("const".into()))])
            .unwrap();
        let s = db.stats();
        // 6 nodes × top-2 each = 12 edges.
        assert_eq!(s.rules[0].edges, 12);
        assert!(!s.rules[0].tripped);
        assert_eq!(s.nodes_live, 6);
        live = s;
    }

    // WAL replay must reproduce stats exactly.
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.stats(), live);
    drop(reopened);

    // Snapshot + reopen must also reproduce stats.
    {
        let mut db = GraphDb::open(&dir).unwrap();
        let before = db.stats();
        db.snapshot().unwrap();
        drop(db);
        let after = GraphDb::open(&dir).unwrap();
        assert_eq!(after.stats(), before);
        assert_eq!(after.stats(), live);
    }
}

/// rebuild_rule on a top-k rule re-establishes the correct per-source set
/// after additional nodes have been inserted.
#[test]
fn topk_rebuild_correct_set_after_inserts() {
    let dir = tmp("topk-rebuild-set");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_rule(topk_eq_rule(2)).unwrap();

    // Insert 5 nodes; top-2 per source = 10 edges total.
    for i in 0..5usize {
        db.insert_node(
            "N",
            &format!("n{i}"),
            vec![("k".into(), Value::Str("const".into()))],
        )
        .unwrap();
    }
    let before = out_pairs(&db, &["n0", "n1", "n2", "n3", "n4"], "EQ");
    assert_eq!(before.len(), 10);
    assert!(!db.stats().rules[0].tripped);

    // rebuild must be a no-op (top-k is already correct).
    db.rebuild_rule("eq").unwrap();
    let after = out_pairs(&db, &["n0", "n1", "n2", "n3", "n4"], "EQ");
    assert_eq!(after, before, "rebuild must not change a correct top-k set");
    assert!(!db.stats().rules[0].tripped);
    assert_eq!(db.stats().rules[0].edges, 10);
}

/// Property changes trigger top-k eviction and backfill.
/// Nodes that lose matches are removed from affected srcs' top-k; the
/// next-best candidate is automatically promoted.
#[test]
fn topk_eviction_on_prop_change_and_backfill() {
    let dir = tmp("topk-evict-backfill");
    let mut db = GraphDb::open(&dir).unwrap();

    // top-k=1: each src gets only its single best (smallest key) dst.
    db.create_rule(topk_eq_rule(1)).unwrap();

    // Insert 4 nodes all with k="const". Keys: n0 < n1 < n2 < n3.
    for i in 0..4usize {
        db.insert_node(
            "N",
            &format!("n{i}"),
            vec![("k".into(), Value::Str("const".into()))],
        )
        .unwrap();
    }
    // n0→n1, n1→n0, n2→n0, n3→n0 (each points to smallest key ≠ self).
    assert_eq!(db.stats().rules[0].edges, 4);
    assert!(!db.stats().rules[0].tripped);

    // Change n0 so it no longer FieldEquals "const" (eviction event).
    // Nodes that had n0 as their top-1 dst must backfill.
    db.set_prop("n0", "k", Value::Str("other".into())).unwrap();

    // n1→n0 is retracted; n1 backfills to n2 (next-smallest key that matches).
    // n2→n0 retracted; n2 backfills to n1.
    // n3→n0 retracted; n3 backfills to n1.
    // n0 now has k="other" → no FieldEqual match → 0 dsts.
    assert_eq!(
        db.stats().rules[0].edges,
        3,
        "3 edges after n0 eviction: n1→n2, n2→n1, n3→n1"
    );
    assert!(
        db.neighbors("n0", "EQ", Direction::Out).unwrap().is_empty(),
        "n0 no longer matches anyone"
    );
    assert_eq!(
        db.neighbors("n1", "EQ", Direction::Out).unwrap(),
        vec!["n2"],
        "n1 backfills to n2"
    );

    // Rebuild must give the same result (top-k is already correct).
    db.rebuild_rule("eq").unwrap();
    assert_eq!(db.stats().rules[0].edges, 3);
    assert!(!db.stats().rules[0].tripped);
}

/// Top-k WAL replay: after top-k rule creates edges, close and reopen.
/// The top-k derived set must be identical to pre-close state.
#[test]
fn topk_wal_replay_preserves_top_k() {
    let dir = tmp("topk-wal-replay");
    let live;
    let live_edges;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.create_rule(topk_eq_rule(2)).unwrap();

        for i in 0..5usize {
            db.insert_node(
                "N",
                &format!("n{i}"),
                vec![("k".into(), Value::Str("const".into()))],
            )
            .unwrap();
        }

        // Change some props to trigger eviction + backfill paths through WAL.
        db.set_prop("n2", "k", Value::Str("x2".into())).unwrap();
        db.set_prop("n2", "k", Value::Str("const".into())).unwrap();

        live = db.stats();
        live_edges = out_pairs(&db, &["n0", "n1", "n2", "n3", "n4"], "EQ");
        assert!(!live.rules[0].tripped);
        assert_eq!(live.rules[0].edges, 10); // 5 nodes × top-2
        assert_eq!(live_edges.len(), 10);
    }

    // WAL replay must reproduce the exact same edges.
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(reopened.stats(), live);
    assert_eq!(
        out_pairs(&reopened, &["n0", "n1", "n2", "n3", "n4"], "EQ"),
        live_edges
    );
}

#[test]
fn delete_rule_drops_rule_stats() {
    let dir = tmp("budget-delete-rule-stats");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_rule(const_eq_rule(10)).unwrap();
    insert_const_nodes(&mut db, 0, 2);
    assert_eq!(db.stats().rules.len(), 1);
    db.delete_rule("eq").unwrap();
    assert!(db.stats().rules.is_empty());
}

#[test]
fn stats_live_and_tombstoned_after_delete_node() {
    let dir = tmp("stats-tombstone");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![]).unwrap();
    db.insert_node("A", "b", vec![]).unwrap();
    db.insert_edge("E", "a", "b").unwrap();
    let s = db.stats();
    assert_eq!(
        s,
        Stats {
            nodes_live: 2,
            nodes_tombstoned: 0,
            edges: 1,
            rules: vec![],
        }
    );
    db.delete_node("a").unwrap();
    let s = db.stats();
    assert_eq!(s.nodes_live, 1);
    assert_eq!(s.nodes_tombstoned, 1);
    assert_eq!(s.edges, 0);
    assert!(db.has_node("b"));
    assert!(!db.has_node("a"));
}

/// Top-k k=1: deleting the current top-1 dst promotes the next-best candidate.
///
/// Setup: 3 nodes (n0, n1, n2) all with k="x". Rule: FieldEqual top-k=1.
/// Each src gets its single-best (smallest key ≠ self) dst:
///   n0→n1, n1→n0, n2→n0.
/// Deleting n0 must retract n1→n0 and n2→n0, then backfill both to n2 and n1
/// respectively (the next eligible dst for each src).
///   After delete: n1→n2, n2→n1.
#[test]
fn topk_backfill_on_delete_promotes_next_candidate() {
    let dir = tmp("topk-delete-backfill");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_rule(topk_eq_rule(1)).unwrap();

    for i in 0..3usize {
        db.insert_node(
            "N",
            &format!("n{i}"),
            vec![("k".into(), Value::Str("x".into()))],
        )
        .unwrap();
    }
    // n0→n1, n1→n0, n2→n0  (each points to smallest-key ≠ self)
    assert_eq!(db.stats().rules[0].edges, 3);
    assert_eq!(
        db.neighbors("n1", "EQ", Direction::Out).unwrap(),
        vec!["n0"]
    );
    assert_eq!(
        db.neighbors("n2", "EQ", Direction::Out).unwrap(),
        vec!["n0"]
    );

    let report = db.delete_node("n0").unwrap();
    // n0 was the dst of n1→n0 and n2→n0 (2 derived edges where n0 is dst),
    // and the src of n0→n1 (1 derived edge where n0 is src). Total derived = 3.
    assert_eq!(report.derived_edges, 3, "all 3 edges were derived");
    assert_eq!(report.manual_edges, 0);

    assert!(!db.has_node("n0"));
    // n1 and n2 must have backfilled: each now points to the other.
    assert_eq!(
        db.neighbors("n1", "EQ", Direction::Out).unwrap(),
        vec!["n2"],
        "n1 backfills to n2 after n0 deleted"
    );
    assert_eq!(
        db.neighbors("n2", "EQ", Direction::Out).unwrap(),
        vec!["n1"],
        "n2 backfills to n1 after n0 deleted"
    );
    assert_eq!(db.stats().rules[0].edges, 2);
}

/// DeleteReport counts: manual and derived edge counts are accurate.
#[test]
fn delete_report_counts_are_accurate() {
    let dir = tmp("delete-report-counts");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "u", vec![]).unwrap();
    db.insert_node("A", "v", vec![]).unwrap();

    // 2 manual edges touching "a"
    db.insert_edge("KNOWS", "a", "u").unwrap();
    db.insert_edge("LIKES", "v", "a").unwrap();
    // 2 derived edges via overlap rule (a↔b)
    db.create_rule(overlap_rule("rel", "REL")).unwrap();
    assert_eq!(db.edge_count(), 4);

    let report = db.delete_node("a").unwrap();
    assert_eq!(report.manual_edges, 2, "2 manual edges (a→u and v→a)");
    assert_eq!(report.derived_edges, 2, "2 derived edges (a↔b)");
    assert_eq!(db.edge_count(), 0);
}

/// Cypher-SET-driven top-k evict/backfill integration test (Plan-13 carryover).
///
/// Uses `query_write` (MATCH…SET) to change the score-driving field; asserts
/// eviction and backfill happen through the full Cypher write entry.
#[test]
fn cypher_set_topk_evict_and_backfill() {
    let dir = tmp("cypher-set-topk");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_rule(topk_eq_rule(1)).unwrap();
    let no_params = BTreeMap::new();

    // 3 nodes, all k="x". top-1 per-src: n0→n1, n1→n0, n2→n0.
    // Include "id" as a prop so Cypher WHERE can target individual nodes.
    for i in 0..3usize {
        db.insert_node(
            "N",
            &format!("n{i}"),
            vec![
                ("id".into(), Value::Str(format!("n{i}"))),
                ("k".into(), Value::Str("x".into())),
            ],
        )
        .unwrap();
    }
    assert_eq!(db.stats().rules[0].edges, 3);
    assert_eq!(
        db.neighbors("n1", "EQ", Direction::Out).unwrap(),
        vec!["n0"]
    );

    // Change n0's field via Cypher SET so it no longer matches "x".
    // This must trigger eviction of n1→n0 and n2→n0 and backfill.
    db.query_write(
        "MATCH (n:N) WHERE n.id = 'n0' SET n.k = 'other'",
        &no_params,
    )
    .unwrap();

    // n0 no longer matches — its out-edges are retracted.
    assert!(
        db.neighbors("n0", "EQ", Direction::Out).unwrap().is_empty(),
        "n0 has no EQ edges after SET"
    );
    // n1 and n2 backfill to each other.
    assert_eq!(
        db.neighbors("n1", "EQ", Direction::Out).unwrap(),
        vec!["n2"],
        "n1 backfills to n2 after n0 evicted via Cypher SET"
    );
    assert_eq!(
        db.neighbors("n2", "EQ", Direction::Out).unwrap(),
        vec!["n1"],
        "n2 backfills to n1 after n0 evicted via Cypher SET"
    );
    assert_eq!(db.stats().rules[0].edges, 2);
}
