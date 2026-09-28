//! v0.6.11 Task 14 Step 8 — the path where the fold is *not* amortised.
//!
//! `fold_now` runs every `FOLD_EVERY_K` commits on the write path, so a
//! per-commit measurement divides its cost by 64 and the WAL swamps it. But
//! `refresh()` folds on **every call that applies at least one peer commit**
//! (`db.rs:2788`), with no threshold — so a reader tailing a writer pays a whole
//! fold per refresh. That is the cost this change is really about.
//!
//! Two handles on one store directory: a writer, and a reader that refreshes.
//!
//! Run: `cargo run --release -p mushroomdb --example refresh_bench`
use core_api::{GraphDb, OpenOptions};
use std::time::Instant;

fn main() {
    let n: usize = std::env::var("REFRESH_BENCH_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    let refreshes: usize = 40;

    let dir = std::env::temp_dir().join("mdb-refresh-bench");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut w = GraphDb::open(&dir).unwrap();
    println!("seeding {n} nodes...");
    for i in 0..n {
        w.insert_node("N", &format!("n{i}"), vec![]).unwrap();
    }
    w.snapshot().unwrap();

    // A second handle on the same directory, which will tail the writer. It must
    // be read-only: the writer holds the advisory lock for its lifetime, so a
    // second writing open answers `Busy`.
    let mut r = GraphDb::open_with_options(
        &dir,
        OpenOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    let _ = r.refresh().unwrap();

    // Each round: one peer commit, then one refresh that must fold.
    let mut total = std::time::Duration::ZERO;
    for i in 0..refreshes {
        w.insert_node("N", &format!("peer{i}"), vec![]).unwrap();
        let t = Instant::now();
        let applied = r.refresh().unwrap();
        total += t.elapsed();
        assert!(
            applied >= 1,
            "refresh {i} applied nothing — the bench is wrong"
        );
    }
    println!(
        "{refreshes} refreshes, each applying >=1 peer commit and folding: \
         {total:?} total, {:?}/refresh",
        total / refreshes as u32
    );
}
