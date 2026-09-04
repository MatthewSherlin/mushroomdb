//! Rebuilding a via-hop rule.
//!
//! A via-hop rule evaluates its predicate between the *via* node and the
//! destination, which the candidate index cannot express. Every path that
//! recomputes a rule from scratch — `rebuild_rule`, the sibling rebuild inside
//! `delete_rule`, and the top-k backfill a node deletion triggers — has to take
//! the via path, or it computes an empty desired set and retracts edges the rule
//! legitimately owns.
use core_api::{Direction, GraphDb, Predicate, RuleDef, Value};
use core_storage::fs::RealFs;

type Db = GraphDb<RealFs>;

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("via-rebuild-{name}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn strs(v: &[&str]) -> Value {
    Value::List(v.iter().map(|s| Value::Str((*s).into())).collect())
}

fn knows_rule(max_edges: Option<u64>) -> RuleDef {
    RuleDef {
        name: "knows".into(),
        src_label: "Author".into(),
        dst_label: "File".into(),
        predicate: Predicate::Overlap {
            field: "commits".into(),
            min: 0.5,
        },
        edge_type: "KNOWS".into(),
        weight_prop: Some("score".into()),
        max_edges,
        approximate: false,
        via_label: Some("File".into()),
        via_edge: Some("TOP_AUTHOR".into()),
        via_dir: Some(Direction::In),
    }
}

/// Authors and Files; `File.top_author_id` → Author by KeyMatch (`TOP_AUTHOR`);
/// `knows`: Author → File hopping over `TOP_AUTHOR` (In) with Overlap on
/// `commits`. `docs_owner` decides who owns the third file, which is what makes
/// it participate or not.
fn seed(db: &mut Db, max_edges: Option<u64>, docs_owner: &str, docs_commits: &[&str]) {
    db.insert_node("Author", "alice", vec![]).unwrap();
    db.insert_node("Author", "bob", vec![]).unwrap();
    for key in ["api", "model"] {
        db.insert_node(
            "File",
            key,
            vec![
                ("commits".into(), strs(&["c1", "c2", "c3"])),
                ("top_author_id".into(), Value::Str("alice".into())),
            ],
        )
        .unwrap();
    }
    db.insert_node(
        "File",
        "docs",
        vec![
            ("commits".into(), strs(docs_commits)),
            ("top_author_id".into(), Value::Str(docs_owner.into())),
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
    db.create_rule(knows_rule(max_edges)).unwrap();
}

/// The default fixture: `docs` is owned by a key with no Author node and shares
/// no commits, so it never participates.
fn seed_default(db: &mut Db, max_edges: Option<u64>) {
    seed(db, max_edges, "carol", &["c9"]);
}

fn knows(db: &Db, a: &str) -> Vec<String> {
    let mut v = db.neighbors(a, "KNOWS", Direction::Out).unwrap();
    v.sort();
    v
}

fn rule_edges(db: &Db, rule: &str) -> u64 {
    db.stats()
        .rules
        .iter()
        .find(|r| r.name == rule)
        .map(|r| r.edges)
        .unwrap_or_else(|| panic!("rule {rule} not found"))
}

#[test]
fn rebuild_of_a_via_hop_rule_preserves_its_edges() {
    let dir = tmp("topk");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_default(&mut db, Some(10));
    assert_eq!(knows(&db, "alice"), vec!["api", "model"]);
    let edges_before = db.edge_count();

    db.rebuild_rule("knows").unwrap();

    assert_eq!(
        knows(&db, "alice"),
        vec!["api", "model"],
        "rebuild must reproduce what incremental evaluation derived"
    );
    assert_eq!(rule_edges(&db, "knows"), 2);
    assert_eq!(db.edge_count(), edges_before);
    // The weight the rule stores must survive the rebuild too.
    let ex = db.explain("alice", "api").unwrap();
    let k = ex.iter().find(|e| e.rule == "knows").expect("knows entry");
    assert_eq!(k.weight, Some(1.0));
    assert_eq!(k.via_edge.as_deref(), Some("TOP_AUTHOR"));
}

#[test]
fn rebuild_of_an_uncapped_via_hop_rule_preserves_its_edges() {
    // `max_edges: None` takes the global-budget rebuild path, a different branch
    // from the top-k one above.
    let dir = tmp("uncapped");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_default(&mut db, None);
    assert_eq!(knows(&db, "alice"), vec!["api", "model"]);

    db.rebuild_rule("knows").unwrap();

    assert_eq!(knows(&db, "alice"), vec!["api", "model"]);
    assert_eq!(rule_edges(&db, "knows"), 2);
    // Rebuild is the only exit from the tripped latch; it must not set it here.
    assert!(!db.stats().rules.iter().any(|r| r.tripped));
}

#[test]
fn rebuild_of_a_via_hop_rule_still_retracts_what_is_no_longer_derived() {
    // Rebuild is a full recompute, not a no-op: state that drifted out from
    // under the rule has to come out.
    let dir = tmp("retracts");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_default(&mut db, None);
    assert_eq!(knows(&db, "alice"), vec!["api", "model"]);

    // Drop the overlap on one file, then rebuild.
    db.set_prop("model", "commits", strs(&["z1"])).unwrap();
    db.rebuild_rule("knows").unwrap();

    assert_eq!(
        knows(&db, "alice"),
        vec!["api", "model"],
        "model still hops through itself; api still overlaps api"
    );
    // Now alice owns nothing that overlaps: retract everything.
    db.set_prop("api", "top_author_id", Value::Str("carol".into()))
        .unwrap();
    db.set_prop("model", "top_author_id", Value::Str("carol".into()))
        .unwrap();
    db.rebuild_rule("knows").unwrap();
    assert_eq!(knows(&db, "alice"), Vec::<String>::new());
    assert_eq!(rule_edges(&db, "knows"), 0);
}

#[test]
fn deleting_a_sibling_rule_rebuilds_the_via_rule_correctly() {
    // `delete_rule` rebuilds every surviving rule that shares the deleted rule's
    // edge type, so deleting an unrelated KNOWS producer must not cost `knows`
    // its edges.
    let dir = tmp("sibling");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_default(&mut db, Some(10));
    db.create_rule(RuleDef {
        name: "knows_manual".into(),
        src_label: "Author".into(),
        dst_label: "File".into(),
        // No node carries this field, so this rule derives nothing.
        predicate: Predicate::FieldEqual {
            field: "unshared".into(),
        },
        edge_type: "KNOWS".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    assert_eq!(knows(&db, "alice"), vec!["api", "model"]);

    db.delete_rule("knows_manual").unwrap();

    assert_eq!(
        knows(&db, "alice"),
        vec!["api", "model"],
        "the sibling rebuild must go through the via path"
    );
    assert_eq!(rule_edges(&db, "knows"), 2);
}

#[test]
fn via_rebuild_respects_top_k() {
    let dir = tmp("k1");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_default(&mut db, Some(1));
    // Both files score 1.0, so the cap is resolved by the deterministic
    // tie-break: BTree order of the destination key.
    assert_eq!(knows(&db, "alice"), vec!["api"]);

    db.rebuild_rule("knows").unwrap();

    assert_eq!(
        knows(&db, "alice"),
        vec!["api"],
        "same edge, same tie-break"
    );
    assert_eq!(rule_edges(&db, "knows"), 1);
}

#[test]
fn delete_node_top_k_backfill_does_not_retract_unrelated_sources() {
    // `docs` overlaps everything and is owned by bob, so both authors derive a
    // KNOWS edge to it. Deleting it puts BOTH of them in the delete-time top-k
    // backfill, which recomputes each source's whole set — through the via path,
    // or alice loses edges that have nothing to do with `docs`.
    let dir = tmp("delete-node");
    let mut db = GraphDb::open(&dir).unwrap();
    seed(&mut db, Some(10), "bob", &["c1", "c2", "c3"]);
    assert_eq!(knows(&db, "alice"), vec!["api", "docs", "model"]);
    assert_eq!(knows(&db, "bob"), vec!["api", "docs", "model"]);

    db.delete_node("docs").unwrap();

    assert_eq!(
        knows(&db, "alice"),
        vec!["api", "model"],
        "alice keeps everything that does not involve docs"
    );
    assert_eq!(
        knows(&db, "bob"),
        Vec::<String>::new(),
        "bob owned only docs, so his hop is gone"
    );
    let s = db.stats();
    assert_eq!(
        s.edges,
        s.rules.iter().map(|r| r.edges).sum::<u64>(),
        "topology and provenance must agree"
    );
}

#[test]
fn via_rebuild_survives_wal_replay() {
    let dir = tmp("replay");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        seed_default(&mut db, Some(10));
        db.rebuild_rule("knows").unwrap();
        assert_eq!(knows(&db, "alice"), vec!["api", "model"]);
    }
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(knows(&db, "alice"), vec!["api", "model"], "after replay");
    assert_eq!(rule_edges(&db, "knows"), 2);
    drop(db);
    let mut db = GraphDb::open(&dir).unwrap();
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        knows(&db, "alice"),
        vec!["api", "model"],
        "after snapshot roundtrip"
    );
    assert_eq!(rule_edges(&db, "knows"), 2);
}

/// Every via-hop score, not just every via-hop edge, survives a reopen.
///
/// `compute_desired_via` stops scanning via nodes once one of them scores 1.0,
/// which an `Overlap` predicate cannot exceed. The stopping point depends on the
/// order the via nodes come back in, so if the early exit could ever settle on a
/// different maximum, the score written while the store was live and the score
/// recomputed during WAL replay would disagree. They must not.
#[test]
fn via_hop_overlap_scores_are_the_same_after_replay_and_after_snapshot() {
    let dir = tmp("overlap-scores");
    // Partial overlaps, so the scores are spread out rather than all 1.0 —
    // an early exit that fired too eagerly would show up as a changed score.
    // `docs` and `extra` belong to bob, so they are destinations alice reaches
    // only through her own files. A destination that is also one of the source's
    // via nodes scores 1.0 against itself, which would hide how the maximum was
    // reached.
    //
    // alice's two via files carry different commit sets, and the second scores
    // higher than the first against both of bob's files. The maximum is
    // therefore only correct if the scan does not stop at the first via that
    // scores at all.
    let live = {
        let mut db = GraphDb::open(&dir).unwrap();
        seed(&mut db, Some(10), "bob", &["c1", "c2", "c3", "c4", "c5"]);
        db.set_prop("model", "commits", strs(&["c1", "c2", "c3", "c4", "c5"]))
            .unwrap();
        db.insert_node(
            "File",
            "extra",
            vec![
                ("commits".into(), strs(&["c1", "c2", "c3", "c4"])),
                ("top_author_id".into(), Value::Str("bob".into())),
            ],
        )
        .unwrap();
        let live = scores(&db, "alice");
        // api scores 3/5 against docs and model scores 5/5; 4/5 and 3/4 against
        // extra. Pinning them keeps the fixture honest if the seed changes.
        assert_eq!(
            live,
            vec![
                ("api".to_string(), 1.0),
                ("docs".to_string(), 1.0),
                ("extra".to_string(), 0.8),
                ("model".to_string(), 1.0),
            ],
            "the maximum over via nodes, not the first via that scores"
        );
        live
    };

    assert!(
        live.iter().any(|(_, s)| *s < 1.0),
        "the fixture must produce partial overlaps, got {live:?}"
    );

    let replayed = {
        let db = GraphDb::open(&dir).unwrap();
        scores(&db, "alice")
    };
    assert_eq!(replayed, live, "WAL replay must reproduce the live scores");

    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.snapshot().unwrap();
    }
    let from_snapshot = {
        let db = GraphDb::open(&dir).unwrap();
        scores(&db, "alice")
    };
    assert_eq!(
        from_snapshot, live,
        "a snapshot roundtrip must reproduce the live scores"
    );
}

/// `alice`'s KNOWS edges as (destination, score), destination order.
fn scores(db: &Db, a: &str) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = db
        .neighbors(a, "KNOWS", Direction::Out)
        .unwrap()
        .into_iter()
        .map(|d| {
            let ex = db.explain(a, &d).unwrap();
            let s = ex
                .iter()
                .find(|e| e.rule == "knows")
                .and_then(|e| e.weight)
                .unwrap_or_else(|| panic!("KNOWS {a}->{d} reports no score"));
            (d, s)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ── The candidate index must be built before anything probes it ──────────────
//
// `indexes_populated` is a single flag for the whole engine, but `create_rule`
// builds only the index of the rule it is creating. A store opened from a
// snapshot with an empty WAL has every index empty, because the lazy-init guard
// that fills them runs on the first mutation and `create_rule` is not one. If
// creating a rule marks the engine ready anyway, the next property write probes
// indexes that were never built, finds no candidates, and retracts the derived
// edges of every rule that existed before.
//
// Both a via-hop rule and a plain one are covered: the defect is in the flag,
// not in either rule shape.

/// A rule over labels no node carries, so creating it derives nothing and the
/// only thing it changes is the engine's idea of whether indexes are built.
fn inert_rule() -> RuleDef {
    RuleDef {
        name: "inert".into(),
        src_label: "Zeta".into(),
        dst_label: "Zeta".into(),
        predicate: Predicate::KeyMatch {
            field: "zeta_id".into(),
        },
        edge_type: "ZETA".into(),
        weight_prop: None,
        max_edges: Some(1),
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
    }
}

#[test]
fn creating_a_rule_does_not_strand_a_via_hop_rules_edges() {
    let dir = tmp("create-rule-via");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        seed_default(&mut db, Some(10));
        assert_eq!(knows(&db, "alice"), vec!["api", "model"]);
        // Snapshot leaves the WAL empty, so the reopen below replays nothing
        // and no index is built at open.
        db.snapshot().unwrap();
    }

    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(knows(&db, "alice"), vec!["api", "model"], "after reopen");
    db.create_rule(inert_rule()).unwrap();
    // A watched property on a destination of the via-hop rule.
    db.set_prop("api", "commits", strs(&["c1", "c2", "c3", "c4"]))
        .unwrap();
    assert_eq!(
        knows(&db, "alice"),
        vec!["api", "model"],
        "writing to one destination must not retract the others"
    );
}

#[test]
fn creating_a_rule_does_not_strand_a_plain_rules_edges() {
    let dir = tmp("create-rule-plain");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        // All three files share every commit, so each co-changes with the other
        // two. One source with two destinations is what makes the difference
        // visible: a top-k rule recomputes a source's whole set at once, so an
        // empty index costs it every edge, not just the one being written to.
        seed(&mut db, Some(10), "alice", &["c1", "c2", "c3"]);
        db.create_rule(RuleDef {
            name: "co_changed".into(),
            src_label: "File".into(),
            dst_label: "File".into(),
            predicate: Predicate::Overlap {
                field: "commits".into(),
                min: 0.5,
            },
            edge_type: "CO_CHANGED".into(),
            weight_prop: Some("score".into()),
            max_edges: Some(10),
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
        })
        .unwrap();
        assert_eq!(
            db.neighbors("model", "CO_CHANGED", Direction::Out).unwrap(),
            vec!["api", "docs"]
        );
        db.snapshot().unwrap();
    }

    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.neighbors("model", "CO_CHANGED", Direction::Out).unwrap(),
        vec!["api", "docs"],
        "after reopen"
    );
    db.create_rule(inert_rule()).unwrap();
    // `api` moves to commits nothing else shares, so model no longer co-changes
    // with it. model's edge to `docs` is untouched by that and must survive.
    db.set_prop("api", "commits", strs(&["z1", "z2", "z3"]))
        .unwrap();
    assert_eq!(
        db.neighbors("model", "CO_CHANGED", Direction::Out).unwrap(),
        vec!["docs"],
        "only the edge the write invalidated may retract"
    );
}
#[test]
fn a_write_after_a_snapshot_open_does_not_strand_a_via_hop_rules_edges() {
    let dir = tmp("snapshot-write-via");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        seed_default(&mut db, Some(10));
        db.snapshot().unwrap();
    }
    // No create_rule here: the only thing that happens after the snapshot open
    // is one property write on a destination of the via-hop rule.
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(knows(&db, "alice"), vec!["api", "model"], "after reopen");
    db.set_prop("api", "commits", strs(&["c1", "c2", "c3", "c4"]))
        .unwrap();
    assert_eq!(
        knows(&db, "alice"),
        vec!["api", "model"],
        "a via-hop rule must still see the via edges its snapshot holds"
    );
}

// ── An `Any` predicate holding a KeyMatch cannot be narrowed by the index ────
//
// `KeyMatch` candidates are resolved by id lookup, so the spec it compiles to
// (`ByKey`) offers the index no keys to probe. Under `Any` the FK fast path
// does not apply either, so narrowing the candidate set through the index would
// consider only the destinations the *other* branch reaches, and every
// destination that matches solely through the `KeyMatch` branch would silently
// derive no edge. Such a predicate has to fall back to the full candidate set.

/// Author `alice` owns `api`; the rule hops Author → (TOP_AUTHOR, In) → File
/// and scores that via File against every File.
///
/// `Any([KeyMatch { sibling_id }, FieldEqual { team }])` splits the
/// destinations cleanly: `docs` is named by `api.sibling_id` and shares no
/// team, so only the KeyMatch branch reaches it; `model` shares `api`'s team
/// and is named by nothing, so only the FieldEqual branch reaches it.
fn seed_mixed_any(db: &mut Db) {
    db.insert_node("Author", "alice", vec![]).unwrap();
    db.insert_node(
        "File",
        "api",
        vec![
            ("top_author_id".into(), Value::Str("alice".into())),
            ("sibling_id".into(), Value::Str("docs".into())),
            ("team".into(), Value::Str("core".into())),
        ],
    )
    .unwrap();
    db.insert_node(
        "File",
        "docs",
        vec![("team".into(), Value::Str("other".into()))],
    )
    .unwrap();
    db.insert_node(
        "File",
        "model",
        vec![("team".into(), Value::Str("core".into()))],
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
        name: "mixed".into(),
        src_label: "Author".into(),
        dst_label: "File".into(),
        predicate: Predicate::Any(vec![
            Predicate::KeyMatch {
                field: "sibling_id".into(),
            },
            Predicate::FieldEqual {
                field: "team".into(),
            },
        ]),
        edge_type: "MIXED".into(),
        weight_prop: Some("score".into()),
        max_edges: Some(10),
        approximate: false,
        via_label: Some("File".into()),
        via_edge: Some("TOP_AUTHOR".into()),
        via_dir: Some(Direction::In),
    })
    .unwrap();
}

fn mixed(db: &Db, a: &str) -> Vec<String> {
    let mut v = db.neighbors(a, "MIXED", Direction::Out).unwrap();
    v.sort();
    v
}

#[test]
fn a_via_rule_with_any_of_keymatch_and_field_equal_derives_both_branches() {
    let dir = tmp("via-any-keymatch");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_mixed_any(&mut db);
    assert_eq!(
        mixed(&db, "alice"),
        vec!["api", "docs", "model"],
        "docs is reachable only through the KeyMatch branch and must be derived"
    );

    // A rebuild is a full recompute through the same candidate path.
    db.rebuild_rule("mixed").unwrap();
    assert_eq!(mixed(&db, "alice"), vec!["api", "docs", "model"], "rebuild");

    // Incremental: retract the KeyMatch branch and only `docs` may leave.
    db.set_prop("api", "sibling_id", Value::Str("nobody".into()))
        .unwrap();
    assert_eq!(mixed(&db, "alice"), vec!["api", "model"]);

    // And it comes back when the field names it again.
    db.set_prop("api", "sibling_id", Value::Str("docs".into()))
        .unwrap();
    assert_eq!(mixed(&db, "alice"), vec!["api", "docs", "model"]);
}

#[test]
fn a_via_rule_with_any_of_keymatch_survives_a_snapshot_open() {
    let dir = tmp("via-any-keymatch-snapshot");
    {
        let mut db = GraphDb::open(&dir).unwrap();
        seed_mixed_any(&mut db);
        assert_eq!(mixed(&db, "alice"), vec!["api", "docs", "model"]);
        // Snapshot leaves the WAL empty, so the reopen replays nothing.
        db.snapshot().unwrap();
    }
    let mut db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        mixed(&db, "alice"),
        vec!["api", "docs", "model"],
        "after reopen"
    );
    // One write on a destination must not cost the source its other edges.
    db.set_prop("model", "team", Value::Str("core".into()))
        .unwrap();
    assert_eq!(
        mixed(&db, "alice"),
        vec!["api", "docs", "model"],
        "the KeyMatch branch must survive a write that only touches the other"
    );
}
