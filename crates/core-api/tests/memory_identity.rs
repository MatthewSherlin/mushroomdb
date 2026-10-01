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

/// A `remember` refused for its aliases declares nothing either: the alias
/// check runs before the full-text self-declare for an unseen label.
#[test]
fn a_remember_refused_for_aliases_declares_no_fulltext() {
    let mut db = store("refused-declares-nothing");
    let before = db.fulltext_pairs();
    let many: Vec<String> = (0..=MAX_ALIASES).map(|i| format!("alias {i}")).collect();
    let entities = vec![EntityIn {
        key: "v0.7".into(),
        label: "Release".into(),
        props: BTreeMap::new(),
        aliases: many,
    }];
    let err = remember(&mut db, &note("too many names", &[], &entities))
        .unwrap_err()
        .to_string();
    assert!(err.contains("aliases"), "{err}");
    assert_eq!(
        db.fulltext_pairs(),
        before,
        "a refused call must not declare (Release, name)"
    );
}

// ── the identity preset ─────────────────────────────────────────────────────

use core_api::memory::identity::{SameAsPair, SAME_AS_EDGE, SAME_AS_FLOOR};
use core_api::memory_schema::memory_identity;
use core_api::Predicate;

fn identity_store(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = store(name);
    db.apply_schema(&memory_identity()).unwrap();
    db
}

fn remember_people(
    db: &mut GraphDb<core_storage::fs::RealFs>,
    text: &str,
    people: Vec<EntityIn>,
) -> core_api::memory::remember::RememberReport {
    remember(db, &note(text, &[], &people)).unwrap()
}

#[test]
fn the_preset_is_eleven_global_rules_over_aliases() {
    let s = memory_identity();
    assert_eq!(
        s.rules.len(),
        11,
        "{:?}",
        s.rules.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
    for r in &s.rules {
        assert_eq!(r.edge_type, SAME_AS_EDGE, "{}", r.name);
        assert_eq!(r.namespace, None, "{} must be global", r.name);
        assert_eq!(r.weight_prop.as_deref(), Some("weight"), "{}", r.name);
        assert_eq!(r.max_edges, Some(32), "{}", r.name);
        assert_eq!(
            r.predicate,
            Predicate::Overlap {
                field: ALIASES_FIELD.into(),
                min: SAME_AS_FLOOR
            },
            "{}",
            r.name
        );
    }
    let pairs: Vec<(&str, &str)> = s
        .rules
        .iter()
        .map(|r| (r.src_label.as_str(), r.dst_label.as_str()))
        .collect();
    for label in ["Person", "Org", "Project", "Concept", "Event", "Entity"] {
        assert!(
            pairs.contains(&(label, label)),
            "no same-label rule for {label}"
        );
    }
    for label in ["Person", "Org", "Project", "Concept", "Event"] {
        assert!(pairs.contains(&("Entity", label)), "no Entity→{label} rule");
    }
}

/// OD-2: the defaults a new store is created with stay rule-free.
#[test]
fn the_memory_defaults_still_create_no_rule() {
    assert!(memory_defaults().rules.is_empty());
}

#[test]
fn two_nodes_with_the_same_full_name_link_and_the_report_says_so() {
    let mut db = identity_store("fullname");
    remember_people(
        &mut db,
        "first",
        vec![person("matthew-sherlin", "Matthew Sherlin", &[])],
    );
    let report = remember_people(
        &mut db,
        "second",
        vec![person("msherlin", "Matthew Sherlin", &[])],
    );
    assert_eq!(
        report.same_as,
        vec![SameAsPair {
            a: "matthew-sherlin".into(),
            b: "msherlin".into(),
            score: 0.6
        }]
    );
}

/// Review focus: a same-label rule writes a→b and b→a. That is one claim.
#[test]
fn both_directions_of_a_same_label_link_are_one_claim() {
    let mut db = identity_store("directed");
    let report = remember_people(
        &mut db,
        "both at once",
        vec![
            person("matthew-sherlin", "Matthew Sherlin", &[]),
            person("msherlin", "Matthew Sherlin", &[]),
        ],
    );
    assert_eq!(
        db.weighted_edges(SAME_AS_EDGE, Some("weight")).len(),
        2,
        "the engine derives both directions"
    );
    assert_eq!(report.same_as.len(), 1, "{:?}", report.same_as);
}

#[test]
fn a_stub_links_to_the_entity_whose_name_it_spells() {
    let mut db = identity_store("stub-link");
    let about = strings(&["Matthew_Sherlin"]);
    remember(&mut db, &note("named first", &about, &[])).unwrap();
    let report = remember_people(
        &mut db,
        "described later",
        vec![person("matthew-sherlin", "Matthew Sherlin", &[])],
    );
    assert_eq!(
        report.same_as,
        vec![SameAsPair {
            a: "Matthew_Sherlin".into(),
            b: "matthew-sherlin".into(),
            score: 0.6
        }]
    );
}

/// Precision: one shared word is not an identity.
#[test]
fn people_who_share_a_first_name_do_not_link() {
    let mut db = identity_store("alexes");
    let report = remember_people(
        &mut db,
        "two alexes",
        vec![
            person("alex-chen", "Alex Chen", &[]),
            person("alex-kim", "Alex Kim", &[]),
            person("alex-1", "Alex", &[]),
        ],
    );
    assert!(report.same_as.is_empty(), "{:?}", report.same_as);
    let about = strings(&["alex"]);
    let report = remember(&mut db, &note("which alex?", &about, &[])).unwrap();
    assert!(
        report.same_as.is_empty(),
        "a bare first name scores 1/2 against alex-1 and must not link: {:?}",
        report.same_as
    );
}

#[test]
fn remembering_the_same_people_again_reports_nothing_new() {
    let mut db = identity_store("again");
    let people = || {
        vec![
            person("matthew-sherlin", "Matthew Sherlin", &[]),
            person("msherlin", "Matthew Sherlin", &[]),
        ]
    };
    assert_eq!(remember_people(&mut db, "once", people()).same_as.len(), 1);
    assert!(remember_people(&mut db, "twice", people())
        .same_as
        .is_empty());
}

/// Review focus: the preset's rules are global, so in a namespaced store a
/// link crosses namespaces — and is reported, not hidden.
#[test]
fn a_global_identity_rule_links_across_namespaces_and_says_so() {
    let mut db = identity_store("namespaces");
    let in_ns = |key: &str, ns: &str| {
        let mut e = person(key, "Matthew Sherlin", &[]);
        e.props.insert("ns".into(), Value::Str(ns.into()));
        e
    };
    let report = remember_people(
        &mut db,
        "two teams",
        vec![in_ns("matthew-a", "team-a"), in_ns("matthew-b", "team-b")],
    );
    assert_eq!(db.namespace_of("matthew-a").as_deref(), Some("team-a"));
    assert_eq!(db.namespace_of("matthew-b").as_deref(), Some("team-b"));
    assert_eq!(
        report.same_as.len(),
        1,
        "the global rule links across namespaces: {:?}",
        report.same_as
    );
}

/// The preset's real shape through the lock-free reader: eleven rules exist
/// before any node carries `aliases`.
#[test]
fn the_reader_survives_the_preset_on_an_empty_store() {
    let mut db = identity_store("reader");
    remember_people(
        &mut db,
        "after the preset",
        vec![person("matthew-sherlin", "Matthew Sherlin", &[])],
    );
    let rows = db
        .reader()
        .query("MATCH (n:Person) RETURN n.id", &Default::default())
        .map(|rs| rs.len())
        .map_err(|e| e.to_string());
    assert_eq!(rows, Ok(1));
}
