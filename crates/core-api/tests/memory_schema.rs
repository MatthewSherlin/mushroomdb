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
