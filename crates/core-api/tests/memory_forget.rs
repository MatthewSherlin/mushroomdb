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

/// An edge written by hand that a live rule would derive is refused as well,
/// and no rule derived it: it is in no provenance. The refusal says so, and
/// names only the rule whose predicate holds for this pair — `same_city`
/// has the same edge type and labels and does not match these two.
///
/// Like the test above, this never retracts (ledger row 37).
#[test]
fn a_hand_written_fact_a_rule_would_rederive_is_refused_without_a_false_owner() {
    let mut db = store("hand-written");
    for (key, city) in [("a", "Oslo"), ("b", "Lima")] {
        db.insert_node(
            "Person",
            key,
            vec![
                ("team".into(), Value::Str("red".into())),
                ("city".into(), Value::Str(city.into())),
            ],
        )
        .unwrap();
    }
    // The edge first, by hand; the rules after it.
    db.insert_edge("LINKED", "a", "b").unwrap();
    let mut by_city = same_team_rule();
    by_city.name = "same_city".into();
    by_city.predicate = Predicate::FieldEqual {
        field: "city".into(),
    };
    by_city.edge_type = "LINKED".into();
    let mut by_team = same_team_rule();
    by_team.edge_type = "LINKED".into();
    db.create_rule(by_city).unwrap();
    db.create_rule(by_team).unwrap();
    assert!(
        db.explain("a", "b")
            .unwrap()
            .iter()
            .all(|e| !(e.edge_type == "LINKED" && e.src_key == "a" && e.dst_key == "b")),
        "fixture: no rule derived a → b, so provenance has nothing to say"
    );

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
            "refused: LINKED a → b was written by hand and no rule derived it, but rule \
             same_team would derive it again, so the delete is refused. It can be deleted \
             once the fields that rule reads (team) no longer match, or once the rule is \
             deleted. Nothing was written.",
        ),
        other => panic!("expected a RuleOwned refusal, got {other:?}"),
    }
    assert_eq!(
        db.neighbors("a", "LINKED", Direction::Out).unwrap(),
        vec!["b".to_string()],
        "nothing was written"
    );
}

/// Two `Person` nodes on team red, and nothing else.
fn two_reds(db: &mut Db) {
    for key in ["a", "b"] {
        db.insert_node(
            "Person",
            key,
            vec![("team".into(), Value::Str("red".into()))],
        )
        .unwrap();
    }
}

/// `same_team` deriving `LINKED`, through a `Team` node one `MEMBER_OF` hop
/// from the source.
fn via_team_rule() -> RuleDef {
    let mut rule = same_team_rule();
    rule.name = "via_team".into();
    rule.edge_type = "LINKED".into();
    rule.via_label = Some("Team".into());
    rule.via_edge = Some("MEMBER_OF".into());
    rule
}

fn linked_a_b() -> ForgetTarget {
    ForgetTarget::Fact {
        subject: "a".into(),
        predicate: "LINKED".into(),
        object: "b".into(),
    }
}

fn refusal(db: &mut Db) -> String {
    match forget(db, &linked_a_b()) {
        Err(GraphError::RuleOwned { detail }) => detail,
        other => panic!("expected a RuleOwned refusal, got {other:?}"),
    }
}

/// The engine's delete guard refuses before it looks for the edge (ledger row
/// 67), so a fact that was never stated can come back `RuleOwned`. `forget`
/// answers what it answers for any absent fact: nothing to retract, nothing
/// written — not a refusal that says the edge was written by hand.
#[test]
fn a_fact_that_does_not_exist_is_nothing_to_retract_even_when_a_rule_matches_the_pair() {
    let mut db = store("absent-rule");
    two_reds(&mut db);
    db.create_rule(via_team_rule()).unwrap();
    assert!(
        db.neighbors("a", "LINKED", Direction::Out)
            .unwrap()
            .is_empty(),
        "fixture: no Team node, so the rule derived nothing"
    );
    let commits = db.commit_seq();

    let report = forget(&mut db, &linked_a_b()).expect("absent, not refused");
    assert!(!report.changed, "{report:?}");
    assert_eq!(report.mode, "fact");
    assert_eq!(report.target, "LINKED a → b");
    assert_eq!(report.notes_total, 0);
    assert_eq!(db.commit_seq(), commits, "nothing was written");

    // Exactly the report an absent fact gets in a store with no rule at all.
    let mut plain = store("absent-plain");
    two_reds(&mut plain);
    let expected = forget(&mut plain, &linked_a_b()).unwrap();
    assert_eq!(report, expected);
}

/// A via-hop rule with no via node would not derive the edge. The engine's
/// guard refuses all the same — it evaluates the predicate on the pair and
/// never looks at the hop — so the refusal names the rule as the reason
/// without saying it would derive anything.
#[test]
fn a_via_rule_with_no_via_node_is_not_said_to_derive_the_hand_written_fact() {
    let mut db = store("via-missing");
    two_reds(&mut db);
    db.insert_edge("LINKED", "a", "b").unwrap();
    db.create_rule(via_team_rule()).unwrap();

    assert_eq!(
        refusal(&mut db),
        "refused: LINKED a → b was written by hand and no rule derived it, but rule \
         via_team matches these two nodes' properties and may derive it again, so the \
         delete is refused. It can be deleted once the fields that rule reads (team) no \
         longer match, or once the rule is deleted. Nothing was written."
    );
}

/// The same rule with its hop in place — a `Team` node the source belongs to,
/// satisfying the predicate against the destination — would derive the edge,
/// and the refusal says so.
#[test]
fn a_via_rule_whose_hop_holds_is_said_to_derive_the_hand_written_fact() {
    let mut db = store("via-present");
    two_reds(&mut db);
    db.insert_edge("LINKED", "a", "b").unwrap();
    db.insert_node("Team", "t", vec![("team".into(), Value::Str("red".into()))])
        .unwrap();
    db.insert_edge("MEMBER_OF", "a", "t").unwrap();
    db.create_rule(via_team_rule()).unwrap();
    assert!(
        db.explain("a", "b").unwrap().is_empty(),
        "fixture: the edge was there first, so it is in no provenance"
    );

    assert_eq!(
        refusal(&mut db),
        "refused: LINKED a → b was written by hand and no rule derived it, but rule \
         via_team would derive it again, so the delete is refused. It can be deleted \
         once the fields that rule reads (team) no longer match, or once the rule is \
         deleted. Nothing was written."
    );
}

/// A rule scoped to a namespace sees neither node outside it and derives
/// nothing between them; the guard does not look at the namespace either.
#[test]
fn a_rule_scoped_to_another_namespace_is_not_said_to_derive_the_hand_written_fact() {
    let mut db = store("other-namespace");
    two_reds(&mut db);
    db.insert_edge("LINKED", "a", "b").unwrap();
    let mut scoped = same_team_rule();
    scoped.edge_type = "LINKED".into();
    scoped.namespace = Some("tenant".into());
    db.create_rule(scoped).unwrap();

    let detail = refusal(&mut db);
    assert!(
        detail.contains("rule same_team matches these two nodes' properties and may derive it")
            && !detail.contains("would derive"),
        "{detail}"
    );
}

/// A rule keeps its best `max_edges` targets per source. With more candidates
/// than that, whether this pair is among them is not something the predicate
/// answers, so the refusal does not say the rule would derive it.
#[test]
fn a_rule_whose_per_source_cap_could_bind_is_not_said_to_derive_the_hand_written_fact() {
    let mut db = store("cap-binds");
    two_reds(&mut db);
    db.insert_edge("LINKED", "a", "b").unwrap();
    for key in ["c", "d"] {
        db.insert_node(
            "Person",
            key,
            vec![("team".into(), Value::Str("blue".into()))],
        )
        .unwrap();
    }
    let mut capped = same_team_rule();
    capped.edge_type = "LINKED".into();
    capped.max_edges = Some(1);
    db.create_rule(capped).unwrap();
    assert!(
        db.explain("a", "b")
            .unwrap()
            .iter()
            .all(|e| e.src_key != "a"),
        "fixture: a → b was there first, so it is in no provenance"
    );

    let detail = refusal(&mut db);
    assert!(
        detail.contains("rule same_team matches these two nodes' properties and may derive it")
            && !detail.contains("would derive"),
        "{detail}"
    );
}
