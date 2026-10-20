//! `durable_wal_position` reports only fsynced WAL bytes, ending on a frame
//! boundary, and starts a new generation whenever `wal.bin` is replaced.
//!
//! The backup worker ships exactly what this position says, so every way a
//! byte reaches the WAL — a direct write, a group commit, a relaxed write, a
//! failed group fsync, a snapshot, a reopen — is pinned here.

use core_storage::wal::decode_all;
use mushroomdb::{BatchOp, FsyncPolicy, SharedDb, SnapshotOptions, SnapshotStop, WalPosition};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "graphdb-durable-{}-{}-{}",
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

fn wal_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("wal.bin"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// The position ends on a frame boundary of the file on disk.
fn assert_on_frame_boundary(dir: &Path, pos: WalPosition) {
    let bytes = std::fs::read(dir.join("wal.bin")).unwrap_or_default();
    assert!(pos.len as usize <= bytes.len(), "{pos:?} past the file");
    let (_, valid) = decode_all(&bytes[..pos.len as usize]);
    assert_eq!(valid as u64, pos.len, "{pos:?} is not on a frame boundary");
}

fn insert(key: &str) -> Vec<BatchOp> {
    vec![BatchOp::InsertNode {
        label: "A".into(),
        key: key.into(),
        props: vec![],
    }]
}

#[test]
fn a_strict_direct_write_is_durable_up_to_its_last_frame() {
    let dir = tmp("strict");
    let db = SharedDb::open(&dir).unwrap();
    let before = db.durable_wal_position();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let pos = db.durable_wal_position();
    assert_eq!(pos.generation, before.generation);
    assert!(pos.len > before.len);
    assert_eq!(pos.len, wal_len(&dir));
    assert_eq!(pos.frames, db.read().wal_total_commits().unwrap());
    assert!(!pos.snapshot_in_progress);
    assert_on_frame_boundary(&dir, pos);
}

#[test]
fn a_group_commit_is_durable_once_its_fsync_returns() {
    let dir = tmp("group");
    let db = SharedDb::open(&dir).unwrap();
    db.submit_batch(insert("a")).unwrap();
    db.submit_batch(insert("b")).unwrap();
    let pos = db.durable_wal_position();
    assert_eq!(pos.len, wal_len(&dir));
    assert_eq!(pos.frames, db.read().wal_total_commits().unwrap());
    assert_on_frame_boundary(&dir, pos);
}

#[test]
fn a_relaxed_write_is_not_durable_until_something_fsyncs_it() {
    let dir = tmp("relaxed");
    let db = SharedDb::open(&dir).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let synced = db.durable_wal_position();
    db.write().set_fsync_policy(FsyncPolicy::Relaxed);
    db.write().insert_node("A", "b", vec![]).unwrap();
    db.submit_batch(insert("c")).unwrap();
    assert!(
        wal_len(&dir) > synced.len,
        "the relaxed writes reached the file"
    );
    assert_eq!(
        db.durable_wal_position(),
        synced,
        "bytes no fsync covered must not count as durable"
    );
    // The next fsync covers everything before it.
    db.write().set_fsync_policy(FsyncPolicy::Strict);
    db.write().insert_node("A", "d", vec![]).unwrap();
    assert_eq!(db.durable_wal_position().len, wal_len(&dir));
}

#[test]
fn a_failed_group_fsync_leaves_the_position_where_it_was() {
    let dir = tmp("failed-fsync");
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
    db.submit_batch(insert("ok")).unwrap();
    let synced = db.durable_wal_position();
    assert_eq!(synced.len, wal_len(&dir));
    fail.store(true, Ordering::Release);
    assert!(db.submit_batch(insert("lost")).is_err());
    assert_eq!(db.durable_wal_position(), synced);
}

#[test]
fn a_truncating_snapshot_starts_a_new_generation_at_its_baseline() {
    let dir = tmp("truncate");
    let db = SharedDb::open(&dir).unwrap();
    for k in ["a", "b", "c"] {
        db.write().insert_node("A", k, vec![]).unwrap();
    }
    let before = db.durable_wal_position();
    db.write().snapshot().unwrap();
    let pos = db.durable_wal_position();
    assert_eq!(pos.generation, before.generation + 1);
    assert_eq!(pos.len, wal_len(&dir), "the baseline is written durably");
    assert_eq!(pos.frames, db.read().wal_total_commits().unwrap());
    assert!(!pos.snapshot_in_progress);
    assert_on_frame_boundary(&dir, pos);
}

#[test]
fn an_archiving_snapshot_starts_a_new_generation() {
    let dir = tmp("archive");
    let db = SharedDb::open(&dir).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let before = db.durable_wal_position();
    db.write()
        .snapshot_with(SnapshotOptions {
            keep_wal: false,
            archive_wal: true,
        })
        .unwrap();
    let pos = db.durable_wal_position();
    assert!(pos.generation > before.generation);
    assert_eq!(pos.len, wal_len(&dir));
    // Frames keep counting across the archive: the archived ones are still
    // addressable history.
    assert!(pos.frames >= before.frames);
    assert_eq!(pos.frames, db.read().wal_total_commits().unwrap());
}

#[test]
fn a_snapshot_that_keeps_the_wal_keeps_the_generation() {
    let dir = tmp("keep");
    let db = SharedDb::open(&dir).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let before = db.durable_wal_position();
    db.write()
        .snapshot_with(SnapshotOptions {
            keep_wal: true,
            archive_wal: false,
        })
        .unwrap();
    let pos = db.durable_wal_position();
    assert_eq!(pos.generation, before.generation);
    assert_eq!(pos.len, before.len);
    assert!(!pos.snapshot_in_progress);
}

#[test]
fn a_snapshot_that_fails_clears_its_in_progress_mark() {
    let dir = tmp("stopped");
    let db = SharedDb::open(&dir).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let before = db.durable_wal_position();
    {
        let mut w = db.write();
        w.stop_next_snapshot(SnapshotStop::BeforeWalReplacement);
        assert!(w.snapshot().is_err());
    }
    let pos = db.durable_wal_position();
    assert!(!pos.snapshot_in_progress, "the mark must drop on error");
    assert_eq!(
        pos.generation, before.generation,
        "the WAL was not replaced"
    );
}

#[test]
fn a_reopen_starts_at_the_end_of_what_it_replayed() {
    let dir = tmp("reopen");
    {
        let db = SharedDb::open(&dir).unwrap();
        db.write().set_fsync_policy(FsyncPolicy::Relaxed);
        db.write().insert_node("A", "a", vec![]).unwrap();
        db.write().insert_node("A", "b", vec![]).unwrap();
        assert_eq!(db.durable_wal_position().len, 0);
    }
    let db = SharedDb::open(&dir).unwrap();
    let pos = db.durable_wal_position();
    assert_eq!(pos.generation, 0);
    assert_eq!(
        pos.len,
        wal_len(&dir),
        "the first read fsyncs what the open replayed"
    );
    assert_eq!(pos.frames, db.read().wal_total_commits().unwrap());
    assert_on_frame_boundary(&dir, pos);
}

#[test]
fn reading_the_position_never_waits_for_the_write_lock() {
    let dir = tmp("nolock");
    let db = SharedDb::open(&dir).unwrap();
    db.write().insert_node("A", "a", vec![]).unwrap();
    let guard = db.write();
    // Held write guard: a lock-taking read here would deadlock the test.
    let pos = db.durable_wal_position();
    assert!(pos.len > 0);
    drop(guard);
}
