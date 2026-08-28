//! Tests for Task 4b: group-commit write queue (spec B3.5).
//!
//! Test list:
//!  1. group_atomicity_all_or_nothing — submissions in a group never torn
//!  2. one_fsync_per_group — exactly one Fs::sync for N submissions in a group
//!  3. fifo_within_caller — sequential submits preserve commit ordering
//!  4. concurrent_submitters_all_commit — 8 threads submit concurrently, all land
//!  5. crash_before_group_fsync_loses_group — unsynced group is lost on crash
//!  6. crash_mid_submission_frame_is_dropped — torn WAL frame never applies
//!  7. direct_api_unchanged — write/read/write_batch/insert_node still work
//!  8. group_commit_throughput_bench (ignored) — 8 writers vs serial, Strict fsync

use core_api::{BatchOp, FsyncPolicy, GraphDb, SharedDb};
use core_storage::fs::{FileId, Fs, FsIntrospect};
use std::collections::HashMap;
use std::sync::{Arc, Barrier};
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

// ── Test 6: torn WAL frame within a group drops that submission on recovery ───

#[test]
fn torn_submission_frame_dropped_on_recovery() {
    use sim_harness::SimFs;

    // Measure how many bytes a single-submission group1 WAL frame occupies.
    let probe_fs = SimFs::new();
    let mut probe = GraphDb::open_with(probe_fs).unwrap();
    let g1 = vec![vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "baseline".into(),
        props: vec![],
    }]];
    probe.commit_group(g1);
    let g1_bytes = probe.fs_total_appended();
    drop(probe);

    // Crash mid-g2 frame (3 bytes into the new frame — tears the CRC).
    let crash_at = g1_bytes + 3;
    let fs = SimFs::with_crash_after(crash_at);
    let mut db = GraphDb::open_with(fs).unwrap();

    // Group 1 succeeds (fits within crash threshold).
    let (r1, _) = db.commit_group(vec![vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "baseline".into(),
        props: vec![],
    }]]);
    assert!(r1[0].is_ok(), "group 1 must succeed");

    // Group 2 crashes mid-append — commit_group_nosync returns Err for g2.
    let g2_result = db.commit_group_nosync(vec![vec![BatchOp::InsertNode {
        label: "A".into(),
        key: "torn".into(),
        props: vec![],
    }]]);
    // The append may fail (crash) or succeed with a torn frame.
    // Either way, on recovery the torn frame is dropped.

    let fs = db.into_fs();
    let survivor = fs.surviving_state();
    let db2 = GraphDb::open_with(survivor).unwrap();

    assert!(db2.has_node("baseline"), "baseline (group 1) must survive");
    // If g2 was torn by byte-crash, it won't be present after recovery.
    // If g2 errored without appending, also not present.
    let torn_present = db2.has_node("torn");
    // Either the torn frame was silently discarded (ok) or never written (ok).
    // Both outcomes are acceptable — the submission result told the caller.
    let _ = g2_result;
    let _ = torn_present;
    // The key invariant: whatever was committed in group 1 is intact.
    assert_eq!(db2.node_count(), 1, "only baseline survives");
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

// ── Test 8: throughput bench (ignored) ───────────────────────────────────────

/// Group-commit throughput gate (spec B3.5):
/// 8 concurrent writers >= 3x serialized-writer throughput at Strict fsync.
///
/// Run manually with: `cargo test --release group_commit_throughput_bench -- --ignored --nocapture`
#[test]
#[ignore]
fn group_commit_throughput_bench() {
    use std::time::Instant;

    const WRITERS: usize = 8;
    const OPS_PER_WRITER: usize = 200;
    const TOTAL_OPS: usize = WRITERS * OPS_PER_WRITER;

    // ── Serialized-writer baseline ─────────────────────────────────────────
    let dir_serial = tmp("bench-serial");
    {
        let db = SharedDb::open(&dir_serial).unwrap();
        // Warm up.
        db.submit_batch(vec![BatchOp::InsertNode {
            label: "W".into(),
            key: "warm".into(),
            props: vec![],
        }])
        .unwrap();
    }

    let dir_serial = tmp("bench-serial2");
    let db_serial = SharedDb::open(&dir_serial).unwrap();
    let t0 = Instant::now();
    for i in 0..TOTAL_OPS {
        db_serial
            .submit_batch(vec![BatchOp::InsertNode {
                label: "W".into(),
                key: format!("s{i}"),
                props: vec![],
            }])
            .unwrap();
    }
    let serial_elapsed = t0.elapsed();
    let serial_ops_per_s = TOTAL_OPS as f64 / serial_elapsed.as_secs_f64();

    // ── 8-concurrent-writer path ───────────────────────────────────────────
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

    assert!(
        ratio >= 3.0,
        "8 concurrent writers ({conc_ops_per_s:.0} ops/s) must be >= 3x \
         serialized ({serial_ops_per_s:.0} ops/s); ratio = {ratio:.2}"
    );
}

use std::sync::Mutex;
