use core_api::{Dir, Direction, GraphDb, GraphError, Predicate, ResultSet, RuleDef, Value};
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
