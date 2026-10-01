//! The entity upsert's policy, at the layer every surface calls: `id` never
//! comes from `props`, a create needs a label, an update never changes one,
//! and a namespace a node is already in is not an update.
use core_api::memory::remember::{remember, upsert_entity, RememberInput};
use core_api::memory_schema::memory_defaults;
use core_api::{GraphDb, GraphError, Value};
use std::collections::BTreeMap;

type Db = GraphDb<core_storage::fs::RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-memupsert-{name}-{}-{nanos}",
        std::process::id()
    ))
}

fn store(name: &str) -> Db {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    db
}

fn row(pairs: &[(&str, &str)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), Value::Str((*v).to_string())))
        .collect()
}

#[test]
fn a_create_needs_a_label_and_takes_its_id_from_the_key() {
    let mut db = store("create");

    let no_label = upsert_entity(&mut db, "ada", None, row(&[("name", "Ada")]), &[]);
    match no_label {
        Err(GraphError::IngestError { detail }) => {
            assert_eq!(detail, "label required when creating a new entity")
        }
        other => panic!("expected the label refusal, got {other:?}"),
    }
    assert!(!db.has_node("ada"), "refused before anything was written");

    let made = upsert_entity(
        &mut db,
        "ada",
        Some("Person"),
        row(&[("name", "Ada Lovelace"), ("id", "not-ada")]),
        &[],
    )
    .unwrap();
    assert!(made.created);
    assert_eq!(made.label, "Person");
    assert_eq!(made.updated_fields, 0);
    assert!(made.same_as_lost.is_empty());
    assert_eq!(
        db.get_prop("ada", "id"),
        Some(Value::Str("ada".into())),
        "`id` repeats the key, whatever `props` said"
    );
}

#[test]
fn an_update_never_changes_a_label_and_writes_nothing_when_refused() {
    let mut db = store("relabel");
    // An unknown `about` key is stubbed provisional, label `Entity`.
    let about = vec!["matthew".to_string()];
    remember(
        &mut db,
        &RememberInput {
            text: "Matthew reviewed it",
            about: &about,
            kind: "note",
            ts: 1_759_000_000,
            source: None,
            entities: &[],
            facts: &[],
        },
    )
    .unwrap();

    let refused = upsert_entity(
        &mut db,
        "matthew",
        Some("Person"),
        row(&[("name", "Matthew Sherlin")]),
        &[],
    );
    match refused {
        Err(GraphError::IngestError { detail }) => assert_eq!(
            detail,
            "'matthew' exists as \"Entity\", not \"Person\"; this release cannot relabel a \
             node. Pass the label in remember's 'entities' when you know it (at first \
             mention, before it goes provisional), or use a different key."
        ),
        other => panic!("expected the relabel refusal, got {other:?}"),
    }
    assert_eq!(
        db.get_prop("matthew", "name"),
        Some(Value::Str("matthew".into())),
        "all-or-nothing: `name` is still the stub's own key"
    );

    // Naming the label it has, or none, is an ordinary update.
    let same = upsert_entity(
        &mut db,
        "matthew",
        Some("Entity"),
        row(&[("name", "Matthew Sherlin")]),
        &[],
    )
    .unwrap();
    assert!(!same.created);
    assert_eq!(same.label, "Entity");
    assert_eq!(same.updated_fields, 1);
    assert_eq!(
        db.get_prop("matthew", "provisional"),
        None,
        "described now, so no longer provisional"
    );
}

#[test]
fn the_namespace_a_node_is_already_in_is_not_an_updated_field() {
    let mut db = store("ns");
    upsert_entity(
        &mut db,
        "doc-1",
        Some("Concept"),
        row(&[("name", "Doc One"), ("ns", "team-a")]),
        &[],
    )
    .unwrap();
    assert_eq!(db.namespace_of("doc-1").as_deref(), Some("team-a"));

    let again = upsert_entity(
        &mut db,
        "doc-1",
        None,
        row(&[("ns", "team-a"), ("summary", "the first one")]),
        &[],
    )
    .unwrap();
    assert_eq!(
        again.updated_fields, 1,
        "`summary` only: the same namespace writes no record"
    );

    let moved = upsert_entity(&mut db, "doc-1", None, row(&[("ns", "team-b")]), &[]);
    assert!(
        matches!(moved, Err(GraphError::NamespaceImmutable { .. })),
        "a different namespace is the engine's refusal: {moved:?}"
    );
}
