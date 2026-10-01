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

// ── an `aliases` property a 0.6 store already carries ──────────────────────

/// A node as a 0.6 store holds it: written raw, with its own `aliases`.
fn upgraded_node(name: &str, key: &str, aliases: Value) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = store(name);
    db.insert_node(
        "Person",
        key,
        vec![
            ("name".to_string(), Value::Str("Matthew Sherlin".into())),
            (ALIASES_FIELD.to_string(), aliases),
        ],
    )
    .unwrap();
    db
}

fn set_role(db: &mut GraphDb<core_storage::fs::RealFs>, key: &str) -> core_api::Result<()> {
    describe_entity(
        db,
        key,
        None,
        &[("role".to_string(), Value::Str("owner".into()))],
    )
    .map(|_| ())
}

#[test]
fn an_existing_string_alias_is_kept_in_canonical_form() {
    let mut db = upgraded_node("str-alias", "matthew-sherlin", Value::Str("Matt S".into()));
    set_role(&mut db, "matthew-sherlin").unwrap();
    assert_eq!(
        aliases_of(&db, "matthew-sherlin"),
        strings(&[
            "matt s",
            "matthew",
            "matthew sherlin",
            "matthew-sherlin",
            "sherlin"
        ]),
        "the caller's string must survive as one alias"
    );
}

#[test]
fn an_existing_mixed_case_list_is_normalised() {
    let mut db = upgraded_node(
        "list-alias",
        "matthew-sherlin",
        Value::List(vec![
            Value::Str("Countess".into()),
            Value::Str("Ada".into()),
        ]),
    );
    set_role(&mut db, "matthew-sherlin").unwrap();
    let aliases = aliases_of(&db, "matthew-sherlin");
    for want in ["countess", "ada"] {
        assert!(aliases.contains(&want.to_string()), "{want}: {aliases:?}");
    }
    for gone in ["Countess", "Ada"] {
        assert!(!aliases.contains(&gone.to_string()), "{gone}: {aliases:?}");
    }
}

#[test]
fn an_existing_aliases_of_another_type_is_refused_and_nothing_is_written() {
    let mut db = upgraded_node("int-alias", "matthew-sherlin", Value::Int(3));
    let err = set_role(&mut db, "matthew-sherlin")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("matthew-sherlin") && err.contains("forget") && err.contains("aliases"),
        "{err}"
    );
    assert_eq!(
        db.get_prop("matthew-sherlin", "role"),
        None,
        "nothing written"
    );
    assert_eq!(
        db.get_prop("matthew-sherlin", ALIASES_FIELD),
        Some(Value::Int(3)),
        "the value is not overwritten"
    );
}

/// The lowercased key is the one item the store writes that is not in
/// canonical form; folding it would rewrite every list on the next write.
#[test]
fn a_list_the_store_wrote_is_left_byte_identical() {
    let mut db = store("store-written");
    describe_entity(
        &mut db,
        "Matthew-Sherlin",
        Some("Person"),
        &[("name".to_string(), Value::Str("Matthew Sherlin".into()))],
    )
    .unwrap();
    let first = aliases_of(&db, "Matthew-Sherlin");
    assert!(first.contains(&"matthew-sherlin".to_string()), "{first:?}");
    assert_eq!(
        core_api::memory::identity::aliases_after_write(&db, "Matthew-Sherlin", &[], &[]).unwrap(),
        None,
        "nothing to rewrite"
    );
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

use core_api::memory::identity::{
    same_as_pairs, SameAsPair, ALIAS_KEYS_FIELD, MAX_ALIAS_KEYS, SAME_AS_EDGE, SAME_AS_FLOOR,
};
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
fn the_preset_is_sixteen_global_rules_eleven_over_aliases_and_five_over_alias_keys() {
    let s = memory_identity();
    let shape: Vec<(&str, &str, &str, &Predicate, Option<u64>)> = s
        .rules
        .iter()
        .map(|r| {
            (
                r.name.as_str(),
                r.src_label.as_str(),
                r.dst_label.as_str(),
                &r.predicate,
                r.max_edges,
            )
        })
        .collect();
    let overlap = Predicate::Overlap {
        field: ALIASES_FIELD.into(),
        min: SAME_AS_FLOOR,
    };
    let claim = Predicate::KeyMatch {
        field: ALIAS_KEYS_FIELD.into(),
    };
    assert_eq!(
        shape,
        vec![
            ("same_as_person", "Person", "Person", &overlap, Some(32)),
            ("same_as_org", "Org", "Org", &overlap, Some(32)),
            ("same_as_project", "Project", "Project", &overlap, Some(32)),
            ("same_as_concept", "Concept", "Concept", &overlap, Some(32)),
            ("same_as_event", "Event", "Event", &overlap, Some(32)),
            ("same_as_entity", "Entity", "Entity", &overlap, Some(32)),
            (
                "same_as_entity_person",
                "Entity",
                "Person",
                &overlap,
                Some(32)
            ),
            ("same_as_entity_org", "Entity", "Org", &overlap, Some(32)),
            (
                "same_as_entity_project",
                "Entity",
                "Project",
                &overlap,
                Some(32)
            ),
            (
                "same_as_entity_concept",
                "Entity",
                "Concept",
                &overlap,
                Some(32)
            ),
            (
                "same_as_entity_event",
                "Entity",
                "Event",
                &overlap,
                Some(32)
            ),
            (
                "same_as_claim_person",
                "Person",
                "Entity",
                &claim,
                Some(512)
            ),
            ("same_as_claim_org", "Org", "Entity", &claim, Some(512)),
            (
                "same_as_claim_project",
                "Project",
                "Entity",
                &claim,
                Some(512)
            ),
            (
                "same_as_claim_concept",
                "Concept",
                "Entity",
                &claim,
                Some(512)
            ),
            ("same_as_claim_event", "Event", "Entity", &claim, Some(512)),
        ]
    );
    for r in &s.rules {
        assert_eq!(r.edge_type, SAME_AS_EDGE, "{}", r.name);
        assert_eq!(r.namespace, None, "{} must be global", r.name);
        assert_eq!(r.weight_prop.as_deref(), Some("weight"), "{}", r.name);
        assert!(!r.approximate, "{}", r.name);
        assert_eq!(
            (&r.via_label, &r.via_edge, &r.via_dir),
            (&None, &None, &None),
            "{}",
            r.name
        );
        assert!(
            !matches!(r.predicate, Predicate::Any(_)),
            "{}: a KeyMatch under Any loses the FK fast path",
            r.name
        );
    }
    for r in s.rules.iter().filter(|r| r.name.contains("claim")) {
        assert!(
            core_api::is_keymatch_rooted(&r.predicate),
            "{} must take the FK fast path",
            r.name
        );
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

/// The preset's real shape through the lock-free reader: sixteen rules exist
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

// ── resolution: complete linkage ────────────────────────────────────────────

use core_api::memory::identity::identity_clusters;

/// A node with no name whose alias set is its key plus `words`.
fn tagged(key: &str, words: std::ops::RangeInclusive<u32>) -> EntityIn {
    EntityIn {
        key: key.into(),
        label: "Person".into(),
        props: BTreeMap::new(),
        aliases: words.map(|w| format!("t{w}")).collect(),
    }
}

fn clusters_of(db: &GraphDb<core_storage::fs::RealFs>) -> Vec<Vec<String>> {
    identity_clusters(db, SAME_AS_FLOOR)
        .clusters
        .into_iter()
        .map(|c| c.members)
        .collect()
}

/// c0~c1 at 9/13 and c1~c2 at 9/13, but c0~c2 at 8/14 is below the floor.
/// Closure would make one person of three; complete linkage does not.
#[test]
fn a_chain_of_two_claims_is_not_one_identity() {
    let mut db = identity_store("chain");
    remember_people(
        &mut db,
        "chain",
        vec![
            tagged("c0", 1..=10),
            tagged("c1", 2..=11),
            tagged("c2", 3..=12),
        ],
    );
    assert_eq!(
        identity_clusters(&db, SAME_AS_FLOOR).claims,
        2,
        "the fixture must hold exactly the two adjacent claims"
    );
    assert_eq!(clusters_of(&db), vec![strings(&["c0", "c1"])]);
}

/// The same chain with its middle node oldest: the seed c1 admits c0, then
/// must refuse c2 because c0~c2 is 8/14. Admitting every neighbour of the
/// seed would answer all three.
#[test]
fn a_candidate_must_link_to_every_member_not_just_the_seed() {
    let mut db = identity_store("hub-first");
    remember_people(
        &mut db,
        "hub-first",
        vec![
            tagged("c1", 2..=11),
            tagged("c0", 1..=10),
            tagged("c2", 3..=12),
        ],
    );
    assert_eq!(identity_clusters(&db, SAME_AS_FLOOR).claims, 2);
    assert_eq!(clusters_of(&db), vec![strings(&["c1", "c0"])]);
}

/// Locality: a hundred unrelated linked pairs added after the fact do not move
/// an existing identity. Modularity clustering fails exactly this.
#[test]
fn unrelated_identities_cannot_move_an_existing_one() {
    let mut db = identity_store("local");
    remember_people(
        &mut db,
        "chain",
        vec![
            tagged("c0", 1..=10),
            tagged("c1", 2..=11),
            tagged("c2", 3..=12),
        ],
    );
    let before = clusters_of(&db);
    for i in 0..100 {
        let name = format!("Other Person{i}");
        remember_people(
            &mut db,
            &format!("pair {i}"),
            vec![
                person(&format!("x{i}"), &name, &[]),
                person(&format!("y{i}"), &name, &[]),
            ],
        );
    }
    let after = clusters_of(&db);
    assert_eq!(after.len(), 101, "a hundred pairs plus the chain's one");
    assert!(
        after.contains(&before[0]),
        "the chain's identity moved: {after:?}"
    );
}

/// The canonical is the oldest live node, whatever its key sorts as, and it
/// survives a rename — the id does.
#[test]
fn the_canonical_is_the_oldest_live_node() {
    let mut db = identity_store("canonical");
    remember_people(&mut db, "first", vec![person("zed", "Zed Shaw", &[])]);
    remember_people(&mut db, "second", vec![person("abe", "Zed Shaw", &[])]);
    let report = identity_clusters(&db, SAME_AS_FLOOR);
    assert_eq!(
        report.clusters[0].canonical, "zed",
        "oldest, not first by key"
    );
    assert_eq!(report.clusters[0].members, strings(&["zed", "abe"]));
    // {zed, zed shaw, shaw} against {abe, zed shaw, zed, shaw}: 3/4.
    assert!((report.clusters[0].weakest - 0.75).abs() < 1e-12);

    db.rename_node("zed", "zed-shaw").unwrap();
    assert_eq!(
        identity_clusters(&db, SAME_AS_FLOOR).clusters[0].canonical,
        "zed-shaw"
    );

    remember_people(&mut db, "third", vec![person("zs", "Zed Shaw", &[])]);
    db.delete_node("zed-shaw").unwrap();
    let report = identity_clusters(&db, SAME_AS_FLOOR);
    assert_eq!(
        report.clusters[0].canonical, "abe",
        "the next oldest live node"
    );
    assert_eq!(report.clusters[0].members, strings(&["abe", "zs"]));
}

/// A provisional stub `remember` made for an `about` key is a node like any
/// other: named first, it is the oldest, so it is the canonical.
#[test]
fn a_stub_named_first_is_the_canonical_of_its_identity() {
    let mut db = identity_store("stub-canonical");
    let about = strings(&["Matthew_Sherlin"]);
    remember(&mut db, &note("named first", &about, &[])).unwrap();
    remember_people(
        &mut db,
        "described later",
        vec![person("matthew-sherlin", "Matthew Sherlin", &[])],
    );
    let report = identity_clusters(&db, SAME_AS_FLOOR);
    assert_eq!(report.clusters.len(), 1, "{report:?}");
    assert_eq!(report.clusters[0].canonical, "Matthew_Sherlin");
    assert_eq!(
        report.clusters[0].members,
        strings(&["Matthew_Sherlin", "matthew-sherlin"])
    );
}

#[test]
fn a_store_without_claims_has_no_identities() {
    let db = identity_store("none");
    let report = identity_clusters(&db, SAME_AS_FLOOR);
    assert!(report.clusters.is_empty());
    assert_eq!((report.linked, report.claims), (0, 0));
}

// ── an explicit alias claim links a stub ────────────────────────────────────

fn alias_keys_of(db: &GraphDb<core_storage::fs::RealFs>, key: &str) -> Option<Vec<String>> {
    match db.get_prop(key, ALIAS_KEYS_FIELD) {
        None => None,
        Some(Value::List(items)) => Some(
            items
                .into_iter()
                .map(|v| match v {
                    Value::Str(s) => s,
                    other => panic!("non-string alias key {other:?}"),
                })
                .collect(),
        ),
        other => panic!("{key} holds a foreign alias_keys: {other:?}"),
    }
}

/// The `SAME_AS` edges as stored: `(src, dst, weight)`, sorted.
fn same_as_edges(db: &GraphDb<core_storage::fs::RealFs>) -> Vec<(String, String, Option<f64>)> {
    let mut edges = db.weighted_edges(SAME_AS_EDGE, Some("weight"));
    edges.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    edges
}

fn the_claim() -> Vec<SameAsPair> {
    vec![SameAsPair {
        a: "matt".into(),
        b: "matthew-sherlin".into(),
        score: 1.0,
    }]
}

/// Owner decision Q2. `matt` holds one alias and `matthew-sherlin` five, so
/// `Overlap` scores 1/5 and never links them; the declared alias does, at 1.0.
#[test]
fn a_declared_alias_links_the_stub_it_names_entity_first() {
    let mut db = identity_store("claim-entity-first");
    let report = remember_people(
        &mut db,
        "described first",
        vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])],
    );
    assert!(
        report.same_as.is_empty(),
        "no stub yet: {:?}",
        report.same_as
    );
    assert_eq!(
        alias_keys_of(&db, "matthew-sherlin"),
        Some(strings(&["matt"]))
    );
    let about = strings(&["matt"]);
    let report = remember(&mut db, &note("named later", &about, &[])).unwrap();
    assert_eq!(report.same_as, the_claim());
    assert_eq!(
        same_as_edges(&db),
        vec![("matthew-sherlin".into(), "matt".into(), Some(1.0))],
        "one directed edge, entity to stub, at the exact claim's weight"
    );
    assert_eq!(
        same_as_pairs(&db, &strings(&["matt", "matthew-sherlin"])),
        the_claim(),
        "asked from both ends, it is still one claim"
    );
    assert_eq!(alias_keys_of(&db, "matt"), None, "a stub declares nothing");
}

/// The reverse order: the stub exists, and the entity that declares it
/// arrives afterwards.
#[test]
fn a_declared_alias_links_the_stub_it_names_stub_first() {
    let mut db = identity_store("claim-stub-first");
    let about = strings(&["matt"]);
    let report = remember(&mut db, &note("named first", &about, &[])).unwrap();
    assert!(report.same_as.is_empty(), "{:?}", report.same_as);
    let report = remember_people(
        &mut db,
        "described later",
        vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])],
    );
    assert_eq!(report.same_as, the_claim());
    assert_eq!(
        same_as_edges(&db),
        vec![("matthew-sherlin".into(), "matt".into(), Some(1.0))]
    );
}

/// The entity and the stub it names, written by one call.
#[test]
fn a_declared_alias_links_a_stub_made_by_the_same_call() {
    let mut db = identity_store("claim-same-call");
    let about = strings(&["matt"]);
    let entities = vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])];
    let report = remember(&mut db, &note("both at once", &about, &entities)).unwrap();
    assert_eq!(report.same_as, the_claim());
}

/// `upsert_entity`'s path declares a claim the same way, on a create and on a
/// later update of a node that declared nothing.
#[test]
fn describe_entity_declares_a_claim_on_create_and_on_update() {
    let mut db = identity_store("claim-describe");
    let about = strings(&["matt", "sherl"]);
    remember(&mut db, &note("two stubs", &about, &[])).unwrap();
    let name = vec![("name".to_string(), Value::Str("Matthew Sherlin".into()))];
    describe_entity_with_aliases(
        &mut db,
        "matthew-sherlin",
        Some("Person"),
        &name,
        &strings(&["matt"]),
    )
    .unwrap();
    assert_eq!(
        same_as_pairs(&db, &strings(&["matthew-sherlin"])),
        the_claim()
    );
    describe_entity_with_aliases(&mut db, "matthew-sherlin", None, &[], &strings(&["sherl"]))
        .unwrap();
    assert_eq!(
        alias_keys_of(&db, "matthew-sherlin"),
        Some(strings(&["matt", "sherl"])),
        "declared aliases accumulate"
    );
    assert_eq!(
        same_as_pairs(&db, &strings(&["matthew-sherlin"])).len(),
        2,
        "the second claim links the second stub"
    );
}

/// A claim that is also a full-name match is one claim, at the higher score:
/// `Entity→Person` by `Overlap` at 0.6 and `Person→Entity` by `KeyMatch` at 1.0.
#[test]
fn a_claim_and_an_overlap_on_one_pair_are_one_claim_at_the_higher_score() {
    let mut db = identity_store("claim-and-overlap");
    let about = strings(&["Matthew_Sherlin"]);
    remember(&mut db, &note("named first", &about, &[])).unwrap();
    let report = remember_people(
        &mut db,
        "described later",
        vec![person(
            "matthew-sherlin",
            "Matthew Sherlin",
            &["Matthew_Sherlin"],
        )],
    );
    assert_eq!(
        same_as_edges(&db),
        vec![
            (
                "Matthew_Sherlin".into(),
                "matthew-sherlin".into(),
                Some(0.6)
            ),
            (
                "matthew-sherlin".into(),
                "Matthew_Sherlin".into(),
                Some(1.0)
            ),
        ],
        "each direction is owned by its own rule"
    );
    assert_eq!(
        report.same_as,
        vec![SameAsPair {
            a: "Matthew_Sherlin".into(),
            b: "matthew-sherlin".into(),
            score: 1.0
        }]
    );
}

/// The false positive the design avoids. `alex-1` is named "Alex", so its
/// derived `aliases` hold `alex` — which is also a stub's key. A rule keyed on
/// `aliases` would link them; `alias_keys` holds only what a caller declared,
/// and nobody declared this.
#[test]
fn a_name_that_merely_spells_a_stubs_key_does_not_link_it() {
    let mut db = identity_store("claim-alex");
    remember_people(&mut db, "an alex", vec![person("alex-1", "Alex", &[])]);
    assert!(aliases_of(&db, "alex-1").contains(&"alex".to_string()));
    assert_eq!(alias_keys_of(&db, "alex-1"), None, "nothing was declared");
    let about = strings(&["alex"]);
    let report = remember(&mut db, &note("which alex?", &about, &[])).unwrap();
    assert!(report.same_as.is_empty(), "{:?}", report.same_as);
    assert!(same_as_edges(&db).is_empty());
}

/// The limitation, pinned: a claim matches the stub's key exactly, and keys
/// are case-sensitive. `Matt` is stored as declared and does not name `matt`.
#[test]
fn a_claim_matches_the_stubs_key_exactly_so_case_differs_do_not_link() {
    let mut db = identity_store("claim-case");
    let about = strings(&["matt"]);
    remember(&mut db, &note("named first", &about, &[])).unwrap();
    let report = remember_people(
        &mut db,
        "described later",
        vec![person("matthew-sherlin", "Matthew Sherlin", &["Matt"])],
    );
    assert_eq!(
        alias_keys_of(&db, "matthew-sherlin"),
        Some(strings(&["Matt"])),
        "stored verbatim"
    );
    assert!(report.same_as.is_empty(), "{:?}", report.same_as);
    assert!(same_as_edges(&db).is_empty());
}

/// The limitation's other half: the claim's target must carry label `Entity`.
/// Two nodes that both carry an entity label are never linked by claim, even
/// when one declares the other's key.
#[test]
fn a_claim_does_not_link_two_nodes_under_entity_labels() {
    let mut db = identity_store("claim-described");
    let report = remember_people(
        &mut db,
        "two people",
        vec![
            person("matt", "Matt", &[]),
            person("matthew-sherlin", "Matthew Sherlin", &["matt"]),
        ],
    );
    assert!(report.same_as.is_empty(), "{:?}", report.same_as);
}

#[test]
fn alias_keys_are_trimmed_deduped_sorted_and_otherwise_verbatim() {
    let mut db = store("alias-keys-shape");
    let entities = vec![person(
        "matthew-sherlin",
        "Matthew Sherlin",
        &["  matt ", "Matt", "matt", "M. Sherlin", "", "   "],
    )];
    remember(&mut db, &note("declared", &[], &entities)).unwrap();
    assert_eq!(
        alias_keys_of(&db, "matthew-sherlin"),
        Some(strings(&["M. Sherlin", "Matt", "matt"])),
        "byte order, case and punctuation kept, blanks dropped"
    );
    // The derived name and key tokens never go in.
    for derived in ["matthew", "sherlin", "matthew sherlin", "matthew-sherlin"] {
        assert!(
            !alias_keys_of(&db, "matthew-sherlin")
                .unwrap()
                .contains(&derived.to_string()),
            "{derived} is derived, not declared"
        );
    }
}

#[test]
fn an_entity_that_declares_nothing_carries_no_alias_keys() {
    let mut db = store("alias-keys-absent");
    remember_people(
        &mut db,
        "plain",
        vec![person("matthew-sherlin", "Matthew Sherlin", &[])],
    );
    assert_eq!(alias_keys_of(&db, "matthew-sherlin"), None);
}

#[test]
fn alias_keys_accumulate_across_writes_and_a_repeat_writes_nothing() {
    let mut db = store("alias-keys-accumulate");
    remember_people(
        &mut db,
        "first",
        vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])],
    );
    remember_people(
        &mut db,
        "second",
        vec![person("matthew-sherlin", "Matthew Sherlin", &["sherl"])],
    );
    describe_entity_with_aliases(&mut db, "matthew-sherlin", None, &[], &strings(&["ms"])).unwrap();
    assert_eq!(
        alias_keys_of(&db, "matthew-sherlin"),
        Some(strings(&["matt", "ms", "sherl"]))
    );
    // A write that declares nothing, or only what is already held, leaves it.
    describe_entity(&mut db, "matthew-sherlin", None, &[]).unwrap();
    assert_eq!(
        core_api::memory::identity::alias_keys_after_write(
            &db,
            "matthew-sherlin",
            &[],
            &strings(&["matt", " ms "])
        )
        .unwrap(),
        None,
        "nothing to rewrite"
    );
    assert_eq!(
        alias_keys_of(&db, "matthew-sherlin"),
        Some(strings(&["matt", "ms", "sherl"]))
    );
}

#[test]
fn an_alias_keys_property_is_refused_with_the_argument_named() {
    let mut db = store("alias-keys-prop");
    let claim = (
        ALIAS_KEYS_FIELD.to_string(),
        Value::List(vec![Value::Str("matt".into())]),
    );
    let err = describe_entity(
        &mut db,
        "matthew",
        Some("Person"),
        std::slice::from_ref(&claim),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("'alias_keys' is maintained by the store")
            && err.contains("'aliases' argument"),
        "{err}"
    );
    assert!(!db.has_node("matthew"), "a refusal writes nothing");

    let mut entity = person("matthew", "Matthew", &[]);
    entity.props.insert(claim.0, claim.1);
    let before = db.stats().nodes_live;
    let err = remember(&mut db, &note("refused", &[], &[entity]))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("'alias_keys' is maintained by the store"),
        "{err}"
    );
    assert_eq!(
        db.stats().nodes_live,
        before,
        "neither the note nor the entity"
    );
}

/// Thirty-three declared aliases that fold to one canonical alias: `aliases`
/// stays far under its own cap, so it is the `alias_keys` cap that refuses.
#[test]
fn more_alias_keys_than_the_cap_is_refused_before_anything_is_written() {
    let mut db = store("alias-keys-cap");
    assert_eq!(MAX_ALIAS_KEYS, 32);
    let spelled =
        |n: usize| -> Vec<String> { (0..n).map(|i| format!("a{}", ".".repeat(i))).collect() };
    let entity = |aliases: Vec<String>| EntityIn {
        key: "hoarder".into(),
        label: "Person".into(),
        props: BTreeMap::new(),
        aliases,
    };
    let before = db.stats().nodes_live;
    let err = remember(
        &mut db,
        &note("too many", &[], &[entity(spelled(MAX_ALIAS_KEYS + 1))]),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("hoarder")
            && err.contains("33 declared aliases")
            && err.contains("alias_keys"),
        "{err}"
    );
    assert_eq!(
        db.stats().nodes_live,
        before,
        "neither the note nor the entity"
    );

    // Exactly the cap is accepted; one more on a later write is refused and
    // leaves the stored list as it was.
    remember(
        &mut db,
        &note("at the cap", &[], &[entity(spelled(MAX_ALIAS_KEYS))]),
    )
    .unwrap();
    assert_eq!(alias_keys_of(&db, "hoarder").unwrap().len(), MAX_ALIAS_KEYS);
    let err = describe_entity_with_aliases(&mut db, "hoarder", None, &[], &strings(&["one more"]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("alias_keys"), "{err}");
    assert_eq!(alias_keys_of(&db, "hoarder").unwrap().len(), MAX_ALIAS_KEYS);
    assert!(!aliases_of(&db, "hoarder").contains(&"one more".to_string()));
}

/// An `alias_keys` value written raw before the store maintained it: a string
/// or a list of strings is taken in, anything else refused with the node named.
#[test]
fn an_existing_alias_keys_value_is_taken_in_or_refused_never_overwritten() {
    let mut db = store("alias-keys-foreign");
    for (key, value) in [
        ("as-string", Value::Str(" matt ".into())),
        ("as-list", Value::List(vec![Value::Str("matt".into())])),
        ("as-int", Value::Int(3)),
    ] {
        db.insert_node("Person", key, vec![(ALIAS_KEYS_FIELD.to_string(), value)])
            .unwrap();
    }
    for key in ["as-string", "as-list"] {
        describe_entity_with_aliases(&mut db, key, None, &[], &strings(&["sherl"])).unwrap();
        assert_eq!(
            alias_keys_of(&db, key),
            Some(strings(&["matt", "sherl"])),
            "{key}"
        );
    }
    let err = set_role(&mut db, "as-int").unwrap_err().to_string();
    assert!(
        err.contains("as-int") && err.contains("forget") && err.contains("alias_keys"),
        "{err}"
    );
    assert_eq!(db.get_prop("as-int", "role"), None, "nothing written");
    assert_eq!(db.get_prop("as-int", ALIAS_KEYS_FIELD), Some(Value::Int(3)));
}

/// Removing the declared list retracts the claim; the engine re-evaluates the
/// rule that read it.
#[test]
fn clearing_alias_keys_retracts_the_claim() {
    let mut db = identity_store("claim-cleared");
    let about = strings(&["matt"]);
    let entities = vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])];
    remember(&mut db, &note("both", &about, &entities)).unwrap();
    assert_eq!(same_as_edges(&db).len(), 1);
    assert!(db.remove_prop("matthew-sherlin", ALIAS_KEYS_FIELD).unwrap());
    assert!(same_as_edges(&db).is_empty());
    // A later write that declares nothing does not bring it back.
    describe_entity(&mut db, "matthew-sherlin", None, &[]).unwrap();
    assert_eq!(alias_keys_of(&db, "matthew-sherlin"), None);
    assert!(same_as_edges(&db).is_empty());
}

// ── a claimed stub in identity resolution ───────────────────────────────────

/// A stub linked by claim joins the entity's identity, and the canonical is
/// the older of the two whichever one that is.
#[test]
fn a_claimed_stub_joins_the_identity_and_the_oldest_is_canonical() {
    let mut db = identity_store("claim-cluster-stub-first");
    let about = strings(&["matt"]);
    remember(&mut db, &note("named first", &about, &[])).unwrap();
    remember_people(
        &mut db,
        "described later",
        vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])],
    );
    let report = identity_clusters(&db, SAME_AS_FLOOR);
    assert_eq!((report.linked, report.claims), (2, 1), "{report:?}");
    assert_eq!(report.clusters.len(), 1, "{report:?}");
    assert_eq!(report.clusters[0].canonical, "matt");
    assert_eq!(
        report.clusters[0].members,
        strings(&["matt", "matthew-sherlin"])
    );
    assert_eq!(report.clusters[0].weakest, 1.0);

    let mut db = identity_store("claim-cluster-entity-first");
    remember_people(
        &mut db,
        "described first",
        vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])],
    );
    remember(&mut db, &note("named later", &about, &[])).unwrap();
    let report = identity_clusters(&db, SAME_AS_FLOOR);
    assert_eq!(report.clusters.len(), 1, "{report:?}");
    assert_eq!(report.clusters[0].canonical, "matthew-sherlin");
    assert_eq!(
        report.clusters[0].members,
        strings(&["matthew-sherlin", "matt"])
    );
}

/// Complete linkage still holds for a claim: a stub joins an identity of two
/// only when both members claim it. One claim is one link, not a merge.
///
/// c0 declares t1..t10 and c1 declares t2..t11, so they link at 9/13; only c0
/// names the stub keyed `t1`.
#[test]
fn a_claimed_stub_joins_a_larger_identity_only_when_every_member_claims_it() {
    let mut db = identity_store("claim-cluster-three");
    remember_people(
        &mut db,
        "two linked nodes",
        vec![tagged("c0", 1..=10), tagged("c1", 2..=11)],
    );
    let about = strings(&["t1"]);
    remember(&mut db, &note("named later", &about, &[])).unwrap();
    let report = identity_clusters(&db, SAME_AS_FLOOR);
    assert_eq!(report.claims, 2, "c0~c1 and c0~t1: {report:?}");
    assert_eq!(
        clusters_of(&db),
        vec![strings(&["c0", "c1"])],
        "c1 never claimed t1"
    );

    remember_people(&mut db, "c1 claims it too", vec![tagged("c1", 1..=1)]);
    assert_eq!(identity_clusters(&db, SAME_AS_FLOOR).claims, 3);
    assert_eq!(clusters_of(&db), vec![strings(&["c0", "c1", "t1"])]);
}

// ── NFC: a decomposed name is the same name ─────────────────────────────────

/// `E` followed by U+0301 COMBINING ACUTE ACCENT is `É` typed another way —
/// what macOS file names and some keyboards produce. Both spell one name.
#[test]
fn a_decomposed_name_yields_the_same_aliases_as_its_composed_form() {
    let decomposed = "E\u{301}mile Zola";
    let composed = "Émile Zola";
    assert_ne!(decomposed, composed, "the fixture must differ in bytes");
    assert_eq!(
        core_api::memory::identity::canonical(decomposed),
        "émile zola"
    );
    assert_eq!(
        derive_aliases("zola", Some(decomposed), &strings(&[decomposed])),
        derive_aliases("zola", Some(composed), &strings(&[composed])),
    );
    assert_eq!(
        derive_aliases("zola", Some(decomposed), &[]),
        strings(&["zola", "émile", "émile zola"])
    );
}

/// End to end: two people whose names differ only in Unicode form link.
#[test]
fn a_decomposed_and_a_composed_name_link() {
    let mut db = identity_store("nfc-link");
    remember_people(
        &mut db,
        "composed",
        vec![person("zola-1", "Émile Zola", &[])],
    );
    let report = remember_people(
        &mut db,
        "decomposed",
        vec![person("zola-2", "E\u{301}mile Zola", &[])],
    );
    assert_eq!(
        report.same_as,
        vec![SameAsPair {
            a: "zola-1".into(),
            b: "zola-2".into(),
            score: 0.6
        }]
    );
    assert_eq!(
        db.get_prop("zola-2", "name"),
        Some(Value::Str("E\u{301}mile Zola".into())),
        "the name itself is stored as written; only the aliases are normalised"
    );
}

// ── limits the changelog states, pinned ─────────────────────────────────────

/// A declared alias is one more entry in `aliases`, so it counts against
/// `Overlap`: two same-named entities link at 3/5, and at 3/6 — under the
/// floor — once one of them declares a nickname the other does not.
#[test]
fn a_declared_alias_counts_against_overlap() {
    let mut db = identity_store("claim-dilutes");
    let report = remember_people(
        &mut db,
        "one declares a nickname",
        vec![
            person("matthew-sherlin", "Matthew Sherlin", &["matt"]),
            person("msherlin", "Matthew Sherlin", &[]),
        ],
    );
    assert!(report.same_as.is_empty(), "3/6 = 0.5: {:?}", report.same_as);
}

/// The claim rules run from the five entity labels. An entity under any other
/// label declares its aliases all the same, but no rule reads them.
#[test]
fn a_claim_from_a_label_outside_the_five_does_not_link() {
    let mut db = identity_store("claim-other-label");
    let about = strings(&["seven"]);
    let entities = vec![EntityIn {
        key: "v0.7".into(),
        label: "Release".into(),
        props: BTreeMap::new(),
        aliases: strings(&["seven"]),
    }];
    let report = remember(&mut db, &note("a release", &about, &entities)).unwrap();
    assert_eq!(alias_keys_of(&db, "v0.7"), Some(strings(&["seven"])));
    assert!(report.same_as.is_empty(), "{:?}", report.same_as);
}

/// The byte-identity exception the doc comments state: a key with padding
/// keeps its untrimmed lowercase alias, gains the canonical one on the next
/// write, and is stable after that.
#[test]
fn a_padded_keys_alias_list_settles_after_one_more_write() {
    let mut db = store("padded-key");
    describe_entity(&mut db, " Matt ", Some("Person"), &[]).unwrap();
    assert_eq!(aliases_of(&db, " Matt "), strings(&[" matt "]));
    set_role(&mut db, " Matt ").unwrap();
    assert_eq!(aliases_of(&db, " Matt "), strings(&[" matt ", "matt"]));
    assert_eq!(
        core_api::memory::identity::aliases_after_write(&db, " Matt ", &[], &[]).unwrap(),
        None,
        "stable from here"
    );
}

// ── a described ex-stub: claimable, and unable to claim ─────────────────────

fn label_of(db: &GraphDb<core_storage::fs::RealFs>, key: &str) -> String {
    db.node_ref(key).expect("node").label().to_string()
}

/// A node `about` created keeps label `Entity` for life, described or not. The
/// claim rules key on that label, so a stub that was later described is still
/// claimed by a Person's declared alias — in either order.
#[test]
fn a_described_ex_stub_is_still_linked_by_a_declared_alias() {
    let described = vec![("name".to_string(), Value::Str("Matt S".into()))];

    // Described before the claim arrives.
    let mut db = identity_store("ex-stub-claimed-after");
    let about = strings(&["matt"]);
    remember(&mut db, &note("named first", &about, &[])).unwrap();
    describe_entity(&mut db, "matt", Some("Person"), &described).unwrap();
    assert_eq!(db.get_prop("matt", "provisional"), None, "it is described");
    assert_eq!(label_of(&db, "matt"), "Entity", "and still an Entity");
    let report = remember_people(
        &mut db,
        "the claim",
        vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])],
    );
    assert_eq!(report.same_as, the_claim());

    // Claimed first, described afterwards: the link stays.
    let mut db = identity_store("ex-stub-claimed-before");
    let entities = vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])];
    remember(&mut db, &note("both", &about, &entities)).unwrap();
    describe_entity(&mut db, "matt", Some("Person"), &described).unwrap();
    assert_eq!(label_of(&db, "matt"), "Entity");
    assert_eq!(
        same_as_pairs(&db, &strings(&["matt"])),
        the_claim(),
        "describing the stub does not retract the claim on it"
    );
}

/// The other half, pinned as it is today: a described ex-stub cannot itself
/// claim. `matthew-sherlin` was named by `about` before it was described, so
/// it is an `Entity`, and the claim rules run from the five entity labels
/// only. The alias it declares is stored and no rule reads it.
///
/// An `Entity→Entity` claim rule would close this. It is an owner decision
/// (plan 3, "Owner decisions — answered 2026-10-01", Q2), not taken here.
#[test]
fn a_described_ex_stub_cannot_itself_claim() {
    let mut db = identity_store("ex-stub-cannot-claim");
    let about = strings(&["matthew-sherlin"]);
    remember(&mut db, &note("named first", &about, &[])).unwrap();
    describe_entity_with_aliases(
        &mut db,
        "matthew-sherlin",
        Some("Person"),
        &[("name".to_string(), Value::Str("Matthew Sherlin".into()))],
        &strings(&["matt"]),
    )
    .unwrap();
    assert_eq!(label_of(&db, "matthew-sherlin"), "Entity");
    assert_eq!(
        alias_keys_of(&db, "matthew-sherlin"),
        Some(strings(&["matt"])),
        "the declaration is kept"
    );
    let about = strings(&["matt"]);
    let report = remember(&mut db, &note("the nickname", &about, &[])).unwrap();
    assert!(report.same_as.is_empty(), "{:?}", report.same_as);
    assert!(same_as_edges(&db).is_empty());
}

// ── a write that retracts identity links says so ────────────────────────────

/// Every full-name link sits exactly on the floor, 3/5. A declared alias the
/// other node lacks makes it 3/6, and the link is retracted — the report
/// names it, with the score it had.
#[test]
fn declaring_an_alias_reports_the_link_it_costs() {
    let mut db = identity_store("lost-pair");
    let report = remember_people(
        &mut db,
        "two keys, one name",
        vec![
            person("matthew-sherlin", "Matthew Sherlin", &[]),
            person("msherlin", "Matthew Sherlin", &[]),
        ],
    );
    assert_eq!(report.same_as.len(), 1);
    assert!(report.same_as_lost.is_empty(), "{:?}", report.same_as_lost);

    let report = remember_people(
        &mut db,
        "one declares a nickname",
        vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])],
    );
    assert!(report.same_as.is_empty(), "{:?}", report.same_as);
    assert_eq!(
        report.same_as_lost,
        vec![SameAsPair {
            a: "matthew-sherlin".into(),
            b: "msherlin".into(),
            score: 0.6
        }]
    );
    assert!(same_as_edges(&db).is_empty(), "and the edges are gone");
}

/// The same loss against a stub that spells the full name — four aliases
/// against five becomes 3/6 — which a stub cannot declare its way out of.
#[test]
fn declaring_an_alias_reports_the_full_name_stub_it_unlinks() {
    let mut db = identity_store("lost-stub");
    let about = strings(&["Matthew_Sherlin"]);
    remember(&mut db, &note("named first", &about, &[])).unwrap();
    remember_people(
        &mut db,
        "described",
        vec![person("matthew-sherlin", "Matthew Sherlin", &[])],
    );
    let about = strings(&["matt"]);
    let entities = vec![person("matthew-sherlin", "Matthew Sherlin", &["matt"])];
    let report = remember(&mut db, &note("a nickname", &about, &entities)).unwrap();
    assert_eq!(report.same_as, the_claim(), "one link gained");
    assert_eq!(
        report.same_as_lost,
        vec![SameAsPair {
            a: "Matthew_Sherlin".into(),
            b: "matthew-sherlin".into(),
            score: 0.6
        }],
        "and one lost"
    );
}

/// A write that keeps every link — the same aliases declared on both sides,
/// 4/6 — reports nothing lost; nor does a repeat of it.
#[test]
fn a_write_that_retracts_nothing_reports_nothing_lost() {
    let mut db = identity_store("lost-nothing");
    let both = || {
        vec![
            person("matthew-sherlin", "Matthew Sherlin", &["matt"]),
            person("msherlin", "Matthew Sherlin", &["matt"]),
        ]
    };
    remember_people(
        &mut db,
        "plain",
        vec![
            person("matthew-sherlin", "Matthew Sherlin", &[]),
            person("msherlin", "Matthew Sherlin", &[]),
        ],
    );
    let report = remember_people(&mut db, "both declare", both());
    assert!(report.same_as_lost.is_empty(), "{:?}", report.same_as_lost);
    assert_eq!(same_as_pairs(&db, &strings(&["msherlin"])).len(), 1);
    let report = remember_people(&mut db, "again", both());
    assert!(report.same_as_lost.is_empty(), "{:?}", report.same_as_lost);
}

/// The foreign-value refusal, word for word: one sentence, no run of spaces.
#[test]
fn the_alias_keys_foreign_value_refusal_reads_as_one_sentence() {
    let mut db = store("alias-keys-foreign-text");
    db.insert_node(
        "Person",
        "as-int",
        vec![(ALIAS_KEYS_FIELD.to_string(), Value::Int(3))],
    )
    .unwrap();
    let err = set_role(&mut db, "as-int").unwrap_err();
    let core_api::GraphError::IngestError { detail } = err else {
        panic!("expected an ingest error, got {err:?}");
    };
    assert_eq!(
        detail,
        "'as-int' already carries an 'alias_keys' property that is not a string or a list \
         of strings; clear it with forget {key: \"as-int\", prop: \"alias_keys\"} first"
    );
}
