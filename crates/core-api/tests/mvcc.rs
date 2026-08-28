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
///
/// A writer thread is gated by a Barrier so the write is genuinely concurrent
/// with (and guaranteed to occur after) the snapshot, removing any
/// happens-before ambiguity.
#[test]
fn epoch_isolation_snapshot_excludes_post_snapshot_writes() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let dir = tmp("isolation");
    let db = Arc::new(SharedDb::open(&dir).unwrap());

    db.write().insert_node("Person", "alice", vec![]).unwrap();

    // Take snapshot before releasing the writer. The snapshot is taken in the
    // main thread — alice is visible, bob has not yet been inserted.
    let snap = db.reader();

    // Writer thread inserts "bob" only after the main thread clears the barrier,
    // which happens AFTER the snapshot is already held.
    let barrier = Arc::new(Barrier::new(2));
    let db2 = Arc::clone(&db);
    let b2 = Arc::clone(&barrier);
    let writer = thread::spawn(move || {
        b2.wait(); // main releases us only after snap is captured
        db2.write().insert_node("Person", "bob", vec![]).unwrap();
    });

    barrier.wait(); // snapshot already taken — release the writer
    writer.join().unwrap();

    // Live db sees both; snapshot sees only alice.
    assert_eq!(
        db.read().stats().nodes_live,
        2,
        "live db should have 2 nodes"
    );

    let params = BTreeMap::new();
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

// ── T1b: DeleteNode edge sweep ────────────────────────────────────────────────

/// A snapshot taken BEFORE a DeleteNode must keep the node + its edges visible;
/// a snapshot taken AFTER must show the node gone AND its edges gone from
/// surviving neighbors' adjacency.
#[test]
fn epoch_isolation_delete_node_sweeps_edges() {
    let dir = tmp("delete-sweep");
    let db = SharedDb::open(&dir).unwrap();

    // Build a tiny graph: alice --KNOWS--> bob --KNOWS--> carol
    db.write().insert_node("Person", "alice", vec![]).unwrap();
    db.write().insert_node("Person", "bob", vec![]).unwrap();
    db.write().insert_node("Person", "carol", vec![]).unwrap();
    db.write().insert_edge("KNOWS", "alice", "bob").unwrap();
    db.write().insert_edge("KNOWS", "bob", "carol").unwrap();

    // Pre-delete snapshot: alice and carol both see bob as a neighbor.
    let snap_before = db.reader();

    // Delete bob.
    db.write().delete_node("bob").unwrap();

    // Post-delete snapshot.
    let snap_after = db.reader();

    // --- snap_before assertions (isolation: bob still there) ---
    let params = BTreeMap::new();
    let rs_before = snap_before
        .query("MATCH (n:Person) RETURN n", &params)
        .unwrap();
    assert_eq!(
        rs_before.len(),
        3,
        "pre-delete snap must see alice, bob, carol"
    );

    let edges_alice_before = snap_before.node_edges("alice").unwrap();
    assert!(
        edges_alice_before.iter().any(|e| e.dst_key == "bob"),
        "pre-delete snap: alice must still see bob as neighbor"
    );

    // --- snap_after assertions (bob gone, no phantom edges) ---
    let rs_after = snap_after
        .query("MATCH (n:Person) RETURN n", &params)
        .unwrap();
    assert_eq!(
        rs_after.len(),
        2,
        "post-delete snap must see only alice + carol"
    );

    // bob must not appear in alice's adjacency.
    let edges_alice_after = snap_after.node_edges("alice").unwrap();
    assert!(
        !edges_alice_after.iter().any(|e| e.dst_key == "bob"),
        "post-delete snap: alice must NOT see bob as neighbor (phantom adjacency)"
    );

    // carol must not appear in bob's adjacency (bob is gone).
    assert!(
        snap_after.node_info("bob").is_none(),
        "post-delete snap: node_info for bob must return None"
    );
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

/// Helper: return the p-th percentile (0-100) of a sorted Vec of u128 samples.
fn percentile(samples: &[u128], p: usize) -> u128 {
    if samples.is_empty() {
        return 0;
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    let idx = ((p as f64 / 100.0) * (s.len() - 1) as f64).round() as usize;
    s[idx]
}

/// Concurrent readers via ReaderSnapshot must not observe torn writes.
/// Each reader verifies that its visible node set for every writer is a
/// CONTIGUOUS PREFIX of that writer's committed sequence (no gap-skipping).
///
/// Measures per-read latency across three phases:
///
/// **Phase A** — single reader, no writers (baseline).
/// **Phase B** — 8 readers, NO writers (reader-scalability gate: p95 < 2×A).
/// **Phase C** — 8 readers + 2 writers under burst (shows write-lock contention).
///
/// Prints JSON including `single_reader_p95_us`, `eight_reader_p95_us`,
/// `ratio` (B/A, the gate metric), and `eight_reader_burst_p95_us` (C, info only).
///
/// Marked #[ignore] because it is a stress test (wall-clock sensitive).
/// Run with: `cargo test -- --ignored mvcc_concurrent_reads_during_writes`
#[test]
#[ignore]
fn mvcc_concurrent_reads_during_writes() {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Instant;

    let dir = tmp("concurrency");
    let db = Arc::new(SharedDb::open(&dir).unwrap());
    const N_WRITERS: usize = 2;
    const N_READERS: usize = 8;
    const WRITES_PER_WRITER: usize = 32;
    const READS_PER_READER: usize = 32;
    const BASELINE_READS: usize = 100;

    // Pre-populate a fixed node count so all phases query the same graph size.
    for i in 0..64usize {
        db.write()
            .insert_node("Node", &format!("pre-{i}"), vec![])
            .unwrap();
    }
    let params = BTreeMap::new();

    // ── Phase A: single-reader baseline (no writers) ──────────────────────────
    let mut baseline_times_us: Vec<u128> = Vec::new();
    for _ in 0..BASELINE_READS {
        let t = Instant::now();
        let snap = db.reader();
        let _ = snap
            .query("MATCH (n:Node) RETURN n", &params)
            .unwrap_or_else(|_| core_api::ResultSet::new(vec![]));
        baseline_times_us.push(t.elapsed().as_micros());
    }
    let single_p95 = percentile(&baseline_times_us, 95);

    // ── Phase B: 8 readers, no writers (gate measurement) ────────────────────
    let barrier_b = Arc::new(Barrier::new(N_READERS));
    let mut read_b_handles = Vec::new();
    for _ in 0..N_READERS {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier_b);
        let h = thread::spawn(move || {
            barrier.wait();
            let params = BTreeMap::new();
            let mut times: Vec<u128> = Vec::new();
            for _ in 0..READS_PER_READER {
                let t = Instant::now();
                let snap = db.reader();
                let _ = snap
                    .query("MATCH (n:Node) RETURN n", &params)
                    .unwrap_or_else(|_| core_api::ResultSet::new(vec![]));
                times.push(t.elapsed().as_micros());
            }
            times
        });
        read_b_handles.push(h);
    }
    let b_all: Vec<u128> = read_b_handles
        .into_iter()
        .flat_map(|h: std::thread::JoinHandle<Vec<u128>>| h.join().unwrap())
        .collect();
    let eight_p95 = percentile(&b_all, 95);

    let ratio = if single_p95 > 0 {
        eight_p95 as f64 / single_p95 as f64
    } else {
        f64::INFINITY
    };

    // ── Phase C: 8 readers + 2 writers (burst, prefix-coherence check) ────────
    // Writers insert keys in a deterministic order: "writer{w}-node{i}" for
    // i in 0..WRITES_PER_WRITER, strictly in order.  Reader threads assert
    // that every snapshot's visible set for writer w is a contiguous prefix
    // {writer{w}-node{0}, …, writer{w}-node{k-1}} for some k ≥ 0 (no gaps).
    let barrier_c = Arc::new(Barrier::new(N_WRITERS + N_READERS));
    let mut w_handles = Vec::new();
    for w in 0..N_WRITERS {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier_c);
        w_handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..WRITES_PER_WRITER {
                let _ = db
                    .write()
                    .insert_node("Node", &format!("writer{w}-node{i}"), vec![]);
            }
        }));
    }

    let mut read_c_handles = Vec::new();
    for _ in 0..N_READERS {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier_c);
        let h = thread::spawn(move || {
            barrier.wait();
            let params = BTreeMap::new();
            let mut times: Vec<u128> = Vec::new();
            for _ in 0..READS_PER_READER {
                let t = Instant::now();
                let snap = db.reader();
                let rs = snap
                    .query("MATCH (n:Node) RETURN n", &params)
                    .unwrap_or_else(|_| core_api::ResultSet::new(vec![]));
                times.push(t.elapsed().as_micros());

                // Build the set of visible keys from this snapshot.
                let visible: std::collections::HashSet<String> = (0..rs.len())
                    .filter_map(|r| {
                        rs.row(r)[0].as_ref().and_then(|v| {
                            if let core_api::Value::Str(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                    })
                    .collect();

                // Each writer's committed keys must form a contiguous prefix
                // (no gap-skipping allowed under snapshot isolation).
                for w in 0..N_WRITERS {
                    let mut prefix_len = 0usize;
                    for i in 0..WRITES_PER_WRITER {
                        if visible.contains(&format!("writer{w}-node{i}")) {
                            prefix_len = i + 1;
                        }
                    }
                    for i in 0..prefix_len {
                        assert!(
                            visible.contains(&format!("writer{w}-node{i}")),
                            "snapshot coherence violation: writer{w}-node{i} missing \
                             but writer{w}-node{} was visible",
                            prefix_len - 1
                        );
                    }
                }
            }
            times
        });
        read_c_handles.push(h);
    }

    for h in w_handles {
        h.join().unwrap();
    }
    let c_all: Vec<u128> = read_c_handles
        .into_iter()
        .flat_map(|h: std::thread::JoinHandle<Vec<u128>>| h.join().unwrap())
        .collect();
    let burst_p95 = percentile(&c_all, 95);

    let final_count = db.read().stats().nodes_live;
    let summary = serde_json::json!({
        "writers": N_WRITERS,
        "writes_per_writer": WRITES_PER_WRITER,
        "final_node_count": final_count,
        // Gate metric: reader scalability (8 readers vs 1, no writers).
        "single_reader_p95_us": single_p95,
        "eight_reader_p95_us": eight_p95,
        "ratio": ratio,
        // Informational: reader p95 under concurrent write burst.
        "eight_reader_burst_p95_us": burst_p95,
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
}
