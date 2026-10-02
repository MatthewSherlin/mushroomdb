//! A rule created before anything carries its edge type must not break the
//! lock-free reader.
//!
//! `GraphDb::create_rule` logged its `CreateRule` record raw, while the batch
//! path pre-interns the rule's `edge_type` and logs an `Intern` record ahead
//! of it. The engine interns that edge type lazily during the backfill, so on
//! the standalone path the writer's interner gained a symbol no record named.
//! The reader replays records, skips `CreateRule`, and so fell one symbol
//! behind: the next `Intern` the writer logged — any new field, label or edge
//! type — failed with "mvcc delta intern mismatch", and every `reader()`
//! snapshot failed with it until the next fold, up to 63 commits later.
use core_api::{GraphDb, Predicate, RuleDef, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-ruleintern-{name}-{}-{nanos}",
        std::process::id()
    ))
}

fn same_as_rule(predicate: Predicate) -> RuleDef {
    RuleDef {
        name: "same_as_person".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate,
        edge_type: "SAME_AS".into(),
        weight_prop: Some("weight".into()),
        max_edges: Some(32),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

fn person_count_from_reader(db: &GraphDb<core_storage::fs::RealFs>) -> Result<usize, String> {
    db.reader()
        .query("MATCH (n:Person) RETURN n.id", &Default::default())
        .map(|rs| rs.len())
        .map_err(|e| e.to_string())
}

/// The minimal repro: one node, a rule on a field nothing has yet, then a
/// node that has it.
#[test]
fn a_rule_on_an_unseen_field_does_not_break_the_reader() {
    for (field, predicate, value) in [
        (
            "names",
            Predicate::Overlap {
                field: "names".into(),
                min: 0.6,
            },
            Value::List(vec![Value::Str("x".into())]),
        ),
        (
            "embedding",
            Predicate::VectorSimilar {
                field: "embedding".into(),
                min: 0.88,
            },
            Value::List(vec![Value::Float(1.0), Value::Float(0.0)]),
        ),
    ] {
        let mut db = GraphDb::open(&tmp(field)).unwrap();
        db.insert_node("Person", "a", vec![("name".into(), Value::Str("A".into()))])
            .unwrap();
        db.create_rule(same_as_rule(predicate)).unwrap();
        db.insert_node("Person", "v", vec![(field.into(), value)])
            .unwrap();

        let reader = db.reader();
        assert_eq!(
            reader.resolve_key("v"),
            Some(1),
            "the reader cannot see a node the writer just committed ({field})"
        );
        assert_eq!(
            person_count_from_reader(&db),
            Ok(2),
            "a reader query failed after a rule on {field}"
        );
    }
}

/// The derived edge's type is a symbol too: a reader must be able to name it
/// in a pattern, not only survive the next intern.
#[test]
fn a_reader_sees_the_edges_a_new_rule_derived() {
    let mut db = GraphDb::open(&tmp("derived")).unwrap();
    for key in ["a", "b"] {
        db.insert_node(
            "Person",
            key,
            vec![(
                "aliases".into(),
                Value::List(vec![Value::Str("matt".into())]),
            )],
        )
        .unwrap();
    }
    db.create_rule(same_as_rule(Predicate::Overlap {
        field: "aliases".into(),
        min: 0.6,
    }))
    .unwrap();
    let rows = db
        .reader()
        .query(
            "MATCH (a:Person)-[:SAME_AS]->(b:Person) RETURN a.id, b.id",
            &Default::default(),
        )
        .map(|rs| rs.len())
        .map_err(|e| e.to_string());
    assert_eq!(rows, Ok(2), "both directions of the derived SAME_AS");
}

/// The identity preset's real shape: every rule exists before any node has
/// the field it reads, on a store with nothing in it.
#[test]
fn rules_declared_on_an_empty_store_leave_the_reader_working() {
    let mut db = GraphDb::open(&tmp("empty")).unwrap();
    db.create_rule(same_as_rule(Predicate::Overlap {
        field: "aliases".into(),
        min: 0.6,
    }))
    .unwrap();
    db.insert_node(
        "Person",
        "matthew",
        vec![(
            "aliases".into(),
            Value::List(vec![Value::Str("matthew".into())]),
        )],
    )
    .unwrap();
    db.insert_node("Note", "n1", vec![("text".into(), Value::Str("hi".into()))])
        .unwrap();
    assert_eq!(person_count_from_reader(&db), Ok(1));
}

/// The frame `create_rule` now writes — `Intern` ahead of `CreateRule` —
/// replays to the same store: the rule, its edges and a working reader.
#[test]
fn a_rule_created_alone_replays_after_reopen() {
    let dir = tmp("reopen");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.create_rule(same_as_rule(Predicate::Overlap {
            field: "aliases".into(),
            min: 0.6,
        }))
        .unwrap();
        for key in ["a", "b"] {
            db.insert_node(
                "Person",
                key,
                vec![(
                    "aliases".into(),
                    Value::List(vec![Value::Str("matt".into())]),
                )],
            )
            .unwrap();
        }
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 1);
    assert_eq!(db.weighted_edges("SAME_AS", Some("weight")).len(), 2);
    assert_eq!(person_count_from_reader(&db), Ok(2));
}
