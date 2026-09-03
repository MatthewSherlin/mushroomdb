#![cfg(feature = "mp-test")]
//! Multi-process safety: advisory cross-process write lock + WAL tailing.
//!
//! Every cross-process test spawns the real `mp_worker` binary (built by the
//! same `mp-test` feature that gates this file), so the behaviour under test is
//! genuine inter-process contention on the store's `LOCK` file, not an
//! in-process simulation.
//!
//! Test list:
//!  1. server_handle_sees_child_process_writes_after_refresh
//!  2. two_writers_serialise_without_corruption
//!  3. writer_gets_busy_when_lock_held
//!  4. read_only_open_never_blocks_and_never_locks
//!  5. refresh_survives_snapshot_by_other_process
//!  6. partial_trailing_frame_is_not_an_error
//!  7. derived_edges_fire_on_refreshed_frames
//!  8. wal_consumed_tracks_bytes_exactly
//!  9. refresh_on_unchanged_store_is_zero_cost

use core_api::{GraphDb, OpenOptions, Predicate, RuleDef, SharedDb, Value};
use core_storage::fs::{FileId, Fs, RealFs};
use core_storage::wal::{decode_all, encode_record, WalRecord};
use core_storage::GraphError;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-mp-{}-{}-{}",
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

fn worker(dir: &Path, args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_mp_worker"))
        .arg(dir)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn mp_worker")
}

/// Run a worker to completion and return `(exit_code, stdout)`.
fn run_worker(dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mp_worker"))
        .arg(dir)
        .args(args)
        .output()
        .expect("run mp_worker");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    )
}

fn same_team_rule() -> RuleDef {
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
    }
}

// ── 1. A long-lived server handle picks up a child process's writes ───────────

#[test]
fn server_handle_sees_child_process_writes_after_refresh() {
    let d = tmp("sees-child");
    let db = SharedDb::open(&d).unwrap();
    assert_eq!(db.read().node_count(), 0);

    let (code, _) = run_worker(&d, &["write", "child", "100"]);
    assert_eq!(code, 0, "child writer must succeed");

    // No reopen: the same handle refreshes on read and sees all 100 commits.
    assert_eq!(db.read().node_count(), 100);
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 2. Two concurrent writer processes serialise without corrupting the WAL ───

#[test]
fn two_writers_serialise_without_corruption() {
    let d = tmp("two-writers");
    std::fs::create_dir_all(&d).unwrap();

    let mut a = worker(&d, &["write", "a", "500"]);
    let mut b = worker(&d, &["write", "b", "500"]);
    assert_eq!(a.wait().unwrap().code(), Some(0), "writer a");
    assert_eq!(b.wait().unwrap().code(), Some(0), "writer b");

    // Every frame both processes wrote decodes: zero partial frames.
    let bytes = std::fs::read(d.join("wal.bin")).unwrap();
    let (frames, valid_len) = decode_all(&bytes);
    assert_eq!(
        valid_len,
        bytes.len(),
        "WAL tail must decode completely: {} of {} bytes valid",
        valid_len,
        bytes.len()
    );
    assert!(!frames.is_empty());

    let mut db = GraphDb::open(&d).unwrap();
    assert_eq!(db.node_count(), 1000, "both writers' nodes are present");
    for i in [0usize, 250, 499] {
        assert!(db.node_info(&format!("a-{i}")).is_some(), "a-{i} present");
        assert!(db.node_info(&format!("b-{i}")).is_some(), "b-{i} present");
    }

    // `verify` passes on the resulting store.
    db.snapshot().unwrap();
    for (_, name, _, res) in core_api::verify_snapshot(&d).unwrap() {
        assert!(res.is_ok(), "section {name} failed verification: {res:?}");
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 3. A writer that cannot get the lock in time is told the store is Busy ────

#[test]
fn writer_gets_busy_when_lock_held() {
    let d = tmp("busy");
    let db = SharedDb::open(&d).unwrap();

    // Hold the cross-process write lock for the whole child lifetime.
    let guard = db.write();
    let (code, _) = run_worker(&d, &["busy", "500"]);
    assert_eq!(code, 3, "child must exit with the Busy code");
    drop(guard);

    // The lock is released with the guard: the next writer proceeds.
    let (code, _) = run_worker(&d, &["busy", "2000"]);
    assert_eq!(code, 0, "child writes once the lock is free");
    assert_eq!(db.read().node_count(), 1);
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 4. A read-only open never waits for, and never takes, the lock ────────────

#[test]
fn read_only_open_never_blocks_and_never_locks() {
    let d = tmp("read-only");
    let db = SharedDb::open(&d).unwrap();
    db.write().insert_node("Person", "alice", vec![]).unwrap();

    let guard = db.write(); // lock held for the duration of the child
    let start = std::time::Instant::now();
    let (code, out) = run_worker(&d, &["ro-read"]);
    let elapsed = start.elapsed();
    drop(guard);

    assert_eq!(
        code, 0,
        "read-only open must succeed while the lock is held"
    );
    assert_eq!(out, "1");
    assert!(
        elapsed < Duration::from_secs(2),
        "read-only open waited {elapsed:?} — it must not poll the write lock"
    );

    // A read-only handle refuses mutations and writes nothing to disk.
    let opts = OpenOptions {
        read_only: true,
        ..OpenOptions::default()
    };
    let mut ro = GraphDb::open_with_options(&d, opts).unwrap();
    assert!(matches!(
        ro.insert_node("Person", "bob", vec![]),
        Err(GraphError::ReadOnly)
    ));
    drop(ro);
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 5. A snapshot taken by another process is absorbed by refresh ─────────────

#[test]
fn refresh_survives_snapshot_by_other_process() {
    let d = tmp("peer-snapshot");
    let db = SharedDb::open(&d).unwrap();
    {
        let mut g = db.write();
        for i in 0..10 {
            g.insert_node("Person", &format!("p-{i}"), vec![]).unwrap();
        }
    }
    assert_eq!(db.read().node_count(), 10);

    let (code, _) = run_worker(&d, &["snapshot"]);
    assert_eq!(code, 0, "peer snapshot must succeed");

    let mut g = db.write();
    g.refresh().expect("refresh across a peer's snapshot");
    assert_eq!(g.node_count(), 10, "state matches after the peer snapshot");
    assert!(g.node_info("p-7").is_some());
    drop(g);
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 6. A trailing partial frame is a wait, not a corruption ───────────────────

#[test]
fn partial_trailing_frame_is_not_an_error() {
    let d = tmp("partial-frame");
    let mut db = GraphDb::open(&d).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();
    assert!(!db.is_stale().unwrap());

    // Simulate another process mid-append: write the first half of a frame.
    let rec = WalRecord::InsertNode {
        label: "Person".into(),
        key: "bob".into(),
        props: vec![],
    };
    let frame = encode_record(&rec);
    let split = frame.len() / 2;
    let mut fs = RealFs::new(&d).unwrap();
    fs.append(FileId::Wal, &frame[..split]).unwrap();

    assert!(db.is_stale().unwrap(), "a partial tail reads as stale");
    assert_eq!(db.refresh().unwrap(), 0, "no complete frame to apply");
    assert!(
        db.is_stale().unwrap(),
        "still stale until the frame completes"
    );
    assert!(db.node_info("bob").is_none());

    // The writer finishes its frame.
    fs.append(FileId::Wal, &frame[split..]).unwrap();
    assert_eq!(db.refresh().unwrap(), 1, "the completed frame applies");
    assert!(!db.is_stale().unwrap());
    assert!(db.node_info("bob").is_some());
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 7. Rules fire on refreshed frames exactly as during open replay ───────────

#[test]
fn derived_edges_fire_on_refreshed_frames() {
    let d = tmp("derived-refresh");
    let db = SharedDb::open(&d).unwrap();
    db.write().create_rule(same_team_rule()).unwrap();

    // The child inserts Person rows that all share `team = "x"`.
    let (code, _) = run_worker(&d, &["write", "x", "4"]);
    assert_eq!(code, 0);

    let rows = db
        .read()
        .query(
            "MATCH (a:Person)-[:SAME_TEAM]->(b:Person) RETURN a, b",
            &Default::default(),
        )
        .unwrap();
    assert!(
        rows.len() >= 6,
        "4 same-team nodes derive at least 6 ordered pairs, got {}",
        rows.len()
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 8. The frame cursor advances by exactly the bytes appended ────────────────

#[test]
fn wal_consumed_tracks_bytes_exactly() {
    let d = tmp("cursor-bytes");
    let mut db = GraphDb::open(&d).unwrap();
    assert_eq!(db.wal_consumed(), 0);

    for i in 0..5 {
        db.insert_node(
            "Person",
            &format!("p-{i}"),
            vec![("n".into(), Value::Int(i))],
        )
        .unwrap();
        let on_disk = std::fs::metadata(d.join("wal.bin")).unwrap().len();
        assert_eq!(
            db.wal_consumed(),
            on_disk,
            "cursor must equal the WAL length after write {i}"
        );
        assert!(!db.is_stale().unwrap());
    }

    // An externally appended frame is decoded from exactly the cursor offset:
    // a cursor off by even one byte would fail to decode this frame.
    let frame = encode_record(&WalRecord::InsertNode {
        label: "Person".into(),
        key: "external".into(),
        props: vec![],
    });
    let before = db.wal_consumed();
    RealFs::new(&d)
        .unwrap()
        .append(FileId::Wal, &frame)
        .unwrap();
    assert_eq!(db.refresh().unwrap(), 1);
    assert_eq!(db.wal_consumed(), before + frame.len() as u64);
    assert!(db.node_info("external").is_some());
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 9. Refreshing an unchanged store reads no WAL bytes ───────────────────────

/// `RealFs` wrapper that counts every call that reads file *contents*.
/// Metadata-only calls (`wal_len`, `snapshot_ident`) are deliberately not
/// counted — they are what a staleness check is allowed to do.
struct CountingRealFs {
    inner: RealFs,
    content_reads: Arc<AtomicUsize>,
}

impl Fs for CountingRealFs {
    fn append(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        self.inner.append(file, data)
    }
    fn sync(&mut self, file: FileId) -> std::io::Result<()> {
        self.inner.sync(file)
    }
    fn read(&self, file: FileId) -> std::io::Result<Vec<u8>> {
        self.content_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read(file)
    }
    fn write_atomic(&mut self, file: FileId, data: &[u8]) -> std::io::Result<()> {
        self.inner.write_atomic(file, data)
    }
    fn snapshot_path(&self) -> Option<PathBuf> {
        self.inner.snapshot_path()
    }
    fn wal_path(&self) -> Option<PathBuf> {
        self.inner.wal_path()
    }
    fn read_prefix(&self, file: FileId, n: usize) -> std::io::Result<Vec<u8>> {
        self.content_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_prefix(file, n)
    }
    fn read_range(&self, file: FileId, from: u64) -> std::io::Result<Vec<u8>> {
        self.content_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_range(file, from)
    }
    fn wal_len(&self) -> std::io::Result<u64> {
        self.inner.wal_len()
    }
    fn snapshot_ident(&self) -> std::io::Result<Option<(u64, u64)>> {
        self.inner.snapshot_ident()
    }
    fn try_lock_exclusive(&mut self) -> std::io::Result<bool> {
        self.inner.try_lock_exclusive()
    }
    fn unlock(&mut self) -> std::io::Result<()> {
        self.inner.unlock()
    }
    fn list_archives(&self) -> std::io::Result<Vec<u64>> {
        self.inner.list_archives()
    }
    fn read_archive(&self, n: u64) -> std::io::Result<Vec<u8>> {
        self.content_reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_archive(n)
    }
    fn archive_wal(&mut self, n: u64) -> std::io::Result<()> {
        self.inner.archive_wal(n)
    }
    fn delete_archive(&mut self, n: u64) -> std::io::Result<()> {
        self.inner.delete_archive(n)
    }
    fn read_horizon_floor(&self) -> std::io::Result<u64> {
        self.inner.read_horizon_floor()
    }
    fn write_horizon_floor(&mut self, floor: u64) -> std::io::Result<()> {
        self.inner.write_horizon_floor(floor)
    }
    fn has_genesis_marker(&self) -> bool {
        self.inner.has_genesis_marker()
    }
    fn write_genesis_marker(&mut self) -> std::io::Result<()> {
        self.inner.write_genesis_marker()
    }
    fn delete_genesis_marker(&mut self) -> std::io::Result<()> {
        self.inner.delete_genesis_marker()
    }
}

#[test]
fn refresh_on_unchanged_store_is_zero_cost() {
    let d = tmp("zero-cost");
    let counter = Arc::new(AtomicUsize::new(0));
    let fs = CountingRealFs {
        inner: RealFs::new(&d).unwrap(),
        content_reads: Arc::clone(&counter),
    };
    let mut db = GraphDb::open_with(fs).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();

    let baseline = counter.load(Ordering::Relaxed);
    for _ in 0..5 {
        assert!(!db.is_stale().unwrap());
        assert_eq!(db.refresh().unwrap(), 0);
    }
    assert_eq!(
        counter.load(Ordering::Relaxed),
        baseline,
        "refreshing an unchanged store must not read any file contents"
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}
