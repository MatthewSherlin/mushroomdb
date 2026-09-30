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
fn terms_split_one_per_node_across_two_nodes_each_cover_half() {
    // Restated in 0.7 plan 2. This used to require NoMatch under a *strict*
    // majority. Half is now enough, because the shape that rule was really
    // rejecting is the shape the product is made of: an entity node holding a
    // name and a note holding the fact, where the topic's two words never
    // co-occur. A store asked for "apple banana" that holds an apple note and
    // a banana note has two honest answers; returning nothing was the worse
    // of the two. The minority cases below still refuse.
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
        RecallOutcome::Hits(d) => {
            assert!(d.contains("apple-note") && d.contains("banana-note"), "{d}");
            assert_eq!(
                d.matches("(1/2 terms)").count(),
                2,
                "both are half-covered: {d}"
            );
        }
        other => panic!("half coverage is a hit, shown as such, got {other:?}"),
    }
}

#[test]
fn the_entity_a_question_is_about_is_not_dropped_for_holding_only_its_name() {
    // The defect this task exists for. "What does Matthew prefer?" is two
    // terms after the stopwords go: matthew, prefer. The note holds both.
    // The Person node holds one, which is all a Person node ever holds, and
    // under a strict majority the subject of the question was dropped from
    // the answer to it.
    let mut db = store("subject");
    db.insert_node("Note", "note-1", vec![]).unwrap();
    db.set_prop(
        "note-1",
        "text",
        Value::Str("Matthew prefers concise summaries".into()),
    )
    .unwrap();
    match recall_digest(&db, "What does Matthew prefer?", "store", 4000) {
        RecallOutcome::Hits(d) => {
            assert!(d.contains("note-1"), "the fact must be there: {d}");
            assert!(d.contains("matthew"), "the subject must be there too: {d}");
            let note_first = d.find("  note-1").unwrap() < d.find("  matthew").unwrap();
            assert!(note_first, "the fuller match ranks first: {d}");
        }
        other => panic!("expected the note and its subject, got {other:?}"),
    }
}

#[test]
fn a_digest_says_how_much_of_the_topic_each_hit_covered() {
    let db = store("shown");
    match recall_digest(&db, "Matthew Sherlin", "store", 4000) {
        RecallOutcome::Hits(d) => {
            assert!(
                d.contains("\n  matthew — Matthew Sherlin (2/2 terms)\n"),
                "line shape: {d:?}"
            )
        }
        other => panic!("expected hits, got {other:?}"),
    }
}

#[test]
fn a_long_prompt_is_capped_before_it_becomes_a_query() {
    // The prompt hook feeds whole prompts through this path from plan 2 Task
    // 4 onward. Without a cap a two-hundred-word prompt becomes a
    // two-hundred-clause OR query on every turn. `repograph::recall` capped
    // at 24; this had no cap at all.
    let db = store("capped");
    let long: String = (0..200).map(|i| format!("tok{i} ")).collect();
    // 200 unique non-stopwords, none of them in the store: the call must
    // return rather than hang, and must not error.
    assert!(matches!(
        recall_digest(&db, &format!("{long} matthew"), "store", 4000),
        RecallOutcome::NoMatch | RecallOutcome::Hits(_)
    ));
    assert_eq!(core_api::memory::recall::MAX_QUERY_TERMS, 24);
}
