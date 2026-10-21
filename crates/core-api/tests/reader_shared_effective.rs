//! TX-5: snapshots share one materialisation per fold and tail, and a longer
//! tail starts from the shorter tail's state.
//!
//! Both are invisible when they work and silently wrong when they do not, so
//! this pins them against the live handle: after every commit of a mixed
//! workload, a fresh snapshot answers exactly what `GraphDb` answers, and every
//! snapshot taken earlier still answers what it answered when it was taken.

use mushroomdb::{BatchOp, Dir, Predicate, RuleDef, SharedDb, Value, FOLD_EVERY_K};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mdb-reader-shared-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Wait (up to 10 s) until the background builder has built `snap`'s
/// version's cells, so what a test then reads is builder-built.
fn await_background_build(snap: &mushroomdb::ReaderSnapshot) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !snap.built_in_background() {
        assert!(
            std::time::Instant::now() < deadline,
            "version {}: the background builder never built its cells",
            snap.version
        );
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
}

const KEYS: &[&str] = &["a", "b", "c", "d", "e", "renamed", "gone"];
const QUERIES: &[&str] = &[
    "MATCH (n:N) RETURN n.key AS k, n.v AS v ORDER BY k",
    "MATCH (n:N)-[:R]->(m) RETURN n.key AS s, m.key AS d ORDER BY s, d",
    "MATCH (n:N) RETURN n.key AS k UNION MATCH (n:M) RETURN n.key AS k",
    "MATCH (n:N)-[r:FITS]->(m:M) RETURN n.key AS s, m.key AS d, r.score AS w ORDER BY s, d",
];

/// Everything a snapshot answers, in a comparable form.
#[derive(Debug, PartialEq)]
struct Answers(Vec<String>);

fn snapshot_answers(snap: &mushroomdb::ReaderSnapshot) -> Answers {
    let mut out = Vec::new();
    for q in QUERIES {
        out.push(format!("{q} => {:?}", snap.query(q, &BTreeMap::new())));
    }
    for k in KEYS {
        out.push(format!("info {k} => {:?}", snap.node_info(k)));
        out.push(format!("edges {k} => {:?}", snap.node_edges(k)));
        out.push(format!(
            "nb {k} => {:?}",
            snap.neighborhood(k, 2, None, Dir::Both)
        ));
    }
    Answers(out)
}

fn live_answers(db: &SharedDb) -> Answers {
    let g = db.read();
    let mut out = Vec::new();
    for q in QUERIES {
        out.push(format!("{q} => {:?}", g.query(q, &BTreeMap::new())));
    }
    for k in KEYS {
        out.push(format!("info {k} => {:?}", g.node_info(k)));
        out.push(format!("edges {k} => {:?}", g.node_edges(k)));
        out.push(format!(
            "nb {k} => {:?}",
            g.node_ref(k).map(|n| n.neighborhood(2, None, Dir::Both))
        ));
    }
    Answers(out)
}

fn node(label: &str, key: &str, v: i64) -> BatchOp {
    BatchOp::InsertNode {
        label: label.into(),
        key: key.into(),
        props: vec![("v".into(), Value::Int(v))],
    }
}

fn skills(key: &str, list: &[&str]) -> BatchOp {
    BatchOp::SetProp {
        key: key.into(),
        field: "skills".into(),
        value: Value::List(list.iter().map(|s| Value::Str(s.to_string())).collect()),
    }
}

fn weighted_rule() -> RuleDef {
    RuleDef {
        name: "fits".into(),
        src_label: "N".into(),
        dst_label: "M".into(),
        predicate: Predicate::Overlap {
            field: "skills".into(),
            min: 0.1,
        },
        edge_type: "FITS".into(),
        weight_prop: Some("score".into()),
        max_edges: None,
        approximate: false,
        via_label: None,
        via_edge: None,
        via_dir: None,
        namespace: None,
    }
}

fn workload() -> Vec<Vec<BatchOp>> {
    let edge = |s: &str, d: &str| BatchOp::InsertEdge {
        edge_type: "R".into(),
        src_key: s.into(),
        dst_key: d.into(),
    };
    let set = |k: &str, v: i64| BatchOp::SetProp {
        key: k.into(),
        field: "v".into(),
        value: Value::Int(v),
    };
    let mut steps = vec![
        vec![node("N", "a", 1), node("N", "b", 2)],
        vec![node("N", "c", 3), edge("a", "b")],
        vec![set("a", 10)],
        vec![edge("b", "c"), edge("c", "c")],
        vec![node("M", "d", 4)],
        vec![BatchOp::RemoveProp {
            key: "b".into(),
            field: "v".into(),
        }],
        vec![node("N", "gone", 5), edge("gone", "a")],
        vec![BatchOp::DeleteNode { key: "gone".into() }],
        vec![BatchOp::DeleteEdge {
            edge_type: "R".into(),
            src_key: "a".into(),
            dst_key: "b".into(),
        }],
        vec![node("N", "e", 6), edge("e", "a"), set("e", 60)],
        vec![BatchOp::RenameNode {
            old_key: "e".into(),
            new_key: "renamed".into(),
        }],
        // A weighted rule's scores are written by the engine with no record;
        // the second write moves a score on an edge that already exists.
        vec![skills("d", &["x", "y"]), skills("a", &["x"])],
        vec![skills("c", &["x", "y"])],
        vec![skills("a", &["x", "y"])],
        vec![skills("c", &["z"])],
    ];
    // Enough single-write commits to cross a fold and grow a new tail.
    for i in 0..(FOLD_EVERY_K as i64 + 5) {
        steps.push(vec![set(if i % 2 == 0 { "a" } else { "c" }, 100 + i)]);
    }
    steps
}

#[test]
fn every_snapshot_matches_the_live_handle_and_keeps_its_own_answer() {
    let db = SharedDb::open(&tmp("chain")).unwrap();
    db.write().create_rule(weighted_rule()).unwrap();
    let mut kept: Vec<(mushroomdb::ReaderSnapshot, Answers)> = Vec::new();
    for (i, ops) in workload().into_iter().enumerate() {
        db.submit_batch(ops).unwrap();
        // Two snapshots of the same state share a cell; the second must see
        // what the first computed, not a half-built or stale state.
        let first = db.reader();
        // Every write publishes and hands its version to the builder; check
        // the cells it built, not ones this read would build itself.
        if !first.deltas.is_empty() {
            await_background_build(&first);
        }
        let got = snapshot_answers(&first);
        assert_eq!(got, live_answers(&db), "step {i}: snapshot vs live");
        let second = db.reader();
        assert_eq!(snapshot_answers(&second), got, "step {i}: shared cell");
        kept.push((first, got));
    }
    // Isolation: nothing that happened after a snapshot reached it.
    for (i, (snap, answered)) in kept.iter().enumerate() {
        assert_eq!(&snapshot_answers(snap), answered, "snapshot {i} drifted");
    }
}

/// A snapshot taken but never read until later commits have landed still
/// materialises its own state, not the seed's or the newest one.
#[test]
fn an_unread_snapshot_materialises_its_own_tail() {
    let db = SharedDb::open(&tmp("unread")).unwrap();
    db.submit_batch(vec![node("N", "a", 1)]).unwrap();
    let warm = db.reader();
    let _ = snapshot_answers(&warm);
    db.submit_batch(vec![node("N", "b", 2)]).unwrap();
    let unread = db.reader();
    let expected = live_answers(&db);
    db.submit_batch(vec![node("N", "c", 3)]).unwrap();
    let newest = db.reader();
    let _ = snapshot_answers(&newest);
    assert_eq!(snapshot_answers(&unread), expected);
    assert_eq!(snapshot_answers(&newest), live_answers(&db));
}

/// R2 (TX-5 fix round 1): an equality lookup on a declared index is served by
/// the reader from the fold's index layered with the tail's touched nodes, and
/// answers exactly what the live handle's index answers — rows and row order —
/// at every version a concurrent writer passes through: inserts, value moves,
/// deletes, renames, across folds.
#[test]
fn indexed_lookups_on_the_reader_match_the_live_index_under_concurrent_writes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let db = SharedDb::open(&tmp("indexed")).unwrap();
    db.write().enable_index("N", "name").unwrap();
    let nodes: Vec<BatchOp> = (0..200)
        .map(|i| BatchOp::InsertNode {
            label: "N".into(),
            key: format!("k{i}"),
            props: vec![("name".into(), Value::Str(format!("n{}", i % 20)))],
        })
        .collect();
    db.submit_batch(nodes).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (db, stop) = (db.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut i = 0usize;
            let mut next_key = 200usize;
            while !stop.load(Ordering::Relaxed) && i < 400 {
                let k = format!("k{}", (i * 37) % next_key);
                let op = match i % 6 {
                    0..=2 => BatchOp::SetProp {
                        key: k,
                        field: "name".into(),
                        value: Value::Str(format!("n{}", (i * 7) % 20)),
                    },
                    3 => {
                        next_key += 1;
                        BatchOp::InsertNode {
                            label: "N".into(),
                            key: format!("k{}", next_key - 1),
                            props: vec![("name".into(), Value::Str(format!("n{}", i % 20)))],
                        }
                    }
                    4 => BatchOp::DeleteNode { key: k },
                    _ => BatchOp::RemoveProp {
                        key: k,
                        field: "name".into(),
                    },
                };
                // Keys already deleted make a write fail; that is part of the
                // churn, not of the test.
                let _ = db.submit_batch(vec![op]);
                i += 1;
            }
        })
    };

    let queries: Vec<String> = [0, 3, 7, 12, 19]
        .into_iter()
        .flat_map(|v| {
            [
                format!("MATCH (n:N {{name: 'n{v}'}}) RETURN n.key AS k"),
                format!("MATCH (n:N) WHERE n.name = 'n{v}' RETURN count(n) AS c"),
            ]
        })
        .collect();
    let params = BTreeMap::new();
    let (mut checked, mut indexed, mut built_by_builder) = (0usize, 0usize, 0usize);
    let mut last_version = None;
    while !writer.is_finished() || checked == 0 {
        // One guard: the live handle and a snapshot of the very same version.
        let g = db.read();
        let snap = g.reader();
        // A version seen for the first time was last published by the writer
        // and handed to the builder: wait for it, so the cells compared are
        // the builder's.
        if last_version != Some(snap.version) && !snap.deltas.is_empty() {
            await_background_build(&snap);
            built_by_builder += 1;
        }
        last_version = Some(snap.version);
        indexed += usize::from(snap.queries_use_index());
        for q in &queries {
            assert_eq!(
                format!("{:?}", snap.query(q, &params)),
                format!("{:?}", g.query(q, &params)),
                "version {}: {q}",
                g.commit_seq()
            );
        }
        drop(g);
        checked += 1;
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(checked > 10, "only {checked} versions compared");
    assert!(
        indexed * 2 > checked,
        "the index served only {indexed} of {checked} versions"
    );
    // Every version with a tail was compared on builder-built cells (the
    // wait above fails the test otherwise); make sure there were many.
    assert!(
        built_by_builder > 10,
        "only {built_by_builder} versions were checked on builder-built cells"
    );
}

/// TX-5 fix round 3: the writer builds a version's delta-tail layer and query
/// index as it publishes, so a reader served the published snapshot (here,
/// while another writer holds the lock) finds both built and pays no first
/// build — after a bulk commit that was the whole batch.
#[test]
fn the_published_snapshot_arrives_with_its_layer_and_index_built() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let db = SharedDb::open(&tmp("eager")).unwrap();
    db.write().enable_index("N", "name").unwrap();
    let bulk: Vec<BatchOp> = (0..5_000)
        .map(|i| BatchOp::InsertNode {
            label: "N".into(),
            key: format!("b{i}"),
            props: vec![("name".into(), Value::Str(format!("n{}", i % 50)))],
        })
        .collect();
    db.submit_batch(bulk).unwrap();
    db.write()
        .set_prop("b7", "name", Value::Str("moved".into()))
        .unwrap();

    let held = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let holder = {
        let (db, held, release) = (db.clone(), Arc::clone(&held), Arc::clone(&release));
        std::thread::spawn(move || {
            let _guard = db.write();
            held.store(true, Ordering::SeqCst);
            while !release.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    };
    while !held.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let snap = db.reader();
    assert!(
        !snap.deltas.is_empty(),
        "the bulk commit must be in the tail"
    );
    // Built off the write lock by the background builder, not by this read.
    await_background_build(&snap);
    assert!(
        snap.layer_and_index_built(),
        "the published snapshot's layer or index was left for the reader to build"
    );
    let rs = snap
        .query(
            "MATCH (n:N {name: 'moved'}) RETURN n.key AS k",
            &BTreeMap::new(),
        )
        .unwrap();
    assert_eq!(rs.len(), 1);
    release.store(true, Ordering::SeqCst);
    holder.join().unwrap();
}
