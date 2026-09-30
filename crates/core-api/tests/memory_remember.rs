//! remember: a fact may arrive before its subject, and the caller is told
//! what the store did with it.
use core_api::memory::remember::{describe_entity, remember, EntityIn, FactIn, RememberInput};
use core_api::memory_schema::{memory_defaults, NAME_FIELD, PROVISIONAL_PROP};
use core_api::{GraphDb, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mdb-memrem-{name}-{}-{nanos}", std::process::id()))
}

fn store(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    db
}

fn input<'a>(text: &'a str, about: &'a [String]) -> RememberInput<'a> {
    RememberInput {
        text,
        about,
        kind: "note",
        ts: 1_759_000_000,
        source: None,
        entities: &[],
        facts: &[],
    }
}

#[test]
fn an_unknown_subject_becomes_provisional_instead_of_an_error() {
    let mut db = store("provisional");
    let about = vec!["reid".to_string()];
    let report = remember(&mut db, &input("Reid reviewed the copy", &about)).expect("remember");
    assert_eq!(report.provisional, vec!["reid".to_string()]);
    assert_eq!(
        db.get_prop("reid", PROVISIONAL_PROP),
        Some(Value::Bool(true)),
        "the stub must be marked provisional"
    );
}

#[test]
fn describing_the_subject_later_clears_provisional() {
    let mut db = store("fillin");
    let about = vec!["reid".to_string()];
    remember(&mut db, &input("Reid reviewed the copy", &about)).unwrap();
    let created = describe_entity(
        &mut db,
        "reid",
        Some("Person"),
        &[("name".to_string(), Value::Str("Reid".into()))],
    )
    .unwrap();
    assert!(!created, "the provisional stub already existed");
    assert_eq!(
        db.get_prop("reid", PROVISIONAL_PROP),
        None,
        "a described subject is no longer provisional"
    );
}

#[test]
fn an_existing_subject_is_matched_not_created() {
    let mut db = store("matched");
    describe_entity(
        &mut db,
        "matthew",
        Some("Person"),
        &[("name".to_string(), Value::Str("Matthew".into()))],
    )
    .unwrap();
    let about = vec!["matthew".to_string()];
    let report = remember(&mut db, &input("Matthew prefers brevity", &about)).unwrap();
    assert!(report.provisional.is_empty(), "{report:?}");
    assert_eq!(report.matched, 1, "{report:?}");
}

#[test]
fn entities_and_facts_land_in_one_commit() {
    let mut db = store("structured");
    let before = db.stats().nodes_live;
    let entities = vec![
        EntityIn {
            key: "matthew".into(),
            label: "Person".into(),
            props: BTreeMap::new(),
        },
        EntityIn {
            key: "v0.7".into(),
            label: "Release".into(),
            props: BTreeMap::new(),
        },
    ];
    let facts = vec![FactIn {
        subject: "matthew".into(),
        predicate: "WANTS".into(),
        object: "v0.7".into(),
    }];
    let report = remember(
        &mut db,
        &RememberInput {
            text: "Matthew wants 0.7 to focus on the write path",
            about: &[],
            kind: "note",
            ts: 1_759_000_000,
            source: Some("session:abc"),
            entities: &entities,
            facts: &facts,
        },
    )
    .expect("remember");
    assert_eq!(report.created, 2, "{report:?}");
    // note + 2 entities
    assert_eq!(db.stats().nodes_live, before + 3);
    assert_eq!(
        db.get_prop(&report.note, "source"),
        Some(Value::Str("session:abc".into())),
        "caller-supplied provenance must be stored, not overwritten with \"agent\""
    );
}

/// Fix round 1: a `facts[]` endpoint named nowhere else used to land as a
/// bare, unmarked `Entity` node — no `name`, no `provisional`, absent from
/// every report field — because the facts loop auto-created it through
/// `insert_edge_upsert`, which stamps no props at all. It must get exactly
/// the `about` path's treatment: a `name`, `provisional: true`, a place in
/// `report.provisional`, and a hit from the store's own `provisional` query.
#[test]
fn an_unnamed_fact_endpoint_becomes_provisional_too() {
    let mut db = store("fact-endpoint");
    let entities = vec![EntityIn {
        key: "matthew".into(),
        label: "Person".into(),
        props: BTreeMap::new(),
    }];
    let facts = vec![FactIn {
        subject: "matthew".into(),
        predicate: "WANTS".into(),
        object: "v0.8-typoo".into(),
    }];
    let report = remember(
        &mut db,
        &RememberInput {
            text: "Matthew wants 0.8, typo and all",
            about: &[],
            kind: "note",
            ts: 1_759_000_001,
            source: None,
            entities: &entities,
            facts: &facts,
        },
    )
    .expect("remember");

    assert_eq!(
        report.provisional,
        vec!["v0.8-typoo".to_string()],
        "{report:?}"
    );
    assert_eq!(
        db.get_prop("v0.8-typoo", PROVISIONAL_PROP),
        Some(Value::Bool(true)),
        "an unnamed fact endpoint must be marked provisional"
    );
    assert_eq!(
        db.get_prop("v0.8-typoo", NAME_FIELD),
        Some(Value::Str("v0.8-typoo".into())),
        "an unnamed fact endpoint must carry a name"
    );

    // The same convention the `about` path and the first-run gate rely on:
    // a bare truthy predicate finds it.
    let rows = db
        .query(
            "MATCH (n) WHERE n.provisional RETURN n.id AS id",
            &BTreeMap::new(),
        )
        .expect("query");
    let ids: Vec<String> = (0..rows.len())
        .map(|i| match &rows.row(i)[0] {
            Some(Value::Str(s)) => s.clone(),
            other => panic!("expected a string id, got {other:?}"),
        })
        .collect();
    assert!(
        ids.contains(&"v0.8-typoo".to_string()),
        "the store's own provisional query must surface it: {ids:?}"
    );

    // `matthew` was named in `entities`: real, not stubbed, not reported.
    assert_eq!(
        db.get_prop("matthew", PROVISIONAL_PROP),
        None,
        "an entity explicitly described must never be marked provisional"
    );
}

#[test]
fn the_caller_owns_the_timestamp() {
    let mut db = store("ts");
    let report = remember(&mut db, &input("an imported fact", &[])).unwrap();
    assert_eq!(
        db.get_prop(&report.note, "ts"),
        Some(Value::Int(1_759_000_000)),
        "a fact imported from a transcript must not be stamped with import time"
    );
}
