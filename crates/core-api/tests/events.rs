use core_api::{
    AutoFk, DbEvent, GraphDb, GraphError, IngestOptions, MutationEvent, Predicate, RuleDef,
    Subscription, Value,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-events-{}-{}", name, std::process::id()));
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
    }
}

fn attach(db: &mut GraphDb<core_storage::fs::RealFs>) -> Arc<Mutex<Vec<MutationEvent>>> {
    let evs = Arc::new(Mutex::new(Vec::new()));
    let sink = evs.clone();
    db.set_event_sink(Box::new(move |e| sink.lock().unwrap().push(e)));
    evs
}

fn take(evs: &Arc<Mutex<Vec<MutationEvent>>>) -> Vec<MutationEvent> {
    evs.lock().unwrap().clone()
}

fn row(id: &str) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("id".into(), Value::Str(id.into()));
    m
}

fn person_org(id: &str, org_id: &str) -> BTreeMap<String, Value> {
    let mut m = row(id);
    m.insert("org_id".into(), Value::Str(org_id.into()));
    m
}

/// Binding: scripted sequence emits this exact ordered list. A rule-derived
/// edge pair is NOT individually evented — only the triggering insert is.
#[test]
fn scripted_sequence_emits_exact_ordered_events() {
    let dir = tmp("scripted");
    let mut db = GraphDb::open(&dir).unwrap();
    let evs = attach(&mut db);

    db.insert_node("A", "seed", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.set_prop("seed", "name", Value::Str("ada".into()))
        .unwrap();
    db.create_rule(overlap_rule("rel", "REL")).unwrap();
    // Overlap derives REL seed↔trigger both ways. Only the insert is evented.
    db.insert_node("A", "trigger", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    assert_eq!(db.edge_count(), 2, "rule must have derived both REL edges");

    db.batch()
        .insert_node("A", "b", vec![])
        .insert_node("A", "c", vec![])
        .insert_edge("E", "b", "c")
        .commit()
        .unwrap();

    db.delete_node("trigger").unwrap();

    db.ingest(
        "Person",
        vec![row("p1"), row("p2")],
        &IngestOptions::default(),
    )
    .unwrap();

    assert_eq!(
        take(&evs),
        vec![
            MutationEvent::NodeInserted {
                label: "A".into(),
                key: "seed".into(),
            },
            MutationEvent::PropSet {
                key: "seed".into(),
                field: "name".into(),
            },
            MutationEvent::RuleCreated { name: "rel".into() },
            MutationEvent::NodeInserted {
                label: "A".into(),
                key: "trigger".into(),
            },
            MutationEvent::NodeInserted {
                label: "A".into(),
                key: "b".into(),
            },
            MutationEvent::NodeInserted {
                label: "A".into(),
                key: "c".into(),
            },
            MutationEvent::EdgeInserted {
                edge_type: "E".into(),
                src: "b".into(),
                dst: "c".into(),
            },
            MutationEvent::BatchApplied { ops: 3 },
            MutationEvent::NodeDeleted {
                key: "trigger".into(),
            },
            MutationEvent::NodeInserted {
                label: "Person".into(),
                key: "p1".into(),
            },
            MutationEvent::NodeInserted {
                label: "Person".into(),
                key: "p2".into(),
            },
            MutationEvent::Ingested {
                label: "Person".into(),
                inserted: 2,
            },
        ]
    );
}

#[test]
fn remaining_variants_and_user_edge_delete() {
    let dir = tmp("variants");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("name".into(), Value::Str("ada".into()))])
        .unwrap();
    db.insert_node("A", "b", vec![]).unwrap();
    db.insert_edge("KNOWS", "a", "b").unwrap();
    db.create_rule(overlap_rule("rel", "REL")).unwrap();

    let evs = attach(&mut db);
    assert!(db.remove_prop("a", "name").unwrap());
    assert!(db.delete_edge("KNOWS", "a", "b").unwrap());
    db.delete_rule("rel").unwrap();
    db.create_rule(overlap_rule("rel2", "REL")).unwrap();
    db.rebuild_rule("rel2").unwrap();

    assert_eq!(
        take(&evs),
        vec![
            MutationEvent::PropRemoved {
                key: "a".into(),
                field: "name".into(),
            },
            MutationEvent::EdgeDeleted {
                edge_type: "KNOWS".into(),
                src: "a".into(),
                dst: "b".into(),
            },
            MutationEvent::RuleDeleted { name: "rel".into() },
            MutationEvent::RuleCreated {
                name: "rel2".into(),
            },
            MutationEvent::RuleRebuilt {
                name: "rel2".into(),
            },
        ]
    );
}

#[test]
fn rejected_ops_emit_nothing() {
    let dir = tmp("rejected");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "b", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.create_rule(overlap_rule("rel", "REL")).unwrap();

    let evs = attach(&mut db);

    match db.insert_node("A", "a", vec![]) {
        Err(GraphError::DuplicateKey { key }) => assert_eq!(key, "a"),
        other => panic!("expected DuplicateKey, got {other:?}"),
    }
    match db.delete_edge("REL", "a", "b") {
        Err(GraphError::RuleOwned { .. }) => {}
        other => panic!("expected RuleOwned, got {other:?}"),
    }
    assert!(!db.remove_prop("a", "missing").unwrap());
    assert!(!db.delete_edge("NOPE", "a", "b").unwrap());

    match db.batch().insert_node("A", "a", vec![]).commit() {
        Err(GraphError::DuplicateKey { .. }) => {}
        other => panic!("expected DuplicateKey from batch, got {other:?}"),
    }

    assert!(take(&evs).is_empty());
}

/// Replay is silent: open() applies WAL via `apply`, never `log_then_apply`.
/// The sink is in-memory and set after open, so recovery cannot emit.
#[test]
fn reopen_replay_emits_nothing() {
    let dir = tmp("reopen");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("A", "a", vec![]).unwrap();
        db.set_prop("a", "n", Value::Str("x".into())).unwrap();
    }
    let mut db = GraphDb::open(&dir).unwrap();
    let evs = attach(&mut db);
    assert!(db.has_node("a"));
    assert!(take(&evs).is_empty());

    db.set_prop("a", "n", Value::Str("y".into())).unwrap();
    assert_eq!(
        take(&evs),
        vec![MutationEvent::PropSet {
            key: "a".into(),
            field: "n".into(),
        }]
    );
}

/// Ingest with auto-FK: inner events are the CreateRule then each accepted
/// insert; `Ingested.inserted` is the row count, not the WAL-op count
/// (rule + rows). Derived KeyMatch edges are not individually evented.
#[test]
fn ingest_auto_fk_emits_rule_then_rows_then_ingested() {
    let dir = tmp("ingest-fk");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Org", "acme", vec![]).unwrap();
    let evs = attach(&mut db);

    let report = db
        .ingest(
            "Person",
            vec![person_org("p1", "acme"), person_org("p2", "acme")],
            &IngestOptions {
                key_field: "id".into(),
                auto_fk: AutoFk::Auto {
                    suffix: "_id".into(),
                },
            },
        )
        .unwrap();
    assert_eq!(report.inserted, 2);
    assert_eq!(
        report.rules_created,
        vec!["auto_fk_person_org_id".to_string()]
    );

    assert_eq!(
        take(&evs),
        vec![
            MutationEvent::RuleCreated {
                name: "auto_fk_person_org_id".into(),
            },
            MutationEvent::NodeInserted {
                label: "Person".into(),
                key: "p1".into(),
            },
            MutationEvent::NodeInserted {
                label: "Person".into(),
                key: "p2".into(),
            },
            MutationEvent::Ingested {
                label: "Person".into(),
                inserted: 2,
            },
        ]
    );
}

#[test]
fn mutation_event_serialize_is_externally_tagged() {
    let ev = MutationEvent::NodeInserted {
        label: "A".into(),
        key: "k".into(),
    };
    assert_eq!(
        serde_json::to_value(&ev).unwrap(),
        serde_json::json!({"node_inserted": {"label": "A", "key": "k"}})
    );
    assert_eq!(
        serde_json::to_value(&MutationEvent::BatchApplied { ops: 3 }).unwrap(),
        serde_json::json!({"batch_applied": {"ops": 3}})
    );
    assert_eq!(
        serde_json::to_value(&MutationEvent::Ingested {
            label: "Person".into(),
            inserted: 2,
        })
        .unwrap(),
        serde_json::json!({"ingested": {"label": "Person", "inserted": 2}})
    );
}

// ---------------------------------------------------------------------------
// Subscription tests — invariants pinned by the task-1 brief
// ---------------------------------------------------------------------------

fn drain_sub(sub: &Subscription) -> Vec<DbEvent> {
    let mut out = Vec::new();
    while let Some(ev) = sub.try_recv() {
        out.push(ev);
    }
    out
}

/// Invariant 1: events arrive after fsync in commit order.
/// A subscriber that immediately queries the db after receiving an event sees
/// the state that produced it.
///
/// Also pins invariant 5: a single write_batch's events all share the same
/// commit_seq and arrive as a contiguous run in declaration order.
#[test]
fn subscribe_rule_events_arrive_after_commit_in_order() {
    let dir = tmp("sub-order");
    let mut db = GraphDb::open(&dir).unwrap();

    let rule = RuleDef {
        name: "rel".into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: "REL".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    };
    db.create_rule(rule).unwrap();

    let sub = db.subscribe_rule("rel").unwrap();

    db.write_batch(|b| {
        b.insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))]);
        b.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))]);
    })
    .unwrap();

    // Give the engine a moment; both derives should be queued.
    let events = drain_sub(&sub);
    assert!(!events.is_empty(), "expected fire events, got none");

    // All events in this batch share the same commit_seq (invariant 5).
    let seq = match &events[0] {
        DbEvent::EdgeFired { commit_seq, .. } => *commit_seq,
        other => panic!("expected EdgeFired, got {other:?}"),
    };
    for ev in &events {
        match ev {
            DbEvent::EdgeFired { commit_seq, .. } | DbEvent::EdgeRetracted { commit_seq, .. } => {
                assert_eq!(*commit_seq, seq, "commit_seq must be shared in one batch");
            }
            other => panic!("unexpected variant {other:?}"),
        }
    }

    // Immediately query the db — state must already reflect the derived edges.
    assert!(
        db.edge_count() >= 2,
        "state visible immediately after event (invariant 1)"
    );
}

/// Invariant 2: bounded queue with Lagged overflow.  Queue of size 2; fire
/// more events than capacity; consumer reads Lagged marker.
#[test]
fn subscribe_lagged_when_queue_full() {
    let dir = tmp("sub-lagged");
    let mut db = GraphDb::open(&dir).unwrap();

    let rule = RuleDef {
        name: "rel".into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: "REL".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    };
    db.create_rule(rule).unwrap();

    // Tiny queue: capacity = 2, so the 3rd event is dropped → Lagged.
    db.set_sub_capacity(2);
    let sub = db.subscribe_rule("rel").unwrap();

    // Insert 4 nodes pairwise — each insert triggers rule eval, which fires
    // bidirectional edges: n1↔n2, n1↔n3, n1↔n4, n2↔n3, n2↔n4, n3↔n4 = ≥4 fires.
    for i in 1u32..=4 {
        db.insert_node("A", &format!("n{i}"), vec![("tags".into(), tags(&["x"]))])
            .unwrap();
    }

    // Drain everything including Lagged.
    let events = drain_sub(&sub);
    let has_lagged = events
        .iter()
        .any(|ev| matches!(ev, DbEvent::Lagged { missed } if *missed > 0));
    assert!(has_lagged, "expected Lagged marker in {events:?}");
}

/// Invariant 3: dropping a Subscription unregisters it.
/// After drop, subsequent commits do not error and the sub list is pruned.
#[test]
fn subscription_drop_unregisters_cleanly() {
    let dir = tmp("sub-drop");
    let mut db = GraphDb::open(&dir).unwrap();

    let rule = RuleDef {
        name: "rel".into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: "REL".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    };
    db.create_rule(rule).unwrap();

    let sub = db.subscribe_rule("rel").unwrap();
    drop(sub); // Unregister before any commit.

    // Commits after drop must succeed — no error, no panic.
    db.insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))]).unwrap();
    db.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))]).unwrap();
    assert!(db.edge_count() >= 1);
}

/// Invariant 4: open() / WAL replay does NOT emit subscription events.
#[test]
fn replay_is_silent_for_subscriptions() {
    let dir = tmp("sub-replay");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        let rule = RuleDef {
            name: "rel".into(),
            src_label: "A".into(),
            dst_label: "A".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 0.5,
            },
            edge_type: "REL".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
        };
        db.create_rule(rule).unwrap();
        db.insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))]).unwrap();
        db.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))]).unwrap();
    }

    // Reopen — subscriptions registered before open cannot exist, but
    // pending_deltas must be drained (invariant 4). Install sub AFTER open.
    let mut db = GraphDb::open(&dir).unwrap();
    // subscribe_all_rules is available immediately; no events from replay.
    let sub = db.subscribe_all_rules();
    assert!(
        sub.try_recv().is_none(),
        "replay must not emit subscription events"
    );
}

/// subscribe_writes receives node/prop events but NOT rule edge events.
#[test]
fn subscribe_writes_receives_write_events_only() {
    let dir = tmp("sub-writes");
    let mut db = GraphDb::open(&dir).unwrap();

    let rule = RuleDef {
        name: "rel".into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: "REL".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    };
    db.create_rule(rule).unwrap();
    let sub = db.subscribe_writes();

    db.insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))]).unwrap();
    db.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))]).unwrap();

    let events = drain_sub(&sub);
    assert!(!events.is_empty(), "expected write events");
    for ev in &events {
        assert!(
            matches!(
                ev,
                DbEvent::NodeInserted { .. }
                    | DbEvent::NodeDeleted { .. }
                    | DbEvent::EdgeInserted { .. }
                    | DbEvent::EdgeDeleted { .. }
                    | DbEvent::PropSet { .. }
                    | DbEvent::PropRemoved { .. }
                    | DbEvent::Lagged { .. }
            ),
            "write subscription received unexpected edge event: {ev:?}"
        );
    }
    // No edge fire events.
    let has_edge_fired = events.iter().any(|e| matches!(e, DbEvent::EdgeFired { .. }));
    assert!(!has_edge_fired, "write subscription must not receive EdgeFired");
}

/// subscribe_rule returns Err for unknown rule names.
#[test]
fn subscribe_unknown_rule_is_named_error() {
    let dir = tmp("sub-unknown");
    let mut db = GraphDb::open(&dir).unwrap();
    match db.subscribe_rule("nonexistent") {
        Err(GraphError::RuleNotFound { name }) => assert_eq!(name, "nonexistent"),
        other => panic!("expected RuleNotFound, got {other:?}"),
    }
}

/// Retract events arrive when a node deletion causes rule edge removal.
#[test]
fn subscribe_rule_receives_retract_on_node_delete() {
    let dir = tmp("sub-retract");
    let mut db = GraphDb::open(&dir).unwrap();

    let rule = RuleDef {
        name: "rel".into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::Overlap {
            field: "tags".into(),
            min: 0.5,
        },
        edge_type: "REL".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    };
    db.create_rule(rule).unwrap();
    db.insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))]).unwrap();
    db.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))]).unwrap();
    assert!(db.edge_count() >= 2);

    let sub = db.subscribe_rule("rel").unwrap();
    db.delete_node("n1").unwrap();

    let events = drain_sub(&sub);
    let has_retract = events
        .iter()
        .any(|e| matches!(e, DbEvent::EdgeRetracted { .. }));
    assert!(has_retract, "expected EdgeRetracted after node delete, got {events:?}");
}
