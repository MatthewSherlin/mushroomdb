/// Unit tests for the slow-query log on [`GraphDb`].
///
/// Tests use `set_slow_query_threshold_ms` instead of env vars to avoid
/// process-global state races with parallel test threads.
use core_api::{GraphDb, Value};
use std::collections::BTreeMap;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("graphdb-slow-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A query whose cost is bounded below regardless of CPU speed: two
/// independent full scans of `n` nodes joined as a cross product before
/// `count(*)` aggregates them. Unlike a single-scan fixture (whose duration
/// shrinks as CPUs get faster), this is O(n^2) evaluation work by
/// construction, so it stays reliably above a 1ms threshold. Measured
/// locally with `n = 500` via `std::time::Instant`: ~28ms (28x the 1ms
/// threshold) with a ~2s fixture insert, on an Apple M4 Pro (2026-09-03).
const SLOW_QUERY: &str = "MATCH (a:N) MATCH (b:N) RETURN count(*)";

/// Insert `n` nodes labeled `N` with a `k` property, for use with
/// [`SLOW_QUERY`]'s self cross product.
fn seed_cross(db: &mut GraphDb<core_storage::fs::RealFs>, n: usize) {
    for i in 0..n {
        db.insert_node(
            "N",
            &format!("n-{i}"),
            vec![("k".into(), Value::Int(i as i64))],
        )
        .unwrap();
    }
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

/// Threshold 1ms with a query that is slow by construction (a self cross
/// product, see [`SLOW_QUERY`]): ring must contain the query.
#[test]
fn slow_query_captured_when_threshold_exceeded() {
    let dir = tmp("sq-captured");
    let mut db = GraphDb::open(&dir).unwrap();
    seed_cross(&mut db, 500);

    // Threshold 1ms — the cross-product query is ~28ms by construction and
    // should always exceed this regardless of CPU speed.
    db.set_slow_query_threshold_ms(1);

    let params = BTreeMap::new();
    let rs = db.query(SLOW_QUERY, &params).unwrap();
    assert_eq!(
        rs.row(0)[0],
        Some(Value::Int(250_000)),
        "500 x 500 cross product must count 250000 rows"
    );

    let snap = db.slow_query_snapshot();
    assert_eq!(snap.threshold_ms, 1);
    assert_eq!(
        snap.count, 1,
        "slow_query count must be 1 after one exceeding query, got {}",
        snap.count
    );
    assert_eq!(
        snap.last.len(),
        1,
        "slow_query ring must hold exactly the one exceeding query"
    );
    let entry = &snap.last[0];
    assert_eq!(
        entry.query, SLOW_QUERY,
        "entry query must equal the Cypher text that was run"
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
    seed_cross(&mut db, 500);
    db.set_slow_query_threshold_ms(1);

    let params = BTreeMap::new();
    for _ in 0..17 {
        db.query(SLOW_QUERY, &params).unwrap();
    }

    let snap = db.slow_query_snapshot();
    assert_eq!(snap.count, 17, "lifetime count must track all 17");
    assert_eq!(
        snap.last.len(),
        16,
        "ring must hold exactly 16 entries after 17 slow queries, got {}",
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
