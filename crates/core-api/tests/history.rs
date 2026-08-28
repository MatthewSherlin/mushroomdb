use core_api::{EdgeEvent, GraphDb, GraphError, HistoryChange, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-history-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn node_history_insert_prop_edge_sequence() {
    let dir = tmp("seq");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("Person", "a", vec![]).unwrap();
    db.set_prop("a", "color", Value::Str("red".into())).unwrap();
    db.insert_node("Person", "b", vec![]).unwrap();
    db.insert_edge("Knows", "a", "b").unwrap();
    db.remove_prop("a", "color").unwrap();
    db.delete_edge("Knows", "a", "b").unwrap();

    let history_a = db.node_history("a").unwrap();

    // Exactly 5 entries in strict commit order.
    assert_eq!(history_a.len(), 5, "history_a: {history_a:?}");

    // Commits are strictly increasing.
    for w in history_a.windows(2) {
        assert!(
            w[0].commit < w[1].commit,
            "commits not strictly increasing: {:?} >= {:?}",
            w[0].commit,
            w[1].commit
        );
    }

    assert!(
        matches!(&history_a[0].change, HistoryChange::NodeInserted { label } if label == "Person"),
        "expected NodeInserted got {:?}",
        history_a[0]
    );
    assert!(
        matches!(&history_a[1].change, HistoryChange::PropSet { field, value }
            if field == "color" && *value == Value::Str("red".into())),
        "expected PropSet got {:?}",
        history_a[1]
    );
    assert!(
        matches!(&history_a[2].change, HistoryChange::EdgeAdded { edge_type, other, outgoing }
            if edge_type == "Knows" && other == "b" && *outgoing),
        "expected EdgeAdded{{outgoing:true}} got {:?}",
        history_a[2]
    );
    assert!(
        matches!(&history_a[3].change, HistoryChange::PropRemoved { field } if field == "color"),
        "expected PropRemoved got {:?}",
        history_a[3]
    );
    assert!(
        matches!(&history_a[4].change, HistoryChange::EdgeRemoved { edge_type, other, outgoing }
            if edge_type == "Knows" && other == "b" && *outgoing),
        "expected EdgeRemoved{{outgoing:true}} got {:?}",
        history_a[4]
    );

    // history("b") sees NodeInserted + EdgeAdded{outgoing:false} + EdgeRemoved{outgoing:false}
    let history_b = db.node_history("b").unwrap();
    assert_eq!(history_b.len(), 3, "history_b: {history_b:?}");

    // Commits are strictly increasing for b too.
    for w in history_b.windows(2) {
        assert!(
            w[0].commit < w[1].commit,
            "history_b commits not strictly increasing: {:?} >= {:?}",
            w[0].commit,
            w[1].commit
        );
    }

    assert!(
        matches!(&history_b[0].change, HistoryChange::NodeInserted { label } if label == "Person"),
        "expected NodeInserted got {:?}",
        history_b[0]
    );
    assert!(
        matches!(&history_b[1].change, HistoryChange::EdgeAdded { edge_type, other, outgoing }
            if edge_type == "Knows" && other == "a" && !outgoing),
        "expected EdgeAdded{{outgoing:false}} got {:?}",
        history_b[1]
    );
    assert!(
        matches!(&history_b[2].change, HistoryChange::EdgeRemoved { edge_type, other, outgoing }
            if edge_type == "Knows" && other == "a" && !outgoing),
        "expected EdgeRemoved{{outgoing:false}} got {:?}",
        history_b[2]
    );
}

#[test]
fn node_history_delete_node() {
    let dir = tmp("deleted");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("Thing", "x", vec![]).unwrap();
    db.delete_node("x").unwrap();

    let history = db.node_history("x").unwrap();

    // NodeInserted then NodeDeleted.  Dense-id prop records for a tombstoned node are
    // unresolvable (documented), so we don't set props here to keep the test unambiguous.
    assert_eq!(history.len(), 2, "history: {history:?}");
    assert!(
        matches!(&history[0].change, HistoryChange::NodeInserted { label } if label == "Thing"),
        "expected NodeInserted, got {:?}",
        history[0]
    );
    assert!(
        matches!(&history[1].change, HistoryChange::NodeDeleted),
        "expected NodeDeleted, got {:?}",
        history[1]
    );
    assert!(
        history[0].commit < history[1].commit,
        "commits not ordered: {:?}",
        history
    );
}

#[test]
fn node_history_horizon_after_snapshot() {
    let dir = tmp("horizon");
    let mut db = GraphDb::open(&dir).unwrap();

    // Pre-snapshot operations — these should be invisible after a WAL-truncating snapshot.
    db.insert_node("X", "a", vec![]).unwrap();
    db.set_prop("a", "v", Value::Int(1)).unwrap();

    // Default snapshot() truncates the WAL.
    db.snapshot().unwrap();

    // Post-snapshot mutation — the only thing in the new WAL.
    db.set_prop("a", "v", Value::Int(2)).unwrap();

    let history = db.node_history("a").unwrap();

    // Only the post-snapshot PropSet should appear — pre-snapshot history is beyond the horizon.
    assert_eq!(history.len(), 1, "history after snapshot: {history:?}");
    assert!(
        matches!(&history[0].change, HistoryChange::PropSet { field, value }
            if field == "v" && *value == Value::Int(2)),
        "expected post-snapshot PropSet, got {:?}",
        history[0]
    );
}

#[test]
fn node_history_unknown_key_returns_empty() {
    let dir = tmp("empty");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("X", "exists", vec![]).unwrap();
    let history = db.node_history("no_such_key").unwrap();
    assert!(history.is_empty());
}

// ── edge_history tests ────────────────────────────────────────────────────────

#[test]
fn edge_history_manual_add_appears_as_added_no_rule() {
    let dir = tmp("eh-add");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    db.insert_edge("Knows", "a", "b").unwrap();

    let result = db.edge_history("a", "b").unwrap();
    assert_eq!(result.items.len(), 1, "items: {:?}", result.items);
    assert_eq!(result.items[0].edge_type, "Knows");
    assert_eq!(result.items[0].event, EdgeEvent::Added);
    assert_eq!(
        result.items[0].rule, None,
        "manual edge must have rule: None"
    );
}

#[test]
fn edge_history_delete_edge_appears_as_retracted() {
    let dir = tmp("eh-del");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    db.insert_edge("Knows", "a", "b").unwrap();
    db.delete_edge("Knows", "a", "b").unwrap();

    let result = db.edge_history("a", "b").unwrap();
    assert_eq!(result.items.len(), 2, "items: {:?}", result.items);
    assert_eq!(result.items[0].event, EdgeEvent::Added);
    assert_eq!(result.items[1].event, EdgeEvent::Retracted);
    assert_eq!(result.items[1].rule, None);
    // Commits are strictly increasing.
    assert!(
        result.items[0].commit < result.items[1].commit,
        "commits not ordered: {:?}",
        result.items
    );
}

#[test]
fn edge_history_delete_node_produces_retracted_for_incident_edges() {
    let dir = tmp("eh-delnode");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    db.insert_edge("Knows", "a", "b").unwrap();
    db.delete_node("a").unwrap();

    let result = db.edge_history("a", "b").unwrap();
    // Should have Added then Retracted (from node deletion).
    assert_eq!(result.items.len(), 2, "items: {:?}", result.items);
    assert_eq!(result.items[0].event, EdgeEvent::Added);
    assert_eq!(result.items[1].event, EdgeEvent::Retracted);
    assert_eq!(result.items[1].rule, None);
}

#[test]
fn edge_history_both_directions_reported() {
    let dir = tmp("eh-bidir");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    db.insert_node("N", "b", vec![]).unwrap();
    // Add edge in both directions.
    db.insert_edge("Follows", "a", "b").unwrap();
    db.insert_edge("Follows", "b", "a").unwrap();

    let result = db.edge_history("a", "b").unwrap();
    assert_eq!(result.items.len(), 2, "both directions: {:?}", result.items);
    assert!(result.items.iter().all(|e| e.event == EdgeEvent::Added));
    assert!(result.items.iter().all(|e| e.edge_type == "Follows"));
}

#[test]
fn was_linked_true_between_add_and_retract_false_outside() {
    let dir = tmp("wl-window");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap(); // commit 0
    db.insert_node("N", "b", vec![]).unwrap(); // commit 1
    db.insert_edge("Knows", "a", "b").unwrap(); // commit 2
    db.set_prop("a", "x", Value::Int(9)).unwrap(); // commit 3 (unrelated)
    db.delete_edge("Knows", "a", "b").unwrap(); // commit 4

    // Before the add: false.
    assert!(!db.was_linked("a", "b", "Knows", 0).unwrap(), "before add");
    assert!(
        !db.was_linked("a", "b", "Knows", 1).unwrap(),
        "before add commit 1"
    );
    // At the add commit: true.
    assert!(
        db.was_linked("a", "b", "Knows", 2).unwrap(),
        "at add commit"
    );
    // Between add and retract: true.
    assert!(
        db.was_linked("a", "b", "Knows", 3).unwrap(),
        "between add and retract"
    );
    // At retract commit: false.
    assert!(
        !db.was_linked("a", "b", "Knows", 4).unwrap(),
        "at retract commit"
    );
}

#[test]
fn was_linked_commit_out_of_range_errors() {
    let dir = tmp("wl-range");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap(); // commit 0
    db.insert_node("N", "b", vec![]).unwrap(); // commit 1
    db.insert_edge("Knows", "a", "b").unwrap(); // commit 2
                                                // WAL has 3 frames (commits 0, 1, 2).

    // Last valid commit: 2 → Ok.
    assert!(db.was_linked("a", "b", "Knows", 2).unwrap());
    // One past end (commit 3) → CommitOutOfRange.
    match db.was_linked("a", "b", "Knows", 3) {
        Err(GraphError::CommitOutOfRange {
            commit: 3,
            total: 3,
        }) => {}
        other => panic!("expected CommitOutOfRange{{3,3}}, got {other:?}"),
    }
    // Well past end → CommitOutOfRange.
    match db.was_linked("a", "b", "Knows", 999) {
        Err(GraphError::CommitOutOfRange {
            commit: 999,
            total: 3,
        }) => {}
        other => panic!("expected CommitOutOfRange{{999,3}}, got {other:?}"),
    }
}

#[test]
fn was_linked_empty_wal_always_out_of_range() {
    let dir = tmp("wl-empty");
    // Open without inserting anything → empty WAL.
    let db = GraphDb::open(&dir).unwrap();
    match db.was_linked("a", "b", "X", 0) {
        Err(GraphError::CommitOutOfRange {
            commit: 0,
            total: 0,
        }) => {}
        other => panic!("expected CommitOutOfRange{{0,0}} for empty WAL, got {other:?}"),
    }
}

#[test]
fn edge_history_no_mask_filtering_consistent_with_node_history() {
    // edge_history (like node_history) has no built-in mask filtering.
    // This test documents the behaviour: even if a node is not visible
    // under a mask, the base API still returns its WAL history.
    let dir = tmp("eh-mask");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "visible", vec![]).unwrap();
    db.insert_node("N", "hidden", vec![]).unwrap();
    db.insert_edge("Link", "visible", "hidden").unwrap();

    // edge_history has no mask parameter; it returns history regardless.
    let result = db.edge_history("visible", "hidden").unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].event, EdgeEvent::Added);
}

#[test]
fn edge_history_derived_edges_not_present() {
    // Derived edges are not WAL-logged; rule: Some(...) entries cannot
    // appear via WAL scan. This test documents the limitation.
    use core_api::RuleDef;
    let dir = tmp("eh-derived");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    // A simple KeyMatch rule: Person→Org via org_id==key.
    let rule = RuleDef {
        name: "fk".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        edge_type: "BelongsTo".into(),
        predicate: core_api::Predicate::KeyMatch {
            field: "org_id".into(),
        },
        max_edges: None,
        weight_prop: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    };
    db.create_rule(rule).unwrap();
    // The derived edge exists in the graph.
    assert_eq!(db.edge_count(), 1);
    // But edge_history has no WAL record for it.
    let result = db.edge_history("p1", "o1").unwrap();
    assert!(
        result.items.is_empty(),
        "derived edges must not appear in edge_history WAL scan: {:?}",
        result.items
    );
}
