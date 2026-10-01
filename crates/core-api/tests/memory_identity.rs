//! Identity: every entity carries a normalised alias list, so a `SAME_AS`
//! rule has something to compare that both sides wrote the same way.
use core_api::memory::identity::{derive_aliases, ALIASES_FIELD, MAX_ALIASES};
use core_api::memory::remember::{
    describe_entity, describe_entity_with_aliases, remember, EntityIn, RememberInput,
};
use core_api::memory_schema::memory_defaults;
use core_api::{GraphDb, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mdb-memid-{name}-{}-{nanos}", std::process::id()))
}

fn store(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    db
}

fn aliases_of(db: &GraphDb<core_storage::fs::RealFs>, key: &str) -> Vec<String> {
    match db.get_prop(key, ALIASES_FIELD) {
        Some(Value::List(items)) => items
            .into_iter()
            .map(|v| match v {
                Value::Str(s) => s,
                other => panic!("non-string alias {other:?}"),
            })
            .collect(),
        other => panic!("{key} has no alias list: {other:?}"),
    }
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| (*s).to_string()).collect()
}

fn note<'a>(text: &'a str, about: &'a [String], entities: &'a [EntityIn]) -> RememberInput<'a> {
    RememberInput {
        text,
        about,
        kind: "note",
        ts: 1_759_000_000,
        source: None,
        entities,
        facts: &[],
    }
}

fn person(key: &str, name: &str, aliases: &[&str]) -> EntityIn {
    let mut props = BTreeMap::new();
    props.insert("name".to_string(), Value::Str(name.into()));
    EntityIn {
        key: key.into(),
        label: "Person".into(),
        props,
        aliases: strings(aliases),
    }
}

#[test]
fn an_entity_carries_its_key_its_name_its_words_and_the_callers_aliases() {
    let mut db = store("entity");
    let entities = vec![person("matthew-sherlin", "Matthew Sherlin", &["Matt"])];
    remember(&mut db, &note("Matthew owns 0.7", &[], &entities)).unwrap();
    assert_eq!(
        aliases_of(&db, "matthew-sherlin"),
        strings(&[
            "matt",
            "matthew",
            "matthew sherlin",
            "matthew-sherlin",
            "sherlin"
        ])
    );
}

#[test]
fn a_provisional_stub_carries_the_aliases_its_key_implies() {
    let mut db = store("stub");
    let about = strings(&["Matthew_Sherlin"]);
    remember(&mut db, &note("named before described", &about, &[])).unwrap();
    assert_eq!(
        aliases_of(&db, "Matthew_Sherlin"),
        strings(&["matthew", "matthew sherlin", "matthew_sherlin", "sherlin"])
    );
}

/// Review focus: case, punctuation and non-ASCII letters fold the same way on
/// every node, and the order is byte order, so it is the same on every
/// machine.
#[test]
fn normalisation_folds_case_punctuation_and_unicode_letters() {
    assert_eq!(
        derive_aliases("ÉMILE-Zola", Some("Émile  ZOLA"), &strings(&["É. Zola"])),
        strings(&["zola", "é zola", "émile", "émile zola", "émile-zola"])
    );
    assert_eq!(
        derive_aliases("McDonald", Some("McDONALD"), &[]),
        strings(&["mcdonald"]),
        "a one-word name adds no separate words, and the key folds into it"
    );
    assert_eq!(
        derive_aliases("x", Some("!!!"), &strings(&["", "  "])),
        strings(&["x"]),
        "a name or alias with no letters contributes nothing"
    );
}

#[test]
fn describing_with_the_same_inputs_leaves_the_list_alone() {
    let mut db = store("idem");
    let props = vec![("name".to_string(), Value::Str("Reid Hoffman".into()))];
    describe_entity(&mut db, "reid", Some("Person"), &props).unwrap();
    let first = aliases_of(&db, "reid");
    let before = db.stats();
    describe_entity_with_aliases(&mut db, "reid", None, &[], &[]).unwrap();
    assert_eq!(aliases_of(&db, "reid"), first);
    assert_eq!(
        db.stats().nodes_live,
        before.nodes_live,
        "describing again must not create anything"
    );
    assert_eq!(
        core_api::memory::identity::aliases_after_write(&db, "reid", &[], &[]).unwrap(),
        None,
        "nothing to rewrite"
    );
}

#[test]
fn a_former_name_stays_an_alias() {
    let mut db = store("rename");
    describe_entity(
        &mut db,
        "matthew",
        Some("Person"),
        &[("name".to_string(), Value::Str("Matthew Sherlin".into()))],
    )
    .unwrap();
    describe_entity(
        &mut db,
        "matthew",
        None,
        &[("name".to_string(), Value::Str("Matt Sherlin".into()))],
    )
    .unwrap();
    let aliases = aliases_of(&db, "matthew");
    for want in ["matthew sherlin", "matt sherlin", "matt", "matthew"] {
        assert!(
            aliases.contains(&want.to_string()),
            "{want} missing: {aliases:?}"
        );
    }
}

#[test]
fn an_aliases_property_is_refused_with_the_argument_named() {
    let mut db = store("prop");
    let err = describe_entity(
        &mut db,
        "matthew",
        Some("Person"),
        &[(
            ALIASES_FIELD.to_string(),
            Value::List(vec![Value::Str("Matt".into())]),
        )],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("maintained by the store"), "{err}");
    assert!(!db.has_node("matthew"), "a refusal writes nothing");
}

#[test]
fn more_aliases_than_the_cap_is_refused_before_anything_is_written() {
    let mut db = store("cap");
    let many: Vec<String> = (0..=MAX_ALIASES).map(|i| format!("alias {i}")).collect();
    let entities = vec![EntityIn {
        key: "hoarder".into(),
        label: "Person".into(),
        props: BTreeMap::new(),
        aliases: many,
    }];
    let before = db.stats().nodes_live;
    let err = remember(&mut db, &note("too many names", &[], &entities))
        .unwrap_err()
        .to_string();
    assert!(err.contains("hoarder") && err.contains("aliases"), "{err}");
    assert_eq!(
        db.stats().nodes_live,
        before,
        "neither the note nor the entity"
    );
}
