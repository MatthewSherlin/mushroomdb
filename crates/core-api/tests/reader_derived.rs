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

/// One batch that moves `c` off its team and back (or the reverse), in the
/// orders a commit's deltas can arrive. The engine reports each move as a
/// retraction and a re-derivation of the same edges, in order; keeping only
/// which kind each was lost the order, and the reader then dropped or flagged
/// those edges wrongly — in the tail, and after the next fold.
fn batch_shape_probe(name: &str, setup_off_team: bool, moves: &[&str]) {
    let dir = tmp(name);
    let mut db = populated(&dir);
    if setup_off_team {
        db.set_prop("c", "team", Value::Str("blue".into())).unwrap();
    }
    let mut batch = db.batch();
    for team_name in moves {
        batch.set_prop("c", "team", Value::Str((*team_name).into()));
    }
    batch.commit().unwrap();
    // In the delta tail.
    assert_reader_matches_live(&db);
    // After the next fold.
    for i in 0..FOLD_EVERY_K {
        db.insert_node("Person", &format!("filler{i}"), team("green"))
            .unwrap();
    }
    assert_reader_matches_live(&db);
    // After a snapshot.
    db.snapshot().unwrap();
    assert_reader_matches_live(&db);
    // After a reopen.
    drop(db);
    let db = GraphDb::open(&dir).unwrap();
    assert_reader_matches_live(&db);
}

#[test]
fn a_batch_that_retracts_then_re_derives() {
    batch_shape_probe("batch-retract-derive", false, &["blue", "red"]);
}

#[test]
fn a_batch_that_re_derives_then_retracts() {
    batch_shape_probe("batch-derive-retract", true, &["red", "blue"]);
}

#[test]
fn a_batch_that_does_both_several_times() {
    batch_shape_probe(
        "batch-several",
        false,
        &["blue", "red", "blue", "red", "blue", "red"],
    );
    batch_shape_probe(
        "batch-several-odd",
        false,
        &["blue", "red", "blue", "red", "blue"],
    );
}

/// A WAL tail after a snapshot: the reopen replays commits whose derived edges
/// the reader must report, retracted ones included, with no snapshot of them.
#[test]
fn a_snapshot_with_a_wal_tail_then_reopen() {
    let dir = tmp("wal-tail");
    let mut db = populated(&dir);
    db.snapshot().unwrap();
    db.set_prop("c", "team", Value::Str("blue".into())).unwrap();
    db.set_prop("d", "team", Value::Str("red".into())).unwrap();
    db.insert_node("Person", "f", team("red")).unwrap();
    drop(db);
    let mut db = GraphDb::open(&dir).unwrap();
    assert!(assert_reader_matches_live(&db) > 0);
    // And onward: more commits, a fold, a snapshot.
    db.set_prop("c", "team", Value::Str("red".into())).unwrap();
    assert_reader_matches_live(&db);
    db.snapshot().unwrap();
    assert_reader_matches_live(&db);
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn median(mut samples: Vec<std::time::Duration>) -> std::time::Duration {
    samples.sort();
    samples[samples.len() / 2]
}

/// Timing probe: open, first read, and 2000 `node_edges` reads on a 100k-node
/// store holding 900k derived edges (teams of ten), against whichever build
/// runs it. Set `MDB_BENCH_STORE` to a directory to build the store once and
/// share it between builds. Run with `--release -- --ignored --nocapture`.
#[test]
#[ignore = "timing probe; run in release"]
fn reader_derived_bench() {
    use std::time::Instant;
    let store = std::env::var("MDB_BENCH_STORE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| tmp("bench"));
    let clean = store.join("clean");
    let one_commit = store.join("one-commit");
    if !clean.exists() {
        let mut db = GraphDb::open(&clean).unwrap();
        db.create_rule(team_rule()).unwrap();
        for chunk in 0..100 {
            let mut batch = db.batch();
            for i in 0..1000 {
                let n = chunk * 1000 + i;
                batch.insert_node("Person", &format!("n{n}"), team(&format!("t{}", n / 10)));
            }
            batch.commit().unwrap();
        }
        db.snapshot().unwrap();
        drop(db);
        copy_dir(&clean, &one_commit);
        let mut db = GraphDb::open(&one_commit).unwrap();
        db.set_prop("n5", "team", Value::Str("t7".into())).unwrap();
    }
    let work = tmp("bench-work");
    let fresh = |from: &std::path::Path, i: usize| {
        let dir = work.join(format!("{i}"));
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(from, &dir);
        dir
    };
    let probes: Vec<String> = (0..2000).map(|i| format!("n{}", i * 50)).collect();
    let read_all = |reader: &mushroomdb::ReaderSnapshot| {
        let start = Instant::now();
        let mut derived = 0;
        for key in &probes {
            derived += reader
                .node_edges(key)
                .unwrap()
                .iter()
                .filter(|e| e.derived)
                .count();
        }
        (start.elapsed(), derived)
    };

    for (label, from) in [
        ("open clean snapshot", &clean),
        ("open snapshot+1 WAL commit", &one_commit),
    ] {
        let mut samples = Vec::new();
        for i in 0..7 {
            let dir = fresh(from, i);
            let start = Instant::now();
            let db = GraphDb::open(&dir).unwrap();
            samples.push(start.elapsed());
            drop(db);
        }
        eprintln!("BENCH {label}: {:?}", median(samples));
    }

    let mut first = Vec::new();
    for i in 0..7 {
        let dir = fresh(&clean, i);
        let db = GraphDb::open(&dir).unwrap();
        let reader = db.reader();
        let start = Instant::now();
        reader.node_edges("n50").unwrap();
        first.push(start.elapsed());
    }
    eprintln!("BENCH first read after open: {:?}", median(first));

    for scenario in ["empty tail", "60-commit tail", "20-batch tail"] {
        let dir = fresh(&clean, 0);
        let mut db = GraphDb::open(&dir).unwrap();
        match scenario {
            "60-commit tail" => {
                for c in 0..60 {
                    db.set_prop(
                        &format!("n{}", c * 50),
                        "team",
                        Value::Str(format!("t{}", c + 1)),
                    )
                    .unwrap();
                }
            }
            "20-batch tail" => {
                for b in 0..20 {
                    let mut batch = db.batch();
                    for j in 0..100 {
                        batch.insert_node(
                            "Person",
                            &format!("new{b}-{j}"),
                            team(&format!("t{}", b * 100 + j)),
                        );
                    }
                    batch.commit().unwrap();
                }
            }
            _ => {}
        }
        let reader = db.reader();
        let first_pass = read_all(&reader).0;
        let steady: Vec<_> = (0..7).map(|_| read_all(&reader).0).collect();
        eprintln!(
            "BENCH 2000 reads, {scenario}: steady {:?} (first pass {:?})",
            median(steady),
            first_pass
        );
    }
}
