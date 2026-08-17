use core_api::{Direction, GraphDb, GraphError, Predicate, RuleDef, Value};

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
