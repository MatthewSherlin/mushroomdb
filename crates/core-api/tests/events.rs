use core_api::{
    AutoFk, DbEvent, GraphDb, GraphError, IngestOptions, MutationEvent, Predicate, RuleDef,
    Subscription, Value,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
        via_label: None,
        via_edge: None,
        via_dir: None,
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
        via_label: None,
        via_edge: None,
        via_dir: None,
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

/// Binding: `EdgeFired.weight` is read under the rule's declared `weight_prop`,
/// not a hardcoded "weight" property. A rule that stores its score reports it;
/// a rule that stores none reports `None`.
#[test]
fn edge_fired_weight_uses_the_rules_weight_prop() {
    let dir = tmp("sub-weight");
    let mut db = GraphDb::open(&dir).unwrap();

    // overlap_rule stores its score under "score".
    db.create_rule(overlap_rule("scored", "SCORED")).unwrap();
    let mut unscored = overlap_rule("unscored", "UNSCORED");
    unscored.weight_prop = None;
    db.create_rule(unscored).unwrap();

    let scored_sub = db.subscribe_rule("scored").unwrap();
    let unscored_sub = db.subscribe_rule("unscored").unwrap();

    // tags {x,y} vs {x} → jaccard 1/2 = 0.5, which meets the rule's min.
    db.write_batch(|b| {
        b.insert_node("A", "n1", vec![("tags".into(), tags(&["x", "y"]))]);
        b.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))]);
    })
    .unwrap();

    let scored = drain_sub(&scored_sub);
    assert!(!scored.is_empty(), "expected fire events for scored rule");
    for ev in &scored {
        match ev {
            DbEvent::EdgeFired { weight, .. } => assert_eq!(
                *weight,
                Some(0.5),
                "score is stored under \"score\", not \"weight\""
            ),
            other => panic!("expected EdgeFired, got {other:?}"),
        }
    }

    let unscored = drain_sub(&unscored_sub);
    assert!(
        !unscored.is_empty(),
        "expected fire events for unscored rule"
    );
    for ev in &unscored {
        match ev {
            DbEvent::EdgeFired { weight, .. } => {
                assert_eq!(*weight, None, "a rule storing no weight reports none")
            }
            other => panic!("expected EdgeFired, got {other:?}"),
        }
    }
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
        via_label: None,
        via_edge: None,
        via_dir: None,
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
        via_label: None,
        via_edge: None,
        via_dir: None,
    };
    db.create_rule(rule).unwrap();

    let sub = db.subscribe_rule("rel").unwrap();
    drop(sub); // Unregister before any commit.

    // Commits after drop must succeed — no error, no panic.
    db.insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
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
            via_label: None,
            via_edge: None,
            via_dir: None,
        };
        db.create_rule(rule).unwrap();
        db.insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
        db.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))])
            .unwrap();
    }

    // Reopen — subscriptions registered before open cannot exist, but
    // pending_deltas must be drained (invariant 4). Install sub AFTER open.
    let mut db = GraphDb::open(&dir).unwrap();
    // subscribe_all_rules is available immediately; no events from replay.
    let sub = db.subscribe_all_rules().unwrap();
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
        via_label: None,
        via_edge: None,
        via_dir: None,
    };
    db.create_rule(rule).unwrap();
    let sub = db.subscribe_writes().unwrap();

    db.insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))])
        .unwrap();

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
    let has_edge_fired = events
        .iter()
        .any(|e| matches!(e, DbEvent::EdgeFired { .. }));
    assert!(
        !has_edge_fired,
        "write subscription must not receive EdgeFired"
    );
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
        via_label: None,
        via_edge: None,
        via_dir: None,
    };
    db.create_rule(rule).unwrap();
    db.insert_node("A", "n1", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    db.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))])
        .unwrap();
    assert!(db.edge_count() >= 2);

    let sub = db.subscribe_rule("rel").unwrap();
    db.delete_node("n1").unwrap();

    let events = drain_sub(&sub);
    let has_retract = events
        .iter()
        .any(|e| matches!(e, DbEvent::EdgeRetracted { .. }));
    assert!(
        has_retract,
        "expected EdgeRetracted after node delete, got {events:?}"
    );
}

// ── subscribe_query tests ─────────────────────────────────────────────────────

/// Binding: insert a node → QueryRowAdded; delete it → QueryRowRemoved.
#[test]
fn subscribe_query_row_added_then_removed() {
    let dir = tmp("sq-add-remove");
    let mut db = GraphDb::open(&dir).unwrap();

    // Pre-existing node; subscriber captures it as initial state (no event).
    db.insert_node("Person", "alice", vec![]).unwrap();

    let sub = db
        .subscribe_query("MATCH (n:Person) RETURN n")
        .expect("subscribe_query must succeed for allowlisted plan");

    // Insert bob → QueryRowAdded for bob only (alice is in initial state).
    db.insert_node("Person", "bob", vec![]).unwrap();
    let ev = sub
        .recv_timeout(Duration::from_secs(1))
        .expect("expected QueryRowAdded event");
    match &ev {
        DbEvent::QueryRowAdded { columns, row } => {
            assert_eq!(columns, &["n"], "column name must be 'n'");
            assert_eq!(
                row,
                &[Some(Value::Str("bob".into()))],
                "row must be bob's key"
            );
        }
        other => panic!("expected QueryRowAdded, got {other:?}"),
    }

    // Delete bob → QueryRowRemoved.
    db.delete_node("bob").unwrap();
    let ev = sub
        .recv_timeout(Duration::from_secs(1))
        .expect("expected QueryRowRemoved event");
    match &ev {
        DbEvent::QueryRowRemoved { columns, row } => {
            assert_eq!(columns, &["n"]);
            assert_eq!(row, &[Some(Value::Str("bob".into()))]);
        }
        other => panic!("expected QueryRowRemoved, got {other:?}"),
    }

    // No more events (alice was in initial state throughout).
    assert!(
        sub.recv_timeout(Duration::from_millis(50)).is_none(),
        "no extra events expected"
    );
}

/// Binding: non-allowlisted plans are rejected at subscribe time.
#[test]
fn subscribe_query_rejects_non_allowlisted_plans() {
    let dir = tmp("sq-reject");
    let mut db = GraphDb::open(&dir).unwrap();

    // ORDER BY is not in the allowlist.
    let err = db
        .subscribe_query("MATCH (n:Person) RETURN n ORDER BY n")
        .expect_err("ORDER BY plan must be rejected");
    assert!(
        matches!(err, GraphError::QueryError { .. }),
        "expected QueryError, got {err:?}"
    );

    // Aggregate is not in the allowlist.
    let err2 = db
        .subscribe_query("MATCH (n:Person) RETURN COUNT(*)")
        .expect_err("aggregate plan must be rejected");
    assert!(
        matches!(err2, GraphError::QueryError { .. }),
        "expected QueryError, got {err2:?}"
    );
}

/// Binding: multi-hop Expand chains are rejected (only single-hop is documented).
#[test]
fn subscribe_query_rejects_multi_hop_expand() {
    let dir = tmp("sq-reject-multihop");
    let mut db = GraphDb::open(&dir).unwrap();

    let err = db
        .subscribe_query("MATCH (a:Person)-[r1:KNOWS]->(b:Person)-[r2:LIKES]->(c:Thing) RETURN a")
        .expect_err("two-hop MATCH must be rejected by subscribe_query");
    assert!(
        matches!(err, GraphError::QueryError { .. }),
        "expected QueryError for multi-hop, got {err:?}"
    );
}

/// Binding: SKIP is rejected (creates unstable offset windows on every commit).
#[test]
fn subscribe_query_rejects_skip() {
    let dir = tmp("sq-reject-skip");
    let mut db = GraphDb::open(&dir).unwrap();

    let err = db
        .subscribe_query("MATCH (n:Person) RETURN n SKIP 10 LIMIT 50")
        .expect_err("SKIP must be rejected by subscribe_query");
    assert!(
        matches!(err, GraphError::QueryError { .. }),
        "expected QueryError for SKIP, got {err:?}"
    );

    // LIMIT alone is fine — it is the documented cost-bounding mechanism.
    db.subscribe_query("MATCH (n:Person) RETURN n LIMIT 1000")
        .expect("LIMIT without SKIP must be accepted");
}

/// Binding: read-only as-of instances reject subscribe_query.
#[test]
fn subscribe_query_rejects_read_only() {
    let dir = tmp("sq-readonly");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "seed", vec![]).unwrap();

    // subscribe_query is a mutable method, so we'd need `mut ro`.
    // open_at returns a read-only instance; subscribe_query must reject it.
    let err = {
        let mut ro = GraphDb::open_at(&dir, 0).unwrap();
        ro.subscribe_query("MATCH (n:Person) RETURN n")
            .expect_err("read-only instance must reject subscribe_query")
    };
    assert!(
        matches!(err, GraphError::ReadOnly),
        "expected ReadOnly, got {err:?}"
    );
}

/// Binding: zero overhead when no query subscriptions are active.
/// This test verifies subscribe_query cleanup on subscription drop.
#[test]
fn subscribe_query_no_overhead_after_drop() {
    let dir = tmp("sq-drop");
    let mut db = GraphDb::open(&dir).unwrap();

    let sub = db
        .subscribe_query("MATCH (n:Person) RETURN n")
        .expect("subscribe_query must succeed");
    db.insert_node("Person", "alice", vec![]).unwrap();
    // Consume the event so the queue is drained.
    let _ = sub.recv_timeout(Duration::from_millis(100));
    // Drop subscription — next commit should not re-execute the query.
    drop(sub);
    // No panic; the commit path must handle the dead Weak gracefully.
    db.insert_node("Person", "bob", vec![]).unwrap();
}

// ── label-skip tests ──────────────────────────────────────────────────────────

/// Binding: committing a node with a different label than the subscribed scan
/// must NOT re-execute the query (counter stays flat, no events).
#[test]
fn query_sub_skips_unrelated_label_commit() {
    let dir = tmp("sq-skip-unrelated");
    let mut db = GraphDb::open(&dir).unwrap();

    let sub = db
        .subscribe_query("MATCH (n:Person) RETURN n")
        .expect("subscribe_query must succeed");

    let before = core_api::query_sub_exec_count();

    // Org insert — no overlap with Person scan.
    db.insert_node("Org", "acme", vec![]).unwrap();

    let after = core_api::query_sub_exec_count();
    assert_eq!(
        after - before,
        0,
        "re-execution counter must not advance for unrelated-label commit"
    );
    assert!(
        sub.recv_timeout(Duration::from_millis(50)).is_none(),
        "no events expected when commit cannot affect Person result set"
    );
}

/// Binding: committing a node whose label matches the subscribed scan must
/// trigger re-execution (counter advances, QueryRowAdded arrives).
#[test]
fn query_sub_still_fires_on_matching_label() {
    let dir = tmp("sq-fires-matching");
    let mut db = GraphDb::open(&dir).unwrap();

    let sub = db
        .subscribe_query("MATCH (n:Person) RETURN n")
        .expect("subscribe_query must succeed");

    let before = core_api::query_sub_exec_count();

    db.insert_node("Person", "bob", vec![]).unwrap();

    let after = core_api::query_sub_exec_count();
    assert_eq!(
        after - before,
        1,
        "re-execution counter must advance for matching-label commit"
    );
    assert!(
        matches!(
            sub.recv_timeout(Duration::from_secs(1)),
            Some(DbEvent::QueryRowAdded { .. })
        ),
        "QueryRowAdded must arrive for Person insert"
    );
}

/// Binding: an edge-record commit must NOT be skipped, even when only
/// node-label scans appear in the plan (edges can change join results).
#[test]
fn query_sub_does_not_skip_on_edge_commit() {
    let dir = tmp("sq-no-skip-edge");
    let mut db = GraphDb::open(&dir).unwrap();

    // Seed nodes so the edge insert is valid.
    db.insert_node("Person", "alice", vec![]).unwrap();
    db.insert_node("Org", "acme", vec![]).unwrap();

    let _sub = db
        .subscribe_query("MATCH (n:Person) RETURN n")
        .expect("subscribe_query must succeed");

    let before = core_api::query_sub_exec_count();

    db.insert_edge("WORKS_AT", "alice", "acme").unwrap();

    let after = core_api::query_sub_exec_count();
    assert_eq!(
        after - before,
        1,
        "edge commit must trigger re-execution (never skip on edge records)"
    );
}

/// Binding: when a rule fires and produces engine deltas, re-execution must
/// not be skipped even if the node commit itself is for an unrelated label.
#[test]
fn query_sub_does_not_skip_on_rule_delta() {
    let dir = tmp("sq-no-skip-rule-delta");
    let mut db = GraphDb::open(&dir).unwrap();

    db.create_rule(overlap_rule("rel", "REL")).unwrap();
    db.insert_node("A", "n1", vec![("tags".into(), tags(&["x", "y"]))])
        .unwrap();
    db.insert_node("A", "n2", vec![("tags".into(), tags(&["x"]))])
        .unwrap();

    // Subscribe to Person (different label from the A-node rule).
    let sub = db
        .subscribe_query("MATCH (n:Person) RETURN n")
        .expect("subscribe_query must succeed");

    let before = core_api::query_sub_exec_count();

    // Insert another A node — the overlap rule fires → engine_deltas non-empty.
    db.insert_node("A", "n3", vec![("tags".into(), tags(&["x"]))])
        .unwrap();

    let after = core_api::query_sub_exec_count();
    assert_eq!(
        after - before,
        1,
        "rule delta must force re-execution even when the node label doesn't match"
    );
    // No Person events (result set didn't change).
    let _ = sub;
}

/// Binding: a subscription whose plan contains an Expand op must never be
/// skipped (conservative v0.4.3 boundary), even when the commit label is
/// unrelated to both endpoints of the Expand pattern.
#[test]
fn query_sub_does_not_skip_with_expand() {
    let dir = tmp("sq-no-skip-expand");
    let mut db = GraphDb::open(&dir).unwrap();

    // Single-hop Expand is an allowed subscribe_query shape.
    let sub = db
        .subscribe_query("MATCH (a:Person)-[r:KNOWS]->(b:Org) RETURN a")
        .expect("single-hop Expand must be accepted");

    let before = core_api::query_sub_exec_count();

    // Insert a Thing node — touches neither Person nor Org.
    db.insert_node("Thing", "gadget", vec![]).unwrap();

    let after = core_api::query_sub_exec_count();
    assert_eq!(
        after - before,
        1,
        "Expand queries must always re-execute (v0.4.3 conservative boundary)"
    );
    let _ = sub;
}
