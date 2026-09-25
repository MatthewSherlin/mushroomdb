//! v0.6.11 Task 14 Step 8 — what Arc-ing the fold bought.
//!
//! Times the commits that straddle a fold on a store large enough for the copy
//! to matter. Before the change `fold_now` deep-cloned `ids`, `syms`, `fulltext`
//! and the accrued overlay; after it, eight `Arc::clone`s.
//!
//! Run: `cargo run --release -p mushroomdb --example fold_bench`
use core_api::{SharedDb, FOLD_EVERY_K};
use std::time::Instant;

fn main() {
    let n: usize = std::env::var("FOLD_BENCH_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    let dir = std::env::temp_dir().join("mdb-fold-bench");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let db = SharedDb::open(&dir).unwrap();
    println!("seeding {n} nodes...");
    let t0 = Instant::now();
    for i in 0..n {
        db.write().insert_node("N", &format!("n{i}"), vec![]).unwrap();
    }
    println!("  seeded in {:?}", t0.elapsed());

    // A reader held across the folds, which is what makes make_mut copy.
    let _snap = db.reader();

    // Time enough commits to cross several fold boundaries.
    let rounds = FOLD_EVERY_K * 4;
    let t1 = Instant::now();
    for i in 0..rounds {
        db.write()
            .insert_node("N", &format!("post{i}"), vec![])
            .unwrap();
    }
    let total = t1.elapsed();
    println!(
        "{rounds} commits across {} fold boundaries: {:?} total, {:?}/commit",
        rounds / FOLD_EVERY_K,
        total,
        total / rounds as u32
    );
}
