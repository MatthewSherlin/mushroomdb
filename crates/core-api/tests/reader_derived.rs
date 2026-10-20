//! Ledger row 114: the lock-free reader's `node_edges` reported `derived: false`
//! for every rule-derived edge, because the snapshot carries no rule engine.
//!
//! Every test here is differential: whatever the write handle's `node_edges`
//! says about an edge, `reader().node_edges` must say the same, across the
//! states a reader can be taken in.

use mushroomdb::{GraphDb, NodeMask, Predicate, RealFs, RuleDef, Value, FOLD_EVERY_K};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mdb-readerderived-{name}-{}-{nanos}",
        std::process::id()
    ))
}

fn team_rule() -> RuleDef {
    RuleDef {
        name: "same_team".into(),
        src_label: "Person".into(),
        dst_label: "Person".into(),
        predicate: Predicate::FieldEqual {
            field: "team".into(),
        },
        edge_type: "SAME_TEAM".into(),
        weight_prop: None,
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

fn team(name: &str) -> Vec<(String, Value)> {
    vec![("team".to_string(), Value::Str(name.into()))]
}

const KEYS: [&str; 5] = ["a", "b", "c", "d", "e"];

/// `a`, `b`, `c` on one team (derived edges among them), `d` alone, and a
/// user-written `a -> d` edge that no rule owns. The rule is created first so
/// the inserts derive through the write path.
fn populated(dir: &std::path::Path) -> GraphDb<RealFs> {
    let mut db = GraphDb::open(dir).unwrap();
    db.create_rule(team_rule()).unwrap();
    for key in ["a", "b", "c"] {
        db.insert_node("Person", key, team("red")).unwrap();
    }
    db.insert_node("Person", "d", team("blue")).unwrap();
    db.insert_node("Person", "e", team("green")).unwrap();
    db.insert_edge("KNOWS", "a", "d").unwrap();
    db
}

/// The reader reports every edge exactly as the live handle does; returns how
/// many derived edges that covered, so a test can insist it was not vacuous.
fn assert_reader_matches_live(db: &GraphDb<RealFs>) -> usize {
    let reader = db.reader();
    let mut derived = 0;
    for key in KEYS {
        let live = db.node_edges(key).unwrap();
        let seen = reader.node_edges(key).unwrap();
        assert_eq!(seen, live, "node_edges({key}) differs from the live handle");
        derived += live.iter().filter(|e| e.derived).count();
    }
    derived
}

#[test]
fn node_edges_derived_flag() {
    let dir = tmp("flag");
    let db = populated(&dir);
    let reader = db.reader();
    let edges = reader.node_edges("a").unwrap();
    assert!(edges.iter().any(|e| e.derived), "a derived edge is flagged");
    let user = edges.iter().find(|e| e.edge_type == "KNOWS").unwrap();
    assert!(!user.derived, "a user-written edge is not derived");
    assert!(assert_reader_matches_live(&db) > 0);
}

#[test]
fn no_snapshot_with_edges_in_the_delta_tail() {
    let db = populated(&tmp("tail"));
    assert!(assert_reader_matches_live(&db) > 0);
}

#[test]
fn no_snapshot_after_the_tail_has_folded() {
    let mut db = populated(&tmp("folded"));
    for i in 0..(FOLD_EVERY_K + 3) {
        db.insert_node("Person", &format!("filler{i}"), team("red"))
            .unwrap();
    }
    assert!(assert_reader_matches_live(&db) > 0);
    db.insert_node("Person", "late", team("red")).unwrap();
    assert!(assert_reader_matches_live(&db) > 0);
}

/// Enough folds to merge the derived layers, with edges derived and retracted
/// across them, over no base and over one.
#[test]
fn many_folds_of_derived_changes() {
    for snapshot in [false, true] {
        let mut db = populated(&tmp("layers"));
        if snapshot {
            db.snapshot().unwrap();
        }
        for round in 0..(FOLD_EVERY_K * 10) {
            let team_name = if round % 3 == 0 { "red" } else { "blue" };
            db.set_prop("d", "team", Value::Str(team_name.into()))
                .unwrap();
            if round % FOLD_EVERY_K == 5 {
                assert_reader_matches_live(&db);
            }
        }
        assert_reader_matches_live(&db);
    }
}

#[test]
fn after_a_snapshot_the_base_carries_derived_edges() {
    let mut db = populated(&tmp("snap"));
    db.snapshot().unwrap();
    assert!(assert_reader_matches_live(&db) > 0);
}

#[test]
fn after_a_snapshot_and_reopen() {
    let dir = tmp("reopen");
    let mut db = populated(&dir);
    db.snapshot().unwrap();
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert!(assert_reader_matches_live(&db) > 0);
}

#[test]
fn derived_in_the_tail_over_a_snapshot_base() {
    let dir = tmp("tail-over-base");
    let mut db = populated(&dir);
    db.snapshot().unwrap();
    db.set_prop("d", "team", Value::Str("red".into())).unwrap();
    assert!(assert_reader_matches_live(&db) > 0);
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert!(assert_reader_matches_live(&db) > 0);
}

#[test]
fn a_retracted_edge_stops_being_derived() {
    let mut db = populated(&tmp("retract"));
    db.snapshot().unwrap();
    // `c` leaves the team: the rule retracts its edges, base ones included.
    db.set_prop("c", "team", Value::Str("blue".into())).unwrap();
    assert_reader_matches_live(&db);
    let reader = db.reader();
    assert!(reader
        .node_edges("c")
        .unwrap()
        .iter()
        .all(|e| e.edge_type != "SAME_TEAM" || e.src_key == "d" || e.dst_key == "d"));
}

#[test]
fn a_dropped_rule_leaves_no_derived_flags() {
    let mut db = populated(&tmp("drop"));
    assert!(assert_reader_matches_live(&db) > 0);
    db.delete_rule("same_team").unwrap();
    assert_reader_matches_live(&db);
    let reader = db.reader();
    for key in KEYS {
        assert!(reader.node_edges(key).unwrap().iter().all(|e| !e.derived));
    }
}

#[test]
fn a_dropped_rule_after_a_snapshot() {
    let mut db = populated(&tmp("drop-snap"));
    db.snapshot().unwrap();
    db.delete_rule("same_team").unwrap();
    assert_reader_matches_live(&db);
}

#[test]
fn a_scoped_read_reports_the_same_derived_flags() {
    for snapshot in [false, true] {
        let mut db = populated(&tmp("scoped"));
        if snapshot {
            db.snapshot().unwrap();
        }
        db.insert_node("Person", "f", team("red")).unwrap();
        let mask = NodeMask::from_keys(&db, ["a", "b", "d", "f"]);
        let reader = db.reader();
        let seen = reader.node_edges_scoped("a", &mask).unwrap();
        let live = db.node_edges_scoped("a", &mask).unwrap();
        assert_eq!(seen, live);
        assert!(seen.iter().any(|e| e.derived));
    }
}

/// Timing of `reader().node_edges` on a 100k-node store carrying derived
/// edges. Run with `--release -- --ignored --nocapture`.
#[test]
#[ignore = "timing probe; run in release"]
fn node_edges_timing_100k() {
    let dir = tmp("timing");
    let mut db = GraphDb::open(&dir).unwrap();
    db.create_rule(team_rule()).unwrap();
    // Teams of ten: nine derived edges each way per node.
    for chunk in 0..100 {
        let mut batch = db.batch();
        for i in 0..1000 {
            let n = chunk * 1000 + i;
            batch.insert_node("Person", &format!("n{n}"), team(&format!("t{}", n / 10)));
        }
        batch.commit().unwrap();
    }
    db.snapshot().unwrap();
    let probes: Vec<String> = (0..2000).map(|i| format!("n{}", i * 50)).collect();
    let mut samples = Vec::new();
    for _ in 0..7 {
        let reader = db.reader();
        let start = std::time::Instant::now();
        let mut derived = 0;
        for key in &probes {
            derived += reader
                .node_edges(key)
                .unwrap()
                .iter()
                .filter(|e| e.derived)
                .count();
        }
        samples.push((start.elapsed(), derived));
    }
    samples.sort();
    eprintln!(
        "node_edges x2000 median: {:?} (derived seen: {})",
        samples[3].0, samples[3].1
    );
}
