use core_api::{EdgeEvent, GraphDb, GraphError, HistoryChange, Predicate, RuleDef, Value};

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

fn fk_rule() -> RuleDef {
    RuleDef {
        name: "fk".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        edge_type: "BelongsTo".into(),
        predicate: Predicate::KeyMatch {
            field: "org_id".into(),
        },
        max_edges: None,
        weight_prop: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

#[test]
fn edge_history_derived_edge_fire_has_rule_attribution() {
    // When a rule fires on insert, DerivedEdgeAdded marker is written to the WAL.
    // edge_history must surface it with rule: Some("fk") and event: Added.
    let dir = tmp("eh-derived-attr");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.create_rule(fk_rule()).unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    // The derived edge exists in the graph.
    assert_eq!(db.edge_count(), 1);
    // edge_history must now show the marker with rule attribution.
    let result = db.edge_history("p1", "o1").unwrap();
    assert_eq!(
        result.items.len(),
        1,
        "expected one Added marker, got: {:?}",
        result.items
    );
    assert_eq!(result.items[0].event, EdgeEvent::Added);
    assert_eq!(result.items[0].edge_type, "BelongsTo");
    assert_eq!(
        result.items[0].rule,
        Some("fk".to_string()),
        "derived edge must carry rule attribution"
    );
}

#[test]
fn edge_history_derived_edge_retraction_has_rule_attribution() {
    // When a prop update causes a derived edge to be retracted and a new one fired,
    // edge_history shows Retracted(Some("fk")) for the old pair and Added(Some("fk"))
    // for the new pair.
    let dir = tmp("eh-derived-retract");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.insert_node("Org", "o2", vec![]).unwrap();
    db.create_rule(fk_rule()).unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    // Now redirect p1's FK to o2; the rule should retract p1→o1 and fire p1→o2.
    db.set_prop("p1", "org_id", Value::Str("o2".into()))
        .unwrap();
    assert_eq!(db.edge_count(), 1, "only one derived edge after redirect");

    // p1→o1 history: Added then Retracted, both with rule attribution.
    let result_o1 = db.edge_history("p1", "o1").unwrap();
    assert_eq!(
        result_o1.items.len(),
        2,
        "p1→o1 history: {:?}",
        result_o1.items
    );
    assert_eq!(result_o1.items[0].event, EdgeEvent::Added);
    assert_eq!(result_o1.items[0].rule, Some("fk".to_string()));
    assert_eq!(result_o1.items[1].event, EdgeEvent::Retracted);
    assert_eq!(
        result_o1.items[1].rule,
        Some("fk".to_string()),
        "retraction must carry rule attribution"
    );
    assert!(
        result_o1.items[0].commit < result_o1.items[1].commit,
        "Added before Retracted in commit order"
    );

    // p1→o2 history: only Added.
    let result_o2 = db.edge_history("p1", "o2").unwrap();
    assert_eq!(
        result_o2.items.len(),
        1,
        "p1→o2 history: {:?}",
        result_o2.items
    );
    assert_eq!(result_o2.items[0].event, EdgeEvent::Added);
    assert_eq!(result_o2.items[0].rule, Some("fk".to_string()));
}

#[test]
fn replay_identity_with_markers() {
    // Markers (DerivedEdgeAdded/Retracted) are STATE NO-OPS on WAL replay.
    // After close and reopen, derived edges must be re-derived identically
    // and edge_history must still return the correct marker items.
    let dir = tmp("eh-replay");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Org", "o1", vec![]).unwrap();
        db.create_rule(fk_rule()).unwrap();
        db.insert_node(
            "Person",
            "p1",
            vec![("org_id".into(), Value::Str("o1".into()))],
        )
        .unwrap();
        assert_eq!(db.edge_count(), 1);
    }
    // Reopen: markers in the WAL are skipped as state no-ops; rule re-derives the edge.
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.edge_count(), 1, "derived edge survives reopen");
    // edge_history still surfaces the marker with rule attribution.
    let result = db.edge_history("p1", "o1").unwrap();
    assert_eq!(
        result.items.len(),
        1,
        "marker survives reopen: {:?}",
        result.items
    );
    assert_eq!(result.items[0].event, EdgeEvent::Added);
    assert_eq!(result.items[0].rule, Some("fk".to_string()));
}

#[test]
fn reader_markers_are_no_op() {
    // open_at (ReaderSnapshot) replaying a WAL tail that contains DerivedEdgeAdded
    // markers must treat them as no-ops — derived edge state comes from the rule engine,
    // not from the marker.
    let dir = tmp("eh-reader-noop");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap(); // commit 0
    db.create_rule(fk_rule()).unwrap(); // commit 1
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap(); // commit 2 (InsertNode) + commit 3 (DerivedEdgeAdded marker)
    db.set_prop("p1", "color", Value::Str("blue".into()))
        .unwrap(); // commit 4

    // Open at commit 4 (which is after the marker frame at commit 3).
    // The reader must not double-insert the derived edge from the marker.
    let ro = GraphDb::open_at(&dir, 4).unwrap();
    // The rule engine re-derives the edge; adjacency must be correct.
    assert_eq!(ro.edge_count(), 1, "one derived edge at commit 4");
    // Prop must be visible.
    assert_eq!(ro.get_prop("p1", "color"), Some(Value::Str("blue".into())));
}

#[test]
fn was_linked_derived_edge_lifetime() {
    // was_linked must work across a derived edge's WAL lifetime.
    // The DerivedEdgeAdded marker is the commit at which the edge is "linked".
    let dir = tmp("wl-derived");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap(); // commit 0
    db.insert_node("Org", "o2", vec![]).unwrap(); // commit 1
    db.create_rule(fk_rule()).unwrap(); // commit 2
                                        // insert_node fires the rule → InsertNode at commit 3, DerivedEdgeAdded at commit 4.
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    // set_prop retracts p1→o1 and fires p1→o2.
    // SetProp at commit 5, Batch[DerivedEdgeRetracted(p1→o1), DerivedEdgeAdded(p1→o2)] at commit 6.
    db.set_prop("p1", "org_id", Value::Str("o2".into()))
        .unwrap();

    // Determine exact commit of the DerivedEdgeAdded marker by checking edge_history.
    let h = db.edge_history("p1", "o1").unwrap();
    assert_eq!(h.items.len(), 2, "p1→o1 history: {:?}", h.items);
    let added_commit = h.items[0].commit;
    let retracted_commit = h.items[1].commit;

    // Before the added commit: not linked.
    if added_commit > 0 {
        assert!(
            !db.was_linked("p1", "o1", "BelongsTo", added_commit - 1)
                .unwrap(),
            "before marker: not linked"
        );
    }
    // At added commit: linked.
    assert!(
        db.was_linked("p1", "o1", "BelongsTo", added_commit)
            .unwrap(),
        "at added commit: linked"
    );
    // Between added and retracted: still linked (if there are commits between).
    if retracted_commit > added_commit + 1 {
        assert!(
            db.was_linked("p1", "o1", "BelongsTo", retracted_commit - 1)
                .unwrap(),
            "before retract: linked"
        );
    }
    // At retracted commit: no longer linked.
    assert!(
        !db.was_linked("p1", "o1", "BelongsTo", retracted_commit)
            .unwrap(),
        "at retracted commit: not linked"
    );
}

#[test]
fn group_commit_triggers_rule_markers_written() {
    // write_batch (group-commit path) must also write DerivedEdgeAdded markers
    // when a rule fires during the batch commit.
    let dir = tmp("eh-batch-markers");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.create_rule(fk_rule()).unwrap();
    // Trigger rule via write_batch (one Batch WAL frame).
    db.write_batch(|b| {
        b.insert_node(
            "Person",
            "p1",
            vec![("org_id".into(), Value::Str("o1".into()))],
        );
    })
    .unwrap();
    assert_eq!(db.edge_count(), 1, "derived edge created via batch");
    // edge_history must show the DerivedEdgeAdded marker with rule attribution.
    let result = db.edge_history("p1", "o1").unwrap();
    assert_eq!(
        result.items.len(),
        1,
        "marker from batch commit: {:?}",
        result.items
    );
    assert_eq!(result.items[0].event, EdgeEvent::Added);
    assert_eq!(
        result.items[0].rule,
        Some("fk".to_string()),
        "batch-commit marker must carry rule attribution"
    );
}

#[test]
fn delete_node_with_live_derived_edge_single_retraction() {
    // C1 regression: deleting a node while a derived edge is live must produce
    // EXACTLY ONE Retracted event with rule attribution (Some("fk")), not two
    // (a synthetic rule:None from the DeleteNode sweep + a marker rule:Some).
    let dir = tmp("eh-delete-derived");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.create_rule(fk_rule()).unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    assert_eq!(db.edge_count(), 1, "derived edge exists before delete");

    // Delete the src node — triggers DeleteNode WAL record immediately followed
    // by a DerivedEdgeRetracted marker.
    db.delete_node("p1").unwrap();

    let result = db.edge_history("p1", "o1").unwrap();
    assert_eq!(
        result.items.len(),
        2,
        "expected Added + exactly one Retracted; got: {:?}",
        result.items
    );
    assert_eq!(result.items[0].event, EdgeEvent::Added);
    assert_eq!(result.items[0].rule, Some("fk".to_string()));
    assert_eq!(result.items[1].event, EdgeEvent::Retracted);
    assert_eq!(
        result.items[1].rule,
        Some("fk".to_string()),
        "single Retracted must carry rule attribution, not rule:None from synthetic sweep"
    );
}

#[test]
fn was_linked_false_after_delete_node_removes_derived_edge() {
    // was_linked must return false at the DeleteNode commit for a derived edge
    // that was live on the deleted node.
    let dir = tmp("wl-delete-derived");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.create_rule(fk_rule()).unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    assert_eq!(db.edge_count(), 1, "derived edge exists before delete");

    // Locate the Added marker commit from edge_history.
    let h = db.edge_history("p1", "o1").unwrap();
    let added_commit = h.items[0].commit;
    assert!(
        db.was_linked("p1", "o1", "BelongsTo", added_commit)
            .unwrap(),
        "linked at Added commit"
    );

    db.delete_node("p1").unwrap();

    // Re-read history to find the Retracted commit.
    let h2 = db.edge_history("p1", "o1").unwrap();
    assert_eq!(h2.items.len(), 2, "Added + Retracted: {:?}", h2.items);
    let retracted_commit = h2.items[1].commit;

    // At the Retracted commit: not linked.
    assert!(
        !db.was_linked("p1", "o1", "BelongsTo", retracted_commit)
            .unwrap(),
        "not linked at Retracted commit"
    );
}

#[test]
fn delete_node_with_mixed_manual_and_derived_edges() {
    // When a node has both a manual edge and a derived edge, DeleteNode must
    // emit exactly two Retracted events: one rule:None (manual) and one
    // rule:Some (derived).  Order follows the WAL: manual sweep before marker.
    let dir = tmp("eh-delete-mixed");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.insert_node("Org", "o2", vec![]).unwrap();
    db.create_rule(fk_rule()).unwrap();
    // Insert p1 with a FK to o1 (fires derived BelongsTo p1→o1).
    db.insert_node(
        "Person",
        "p1",
        vec![("org_id".into(), Value::Str("o1".into()))],
    )
    .unwrap();
    // Also manually link p1 to o2.
    db.insert_edge("Link", "p1", "o2").unwrap();

    // Verify both edges exist.
    assert_eq!(db.edge_count(), 2, "one derived + one manual edge");

    db.delete_node("p1").unwrap();

    // Manual edge p1→o2: Added(rule:None) then Retracted(rule:None).
    let manual = db.edge_history("p1", "o2").unwrap();
    assert_eq!(
        manual.items.len(),
        2,
        "manual edge history: {:?}",
        manual.items
    );
    assert_eq!(manual.items[0].event, EdgeEvent::Added);
    assert_eq!(manual.items[0].rule, None);
    assert_eq!(manual.items[1].event, EdgeEvent::Retracted);
    assert_eq!(
        manual.items[1].rule, None,
        "manual edge retraction must have rule:None"
    );

    // Derived edge p1→o1: Added(rule:Some) then Retracted(rule:Some).
    let derived = db.edge_history("p1", "o1").unwrap();
    assert_eq!(
        derived.items.len(),
        2,
        "derived edge history: {:?}",
        derived.items
    );
    assert_eq!(derived.items[0].event, EdgeEvent::Added);
    assert_eq!(derived.items[0].rule, Some("fk".to_string()));
    assert_eq!(derived.items[1].event, EdgeEvent::Retracted);
    assert_eq!(
        derived.items[1].rule,
        Some("fk".to_string()),
        "derived edge retraction must carry rule attribution"
    );
}
