//! Rule chaining: a derived edge feeding a via-hop rule re-derives that rule's
//! edges in the same write, deterministically, with a depth cap and no cycles.
use core_api::{
    Direction, EdgeEvent, GraphDb, GraphError, Predicate, RuleDef, Value, ViewDef, ViewSource,
};
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
    assert_eq!(
        db.stats().chain_truncations,
        0,
        "backfill alone reaches a fixpoint"
    );
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
    // The ceiling, pinned: E5 is one level past the cap, so it is NOT retracted
    // and still points at a via hop that no longer exists. Raising the cap would
    // clear it and fail here — this is what fixes the cap at exactly 4 rather
    // than "at least 4".
    assert!(
        !db.neighbors("n0", "E5", Direction::Out).unwrap().is_empty(),
        "E5 is beyond the cap and must be left stale"
    );
    assert_eq!(
        db.stats().chain_truncations,
        1,
        "the truncated write must be counted"
    );
    // Restore the root: the same chain re-derives every level within the cap,
    // and truncates in the same place.
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
    assert_eq!(db.stats().chain_truncations, 2);
    // The counter is not persisted; it is re-accumulated by replay, which runs
    // the identical hooks and therefore truncates in the identical places.
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.stats().chain_truncations,
        2,
        "replay must reproduce the same truncations"
    );
    assert!(
        !db.neighbors("n0", "E5", Direction::Out).unwrap().is_empty(),
        "and the same stale tail"
    );
}

#[test]
fn a_later_level_can_refire_a_rule_an_earlier_level_already_ran() {
    // Two producers of the same edge type. `x_rule` writes X at level 0;
    // `a_rule` (hopping over Y) writes a SECOND X edge at level 2. `r_rule`
    // hops over X and must see both — recomputing at level 1 off the first X
    // does not excuse it from recomputing at level 2 off the second.
    let dir = tmp("two-producers");
    let mut db = GraphDb::open(&dir).unwrap();
    let t = |k: &str| {
        vec![
            ("k".into(), Value::Str(k.into())),
            ("grp".into(), Value::Str("g".into())),
        ]
    };
    db.insert_node("S", "s", vec![("k".into(), Value::Str("a".into()))])
        .unwrap();
    db.insert_node("T", "t1", t("a")).unwrap();
    db.insert_node("T", "t2", t("b")).unwrap();
    db.insert_node("U", "u1", vec![("k".into(), Value::Str("a".into()))])
        .unwrap();
    db.insert_node("U", "u2", vec![("k".into(), Value::Str("b".into()))])
        .unwrap();
    let plain = |name: &str, edge: &str| RuleDef {
        name: name.into(),
        src_label: "S".into(),
        dst_label: "T".into(),
        predicate: Predicate::KeyMatch {
            field: "link".into(),
        },
        edge_type: edge.into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    };
    // Rule iteration is BTree name order, so at level 0 the X delta is appended
    // before the Y delta. That is the order that exposes the bug.
    db.create_rule(plain("x_rule", "X")).unwrap();
    db.create_rule(plain("y_rule", "Y")).unwrap();
    // Hops over Y, writes X: the second producer.
    db.create_rule(RuleDef {
        name: "a_rule".into(),
        src_label: "S".into(),
        dst_label: "T".into(),
        predicate: Predicate::FieldEqual {
            field: "grp".into(),
        },
        edge_type: "X".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("T".into()),
        via_edge: Some("Y".into()),
        via_dir: Some(Direction::Out),
    })
    .unwrap();
    // Hops over X, writes Z. Its result depends on WHICH T nodes it reaches.
    db.create_rule(RuleDef {
        name: "r_rule".into(),
        src_label: "S".into(),
        dst_label: "U".into(),
        predicate: Predicate::FieldEqual { field: "k".into() },
        edge_type: "Z".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("T".into()),
        via_edge: Some("X".into()),
        via_dir: Some(Direction::Out),
    })
    .unwrap();
    assert!(db.neighbors("s", "Z", Direction::Out).unwrap().is_empty());

    db.set_prop("s", "link", Value::Str("t1".into())).unwrap();

    let mut x = db.neighbors("s", "X", Direction::Out).unwrap();
    x.sort();
    assert_eq!(x, vec!["t1", "t2"], "both producers of X must have run");
    let mut z = db.neighbors("s", "Z", Direction::Out).unwrap();
    z.sort();
    assert_eq!(
        z,
        vec!["u1", "u2"],
        "u2 is only reachable through the X edge written at level 2"
    );
}

#[test]
fn deleting_a_node_does_not_let_the_chain_re_derive_edges_onto_it() {
    // `model` is simultaneously a via node for `knows` (alice hops through it)
    // and one of `knows`'s destination candidates. Deleting it retracts
    // TOP_AUTHOR(model→alice), which chains into `knows` — while `model` still
    // carries its label and props. Nothing may re-derive an edge onto it: the
    // caller strips topology afterwards but never provenance, so a re-derived
    // edge would leak forever and lie in edge history.
    let dir = tmp("delete-node");
    let mut db = GraphDb::open(&dir).unwrap();
    seed(&mut db);
    assert_eq!(knows(&db, "alice"), vec!["api", "model"]);
    // `edge_history` counts WAL frames, which include the derived-edge markers,
    // so this is not `commit_seq`.
    let before_delete = db.edge_history("alice", "model").unwrap().total_commits;

    db.delete_node("model").unwrap();

    let check = |db: &Db, label: &str| {
        assert_eq!(knows(db, "alice"), vec!["api"], "{label}: live KNOWS");
        let s = db.stats();
        let per_rule: Vec<(String, u64)> =
            s.rules.iter().map(|r| (r.name.clone(), r.edges)).collect();
        assert_eq!(
            per_rule,
            vec![("knows".to_string(), 1), ("top_author".to_string(), 1)],
            "{label}: provenance must match the live topology, with no leak"
        );
        // Every edge in this fixture is rule-derived, so the two must agree.
        assert_eq!(
            s.edges,
            s.rules.iter().map(|r| r.edges).sum::<u64>(),
            "{label}: topology and provenance edge counts must agree"
        );
    };
    // History is WAL-bounded, so it is only meaningful before the snapshot
    // compacts the frames these events live in.
    let check_history = |db: &Db, label: &str| {
        let hist = db.edge_history("alice", "model").unwrap();
        let bogus: Vec<_> = hist
            .items
            .iter()
            .filter(|e| e.event == EdgeEvent::Added && e.commit >= before_delete)
            .collect();
        assert!(
            bogus.is_empty(),
            "{label}: no edge may be added to a deleted node, got {bogus:?}"
        );
        assert_eq!(
            hist.items.last().map(|e| &e.event),
            Some(&EdgeEvent::Retracted),
            "{label}: the pair's last recorded event must be a retraction"
        );
        let now = hist.total_commits - 1;
        assert!(
            !db.was_linked("alice", "model", "KNOWS", now).unwrap(),
            "{label}: a deleted node must not read as linked"
        );
    };
    check(&db, "live");
    check_history(&db, "live");
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    check(&db, "after wal replay");
    check_history(&db, "after wal replay");
    drop(db);
    let mut db = GraphDb::open(&dir).unwrap();
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    check(&db, "after snapshot roundtrip");
}

#[test]
fn a_batch_cannot_assemble_a_cycle_one_rule_at_a_time() {
    let dir = tmp("batch-cycle");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("k".into(), Value::Str("x".into()))])
        .unwrap();
    db.insert_node("B", "b", vec![("k".into(), Value::Str("x".into()))])
        .unwrap();
    let hop = |name: &str, via: &str, writes: &str| RuleDef {
        name: name.into(),
        src_label: "A".into(),
        dst_label: "B".into(),
        predicate: Predicate::FieldEqual { field: "k".into() },
        edge_type: writes.into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: Some("B".into()),
        via_edge: Some(via.into()),
        via_dir: Some(Direction::Out),
    };
    // Neither rule closes a cycle on its own; together they do.
    let err = db
        .batch()
        .create_rule(hop("first", "X", "Y"))
        .create_rule(hop("second", "Y", "X"))
        .commit()
        .unwrap_err();
    match err {
        GraphError::RuleInvalid { detail } => {
            assert_eq!(detail, "rule chain cycle: Y -> X -> Y")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(db.rules().len(), 0, "a rejected batch commits nothing");
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.rules().len(), 0, "and nothing was logged");
}

#[test]
fn a_view_over_a_chained_edge_type_updates_exactly_once_per_chained_delta() {
    // `authored` counts KNOWS edges, which only ever change through the chain.
    // If chained deltas were dropped the view would stay at its backfill value;
    // if they were applied twice the count would drift off the topology.
    let dir = tmp("view");
    let mut db = GraphDb::open(&dir).unwrap();
    seed(&mut db);
    db.create_view(ViewDef {
        name: "knows_out".into(),
        label: "Author".into(),
        view_prop: "known_files".into(),
        source: ViewSource::Degree {
            edge_type: "KNOWS".into(),
            direction: Direction::Out,
        },
    })
    .unwrap();
    let deg = |db: &Db, who: &str| db.get_prop(who, "known_files");
    assert_eq!(deg(&db, "alice"), Some(Value::Int(2)));
    assert_eq!(deg(&db, "bob"), Some(Value::Int(0)));

    // One SET: two KNOWS fires for bob, none retracted for alice.
    db.set_prop("api", "top_author_id", Value::Str("bob".into()))
        .unwrap();
    assert_eq!(deg(&db, "bob"), Some(Value::Int(2)));
    assert_eq!(deg(&db, "alice"), Some(Value::Int(2)));

    // One SET: alice's two KNOWS edges retract.
    db.set_prop("model", "top_author_id", Value::Str("bob".into()))
        .unwrap();
    assert_eq!(deg(&db, "alice"), Some(Value::Int(0)));
    assert_eq!(deg(&db, "bob"), Some(Value::Int(2)));

    // The view value must equal the live topology, not an accumulated drift.
    for who in ["alice", "bob"] {
        assert_eq!(
            deg(&db, who),
            Some(Value::Int(knows(&db, who).len() as i64)),
            "{who}"
        );
    }
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    for who in ["alice", "bob"] {
        assert_eq!(
            db.get_prop(who, "known_files"),
            Some(Value::Int(knows(&db, who).len() as i64)),
            "{who} after replay"
        );
    }
}

#[test]
fn create_rule_backfill_chains_into_an_existing_via_hop_rule() {
    // The reverse creation order from `seed`: the consumer exists first, so the
    // producer's backfill is what has to feed it.
    let dir = tmp("backfill-chain");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Author", "alice", vec![]).unwrap();
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
    assert_eq!(knows(&db, "alice"), Vec::<String>::new(), "no hops yet");

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
    assert_eq!(
        knows(&db, "alice"),
        vec!["api", "model"],
        "the backfill's TOP_AUTHOR edges must feed knows in the same write"
    );
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(knows(&db, "alice"), vec!["api", "model"], "after replay");
}

#[test]
fn deleting_the_upstream_rule_retracts_downstream_edges() {
    // Deleting `top_author` retracts every TOP_AUTHOR edge; `knows` hops over
    // exactly those, so it loses its edges in the same write rather than
    // keeping a set derived from a hop that no longer exists.
    let dir = tmp("delete-upstream");
    let mut db = GraphDb::open(&dir).unwrap();
    seed(&mut db);
    assert_eq!(knows(&db, "alice"), vec!["api", "model"]);
    db.delete_rule("top_author").unwrap();
    assert!(db
        .neighbors("api", "TOP_AUTHOR", Direction::Out)
        .unwrap()
        .is_empty());
    assert_eq!(knows(&db, "alice"), Vec::<String>::new());
    // And the retraction is durable, not just a live-memory effect.
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(knows(&db, "alice"), Vec::<String>::new());
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
