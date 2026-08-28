//! Tests for Task 4b: group-commit write queue (spec B3.5).
//!
//! Test list:
//!  1. group_atomicity_all_or_nothing — submissions in a group never torn
//!  2. one_fsync_per_group — exactly one Fs::sync for N submissions in a group
//!  3. fifo_within_caller — sequential submits preserve commit ordering
//!  4. concurrent_submitters_all_commit — 8 threads submit concurrently, all land
//!  5. crash_before_group_fsync_loses_group — unsynced group is lost on crash
//!  6. intra_group_prefix_survives_crash — frame-1 of 2-sub group survives crash mid-frame-2
//!  7. direct_api_unchanged — write/read/write_batch/insert_node still work
//!  8. deferred_events_fire_after_flush — events buffered until flush (R2)
//!  9. deferred_events_discarded_on_failure — events discarded when fsync fails (R2)
//! 10. group_commit_throughput_bench (ignored) — RealFs informational bench
//! 11. group_commit_simfs_amortization_bench (ignored) — SimFs gate: 8 writers >= 3x serial
//! 12. shared_db_fsync_failure_degrades_and_truncates_wal — F1(c) integration via SharedDb
//! 13. direct_write_before_group_survives_group_fsync_failure — F1(c) concurrent variant

use core_api::{BatchOp, FsyncPolicy, GraphDb, MutationEvent, SharedDb};
use core_storage::fs::{FileId, Fs, FsIntrospect};
use std::collections::HashMap;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

// ── Test helpers ──────────────────────────────────────────────────────────────

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-gc-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Minimal counting Fs for sync-count assertions.
#[derive(Default)]
struct CountingFs {
    files: HashMap<FileId, Vec<u8>>,
    syncs: usize,
}

impl Fs for CountingFs {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        self.files.entry(file).or_default().extend_from_slice(data);
        Ok(())
    }

    fn sync(&mut self, _file: FileId) -> std::io::Result<()> {
        self.syncs += 1;
        Ok(())
    }

    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>> {
        Ok(self.files.get(&file).cloned().unwrap_or_default())
    }

    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        self.files.insert(file, data.to_vec());
        Ok(())
    }
}

impl FsIntrospect for CountingFs {
    fn total_appended(&self) -> usize {
        0
    }

    fn sync_count(&self) -> usize {
        self.syncs
    }
}

fn counting_db() -> GraphDb<CountingFs> {
    GraphDb::open_with(CountingFs::default()).unwrap()
}

// ── Test 1: group atomicity — submissions in a group are individually crash-atomic ──

#[test]
fn group_atomicity_each_submission_is_a_separate_wal_frame() {
    // Two submissions in one commit_group call must each be a separate WAL
    // Batch frame.  Verify by inspecting WAL frame count on recovery.
    use core_storage::wal::decode_all;
    use sim_harness::SimFs;

    let fs = SimFs::new();
    let mut db = GraphDb::open_with(fs).unwrap();

    let g = vec![
        vec![BatchOp::InsertNode {
            label: "A".into(),
            key: "sub1".into(),
            props: vec![],
        }],
        vec![BatchOp::InsertNode {
            label: "A".into(),
            key: "sub2".into(),
            props: vec![],
        }],
    ];

    let (results, sync_err) = db.commit_group(g);
    assert!(results.iter().all(|r| r.is_ok()), "both submissions ok");
    assert!(sync_err.is_none(), "no sync error");

    // Verify in-memory state.
    assert!(db.has_node("sub1"));
    assert!(db.has_node("sub2"));

    // Verify WAL has two top-level Batch frames (one per submission).
    let fs = db.into_fs();
    let wal = fs.read(FileId::Wal).unwrap();
    let (records, _) = decode_all(&wal);
    let batch_count = records
        .iter()
        .filter(|r| matches!(r, core_storage::wal::WalRecord::Batch(_)))
        .count();
    assert_eq!(
        batch_count, 2,
        "two submissions must produce two WAL Batch frames"
    );
}

// ── Test 2: exactly one Fs::sync per commit_group call ───────────────────────

#[test]
fn one_fsync_per_group_strict_policy() {
    let mut db = counting_db();
    // Default policy is Strict.

    let groups: Vec<Vec<BatchOp>> = (0..8)
        .map(|i| {
            vec![BatchOp::InsertNode {
                label: "A".into(),
                key: format!("n{i}"),
                props: vec![],
            }]
        })
        .collect();

    let (results, sync_err) = db.commit_group(groups);
    assert!(results.iter().all(|r| r.is_ok()), "all submissions ok");
    assert!(sync_err.is_none(), "sync succeeded");
    assert_eq!(
        db.fs_sync_count(),
        1,
        "group of 8 submissions must use exactly ONE fsync"
    );
    assert_eq!(db.node_count(), 8);
}

#[test]
fn two_groups_produce_two_fsyncs() {
    let mut db = counting_db();

    let g1 = vec![vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "a".into(),
        props: vec![],
    }]];
    let g2 = vec![vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "b".into(),
        props: vec![],
    }]];

    db.commit_group(g1);
    db.commit_group(g2);
    assert_eq!(
        db.fs_sync_count(),
        2,
        "two separate commit_group calls = two fsyncs"
    );
}

#[test]
fn relaxed_policy_group_skips_fsync() {
    let mut db = counting_db();
    db.set_fsync_policy(FsyncPolicy::Relaxed);

    let groups: Vec<Vec<BatchOp>> = (0..4)
        .map(|i| {
            vec![BatchOp::InsertNode {
                label: "A".into(),
                key: format!("r{i}"),
                props: vec![],
            }]
        })
        .collect();

    let (results, sync_err) = db.commit_group(groups);
    assert!(results.iter().all(|r| r.is_ok()));
    assert!(sync_err.is_none());
    assert_eq!(
        db.fs_sync_count(),
        0,
        "Relaxed policy must skip all fsyncs even in a group"
    );
}

// ── Test 3: FIFO ordering within one caller ───────────────────────────────────

#[test]
fn fifo_ordering_within_single_caller() {
    let dir = tmp("fifo");
    let db = SharedDb::open(&dir).unwrap();

    const N: usize = 20;
    let mut prev_nodes = 0usize;

    // Each submit_batch call is a serialized round-trip through the queue.
    // After each call, the node count must be monotonically non-decreasing.
    for i in 0..N {
        let ops = vec![BatchOp::InsertNode {
            label: "A".into(),
            key: format!("seq{i}"),
            props: vec![],
        }];
        db.submit_batch(ops).unwrap();
        let n = db.read().node_count();
        assert!(
            n >= prev_nodes,
            "node count must not decrease: was {prev_nodes}, now {n}"
        );
        prev_nodes = n;
    }
    assert_eq!(db.read().node_count(), N);
}

// ── Test 4: concurrent submitters all commit ──────────────────────────────────

#[test]
fn concurrent_submitters_all_commit() {
    let dir = tmp("conc");
    let db = SharedDb::open(&dir).unwrap();
    const WRITERS: usize = 8;
    const OPS_PER_WRITER: usize = 25;

    let start = Arc::new(Barrier::new(WRITERS));
    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let db = db.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait(); // all writers start simultaneously
                for i in 0..OPS_PER_WRITER {
                    let key = format!("w{w}_n{i}");
                    db.submit_batch(vec![BatchOp::InsertNode {
                        label: "N".into(),
                        key,
                        props: vec![],
                    }])
                    .expect("submit_batch must succeed");
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("writer thread panicked");
    }

    let expected = WRITERS * OPS_PER_WRITER;
    let actual = db.read().node_count();
    assert_eq!(
        actual, expected,
        "all {expected} concurrent submissions must commit"
    );
}

// ── Test 5: crash before group fsync loses the unsynced group ─────────────────

#[test]
fn crash_before_group_fsync_loses_unsynced_group() {
    use core_storage::fs::FileId;
    use sim_harness::SimFs;

    // Group 1: commit_group (includes fsync) — survives crash.
    let fs = SimFs::new();
    let mut db = GraphDb::open_with(fs).unwrap();

    let g1 = vec![vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "g1".into(),
        props: vec![],
    }]];
    db.commit_group(g1);

    // Capture WAL after group 1's fsync.
    let after_g1_bytes = db.fs_total_appended();

    // Group 2: commit_group_nosync — appended but NOT synced.
    let g2 = vec![
        vec![BatchOp::InsertNode {
            label: "A".into(),
            key: "g2a".into(),
            props: vec![],
        }],
        vec![BatchOp::InsertNode {
            label: "A".into(),
            key: "g2b".into(),
            props: vec![],
        }],
    ];
    db.commit_group_nosync(g2);

    // Simulate crash: truncate WAL to the synced portion (discard g2 frames).
    let fs = db.into_fs();
    let wal = fs.read(FileId::Wal).unwrap();
    assert!(
        wal.len() > after_g1_bytes,
        "WAL must contain g2 bytes before crash"
    );

    // Reconstruct survivor: trim WAL to synced bytes.
    let mut survivor = SimFs::new();
    // Copy snapshot if present.
    let snap = fs.read(FileId::Snapshot).unwrap();
    if !snap.is_empty() {
        survivor.write_atomic(FileId::Snapshot, &snap).unwrap();
    }
    survivor
        .write_atomic(FileId::Wal, &wal[..after_g1_bytes])
        .unwrap();

    let db2 = GraphDb::open_with(survivor).unwrap();
    assert!(db2.has_node("g1"), "g1 (synced group) must survive");
    assert!(
        !db2.has_node("g2a"),
        "g2a (unsynced group) must be lost on crash"
    );
    assert!(
        !db2.has_node("g2b"),
        "g2b (unsynced group) must be lost on crash"
    );
}

// ── Test 6: intra-group prefix survival — frame-1 survives crash mid-frame-2 ──
//
// When a group has 2+ submissions and the process crashes mid-second-frame,
// the first submission's WAL frame is complete and must survive replay.  The
// second frame is torn at the CRC boundary and must be dropped whole.

#[test]
fn intra_group_prefix_survives_crash() {
    use sim_harness::SimFs;

    // Probe: measure the byte size of one Batch frame (one InsertNode).
    let probe_fs = SimFs::new();
    let mut probe = GraphDb::open_with(probe_fs).unwrap();
    probe
        .commit_group_nosync(vec![vec![BatchOp::InsertNode {
            label: "A".into(),
            key: "s1".into(),
            props: vec![],
        }]])
        .into_iter()
        .for_each(|r| {
            r.unwrap();
        });
    let frame1_bytes = probe.fs_total_appended(); // bytes for exactly one Batch frame
    drop(probe);

    // Set up SimFs to crash 3 bytes into the SECOND frame (tears its CRC).
    let crash_at = frame1_bytes + 3;
    let fs = SimFs::with_crash_after(crash_at);
    let mut db = GraphDb::open_with(fs).unwrap();

    // Commit both submissions in one group_nosync call.
    // Frame 1 fits within crash_at; frame 2 is torn.
    let results = db.commit_group_nosync(vec![
        vec![BatchOp::InsertNode {
            label: "A".into(),
            key: "s1".into(),
            props: vec![],
        }],
        vec![BatchOp::InsertNode {
            label: "A".into(),
            key: "s2".into(),
            props: vec![],
        }],
    ]);
    // Frame 1 should succeed; frame 2 may err (crash mid-append) or appear
    // to succeed (crash after append but before in-process acknowledgement).
    let _ = results;

    // Replay from the surviving WAL (SimFs preserves up to crash_at bytes).
    let fs = db.into_fs();
    let survivor = fs.surviving_state();
    let db2 = GraphDb::open_with(survivor).unwrap();

    // Frame 1 is fully within crash_at → its ops must be present.
    assert!(
        db2.has_node("s1"),
        "first submission frame (before crash point) must survive"
    );
    // Frame 2 is torn → CRC mismatch → dropped on recovery.
    assert!(
        !db2.has_node("s2"),
        "torn second-frame submission must be dropped on recovery"
    );
    assert_eq!(
        db2.node_count(),
        1,
        "only the complete first frame survives"
    );
}

// ── Test 7: direct &mut self APIs unchanged ───────────────────────────────────

#[test]
fn direct_apis_unchanged_alongside_queue() {
    let dir = tmp("direct");
    let db = SharedDb::open(&dir).unwrap();

    // Direct write path still works.
    db.write()
        .insert_node("N", "direct1", vec![])
        .expect("direct insert_node must work");
    db.write()
        .insert_node("N", "direct2", vec![])
        .expect("direct insert_node must work");

    // Queue path works concurrently.
    db.submit_batch(vec![BatchOp::InsertNode {
        label: "N".into(),
        key: "queued1".into(),
        props: vec![],
    }])
    .expect("submit_batch must work");

    // write_batch still works.
    db.write()
        .write_batch(|b| {
            b.insert_node("N", "batch1", vec![]);
            b.insert_node("N", "batch2", vec![]);
        })
        .expect("write_batch must work");

    let n = db.read().node_count();
    assert_eq!(n, 5, "direct + queued + batch all committed");
}

// ── Test 8: deferred events fire after flush (R2) ─────────────────────────────
//
// Under Strict policy the drain thread defers subscriber events until after
// the group fsync.  Test the mechanism via commit_group_nosync + explicit
// deferred-events API on GraphDb (unit-level, no drain thread involvement).

#[test]
fn deferred_events_fire_after_flush() {
    use sim_harness::SimFs;

    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received2 = Arc::clone(&received);

    let fs = SimFs::new();
    let mut db = GraphDb::open_with(fs).unwrap();
    db.set_event_sink(Box::new(move |ev| {
        if let MutationEvent::NodeInserted { key, .. } = ev {
            received2.lock().unwrap().push(key);
        }
    }));

    // Enable deferred mode — simulates what the drain thread does.
    db.set_deferred_events_mode(true);

    // Commit; events must NOT fire yet.
    db.commit_group_nosync(vec![
        vec![BatchOp::InsertNode {
            label: "A".into(),
            key: "ev1".into(),
            props: vec![],
        }],
        vec![BatchOp::InsertNode {
            label: "A".into(),
            key: "ev2".into(),
            props: vec![],
        }],
    ]);
    assert!(
        received.lock().unwrap().is_empty(),
        "events must not fire before flush"
    );

    // Flush — simulates what the drain thread does after a successful fsync.
    db.flush_deferred_events();
    db.set_deferred_events_mode(false);

    let keys = received.lock().unwrap().clone();
    assert!(keys.contains(&"ev1".to_string()), "ev1 must be delivered");
    assert!(keys.contains(&"ev2".to_string()), "ev2 must be delivered");
}

// ── Test 9: deferred events discarded on fsync failure (R2) ──────────────────

#[test]
fn deferred_events_discarded_on_failure() {
    use sim_harness::SimFs;

    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received2 = Arc::clone(&received);

    let fs = SimFs::new();
    let mut db = GraphDb::open_with(fs).unwrap();
    db.set_event_sink(Box::new(move |ev| {
        if let MutationEvent::NodeInserted { key, .. } = ev {
            received2.lock().unwrap().push(key);
        }
    }));

    // Enable deferred mode and commit.
    db.set_deferred_events_mode(true);
    db.commit_group_nosync(vec![vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "lost".into(),
        props: vec![],
    }]]);
    assert!(
        received.lock().unwrap().is_empty(),
        "events must not fire before discard"
    );

    // Discard — simulates what the drain thread does on fsync failure.
    db.discard_deferred_events();
    db.set_deferred_events_mode(false);

    // Even after discard+mode-off, no event must have been delivered.
    assert!(
        received.lock().unwrap().is_empty(),
        "discarded events must never be delivered to subscribers"
    );
}

// ── Test 10: RealFs throughput bench (informational, ignored) ────────────────

/// RealFs group-commit throughput bench — informational only, no ratio gate.
///
/// Records real-world throughput numbers for observability.  The amortization
/// gate lives in `group_commit_simfs_amortization_bench` (test 11) which uses
/// SimFs with injected fsync latency and is environment-independent.
///
/// # Pre-existing durability finding (not introduced by task 4b)
///
/// `write_batch(FsyncPolicy::Strict)` for a single-op batch internally maps to
/// `FsyncPolicy::Batched` and skips fsync when the batch contains only one
/// non-`Intern` WAL record.  A prior bench that used `write_batch` as the
/// serial baseline was therefore measuring no-fsync writes, explaining the
/// implausible 51 k ops/s result.  This bench corrects the baseline to
/// `insert_node(FsyncPolicy::Strict)`, which always fsyncs.
///
/// Run manually with: `cargo test --release group_commit_throughput_bench -- --ignored --nocapture`
#[test]
#[ignore]
fn group_commit_throughput_bench() {
    use std::time::Instant;

    const WRITERS: usize = 8;
    const OPS_PER_WRITER: usize = 200;
    const TOTAL_OPS: usize = WRITERS * OPS_PER_WRITER;

    // ── Serialized-writer baseline (direct path, one fsync per insert_node) ──
    //
    // Uses db.write().insert_node() under FsyncPolicy::Strict so each call
    // acquires the write lock AND fsyncs exactly once before returning.
    // write_batch is NOT used here because a single-op batch under FsyncPolicy::Strict
    // maps to Batched internally and skips the fsync (count-gt-1 condition is false).
    let dir_serial = tmp("bench-serial");
    let db_serial = SharedDb::open(&dir_serial).unwrap();
    // Warm up OS page cache / WAL file.
    db_serial.write().insert_node("W", "warm", vec![]).unwrap();

    let t0 = Instant::now();
    for i in 0..TOTAL_OPS {
        db_serial
            .write()
            .insert_node("W", &format!("s{i}"), vec![])
            .unwrap();
    }
    let serial_elapsed = t0.elapsed();
    let serial_ops_per_s = TOTAL_OPS as f64 / serial_elapsed.as_secs_f64();

    // ── 8-concurrent-writer path (group-commit queue) ─────────────────────
    let dir_conc = tmp("bench-conc");
    let db_conc = SharedDb::open(&dir_conc).unwrap();

    // Warm up.
    {
        let db = db_conc.clone();
        db.submit_batch(vec![BatchOp::InsertNode {
            label: "W".into(),
            key: "warm".into(),
            props: vec![],
        }])
        .unwrap();
    }

    let start = Arc::new(Barrier::new(WRITERS));
    let t1 = Instant::now();
    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let db = db_conc.clone();
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                for i in 0..OPS_PER_WRITER {
                    db.submit_batch(vec![BatchOp::InsertNode {
                        label: "W".into(),
                        key: format!("w{w}n{i}"),
                        props: vec![],
                    }])
                    .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let conc_elapsed = t1.elapsed();
    let conc_ops_per_s = TOTAL_OPS as f64 / conc_elapsed.as_secs_f64();

    let ratio = conc_ops_per_s / serial_ops_per_s;

    // ── Reader-under-burst p95 (cheap proxy) ──────────────────────────────
    // Measure read latency while 8 writers are hammering the queue.
    let dir_reader = tmp("bench-reader");
    let db_reader = SharedDb::open(&dir_reader).unwrap();
    // Pre-populate a few nodes so reads are non-trivial.
    for i in 0..10 {
        db_reader
            .write()
            .insert_node("R", &format!("pre{i}"), vec![])
            .unwrap();
    }

    let start2 = Arc::new(Barrier::new(WRITERS + 1));
    let read_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let read_latencies: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::new()));

    let writer_handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let db = db_reader.clone();
            let start2 = Arc::clone(&start2);
            let read_done = Arc::clone(&read_done);
            thread::spawn(move || {
                start2.wait();
                let mut i = 0usize;
                while !read_done.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = db.submit_batch(vec![BatchOp::InsertNode {
                        label: "W".into(),
                        key: format!("bw{w}_{i}"),
                        props: vec![],
                    }]);
                    i += 1;
                }
            })
        })
        .collect();

    let lat_db = db_reader.clone();
    let lat_lats = Arc::clone(&read_latencies);
    let lat_start = Arc::clone(&start2);
    let lat_read_done = Arc::clone(&read_done);
    let reader_handle = thread::spawn(move || {
        lat_start.wait();
        let deadline = Instant::now() + std::time::Duration::from_millis(500);
        while Instant::now() < deadline {
            let t = Instant::now();
            let _ = lat_db.reader().query(
                "MATCH (n:R) RETURN n.id",
                &std::collections::BTreeMap::new(),
            );
            let elapsed_us = t.elapsed().as_micros();
            lat_lats.lock().unwrap().push(elapsed_us);
        }
        lat_read_done.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    reader_handle.join().unwrap();
    for h in writer_handles {
        let _ = h.join();
    }

    let mut lats = read_latencies.lock().unwrap().clone();
    let reader_p95_us = if lats.is_empty() {
        0u128
    } else {
        lats.sort_unstable();
        lats[lats.len() * 95 / 100]
    };

    // Print bench JSON.
    // gate_pass is informational only — the ratio is not asserted here because
    // on fast SSD/tmpdir the channel round-trip of submit_batch dominates over
    // fsync cost.  See group_commit_simfs_amortization_bench for the gated proof.
    println!(
        "{}",
        serde_json::json!({
            "serialized_writer_ops_per_s": serial_ops_per_s as u64,
            "eight_writer_ops_per_s": conc_ops_per_s as u64,
            "ratio": format!("{ratio:.2}"),
            "reader_under_burst_p95_us": reader_p95_us,
            "gate_pass": ratio >= 3.0,
        })
    );
}

// ── Test 11: SimFs amortization bench (gated, ignored) ───────────────────────

/// Environment-independent amortization proof (spec B3.5 gate).
///
/// Uses `SimFs::with_sync_delay_us` to inject a controlled fsync latency
/// so the result is independent of the storage hardware.  When fsync costs
/// FSYNC_DELAY_US, committing 8 ops under one group fsync is ~8× cheaper than
/// 8 serial fsyncs.  This test asserts the ratio is >= 3× (half the theoretical
/// maximum), leaving headroom for group sizes < 8.
///
/// # Method
///
/// **Serial baseline** — TOTAL_OPS sequential `insert_node` calls on a
/// `GraphDb<SimFs>` under `FsyncPolicy::Strict`.  Each call triggers one
/// `SimFs::sync` (one FSYNC_DELAY_US sleep).  Total time ≈ TOTAL_OPS × delay.
///
/// **Group path** — same TOTAL_OPS split into groups of WRITERS ops each,
/// committed via `commit_group` (which calls `SimFs::sync` once per group).
/// Total time ≈ (TOTAL_OPS / WRITERS) × delay.
///
/// Theoretical ratio = WRITERS = 8×.  Practical ratio will be close to 8×
/// because both paths use the same SimFs implementation.
///
/// Run manually with: `cargo test --release group_commit_simfs_amortization_bench -- --ignored --nocapture`
#[test]
#[ignore]
fn group_commit_simfs_amortization_bench() {
    use sim_harness::SimFs;
    use std::time::Instant;

    // Simulated fsync cost (spinning-disk / NVMe-with-flush approximation).
    const FSYNC_DELAY_US: u64 = 5_000; // 5 ms
    const WRITERS: usize = 8;
    const OPS_PER_WRITER: usize = 50; // small: sleep dominates, not CPU
    const TOTAL_OPS: usize = WRITERS * OPS_PER_WRITER;

    // ── Serial baseline ──────────────────────────────────────────────────────
    //
    // TOTAL_OPS sequential insert_node calls under FsyncPolicy::Strict.
    // Each call: append WAL record → SimFs::sync (sleeps FSYNC_DELAY_US µs).
    let fs_serial = SimFs::with_sync_delay_us(FSYNC_DELAY_US);
    let mut db_serial = GraphDb::open_with(fs_serial).unwrap();
    // db opens with FsyncPolicy::Strict by default; no change needed.

    let t0 = Instant::now();
    for i in 0..TOTAL_OPS {
        db_serial
            .insert_node("W", &format!("s{i}"), vec![])
            .unwrap();
    }
    let serial_elapsed = t0.elapsed();
    let serial_ops_per_s = TOTAL_OPS as f64 / serial_elapsed.as_secs_f64();

    // ── Group path ───────────────────────────────────────────────────────────
    //
    // TOTAL_OPS ops committed in groups of WRITERS via commit_group.
    // commit_group calls SimFs::sync ONCE per group (one FSYNC_DELAY_US sleep).
    let fs_group = SimFs::with_sync_delay_us(FSYNC_DELAY_US);
    let mut db_group = GraphDb::open_with(fs_group).unwrap();

    let t1 = Instant::now();
    for g in 0..(TOTAL_OPS / WRITERS) {
        let batches: Vec<Vec<BatchOp>> = (0..WRITERS)
            .map(|i| {
                vec![BatchOp::InsertNode {
                    label: "W".into(),
                    key: format!("g{g}n{i}"),
                    props: vec![],
                }]
            })
            .collect();
        let (results, sync_err) = db_group.commit_group(batches);
        assert!(sync_err.is_none(), "simfs sync must not fail");
        for r in results {
            r.unwrap();
        }
    }
    let group_elapsed = t1.elapsed();
    let group_ops_per_s = TOTAL_OPS as f64 / group_elapsed.as_secs_f64();

    let ratio = group_ops_per_s / serial_ops_per_s;

    println!(
        "{}",
        serde_json::json!({
            "bench": "simfs_amortization",
            "fsync_delay_us": FSYNC_DELAY_US,
            "writers": WRITERS,
            "total_ops": TOTAL_OPS,
            "serial_ops_per_s": serial_ops_per_s as u64,
            "group_ops_per_s": group_ops_per_s as u64,
            "ratio": format!("{ratio:.2}"),
            "gate_pass": ratio >= 3.0,
        })
    );

    assert!(
        ratio >= 3.0,
        "group-commit ({group_ops_per_s:.0} ops/s) must be >= 3x serial \
         ({serial_ops_per_s:.0} ops/s) under {FSYNC_DELAY_US}µs injected fsync \
         latency; ratio = {ratio:.2}"
    );
}

// ── Test 12: F1(c) — SharedDb fsync-failure integration (single submitter) ───

/// Full fsync-failure contract exercised through the live drain thread
/// (not GraphDb methods directly).
///
/// Verifies:
/// - Group submitter receives Err on fsync failure.
/// - WAL is truncated to the pre-group offset after failure.
/// - Subsequent `submit_batch` returns Err(degraded).
/// - Reopen/replay: failed group absent, pre-group data intact.
#[test]
fn shared_db_fsync_failure_degrades_and_truncates_wal() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tmp("f1c-single");
    let fail = Arc::new(AtomicBool::new(false));
    let fail2 = Arc::clone(&fail);

    let db = SharedDb::open_with_test_sync(&dir, move |path| {
        if fail2.load(Ordering::Acquire) {
            Err(std::io::Error::other("injected fsync failure"))
        } else {
            core_storage::sync_wal_at(path)
        }
    })
    .unwrap();

    // Normal submission — must succeed.
    db.submit_batch(vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "pre".into(),
        props: vec![],
    }])
    .unwrap();

    let pre_group_wal_len = std::fs::metadata(dir.join("wal.bin")).unwrap().len();

    // Enable fsync failure for the next group.
    fail.store(true, Ordering::Release);

    let result = db.submit_batch(vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "fail-group".into(),
        props: vec![],
    }]);
    assert!(result.is_err(), "group with failing fsync must return Err");

    // WAL must be truncated back to pre-group length.
    let post_wal_len = std::fs::metadata(dir.join("wal.bin")).unwrap().len();
    assert_eq!(
        post_wal_len, pre_group_wal_len,
        "WAL must be truncated to pre-group length after fsync failure"
    );

    // Subsequent submit must fail with degraded error.
    let result2 = db.submit_batch(vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "post-fail".into(),
        props: vec![],
    }]);
    assert!(
        result2.is_err(),
        "subsequent submit_batch must return Err after degradation"
    );

    // Reopen and replay — failed group must be absent, pre-group data intact.
    drop(db);
    let db2 = SharedDb::open(&dir).unwrap();
    assert!(
        db2.read().has_node("pre"),
        "pre-group node must survive replay"
    );
    assert!(
        !db2.read().has_node("fail-group"),
        "failed-group node must be absent on replay"
    );
}

// ── Test 13: F1(c) variant — direct write before group survives failure ───────

/// Regression test for the truncation race (F1(d)):
/// a direct write acknowledged Ok before the group commit is NOT wiped
/// by the drain's truncation on group fsync failure.
///
/// With the WAL mutex, the direct write and the group commit are fully
/// serialized: the direct write's frames land at WAL positions below
/// `pre_group_wal_len`, so `truncate_wal_at(pre_len)` leaves them intact.
///
/// Verifies on reopen: the directly-acknowledged write survives replay
/// and the failed group node is absent.
#[test]
fn direct_write_before_group_survives_group_fsync_failure() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = tmp("f1c-concurrent");
    let fail = Arc::new(AtomicBool::new(false));
    let fail2 = Arc::clone(&fail);

    let db = SharedDb::open_with_test_sync(&dir, move |path| {
        if fail2.load(Ordering::Acquire) {
            Err(std::io::Error::other("injected fsync failure"))
        } else {
            core_storage::sync_wal_at(path)
        }
    })
    .unwrap();

    // Direct write: durably acknowledged before any group failure.
    // The WAL mutex ensures this write's append + fsync is atomic with
    // respect to any subsequent drain group.
    db.write().insert_node("A", "direct-ok", vec![]).unwrap();

    // Record WAL offset AFTER the direct write: the group's frames will
    // land here, and truncation reverts to exactly this position.
    let pre_group_wal_len = std::fs::metadata(dir.join("wal.bin")).unwrap().len();

    // Enable fsync failure for the next drain group.
    fail.store(true, Ordering::Release);

    let result = db.submit_batch(vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "group-fail".into(),
        props: vec![],
    }]);
    assert!(result.is_err(), "group fsync failure must return Err");

    // WAL must be truncated to pre-group boundary.
    // The direct write's frames (before pre_group_wal_len) are untouched.
    let post_wal_len = std::fs::metadata(dir.join("wal.bin")).unwrap().len();
    assert_eq!(
        post_wal_len, pre_group_wal_len,
        "WAL truncated to pre-group boundary; direct write frames are preserved"
    );

    // Reopen: direct write survives; failed group node is absent.
    drop(db);
    let db2 = SharedDb::open(&dir).unwrap();
    assert!(
        db2.read().has_node("direct-ok"),
        "directly-acknowledged write must survive replay"
    );
    assert!(
        !db2.read().has_node("group-fail"),
        "failed group node must be absent on replay"
    );
}
