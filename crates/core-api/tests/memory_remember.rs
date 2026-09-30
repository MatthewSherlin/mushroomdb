//! remember: a fact may arrive before its subject, and the caller is told
//! what the store did with it.
use core_api::memory::recall::{recall_digest, RecallOutcome};
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

/// Fix round 2: `entities[].label` is free-form — `memory_defaults()` only
/// declares full-text for `name` on the five built-in labels — so an entity
/// under any other label (the spec's own worked example is `Release`)
/// landed with a `name` `recall` could never reach: spec §1.1(b)'s defect,
/// reproduced through this release's own new feature. `remember` must
/// self-declare it, the same way it already does for `Note.text`.
#[test]
fn an_unseen_entity_label_becomes_searchable() {
    let mut db = store("unseen-label");
    let mut props = BTreeMap::new();
    props.insert("name".to_string(), Value::Str("v0.7".into()));
    let entities = vec![EntityIn {
        key: "v0.7".into(),
        label: "Release".into(),
        props,
    }];
    remember(
        &mut db,
        &RememberInput {
            text: "Matthew wants 0.7 to focus on the write path",
            about: &[],
            kind: "note",
            ts: 1_759_000_002,
            source: None,
            entities: &entities,
            facts: &[],
        },
    )
    .expect("remember");

    assert!(
        db.fulltext_pairs()
            .contains(&("Release".to_string(), NAME_FIELD.to_string())),
        "an unseen entity label must be self-declared for full-text: {:?}",
        db.fulltext_pairs()
    );

    match recall_digest(&db, "v0.7", "test", 4_000) {
        RecallOutcome::Hits(digest) => assert!(
            digest.contains("v0.7"),
            "recall must find the entity it was just told about: {digest}"
        ),
        other => panic!("expected a hit, got {other:?}"),
    }
}

/// The self-declare above is bounded: past `MAX_FULLTEXT_PAIRS` distinct
/// declared pairs, a new `entities[].label` is written normally but is not
/// made searchable, rather than growing the store's declared full-text
/// surface — and so every future re-open's rebuild cost — without limit on
/// caller-supplied strings.
#[test]
fn self_declaring_full_text_for_new_labels_is_bounded() {
    let mut db = store("many-labels");
    // Drive the declared surface past any reasonable cap with distinct
    // single-entity calls, each under its own throwaway label.
    for i in 0..64 {
        let mut props = BTreeMap::new();
        props.insert("name".to_string(), Value::Str(format!("thing-{i}")));
        let entities = vec![EntityIn {
            key: format!("thing-{i}"),
            label: format!("Kind{i}"),
            props,
        }];
        remember(
            &mut db,
            &RememberInput {
                text: &format!("a note about thing {i}"),
                about: &[],
                kind: "note",
                ts: 1_759_100_000 + i,
                source: None,
                entities: &entities,
                facts: &[],
            },
        )
        .unwrap_or_else(|e| panic!("remember {i}: {e}"));
    }
    let pairs = db.fulltext_pairs();
    assert!(
        pairs.len() < 64 + 8,
        "the declared full-text surface must stay bounded, not grow with every \
         distinct caller-supplied label: {} pairs",
        pairs.len()
    );
    // Every entity was still written, whether or not it ended up searchable.
    assert!(db.has_node("thing-0"));
    assert!(db.has_node("thing-63"));
}

/// Fix round 3 / finding 4: spec §3.2 promises "a cap per commit, so a
/// malformed batch cannot flood the graph" — there was none. A batch right
/// at the cap (20, `MAX_PROVISIONAL_PER_COMMIT`) must be entirely unaffected
/// by it: every key stubbed, none refused.
#[test]
fn a_batch_at_the_provisional_cap_is_unaffected() {
    let mut db = store("cap-exact");
    let about: Vec<String> = (0..20).map(|i| format!("k{i}")).collect();
    let report = remember(&mut db, &input("twenty unknown subjects", &about)).unwrap();
    assert_eq!(report.provisional.len(), 20, "{report:?}");
    assert!(
        report.provisional_capped.is_empty(),
        "a batch exactly at the cap must not be capped: {report:?}"
    );
    for k in &about {
        assert_eq!(
            db.get_prop(k, PROVISIONAL_PROP),
            Some(Value::Bool(true)),
            "{k} must be stubbed"
        );
        assert!(
            db.node_edges(k)
                .unwrap()
                .iter()
                .any(|e| e.edge_type == "ABOUT"),
            "{k} must have its ABOUT edge"
        );
    }
}

/// A batch over the cap: the excess is refused, reported, and never gets a
/// node or an edge — but the note and every key under the cap still commit.
/// This is the exact defect fix round 3 closes: on the pre-fix binary, 40
/// stubs landed in one call with no cap, no warning, and no report field
/// naming any of them.
#[test]
fn a_batch_over_the_provisional_cap_refuses_and_reports_the_excess() {
    let mut db = store("cap-exceeded");
    let about: Vec<String> = (0..25).map(|i| format!("k{i}")).collect();
    let report = remember(&mut db, &input("twenty-five unknown subjects", &about)).unwrap();

    assert_eq!(report.provisional.len(), 20, "{report:?}");
    assert_eq!(report.provisional_capped.len(), 5, "{report:?}");
    assert_eq!(
        report.provisional_capped,
        about[20..].to_vec(),
        "the capped keys must be named, not just counted: {report:?}"
    );

    // Everything under the cap committed normally.
    for k in &about[..20] {
        assert_eq!(db.get_prop(k, PROVISIONAL_PROP), Some(Value::Bool(true)));
    }
    // Nothing over the cap was written at all — no node, hence no edge.
    for k in &about[20..] {
        assert!(!db.has_node(k), "{k} must not have been created");
    }
    // The note itself, and every key under the cap, still committed — one
    // caller mistake does not cost the whole write.
    assert!(db.has_node(&report.note), "the note must still be written");
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

/// A store ingest-git wrote before 0.7 carries `about_<label>` rules: KeyMatch
/// on `Note.about`, deriving the `ABOUT` edges `remember` also writes itself.
/// Built here with the rule's own shape rather than from `repograph::rules`.
fn about_file_rule() -> core_api::RuleDef {
    let predicate = core_api::Predicate::KeyMatch {
        field: "about".into(),
    };
    core_api::RuleDef {
        name: "about_file".into(),
        src_label: "Note".into(),
        dst_label: "File".into(),
        max_edges: Some(core_api::default_max_edges(&predicate)),
        predicate,
        edge_type: "ABOUT".into(),
        weight_prop: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

#[test]
fn remembering_the_same_note_twice_about_a_rule_linked_node_succeeds() {
    let mut db = store("about-rule");
    db.insert_node("File", "src/zanzibar.rs", vec![]).unwrap();
    db.create_rule(about_file_rule()).unwrap();
    let about = vec!["src/zanzibar.rs".to_string()];

    let first = remember(&mut db, &input("the router lives here", &about)).expect("first");
    // Content-addressed: the same text and ts is the same note, and its ABOUT
    // edge is now the rule's. Saying it again must not be an error.
    let second = remember(&mut db, &input("the router lives here", &about)).expect("second");
    assert_eq!(first.note, second.note);

    let about_edges: Vec<_> = db
        .node_edges(&first.note)
        .unwrap()
        .into_iter()
        .filter(|e| e.edge_type == "ABOUT")
        .collect();
    assert_eq!(about_edges.len(), 1, "{about_edges:?}");
    assert!(about_edges[0].derived, "the rule owns the edge");
    // `derived` counts what this commit's rules produced on the note: the
    // first call's rule derived the edge, the second derived nothing.
    assert_eq!((first.derived, second.derived), (1, 0));
}
