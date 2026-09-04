#![cfg(feature = "mp-test")]
//! Multi-process safety: advisory cross-process write lock + WAL tailing.
//!
//! Tests 1-6 and 8 spawn the real `mp_worker` binary (built by the same
//! `mp-test` feature that gates this file), so what they exercise is genuine
//! inter-process contention on the store's `LOCK` file, not a simulation.
//!
//! Tests 7, 9, 10 and 12 stand a peer in by appending to the WAL through a
//! second `RealFs` handle in this process. That leaves the same bytes on disk
//! a peer would, and it is the only way to time the append precisely — mid-
//! frame (7), unappliable (9), or microseconds before the read that must see
//! it (12). Test 11 has no peer at all: it wraps the filesystem to prove an
//! unchanged store costs no content reads.
//!
//! Test list:
//!  1. server_handle_sees_child_process_writes_after_refresh
//!  2. two_writers_serialise_without_corruption
//!  3. writer_gets_busy_when_lock_held
//!  4. snapshot_without_the_lock_is_refused
//!  5. read_only_open_never_blocks_and_never_locks
//!  6. refresh_survives_snapshot_by_other_process
//!  7. partial_trailing_frame_is_not_an_error
//!  8. derived_edges_fire_on_refreshed_frames
//!  9. refresh_failure_degrades_the_handle
//! 10. wal_consumed_tracks_bytes_exactly
//! 11. refresh_on_unchanged_store_is_zero_cost
//! 12. read_sees_a_peer_commit_that_lands_microseconds_later

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

/// Every file in the store directory as `(name, length)`, sorted — enough to
/// catch a create, a delete, or a rewrite.
fn dir_listing(dir: &Path) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = std::fs::read_dir(dir)
        .expect("read store dir")
        .map(|e| {
            let e = e.expect("dir entry");
            (
                e.file_name().to_string_lossy().into_owned(),
                e.metadata().expect("entry metadata").len(),
            )
        })
        .collect();
    out.sort();
    out
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

// ── 4. A snapshot without the lock is refused, not performed ──────────────────

#[test]
fn snapshot_without_the_lock_is_refused() {
    let d = tmp("snapshot-busy");
    let db = SharedDb::open(&d).unwrap();
    {
        let mut g = db.write();
        for i in 0..5 {
            g.insert_node("Person", &format!("p-{i}"), vec![]).unwrap();
        }
    }
    let wal_before = std::fs::read(d.join("wal.bin")).unwrap();

    // Hold the lock, then have a peer try to snapshot through a write guard —
    // the guard it gets back has no lock, and a snapshot would replace wal.bin
    // out from under this process.
    let guard = db.write();
    let (code, _) = run_worker(&d, &["snapshot-shared"]);
    assert_eq!(code, 3, "a snapshot without the lock must report Busy");
    drop(guard);

    // The WAL is byte-for-byte what it was: the peer wrote nothing.
    assert_eq!(
        std::fs::read(d.join("wal.bin")).unwrap(),
        wal_before,
        "the refused snapshot must not have touched the WAL"
    );
    assert!(!d.join("snapshot.bin").exists(), "no snapshot was written");

    // With the lock free, the same call succeeds.
    let (code, _) = run_worker(&d, &["snapshot-shared"]);
    assert_eq!(code, 0, "the snapshot proceeds once the lock is free");
    assert!(d.join("snapshot.bin").exists());
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 5. A read-only open never waits for, and never takes, the lock ────────────

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

    // A read-only handle refuses mutations and writes nothing to disk — no WAL
    // repair, no snapshot migration, no archive sweep, not even a LOCK file.
    let opts = OpenOptions {
        read_only: true,
        ..OpenOptions::default()
    };
    let before = dir_listing(&d);
    let mut ro = GraphDb::open_with_options(&d, opts).unwrap();
    assert!(matches!(
        ro.insert_node("Person", "bob", vec![]),
        Err(GraphError::ReadOnly)
    ));
    assert!(matches!(ro.snapshot(), Err(GraphError::ReadOnly)));
    ro.refresh().expect("a read-only handle can still refresh");
    assert_eq!(
        dir_listing(&d),
        before,
        "a read-only open must leave every file in the store untouched"
    );
    drop(ro);
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 6. A snapshot taken by another process is absorbed by refresh ─────────────

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

// ── 7. A trailing partial frame is a wait, not a corruption ───────────────────

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

// ── 8. Rules fire on refreshed frames exactly as during open replay ───────────

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

// ── 9. A refresh that cannot apply the tail degrades the handle ───────────────

#[test]
fn refresh_failure_degrades_the_handle() {
    let d = tmp("refresh-degrade");
    let mut db = GraphDb::open(&d).unwrap();
    db.insert_node("Person", "alice", vec![]).unwrap();

    // A frame that decodes cleanly but cannot be applied: SetProp names a key
    // no node has. Half of `apply_frames` may land before the failure and the
    // cursor cannot say how much, so the handle has to stop accepting writes.
    let frame = encode_record(&WalRecord::SetProp {
        key: "ghost".into(),
        field: "x".into(),
        value: Value::Int(1),
    });
    let wal_len_before = std::fs::metadata(d.join("wal.bin")).unwrap().len();
    RealFs::new(&d)
        .unwrap()
        .append(FileId::Wal, &frame)
        .unwrap();

    let err = db.refresh().unwrap_err();
    assert!(
        matches!(err, GraphError::Corrupt { .. }),
        "expected the apply failure to propagate, got {err:?}"
    );

    // Degraded: every write refuses until the handle is reopened.
    let write_err = db.insert_node("Person", "bob", vec![]).unwrap_err();
    assert!(
        matches!(write_err, GraphError::Io(_)),
        "expected the degraded error, got {write_err:?}"
    );
    // Snapshots refuse too — they bypass the WAL append path entirely.
    let snap_err = db.snapshot().unwrap_err();
    assert!(
        matches!(snap_err, GraphError::Io(_)),
        "expected snapshot to refuse on a degraded handle, got {snap_err:?}"
    );

    // The failed refresh wrote nothing: the WAL still holds exactly the bytes
    // that were there, including the frame it could not apply.
    let on_disk = std::fs::metadata(d.join("wal.bin")).unwrap().len();
    assert_eq!(on_disk, wal_len_before + frame.len() as u64);
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}

// ── 10. The frame cursor advances by exactly the bytes appended ───────────────

#[test]
fn wal_consumed_tracks_bytes_exactly() {
    let d = tmp("cursor-bytes");
    let mut db = GraphDb::open(&d).unwrap();
    assert_eq!(db.wal_consumed(), 0);

    // A rule makes the writes below emit derived-edge marker frames alongside
    // the mutation frame. Markers are a separate append with their own cursor
    // arm, so this is the only way the two-appends-per-commit case is covered.
    db.create_rule(same_team_rule()).unwrap();
    let after_rule = std::fs::metadata(d.join("wal.bin")).unwrap().len();
    assert_eq!(db.wal_consumed(), after_rule);

    for i in 0..5 {
        db.insert_node(
            "Person",
            &format!("p-{i}"),
            vec![
                ("n".into(), Value::Int(i)),
                ("team".into(), Value::Str("red".into())),
            ],
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
    // Marker frames really were written: without them the WAL would hold only
    // the six mutation frames, and the cursor assertions above would pass
    // trivially on the single-append path.
    let bytes = std::fs::read(d.join("wal.bin")).unwrap();
    let (frames, _) = decode_all(&bytes);
    assert!(
        frames.len() > 6,
        "expected derived-edge marker frames beyond the 6 mutations, got {}",
        frames.len()
    );

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

// ── 11. Refreshing an unchanged store reads no WAL bytes ──────────────────────

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
    fn try_lock_exclusive(&self) -> std::io::Result<bool> {
        self.inner.try_lock_exclusive()
    }
    fn unlock(&self) -> std::io::Result<()> {
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

// ── 12. A peer's commit is visible to the next read, however fast it landed ───

/// The read path must not gate its staleness check on a wall clock.  It used
/// to: the check ran at most once per 50 ms, so a peer that committed inside
/// that window stayed invisible until the window expired, and whether a read
/// saw an already-completed commit depended on how fast the peer had been.
/// That is what broke test 1 on Linux, where the child writes its 100 nodes
/// in ~11 ms, while the same code passed on macOS, where merely spawning the
/// child costs more than the window.
///
/// The append below lands microseconds after the first read, so this fails
/// wherever the check is rate-limited, not only on the fast platform.
#[test]
fn read_sees_a_peer_commit_that_lands_microseconds_later() {
    let d = tmp("fast-peer");
    let db = SharedDb::open(&d).unwrap();
    assert_eq!(db.read().node_count(), 0);

    // Stand in for a peer process's commit: a complete frame appended to the
    // WAL through a separate handle on the same store, with no delay at all
    // before the read that must see it.
    let frame = encode_record(&WalRecord::InsertNode {
        label: "Person".into(),
        key: "fast".into(),
        props: vec![],
    });
    RealFs::new(&d)
        .unwrap()
        .append(FileId::Wal, &frame)
        .unwrap();

    let g = db.read();
    assert_eq!(
        g.node_count(),
        1,
        "a read after the peer's commit must absorb it, whatever the clock says"
    );
    assert!(g.node_info("fast").is_some());
    drop(g);
    drop(db);
    let _ = std::fs::remove_dir_all(&d);
}
