//! Rule chaining: a derived edge feeding a via-hop rule re-derives that rule's
//! edges in the same write, deterministically, with a depth cap and no cycles.
use core_api::{Direction, GraphDb, GraphError, Predicate, RuleDef, Value};
use core_storage::fs::RealFs;

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("chaining-{name}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn strs(v: &[&str]) -> Value {
    Value::List(v.iter().map(|s| Value::Str((*s).into())).collect())
}

/// Files with commit lists; Authors; `File.top_author_id` → Author (KeyMatch,
/// derived TOP_AUTHOR); KNOWS: Author → File via TOP_AUTHOR (In) with Overlap
/// on commits.
///
/// `api` and `model` share every commit (jaccard 1.0); `docs` shares none and
/// is owned by an author key that has no node, so it never participates.
fn seed(db: &mut Db) {
    db.insert_node("Author", "alice", vec![]).unwrap();
    db.insert_node("Author", "bob", vec![]).unwrap();
    db.insert_node(
        "File",
        "api",
        vec![
            ("commits".into(), strs(&["c1", "c2", "c3"])),
            ("top_author_id".into(), Value::Str("alice".into())),
        ],
    )
    .unwrap();
    db.insert_node(
        "File",
        "model",
        vec![
            ("commits".into(), strs(&["c1", "c2", "c3"])),
            ("top_author_id".into(), Value::Str("alice".into())),
        ],
    )
    .unwrap();
    db.insert_node(
        "File",
        "docs",
        vec![
            ("commits".into(), strs(&["c9"])),
            ("top_author_id".into(), Value::Str("carol".into())),
        ],
    )
    .unwrap();
    db.create_rule(RuleDef {
        name: "top_author".into(),
        src_label: "File".into(),
        dst_label: "Author".into(),
        predicate: Predicate::KeyMatch {
            field: "top_author_id".into(),
        },
        edge_type: "TOP_AUTHOR".into(),
        weight_prop: None,
        max_edges: Some(1),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    db.create_rule(RuleDef {
        name: "knows".into(),
        src_label: "Author".into(),
        dst_label: "File".into(),
        predicate: Predicate::Overlap {
            field: "commits".into(),
            min: 0.5,
        },
        edge_type: "KNOWS".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(10),
        approximate: false,
        via_label: Some("File".into()),
        via_edge: Some("TOP_AUTHOR".into()),
        via_dir: Some(Direction::In),
    })
    .unwrap();
}

fn knows(db: &Db, a: &str) -> Vec<String> {
    let mut v = db.neighbors(a, "KNOWS", Direction::Out).unwrap();
    v.sort();
    v
}

#[test]
fn derived_via_edge_change_recomputes_dependent_rule_in_same_write() {
    let dir = tmp("basic");
    let mut db = GraphDb::open(&dir).unwrap();
    seed(&mut db);
    assert_eq!(knows(&db, "alice"), vec!["api", "model"]);
    assert_eq!(knows(&db, "bob"), Vec::<String>::new());
    let before = db.commit_seq();
    // One SET: TOP_AUTHOR(api→alice) retracts, TOP_AUTHOR(api→bob) fires, KNOWS follows.
    db.set_prop("api", "top_author_id", Value::Str("bob".into()))
        .unwrap();
    assert_eq!(db.commit_seq(), before + 1, "chaining must not add commits");
    assert_eq!(
        knows(&db, "bob"),
        vec!["api", "model"],
        "bob now knows model through api"
    );
    assert_eq!(
        knows(&db, "alice"),
        vec!["api", "model"],
        "alice still owns model, which co-changes with api"
    );
    db.set_prop("model", "top_author_id", Value::Str("bob".into()))
        .unwrap();
    assert_eq!(
        knows(&db, "alice"),
        Vec::<String>::new(),
        "alice owns nothing → KNOWS retracts"
    );
    let ex = db.explain("bob", "api").unwrap();
    assert_eq!(ex[0].rule, "knows");
    assert_eq!(ex[0].via_edge.as_deref(), Some("TOP_AUTHOR"));
}

#[test]
fn chained_edges_survive_wal_replay_and_snapshot() {
    let dir = tmp("replay");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        seed(&mut db);
        db.set_prop("api", "top_author_id", Value::Str("bob".into()))
            .unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(knows(&db, "bob"), vec!["api", "model"]);
    drop(db);
    let mut db = GraphDb::open(&dir).unwrap();
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(knows(&db, "bob"), vec!["api", "model"]);
    assert_eq!(knows(&db, "alice"), vec!["api", "model"]);
}

#[test]
fn fixpoint_is_independent_of_write_order() {
    let a = tmp("order-a");
    let b = tmp("order-b");
    let mut da = GraphDb::open(&a).unwrap();
    let mut dbb = GraphDb::open(&b).unwrap();
    seed(&mut da);
    seed(&mut dbb);
    da.set_prop("api", "top_author_id", Value::Str("bob".into()))
        .unwrap();
    da.set_prop("model", "top_author_id", Value::Str("bob".into()))
        .unwrap();
    dbb.set_prop("model", "top_author_id", Value::Str("bob".into()))
        .unwrap();
    dbb.set_prop("api", "top_author_id", Value::Str("bob".into()))
        .unwrap();
    for who in ["alice", "bob"] {
        assert_eq!(knows(&da, who), knows(&dbb, who), "{who}");
    }
    assert_eq!(da.edge_count(), dbb.edge_count());
}

#[test]
fn cycle_is_rejected_at_create_rule_with_named_error() {
    let dir = tmp("cycle");
    let mut db = GraphDb::open(&dir).unwrap();
    seed(&mut db);
    // A rule that consumes KNOWS and produces TOP_AUTHOR would loop.
    let err = db
        .create_rule(RuleDef {
            name: "loop".into(),
            src_label: "File".into(),
            dst_label: "Author".into(),
            predicate: Predicate::KeyMatch {
                field: "top_author_id".into(),
            },
            edge_type: "TOP_AUTHOR".into(),
            weight_prop: None,
            max_edges: Some(1),
            approximate: false,
            via_label: Some("Author".into()),
            via_edge: Some("KNOWS".into()),
            via_dir: Some(Direction::In),
        })
        .unwrap_err();
    match err {
        GraphError::RuleInvalid { detail } => {
            assert!(detail.contains("rule chain cycle"), "{detail}");
            // The path names the edge types around the loop, candidate first.
            assert_eq!(detail, "rule chain cycle: KNOWS -> TOP_AUTHOR -> KNOWS");
        }
        other => panic!("{other:?}"),
    }
    let err2 = db
        .create_rule(RuleDef {
            name: "self".into(),
            src_label: "File".into(),
            dst_label: "File".into(),
            predicate: Predicate::FieldEqual {
                field: "dir".into(),
            },
            edge_type: "SAME".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: Some("File".into()),
            via_edge: Some("SAME".into()),
            via_dir: Some(Direction::Out),
        })
        .unwrap_err();
    match err2 {
        GraphError::RuleInvalid { detail } => {
            assert_eq!(detail, "rule chain cycle: SAME -> SAME")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(db.rules().len(), 2, "rejected rules never enter the WAL");
}

#[test]
fn depth_cap_terminates_long_chains() {
    // L0 -KeyMatch-> L1 -via-> L2 -via-> L3 -via-> L4 -via-> L5 -via-> L6:
    // six chained levels, cap 4.
    let dir = tmp("depth");
    let mut db = GraphDb::open(&dir).unwrap();
    for lvl in 0..7 {
        db.insert_node(
            &format!("L{lvl}"),
            &format!("n{lvl}"),
            vec![
                ("k".into(), Value::Str("x".into())),
                ("next_id".into(), Value::Str(format!("n{}", lvl + 1))),
            ],
        )
        .unwrap();
    }
    db.create_rule(RuleDef {
        name: "r0".into(),
        src_label: "L0".into(),
        dst_label: "L1".into(),
        predicate: Predicate::KeyMatch {
            field: "next_id".into(),
        },
        edge_type: "E0".into(),
        weight_prop: None,
        max_edges: Some(1),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    for lvl in 1..6 {
        db.create_rule(RuleDef {
            name: format!("r{lvl}"),
            src_label: "L0".into(),
            dst_label: format!("L{}", lvl + 1),
            predicate: Predicate::FieldEqual { field: "k".into() },
            edge_type: format!("E{lvl}"),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: Some(format!("L{lvl}")),
            via_edge: Some(format!("E{}", lvl - 1)),
            via_dir: Some(Direction::Out),
        })
        .unwrap();
    }
    // Backfill derives every level directly (no chaining involved).
    for lvl in 0..6 {
        assert!(
            !db.neighbors("n0", &format!("E{lvl}"), Direction::Out)
                .unwrap()
                .is_empty(),
            "backfill must derive level {lvl}"
        );
    }
    // Break the root: the retraction chains down exactly MAX_CHAIN_DEPTH levels
    // past the level-0 retract, and terminates.
    db.set_prop("n0", "next_id", Value::Str("none".into()))
        .unwrap();
    for lvl in 0..=core_api::MAX_CHAIN_DEPTH {
        assert!(
            db.neighbors("n0", &format!("E{lvl}"), Direction::Out)
                .unwrap()
                .is_empty(),
            "level {lvl} must retract"
        );
    }
    // Restore the root: the same chain re-derives every level within the cap.
    db.set_prop("n0", "next_id", Value::Str("n1".into()))
        .unwrap();
    for lvl in 0..=core_api::MAX_CHAIN_DEPTH {
        assert!(
            !db.neighbors("n0", &format!("E{lvl}"), Direction::Out)
                .unwrap()
                .is_empty(),
            "level {lvl} must derive"
        );
    }
    // E5 sits beyond the cap; its state is deliberately unasserted.
}

#[test]
fn one_level_rules_are_unchanged() {
    // The existing rules suite is the regression oracle; this pins that a plain
    // rule set produces no chained fires.
    let dir = tmp("plain");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node(
        "Org",
        "o1",
        vec![("industry".into(), Value::Str("x".into()))],
    )
    .unwrap();
    db.insert_node(
        "Person",
        "p1",
        vec![("industry".into(), Value::Str("x".into()))],
    )
    .unwrap();
    db.create_rule(RuleDef {
        name: "same".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::FieldEqual {
            field: "industry".into(),
        },
        edge_type: "SAME".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    db.set_prop("p1", "industry", Value::Str("y".into()))
        .unwrap();
    assert!(db
        .neighbors("p1", "SAME", Direction::Out)
        .unwrap()
        .is_empty());
    assert_eq!(db.edge_count(), 0);
}
