use core_api::{
    AutoFk, GraphDb, GraphError, IngestOptions, MutationEvent, Predicate, RuleDef, Value,
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
        serde_json::json!({"NodeInserted": {"label": "A", "key": "k"}})
    );
    assert_eq!(
        serde_json::to_value(&MutationEvent::BatchApplied { ops: 3 }).unwrap(),
        serde_json::json!({"BatchApplied": {"ops": 3}})
    );
    assert_eq!(
        serde_json::to_value(&MutationEvent::Ingested {
            label: "Person".into(),
            inserted: 2,
        })
        .unwrap(),
        serde_json::json!({"Ingested": {"label": "Person", "inserted": 2}})
    );
}
