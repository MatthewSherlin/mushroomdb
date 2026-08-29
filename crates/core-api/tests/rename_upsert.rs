/// Tests for rename_node, InsertEdgeUpsert, and reader.rs MVCC coherence for
/// RenameNode — see Task 2 brief.
use core_api::{
    Direction, EdgeEvent, GraphDb, GraphError, HistoryChange, Predicate, RuleDef, SharedDb, Value,
};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-rename-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

// ── rename_node: basic semantics ─────────────────────────────────────────────

#[test]
fn rename_node_resolves_under_new_name() {
    let dir = tmp("resolve");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![("age".into(), Value::Int(30))])
        .unwrap();
    db.rename_node("alice", "alice2").unwrap();
    assert!(db.has_node("alice2"), "new key must be live");
    assert!(!db.has_node("alice"), "old key must be gone");
    let info = db.node_info("alice2").expect("new key must resolve");
    assert_eq!(info.label, "Person");
    assert_eq!(info.props.get("age"), Some(&Value::Int(30)));
}

#[test]
fn rename_node_old_key_404() {
    let dir = tmp("old404");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.rename_node("alice", "alice2").unwrap();
    assert!(db.node_info("alice").is_none());
}

#[test]
fn rename_node_id_stable_edges_follow() {
    let dir = tmp("edges");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node("Person", "bob", vec![]).unwrap();
    db.insert_edge("KNOWS", "alice", "bob").unwrap();
    db.rename_node("alice", "alice2").unwrap();
    // Edge is still reachable from alice2
    let nbrs = db.neighbors("alice2", "KNOWS", Direction::Out).unwrap();
    assert_eq!(nbrs, vec!["bob"]);
    // And from bob's perspective
    let nbrs_in = db.neighbors("bob", "KNOWS", Direction::In).unwrap();
    assert_eq!(nbrs_in, vec!["alice2"]);
}

#[test]
fn rename_node_duplicate_target_rejected() {
    let dir = tmp("dup");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node("Person", "alice2", vec![]).unwrap();
    let err = db.rename_node("alice", "alice2").unwrap_err();
    assert!(
        matches!(err, GraphError::DuplicateKey { .. }),
        "expected DuplicateKey, got {err:?}"
    );
}

#[test]
fn rename_node_unknown_old_key_rejected() {
    let dir = tmp("unk");
    let mut db = GraphDb::open(&dir).unwrap();
    let err = db.rename_node("ghost", "anything").unwrap_err();
    assert!(
        matches!(err, GraphError::KeyNotFound { .. }),
        "expected KeyNotFound, got {err:?}"
    );
}

#[test]
fn rename_node_last_change_updated() {
    let dir = tmp("lc");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    let before = db.last_changed("alice").unwrap_or(0);
    db.rename_node("alice", "alice2").unwrap();
    // last_changed on the old key: should be None (node gone from that key)
    assert!(
        db.last_changed("alice").is_none(),
        "last_changed(old key) should be None after rename"
    );
    let after = db
        .last_changed("alice2")
        .expect("last_changed(new key) must be Some");
    assert!(after > before, "last_change must increase after rename");
}

#[test]
fn rename_node_replay_identity() {
    let dir = tmp("replay");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("Person", "alice", vec![("age".into(), Value::Int(30))])
            .unwrap();
        db.insert_node("Person", "bob", vec![]).unwrap();
        db.insert_edge("KNOWS", "alice", "bob").unwrap();
        db.rename_node("alice", "alice2").unwrap();
    }
    // Reopen and verify state
    let db = GraphDb::open(&dir).unwrap();
    assert!(db.has_node("alice2"), "alice2 must survive reopen");
    assert!(!db.has_node("alice"), "alice must be gone after reopen");
    let nbrs = db.neighbors("alice2", "KNOWS", Direction::Out).unwrap();
    assert_eq!(nbrs, vec!["bob"]);
    let info = db.node_info("alice2").unwrap();
    assert_eq!(info.props.get("age"), Some(&Value::Int(30)));
}

// ── rename_node: reader.rs MVCC coherence ────────────────────────────────────

#[test]
fn rename_node_reader_coherence() {
    let dir = tmp("mvcc");
    let db = SharedDb::open(&dir).unwrap();
    db.write()
        .insert_node("Person", "alice", vec![("age".into(), Value::Int(30))])
        .unwrap();
    db.write().rename_node("alice", "alice2").unwrap();

    let snap = db.reader();
    let params = BTreeMap::new();
    let rs = snap.query("MATCH (n:Person) RETURN n", &params).unwrap();
    // Only alice2 should be visible; alice must be gone.
    let keys: Vec<String> = (0..rs.len())
        .filter_map(|i| {
            rs.row(i).first().and_then(|c| c.as_ref()).and_then(|v| {
                if let core_api::Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
        })
        .collect();
    assert!(
        keys.contains(&"alice2".to_string()),
        "alice2 must be in snapshot"
    );
    assert!(
        !keys.contains(&"alice".to_string()),
        "alice must not appear in snapshot after rename"
    );
}

// ── rename_node: history follows by id ───────────────────────────────────────

#[test]
fn rename_node_history_follows_by_id() {
    let dir = tmp("hist");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node("Person", "bob", vec![]).unwrap();
    db.insert_edge("KNOWS", "alice", "bob").unwrap();
    db.rename_node("alice", "alice2").unwrap();
    // node_history on the new key should include the prior edge event
    let hist = db.node_history("alice2").unwrap();
    assert!(
        hist.iter().any(|e| {
            matches!(
                &e.change,
                core_api::HistoryChange::EdgeAdded { edge_type, .. } if edge_type == "KNOWS"
            )
        }),
        "history of alice2 should include the KNOWS EdgeAdded event: {hist:?}"
    );
}

// ── rename_node: batch variant ────────────────────────────────────────────────

#[test]
fn rename_node_via_batch() {
    let dir = tmp("batch");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.batch().rename_node("alice", "alice2").commit().unwrap();
    assert!(db.has_node("alice2"));
    assert!(!db.has_node("alice"));
}

#[test]
fn batch_rename_and_insert_edge_in_same_batch() {
    let dir = tmp("batch-rename-edge");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node("Person", "bob", vec![]).unwrap();
    // rename alice→alice2, then insert edge from alice2→bob in the same batch
    db.batch()
        .rename_node("alice", "alice2")
        .insert_edge("KNOWS", "alice2", "bob")
        .commit()
        .unwrap();
    assert!(db.has_node("alice2"));
    assert!(!db.has_node("alice"));
    let nbrs = db.neighbors("alice2", "KNOWS", Direction::Out).unwrap();
    assert_eq!(nbrs, vec!["bob"]);
}

// ── InsertEdgeUpsert ──────────────────────────────────────────────────────────

#[test]
fn insert_edge_upsert_both_endpoints_missing() {
    let dir = tmp("upsert-both");
    let mut db = GraphDb::open(&dir).unwrap();
    db.batch()
        .insert_edge_upsert("KNOWS", "alice", "bob", "Person")
        .commit()
        .unwrap();
    assert!(db.has_node("alice"), "src must be created");
    assert!(db.has_node("bob"), "dst must be created");
    // Check placeholder label
    let info_a = db.node_info("alice").unwrap();
    assert_eq!(info_a.label, "Person");
    // Check no props on placeholder
    assert!(info_a.props.is_empty(), "placeholder must have no props");
    // Check edge exists
    let nbrs = db.neighbors("alice", "KNOWS", Direction::Out).unwrap();
    assert_eq!(nbrs, vec!["bob"]);
}

#[test]
fn insert_edge_upsert_src_missing_only() {
    let dir = tmp("upsert-src");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "bob", vec![("age".into(), Value::Int(30))])
        .unwrap();
    db.batch()
        .insert_edge_upsert("KNOWS", "alice", "bob", "Placeholder")
        .commit()
        .unwrap();
    assert!(db.has_node("alice"));
    let info_a = db.node_info("alice").unwrap();
    assert_eq!(info_a.label, "Placeholder");
    // bob keeps its original props
    let info_b = db.node_info("bob").unwrap();
    assert_eq!(info_b.props.get("age"), Some(&Value::Int(30)));
    let nbrs = db.neighbors("alice", "KNOWS", Direction::Out).unwrap();
    assert_eq!(nbrs, vec!["bob"]);
}

#[test]
fn insert_edge_upsert_dst_missing_only() {
    let dir = tmp("upsert-dst");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![("age".into(), Value::Int(25))])
        .unwrap();
    db.batch()
        .insert_edge_upsert("KNOWS", "alice", "bob", "Placeholder")
        .commit()
        .unwrap();
    assert!(db.has_node("bob"));
    let info_b = db.node_info("bob").unwrap();
    assert_eq!(info_b.label, "Placeholder");
    assert!(info_b.props.is_empty());
    // alice keeps props
    let info_a = db.node_info("alice").unwrap();
    assert_eq!(info_a.props.get("age"), Some(&Value::Int(25)));
    let nbrs = db.neighbors("alice", "KNOWS", Direction::Out).unwrap();
    assert_eq!(nbrs, vec!["bob"]);
}

#[test]
fn insert_edge_upsert_none_missing() {
    let dir = tmp("upsert-none");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node("Person", "bob", vec![]).unwrap();
    // Should succeed and just insert the edge
    db.batch()
        .insert_edge_upsert("KNOWS", "alice", "bob", "Placeholder")
        .commit()
        .unwrap();
    assert_eq!(
        db.neighbors("alice", "KNOWS", Direction::Out).unwrap(),
        vec!["bob"]
    );
    // alice and bob label should be unchanged
    assert_eq!(db.node_info("alice").unwrap().label, "Person");
    assert_eq!(db.node_info("bob").unwrap().label, "Person");
}

#[test]
fn insert_edge_upsert_rules_fire_on_placeholder() {
    // Verify that on_node_changed fires for placeholder nodes: when a rule is
    // configured for "Placeholder" nodes and props are set in the same batch
    // as the upsert, derived edges appear.
    use core_api::{Predicate, RuleDef};
    let dir = tmp("upsert-rules");
    let mut db = GraphDb::open(&dir).unwrap();
    // Rule: link Placeholder nodes that have the same "kind" field value.
    db.create_rule(RuleDef {
        name: "link_by_kind".into(),
        src_label: "Placeholder".into(),
        dst_label: "Placeholder".into(),
        predicate: Predicate::FieldEqual {
            field: "kind".into(),
        },
        edge_type: "RELATED".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    // Upsert creates p1 and p2 as Placeholder; set props in the same batch.
    db.batch()
        .insert_edge_upsert("KNOWS", "p1", "p2", "Placeholder")
        .set_prop("p1", "kind", Value::Str("widget".into()))
        .set_prop("p2", "kind", Value::Str("widget".into()))
        .commit()
        .unwrap();
    // Rule must have fired: p1 and p2 both have kind="widget" → RELATED edge.
    let nbrs = db.neighbors("p1", "RELATED", Direction::Out).unwrap();
    assert_eq!(
        nbrs,
        vec!["p2"],
        "rule must fire for placeholder nodes with matching kind"
    );
}

#[test]
fn insert_edge_upsert_batch_atomicity_failing_op() {
    let dir = tmp("upsert-atomic");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    // A batch with upsert + an op that will fail (insert duplicate node)
    let result = db
        .batch()
        .insert_edge_upsert("KNOWS", "alice", "bob", "Person")
        .insert_node("Person", "alice", vec![]) // alice already exists → DuplicateKey
        .commit();
    assert!(result.is_err(), "batch must fail atomically");
    // bob must not have been inserted (atomicity)
    assert!(
        !db.has_node("bob"),
        "bob must not exist after atomic batch failure"
    );
}

#[test]
fn insert_edge_upsert_placeholder_last_change_touched() {
    let dir = tmp("upsert-lc");
    let mut db = GraphDb::open(&dir).unwrap();
    db.batch()
        .insert_edge_upsert("KNOWS", "alice", "bob", "Person")
        .commit()
        .unwrap();
    assert!(
        db.last_changed("alice").is_some(),
        "last_changed must be set for newly created alice"
    );
    assert!(
        db.last_changed("bob").is_some(),
        "last_changed must be set for newly created bob"
    );
}

// ── history alias chain: rename visibility ────────────────────────────────────

/// node_history on the NEW key must include the NodeInserted record written
/// under the OLD key before the rename.
#[test]
fn node_history_shows_insert_through_rename() {
    let dir = tmp("nh-insert");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.rename_node("alice", "alice2").unwrap();

    let history = db.node_history("alice2").expect("history must succeed");
    let has_inserted = history
        .iter()
        .any(|e| matches!(&e.change, HistoryChange::NodeInserted { label } if label == "Person"));
    assert!(
        has_inserted,
        "node_history(alice2) must include the NodeInserted written under 'alice': {history:?}"
    );
}

/// edge_history on the NEW key pair (after rename) must surface DerivedEdgeAdded
/// markers that were written under the OLD key.
#[test]
fn edge_history_sees_derived_events_after_rename() {
    let dir = tmp("eh-derived");
    let mut db = GraphDb::open(&dir).unwrap();
    // Create two nodes, wire a rule, then rename one.
    db.insert_node("Item", "x", vec![("kind".into(), Value::Str("w".into()))])
        .unwrap();
    db.insert_node("Item", "y", vec![("kind".into(), Value::Str("w".into()))])
        .unwrap();
    db.create_rule(RuleDef {
        name: "ehr-rule".into(),
        src_label: "Item".into(),
        dst_label: "Item".into(),
        predicate: Predicate::FieldEqual {
            field: "kind".into(),
        },
        edge_type: "RELATED".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    // Rule fires: DerivedEdgeAdded written with src="x", dst="y"
    db.rename_node("x", "x2").unwrap();

    // edge_history on the new key pair must surface the Added event.
    let hr = db
        .edge_history("x2", "y")
        .expect("edge_history must succeed");
    let has_added = hr.items.iter().any(|e| e.event == EdgeEvent::Added);
    assert!(
        has_added,
        "edge_history(x2, y) must include DerivedEdgeAdded written under old key 'x': {:?}",
        hr.items
    );
}

/// was_linked on the NEW key pair must see the derived edge added before the rename.
#[test]
fn was_linked_resolves_through_rename() {
    let dir = tmp("wl-rename");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Item", "a", vec![("k".into(), Value::Str("v".into()))])
        .unwrap();
    db.insert_node("Item", "b", vec![("k".into(), Value::Str("v".into()))])
        .unwrap();
    db.create_rule(RuleDef {
        name: "wlr-rule".into(),
        src_label: "Item".into(),
        dst_label: "Item".into(),
        predicate: Predicate::FieldEqual { field: "k".into() },
        edge_type: "LINKED".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    // Rule fires at commit N. Rename happens at commit N+1.
    let total_before_rename = db.wal_total_commits().unwrap();
    db.rename_node("a", "a2").unwrap();

    // was_linked at the commit when the rule fired, querying with the NEW key.
    let linked = db
        .was_linked("a2", "b", "LINKED", total_before_rename - 1)
        .expect("was_linked must succeed");
    assert!(
        linked,
        "was_linked(a2, b, LINKED, commit-before-rename) must be true"
    );
}

// ── N1: temporal alias bounds — recycled-key exclusion ───────────────────────

/// Rename "a" → "b" at commit N, then insert a brand new node also called "a"
/// at commit N+2. node_history("b") must NOT include events from the new "a".
#[test]
fn node_history_alias_excludes_recycled_key_events() {
    let dir = tmp("alias-recycle");
    let mut db = GraphDb::open(&dir).unwrap();

    // commit 0: insert original "a"
    db.insert_node("T", "a", vec![]).unwrap();
    // commit 1: rename "a" → "b"  (retires key "a" for this identity)
    db.rename_node("a", "b").unwrap();
    // commit 2: insert a completely new node called "a" (different identity)
    db.insert_node("T", "a", vec![("recycled".into(), Value::Bool(true))])
        .unwrap();
    // commit 3: set a prop on the new "a" — must NOT appear in history("b")
    db.set_prop("a", "extra", Value::Int(99)).unwrap();

    let hist = db.node_history("b").unwrap();
    // history("b") must NOT contain the new "a"'s extra prop
    let has_recycled_set_prop = hist
        .iter()
        .any(|e| matches!(&e.change, HistoryChange::PropSet { field, .. } if field == "extra"));
    assert!(
        !has_recycled_set_prop,
        "node_history('b') must not include events from the recycled 'a' node"
    );

    // Sanity: history("a") must include the recycled node's prop
    let hist_a = db.node_history("a").unwrap();
    let has_new_a_prop = hist_a
        .iter()
        .any(|e| matches!(&e.change, HistoryChange::PropSet { field, .. } if field == "extra"));
    assert!(
        has_new_a_prop,
        "node_history('a') must include the recycled node's own events"
    );
}

/// Multi-hop reuse: a→b at N1, new "a" at N2, a→c at N3.
/// node_history("c") must see only the SECOND identity of "a" (commits N2..N3),
/// not the original identity (commits 0..N1).
#[test]
fn node_history_alias_multi_hop_reuse() {
    let dir = tmp("alias-multihop");
    let mut db = GraphDb::open(&dir).unwrap();

    // First identity of "a"
    db.insert_node("T", "a", vec![]).unwrap();
    db.set_prop("a", "phase", Value::Int(1)).unwrap();
    // Rename: "a" → "b"
    db.rename_node("a", "b").unwrap();

    // Second identity of "a"
    db.insert_node("T", "a", vec![]).unwrap();
    db.set_prop("a", "phase", Value::Int(2)).unwrap();
    // Rename: "a" → "c"
    db.rename_node("a", "c").unwrap();

    let hist = db.node_history("c").unwrap();

    // Must see phase=2 (second identity)
    let has_phase2 = hist.iter().any(|e| {
        matches!(&e.change, HistoryChange::PropSet { field, value }
            if field == "phase" && *value == Value::Int(2))
    });
    assert!(
        has_phase2,
        "node_history('c') must see the second identity's phase=2 event"
    );

    // Must NOT see phase=1 (first identity, pre-first-rename)
    let has_phase1 = hist.iter().any(|e| {
        matches!(&e.change, HistoryChange::PropSet { field, value }
            if field == "phase" && *value == Value::Int(1))
    });
    assert!(
        !has_phase1,
        "node_history('c') must not see the first identity's phase=1 event"
    );
}
