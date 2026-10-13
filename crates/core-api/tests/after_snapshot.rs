//! What is written after a snapshot has to act on what the snapshot holds.
//!
//! A store opened from a snapshot, or one that has just taken one, keeps the
//! snapshot's edges and columns in a mapped base and what was written since
//! in an overlay beside it. Four writes looked at the overlay alone (ledger
//! row 95), and each was wrong only on a store with a snapshot:
//!
//! - deleting a node left the edges the snapshot held for it, in the writer
//!   and in the reader's replay of the delta tail;
//! - deleting a view left its property readable on the nodes the snapshot
//!   held it for;
//! - a snapshot taken when no rule owned an edge carried the previous
//!   snapshot's provenance forward, so a rule created again owned edges it
//!   had not derived;
//! - an aggregate view created after a snapshot summed nothing for the
//!   neighbours whose values the snapshot held.
//!
//! Found by a probe that ran the same writes through a handle that snapshots
//! and one that never does.
use core_api::{AggFn, Direction, GraphDb, Predicate, RuleDef, Value, ViewDef, ViewSource};
use std::collections::BTreeMap;
use std::path::PathBuf;

type Db = GraphDb<core_storage::fs::RealFs>;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("row95-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn deg_view() -> ViewDef {
    ViewDef {
        name: "deg".into(),
        label: "A".into(),
        view_prop: "deg".into(),
        source: ViewSource::Degree {
            edge_type: "E".into(),
            direction: Direction::Out,
        },
    }
}

fn same_kind_rule() -> RuleDef {
    RuleDef {
        name: "r".into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::FieldEqual {
            field: "kind".into(),
        },
        edge_type: "SAME".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

/// a -E-> b, c -E-> a, all three in the snapshot; then `a` is deleted.
fn store_with_a_deleted_after_the_snapshot(tag: &str) -> (Db, PathBuf) {
    let dir = tmp(tag);
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    for k in ["a", "b", "c"] {
        db.insert_node("A", k, vec![]).unwrap();
    }
    db.insert_edge("E", "a", "b").unwrap();
    db.insert_edge("E", "c", "a").unwrap();
    db.snapshot().unwrap();
    db.delete_node("a").unwrap();
    (db, dir)
}

fn edge_rows(db: &Db) -> usize {
    db.query(
        "MATCH (x)-[:E]->(y) RETURN x.id AS x, y.id AS y",
        &BTreeMap::new(),
    )
    .unwrap()
    .len()
}

// ── a deleted node's edges ──────────────────────────────────────────────────

#[test]
fn deleting_a_node_after_a_snapshot_removes_its_snapshotted_edges_from_the_count() {
    let (db, dir) = store_with_a_deleted_after_the_snapshot("a-count");
    assert_eq!(db.stats().edges, 0, "the handle that snapshotted");
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.stats().edges, 0, "after a reopen");
}

#[test]
fn neighbours_of_a_survivor_are_readable_after_a_snapshotted_neighbour_is_deleted() {
    let (db, dir) = store_with_a_deleted_after_the_snapshot("a-nbrs");
    // Both used to be Err(Corrupt { "topology id 0 has no key" }).
    assert_eq!(
        db.neighbors("b", "E", Direction::In).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        db.neighbors("c", "E", Direction::Out).unwrap(),
        Vec::<String>::new()
    );
    // The query used to answer one row, (c, NULL).
    assert_eq!(edge_rows(&db), 0);
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.neighbors("b", "E", Direction::In).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        db.neighbors("c", "E", Direction::Out).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(edge_rows(&db), 0);
}

/// The lock-free reader the HTTP server answers from replays the delta tail
/// with a sweep of its own, which had the same blind spot.
#[test]
fn a_reader_snapshot_does_not_see_the_snapshotted_edges_of_a_deleted_node() {
    let (db, _dir) = store_with_a_deleted_after_the_snapshot("a-reader");
    let rows = db
        .reader()
        .query(
            "MATCH (x)-[:E]->(y) RETURN x.id AS x, y.id AS y",
            &BTreeMap::new(),
        )
        .unwrap()
        .len();
    assert_eq!(rows, 0);
}

#[test]
fn a_degree_view_counts_down_when_a_snapshotted_neighbour_is_deleted() {
    let (db, dir) = store_with_a_deleted_after_the_snapshot("a-deg");
    assert_eq!(db.get_prop("c", "deg"), Some(Value::Int(0)));
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("c", "deg"), Some(Value::Int(0)));
}

#[test]
fn a_later_snapshot_does_not_keep_the_edges_of_a_deleted_node() {
    let (mut db, dir) = store_with_a_deleted_after_the_snapshot("a-persist");
    db.snapshot().unwrap(); // the dangling edges are merged into the new base
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.stats().edges, 0);
    assert_eq!(
        db.neighbors("c", "E", Direction::Out).unwrap(),
        Vec::<String>::new()
    );
}

// ── a deleted view's property ─────────────────────────────────────────────

#[test]
fn deleting_a_view_after_a_snapshot_removes_its_property_from_every_node() {
    let dir = tmp("b-view");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![]).unwrap();
    db.create_view(deg_view()).unwrap();
    assert_eq!(db.get_prop("a", "deg"), Some(Value::Int(0)));
    db.snapshot().unwrap();
    db.delete_view("deg").unwrap();
    assert_eq!(db.get_prop("a", "deg"), None, "the handle that snapshotted");
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("a", "deg"), None, "after a reopen");
}

// ── provenance carried forward ────────────────────────────────────────────

/// Rule created, snapshot, rule deleted (no rule left), snapshot.
fn store_whose_only_rule_was_deleted_between_two_snapshots(tag: &str) -> PathBuf {
    let dir = tmp(tag);
    let mut db: Db = GraphDb::open(&dir).unwrap();
    for k in ["a", "b"] {
        db.insert_node("A", k, vec![("kind".into(), Value::Str("x".into()))])
            .unwrap();
    }
    db.create_rule(same_kind_rule()).unwrap();
    db.snapshot().unwrap();
    db.delete_rule("r").unwrap();
    db.snapshot().unwrap();
    dir
}

#[test]
fn a_rule_recreated_after_its_deletion_was_snapshotted_derives_its_edges() {
    let dir = store_whose_only_rule_was_deleted_between_two_snapshots("c-recreate");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_rule(same_kind_rule()).unwrap();
    assert_eq!(
        db.neighbors("a", "SAME", Direction::Out).unwrap(),
        vec!["b".to_string()]
    );
    assert_eq!(db.stats().edges, 2);
}

#[test]
fn a_rule_recreated_in_the_snapshotting_process_still_has_its_edges_after_a_reopen() {
    let dir = tmp("c-live");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    for k in ["a", "b"] {
        db.insert_node("A", k, vec![("kind".into(), Value::Str("x".into()))])
            .unwrap();
    }
    db.create_rule(same_kind_rule()).unwrap();
    db.snapshot().unwrap();
    db.delete_rule("r").unwrap();
    db.snapshot().unwrap();
    db.create_rule(same_kind_rule()).unwrap();
    // Right in this process (the engine's own provenance is empty)...
    assert_eq!(
        db.neighbors("a", "SAME", Direction::Out).unwrap(),
        vec!["b".to_string()]
    );
    drop(db);
    // ...and lost by the next process, which replays CreateRule over the stale section.
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.neighbors("a", "SAME", Direction::Out).unwrap(),
        vec!["b".to_string()]
    );
}

// ── an aggregate view over snapshotted values ─────────────────────────────

#[test]
fn an_aggregate_view_created_after_a_snapshot_aggregates_snapshotted_properties() {
    let dir = tmp("d-agg");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![]).unwrap();
    db.insert_node("A", "b", vec![("score".into(), Value::Int(7))])
        .unwrap();
    db.insert_edge("E", "a", "b").unwrap();
    db.snapshot().unwrap();
    db.create_view(ViewDef {
        name: "total".into(),
        label: "A".into(),
        view_prop: "total".into(),
        source: ViewSource::NeighborAgg {
            edge_type: "E".into(),
            direction: Direction::Out,
            agg: AggFn::Sum,
            prop: "score".into(),
        },
    })
    .unwrap();
    // 7.0 without the snapshot; it was 0.0 with it.
    assert_eq!(db.get_prop("a", "total"), Some(Value::Float(7.0)));
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.get_prop("a", "total"),
        Some(Value::Float(7.0)),
        "after a reopen"
    );
}

// ── A store an earlier release left with such edges ─────────────────────────

/// Binding: a snapshot written by 0.7.1 after a node was deleted over an
/// earlier snapshot still holds that node's edges. Opening it drops them.
///
/// The fixture is that snapshot: `a → b`, `c → a` and `c → b`, a snapshot,
/// `delete_node(a)`, a second snapshot. 0.7.1 reopens it reporting three
/// edges and answers `neighbors(c)` with `corrupt data: topology id 0 has no
/// key`.
#[test]
fn opening_a_store_left_with_a_deleted_nodes_edges_drops_them() {
    let dir = tmp("repair");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("snapshot.bin"),
        include_bytes!("fixtures/dangling_edges_071.bin"),
    )
    .unwrap();
    let mut db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.stats().nodes_live, 2);
    assert_eq!(db.neighbors("c", "E", Direction::Out).unwrap(), vec!["b"]);
    assert_eq!(db.neighbors("b", "E", Direction::In).unwrap(), vec!["c"]);
    assert_eq!(db.stats().edges, 1);
    assert_eq!(edge_rows(&db), 1);
    // And the next snapshot no longer carries them.
    db.snapshot().unwrap();
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.stats().edges, 1);
    assert_eq!(db.neighbors("c", "E", Direction::Out).unwrap(), vec!["b"]);
}
