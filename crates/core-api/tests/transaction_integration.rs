//! Interactive transactions (TX-1): BEGIN / execute / COMMIT / ROLLBACK over
//! `SharedDb`, with optimistic conflict detection.
//!
//! Differential probes: one transaction against the same ops through
//! `write_batch`, read live, after a reopen (WAL replay) and through `open_at`;
//! and a commit with a snapshot and a fold epoch taken while it was open.

use mushroomdb::{BatchOp, GraphError, Predicate, RealFs, RuleDef, SharedDb, Value};

type GraphDb = mushroomdb::GraphDb<RealFs>;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "mushroomdb-tx-{name}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn p() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn node(key: &str) -> BatchOp {
    BatchOp::InsertNode {
        label: "P".into(),
        key: key.into(),
        props: vec![("id".into(), Value::Str(key.into()))],
    }
}

/// Every `(key, x)` pair of `P` nodes, sorted — a canonical dump to compare.
fn dump_rows(rs: &mushroomdb::ResultSet) -> Vec<String> {
    let mut rows: Vec<String> = (0..rs.len()).map(|i| format!("{:?}", rs.row(i))).collect();
    rows.sort();
    rows
}

const PROBES: &[&str] = &[
    "MATCH (n:P) RETURN n.id AS k, n.x AS x",
    "MATCH (a:P)-[:KNOWS]->(b:P) RETURN a.id AS s, b.id AS d",
    "MATCH (n:P) RETURN count(n) AS c",
];

fn probe(snap: &mushroomdb::ReaderSnapshot) -> Vec<Vec<String>> {
    PROBES
        .iter()
        .map(|q| dump_rows(&snap.query(q, &p()).unwrap()))
        .collect()
}

fn probe_db(db: &GraphDb) -> Vec<Vec<String>> {
    PROBES
        .iter()
        .map(|q| dump_rows(&db.query(q, &p()).unwrap()))
        .collect()
}

fn has_key(db: &SharedDb, key: &str) -> bool {
    db.reader().node_info(key).is_some()
}

// ── Brief tests ──────────────────────────────────────────────────────────────

#[test]
fn tx_commit_visible() {
    let dir = tmp("commit-visible");
    let db = SharedDb::open(&dir).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (n:P {id: 'a', x: 1})", &p()).unwrap();
    assert!(!has_key(&db, "a"), "uncommitted write leaked to a reader");
    let seq = tx.commit().unwrap();
    assert!(has_key(&db, "a"));
    assert_eq!(seq, db.reader().version);
}

#[test]
fn tx_rollback_invisible() {
    let dir = tmp("rollback");
    let db = SharedDb::open(&dir).unwrap();
    let frames = mushroomdb::wal_commit_count_at(&dir).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (n:P {id: 'a'})", &p()).unwrap();
    tx.rollback();
    assert!(!has_key(&db, "a"));
    assert_eq!(mushroomdb::wal_commit_count_at(&dir).unwrap(), frames);

    // Dropping is a rollback too.
    {
        let mut tx = db.begin_transaction();
        tx.execute("CREATE (n:P {id: 'b'})", &p()).unwrap();
    }
    assert!(!has_key(&db, "b"));
    assert_eq!(mushroomdb::wal_commit_count_at(&dir).unwrap(), frames);
}

#[test]
fn tx_read_own_write() {
    let dir = tmp("read-own-write");
    let db = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![node("base")]).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute(
        "CREATE (a:P {id: 'a', x: 1})-[:KNOWS]->(b:P {id: 'b'})",
        &p(),
    )
    .unwrap();
    tx.execute("MATCH (n:P {id: 'a'}) SET n.x = n.x + 41", &p())
        .unwrap();
    tx.execute("MATCH (n:P {id: 'base'}) DETACH DELETE n", &p())
        .unwrap();

    let rs = tx
        .execute("MATCH (n:P {id: 'a'}) RETURN n.x AS x", &p())
        .unwrap();
    assert_eq!(rs.get(0, "x"), Some(&Value::Int(42)));
    let rs = tx
        .execute("MATCH (a:P)-[:KNOWS]->(b:P) RETURN b.id AS d", &p())
        .unwrap();
    assert_eq!(rs.get(0, "d"), Some(&Value::Str("b".into())));
    let rs = tx
        .execute("MATCH (n:P) RETURN count(n) AS c", &p())
        .unwrap();
    assert_eq!(
        rs.get(0, "c"),
        Some(&Value::Int(2)),
        "base must read as deleted"
    );

    // CREATE … RETURN and MERGE … RETURN read the buffer too.
    let rs = tx
        .execute("CREATE (c:P {id: 'c', x: 7}) RETURN c.x AS x", &p())
        .unwrap();
    assert_eq!(rs.get(0, "x"), Some(&Value::Int(7)));
    let rs = tx
        .execute(
            "MERGE (n:P {id: 'c'}) ON MATCH SET n.x = 8 RETURN n.x AS x",
            &p(),
        )
        .unwrap();
    assert_eq!(rs.get(0, "x"), Some(&Value::Int(8)));

    // Nothing reached the store yet.
    assert!(has_key(&db, "base"));
    assert!(!has_key(&db, "a"));
}

#[test]
fn tx_conflict_on_concurrent_write() {
    let dir = tmp("conflict");
    let db = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![node("A"), node("B"), node("C")])
        .unwrap();

    let mut t1 = db.begin_transaction();
    let mut t2 = db.begin_transaction();
    t1.execute("MATCH (n:P {id: 'A'}) RETURN n.x AS x", &p())
        .unwrap();
    t2.execute("MATCH (n:P {id: 'A'}) RETURN n.x AS x", &p())
        .unwrap();
    t1.execute("MATCH (n:P {id: 'A'}) SET n.x = 1", &p())
        .unwrap();
    // t2 writes a different key: it conflicts because of what it *read*.
    t2.execute("MATCH (n:P {id: 'B'}) SET n.x = 2", &p())
        .unwrap();
    t1.commit().unwrap();
    let frames = mushroomdb::wal_commit_count_at(&dir).unwrap();
    let err = t2.commit().unwrap_err();
    assert!(
        matches!(err, GraphError::CasConflict { ref key, .. } if key == "A"),
        "{err:?}"
    );
    assert_eq!(mushroomdb::wal_commit_count_at(&dir).unwrap(), frames);
    let info = db.reader().node_info("B").unwrap();
    assert_eq!(
        info.props.get("x"),
        None,
        "the conflicting commit must write nothing"
    );

    // Lost update: both write the same key.
    let mut t3 = db.begin_transaction();
    let mut t4 = db.begin_transaction();
    t3.execute("MATCH (n:P {id: 'C'}) SET n.y = 3", &p())
        .unwrap();
    t4.execute("MATCH (n:P {id: 'C'}) SET n.y = 4", &p())
        .unwrap();
    t4.commit().unwrap();
    assert!(matches!(
        t3.commit(),
        Err(GraphError::CasConflict { ref key, .. }) if key == "C"
    ));
    let y = db.reader().node_info("C").unwrap().props.get("y").cloned();
    assert_eq!(y, Some(Value::Int(4)), "the first committer wins");
}

#[test]
fn differential_one_batch_vs_tx() {
    let dir_batch = tmp("diff-batch");
    let batch_db = SharedDb::open(&dir_batch).unwrap();
    let id = |k: &str| vec![("id".to_string(), Value::Str(k.into()))];
    batch_db
        .write()
        .write_batch(|b| {
            b.insert_node("P", "a", id("a"));
            b.insert_node("P", "b", id("b"));
            b.insert_node("P", "c", id("c"));
            b.insert_edge("KNOWS", "a", "b");
            b.insert_edge("KNOWS", "b", "c");
            b.set_prop("a", "x", Value::Int(5));
            b.delete_edge("KNOWS", "b", "c");
            b.delete_node("c");
        })
        .unwrap();

    let dir_tx = tmp("diff-tx");
    let tx_db = SharedDb::open(&dir_tx).unwrap();
    let mut tx = tx_db.begin_transaction();
    for q in [
        "CREATE (a:P {id: 'a'})-[:KNOWS]->(b:P {id: 'b'})-[:KNOWS]->(c:P {id: 'c'})",
        "MATCH (n:P {id: 'a'}) SET n.x = 5",
        "MATCH (b:P {id: 'b'})-[r:KNOWS]->(c:P {id: 'c'}) DELETE r",
        "MATCH (n:P {id: 'c'}) DELETE n",
    ] {
        tx.execute(q, &p()).unwrap();
    }
    let in_tx = PROBES
        .iter()
        .map(|q| dump_rows(&tx.execute(q, &p()).unwrap()))
        .collect::<Vec<_>>();
    let seq = tx.commit().unwrap();

    let want = probe(&batch_db.reader());
    assert_eq!(
        in_tx, want,
        "read-your-own-writes differs from the committed batch"
    );
    assert_eq!(probe(&tx_db.reader()), want, "live");
    drop(tx_db);
    drop(batch_db);
    // WAL replay.
    let reopened = GraphDb::open(&dir_tx).unwrap();
    assert_eq!(probe_db(&reopened), want, "reopened");
    drop(reopened);
    // Time travel: the transaction is one commit (0-based frame seq - 1).
    let at = GraphDb::open_at(&dir_tx, seq - 1).unwrap();
    assert_eq!(probe_db(&at), want, "open_at the tx commit");
}

#[test]
fn snapshot_fires_during_open_tx() {
    let dir = tmp("snapshot-during");
    let db = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![node("A"), node("B")]).unwrap();

    // A transaction whose keys nobody else touches commits across a fold
    // epoch and a snapshot.
    let mut quiet = db.begin_transaction();
    // One whose read key changes before the snapshot must conflict.
    let mut loud = db.begin_transaction();
    quiet
        .execute("MATCH (n:P {id: 'A'}) RETURN n.id AS k", &p())
        .unwrap();
    loud.execute("MATCH (n:P {id: 'B'}) RETURN n.id AS k", &p())
        .unwrap();
    db.submit_batch(vec![BatchOp::SetProp {
        key: "B".into(),
        field: "x".into(),
        value: Value::Int(1),
    }])
    .unwrap();
    for i in 0..(mushroomdb::FOLD_EVERY_K + 3) {
        db.submit_batch(vec![node(&format!("filler{i}"))]).unwrap();
    }
    db.write().snapshot().unwrap();
    quiet
        .execute("CREATE (n:P {id: 'from-quiet'})", &p())
        .unwrap();
    quiet
        .execute("MATCH (n:P {id: 'A'}) SET n.x = 9", &p())
        .unwrap();
    loud.execute("CREATE (n:P {id: 'from-loud'})", &p())
        .unwrap();

    let seq = quiet
        .commit()
        .expect("unchanged keys commit across a snapshot");
    assert_eq!(seq, db.reader().version);
    assert!(has_key(&db, "from-quiet"));
    assert!(matches!(
        loud.commit(),
        Err(GraphError::CasConflict { ref key, .. }) if key == "B"
    ));
    assert!(!has_key(&db, "from-loud"));

    // And after a reopen the committed transaction is there.
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert_eq!(
        reopened
            .query("MATCH (n:P {id: 'A'}) RETURN n.x AS x", &p())
            .unwrap()
            .get(0, "x"),
        Some(&Value::Int(9))
    );
}

/// Child half of `crash_before_commit_no_data`: does nothing unless the parent
/// set the environment variable.
#[test]
fn crash_child_open_tx_then_abort() {
    let Ok(dir) = std::env::var("MUSHROOMDB_TX_CRASH_DIR") else {
        return;
    };
    let db = SharedDb::open(std::path::Path::new(&dir)).unwrap();
    db.submit_batch(vec![node("durable")]).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (n:P {id: 'in-flight'})", &p()).unwrap();
    tx.execute("MATCH (n:P {id: 'durable'}) SET n.x = 1", &p())
        .unwrap();
    // The process dies with the transaction open: nothing was submitted.
    std::process::abort();
}

/// An abort-before-commit in a subprocess (the sim harness's `SimFs` cannot
/// reach `SharedDb`, which is `RealFs`-only): the process dies with an open
/// transaction, and the reopened store holds the commit before it and nothing
/// of the transaction.
#[test]
fn crash_before_commit_no_data() {
    let dir = tmp("crash");
    std::fs::create_dir_all(&dir).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["crash_child_open_tx_then_abort", "--exact", "--nocapture"])
        .env("MUSHROOMDB_TX_CRASH_DIR", &dir)
        .status()
        .unwrap();
    assert!(!status.success(), "the child must die, not exit cleanly");
    let db = GraphDb::open(&dir).unwrap();
    assert!(
        db.node_info("durable").is_some(),
        "the earlier commit survives"
    );
    assert!(
        db.node_info("in-flight").is_none(),
        "the open transaction left data"
    );
    assert_eq!(
        db.query("MATCH (n:P {id: 'durable'}) RETURN n.x AS x", &p())
            .unwrap()
            .get(0, "x"),
        None
    );
}

// ── Further pins ─────────────────────────────────────────────────────────────

/// The commit-path error the Review Focus names: the group fsync fails, the
/// drain thread truncates, the transaction is consumed, and a reopen shows
/// nothing — a retry has nothing to double-apply.
#[test]
fn commit_fsync_failure_leaves_nothing() {
    let dir = tmp("fsync-fail");
    {
        let db = SharedDb::open(&dir).unwrap();
        db.submit_batch(vec![node("seed")]).unwrap();
    }
    let db =
        SharedDb::open_with_test_sync(&dir, |_| Err(std::io::Error::other("injected"))).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (n:P {id: 'lost'})", &p()).unwrap();
    assert!(tx.commit().is_err());
    drop(db);
    let reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.node_info("seed").is_some());
    assert!(reopened.node_info("lost").is_none());
}

#[test]
fn change_between_begin_and_first_read_conflicts() {
    let dir = tmp("begin-then-change");
    let db = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![node("A"), node("B")]).unwrap();
    let mut tx = db.begin_transaction();
    db.submit_batch(vec![BatchOp::SetProp {
        key: "A".into(),
        field: "x".into(),
        value: Value::Int(1),
    }])
    .unwrap();
    // The read sees the base snapshot (no x) …
    let rs = tx
        .execute("MATCH (n:P {id: 'A'}) RETURN n.x AS x", &p())
        .unwrap();
    assert_eq!(rs.get(0, "x"), None);
    tx.execute("MATCH (n:P {id: 'B'}) SET n.x = 2", &p())
        .unwrap();
    // … so the version it records is the base snapshot's, and the commit fails.
    assert!(matches!(
        tx.commit(),
        Err(GraphError::CasConflict { ref key, .. }) if key == "A"
    ));
}

#[test]
fn absent_key_concurrent_insert_conflicts() {
    let dir = tmp("absent-insert");
    let db = SharedDb::open(&dir).unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("MERGE (n:P {id: 'X'}) ON CREATE SET n.origin = 'tx'", &p())
        .unwrap();
    db.submit_batch(vec![node("X")]).unwrap();
    assert!(matches!(
        tx.commit(),
        Err(GraphError::CasConflict { ref key, expected: 0, .. }) if key == "X"
    ));
}

#[test]
fn concurrent_edge_insert_conflicts_through_endpoint() {
    let dir = tmp("edge-endpoint");
    let db = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![node("a"), node("b"), node("z")])
        .unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("MATCH (n:P {id: 'b'}) RETURN n.id AS k", &p())
        .unwrap();
    tx.execute("MATCH (n:P {id: 'z'}) SET n.x = 1", &p())
        .unwrap();
    // No edge precondition exists: the insert reaches the tx through `b`.
    db.submit_batch(vec![BatchOp::InsertEdge {
        edge_type: "KNOWS".into(),
        src_key: "a".into(),
        dst_key: "b".into(),
    }])
    .unwrap();
    assert!(matches!(
        tx.commit(),
        Err(GraphError::CasConflict { ref key, .. }) if key == "b"
    ));
}

/// Snapshot isolation's documented limit: a read records the keys it matched,
/// not the predicate, so two transactions that each see no `Leader` and each
/// create one both commit (a phantom).
#[test]
fn write_skew_through_label_scan_is_possible() {
    let dir = tmp("write-skew");
    let db = SharedDb::open(&dir).unwrap();
    let mut t1 = db.begin_transaction();
    let mut t2 = db.begin_transaction();
    for tx in [&mut t1, &mut t2] {
        let rs = tx
            .execute("MATCH (n:Leader) RETURN count(n) AS c", &p())
            .unwrap();
        assert_eq!(rs.get(0, "c"), Some(&Value::Int(0)));
    }
    t1.execute("CREATE (n:Leader {id: 'l1'})", &p()).unwrap();
    t2.execute("CREATE (n:Leader {id: 'l2'})", &p()).unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
    let rs = db
        .reader()
        .query("MATCH (n:Leader) RETURN count(n) AS c", &p())
        .unwrap();
    assert_eq!(rs.get(0, "c"), Some(&Value::Int(2)));
}

#[test]
fn rule_derived_edges_are_not_visible_inside_tx() {
    let dir = tmp("rules-at-commit");
    let db = SharedDb::open(&dir).unwrap();
    db.write()
        .create_rule(RuleDef {
            name: "same_tags".into(),
            src_label: "P".into(),
            dst_label: "P".into(),
            predicate: Predicate::Overlap {
                field: "tags".into(),
                min: 1.0,
            },
            edge_type: "SAME".into(),
            weight_prop: None,
            max_edges: None,
            approximate: false,
            via_label: None,
            via_edge: None,
            via_dir: None,
            namespace: None,
        })
        .unwrap();
    let derived = "MATCH (a:P)-[:SAME]->(b:P) RETURN a.id AS s, b.id AS d";
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (a:P {id: 'a', tags: ['t']})", &p())
        .unwrap();
    tx.execute("CREATE (b:P {id: 'b', tags: ['t']})", &p())
        .unwrap();
    assert_eq!(tx.execute(derived, &p()).unwrap().len(), 0);
    tx.commit().unwrap();
    assert!(!db.reader().query(derived, &p()).unwrap().is_empty());
}

#[test]
fn unsupported_write_form_is_refused_and_buffers_nothing() {
    let dir = tmp("unsupported");
    let db = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![node("a")]).unwrap();
    let mut tx = db.begin_transaction();
    let err = tx
        .execute("MATCH (n:P {id: 'a'}) SET n.x = 1 RETURN n.x AS x", &p())
        .unwrap_err();
    match err {
        GraphError::QueryError { detail } => assert_eq!(
            detail,
            "not supported in mushroomdb 0.10 inside a transaction: MATCH … SET … RETURN"
        ),
        other => panic!("expected QueryError, got {other:?}"),
    }
    let frames = mushroomdb::wal_commit_count_at(&dir).unwrap();
    let seq = tx.commit().unwrap();
    assert_eq!(
        seq,
        db.reader().version,
        "nothing buffered: commit is a no-op"
    );
    assert_eq!(mushroomdb::wal_commit_count_at(&dir).unwrap(), frames);
    assert_eq!(db.reader().node_info("a").unwrap().props.get("x"), None);
}

#[test]
fn commit_returns_its_own_commit_number_under_concurrency() {
    let dir = tmp("own-seq");
    let db = SharedDb::open(&dir).unwrap();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let db = db.clone();
            std::thread::spawn(move || {
                let mut tx = db.begin_transaction();
                tx.execute(&format!("CREATE (n:P {{id: 't{i}'}})"), &p())
                    .unwrap();
                tx.commit().unwrap()
            })
        })
        .collect();
    let mut seqs: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    seqs.sort();
    seqs.dedup();
    assert_eq!(
        seqs.len(),
        8,
        "every commit has a distinct number: {seqs:?}"
    );
    // Each number is the frame that holds that transaction's node.
    drop(db);
    for seq in seqs {
        let at = GraphDb::open_at(&dir, seq - 1).unwrap();
        let before = if seq >= 2 {
            GraphDb::open_at(&dir, seq - 2)
                .unwrap()
                .query("MATCH (n:P) RETURN count(n) AS c", &p())
                .unwrap()
        } else {
            let mut rs = mushroomdb::ResultSet::new(vec!["c".into()]);
            rs.push_row(vec![Some(Value::Int(0))]);
            rs
        };
        let after = at.query("MATCH (n:P) RETURN count(n) AS c", &p()).unwrap();
        let n = |rs: &mushroomdb::ResultSet| match rs.get(0, "c") {
            Some(Value::Int(n)) => *n,
            _ => 0,
        };
        assert_eq!(n(&after), n(&before) + 1);
    }
}

/// Commit cost of a transaction against `write_batch` of the same ops (the W3
/// gate: ≤ 110 %), with the store's default fsync policy and with `Relaxed`
/// (no fsync, so CPU only). The order of the three paths rotates per run.
/// Release build:
/// `cargo test --release -p mushroomdb --test transaction_integration bench_ -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_tx_commit_vs_write_batch() {
    const N: usize = 1_000;
    const RUNS: usize = 21;
    let median = |mut xs: Vec<u128>| {
        xs.sort();
        xs[xs.len() / 2]
    };
    let props = |i: usize| {
        vec![
            ("id".to_string(), Value::Str(format!("k{i}"))),
            ("x".to_string(), Value::Int(i as i64)),
        ]
    };
    let open = |name: String, relaxed: bool| {
        let db = SharedDb::open(&tmp(&name)).unwrap();
        if relaxed {
            db.write()
                .set_fsync_policy(mushroomdb::FsyncPolicy::Relaxed);
        }
        db
    };
    for relaxed in [false, true] {
        let mut tx_us = Vec::new();
        let mut wb_us = Vec::new();
        let mut sb_us = Vec::new();
        for run in 0..RUNS {
            for path in 0..3 {
                match (run + path) % 3 {
                    0 => {
                        let db = open(format!("bench-tx-{relaxed}-{run}"), relaxed);
                        let mut tx = db.begin_transaction();
                        for i in 0..N {
                            tx.execute(&format!("CREATE (n:P {{id: 'k{i}', x: {i}}})"), &p())
                                .unwrap();
                        }
                        let t = std::time::Instant::now();
                        tx.commit().unwrap();
                        tx_us.push(t.elapsed().as_micros());
                    }
                    1 => {
                        let db = open(format!("bench-wb-{relaxed}-{run}"), relaxed);
                        let t = std::time::Instant::now();
                        db.write()
                            .write_batch(|b| {
                                for i in 0..N {
                                    b.insert_node("P", &format!("k{i}"), props(i));
                                }
                            })
                            .unwrap();
                        wb_us.push(t.elapsed().as_micros());
                    }
                    _ => {
                        let db = open(format!("bench-sb-{relaxed}-{run}"), relaxed);
                        let ops = (0..N)
                            .map(|i| BatchOp::InsertNode {
                                label: "P".into(),
                                key: format!("k{i}"),
                                props: props(i),
                            })
                            .collect();
                        let t = std::time::Instant::now();
                        db.submit_batch(ops).unwrap();
                        sb_us.push(t.elapsed().as_micros());
                    }
                }
            }
        }
        let (tx_m, wb_m, sb_m) = (median(tx_us), median(wb_us), median(sb_us));
        println!(
            "relaxed={relaxed} N={N} runs={RUNS} median tx_commit={tx_m}us \
             write_batch={wb_m}us submit_batch={sb_m}us tx/write_batch={:.3} \
             tx/submit_batch={:.3}",
            tx_m as f64 / wb_m as f64,
            tx_m as f64 / sb_m as f64
        );
    }
}

/// A `DETACH DELETE` advances no neighbour's `last_change` (the WAL holds only
/// the `DeleteNode`), so a transaction that touched only the neighbour commits:
/// it never read the removed edge, which snapshot isolation allows.
#[test]
fn neighbour_of_concurrent_detach_delete_does_not_conflict() {
    let dir = tmp("detach-neighbour");
    let db = SharedDb::open(&dir).unwrap();
    db.submit_batch(vec![
        node("x"),
        node("y"),
        BatchOp::InsertEdge {
            edge_type: "KNOWS".into(),
            src_key: "x".into(),
            dst_key: "y".into(),
        },
    ])
    .unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("MATCH (n:P {id: 'y'}) SET n.v = 1", &p())
        .unwrap();
    db.write()
        .query_write("MATCH (n:P {id: 'x'}) DETACH DELETE n", &p())
        .unwrap();
    tx.commit()
        .expect("the neighbour's last_change did not move");
    let y = db.reader().node_info("y").unwrap();
    assert_eq!(y.props.get("v"), Some(&Value::Int(1)));
    assert!(db.reader().node_info("x").is_none());

    // The other direction: a transaction's own DETACH DELETE records the
    // neighbour, so a concurrent write to it conflicts.
    db.submit_batch(vec![
        node("a"),
        node("b"),
        BatchOp::InsertEdge {
            edge_type: "KNOWS".into(),
            src_key: "a".into(),
            dst_key: "b".into(),
        },
    ])
    .unwrap();
    let mut tx = db.begin_transaction();
    tx.execute("MATCH (n:P {id: 'a'}) DETACH DELETE n", &p())
        .unwrap();
    db.submit_batch(vec![BatchOp::SetProp {
        key: "b".into(),
        field: "v".into(),
        value: Value::Int(2),
    }])
    .unwrap();
    assert!(matches!(
        tx.commit(),
        Err(GraphError::CasConflict { ref key, .. }) if key == "b"
    ));
}

/// A transaction owns its handle: it can be stored in a map and moved to
/// another thread while open (the shape TX-2/TX-4/TX-12 need).
#[test]
fn transaction_is_owned_and_send() {
    let dir = tmp("owned-send");
    let db = SharedDb::open(&dir).unwrap();
    let mut open: std::collections::HashMap<u64, mushroomdb::Transaction> =
        std::collections::HashMap::new();
    let mut tx = db.begin_transaction();
    tx.execute("CREATE (n:P {id: 'moved'})", &p()).unwrap();
    open.insert(1, tx);
    drop(db); // the transaction keeps the store alive on its own
    let tx = open.remove(&1).unwrap();
    let seq = std::thread::spawn(move || tx.commit().unwrap())
        .join()
        .unwrap();
    assert!(seq > 0);
    let reopened = GraphDb::open(&dir).unwrap();
    assert!(reopened.node_info("moved").is_some());
}
