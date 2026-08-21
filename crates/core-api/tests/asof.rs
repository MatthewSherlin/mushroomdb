//! Tests for as-of time travel (Plan-15 Task 2).
//!
//! Covers:
//! - WAL commit counting golden test over a known-history db
//! - open_at at multiple commit points: edge presence/absence, explain content
//! - open_at(latest) == normal open equivalence (same derived set)
//! - Mutation refusal sweep: every mutation surface returns ReadOnly
//! - pending_delta_count == 0 after open_at (mirror of T1's post-loop assert)
//! - Commit out of range returns CommitOutOfRange

use core_api::{Direction, GraphDb, GraphError, Predicate, RuleDef, Value};
use core_storage::wal::wal_commits;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "graphdb-asof-{}-{}-{}",
        name,
        std::process::id(),
        nanos
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Build the known-history database used by multiple tests.
///
/// Commit layout (0-indexed WAL frames):
///   0: InsertNode "a" label="T" tag="x"
///   1: CreateRule "r1" (FieldEqual "tag", T→T→SAME)
///   2: InsertNode "b" label="T" tag="x"  → rule fires: a-[SAME]→b derived
///   3: SetProp "b".tag = "y"             → rule retracts a-[SAME]→b
///   4: DeleteNode "a"
fn build_known_history(dir: &std::path::Path) {
    let mut db = GraphDb::open(dir).unwrap();
    // commit 0
    db.insert_node("T", "a", vec![("tag".into(), Value::Str("x".into()))])
        .unwrap();
    // commit 1
    db.create_rule(RuleDef {
        name: "r1".into(),
        src_label: "T".into(),
        dst_label: "T".into(),
        predicate: Predicate::FieldEqual {
            field: "tag".into(),
        },
        edge_type: "SAME".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    })
    .unwrap();
    // commit 2 — rule fires (a and b share tag="x")
    db.insert_node("T", "b", vec![("tag".into(), Value::Str("x".into()))])
        .unwrap();
    // commit 3 — tag changes, rule retracts
    db.set_prop("b", "tag", Value::Str("y".into())).unwrap();
    // commit 4 — delete a
    db.delete_node("a").unwrap();
}

// ── Golden commit-count test ────────────────────────────────────────────────

/// Pin that wal_commits() counts every WAL frame as one commit (both Batch
/// and single-op), and that the known-history db has exactly 5 commits.
///
/// This is the stable commit-count golden test required by Task 2 scope.
#[test]
fn wal_commits_golden_count() {
    let dir = tmp("commits-golden");
    build_known_history(&dir);
    let bytes = std::fs::read(dir.join("wal.bin")).unwrap();
    let n = wal_commits(&bytes);
    assert_eq!(
        n, 5,
        "known-history db must have exactly 5 WAL frames (commits 0..=4)"
    );
}

// ── open_at at 4 points ─────────────────────────────────────────────────────

#[test]
fn open_at_commit_0_only_node_a() {
    let dir = tmp("at-0");
    build_known_history(&dir);

    let db = GraphDb::open_at(&dir, 0).unwrap();
    assert!(db.has_node("a"), "commit 0: node a must exist");
    assert!(!db.has_node("b"), "commit 0: node b not yet inserted");
    assert!(
        db.rules().is_empty(),
        "commit 0: no rules created yet"
    );
    let nbrs = db.neighbors("a", "SAME", Direction::Out).unwrap();
    assert!(nbrs.is_empty(), "commit 0: no edges");
    assert!(db.is_read_only(), "open_at result must be read-only");
    assert_eq!(
        db.total_wal_commits(),
        5,
        "total_wal_commits must reflect the full WAL count"
    );
}

#[test]
fn open_at_commit_2_edge_present_explain_shows_rule() {
    let dir = tmp("at-2");
    build_known_history(&dir);

    let db = GraphDb::open_at(&dir, 2).unwrap();
    assert!(db.has_node("a"), "commit 2: a exists");
    assert!(db.has_node("b"), "commit 2: b inserted");

    let nbrs = db.neighbors("a", "SAME", Direction::Out).unwrap();
    assert_eq!(
        nbrs,
        vec!["b".to_string()],
        "commit 2: rule-derived edge a-[SAME]->b must be present"
    );

    // explain() answers "why did this edge exist at T=2"
    let exps = db.explain("a", "b").unwrap();
    assert!(
        !exps.is_empty(),
        "commit 2: explain must find the derived edge"
    );
    assert_eq!(exps[0].rule, "r1");
    assert_eq!(exps[0].edge_type, "SAME");
    assert_eq!(exps[0].src_key, "a");
    assert_eq!(exps[0].dst_key, "b");
}

#[test]
fn open_at_commit_3_edge_retracted() {
    let dir = tmp("at-3");
    build_known_history(&dir);

    let db = GraphDb::open_at(&dir, 3).unwrap();
    assert!(db.has_node("a"), "commit 3: a exists");
    assert!(db.has_node("b"), "commit 3: b exists");

    let nbrs = db.neighbors("a", "SAME", Direction::Out).unwrap();
    assert!(
        nbrs.is_empty(),
        "commit 3: edge must be retracted after tag change"
    );

    // explain returns empty for a pair with no derived edge
    let exps = db.explain("a", "b").unwrap();
    assert!(
        exps.is_empty(),
        "commit 3: no derived edge, explain must be empty"
    );
}

#[test]
fn open_at_commit_4_node_a_deleted() {
    let dir = tmp("at-4");
    build_known_history(&dir);

    let db = GraphDb::open_at(&dir, 4).unwrap();
    assert!(
        !db.has_node("a"),
        "commit 4: node a must be deleted"
    );
    assert!(db.has_node("b"), "commit 4: node b still present");
}

// ── open_at(latest) == normal open equivalence ──────────────────────────────

#[test]
fn open_at_latest_equivalent_to_open() {
    let dir = tmp("at-latest");
    build_known_history(&dir);

    let total = wal_commits(&std::fs::read(dir.join("wal.bin")).unwrap());
    let latest = total - 1;

    let at = GraphDb::open_at(&dir, latest).unwrap();
    let normal = GraphDb::open(&dir).unwrap();

    assert_eq!(
        at.has_node("a"),
        normal.has_node("a"),
        "open_at(latest) and open must agree on node a"
    );
    assert_eq!(
        at.has_node("b"),
        normal.has_node("b"),
        "open_at(latest) and open must agree on node b"
    );
    assert_eq!(
        at.stats().edges,
        normal.stats().edges,
        "open_at(latest) and open must have the same edge count"
    );
    assert_eq!(
        at.stats().nodes_live,
        normal.stats().nodes_live,
        "open_at(latest) and open must have the same live node count"
    );
}

// ── Commit out of range ──────────────────────────────────────────────────────

#[test]
fn open_at_out_of_range_returns_error() {
    let dir = tmp("oor");
    build_known_history(&dir);

    // commit 5 is one past the last valid (0..=4)
    let err = GraphDb::open_at(&dir, 5).err().expect("should err");
    match err {
        GraphError::CommitOutOfRange { commit: 5, total: 5 } => {}
        other => panic!("expected CommitOutOfRange{{5,5}}, got {other:?}"),
    }

    // Empty db: any commit is out of range
    let dir2 = tmp("oor-empty");
    let _db = GraphDb::open(&dir2).unwrap(); // creates dir, no WAL writes
    let err2 = GraphDb::open_at(&dir2, 0).err().expect("should err");
    match err2 {
        GraphError::CommitOutOfRange { commit: 0, total: 0 } => {}
        other => panic!("expected CommitOutOfRange{{0,0}} for empty WAL, got {other:?}"),
    }
}

// ── pending_delta_count == 0 after open_at ──────────────────────────────────

/// Pin: open_at drains like open does.  The debug_assert inside open_at_with
/// fires in debug builds; this test additionally verifies read-only behaviour
/// is set correctly, confirming the drain-then-seal sequence ran.
#[test]
fn pending_delta_zero_after_open_at() {
    let dir = tmp("drain-pin");
    build_known_history(&dir);

    // At commit 2 the rule has fired and deltas were drained inside the loop.
    let db = GraphDb::open_at(&dir, 2).unwrap();
    // If debug_assert didn't panic, deltas are 0.  Verify read-only seal:
    assert!(db.is_read_only(), "after open_at the db must be read-only");
}

// ── Mutation refusal sweep ───────────────────────────────────────────────────

/// Every public mutation method on an as-of instance must return ReadOnly.
#[test]
fn mutation_refusal_sweep() {
    let dir = tmp("refusal");
    build_known_history(&dir);

    let mut db = GraphDb::open_at(&dir, 2).unwrap();

    // insert_node
    let e = db
        .insert_node("T", "new", vec![])
        .unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "insert_node must return ReadOnly, got {e:?}"
    );

    // set_prop (b exists at commit 2)
    let e = db.set_prop("b", "tag", Value::Str("z".into())).unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "set_prop must return ReadOnly, got {e:?}"
    );

    // remove_prop
    let e = db.remove_prop("b", "tag").unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "remove_prop must return ReadOnly, got {e:?}"
    );

    // delete_node
    let e = db.delete_node("b").unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "delete_node must return ReadOnly, got {e:?}"
    );

    // insert_edge (both nodes exist at commit 2; SAME is rule-owned, use OTHER)
    let e = db
        .insert_edge("OTHER", "a", "b")
        .unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "insert_edge must return ReadOnly, got {e:?}"
    );

    // delete_edge (would also fail with RuleOwned for SAME, but ReadOnly comes first)
    let e = db.delete_edge("OTHER", "a", "b").unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "delete_edge must return ReadOnly, got {e:?}"
    );

    // create_rule
    let e = db
        .create_rule(RuleDef {
            name: "r2".into(),
            src_label: "T".into(),
            dst_label: "T".into(),
            predicate: Predicate::FieldEqual {
                field: "tag".into(),
            },
            edge_type: "X".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
        })
        .unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "create_rule must return ReadOnly, got {e:?}"
    );

    // delete_rule (r1 exists at commit 2)
    let e = db.delete_rule("r1").unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "delete_rule must return ReadOnly, got {e:?}"
    );

    // rebuild_rule
    let e = db.rebuild_rule("r1").unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "rebuild_rule must return ReadOnly, got {e:?}"
    );

    // batch().commit() — even empty batch must fail
    let e = db.batch().commit().unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "batch().commit() must return ReadOnly, got {e:?}"
    );

    // write_batch — non-empty
    let e = db
        .write_batch(|b| {
            b.insert_node("T", "x", vec![]);
        })
        .unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "write_batch must return ReadOnly, got {e:?}"
    );

    // snapshot
    let e = db.snapshot().unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "snapshot must return ReadOnly, got {e:?}"
    );
}

// ── Read-only ops work normally ──────────────────────────────────────────────

#[test]
fn read_ops_work_on_as_of_instance() {
    let dir = tmp("read-ok");
    build_known_history(&dir);

    let db = GraphDb::open_at(&dir, 2).unwrap();

    // query
    let params = std::collections::BTreeMap::new();
    let rs = db
        .query("MATCH (n:T) RETURN n", &params)
        .unwrap();
    assert_eq!(rs.len(), 2, "commit 2: two T nodes");

    // stats
    let s = db.stats();
    assert!(s.edges > 0, "commit 2: derived edges present");

    // node_info
    assert!(db.node_info("a").is_some());
    assert!(db.node_info("b").is_some());

    // node_edges
    let edges = db.node_edges("a").unwrap();
    assert!(!edges.is_empty(), "commit 2: a has edges");
}
