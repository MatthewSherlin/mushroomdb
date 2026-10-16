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
use mushroomdb::{AggFn, Direction, GraphDb, Predicate, RuleDef, Value, ViewDef, ViewSource};
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

// ── what the review of the above found ──────────────────────────────────────

fn avg_view() -> ViewDef {
    ViewDef {
        name: "avg".into(),
        label: "A".into(),
        view_prop: "avg".into(),
        source: ViewSource::NeighborAgg {
            edge_type: "E".into(),
            direction: Direction::Out,
            agg: AggFn::Avg,
            prop: "score".into(),
        },
    }
}

/// Binding: a view cannot take the name of a property the snapshot holds.
/// The collision check read only what was written since the snapshot, so the
/// view was accepted over a real property — and deleting it then removed that
/// property from every node.
#[test]
fn a_view_cannot_be_created_over_a_property_the_snapshot_holds() {
    let dir = tmp("collision");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    for k in ["a", "b"] {
        db.insert_node("A", k, vec![("score".into(), Value::Int(7))])
            .unwrap();
    }
    db.snapshot().unwrap();
    let refused = db.create_view(ViewDef {
        name: "clash".into(),
        label: "A".into(),
        view_prop: "score".into(),
        source: ViewSource::Degree {
            edge_type: "E".into(),
            direction: Direction::Out,
        },
    });
    assert!(refused.is_err(), "score is a real property: {refused:?}");
    assert_eq!(db.get_prop("a", "score"), Some(Value::Int(7)));
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("b", "score"), Some(Value::Int(7)));
}

/// Binding: a refused `create_view` leaves nothing behind. The refusal used
/// to come after the record was written to the WAL, so the store that had
/// just said no failed its next open replaying it — with or without a
/// snapshot.
#[test]
fn a_refused_view_does_not_stop_the_store_opening() {
    let dir = tmp("refused-view");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.insert_node("A", "a", vec![("score".into(), Value::Int(7))])
        .unwrap();
    let refused = db.create_view(ViewDef {
        name: "clash".into(),
        label: "A".into(),
        view_prop: "score".into(),
        source: ViewSource::Degree {
            edge_type: "E".into(),
            direction: Direction::Out,
        },
    });
    assert!(refused.is_err());
    drop(db);
    let db: Db = GraphDb::open(&dir).expect("the store opens after a refused view");
    assert_eq!(db.get_prop("a", "score"), Some(Value::Int(7)));
}

/// Binding: an average with nothing left to average is absent, on a store
/// with a snapshot as on one without. The view cleared its value from what
/// was written since the snapshot and the snapshot's value read through.
#[test]
fn an_aggregate_with_no_neighbours_left_is_absent_after_a_snapshot() {
    let dir = tmp("avg-absent");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(avg_view()).unwrap();
    db.insert_node("A", "a", vec![]).unwrap();
    for (k, score) in [("b", 4), ("c", 8)] {
        db.insert_node("A", k, vec![("score".into(), Value::Int(score))])
            .unwrap();
        db.insert_edge("E", "a", k).unwrap();
    }
    assert_eq!(db.get_prop("a", "avg"), Some(Value::Float(6.0)));
    db.snapshot().unwrap();
    db.delete_edge("E", "a", "b").unwrap();
    db.delete_edge("E", "a", "c").unwrap();
    assert_eq!(db.get_prop("a", "avg"), None, "the handle that snapshotted");
    drop(db);
    let mut db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("a", "avg"), None, "after a reopen");
    db.snapshot().unwrap();
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("a", "avg"), None, "after the next snapshot");
}

/// Binding: a view over an edge type a rule derives is right after a reopen
/// that replays writes made since the snapshot. The replay does not report
/// the rule's edge changes to the views, and with a snapshot nothing
/// recomputed them afterwards.
#[test]
fn a_view_over_a_derived_edge_is_right_after_replaying_writes_over_a_snapshot() {
    let dir = tmp("derived-view");
    let kind = |k: &str| vec![("kind".into(), Value::Str(k.into()))];
    let degrees = |db: &Db| -> Vec<Option<Value>> {
        ["b", "c", "e"]
            .iter()
            .map(|k| db.get_prop(k, "deg"))
            .collect()
    };
    let live = {
        let mut db: Db = GraphDb::open(&dir).unwrap();
        db.create_rule(same_kind_rule()).unwrap();
        db.create_view(ViewDef {
            name: "deg".into(),
            label: "A".into(),
            view_prop: "deg".into(),
            source: ViewSource::Degree {
                edge_type: "SAME".into(),
                direction: Direction::Out,
            },
        })
        .unwrap();
        db.insert_node("A", "a", kind("draft")).unwrap();
        db.insert_node("A", "b", kind("draft")).unwrap();
        db.insert_node("A", "c", kind("other")).unwrap();
        db.insert_node("A", "d", kind("note")).unwrap();
        db.snapshot().unwrap();
        db.delete_node("a").unwrap();
        db.remove_prop("b", "kind").unwrap();
        db.set_prop("c", "kind", Value::Str("note".into())).unwrap();
        db.insert_node("A", "e", kind("note")).unwrap();
        degrees(&db)
    };
    assert_eq!(
        live,
        vec![
            Some(Value::Int(0)),
            Some(Value::Int(2)),
            Some(Value::Int(2))
        ]
    );
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(degrees(&db), live, "after a reopen");
}

/// Binding: a snapshot says its writer sweeps a deleted node's edges, and the
/// search for leftovers at open runs only where a snapshot does not say so.
/// The fixture from 0.7.1 does not; once this release has opened it, dropped
/// the edges and taken a snapshot, that snapshot does.
#[test]
fn a_snapshot_says_its_edges_were_swept() {
    let has_mark = |dir: &std::path::Path| {
        std::fs::read(dir.join("snapshot.bin"))
            .unwrap()
            .windows(4)
            .any(|w| w == b"MSWP")
    };
    let dir = tmp("swept-mark");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("snapshot.bin"),
        include_bytes!("fixtures/dangling_edges_071.bin"),
    )
    .unwrap();
    assert!(!has_mark(&dir));
    let mut db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.stats().edges, 1);
    db.snapshot().unwrap();
    drop(db);
    assert!(has_mark(&dir));
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.stats().edges, 1);
    assert_eq!(db.neighbors("c", "E", Direction::Out).unwrap(), vec!["b"]);
}

/// Binding: deleting a node that many others point at, on a store with a
/// snapshot and a view over that edge type, leaves every other node's view
/// right. (Its own view values go with it; they used to be recomputed once
/// for each of its edges.)
#[test]
fn deleting_a_hub_after_a_snapshot_leaves_its_neighbours_views_right() {
    let dir = tmp("hub");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    db.insert_node("A", "hub", vec![]).unwrap();
    for i in 0..200 {
        let k = format!("n{i}");
        db.insert_node("A", &k, vec![]).unwrap();
        db.insert_edge("E", "hub", &k).unwrap();
        db.insert_edge("E", &k, "hub").unwrap();
    }
    db.insert_edge("E", "n0", "n1").unwrap();
    db.snapshot().unwrap();
    assert_eq!(db.get_prop("hub", "deg"), Some(Value::Int(200)));
    assert_eq!(db.get_prop("n0", "deg"), Some(Value::Int(2)));
    db.delete_node("hub").unwrap();
    assert_eq!(db.get_prop("n0", "deg"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("n7", "deg"), Some(Value::Int(0)));
    assert_eq!(db.stats().edges, 1);
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("n0", "deg"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("n7", "deg"), Some(Value::Int(0)));
    assert_eq!(db.stats().edges, 1);
}

/// Binding: a view deleted after a snapshot can be made again straight away.
/// The name check looked for the column, and the snapshot still has the
/// column after every value in it is hidden; what makes a name taken is a
/// node that still reads a value under it.
#[test]
fn a_view_deleted_after_a_snapshot_can_be_created_again() {
    let dir = tmp("recreate");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    db.insert_node("A", "a", vec![]).unwrap();
    db.insert_node("A", "b", vec![]).unwrap();
    db.insert_edge("E", "a", "b").unwrap();
    db.snapshot().unwrap();
    db.delete_view("deg").unwrap();
    db.create_view(deg_view()).expect("the name is free again");
    assert_eq!(db.get_prop("a", "deg"), Some(Value::Int(1)));
    drop(db);
    let db: Db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.get_prop("a", "deg"), Some(Value::Int(1)));
    assert_eq!(db.get_prop("b", "deg"), Some(Value::Int(0)));
}

/// Binding: replaying a `CreateView` never stops a store opening. The two
/// stores here were written by a build of the `v0.7.1` tag and open under it:
///
/// - `L1`: a view, nodes, a snapshot keeping the WAL, the view deleted, a
///   second such snapshot. The snapshot still holds the view's column.
/// - `L3`: a view, nodes, the view deleted, a real property written under the
///   freed name, a snapshot keeping the WAL.
///
/// Both snapshots kept their WAL, so the whole WAL — the `CreateView` in it
/// included — replays over a snapshot that already has the name. A name check
/// in the replay that reads the snapshot refuses that record and the open
/// with it.
#[test]
fn a_replayed_create_view_does_not_stop_a_store_from_an_earlier_release_opening() {
    for (name, snapshot, wal) in [
        (
            "L1",
            &include_bytes!("fixtures/legacy_071/L1.snapshot.bin")[..],
            &include_bytes!("fixtures/legacy_071/L1.wal.bin")[..],
        ),
        (
            "L3",
            &include_bytes!("fixtures/legacy_071/L3.snapshot.bin")[..],
            &include_bytes!("fixtures/legacy_071/L3.wal.bin")[..],
        ),
    ] {
        let dir = tmp(&format!("legacy-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("snapshot.bin"), snapshot).unwrap();
        std::fs::write(dir.join("wal.bin"), wal).unwrap();
        let db: Db = GraphDb::open(&dir).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(db.stats().nodes_live > 0, "{name}");
    }
}
