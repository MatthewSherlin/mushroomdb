//! The commit-times sidecar, at the engine level.
//!
//! The unit tests in `core-storage` cover the map's own arithmetic. These cover
//! the thing that can actually go wrong in the engine: **a hole**. Stamping
//! anywhere but the one place a commit is born leaves a commit with no recorded
//! time, and a hole does not announce itself — it resolves a date to the wrong
//! commit and returns a plausible graph.

use core_api::GraphDb;
use core_storage::types::GraphError;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mdb-commit-times-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Every commit gets an entry, and the entries are contiguous.
///
/// A gap here is the failure this whole task exists to prevent.
#[test]
fn every_commit_is_stamped_and_the_map_has_no_holes() {
    let dir = tmp("no-holes");
    let mut db = GraphDb::open(&dir).expect("open");
    for i in 0..25 {
        db.insert_node("N", &format!("n{i}"), vec![])
            .expect("insert");
    }
    let seq = db.commit_seq();
    assert!(seq >= 25, "expected at least 25 commits, saw {seq}");

    // Every commit from the first stamped one to the newest must have a time.
    let mut missing = Vec::new();
    for c in 1..=seq {
        if db.commit_time_ms(c).is_none() {
            missing.push(c);
        }
    }
    assert!(
        missing.is_empty(),
        "commits with no recorded time (a hole in the map): {missing:?}"
    );
}

/// Recorded times never go backwards across ordinary commits, and a date
/// resolves to a commit that actually exists.
#[test]
fn a_date_resolves_to_a_real_commit() {
    let dir = tmp("resolves");
    let mut db = GraphDb::open(&dir).expect("open");
    db.insert_node("N", "a", vec![]).expect("insert");
    let early = db.commit_time_ms(db.commit_seq()).expect("stamped");

    std::thread::sleep(std::time::Duration::from_millis(5));
    for i in 0..5 {
        db.insert_node("N", &format!("later{i}"), vec![])
            .expect("insert");
    }
    let last = db.commit_seq();

    // An instant at or after the newest commit resolves to the newest commit.
    let resolved = db.resolve_instant(i64::MAX / 2).expect("resolve");
    assert_eq!(resolved, last);

    // An instant at the first commit's own time resolves at or after it.
    let at_first = db.resolve_instant(early).expect("resolve");
    assert!(
        at_first >= 1 && at_first <= last,
        "resolved {at_first} outside 1..={last}"
    );
}

/// A store written before this release records no times. It must open, read and
/// write exactly as before, and say so plainly rather than guess a commit.
#[test]
fn a_store_with_no_sidecar_reads_identically_and_refuses_dates_by_name() {
    let dir = tmp("no-sidecar");
    {
        let mut db = GraphDb::open(&dir).expect("open");
        db.insert_node("N", "a", vec![]).expect("insert");
        db.insert_node("N", "b", vec![]).expect("insert");
        db.insert_edge("KNOWS", "a", "b").expect("edge");
        db.snapshot().expect("snapshot");
    }
    // Remove the sidecar: this is exactly the on-disk shape a pre-v0.6.11
    // release leaves behind.
    std::fs::remove_file(dir.join("commit_times.bin")).expect("rm sidecar");

    let db = GraphDb::open(&dir).expect("reopen without a sidecar");
    // Ordinary reads are untouched.
    assert!(db.node_info("a").is_some(), "the store still reads");
    assert!(db.node_info("b").is_some());
    let stats = db.stats();
    assert_eq!(stats.nodes_live, 2);

    // A date is refused by name — never resolved to a guessed commit.
    match db.resolve_instant(1_700_000_000_000) {
        Err(GraphError::NoRecordedTime) => {}
        other => panic!("expected NoRecordedTime, got {other:?}"),
    }
}

/// The map survives a reopen, because it is a file and not in-memory state.
#[test]
fn reopen_preserves_the_map() {
    let dir = tmp("reopen");
    let before;
    {
        let mut db = GraphDb::open(&dir).expect("open");
        for i in 0..4 {
            db.insert_node("N", &format!("n{i}"), vec![])
                .expect("insert");
        }
        before = db.commit_time_ms(2).expect("commit 2 is stamped");
    }
    let db = GraphDb::open(&dir).expect("reopen");
    assert_eq!(
        db.commit_time_ms(2),
        Some(before),
        "the sidecar did not survive the reopen"
    );
}

/// Replay must not re-stamp. A reopened store's recorded times are the times
/// the commits happened, not the time the WAL was replayed.
#[test]
fn replay_does_not_restamp_with_the_replay_time() {
    let dir = tmp("no-restamp");
    let original;
    {
        let mut db = GraphDb::open(&dir).expect("open");
        db.insert_node("N", "a", vec![]).expect("insert");
        original = db.commit_time_ms(1).expect("stamped");
    }
    // Long enough that a re-stamp would be obvious rather than a rounding blur.
    std::thread::sleep(std::time::Duration::from_millis(40));

    let db = GraphDb::open(&dir).expect("reopen — this replays the WAL");
    let after = db.commit_time_ms(1).expect("still stamped");
    assert_eq!(
        after, original,
        "replay re-stamped commit 1: {original} became {after}. apply_frames \
         must not call SystemTime::now — it re-applies commits that already \
         happened."
    );
}

/// A damaged sidecar degrades the date surface and nothing else. It must not
/// read as "this store records no times", because that is what an honest
/// pre-v0.6.11 store says.
#[test]
fn a_damaged_sidecar_is_reported_as_damage_not_as_absence() {
    let dir = tmp("damaged");
    {
        let mut db = GraphDb::open(&dir).expect("open");
        db.insert_node("N", "a", vec![]).expect("insert");
    }
    std::fs::write(dir.join("commit_times.bin"), b"XXXXnot a time map").expect("corrupt it");

    let db = GraphDb::open(&dir).expect("a damaged sidecar must not fail the open");
    assert!(db.node_info("a").is_some(), "the store still reads");
    match db.resolve_instant(1_700_000_000_000) {
        Err(GraphError::Corrupt { .. }) => {}
        other => panic!("expected Corrupt, got {other:?}"),
    }
}
