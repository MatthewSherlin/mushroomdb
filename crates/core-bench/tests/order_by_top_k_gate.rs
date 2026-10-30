//! Performance gate for `ORDER BY … LIMIT` over a range index.
//!
//! Covers the ascending unbounded case that regressed in row 170: a 100k-node
//! store with an indexed Int property, `ORDER BY n.val LIMIT 10`, no range
//! bound.  Before the non_scalar fix the executor called `unindexed_ids`
//! (O(n)) on every query; after the fix it skips the walk when there are
//! enough indexed candidates.
//!
//! Run:
//!   cargo test --release -p core-bench --test order_by_top_k_gate \
//!     -- order_by_top_k_gate --ignored --nocapture

use mushroomdb::{GraphDb, RealFs, Value};
use std::time::{Duration, Instant};

const NODES: usize = 100_000;
/// Maximum allowed median for ascending unbounded ORDER BY LIMIT 10 at 100k nodes.
const ASC_MEDIAN_LIMIT_US: u64 = 200;
/// Iterations per query shape.
const ITERS: usize = 500;

fn tmp_dir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "graphdb-order-by-gate-{}-{}",
        std::process::id(),
        nanos,
    ))
}

fn build_db() -> GraphDb<RealFs> {
    let dir = tmp_dir();
    let mut db = GraphDb::open(&dir).expect("open");
    db.enable_range_index("by_val", "Item", "val").unwrap();
    let props: Vec<(String, Value)> =
        vec![("val".into(), Value::Int(0))];
    // Insert in batches to stay fast.
    let batch_size = 1_000;
    for batch_start in (0..NODES).step_by(batch_size) {
        let batch_end = (batch_start + batch_size).min(NODES);
        for i in batch_start..batch_end {
            db.insert_node(
                "Item",
                &format!("n{i:06}"),
                vec![("val".into(), Value::Int((i as i64 * 7) % 99_991))],
            )
            .unwrap();
        }
    }
    db
}

fn median_us(mut times: Vec<Duration>) -> u64 {
    times.sort_unstable();
    times[times.len() / 2].as_micros() as u64
}

#[test]
#[ignore = "timing gate — run with --release --ignored"]
fn order_by_top_k_gate() {
    let db = build_db();
    let p = std::collections::BTreeMap::new();

    // Warm up.
    for _ in 0..50 {
        let _ = db.query("MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 10", &p);
        let _ = db.query("MATCH (n:Item) RETURN n ORDER BY n.val DESC LIMIT 10", &p);
    }

    let mut asc_times = Vec::with_capacity(ITERS);
    let mut desc_times = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let t = Instant::now();
        let _ = db
            .query("MATCH (n:Item) RETURN n ORDER BY n.val LIMIT 10", &p)
            .unwrap();
        asc_times.push(t.elapsed());

        let t = Instant::now();
        let _ = db
            .query("MATCH (n:Item) RETURN n ORDER BY n.val DESC LIMIT 10", &p)
            .unwrap();
        desc_times.push(t.elapsed());
    }

    let asc_p50 = median_us(asc_times);
    let desc_p50 = median_us(desc_times);
    println!(
        "ORDER BY … LIMIT 10 on {NODES} indexed-int nodes  \
         asc_p50={asc_p50} µs  desc_p50={desc_p50} µs  limit={ASC_MEDIAN_LIMIT_US} µs"
    );
    assert!(
        asc_p50 <= ASC_MEDIAN_LIMIT_US,
        "ASC ORDER BY LIMIT p50 {asc_p50} µs exceeds gate {ASC_MEDIAN_LIMIT_US} µs"
    );
}
