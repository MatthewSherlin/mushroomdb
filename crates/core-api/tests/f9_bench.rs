//! The measurement behind defect #29 (review finding F9), kept so the number
//! can be reproduced rather than taken on trust.
//!
//! `#[ignore]`, and that is the point: it builds 20 000 nodes and 160 000 edge
//! inserts and takes about thirteen minutes, it asserts nothing about timing,
//! and a test that cannot fail has no business in `cargo test --workspace`.
//! Run it deliberately:
//!
//! ```text
//! cargo test -p mushroomdb --release --test f9_bench -- --ignored --nocapture
//! F9_N=50000 cargo test -p mushroomdb --release --test f9_bench -- --ignored --nocapture
//! ```
//!
//! What it showed, release profile, n=20 000, fanout 4, every pair inserted
//! twice, snapshotted and reopened so the mmap'd-base branch is the one taken,
//! 5 reps, median — see `docs/internal/defect-ledger.md` #29 for the numbers
//! and what changed between them.
use core_storage::fs::RealFs;
use mushroomdb::algo::AlgoDir;
use mushroomdb::GraphDb;

type Db = GraphDb<RealFs>;

fn build(dir: &std::path::Path, n: usize, fanout: usize) -> Db {
    let mut db = Db::open(dir).unwrap();
    db.enable_multiplicity().unwrap();
    for i in 0..n {
        db.insert_node("N", &format!("n{i}"), vec![]).unwrap();
    }
    for i in 0..n {
        for f in 0..fanout {
            let dst = (i + f + 1) % n;
            db.insert_edge("E", &format!("n{i}"), &format!("n{dst}"))
                .unwrap();
            // a duplicate so the count is not the trivial 1
            db.insert_edge("E", &format!("n{i}"), &format!("n{dst}"))
                .unwrap();
        }
    }
    // Snapshot so the store has a mmap'd V10 base: `topo_view()` /
    // `edge_props_view()` then take the rkyv-section branch, which is the
    // branch F9 is about.
    db.snapshot().unwrap();
    drop(db);
    Db::open(dir).unwrap()
}

#[test]
#[ignore = "a ~13-minute benchmark that asserts nothing; run it with --ignored"]
fn f9_measure() {
    let n: usize = std::env::var("F9_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    let fanout: usize = 4;
    let tmp = std::env::temp_dir().join(format!("f9-bench-{n}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let db = build(&tmp, n, fanout);

    // warm
    let _ = db
        .degrees_multiplicity(None, Some("N"), None, None, AlgoDir::Out, None)
        .unwrap();

    let reps = 5;
    let mut mult = Vec::new();
    let mut uniq = Vec::new();
    for _ in 0..reps {
        let t = std::time::Instant::now();
        let r = db
            .degrees_multiplicity(None, Some("N"), None, None, AlgoDir::Out, None)
            .unwrap();
        mult.push(t.elapsed().as_secs_f64());
        assert_eq!(r.len(), n);

        let t = std::time::Instant::now();
        let r = db
            .degrees(None, Some("N"), None, None, AlgoDir::Out, None)
            .unwrap();
        uniq.push(t.elapsed().as_secs_f64());
        assert_eq!(r.len(), n);
    }
    mult.sort_by(|a, b| a.partial_cmp(b).unwrap());
    uniq.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "F9 n={n} fanout={fanout} multiplicity median {:.6}s min {:.6}s | unique median {:.6}s min {:.6}s",
        mult[reps / 2],
        mult[0],
        uniq[reps / 2],
        uniq[0]
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
