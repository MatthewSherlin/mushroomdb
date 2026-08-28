use core_api::{BatchOp, GraphDb, GraphError, Precondition, SharedDb, Value};
use std::sync::{Arc, Barrier};
use std::thread;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-cas-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn set_prop_op(key: &str, field: &str, value: Value) -> Vec<BatchOp> {
    vec![BatchOp::SetProp {
        key: key.into(),
        field: field.into(),
        value,
    }]
}

// --- last_changed tracking ---

#[test]
fn last_changed_returns_none_for_unknown_node() {
    let dir = tmp("lc-unknown");
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(db.last_changed("nonexistent"), None);
}

#[test]
fn last_changed_increments_on_insert() {
    let dir = tmp("lc-insert");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    let seq = db.last_changed("a").expect("node must have a commit seq");
    assert!(seq > 0);
}

#[test]
fn last_changed_updates_on_set_prop() {
    let dir = tmp("lc-setprop");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    let seq1 = db.last_changed("a").unwrap();
    db.set_prop("a", "x", Value::Int(1)).unwrap();
    let seq2 = db.last_changed("a").unwrap();
    assert!(seq2 > seq1, "seq must increase after set_prop");
}

#[test]
fn last_changed_updates_on_remove_prop() {
    let dir = tmp("lc-removeprop");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![("x".into(), Value::Int(1))])
        .unwrap();
    let seq1 = db.last_changed("a").unwrap();
    db.remove_prop("a", "x").unwrap();
    let seq2 = db.last_changed("a").unwrap();
    assert!(seq2 > seq1, "seq must increase after remove_prop");
}

#[test]
fn edge_insert_touches_both_endpoints() {
    let dir = tmp("lc-edge");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "src", vec![]).unwrap();
    db.insert_node("N", "dst", vec![]).unwrap();
    let src_seq1 = db.last_changed("src").unwrap();
    let dst_seq1 = db.last_changed("dst").unwrap();
    db.insert_edge("E", "src", "dst").unwrap();
    let src_seq2 = db.last_changed("src").unwrap();
    let dst_seq2 = db.last_changed("dst").unwrap();
    assert!(src_seq2 > src_seq1, "edge insert must touch src");
    assert!(dst_seq2 > dst_seq1, "edge insert must touch dst");
}

#[test]
fn edge_delete_touches_both_endpoints() {
    let dir = tmp("lc-edge-del");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "src", vec![]).unwrap();
    db.insert_node("N", "dst", vec![]).unwrap();
    db.insert_edge("E", "src", "dst").unwrap();
    let src_seq1 = db.last_changed("src").unwrap();
    let dst_seq1 = db.last_changed("dst").unwrap();
    db.delete_edge("E", "src", "dst").unwrap();
    let src_seq2 = db.last_changed("src").unwrap();
    let dst_seq2 = db.last_changed("dst").unwrap();
    assert!(src_seq2 > src_seq1, "edge delete must touch src");
    assert!(dst_seq2 > dst_seq1, "edge delete must touch dst");
}

// --- write_batch_cas: direct GraphDb path ---

#[test]
fn cas_success_when_expected_matches() {
    let dir = tmp("cas-success");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    let seq = db.last_changed("a").unwrap();
    let preconds = vec![Precondition::NodeUnchangedSince {
        key: "a".into(),
        expected: seq,
    }];
    let ops = set_prop_op("a", "x", Value::Int(99));
    let result = db.write_batch_cas(preconds, ops);
    assert!(
        result.is_ok(),
        "CAS should succeed when seq matches: {result:?}"
    );
    assert_eq!(db.get_prop("a", "x"), Some(Value::Int(99)));
}

#[test]
fn cas_conflict_when_seq_mismatch() {
    let dir = tmp("cas-mismatch");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    let seq = db.last_changed("a").unwrap();
    // Use a stale expected value (seq is at least 1, so 0 is always stale)
    let stale_seq = if seq > 0 { seq - 1 } else { seq + 1 };
    let preconds = vec![Precondition::NodeUnchangedSince {
        key: "a".into(),
        expected: stale_seq,
    }];
    let ops = set_prop_op("a", "x", Value::Int(1));
    let err = db.write_batch_cas(preconds, ops).unwrap_err();
    match err {
        GraphError::CasConflict {
            key,
            expected,
            actual,
        } => {
            assert_eq!(key, "a");
            assert_eq!(expected, stale_seq);
            assert_eq!(actual, seq);
        }
        other => panic!("expected CasConflict, got {other:?}"),
    }
    // The batch must not have been applied
    assert_eq!(db.get_prop("a", "x"), None);
}

#[test]
fn cas_node_absent_conflict_when_node_exists() {
    let dir = tmp("cas-absent-conflict");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    let preconds = vec![Precondition::NodeAbsent { key: "a".into() }];
    let ops = set_prop_op("a", "y", Value::Int(2));
    let err = db.write_batch_cas(preconds, ops).unwrap_err();
    match err {
        GraphError::CasConflict {
            key,
            expected,
            actual,
        } => {
            assert_eq!(key, "a");
            // expected = u64::MAX sentinel for NodeAbsent
            assert_eq!(expected, u64::MAX);
            assert!(actual > 0, "actual must be non-zero for an existing node");
        }
        other => panic!("expected CasConflict, got {other:?}"),
    }
}

#[test]
fn cas_node_absent_succeeds_for_missing_node() {
    let dir = tmp("cas-absent-ok");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "other", vec![]).unwrap();
    let preconds = vec![Precondition::NodeAbsent { key: "new".into() }];
    let ops = vec![BatchOp::InsertNode {
        label: "N".into(),
        key: "new".into(),
        props: vec![],
    }];
    let result = db.write_batch_cas(preconds, ops);
    assert!(
        result.is_ok(),
        "NodeAbsent should pass when node doesn't exist: {result:?}"
    );
    assert!(db.has_node("new"), "new node must be inserted");
}

#[test]
fn cas_failing_precond_does_not_write_wal_frame() {
    // Multi-op batch with one failing precondition: NEITHER op must be applied
    // and NO WAL frame must be written.
    let dir = tmp("cas-nowal");
    let wal_len_before;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.snapshot().unwrap(); // flush WAL
        wal_len_before = std::fs::metadata(dir.join("wal.bin"))
            .map(|m| m.len())
            .unwrap_or(0);
    }
    let mut db = GraphDb::open(&dir).unwrap();
    let preconds = vec![Precondition::NodeUnchangedSince {
        key: "a".into(),
        expected: 0, // always stale since seq >= 1
    }];
    // Two ops in the batch: neither must apply on conflict
    let ops = vec![
        BatchOp::SetProp {
            key: "a".into(),
            field: "x".into(),
            value: Value::Int(1),
        },
        BatchOp::SetProp {
            key: "b".into(),
            field: "y".into(),
            value: Value::Int(2),
        },
    ];
    let err = db.write_batch_cas(preconds, ops).unwrap_err();
    assert!(
        matches!(err, GraphError::CasConflict { .. }),
        "expected CasConflict, got {err:?}"
    );
    // State unchanged: neither prop must have been written
    assert_eq!(db.get_prop("a", "x"), None, "op 1 must not have applied");
    assert_eq!(db.get_prop("b", "y"), None, "op 2 must not have applied");
    // WAL must not have grown
    let wal_len_after = std::fs::metadata(dir.join("wal.bin"))
        .map(|m| m.len())
        .unwrap_or(0);
    assert_eq!(
        wal_len_before, wal_len_after,
        "CAS conflict must not append a WAL frame"
    );
}

// --- last_change persists through snapshot + reopen (V8 section round-trip) ---

#[test]
fn last_change_survives_snapshot_and_reopen() {
    let dir = tmp("lc-snapshot");
    let expected_seq_a;
    let expected_seq_b;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.insert_node("N", "b", vec![]).unwrap();
        db.set_prop("a", "x", Value::Int(1)).unwrap();
        expected_seq_a = db.last_changed("a").unwrap();
        expected_seq_b = db.last_changed("b").unwrap();
        db.snapshot().unwrap();
    }
    // Reopen: should load LAST_CHANGE from V8 section 11
    let db = GraphDb::open(&dir).unwrap();
    assert_eq!(
        db.last_changed("a"),
        Some(expected_seq_a),
        "last_changed for 'a' must match after snapshot + reopen"
    );
    assert_eq!(
        db.last_changed("b"),
        Some(expected_seq_b),
        "last_changed for 'b' must match exactly after snapshot + reopen"
    );
}

#[test]
fn last_change_wal_tail_appended_after_snapshot() {
    let dir = tmp("lc-wal-tail");
    let snapshot_seq;
    {
        let mut db = GraphDb::open(&dir).unwrap();
        db.insert_node("N", "a", vec![]).unwrap();
        db.snapshot().unwrap();
        snapshot_seq = db.last_changed("a").unwrap();
        // Write more after snapshot; seq should increase
        db.set_prop("a", "x", Value::Int(7)).unwrap();
    }
    let db = GraphDb::open(&dir).unwrap();
    let recovered_seq = db.last_changed("a").unwrap();
    assert!(
        recovered_seq > snapshot_seq,
        "seq from WAL tail ({recovered_seq}) must exceed snapshot seq ({snapshot_seq})"
    );
}

// --- SharedDb submit_batch_cas ---

#[test]
fn shared_submit_batch_cas_success() {
    let dir = tmp("sdb-cas-ok");
    let db = SharedDb::open(&dir).unwrap();
    db.write().insert_node("N", "a", vec![]).unwrap();
    let seq = db.read().last_changed("a").unwrap();
    let preconds = vec![Precondition::NodeUnchangedSince {
        key: "a".into(),
        expected: seq,
    }];
    let ops = set_prop_op("a", "z", Value::Int(42));
    let result = db.submit_batch_cas(preconds, ops);
    assert!(result.is_ok(), "SharedDb CAS must succeed: {result:?}");
    assert_eq!(db.read().get_prop("a", "z"), Some(Value::Int(42)));
}

#[test]
fn shared_submit_batch_cas_conflict() {
    let dir = tmp("sdb-cas-conflict");
    let db = SharedDb::open(&dir).unwrap();
    db.write().insert_node("N", "a", vec![]).unwrap();
    let preconds = vec![Precondition::NodeUnchangedSince {
        key: "a".into(),
        expected: 0, // stale
    }];
    let ops = set_prop_op("a", "z", Value::Int(1));
    let err = db.submit_batch_cas(preconds, ops).unwrap_err();
    assert!(
        matches!(err, GraphError::CasConflict { .. }),
        "SharedDb must return CasConflict on stale seq: {err:?}"
    );
    assert_eq!(
        db.read().get_prop("a", "z"),
        None,
        "conflicted write must not apply"
    );
}

#[test]
fn shared_cas_race_only_one_writer_wins() {
    // Two threads concurrently submit CAS with the same expected seq.
    // Exactly one must succeed and one must get CasConflict.
    let dir = tmp("sdb-cas-race");
    let db = SharedDb::open(&dir).unwrap();
    db.write().insert_node("N", "a", vec![]).unwrap();
    let seq = db.read().last_changed("a").unwrap();

    let n_threads = 4;
    let barrier = Arc::new(Barrier::new(n_threads));
    let db = Arc::new(db);

    let handles: Vec<_> = (0..n_threads)
        .map(|i| {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let preconds = vec![Precondition::NodeUnchangedSince {
                    key: "a".into(),
                    expected: seq,
                }];
                let ops = vec![BatchOp::SetProp {
                    key: "a".into(),
                    field: "winner".into(),
                    value: Value::Int(i as i64),
                }];
                db.submit_batch_cas(preconds, ops)
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let successes = results.iter().filter(|r| r.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, Err(GraphError::CasConflict { .. })))
        .count();

    assert_eq!(
        successes, 1,
        "exactly one CAS must succeed among {n_threads} concurrent writers; got {successes}"
    );
    assert_eq!(
        conflicts,
        n_threads - 1,
        "remaining threads must get CasConflict; got {conflicts}"
    );
}

// --- M3: delete_node behavior and ABA pattern ---

#[test]
fn delete_node_returns_none_for_last_changed() {
    let dir = tmp("lc-delete");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("N", "a", vec![]).unwrap();
    assert!(
        db.last_changed("a").is_some(),
        "node must have a seq before deletion"
    );
    db.delete_node("a").unwrap();
    assert_eq!(
        db.last_changed("a"),
        None,
        "last_changed must return None for a deleted node"
    );
}

#[test]
fn cas_aba_reinserted_node_conflicts_with_old_seq() {
    // ABA pattern: insert → delete → reinsert with same key.
    // A CAS with the pre-delete seq must conflict with the new post-reinsert seq.
    let dir = tmp("cas-aba");
    let mut db = GraphDb::open(&dir).unwrap();

    db.insert_node("N", "a", vec![]).unwrap();
    let pre_delete_seq = db.last_changed("a").unwrap();

    db.delete_node("a").unwrap();
    // Node is gone; last_changed returns None.
    assert_eq!(db.last_changed("a"), None);

    db.insert_node("N", "a", vec![]).unwrap();
    let post_reinsert_seq = db.last_changed("a").unwrap();
    assert!(
        post_reinsert_seq > pre_delete_seq,
        "reinserted node must get a strictly higher seq"
    );

    // CAS with the pre-delete seq must conflict with the new seq.
    let preconds = vec![Precondition::NodeUnchangedSince {
        key: "a".into(),
        expected: pre_delete_seq,
    }];
    let ops = set_prop_op("a", "x", Value::Int(99));
    let err = db.write_batch_cas(preconds, ops).unwrap_err();
    match err {
        GraphError::CasConflict {
            key,
            expected,
            actual,
        } => {
            assert_eq!(key, "a");
            assert_eq!(expected, pre_delete_seq);
            assert_eq!(actual, post_reinsert_seq);
        }
        other => panic!("expected CasConflict on ABA pattern, got {other:?}"),
    }
}

// --- I2: drain loop preserves group batching for non-CAS submissions ---

#[test]
fn non_cas_submissions_batched_into_single_group_call() {
    // N concurrent non-CAS submit_batch calls must land as N WAL Batch frames
    // and complete with all nodes committed — the same observable state as N
    // sequential single writes, regardless of internal batching structure.
    use core_storage::wal::decode_all;

    let dir = tmp("drain-batching");
    let db = SharedDb::open(&dir).unwrap();

    // Pre-insert a root node so the DB directory is initialized.
    db.write().insert_node("N", "root", vec![]).unwrap();
    db.write().snapshot().unwrap(); // truncate WAL to zero known length

    let n = 8usize;
    let barrier = Arc::new(Barrier::new(n));
    let db = Arc::new(db);

    let handles: Vec<_> = (0..n)
        .map(|i| {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait(); // all threads start simultaneously → land in same drain group
                let ops = vec![BatchOp::InsertNode {
                    label: "N".into(),
                    key: format!("n{i}"),
                    props: vec![],
                }];
                db.submit_batch(ops)
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(
        results.iter().all(|r| r.is_ok()),
        "all non-CAS submissions must succeed: {results:?}"
    );

    // Verify all nodes committed.
    let db = match Arc::try_unwrap(db) {
        Ok(d) => d,
        Err(_) => panic!("Arc still held — bug in test setup"),
    };
    let guard = db.read();
    for i in 0..n {
        assert!(
            guard.has_node(&format!("n{i}")),
            "node n{i} must be committed"
        );
    }
    drop(guard);

    // Verify WAL frame count = N (one Batch frame per submission).
    let wal = std::fs::read(dir.join("wal.bin")).unwrap();
    let (records, _) = decode_all(&wal);
    let batch_frames = records
        .iter()
        .filter(|r| matches!(r, core_storage::wal::WalRecord::Batch(_)))
        .count();
    assert_eq!(
        batch_frames, n,
        "N non-CAS submissions must produce exactly N WAL Batch frames; got {batch_frames}"
    );
}
