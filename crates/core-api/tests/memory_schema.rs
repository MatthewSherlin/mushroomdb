//! The default memory schema: what a memory store is created knowing.
use core_api::memory_schema::{memory_defaults, MEMORY_ENTITY_LABELS};
use core_api::GraphDb;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-memschema-{name}-{}-{nanos}",
        std::process::id()
    ))
}

#[test]
fn every_entity_label_has_a_searchable_name_field() {
    let s = memory_defaults();
    for label in MEMORY_ENTITY_LABELS {
        assert!(
            s.fulltext.iter().any(|(l, _)| l == label),
            "no full-text field declared for entity label {label}; \
             recall searches fulltext_pairs() and would never return a {label}"
        );
    }
}

#[test]
fn notes_are_searchable_too() {
    let s = memory_defaults();
    assert!(
        s.fulltext.iter().any(|(l, f)| l == "Note" && f == "text"),
        "Note.text must be declared: it is what remember writes"
    );
}

#[test]
fn applying_it_twice_is_a_no_op() {
    let mut db = GraphDb::open(&tmp("idem")).unwrap();
    let first = db.apply_schema(&memory_defaults()).unwrap();
    assert!(
        !first.created.is_empty(),
        "first apply created nothing: {first:?}"
    );
    let second = db.apply_schema(&memory_defaults()).unwrap();
    assert!(
        second.created.is_empty() && second.updated.is_empty(),
        "second apply was not a no-op: created={:?} updated={:?}",
        second.created,
        second.updated
    );
}

#[test]
fn it_declares_fulltext_the_engine_then_reports() {
    let mut db = GraphDb::open(&tmp("pairs")).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    let pairs = db.fulltext_pairs();
    assert!(
        pairs.iter().any(|(l, _)| l == "Person"),
        "fulltext_pairs() must report Person after apply: {pairs:?}"
    );
}

#[test]
fn index_pairs_lists_every_equality_index_sorted() {
    let mut db = GraphDb::open(&tmp("index-pairs")).unwrap();
    assert!(db.index_pairs().is_empty(), "a bare store has none");
    db.apply_schema(&memory_defaults()).unwrap();
    let pairs = db.index_pairs();
    let mut sorted = pairs.clone();
    sorted.sort();
    assert_eq!(pairs, sorted, "sorted, so the schema tool is byte-stable");
    for label in MEMORY_ENTITY_LABELS {
        assert!(
            pairs.contains(&((*label).to_string(), "name".to_string())),
            "{label}.name missing from {pairs:?}"
        );
        assert!(db.is_index_enabled(label, "name"));
    }
}

/// The report counts what `stats` never did, and its renderer leaves the
/// framing line to the one caller that stamps it.
#[test]
fn the_schema_report_counts_provisional_nodes_and_is_unframed() {
    use core_api::memory::brief::BriefOptions;
    use core_api::memory::remember::{remember, RememberInput};
    use core_api::memory::schema::{provisional_keys, render_schema, schema_report};

    let mut db = GraphDb::open(&tmp("report")).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    let about = vec!["reid".to_string(), "ada".to_string()];
    remember(
        &mut db,
        &RememberInput {
            text: "two unknowns",
            about: &about,
            kind: "note",
            ts: 1_759_000_000,
            source: None,
            entities: &[],
            facts: &[],
        },
    )
    .unwrap();
    // An `Entity` that is not provisional is not counted.
    db.insert_node("Entity", "deliberate", vec![]).unwrap();

    assert_eq!(provisional_keys(&db), vec!["ada", "reid"]);
    let report = schema_report(&db, &BriefOptions::default());
    assert_eq!(report.provisional, 2);
    let text = render_schema(&report);
    assert!(
        text.contains("provisional: 2 — named but not yet described: ada, reid"),
        "{text}"
    );
    assert!(
        !text.contains(core_api::digest::UNTRUSTED_FRAMING),
        "render_schema must not frame: {text}"
    );
}

/// The rendered schema is bounded in bytes, not only in lines per section: a
/// store whose names are long would otherwise render without limit.
#[test]
fn the_rendered_schema_is_capped_in_bytes_and_says_so() {
    use core_api::memory::brief::BriefOptions;
    use core_api::memory::schema::{render_schema, schema_report, SCHEMA_MAX_BYTES};

    let mut db = GraphDb::open(&tmp("schema-cap")).unwrap();
    // Twenty labels of 700 characters each: 14 KB of label lines alone.
    for i in 0..20 {
        let label = format!("L{i:02}{}", "x".repeat(700));
        db.insert_node(&label, &format!("n{i}"), vec![]).unwrap();
    }
    let text = render_schema(&schema_report(&db, &BriefOptions::default()));
    assert!(
        text.len() <= SCHEMA_MAX_BYTES,
        "{} bytes rendered against a cap of {SCHEMA_MAX_BYTES}",
        text.len()
    );
    let last = text.lines().next_back().unwrap_or_default();
    assert!(
        last.contains("truncated") && last.contains("json: true"),
        "the cut is announced, with the way to get the rest: {last}"
    );
    assert!(text.ends_with('\n'), "cut on a whole line");

    // An ordinary store is untouched.
    let mut small = GraphDb::open(&tmp("schema-small")).unwrap();
    small.insert_node("Person", "ada", vec![]).unwrap();
    let text = render_schema(&schema_report(&small, &BriefOptions::default()));
    assert!(!text.contains("truncated"), "{text}");
}
