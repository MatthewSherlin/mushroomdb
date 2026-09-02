/// Unit tests for the slow-query log on [`GraphDb`].
///
/// Tests use `set_slow_query_threshold_ms` instead of env vars to avoid
/// process-global state races with parallel test threads.
use core_api::GraphDb;
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-slow-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Threshold 0 (disabled): no slow-query entries regardless of actual duration.
#[test]
fn slow_query_disabled_when_threshold_zero() {
    let dir = tmp("sq-disabled");
    let mut db = GraphDb::open(&dir).unwrap();
    db.set_slow_query_threshold_ms(0);

    // Run a trivial query — should never appear in the ring.
    let params = BTreeMap::new();
    db.query("MATCH (n) RETURN n", &params).unwrap();

    let snap = db.slow_query_snapshot();
    assert_eq!(snap.threshold_ms, 0);
    assert_eq!(snap.count, 0, "ring must be empty when threshold is 0");
    assert!(
        snap.last.is_empty(),
        "last must be empty when threshold is 0"
    );
}

/// Threshold 1ms with a deliberately heavy scan: ring must contain the query.
#[test]
fn slow_query_captured_when_threshold_exceeded() {
    let dir = tmp("sq-captured");
    let mut db = GraphDb::open(&dir).unwrap();

    // Insert enough nodes so a full scan takes at least 1ms.
    for i in 0..3000u32 {
        db.insert_node("Item", &format!("item-{i}"), vec![])
            .unwrap();
    }

    // Threshold 1ms — a full scan of 3000 nodes should always exceed this.
    db.set_slow_query_threshold_ms(1);

    let params = BTreeMap::new();
    db.query("MATCH (n:Item) RETURN n", &params).unwrap();

    let snap = db.slow_query_snapshot();
    assert_eq!(snap.threshold_ms, 1);
    assert!(
        snap.count >= 1,
        "slow_query count must be >= 1 after an exceeding query, got {}",
        snap.count
    );
    assert!(
        !snap.last.is_empty(),
        "slow_query ring must be non-empty after an exceeding query"
    );
    let entry = &snap.last[snap.last.len() - 1];
    assert!(
        entry.query.contains("MATCH"),
        "entry query must contain the Cypher text, got {:?}",
        entry.query
    );
    assert!(
        entry.ms >= 1,
        "entry.ms must be >= threshold (1), got {}",
        entry.ms
    );
    assert!(
        entry.at_commit > 0,
        "at_commit must be positive after inserts"
    );
}

/// Ring cap: after 17 slow queries only the 16 most-recent are kept.
#[test]
fn slow_query_ring_cap_at_16() {
    let dir = tmp("sq-cap");
    let mut db = GraphDb::open(&dir).unwrap();

    // Insert enough nodes for each query to be slow at 1ms threshold.
    for i in 0..4000u32 {
        db.insert_node("Item", &format!("cap-{i}"), vec![]).unwrap();
    }
    db.set_slow_query_threshold_ms(1);

    let params = BTreeMap::new();
    for _ in 0..17 {
        db.query("MATCH (n:Item) RETURN n", &params).unwrap();
    }

    let snap = db.slow_query_snapshot();
    assert_eq!(snap.count, 17, "lifetime count must track all 17");
    assert!(
        snap.last.len() <= 16,
        "ring must hold at most 16 entries, got {}",
        snap.last.len()
    );
}

/// commit_seq accessor returns the commit count.
#[test]
fn commit_seq_increments_with_writes() {
    let dir = tmp("sq-commitseq");
    let mut db = GraphDb::open(&dir).unwrap();
    let before = db.commit_seq();
    db.insert_node("X", "x1", vec![]).unwrap();
    db.insert_node("X", "x2", vec![]).unwrap();
    assert!(
        db.commit_seq() >= before + 2,
        "commit_seq must advance by at least 2 after two writes"
    );
}

/// wal_size_bytes returns a positive size for a RealFs database.
#[test]
fn wal_size_bytes_positive_after_write() {
    let dir = tmp("sq-walbytes");
    let mut db = GraphDb::open(&dir).unwrap();
    db.insert_node("Y", "y1", vec![]).unwrap();
    let size = db
        .wal_size_bytes()
        .expect("wal_size_bytes must succeed on RealFs");
    assert!(size > 0, "WAL must be non-empty after a write, got {size}");
}
