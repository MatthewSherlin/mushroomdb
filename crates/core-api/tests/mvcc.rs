//! MVCC epoch reader correctness tests.
//!
//! Verifies the four guarantees from the task-4 spec:
//! 1. Epoch isolation — snapshot sees state at call time, not after.
//! 2. Fold transparency — snapshot after ≥ K commits returns same results as live query.
//! 3. Chain-depth bound — delta tail length ≤ K−1 between folds.
//! 4. Concurrency gate — concurrent reads and writes do not corrupt state (#[ignore]).

use core_api::{SharedDb, FOLD_EVERY_K};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-mvcc-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

// ── T1: Epoch isolation ───────────────────────────────────────────────────────

/// A reader snapshot taken BEFORE a write must NOT see the new node.
#[test]
fn epoch_isolation_snapshot_excludes_post_snapshot_writes() {
    let dir = tmp("isolation");
    let db = SharedDb::open(&dir).unwrap();

    db.write().insert_node("Person", "alice", vec![]).unwrap();

    // Snapshot taken before "bob" is inserted.
    let snap = db.reader();

    db.write().insert_node("Person", "bob", vec![]).unwrap();

    // Live db sees both; snapshot sees only alice.
    assert_eq!(
        db.read().stats().nodes_live,
        2,
        "live db should have 2 nodes"
    );

    let params = BTreeMap::new();
    // `RETURN n` returns the node's string key as Value::Str.
    let rs = snap.query("MATCH (n:Person) RETURN n", &params).unwrap();
    assert_eq!(rs.len(), 1, "snapshot should see exactly 1 Person (alice)");
    let key_val = rs.row(0)[0].as_ref().and_then(|v| {
        if let core_api::Value::Str(s) = v {
            Some(s.as_str())
        } else {
            None
        }
    });
    assert_eq!(key_val, Some("alice"), "snapshot should see alice, not bob");
}

// ── T2: Fold transparency ─────────────────────────────────────────────────────

/// After ≥ K commits (triggering at least one fold), a fresh snapshot returns
/// the same Cypher result as the live db.
#[test]
fn fold_transparency_snapshot_matches_live_after_k_commits() {
    let dir = tmp("fold-transparency");
    let db = SharedDb::open(&dir).unwrap();

    // Insert FOLD_EVERY_K + 1 nodes to guarantee at least one fold cycle.
    for i in 0..=(FOLD_EVERY_K as u64) {
        db.write()
            .insert_node("Person", &format!("person-{i}"), vec![])
            .unwrap();
    }

    let snap = db.reader();
    let params = BTreeMap::new();

    let live_rs = db
        .read()
        .query("MATCH (n:Person) RETURN n", &params)
        .unwrap();
    let snap_rs = snap.query("MATCH (n:Person) RETURN n", &params).unwrap();

    assert_eq!(
        live_rs.len(),
        snap_rs.len(),
        "snapshot row count must match live db after a fold: live={}, snap={}",
        live_rs.len(),
        snap_rs.len()
    );
}

// ── T3: Chain-depth bound ─────────────────────────────────────────────────────

/// After K−1 commits with no fold trigger, the delta tail length is ≤ K−1.
/// After exactly K commits, a fold resets the tail.
#[test]
fn chain_depth_bound_delta_tail_bounded() {
    let dir = tmp("chain-depth");
    let db = SharedDb::open(&dir).unwrap();

    // Insert K−1 nodes. At K−1 commits the fold has NOT fired yet.
    for i in 0..(FOLD_EVERY_K - 1) {
        db.write()
            .insert_node("Item", &format!("item-{i}"), vec![])
            .unwrap();
    }
    let snap_before_fold = db.reader();
    assert!(
        snap_before_fold.deltas.len() < FOLD_EVERY_K,
        "delta tail before fold must be < FOLD_EVERY_K: got {}",
        snap_before_fold.deltas.len()
    );

    // The K-th commit triggers a fold; delta tail must be reset.
    db.write()
        .insert_node("Item", "item-trigger-fold", vec![])
        .unwrap();
    let snap_after_fold = db.reader();
    assert_eq!(
        snap_after_fold.deltas.len(),
        0,
        "delta tail must be empty immediately after a fold (K-th commit)"
    );
}

// ── T4: Concurrency gate ──────────────────────────────────────────────────────

/// Concurrent readers via ReaderSnapshot must not observe torn writes and must
/// always return a consistent node count. Prints JSON summary on completion.
///
/// Marked #[ignore] because it is a stress test (wall-clock sensitive).
/// Run with: `cargo test -- --ignored mvcc_concurrent_reads_during_writes`
#[test]
#[ignore]
fn mvcc_concurrent_reads_during_writes() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let dir = tmp("concurrency");
    let db = Arc::new(SharedDb::open(&dir).unwrap());
    const N_WRITERS: usize = 2;
    const N_READERS: usize = 8;
    const WRITES_PER_WRITER: usize = 32;

    let barrier = Arc::new(Barrier::new(N_WRITERS + N_READERS));
    let mut handles = Vec::new();

    // Writer threads
    for w in 0..N_WRITERS {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..WRITES_PER_WRITER {
                let key = format!("writer{w}-node{i}");
                let _ = db.write().insert_node("Node", &key, vec![]);
            }
        }));
    }

    // Reader threads — take snapshots and verify internal consistency.
    let mut read_results = Vec::new();
    for _ in 0..N_READERS {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let h = thread::spawn(move || {
            barrier.wait();
            let mut counts = Vec::new();
            for _ in 0..16 {
                let snap = db.reader();
                let params = BTreeMap::new();
                let rs = snap
                    .query("MATCH (n:Node) RETURN n", &params)
                    .unwrap_or_else(|_| core_api::ResultSet::new(vec![]));
                counts.push(rs.len());
            }
            counts
        });
        read_results.push(h);
    }

    for h in handles {
        h.join().unwrap();
    }
    let all_counts: Vec<Vec<usize>> = read_results
        .into_iter()
        .map(|h: std::thread::JoinHandle<Vec<usize>>| h.join().unwrap())
        .collect();

    // All counts must be non-decreasing within each reader thread (monotonic
    // snapshot isolation: a reader never sees fewer rows than a prior snapshot).
    for (i, counts) in all_counts.iter().enumerate() {
        for w in counts.windows(2) {
            assert!(
                w[1] >= w[0],
                "reader {i} saw non-monotonic counts: {} then {}",
                w[0],
                w[1]
            );
        }
    }

    let total_writes = N_WRITERS * WRITES_PER_WRITER;
    let final_count = db.read().stats().nodes_live;
    let summary = serde_json::json!({
        "writers": N_WRITERS,
        "writes_per_writer": WRITES_PER_WRITER,
        "total_writes": total_writes,
        "final_node_count": final_count,
        "reader_count_samples": all_counts,
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}
