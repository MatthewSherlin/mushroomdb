//! Tests for as-of time travel (Plan-15 Task 2).
//!
//! Covers:
//! - WAL commit counting golden test over a known-history db
//! - open_at at multiple commit points: edge presence/absence, explain content
//! - open_at(latest) == normal open equivalence (same derived set)
//! - Mutation refusal sweep: every mutation surface returns ReadOnly
//! - pending_delta_count == 0 after open_at (mirror of T1's post-loop assert)
//! - Commit out of range returns CommitOutOfRange

use core_api::{Direction, GraphDb, GraphError, IngestOptions, Predicate, RuleDef, Value};
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
///   2: InsertNode "b" label="T" tag="x"  → rule fires: a-[SAME]→b (and b→a)
///   3: DerivedEdgeAdded history markers  (STATE NO-OP; markers only)
///   4: SetProp "b".tag = "y"             → rule retracts a-[SAME]→b (and b→a)
///   5: DerivedEdgeRetracted history markers (STATE NO-OP; markers only)
///   6: DeleteNode "a"
///
/// Total: 7 WAL frames (commits 0..=6). Commits 3 and 5 are history-marker
/// frames appended by the rule engine after each rule fire/retract; they carry
/// zero replay state and are skipped by `apply()` / `apply_one()`.
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
        via_label: None,
        via_edge: None,
        via_dir: None,
    })
    .unwrap();
    // commit 2 — rule fires (a and b share tag="x"); commit 3 = DerivedEdgeAdded markers
    db.insert_node("T", "b", vec![("tag".into(), Value::Str("x".into()))])
        .unwrap();
    // commit 4 — tag changes, rule retracts; commit 5 = DerivedEdgeRetracted markers
    db.set_prop("b", "tag", Value::Str("y".into())).unwrap();
    // commit 6 — delete a
    db.delete_node("a").unwrap();
}

// ── Golden commit-count test ────────────────────────────────────────────────

/// Pin that wal_commits() counts every WAL frame as one commit (both Batch
/// and single-op), and that the known-history db has exactly 7 commits.
///
/// Frame 3 (DerivedEdgeAdded) and frame 5 (DerivedEdgeRetracted) are history-
/// marker frames appended after each rule fire/retract.  They are STATE NO-OPS
/// on replay but are counted as commits by wal_commits().
///
/// This is the stable commit-count golden test required by Task 2 scope.
#[test]
fn wal_commits_golden_count() {
    let dir = tmp("commits-golden");
    build_known_history(&dir);
    let bytes = std::fs::read(dir.join("wal.bin")).unwrap();
    let n = wal_commits(&bytes);
    assert_eq!(
        n, 7,
        "known-history db must have exactly 7 WAL frames (commits 0..=6; \
         commits 3 and 5 are history-marker frames)"
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
    assert!(db.rules().is_empty(), "commit 0: no rules created yet");
    let nbrs = db.neighbors("a", "SAME", Direction::Out).unwrap();
    assert!(nbrs.is_empty(), "commit 0: no edges");
    assert!(db.is_read_only(), "open_at result must be read-only");
    assert_eq!(
        db.total_wal_commits(),
        7,
        "total_wal_commits must reflect the full WAL count including history-marker frames"
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
fn open_at_commit_4_setprop_edge_retracted() {
    // Commit 3 is the DerivedEdgeAdded marker frame (state no-op).
    // The SetProp that causes the retraction is at commit 4.
    // After replaying 0..=4, the edge is retracted.
    let dir = tmp("at-3");
    build_known_history(&dir);

    let db = GraphDb::open_at(&dir, 4).unwrap();
    assert!(db.has_node("a"), "commit 4: a exists");
    assert!(db.has_node("b"), "commit 4: b exists");

    let nbrs = db.neighbors("a", "SAME", Direction::Out).unwrap();
    assert!(
        nbrs.is_empty(),
        "commit 4: edge must be retracted after tag change at commit 4"
    );

    // explain returns empty for a pair with no derived edge
    let exps = db.explain("a", "b").unwrap();
    assert!(
        exps.is_empty(),
        "commit 4: no derived edge, explain must be empty"
    );
}

#[test]
fn open_at_commit_4_node_a_deleted() {
    // DeleteNode "a" moved to commit 6 due to history-marker frames at commits 3 and 5.
    let dir = tmp("at-4");
    build_known_history(&dir);

    let db = GraphDb::open_at(&dir, 6).unwrap();
    assert!(!db.has_node("a"), "commit 6: node a must be deleted");
    assert!(db.has_node("b"), "commit 6: node b still present");
}

// ── open_at(latest) == normal open equivalence ──────────────────────────────

/// Build a db whose FINAL state has live derived edges so equivalence
/// comparison is non-trivial.
///
/// Commit layout:
///   0: InsertNode "x" tag="hello"
///   1: CreateRule "eq" (FieldEqual "tag", T→T→SAME)
///   2: InsertNode "y" tag="hello"   → eq fires: x-[SAME]->y (and y-[SAME]->x)
///   3: InsertNode "z" tag="hello"   → eq fires: x-[SAME]->z, y-[SAME]->z (+ reverses)
fn build_equivalence_history(dir: &std::path::Path) {
    let mut db = GraphDb::open(dir).unwrap();
    db.insert_node("T", "x", vec![("tag".into(), Value::Str("hello".into()))])
        .unwrap();
    db.create_rule(RuleDef {
        name: "eq".into(),
        src_label: "T".into(),
        dst_label: "T".into(),
        predicate: Predicate::FieldEqual {
            field: "tag".into(),
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
    db.insert_node("T", "y", vec![("tag".into(), Value::Str("hello".into()))])
        .unwrap();
    db.insert_node("T", "z", vec![("tag".into(), Value::Str("hello".into()))])
        .unwrap();
}

#[test]
fn open_at_latest_equivalent_to_open() {
    let dir = tmp("at-latest");
    build_equivalence_history(&dir);

    let total = wal_commits(&std::fs::read(dir.join("wal.bin")).unwrap());
    let latest = total - 1;

    let at = GraphDb::open_at(&dir, latest).unwrap();
    let normal = GraphDb::open(&dir).unwrap();

    // Node set matches
    for key in ["x", "y", "z"] {
        assert_eq!(
            at.has_node(key),
            normal.has_node(key),
            "open_at(latest) and open must agree on node {key}"
        );
    }

    // Edge count matches
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

    // Per-node derived neighbor sets match for all three nodes
    for key in ["x", "y", "z"] {
        let mut at_nbrs = at.neighbors(key, "SAME", Direction::Out).unwrap();
        let mut norm_nbrs = normal.neighbors(key, "SAME", Direction::Out).unwrap();
        at_nbrs.sort();
        norm_nbrs.sort();
        assert_eq!(
            at_nbrs, norm_nbrs,
            "open_at(latest) SAME-Out neighbors of {key} must match open()"
        );
    }

    // explain() output matches for 3 known pairs: (x,y), (x,z), (y,z)
    for (a, b) in [("x", "y"), ("x", "z"), ("y", "z")] {
        let mut at_exps = at.explain(a, b).unwrap();
        let mut norm_exps = normal.explain(a, b).unwrap();
        // Sort by (rule, edge_type) for stable comparison
        at_exps.sort_by(|l, r| l.rule.cmp(&r.rule).then(l.edge_type.cmp(&r.edge_type)));
        norm_exps.sort_by(|l, r| l.rule.cmp(&r.rule).then(l.edge_type.cmp(&r.edge_type)));
        assert_eq!(
            at_exps.len(),
            norm_exps.len(),
            "open_at(latest) explain({a},{b}) count must match open()"
        );
        for (ae, ne) in at_exps.iter().zip(norm_exps.iter()) {
            assert_eq!(ae.rule, ne.rule, "explain rule mismatch for ({a},{b})");
            assert_eq!(
                ae.edge_type, ne.edge_type,
                "explain edge_type mismatch for ({a},{b})"
            );
            assert_eq!(
                ae.src_key, ne.src_key,
                "explain src_key mismatch for ({a},{b})"
            );
            assert_eq!(
                ae.dst_key, ne.dst_key,
                "explain dst_key mismatch for ({a},{b})"
            );
            assert_eq!(
                ae.weight, ne.weight,
                "explain weight mismatch for ({a},{b})"
            );
        }
    }
}

// ── Commit out of range ──────────────────────────────────────────────────────

#[test]
fn open_at_out_of_range_returns_error() {
    let dir = tmp("oor");
    build_known_history(&dir);

    // commit 7 is one past the last valid (0..=6); total is 7 (includes marker frames)
    let err = GraphDb::open_at(&dir, 7).err().expect("should err");
    match err {
        GraphError::CommitOutOfRange {
            commit: 7,
            total: 7,
        } => {}
        other => panic!("expected CommitOutOfRange{{7,7}}, got {other:?}"),
    }

    // Empty db: any commit is out of range
    let dir2 = tmp("oor-empty");
    let _db = GraphDb::open(&dir2).unwrap(); // creates dir, no WAL writes
    let err2 = GraphDb::open_at(&dir2, 0).err().expect("should err");
    match err2 {
        GraphError::CommitOutOfRange {
            commit: 0,
            total: 0,
        } => {}
        other => panic!("expected CommitOutOfRange{{0,0}} for empty WAL, got {other:?}"),
    }
}

// ── Torn WAL tail ───────────────────────────────────────────────────────────

/// Pin torn-tail behaviour: if the last WAL frame is corrupt/incomplete,
/// open_at silently counts only the valid prefix.  No error is returned
/// for the partial write itself; CommitOutOfRange fires if the requested
/// commit is >= the valid frame count.
#[test]
fn torn_tail_open_at_sees_fewer_commits() {
    let dir = tmp("torn-tail");
    // Write 3 commits: InsertNode "p", InsertNode "q", InsertNode "r".
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("T", "p", vec![]).unwrap(); // commit 0
        db.insert_node("T", "q", vec![]).unwrap(); // commit 1
        db.insert_node("T", "r", vec![]).unwrap(); // commit 2
    }

    let wal_path = dir.join("wal.bin");
    let mut bytes = std::fs::read(&wal_path).unwrap();
    assert_eq!(
        core_storage::wal::wal_commits(&bytes),
        3,
        "must have 3 valid frames before tearing"
    );

    // Tear the last frame: remove 4 bytes from the end so the last
    // frame's payload is incomplete (bytes.len() < start + len).
    let new_len = bytes.len() - 4;
    bytes.truncate(new_len);
    std::fs::write(&wal_path, &bytes).unwrap();

    // After tearing: only 2 valid frames remain.
    assert_eq!(
        core_storage::wal::wal_commits(&bytes),
        2,
        "torn WAL must have 2 valid frames"
    );

    // open_at(1) succeeds — within the 2 valid frames.
    let db1 = GraphDb::open_at(&dir, 1).unwrap();
    assert_eq!(
        db1.total_wal_commits(),
        2,
        "total_wal_commits must reflect the valid (post-tear) count"
    );
    assert!(db1.has_node("p"), "commit 1: p inserted at commit 0");
    assert!(db1.has_node("q"), "commit 1: q inserted at commit 1");
    assert!(!db1.has_node("r"), "commit 1: r was in the torn frame");

    // open_at(2) is out of range: valid total is 2, so commit 2 >= 2.
    let err = GraphDb::open_at(&dir, 2).err().expect("should err");
    match err {
        GraphError::CommitOutOfRange {
            commit: 2,
            total: 2,
        } => {}
        other => panic!("expected CommitOutOfRange{{2,2}}, got {other:?}"),
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
    let e = db.insert_node("T", "new", vec![]).unwrap_err();
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
    let e = db.insert_edge("OTHER", "a", "b").unwrap_err();
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
            via_label: None,
            via_edge: None,
            via_dir: None,
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

    // ingest — even with zero rows, commit_ingest hits commit_logged_batch
    let e = db
        .ingest("T", vec![], &IngestOptions::default())
        .unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "ingest must return ReadOnly, got {e:?}"
    );

    // ingest_with_edges — same structural path as ingest
    let e = db
        .ingest_with_edges("T", vec![], &IngestOptions::default(), &[])
        .unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "ingest_with_edges must return ReadOnly, got {e:?}"
    );

    // ingest_json — empty JSON array still reaches commit_logged_batch
    let e = db
        .ingest_json("T", "[]", &IngestOptions::default())
        .unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "ingest_json must return ReadOnly, got {e:?}"
    );

    // query_write — MATCH...SET always calls batch.commit() → commit_logged_batch
    let e = db
        .query_write(
            "MATCH (n:T) SET n.qw_marker = 'x'",
            &std::collections::BTreeMap::new(),
        )
        .unwrap_err();
    assert!(
        matches!(e, GraphError::ReadOnly),
        "query_write must return ReadOnly, got {e:?}"
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
    let rs = db.query("MATCH (n:T) RETURN n", &params).unwrap();
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
