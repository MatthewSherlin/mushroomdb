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
