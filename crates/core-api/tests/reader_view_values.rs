//! A view's value read through `reader()` is the value the handle reads.
//!
//! A reader is the last fold of the store plus the commits since, replayed
//! from their records. A view's property is not in any record: the writer
//! computes it as a consequence of an edge or a property changing. The replay
//! skipped it, so a reader answered a view's value as of the last fold — up
//! to `FOLD_EVERY_K - 1` commits old — while every other property it returned
//! was current.
//!
//! Each test makes fewer commits than a fold takes, so the reader has to get
//! the value from the commits themselves.
use core_api::{AggFn, Direction, GraphDb, Predicate, RuleDef, Value, ViewDef, ViewSource};
use std::path::PathBuf;

type Db = GraphDb<core_storage::fs::RealFs>;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("reader-views-{tag}-{}", std::process::id()));
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

fn agg_view(name: &str, agg: AggFn) -> ViewDef {
    ViewDef {
        name: name.into(),
        label: "A".into(),
        view_prop: name.into(),
        source: ViewSource::NeighborAgg {
            edge_type: "E".into(),
            direction: Direction::Out,
            agg,
            prop: "score".into(),
        },
    }
}

/// Every node's properties as the handle reads them and as a reader taken
/// now reads them.
fn assert_reader_agrees(db: &Db, keys: &[&str], when: &str) {
    let reader = db.reader();
    for key in keys {
        let live = db.node_info(key).map(|n| n.props);
        let read = reader.node_info(key).map(|n| n.props);
        assert_eq!(read, live, "{when}: node {key}");
    }
}

fn scored(db: &mut Db, key: &str, score: f64) {
    db.insert_node("A", key, vec![("score".into(), Value::Float(score))])
        .unwrap();
}

#[test]
fn a_degree_view_is_current_in_a_reader_after_an_edge_is_added() {
    let dir = tmp("deg-add");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    for k in ["a", "b", "c"] {
        db.insert_node("A", k, vec![]).unwrap();
    }
    db.insert_edge("E", "a", "b").unwrap();
    db.insert_edge("E", "a", "c").unwrap();

    let read = db.reader().node_info("a").unwrap().props;
    assert_eq!(read.get("deg"), Some(&Value::Int(2)));
    assert_reader_agrees(&db, &["a", "b", "c"], "two edges added");
}

#[test]
fn a_degree_view_is_current_in_a_reader_after_an_edge_is_removed() {
    let dir = tmp("deg-del");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    for k in ["a", "b", "c"] {
        db.insert_node("A", k, vec![]).unwrap();
    }
    db.insert_edge("E", "a", "b").unwrap();
    db.insert_edge("E", "a", "c").unwrap();
    db.delete_edge("E", "a", "b").unwrap();

    let read = db.reader().node_info("a").unwrap().props;
    assert_eq!(read.get("deg"), Some(&Value::Int(1)));
    assert_reader_agrees(&db, &["a", "b", "c"], "one edge removed");
}

#[test]
fn an_aggregate_view_follows_a_neighbours_property_in_a_reader() {
    let dir = tmp("agg-prop");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(agg_view("total", AggFn::Sum)).unwrap();
    db.create_view(agg_view("avg", AggFn::Avg)).unwrap();
    scored(&mut db, "a", 0.0);
    scored(&mut db, "b", 3.0);
    scored(&mut db, "c", 4.0);
    db.insert_edge("E", "a", "b").unwrap();
    db.insert_edge("E", "a", "c").unwrap();
    db.set_prop("c", "score", Value::Float(10.0)).unwrap();

    let read = db.reader().node_info("a").unwrap().props;
    assert_eq!(read.get("total"), Some(&Value::Float(13.0)));
    assert_eq!(read.get("avg"), Some(&Value::Float(6.5)));
    assert_reader_agrees(&db, &["a", "b", "c"], "a neighbour's score changed");
}

#[test]
fn an_average_that_becomes_absent_after_a_snapshot_is_absent_in_a_reader() {
    let dir = tmp("avg-absent");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(agg_view("avg", AggFn::Avg)).unwrap();
    scored(&mut db, "a", 0.0);
    scored(&mut db, "b", 3.0);
    db.insert_edge("E", "a", "b").unwrap();
    db.snapshot().unwrap();
    // The snapshot holds avg = 3.0 for `a`; with no neighbour left it has none.
    db.delete_edge("E", "a", "b").unwrap();

    assert_eq!(db.node_info("a").unwrap().props.get("avg"), None);
    assert_eq!(db.reader().node_info("a").unwrap().props.get("avg"), None);
    assert_reader_agrees(&db, &["a", "b"], "the only neighbour unlinked");
}

#[test]
fn a_view_created_since_the_last_fold_has_its_values_in_a_reader() {
    let dir = tmp("create");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    for k in ["a", "b", "c"] {
        db.insert_node("A", k, vec![]).unwrap();
    }
    db.insert_edge("E", "a", "b").unwrap();
    db.create_view(deg_view()).unwrap();

    let read = db.reader().node_info("a").unwrap().props;
    assert_eq!(read.get("deg"), Some(&Value::Int(1)));
    assert_reader_agrees(&db, &["a", "b", "c"], "view created");
}

#[test]
fn a_deleted_views_property_is_gone_from_a_reader() {
    let dir = tmp("delete");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    for k in ["a", "b"] {
        db.insert_node("A", k, vec![]).unwrap();
    }
    db.insert_edge("E", "a", "b").unwrap();
    db.snapshot().unwrap();
    db.delete_view("deg").unwrap();

    assert_eq!(db.reader().node_info("a").unwrap().props.get("deg"), None);
    assert_reader_agrees(&db, &["a", "b"], "view deleted after a snapshot");
}

#[test]
fn a_view_over_a_derived_edge_is_current_in_a_reader() {
    let dir = tmp("derived");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_rule(RuleDef {
        name: "same_kind".into(),
        src_label: "A".into(),
        dst_label: "A".into(),
        predicate: Predicate::FieldEqual {
            field: "kind".into(),
        },
        edge_type: "E".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    })
    .unwrap();
    db.create_view(deg_view()).unwrap();
    for k in ["a", "b", "c"] {
        db.insert_node("A", k, vec![("kind".into(), Value::Str("x".into()))])
            .unwrap();
    }
    db.set_prop("c", "kind", Value::Str("y".into())).unwrap();

    assert_reader_agrees(&db, &["a", "b", "c"], "a rule's edges came and went");
    let read = db.reader().node_info("a").unwrap().props;
    assert_eq!(read.get("deg"), db.node_info("a").unwrap().props.get("deg"));
    assert!(read.contains_key("deg"));
}

#[test]
fn a_reader_taken_earlier_keeps_the_view_value_of_its_own_commit() {
    let dir = tmp("isolation");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    for k in ["a", "b", "c"] {
        db.insert_node("A", k, vec![]).unwrap();
    }
    db.insert_edge("E", "a", "b").unwrap();
    let earlier = db.reader();
    db.insert_edge("E", "a", "c").unwrap();

    let then = earlier.node_info("a").unwrap().props;
    assert_eq!(then.get("deg"), Some(&Value::Int(1)));
    let now = db.reader().node_info("a").unwrap().props;
    assert_eq!(now.get("deg"), Some(&Value::Int(2)));
}

#[test]
fn a_reader_agrees_with_the_handle_through_a_run_of_writes_and_a_snapshot() {
    let dir = tmp("run");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    db.create_view(agg_view("total", AggFn::Sum)).unwrap();
    db.create_view(agg_view("low", AggFn::Min)).unwrap();
    let keys = ["n0", "n1", "n2", "n3", "n4"];
    for (i, k) in keys.iter().enumerate() {
        scored(&mut db, k, i as f64);
    }
    // A fixed, uneven walk over the five nodes: enough to add, remove,
    // rescore and delete across a snapshot without a random source.
    let mut step = 7usize;
    for round in 0..40 {
        step = step.wrapping_mul(31).wrapping_add(11);
        let a = keys[step % 5];
        let b = keys[(step / 5) % 5];
        match step % 7 {
            0..=2 => {
                let _ = db.insert_edge("E", a, b);
            }
            3 => {
                let _ = db.delete_edge("E", a, b);
            }
            4 => {
                let _ = db.set_prop(b, "score", Value::Float(round as f64));
            }
            5 => {
                let _ = db.remove_prop(b, "score");
            }
            _ => {
                if db.node_info(a).is_some() {
                    db.delete_node(a).unwrap();
                } else {
                    scored(&mut db, a, 1.5);
                }
            }
        }
        if round == 15 {
            db.snapshot().unwrap();
        }
        assert_reader_agrees(&db, &keys, &format!("round {round}"));
    }
}

#[test]
fn view_values_moved_by_one_batch_are_current_in_a_reader() {
    let dir = tmp("batch");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    db.create_view(agg_view("total", AggFn::Sum)).unwrap();
    scored(&mut db, "a", 0.0);
    scored(&mut db, "gone", 9.0);
    db.insert_edge("E", "a", "gone").unwrap();
    {
        let mut b = db.batch();
        b.insert_node("A", "b", vec![("score".into(), Value::Float(2.0))]);
        b.insert_node("A", "c", vec![("score".into(), Value::Float(5.0))]);
        b.insert_edge("E", "a", "b");
        b.insert_edge("E", "a", "c");
        b.delete_edge("E", "a", "gone");
        b.set_prop("c", "score", Value::Float(6.0));
        b.commit().unwrap();
    }

    let read = db.reader().node_info("a").unwrap().props;
    assert_eq!(read.get("deg"), Some(&Value::Int(2)));
    assert_eq!(read.get("total"), Some(&Value::Float(8.0)));
    assert_reader_agrees(&db, &["a", "b", "c", "gone"], "one batch");
}

#[test]
fn a_neighbours_view_is_current_in_a_reader_after_a_snapshotted_node_is_deleted() {
    let dir = tmp("delete-node");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    db.create_view(agg_view("avg", AggFn::Avg)).unwrap();
    scored(&mut db, "a", 0.0);
    scored(&mut db, "b", 3.0);
    db.insert_edge("E", "a", "b").unwrap();
    db.snapshot().unwrap();
    db.delete_node("b").unwrap();

    let read = db.reader().node_info("a").unwrap().props;
    assert_eq!(read.get("deg"), Some(&Value::Int(0)));
    assert_eq!(read.get("avg"), None);
    assert_reader_agrees(&db, &["a", "b"], "the neighbour deleted");
}

/// One commit that moves a view on more nodes than a delta carries is folded
/// instead; the reader has to be as right on that path as on the other.
#[test]
fn a_commit_that_moves_a_view_on_thousands_of_nodes_is_current_in_a_reader() {
    let dir = tmp("hub");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(ViewDef {
        name: "fans".into(),
        label: "A".into(),
        view_prop: "fans".into(),
        source: ViewSource::Degree {
            edge_type: "E".into(),
            direction: Direction::In,
        },
    })
    .unwrap();
    db.insert_node("A", "hub", vec![]).unwrap();
    let spokes: Vec<String> = (0..1500).map(|i| format!("s{i}")).collect();
    {
        let mut b = db.batch();
        for s in &spokes {
            b.insert_node("A", s, vec![]);
            b.insert_edge("E", "hub", s);
        }
        b.commit().unwrap();
    }
    let reader = db.reader();
    for s in [&spokes[0], &spokes[777], &spokes[1499]] {
        let read = reader.node_info(s).unwrap().props;
        assert_eq!(
            read.get("fans"),
            Some(&Value::Int(1)),
            "{s} after the batch"
        );
    }
    db.delete_node("hub").unwrap();
    let reader = db.reader();
    for s in [&spokes[0], &spokes[777], &spokes[1499]] {
        let read = reader.node_info(s).unwrap().props;
        assert_eq!(
            read.get("fans"),
            Some(&Value::Int(0)),
            "{s} after the delete"
        );
        assert_eq!(db.node_info(s).unwrap().props, read);
    }
}

/// A fold is cheap to take and dear to have taken: it shares the store's
/// maps with the reader, so the commit after it copies them before it can
/// write. A bulk load that folded on every batch paid that copy on every
/// batch, and loaded a tenth as fast.
fn load_in_batches(db: &mut Db, label: &str, batches: usize, per_batch: usize) {
    for batch in 0..batches {
        let mut b = db.batch();
        for i in 0..per_batch {
            b.insert_node(label, &format!("{label}-{batch}-{i}"), vec![]);
        }
        b.commit().unwrap();
    }
}

#[test]
fn a_bulk_load_of_a_label_no_view_is_on_does_not_fold_on_every_batch() {
    let dir = tmp("bulk-other");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    load_in_batches(&mut db, "N", 5, 1100);
    assert_eq!(db.reader().deltas.len(), 5, "one delta for each batch");
}

#[test]
fn a_bulk_load_of_a_label_a_view_is_on_does_not_fold_on_every_batch() {
    let dir = tmp("bulk-viewed");
    let mut db: Db = GraphDb::open(&dir).unwrap();
    db.create_view(deg_view()).unwrap();
    load_in_batches(&mut db, "A", 5, 1100);
    let reader = db.reader();
    assert_eq!(reader.deltas.len(), 5, "one delta for each batch");
    for key in ["A-0-0", "A-2-555", "A-4-1099"] {
        let read = reader.node_info(key).unwrap().props;
        assert_eq!(read.get("deg"), Some(&Value::Int(0)), "{key}");
    }
}
