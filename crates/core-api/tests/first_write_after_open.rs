//! What a write reads before it commits has to come from the snapshot too.
//!
//! A store opened from a snapshot leaves the snapshot's provenance — which rule
//! owns which edge — unread until something needs it. The commit itself always
//! reads it first; the checks and reports a write computes before it commits
//! did not, so on the first write after a clean open they saw no rule owning
//! anything (ledger rows 108 and 110).
use mushroomdb::{DeleteReport, GraphDb, GraphError, Predicate, RuleDef, Value};
use std::path::PathBuf;

type Db = GraphDb<core_storage::fs::RealFs>;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("first-write-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn same_kind_rule() -> RuleDef {
    RuleDef {
        name: "r".into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::FieldEqual {
            field: "kind".into(),
        },
        edge_type: "SAME".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

/// a and b share a kind, so the rule derives a→b and b→a; then a snapshot.
fn snapshotted_store_with_two_derived_edges(tag: &str) -> PathBuf {
    let dir = tmp(tag);
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_rule(same_kind_rule()).unwrap();
    for k in ["a", "b"] {
        db.insert_node("A", k, vec![("kind".into(), Value::Str("x".into()))])
            .unwrap();
    }
    assert_eq!(db.stats().edges, 2);
    db.snapshot().unwrap();
    dir
}

#[test]
fn deleting_a_node_first_after_open_counts_its_derived_edges_as_derived() {
    let dir = snapshotted_store_with_two_derived_edges("row110");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.delete_node("a").unwrap(),
        DeleteReport {
            manual_edges: 0,
            derived_edges: 2,
        }
    );
}

#[test]
fn inserting_a_rule_owned_edge_first_after_open_is_refused_as_rule_owned() {
    let dir = snapshotted_store_with_two_derived_edges("row108-open");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    assert!(matches!(
        db.insert_edge("SAME", "a", "b"),
        Err(GraphError::RuleOwned { .. })
    ));
}

#[test]
fn inserting_a_rule_owned_edge_in_the_handle_that_snapshotted_is_refused_as_rule_owned() {
    let dir = tmp("row108-same");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_rule(same_kind_rule()).unwrap();
    for k in ["a", "b"] {
        db.insert_node("A", k, vec![("kind".into(), Value::Str("x".into()))])
            .unwrap();
    }
    db.snapshot().unwrap();
    assert!(matches!(
        db.insert_edge("SAME", "a", "b"),
        Err(GraphError::RuleOwned { .. })
    ));
}

#[test]
fn inserting_a_rule_owned_edge_in_a_batch_first_after_open_is_refused_as_rule_owned() {
    let dir = snapshotted_store_with_two_derived_edges("row108-batch");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    let mut batch = db.batch();
    batch.insert_edge("SAME", "a", "b");
    assert!(matches!(batch.commit(), Err(GraphError::RuleOwned { .. })));
}

#[test]
fn deleting_a_rule_owned_edge_first_after_open_is_refused_as_rule_owned() {
    let dir = snapshotted_store_with_two_derived_edges("row108-del");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    assert!(matches!(
        db.delete_edge("SAME", "a", "b"),
        Err(GraphError::RuleOwned { .. })
    ));
    assert_eq!(db.stats().edges, 2);
}
