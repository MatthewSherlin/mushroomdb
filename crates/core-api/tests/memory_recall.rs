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

#[test]
fn an_unrelated_topic_sharing_one_word_is_not_a_confident_hit() {
    // A topic can OR-match a node on one incidental word while being about
    // something else entirely. Without a relevance floor this printed a
    // digest with no way to tell it apart from a real hit — the same shape
    // of bug the identifier gate's removal was supposed to fix, with the
    // sign flipped: instead of a real topic finding nothing, an unrelated
    // one finds something.
    let mut db = store("banana");
    db.insert_node("Note", "banana-note", vec![]).unwrap();
    db.set_prop(
        "banana-note",
        "text",
        Value::Str("banana bread recipe notes from grandma".into()),
    )
    .unwrap();
    match recall_digest(
        &db,
        "banana republic economic collapse history",
        "store",
        4000,
    ) {
        RecallOutcome::NoMatch => {}
        other => {
            panic!("an unrelated topic sharing one word must not read as a hit, got {other:?}")
        }
    }
}

#[test]
fn terms_scattered_one_per_node_do_not_add_up_to_a_hit() {
    // A corpus-wide coverage check ("does this word appear somewhere") passes
    // here: "apple", "banana" and "cherry" are each present in the store. But
    // no single node is about more than one of the topic's words — the
    // coverage that matters is per node, not per corpus.
    let mut db = store("scattered");
    for (key, text) in [
        ("apple-note", "apple orchard pie recipe"),
        ("banana-note", "banana bread recipe notes"),
        ("cherry-note", "cherry blossom festival photos"),
    ] {
        db.insert_node("Note", key, vec![]).unwrap();
        db.set_prop(key, "text", Value::Str(text.into())).unwrap();
    }
    match recall_digest(&db, "apple banana cherry", "store", 4000) {
        RecallOutcome::NoMatch => {}
        other => panic!(
            "one word per node across three unrelated nodes must not add up to a hit, got {other:?}"
        ),
    }
}

#[test]
fn terms_scattered_across_two_nodes_do_not_clear_the_majority() {
    // The two-word variant of the same shape: "apple banana" needs both words
    // in the *same* node. Splitting one word to each of two unrelated nodes
    // is a 50/50 split for each candidate, which the majority rule (more than
    // half) rejects.
    let mut db = store("scattered-two");
    db.insert_node("Note", "apple-note", vec![]).unwrap();
    db.set_prop(
        "apple-note",
        "text",
        Value::Str("apple orchard pie recipe".into()),
    )
    .unwrap();
    db.insert_node("Note", "banana-note", vec![]).unwrap();
    db.set_prop(
        "banana-note",
        "text",
        Value::Str("banana bread recipe notes".into()),
    )
    .unwrap();
    match recall_digest(&db, "apple banana", "store", 4000) {
        RecallOutcome::NoMatch => {}
        other => panic!(
            "one word per node across two unrelated nodes must not clear the majority, got {other:?}"
        ),
    }
}
