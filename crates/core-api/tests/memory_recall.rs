//! recall matches language. The 0.6.12 implementation matched identifiers, so
//! a human name — the commonest subject in a memory store — was unreachable.
use core_api::memory::recall::{recall_digest, RecallOutcome};
use core_api::memory_schema::memory_defaults;
use core_api::{GraphDb, Value};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-memrecall-{name}-{}-{nanos}",
        std::process::id()
    ))
}

fn store(name: &str) -> GraphDb<core_storage::fs::RealFs> {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    db.insert_node("Person", "matthew", vec![]).unwrap();
    db.set_prop("matthew", "name", Value::Str("Matthew Sherlin".into()))
        .unwrap();
    db
}

#[test]
fn a_plain_name_is_found() {
    let db = store("name");
    match recall_digest(&db, "Matthew", "store", 4000) {
        RecallOutcome::Hits(d) => assert!(d.contains("matthew"), "digest missed it: {d}"),
        other => panic!("expected hits for a plain name, got {other:?}"),
    }
}

#[test]
fn a_natural_question_is_found() {
    let db = store("question");
    match recall_digest(&db, "who is Matthew Sherlin?", "store", 4000) {
        RecallOutcome::Hits(d) => assert!(d.contains("matthew"), "digest missed it: {d}"),
        other => panic!("expected hits for a question, got {other:?}"),
    }
}

#[test]
fn a_store_with_no_declared_index_is_distinguishable_from_no_match() {
    let bare = GraphDb::open(&tmp("bare")).unwrap();
    assert!(matches!(
        recall_digest(&bare, "anything", "store", 4000),
        RecallOutcome::NoIndex
    ));

    let db = store("nomatch");
    assert!(matches!(
        recall_digest(&db, "zzz-no-such-token", "store", 4000),
        RecallOutcome::NoMatch
    ));
}

#[test]
fn an_identifier_still_works() {
    // The old behaviour was not wrong, only narrow. Code-shaped topics must
    // keep matching so the UserPromptSubmit hook does not regress.
    let mut db = store("ident");
    db.insert_node("Note", "note-1", vec![]).unwrap();
    db.set_prop(
        "note-1",
        "text",
        Value::Str("the graph_db project ships 0.7".into()),
    )
    .unwrap();
    match recall_digest(&db, "graph_db", "store", 4000) {
        RecallOutcome::Hits(d) => assert!(d.contains("note-1"), "digest missed it: {d}"),
        other => panic!("expected hits for an identifier, got {other:?}"),
    }
}
