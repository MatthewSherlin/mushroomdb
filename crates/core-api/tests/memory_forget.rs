//! `forget` at the layer it lives in: a node, one property or one fact, and a
//! report of what is left behind. The MCP tool renders this report; the
//! Python binding returns it. Moved out of `crates/server` in 0.7.
use core_api::memory::forget::{forget, ForgetTarget, FORGET_NOTE_LIST};
use core_api::memory::remember::{remember, RememberInput};
use core_api::memory_schema::memory_defaults;
use core_api::{Direction, GraphDb, GraphError, Predicate, RuleDef, Value};

type Db = GraphDb<core_storage::fs::RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-memforget-{name}-{}-{nanos}",
        std::process::id()
    ))
}

fn store(name: &str) -> Db {
    let mut db = GraphDb::open(&tmp(name)).unwrap();
    db.apply_schema(&memory_defaults()).unwrap();
    db
}

/// One note about `about`, returning the note's key. `ts` makes each distinct.
fn note(db: &mut Db, text: &str, about: &[&str], ts: i64) -> String {
    let about: Vec<String> = about.iter().map(|s| (*s).to_string()).collect();
    remember(
        db,
        &RememberInput {
            text,
            about: &about,
            kind: "note",
            ts,
            source: None,
            entities: &[],
            facts: &[],
        },
    )
    .unwrap()
    .note
}

fn same_team_rule() -> RuleDef {
    RuleDef {
        name: "same_team".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::FieldEqual {
            field: "team".into(),
        },
        edge_type: "SAME_TEAM".into(),
        weight_prop: Some("weight".into()),
        max_edges: Some(32),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

#[test]
fn forgetting_a_node_lists_the_notes_about_it_and_keeps_them() {
    let mut db = store("node");
    // `ada` does not exist, so `remember` stubs it as a provisional Entity
    // and links each note to it with a hand-written ABOUT edge.
    let mut notes: Vec<String> = (0..12)
        .map(|i| {
            note(
                &mut db,
                &format!("Ada fact number {i}"),
                &["ada"],
                1_759_000_000 + i,
            )
        })
        .collect();
    notes.sort();

    let report = forget(&mut db, &ForgetTarget::Node { key: "ada".into() }).unwrap();

    assert_eq!(report.mode, "node");
    assert_eq!(report.target, "ada (Entity)");
    assert!(report.changed);
    assert_eq!(report.manual_edges, 12, "one ABOUT edge per note");
    assert_eq!(report.derived_edges, 0, "a memory store ships no rules");
    assert_eq!(report.notes_total, 12);
    assert_eq!(
        report.notes,
        notes[..FORGET_NOTE_LIST],
        "the first ten, sorted"
    );
    assert!(!db.has_node("ada"));
    for key in &notes {
        assert!(db.has_node(key), "a note is listed, never deleted: {key}");
    }
}

#[test]
fn an_unknown_key_is_an_error_and_an_absent_property_is_not() {
    let mut db = store("prop");
    note(&mut db, "Ada likes Rust", &["ada"], 1_759_000_000);

    let missing = forget(
        &mut db,
        &ForgetTarget::Node {
            key: "nobody".into(),
        },
    );
    assert!(
        matches!(missing, Err(GraphError::KeyNotFound { ref key }) if key == "nobody"),
        "{missing:?}"
    );

    let absent = forget(
        &mut db,
        &ForgetTarget::Prop {
            key: "ada".into(),
            prop: "no_such_prop".into(),
        },
    )
    .unwrap();
    assert_eq!(absent.mode, "prop");
    assert_eq!(absent.target, "ada.no_such_prop");
    assert!(!absent.changed, "nothing was set, so nothing was written");
    assert_eq!(absent.derived_edges, 0);

    // Removing a property that is set is a change. `aliases_rewritten` is
    // carried, not asserted, here: a stub's name is its key, so the list the
    // key alone implies is the list it already has. The server's
    // `forgetting_a_name_takes_its_words_out_of_aliases_and_says_so` pins the
    // case where the name's words do leave.
    let named = forget(
        &mut db,
        &ForgetTarget::Prop {
            key: "ada".into(),
            prop: "name".into(),
        },
    )
    .unwrap();
    assert!(named.changed);
    assert_eq!(named.prop.as_deref(), Some("name"));
    assert_eq!(db.get_prop("ada", "name"), None);
}

#[test]
fn a_rule_derived_fact_is_refused_with_the_rule_and_its_fields_named() {
    let mut db = store("owned");
    for key in ["a", "b"] {
        db.insert_node(
            "Person",
            key,
            vec![("team".into(), Value::Str("red".into()))],
        )
        .unwrap();
    }
    db.create_rule(same_team_rule()).unwrap();
    assert_eq!(
        db.neighbors("a", "SAME_TEAM", Direction::Out).unwrap(),
        vec!["b".to_string()],
        "fixture: the rule derived the edge"
    );

    let refused = forget(
        &mut db,
        &ForgetTarget::Fact {
            subject: "a".into(),
            predicate: "SAME_TEAM".into(),
            object: "b".into(),
        },
    );
    match refused {
        Err(GraphError::RuleOwned { detail }) => assert_eq!(
            detail,
            "refused: SAME_TEAM a → b is derived by rule same_team. It changes only when \
             the fields that rule reads change (team), or when the rule is deleted. \
             Nothing was written."
        ),
        other => panic!("expected a RuleOwned refusal, got {other:?}"),
    }
    assert_eq!(
        db.neighbors("a", "SAME_TEAM", Direction::Out).unwrap(),
        vec!["b".to_string()],
        "nothing was written"
    );
}

#[test]
fn a_hand_written_fact_is_retracted_once_and_the_shapes_are_exclusive() {
    let mut db = store("fact");
    for key in ["a", "b"] {
        db.insert_node("Person", key, vec![]).unwrap();
    }
    db.insert_edge("KNOWS", "a", "b").unwrap();
    let fact = ForgetTarget::Fact {
        subject: "a".into(),
        predicate: "KNOWS".into(),
        object: "b".into(),
    };

    let first = forget(&mut db, &fact).unwrap();
    assert_eq!(first.mode, "fact");
    assert_eq!(first.target, "KNOWS a → b");
    assert!(first.changed);
    assert!(first.notes.is_empty(), "no note is about both ends");

    let second = forget(&mut db, &fact).unwrap();
    assert!(!second.changed, "already gone: nothing to retract");

    // Exactly one of the three shapes.
    let s = |v: &str| Some(v.to_string());
    let triple = Some(("a".to_string(), "KNOWS".to_string(), "b".to_string()));
    assert_eq!(
        ForgetTarget::from_parts(s("a"), None, None),
        Some(ForgetTarget::Node { key: "a".into() })
    );
    assert_eq!(
        ForgetTarget::from_parts(s("a"), s("team"), None),
        Some(ForgetTarget::Prop {
            key: "a".into(),
            prop: "team".into()
        })
    );
    assert_eq!(
        ForgetTarget::from_parts(None, None, triple.clone()),
        Some(fact)
    );
    assert_eq!(ForgetTarget::from_parts(None, None, None), None);
    assert_eq!(ForgetTarget::from_parts(None, s("team"), None), None);
    assert_eq!(
        ForgetTarget::from_parts(s("a"), s("team"), triple.clone()),
        None
    );
    assert_eq!(ForgetTarget::from_parts(s("a"), None, triple), None);
}

/// Two rules derive the same edge type between the same labels; only one of
/// them derived this edge. The refusal names that one — from the engine's
/// provenance — not both.
///
/// Two rules sharing `(src_label, dst_label, edge_type)` is the shape ledger
/// row 37 says the engine mis-retracts. This test never retracts — it only
/// asks who owns an edge that exists — so it holds. Do not extend it with a
/// property change.
#[test]
fn the_refusal_names_the_rule_that_derived_the_edge_not_every_look_alike() {
    let mut db = store("provenance");
    db.insert_node(
        "Person",
        "a",
        vec![
            ("team".into(), Value::Str("red".into())),
            ("city".into(), Value::Str("Oslo".into())),
        ],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "b",
        vec![
            ("team".into(), Value::Str("red".into())),
            ("city".into(), Value::Str("Lima".into())),
        ],
    )
    .unwrap();
    let mut by_team = same_team_rule();
    by_team.edge_type = "LINKED".into();
    let mut by_city = same_team_rule();
    by_city.name = "same_city".into();
    by_city.predicate = Predicate::FieldEqual {
        field: "city".into(),
    };
    by_city.edge_type = "LINKED".into();
    db.create_rule(by_team).unwrap();
    db.create_rule(by_city).unwrap();

    let refused = forget(
        &mut db,
        &ForgetTarget::Fact {
            subject: "a".into(),
            predicate: "LINKED".into(),
            object: "b".into(),
        },
    );
    match refused {
        Err(GraphError::RuleOwned { detail }) => assert_eq!(
            detail,
            "refused: LINKED a → b is derived by rule same_team. It changes only when \
             the fields that rule reads change (team), or when the rule is deleted. \
             Nothing was written.",
            "same_city reads `city`, which these two do not share"
        ),
        other => panic!("expected a RuleOwned refusal, got {other:?}"),
    }
}
