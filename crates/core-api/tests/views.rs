/// Tests for materialized property views (Task 3).
///
/// Coverage:
/// - Degree view basic backfill and incremental update
/// - NeighborAgg Sum backfill and incremental update
/// - NeighborAgg Avg updates on neighbor prop change
/// - MIN retraction recompute (O(degree))
/// - Degree view over derived edges (fire AND retract)
/// - View prop in Cypher WHERE + grouped aggregation
/// - SET on a view prop → ViewPropReadOnly error
/// - Reopen rebuild identity (WAL replay)
/// - View over edges in both directions
/// - delete_view removes values cleanly
/// - Snapshot+WAL round-trip preserves views
/// - DST oracle: quiescent value == scratch recompute
use core_api::{AggFn, Direction, GraphDb, GraphError, Predicate, RuleDef, Value, ViewDef, ViewSource};

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-view-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn degree_view(name: &str, label: &str, view_prop: &str, edge_type: &str, direction: Direction) -> ViewDef {
    ViewDef {
        name: name.into(),
        label: label.into(),
        view_prop: view_prop.into(),
        source: ViewSource::Degree { edge_type: edge_type.into(), direction },
    }
}

fn neighbor_agg_view(name: &str, label: &str, view_prop: &str, edge_type: &str, direction: Direction, agg: AggFn, prop: &str) -> ViewDef {
    ViewDef {
        name: name.into(),
        label: label.into(),
        view_prop: view_prop.into(),
        source: ViewSource::NeighborAgg { edge_type: edge_type.into(), direction, agg, prop: prop.into() },
    }
}

// ---------------------------------------------------------------------------
// Degree views
// ---------------------------------------------------------------------------

#[test]
fn degree_view_backfill_and_incremental() {
    let dir = tmp("deg");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("City", "c1", vec![]).unwrap();
    db.insert_node("Person", "p1", vec![]).unwrap();
    db.insert_node("Person", "p2", vec![]).unwrap();

    // Create view BEFORE edges exist — backfill should yield 0.
    db.create_view(degree_view("city_in_deg", "City", "pop", "LIVES_IN", Direction::In)).unwrap();
    let c1_id = "c1";
    assert_eq!(db.get_prop(c1_id, "pop"), Some(&Value::Int(0)));

    // Add edges incrementally.
    db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
    assert_eq!(db.get_prop(c1_id, "pop"), Some(&Value::Int(1)));
    db.insert_edge("LIVES_IN", "p2", "c1").unwrap();
    assert_eq!(db.get_prop(c1_id, "pop"), Some(&Value::Int(2)));

    // Delete an edge — decrement.
    db.delete_edge("LIVES_IN", "p1", "c1").unwrap();
    assert_eq!(db.get_prop(c1_id, "pop"), Some(&Value::Int(1)));
}

#[test]
fn degree_view_out_direction() {
    let dir = tmp("deg_out");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Person", "p1", vec![]).unwrap();
    db.insert_node("City", "c1", vec![]).unwrap();
    db.insert_node("City", "c2", vec![]).unwrap();
    db.create_view(degree_view("person_out_deg", "Person", "num_cities", "LIVES_IN", Direction::Out)).unwrap();
    assert_eq!(db.get_prop("p1", "num_cities"), Some(&Value::Int(0)));
    db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
    assert_eq!(db.get_prop("p1", "num_cities"), Some(&Value::Int(1)));
    db.insert_edge("LIVES_IN", "p1", "c2").unwrap();
    assert_eq!(db.get_prop("p1", "num_cities"), Some(&Value::Int(2)));
}

// ---------------------------------------------------------------------------
// NeighborAgg views
// ---------------------------------------------------------------------------

#[test]
fn neighbor_sum_backfill_and_incremental() {
    let dir = tmp("sum");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("City", "c1", vec![]).unwrap();
    db.insert_node("Person", "p1", vec![("score".into(), Value::Float(3.0))]).unwrap();
    db.insert_node("Person", "p2", vec![("score".into(), Value::Float(7.0))]).unwrap();
    db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
    db.insert_edge("LIVES_IN", "p2", "c1").unwrap();

    // Create view after edges — backfill should sum 3+7=10.
    db.create_view(neighbor_agg_view("city_score", "City", "score_sum", "LIVES_IN", Direction::In, AggFn::Sum, "score")).unwrap();
    assert_eq!(db.get_prop("c1", "score_sum"), Some(&Value::Float(10.0)));

    // Remove p1's edge — Sum decrements by 3.
    db.delete_edge("LIVES_IN", "p1", "c1").unwrap();
    assert_eq!(db.get_prop("c1", "score_sum"), Some(&Value::Float(7.0)));

    // Add it back.
    db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
    assert_eq!(db.get_prop("c1", "score_sum"), Some(&Value::Float(10.0)));
}

#[test]
fn neighbor_avg_updates_on_prop_change() {
    let dir = tmp("avg");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("City", "c1", vec![]).unwrap();
    db.insert_node("Person", "p1", vec![("score".into(), Value::Float(4.0))]).unwrap();
    db.insert_node("Person", "p2", vec![("score".into(), Value::Float(6.0))]).unwrap();
    db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
    db.insert_edge("LIVES_IN", "p2", "c1").unwrap();

    db.create_view(neighbor_agg_view("city_avg", "City", "score_avg", "LIVES_IN", Direction::In, AggFn::Avg, "score")).unwrap();
    // avg = (4+6)/2 = 5
    assert_eq!(db.get_prop("c1", "score_avg"), Some(&Value::Float(5.0)));

    // Change p1's score — avg should update.
    db.set_prop("p1", "score", Value::Float(10.0)).unwrap();
    // avg = (10+6)/2 = 8
    assert_eq!(db.get_prop("c1", "score_avg"), Some(&Value::Float(8.0)));

    // Remove p1's score entirely — only p2 contributes.
    db.remove_prop("p1", "score").unwrap();
    // avg = 6/1 = 6
    assert_eq!(db.get_prop("c1", "score_avg"), Some(&Value::Float(6.0)));
}

#[test]
fn min_retraction_recomputes_correctly() {
    let dir = tmp("min");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("City", "c1", vec![]).unwrap();
    db.insert_node("Person", "p1", vec![("age".into(), Value::Float(20.0))]).unwrap();
    db.insert_node("Person", "p2", vec![("age".into(), Value::Float(30.0))]).unwrap();
    db.insert_node("Person", "p3", vec![("age".into(), Value::Float(25.0))]).unwrap();
    db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
    db.insert_edge("LIVES_IN", "p2", "c1").unwrap();
    db.insert_edge("LIVES_IN", "p3", "c1").unwrap();

    db.create_view(neighbor_agg_view("city_min_age", "City", "min_age", "LIVES_IN", Direction::In, AggFn::Min, "age")).unwrap();
    // min = 20.0 (p1)
    assert_eq!(db.get_prop("c1", "min_age"), Some(&Value::Float(20.0)));

    // Delete p1 (the min holder) → should recompute from remaining: min of 30, 25 = 25.
    db.delete_node("p1").unwrap();
    // O(degree) recompute triggered.
    assert_eq!(db.get_prop("c1", "min_age"), Some(&Value::Float(25.0)));
}

// ---------------------------------------------------------------------------
// Derived edges (rules)
// ---------------------------------------------------------------------------

#[test]
fn degree_view_over_derived_edges_fire_and_retract() {
    let dir = tmp("derived");
    let mut db = GraphDb::open(&dir).unwrap();

    // Rule: Person with org_id → WORKS_AT → Org
    db.insert_node("Org", "o1", vec![]).unwrap();
    let rule = RuleDef {
        name: "works_at".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::KeyMatch { field: "org_id".into() },
        edge_type: "WORKS_AT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    };
    db.create_rule(rule).unwrap();

    // Degree view on Org for how many people work there.
    db.create_view(degree_view("org_headcount", "Org", "headcount", "WORKS_AT", Direction::In)).unwrap();
    assert_eq!(db.get_prop("o1", "headcount"), Some(&Value::Int(0)));

    // Insert person → rule fires → derived WORKS_AT edge → headcount ++
    db.insert_node("Person", "alice", vec![("org_id".into(), Value::Str("o1".into()))]).unwrap();
    assert_eq!(db.get_prop("o1", "headcount"), Some(&Value::Int(1)));

    // Delete person → rule retracts → headcount --
    db.delete_node("alice").unwrap();
    assert_eq!(db.get_prop("o1", "headcount"), Some(&Value::Int(0)));
}

// ---------------------------------------------------------------------------
// Cypher integration
// ---------------------------------------------------------------------------

#[test]
fn view_prop_in_cypher_where_and_group() {
    use std::collections::BTreeMap;

    let dir = tmp("cypher");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("City", "nyc", vec![]).unwrap();
    db.insert_node("City", "la", vec![]).unwrap();
    db.insert_node("Person", "p1", vec![]).unwrap();
    db.insert_node("Person", "p2", vec![]).unwrap();
    db.insert_node("Person", "p3", vec![]).unwrap();

    db.insert_edge("LIVES_IN", "p1", "nyc").unwrap();
    db.insert_edge("LIVES_IN", "p2", "nyc").unwrap();
    db.insert_edge("LIVES_IN", "p3", "la").unwrap();

    db.create_view(degree_view("city_pop", "City", "pop", "LIVES_IN", Direction::In)).unwrap();

    // Filter: only cities with pop >= 2
    let results = db.query(
        "MATCH (c:City) WHERE c.pop >= 2 RETURN c.name",
        &BTreeMap::new(),
    ).unwrap();
    // "nyc" has pop=2; "la" has pop=1 — only nyc qualifies.
    // The query returns c.name but our nodes don't have a name prop set,
    // so we just check that exactly 1 row comes back.
    assert_eq!(results.len(), 1, "only nyc has pop >= 2");

    // Grouped aggregation: SUM(pop) GROUP BY label
    let results2 = db.query(
        "MATCH (c:City) RETURN SUM(c.pop) AS total",
        &BTreeMap::new(),
    ).unwrap();
    assert_eq!(results2.len(), 1);
    // total = 2 + 1 = 3
    // total = 2 + 1 = 3  (SUM of pop Int values; query engine may return Int or Float)
    let total = results2.get(0, "total").cloned();
    let total_f = match total {
        Some(Value::Int(n)) => n as f64,
        Some(Value::Float(f)) => f,
        other => panic!("unexpected total: {other:?}"),
    };
    assert!((total_f - 3.0).abs() < 1e-10, "expected total=3, got {total_f}");
}

// ---------------------------------------------------------------------------
// Write guard
// ---------------------------------------------------------------------------

#[test]
fn set_on_view_prop_returns_error() {
    let dir = tmp("guard");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("City", "c1", vec![]).unwrap();
    db.create_view(degree_view("city_pop", "City", "pop", "LIVES_IN", Direction::In)).unwrap();

    let err = db.set_prop("c1", "pop", Value::Int(999)).unwrap_err();
    match err {
        GraphError::ViewPropReadOnly { view_name } => {
            assert_eq!(view_name, "city_pop");
        }
        other => panic!("expected ViewPropReadOnly, got {other:?}"),
    }
}

#[test]
fn remove_on_view_prop_returns_error() {
    let dir = tmp("guard_remove");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("City", "c1", vec![]).unwrap();
    db.create_view(degree_view("city_pop", "City", "pop", "LIVES_IN", Direction::In)).unwrap();

    let err = db.remove_prop("c1", "pop").unwrap_err();
    assert!(matches!(err, GraphError::ViewPropReadOnly { .. }));
}

// ---------------------------------------------------------------------------
// Reopen rebuild identity
// ---------------------------------------------------------------------------

#[test]
fn reopen_rebuild_matches_live() {
    let dir = tmp("reopen");
    let vals_live: Vec<_> = {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("City", "c1", vec![]).unwrap();
        db.insert_node("City", "c2", vec![]).unwrap();
        db.insert_node("Person", "p1", vec![("score".into(), Value::Float(5.0))]).unwrap();
        db.insert_node("Person", "p2", vec![("score".into(), Value::Float(3.0))]).unwrap();
        db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
        db.insert_edge("LIVES_IN", "p2", "c1").unwrap();
        db.create_view(degree_view("city_pop", "City", "pop", "LIVES_IN", Direction::In)).unwrap();
        db.create_view(neighbor_agg_view("city_sum", "City", "score_sum", "LIVES_IN", Direction::In, AggFn::Sum, "score")).unwrap();
        vec![
            db.get_prop("c1", "pop").cloned(),
            db.get_prop("c2", "pop").cloned(),
            db.get_prop("c1", "score_sum").cloned(),
        ]
    };

    // Reopen and check values are identical.
    let db2 = GraphDb::open(&dir).unwrap();
    let vals_reopen = vec![
        db2.get_prop("c1", "pop").cloned(),
        db2.get_prop("c2", "pop").cloned(),
        db2.get_prop("c1", "score_sum").cloned(),
    ];

    assert_eq!(vals_live, vals_reopen, "reopen must rebuild identical values");
}

// ---------------------------------------------------------------------------
// delete_view cleanup
// ---------------------------------------------------------------------------

#[test]
fn delete_view_removes_values() {
    let dir = tmp("del");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("City", "c1", vec![]).unwrap();
    db.insert_node("Person", "p1", vec![]).unwrap();
    db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
    db.create_view(degree_view("city_pop", "City", "pop", "LIVES_IN", Direction::In)).unwrap();

    assert!(db.get_prop("c1", "pop").is_some());

    db.delete_view("city_pop").unwrap();
    assert!(db.get_prop("c1", "pop").is_none(), "view values removed after delete");

    // View prop is now writable again.
    db.set_prop("c1", "pop", Value::Int(99)).unwrap();
    assert_eq!(db.get_prop("c1", "pop"), Some(&Value::Int(99)));
}

// ---------------------------------------------------------------------------
// DST oracle: quiescent value == scratch recompute
// ---------------------------------------------------------------------------

#[test]
fn dst_oracle_degree_matches_scratch() {
    let dir = tmp("oracle");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("City", "c1", vec![]).unwrap();
    db.insert_node("Person", "p1", vec![]).unwrap();
    db.insert_node("Person", "p2", vec![]).unwrap();
    db.insert_node("Person", "p3", vec![]).unwrap();
    db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
    db.insert_edge("LIVES_IN", "p2", "c1").unwrap();
    db.insert_edge("LIVES_IN", "p3", "c1").unwrap();

    db.create_view(degree_view("city_pop", "City", "pop", "LIVES_IN", Direction::In)).unwrap();

    let live = db.get_prop("c1", "pop").cloned();
    let scratch = db.scratch_view_value("c1", "city_pop");
    assert_eq!(live, scratch, "live value must equal scratch recompute");

    // Delete one edge and re-check.
    db.delete_edge("LIVES_IN", "p2", "c1").unwrap();
    let live2 = db.get_prop("c1", "pop").cloned();
    let scratch2 = db.scratch_view_value("c1", "city_pop");
    assert_eq!(live2, scratch2, "live value after edge delete must equal scratch");
}

#[test]
fn dst_oracle_neighbor_sum_matches_scratch() {
    let dir = tmp("oracle_sum");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("City", "c1", vec![]).unwrap();
    db.insert_node("Person", "p1", vec![("score".into(), Value::Float(10.0))]).unwrap();
    db.insert_node("Person", "p2", vec![("score".into(), Value::Float(20.0))]).unwrap();
    db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
    db.insert_edge("LIVES_IN", "p2", "c1").unwrap();

    db.create_view(neighbor_agg_view("city_sum", "City", "score_sum", "LIVES_IN", Direction::In, AggFn::Sum, "score")).unwrap();

    let live = db.get_prop("c1", "score_sum").cloned();
    let scratch = db.scratch_view_value("c1", "city_sum");
    assert_eq!(live, scratch);

    // Change a prop and check again.
    db.set_prop("p1", "score", Value::Float(50.0)).unwrap();
    let live2 = db.get_prop("c1", "score_sum").cloned();
    let scratch2 = db.scratch_view_value("c1", "city_sum");
    assert_eq!(live2, scratch2);
}

// ---------------------------------------------------------------------------
// Snapshot round-trip
// ---------------------------------------------------------------------------

#[test]
fn snapshot_preserves_views_and_values_rebuild() {
    let dir = tmp("snap");
    let pre_snap: Vec<_> = {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("City", "c1", vec![]).unwrap();
        db.insert_node("Person", "p1", vec![("score".into(), Value::Float(7.0))]).unwrap();
        db.insert_edge("LIVES_IN", "p1", "c1").unwrap();
        db.create_view(degree_view("city_pop", "City", "pop", "LIVES_IN", Direction::In)).unwrap();
        db.create_view(neighbor_agg_view("city_sum", "City", "score_sum", "LIVES_IN", Direction::In, AggFn::Sum, "score")).unwrap();
        let vals = vec![
            db.get_prop("c1", "pop").cloned(),
            db.get_prop("c1", "score_sum").cloned(),
        ];
        db.snapshot().unwrap();
        vals
    };

    // Reopen after snapshot — views must still be present and values correct.
    let mut db2 = GraphDb::open(&dir).unwrap();
    let post_snap = vec![
        db2.get_prop("c1", "pop").cloned(),
        db2.get_prop("c1", "score_sum").cloned(),
    ];
    assert_eq!(pre_snap, post_snap, "values match after snapshot+reopen");

    // Views still operational after reopen.
    db2.insert_node("Person", "p2", vec![("score".into(), Value::Float(3.0))]).unwrap();
    db2.insert_edge("LIVES_IN", "p2", "c1").unwrap();
    assert_eq!(db2.get_prop("c1", "pop"), Some(&Value::Int(2)));
    assert_eq!(db2.get_prop("c1", "score_sum"), Some(&Value::Float(10.0)));
}

// ---------------------------------------------------------------------------
// Collision / error cases
// ---------------------------------------------------------------------------

#[test]
fn duplicate_view_name_rejected() {
    let dir = tmp("dup");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_view(degree_view("v", "City", "pop", "LIVES_IN", Direction::In)).unwrap();
    let err = db.create_view(degree_view("v", "City", "pop2", "LIVES_IN", Direction::In)).unwrap_err();
    assert!(matches!(err, GraphError::RuleInvalid { .. }));
}

#[test]
fn view_prop_collision_with_existing_view_rejected() {
    let dir = tmp("dup_prop");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_view(degree_view("v1", "City", "pop", "LIVES_IN", Direction::In)).unwrap();
    let err = db.create_view(degree_view("v2", "City", "pop", "LIVES_IN", Direction::Out)).unwrap_err();
    assert!(matches!(err, GraphError::RuleInvalid { .. }));
}

#[test]
fn delete_view_unknown_returns_not_found() {
    let dir = tmp("del_unk");
    let mut db = GraphDb::open(&dir).unwrap();
    let err = db.delete_view("no_such_view").unwrap_err();
    assert!(matches!(err, GraphError::RuleNotFound { .. }));
}

// ---------------------------------------------------------------------------
// pending_deltas discipline: view updates must not accumulate engine deltas
// ---------------------------------------------------------------------------

#[test]
fn pending_deltas_are_clean_through_view_heavy_workload() {
    // This test exercises create_view, insert_node (rule fire), delete_node
    // and verifies that no stale deltas are left after each commit.
    // The debug_assert in log_then_apply_with catches violations at runtime;
    // this test exists to document the invariant and exercise the path.
    let dir = tmp("deltas");
    let mut db = GraphDb::open(&dir).unwrap();

    // Rule: WORKS_AT
    let rule = RuleDef {
        name: "works_at".into(),
        src_label: "Person".into(),
        dst_label: "Org".into(),
        predicate: Predicate::KeyMatch { field: "org_id".into() },
        edge_type: "WORKS_AT".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
    };
    db.insert_node("Org", "o1", vec![]).unwrap();
    db.create_rule(rule).unwrap();
    db.create_view(degree_view("org_headcount", "Org", "headcount", "WORKS_AT", Direction::In)).unwrap();

    // Multiple person inserts → rule fires → view updates.
    for i in 0..10u32 {
        db.insert_node("Person", &format!("p{i}"), vec![("org_id".into(), Value::Str("o1".into()))]).unwrap();
    }
    assert_eq!(db.get_prop("o1", "headcount"), Some(&Value::Int(10)));

    // Delete all persons → rule retracts → view decrements.
    for i in 0..10u32 {
        db.delete_node(&format!("p{i}")).unwrap();
    }
    assert_eq!(db.get_prop("o1", "headcount"), Some(&Value::Int(0)));
}
